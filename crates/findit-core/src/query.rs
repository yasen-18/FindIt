//! Query language: tokenizer, parser, compiler and matcher.
//!
//! | Syntax            | Meaning                                         |
//! |-------------------|-------------------------------------------------|
//! | `a b`             | AND                                             |
//! | `a\|b`            | OR (binds tighter than AND, like Everything)    |
//! | `!a`              | NOT                                             |
//! | `<a b>\|c`        | grouping (`<` and `>` cannot appear in names)   |
//! | `"a b"`           | exact phrase                                    |
//! | `*.pdf`, `IMG_??` | wildcards, match the whole name                 |
//! | `regex:`          | regular expression (linear time, size-limited)  |
//! | `path:`           | match against the full path                     |
//! | `ext:pdf;docx`    | extensions                                      |
//! | `size:>10mb`      | size comparison or range                        |
//! | `dm:last7days`    | date modified                                   |
//! | `type:image`      | type groups                                     |
//! | `file:` `folder:` | only files / only folders                       |
//! | `case:` `word:` `diacritics:` | per-term (`case:Readme`) or whole-query (`case: Readme`) toggles |

use crate::fold::{self, FoldMode};
use crate::index::{flags, DirPaths, Entry, Index};
use crate::time::{self, Clock};
use crate::types;
use memchr::memmem;
use regex::{Regex, RegexBuilder};

/// Options from the UI that act as defaults for every term.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryOptions {
    pub match_case: bool,
    pub match_path: bool,
    pub whole_word: bool,
    pub match_diacritics: bool,
    pub now_unix: i64,
    pub tz_offset_secs: i64,
}

/// A problem with the query, shown inline under the search box.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QueryError {
    pub message: String,
}

impl QueryError {
    fn new(message: impl Into<String>) -> Self {
        QueryError { message: message.into() }
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for QueryError {}

// ----- tokens ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    /// `text` has quotes removed; `plain` is the length of the leading unquoted part,
    /// the only place where a `modifier:` prefix is recognised.
    Word {
        text: String,
        plain: usize,
    },
    Or,
    Not,
    Open,
    Close,
}

fn tokenize(q: &str) -> Vec<Tok> {
    let chars: Vec<char> = q.chars().collect();
    let mut toks = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    let is_sep = |c: char| c.is_whitespace() || c == '|';
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '|' => {
                toks.push(Tok::Or);
                i += 1;
                continue;
            }
            '!' => {
                toks.push(Tok::Not);
                i += 1;
                continue;
            }
            '<' => {
                toks.push(Tok::Open);
                depth += 1;
                i += 1;
                continue;
            }
            '>' if depth > 0 => {
                toks.push(Tok::Close);
                depth -= 1;
                i += 1;
                continue;
            }
            // A stray `>` cannot be part of a file name; ignore it.
            '>' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        let mut text = String::new();
        let mut plain: Option<usize> = None;
        let mut in_quote = false;
        while i < chars.len() {
            let ch = chars[i];
            if ch == '"' {
                in_quote = !in_quote;
                plain.get_or_insert(text.len());
                i += 1;
                continue;
            }
            if !in_quote {
                if is_sep(ch) {
                    break;
                }
                if ch == '>' && depth > 0 && chars.get(i + 1).is_none_or(|&n| is_sep(n) || n == '>') {
                    break;
                }
            }
            text.push(ch);
            i += 1;
        }
        let plain = plain.unwrap_or(text.len());
        toks.push(Tok::Word { text, plain });
    }
    toks
}

// ----- syntax tree ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TermFlags {
    case: bool,
    word: bool,
    diacritics: bool,
    path: bool,
}

impl TermFlags {
    fn merge(self, o: TermFlags) -> TermFlags {
        TermFlags {
            case: self.case || o.case,
            word: self.word || o.word,
            diacritics: self.diacritics || o.diacritics,
            path: self.path || o.path,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RawKind {
    Text(String),
    Regex(String),
    Ext(String),
    Size(String),
    Date(String),
    Type(String),
    Content(String),
    /// `file:` or `folder:` on its own.
    Filter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawTerm {
    flags: TermFlags,
    file_only: bool,
    folder_only: bool,
    kind: RawKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ast {
    And(Vec<Ast>),
    Or(Vec<Ast>),
    Not(Box<Ast>),
    Term(RawTerm),
    Empty,
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    globals: TermFlags,
}

const KNOWN_FILTERS: &str = "case:, word:, diacritics:, path:, file:, folder:, ext:, size:, dm:, type:, regex:, content:";

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn parse_seq(&mut self) -> Result<Ast, QueryError> {
        let mut items = Vec::new();
        while let Some(t) = self.peek() {
            match t {
                Tok::Close => break,
                Tok::Or => {
                    self.pos += 1;
                }
                _ => {
                    let node = self.parse_or()?;
                    if node != Ast::Empty {
                        items.push(node);
                    }
                }
            }
        }
        Ok(match items.len() {
            0 => Ast::Empty,
            1 => items.pop().unwrap_or(Ast::Empty),
            _ => Ast::And(items),
        })
    }

    fn parse_or(&mut self) -> Result<Ast, QueryError> {
        let mut alts = vec![self.parse_unary()?];
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            match self.peek() {
                None | Some(Tok::Close) => break,
                _ => alts.push(self.parse_unary()?),
            }
        }
        alts.retain(|a| *a != Ast::Empty);
        Ok(match alts.len() {
            0 => Ast::Empty,
            1 => alts.pop().unwrap_or(Ast::Empty),
            _ => Ast::Or(alts),
        })
    }

    fn parse_unary(&mut self) -> Result<Ast, QueryError> {
        let mut negate = false;
        while self.peek() == Some(&Tok::Not) {
            self.pos += 1;
            negate = !negate;
        }
        let inner = self.parse_primary()?;
        Ok(if negate && inner != Ast::Empty { Ast::Not(Box::new(inner)) } else { inner })
    }

    fn parse_primary(&mut self) -> Result<Ast, QueryError> {
        let Some(tok) = self.toks.get(self.pos).cloned() else { return Ok(Ast::Empty) };
        self.pos += 1;
        match tok {
            Tok::Open => {
                let inner = self.parse_seq()?;
                if self.peek() == Some(&Tok::Close) {
                    self.pos += 1;
                }
                Ok(inner)
            }
            Tok::Word { text, plain } => self.word(text, plain),
            Tok::Or | Tok::Not | Tok::Close => Ok(Ast::Empty),
        }
    }

    fn word(&mut self, text: String, plain: usize) -> Result<Ast, QueryError> {
        let mut flags = TermFlags::default();
        let mut file_only = false;
        let mut folder_only = false;
        let mut consumed = 0usize;
        let mut value_kind: Option<&'static str> = None;
        loop {
            let region = &text[consumed..plain.max(consumed)];
            let Some(colon) = region.find(':') else { break };
            let name = &region[..colon];
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic()) {
                break;
            }
            let lname = name.to_ascii_lowercase();
            let after = consumed + colon + 1;
            match lname.as_str() {
                "case" | "matchcase" => flags.case = true,
                "word" | "ww" | "wholeword" => flags.word = true,
                "diacritics" | "accents" => flags.diacritics = true,
                "path" | "matchpath" => flags.path = true,
                "file" | "files" => file_only = true,
                "folder" | "folders" | "dir" => folder_only = true,
                "regex" => value_kind = Some("regex"),
                "ext" => value_kind = Some("ext"),
                "size" => value_kind = Some("size"),
                "dm" | "datemodified" | "modified" => value_kind = Some("dm"),
                "type" => value_kind = Some("type"),
                "content" => value_kind = Some("content"),
                // A single letter is a drive (`c:\users`), not a filter.
                _ if name.len() == 1 => break,
                _ => {
                    return Err(QueryError::new(format!("Unknown filter \"{name}:\". Known filters: {KNOWN_FILTERS}")));
                }
            }
            consumed = after;
            if value_kind.is_some() {
                break;
            }
        }
        let value = text[consumed..].to_string();
        let kind = match value_kind {
            Some("regex") => RawKind::Regex(value),
            Some("ext") => RawKind::Ext(value),
            Some("size") => RawKind::Size(value),
            Some("dm") => RawKind::Date(value),
            Some("type") => RawKind::Type(value),
            Some("content") => RawKind::Content(value),
            _ => {
                if value.is_empty() {
                    if file_only || folder_only {
                        self.globals = self.globals.merge(flags);
                        RawKind::Filter
                    } else {
                        // A toggle such as `case:` on its own applies to the whole query.
                        self.globals = self.globals.merge(flags);
                        return Ok(Ast::Empty);
                    }
                } else {
                    RawKind::Text(value)
                }
            }
        };
        Ok(Ast::Term(RawTerm { flags, file_only, folder_only, kind }))
    }
}

// ----- compiled query ---------------------------------------------------------------

#[derive(Debug)]
struct TextPred {
    loose: Vec<u8>,
    finder: memmem::Finder<'static>,
    /// Needle folded with `mode` when the match is case- or accent-sensitive.
    strict: Option<String>,
    mode: FoldMode,
    word: bool,
    path: bool,
}

#[derive(Debug)]
struct WildPred {
    pattern: Vec<char>,
    mode: FoldMode,
    path: bool,
}

#[derive(Debug)]
enum Pred {
    Text(Box<TextPred>),
    Wild(WildPred),
    Regex { re: Regex, path: bool },
    Ext(Vec<Vec<u8>>),
    Group(&'static [&'static str]),
    Size { lo: u64, hi: u64 },
    Date { lo: i64, hi: i64 },
    FileOnly,
    FolderOnly,
}

impl Pred {
    /// Relative cost, so `And` evaluates cheap checks first.
    fn cost(&self) -> u8 {
        match self {
            Pred::FileOnly | Pred::FolderOnly | Pred::Size { .. } | Pred::Date { .. } => 0,
            Pred::Ext(_) | Pred::Group(_) => 1,
            Pred::Text(t) if !t.path => 2,
            Pred::Wild(w) if !w.path => 3,
            Pred::Text(_) | Pred::Wild(_) => 4,
            Pred::Regex { path: false, .. } => 5,
            Pred::Regex { path: true, .. } => 6,
        }
    }
}

#[derive(Debug)]
enum Node {
    True,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Pred(Pred),
}

impl Node {
    fn cost(&self) -> u8 {
        match self {
            Node::True => 0,
            Node::Pred(p) => p.cost(),
            Node::Not(n) => n.cost(),
            Node::And(v) | Node::Or(v) => v.iter().map(Node::cost).max().unwrap_or(0),
        }
    }
}

/// A parsed and compiled query. Cheap to share between threads.
#[derive(Debug)]
pub struct Query {
    root: Node,
    rank_needles: Vec<Vec<u8>>,
    hl_needles: Vec<Vec<u8>>,
    hl_regex: Vec<Regex>,
    needs_dir_paths: bool,
    has_regex: bool,
    has_terms: bool,
    source: String,
}

/// Everything the matcher needs to know about one entry.
pub struct Subject<'a> {
    pub index: &'a Index,
    pub id: u32,
    pub entry: &'a Entry,
    pub name_loose: &'a [u8],
    pub dir_paths: Option<&'a DirPaths>,
}

/// Per-thread buffers reused between entries to avoid allocations.
#[derive(Default)]
pub struct Scratch {
    path_loose: Vec<u8>,
    path_loose_ready: bool,
    strict: String,
    strict_key: Option<(FoldMode, bool)>,
    path_display: Option<String>,
}

impl Scratch {
    /// Forget everything cached for the previous entry.
    #[inline]
    pub fn reset(&mut self) {
        self.path_loose_ready = false;
        self.strict_key = None;
        self.path_display = None;
    }

    fn loose_path(&mut self, s: &Subject) -> &[u8] {
        if !self.path_loose_ready {
            self.path_loose.clear();
            let own = if s.entry.is_dir() { s.dir_paths.and_then(|d| d.get(s.id)) } else { None };
            match own {
                Some(p) => self.path_loose.extend_from_slice(p),
                None => match s.dir_paths.and_then(|d| d.get(s.entry.parent)) {
                    Some(parent) => {
                        self.path_loose.extend_from_slice(parent);
                        self.path_loose.push(b'\\');
                        self.path_loose.extend_from_slice(s.name_loose);
                    }
                    None => {
                        self.path_loose.extend_from_slice(fold::fold(&s.index.path(s.id), FoldMode::LOOSE).as_bytes());
                    }
                },
            }
            self.path_loose_ready = true;
        }
        &self.path_loose
    }

    fn display_path(&mut self, s: &Subject) -> &str {
        self.path_display.get_or_insert_with(|| s.index.path(s.id))
    }

    fn strict_text(&mut self, s: &Subject, mode: FoldMode, path: bool) -> &str {
        if self.strict_key != Some((mode, path)) {
            self.strict.clear();
            if path {
                let p = self.display_path(s).to_string();
                fold::fold_into(&p, mode, &mut self.strict);
            } else {
                fold::fold_into(s.index.name(s.id), mode, &mut self.strict);
            }
            self.strict_key = Some((mode, path));
        }
        &self.strict
    }
}

/// `*` matches any run of characters, `?` exactly one. Matches the whole text.
fn wild_match(p: &[char], s: &str) -> bool {
    let (mut pi, mut si) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        let c = s[si..].chars().next().unwrap_or('\0');
        if pi < p.len() && (p[pi] == '?' || p[pi] == c) {
            pi += 1;
            si += c.len_utf8();
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            let skipped = s[ss..].chars().next().map_or(1, char::len_utf8);
            star = Some((sp, ss + skipped));
            pi = sp + 1;
            si = ss + skipped;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn find_word_bytes(hay: &[u8], finder: &memmem::Finder, nlen: usize) -> bool {
    finder.find_iter(hay).any(|pos| fold::is_whole_word(hay, pos, pos + nlen))
}

/// Substring search in strictly folded text. When accents matter, a match must not
/// be followed by a combining mark (`cafe` must not match the decomposed `café`).
fn find_strict(hay: &str, needle: &str, word: bool, accents: bool) -> bool {
    hay.match_indices(needle).any(|(pos, m)| {
        let end = pos + m.len();
        (!accents || !hay[end..].chars().next().is_some_and(unicode_normalization::char::is_combining_mark))
            && (!word || fold::is_whole_word(hay.as_bytes(), pos, end))
    })
}

fn eval_pred(p: &Pred, s: &Subject, sc: &mut Scratch) -> bool {
    let e = s.entry;
    match p {
        Pred::FileOnly => !e.is_dir(),
        Pred::FolderOnly => e.is_dir(),
        Pred::Size { lo, hi } => !e.is_dir() && e.flags & flags::NO_META == 0 && *lo <= e.size && e.size <= *hi,
        Pred::Date { lo, hi } => e.flags & flags::NO_META == 0 && *lo <= e.mtime && e.mtime < *hi,
        Pred::Ext(list) => !e.is_dir() && types::ext_of(s.name_loose).is_some_and(|x| list.iter().any(|l| l.as_slice() == x)),
        Pred::Group(list) => !e.is_dir() && types::ext_of(s.name_loose).is_some_and(|x| types::in_list(x, list)),
        Pred::Text(t) => {
            {
                let hay: &[u8] = if t.path { sc.loose_path(s) } else { s.name_loose };
                match (&t.strict, t.word) {
                    (None, false) => return t.finder.find(hay).is_some(),
                    (None, true) => return find_word_bytes(hay, &t.finder, t.loose.len()),
                    _ => {
                        if t.finder.find(hay).is_none() {
                            return false;
                        }
                    }
                }
            }
            let needle = t.strict.as_deref().unwrap_or_default();
            let hay = sc.strict_text(s, t.mode, t.path);
            find_strict(hay, needle, t.word, t.mode.diacritics_sensitive)
        }
        Pred::Wild(w) => {
            if w.mode.is_loose() {
                let hay: &[u8] = if w.path { sc.loose_path(s) } else { s.name_loose };
                std::str::from_utf8(hay).is_ok_and(|h| wild_match(&w.pattern, h))
            } else {
                let hay = sc.strict_text(s, w.mode, w.path);
                wild_match(&w.pattern, hay)
            }
        }
        Pred::Regex { re, path } => {
            if *path {
                let p = sc.display_path(s);
                re.is_match(p)
            } else {
                re.is_match(s.index.name(s.id))
            }
        }
    }
}

fn eval(n: &Node, s: &Subject, sc: &mut Scratch) -> bool {
    match n {
        Node::True => true,
        Node::Pred(p) => eval_pred(p, s, sc),
        Node::Not(inner) => !eval(inner, s, sc),
        Node::And(v) => v.iter().all(|c| eval(c, s, sc)),
        Node::Or(v) => v.iter().any(|c| eval(c, s, sc)),
    }
}

/// Parses a `size:` value into an inclusive byte range.
pub fn parse_size_filter(value: &str) -> Result<(u64, u64), String> {
    const KB: u64 = 1024;
    let v = value.trim().to_ascii_lowercase();
    let bad = || format!("\"{value}\" is not a size. Examples: size:>100mb, size:1mb..10mb, size:<=4kb, size:empty");
    if v.is_empty() {
        return Err("size: needs a value, for example size:>100mb or size:1mb..10mb".into());
    }
    match v.as_str() {
        "empty" => return Ok((0, 0)),
        "tiny" => return Ok((0, 10 * KB - 1)),
        "small" => return Ok((10 * KB, 100 * KB - 1)),
        "medium" => return Ok((100 * KB, KB * KB - 1)),
        "large" => return Ok((KB * KB, 16 * KB * KB - 1)),
        "huge" => return Ok((16 * KB * KB, 128 * KB * KB - 1)),
        "gigantic" => return Ok((128 * KB * KB, u64::MAX)),
        _ => {}
    }
    // Returns (bytes, unit size) for "1.5mb", "100", "4kb".
    let num = |s: &str| -> Result<(u64, u64), String> {
        let s = s.trim();
        let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
        let (n, unit) = s.split_at(split);
        if n.is_empty() || n.matches('.').count() > 1 {
            return Err(bad());
        }
        let mult: u64 = match unit.trim() {
            "" | "b" => 1,
            "k" | "kb" | "kib" => KB,
            "m" | "mb" | "mib" => KB * KB,
            "g" | "gb" | "gib" => KB * KB * KB,
            "t" | "tb" | "tib" => KB * KB * KB * KB,
            _ => return Err(bad()),
        };
        let f: f64 = n.parse().map_err(|_| bad())?;
        let bytes = (f * mult as f64).round();
        if !bytes.is_finite() || bytes < 0.0 {
            return Err(bad());
        }
        Ok((if bytes >= u64::MAX as f64 { u64::MAX } else { bytes as u64 }, mult))
    };
    if let Some((a, b)) = v.split_once("..") {
        let lo = if a.is_empty() { 0 } else { num(a)?.0 };
        let hi = if b.is_empty() { u64::MAX } else { num(b)?.0 };
        if lo > hi {
            return Err(format!("The size range \"{value}\" is empty"));
        }
        return Ok((lo, hi));
    }
    for (op, kind) in [(">=", 0), ("<=", 1), (">", 2), ("<", 3), ("=", 4)] {
        if let Some(rest) = v.strip_prefix(op) {
            let (n, _) = num(rest)?;
            return match kind {
                0 => Ok((n, u64::MAX)),
                1 => Ok((0, n)),
                2 => Ok((n.saturating_add(1), u64::MAX)),
                3 if n == 0 => Err(format!("No file is smaller than 0 bytes (\"{value}\")")),
                3 => Ok((0, n - 1)),
                _ => Ok((n, n)),
            };
        }
    }
    let (n, unit) = num(&v)?;
    // "size:1mb" means 1 MB up to (but not including) 2 MB; "size:100" means exactly 100 bytes.
    Ok(if unit == 1 { (n, n) } else { (n, n.saturating_add(unit) - 1) })
}

fn regex_error(e: &regex::Error) -> String {
    let text = e.to_string();
    let detail = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("error: "))
        .map(str::to_string)
        .unwrap_or_else(|| text.lines().next().unwrap_or("invalid pattern").to_string());
    format!("Invalid regular expression: {detail}")
}

struct Compiler<'a> {
    globals: TermFlags,
    opts: &'a QueryOptions,
    clock: Clock,
    rank_needles: Vec<Vec<u8>>,
    hl_needles: Vec<Vec<u8>>,
    hl_regex: Vec<Regex>,
    needs_dir_paths: bool,
    has_regex: bool,
    has_terms: bool,
}

impl Compiler<'_> {
    fn compile(&mut self, ast: Ast, negated: bool) -> Result<Node, QueryError> {
        Ok(match ast {
            Ast::Empty => Node::True,
            Ast::Not(inner) => Node::Not(Box::new(self.compile(*inner, !negated)?)),
            Ast::And(items) => {
                let mut nodes = items.into_iter().map(|a| self.compile(a, negated)).collect::<Result<Vec<_>, _>>()?;
                nodes.sort_by_key(Node::cost);
                Node::And(nodes)
            }
            Ast::Or(items) => Node::Or(items.into_iter().map(|a| self.compile(a, negated)).collect::<Result<Vec<_>, _>>()?),
            Ast::Term(t) => self.term(t, negated)?,
        })
    }

    fn term(&mut self, t: RawTerm, negated: bool) -> Result<Node, QueryError> {
        let ui = TermFlags {
            case: self.opts.match_case,
            word: self.opts.whole_word,
            diacritics: self.opts.match_diacritics,
            path: self.opts.match_path,
        };
        let f = t.flags.merge(self.globals).merge(ui);
        let mode = FoldMode { case_sensitive: f.case, diacritics_sensitive: f.diacritics };
        let pred = match t.kind {
            RawKind::Filter => None,
            RawKind::Text(text) => {
                self.has_terms = true;
                let path = f.path || text.contains('\\') || text.contains('/');
                let text = text.replace('/', "\\");
                if text.contains('*') || text.contains('?') {
                    if path {
                        self.needs_dir_paths = true;
                    }
                    let pattern = fold::fold(&text, mode).chars().collect();
                    Some(Pred::Wild(WildPred { pattern, mode, path }))
                } else {
                    let loose = fold::fold(&text, FoldMode::LOOSE).into_bytes();
                    if loose.is_empty() {
                        return Ok(Node::True);
                    }
                    if path {
                        self.needs_dir_paths = true;
                    }
                    if !negated {
                        self.rank_needles.push(loose.clone());
                        self.hl_needles.push(loose.clone());
                    }
                    let strict = (!mode.is_loose()).then(|| fold::fold(&text, mode));
                    let finder = memmem::Finder::new(&loose).into_owned();
                    Some(Pred::Text(Box::new(TextPred { loose, finder, strict, mode, word: f.word, path })))
                }
            }
            RawKind::Regex(pattern) => {
                if pattern.is_empty() {
                    return Err(QueryError::new("regex: needs a pattern, for example regex:^IMG_\\d{4}"));
                }
                self.has_terms = true;
                self.has_regex = true;
                let re = RegexBuilder::new(&pattern)
                    .case_insensitive(!f.case)
                    .size_limit(8 << 20)
                    .dfa_size_limit(8 << 20)
                    .nest_limit(64)
                    .build()
                    .map_err(|e| QueryError::new(regex_error(&e)))?;
                if !negated && !f.path {
                    self.hl_regex.push(re.clone());
                }
                Some(Pred::Regex { re, path: f.path })
            }
            RawKind::Ext(list) => {
                let exts: Vec<Vec<u8>> = list
                    .split([';', ','])
                    .map(|x| x.trim().trim_start_matches('*').trim_start_matches('.'))
                    .filter(|x| !x.is_empty())
                    .map(|x| fold::fold(x, FoldMode::LOOSE).into_bytes())
                    .collect();
                if exts.is_empty() {
                    return Err(QueryError::new("ext: needs at least one extension, for example ext:pdf;docx"));
                }
                Some(Pred::Ext(exts))
            }
            RawKind::Type(name) => match types::group(name.trim()) {
                Some(list) => Some(Pred::Group(list)),
                None => {
                    return Err(QueryError::new(format!("Unknown type \"{name}\". Use image, video, audio, doc, archive, exe or code")))
                }
            },
            RawKind::Size(v) => {
                let (lo, hi) = parse_size_filter(&v).map_err(QueryError::new)?;
                Some(Pred::Size { lo, hi })
            }
            RawKind::Date(v) => {
                let (lo, hi) = time::parse_date_filter(&v, &self.clock).map_err(QueryError::new)?;
                Some(Pred::Date { lo, hi })
            }
            RawKind::Content(_) => {
                return Err(QueryError::new(
                    "Full-text search (content:) is not part of this version. FindIt searches names and paths only",
                ))
            }
        };
        let mut parts = Vec::new();
        if t.file_only {
            parts.push(Node::Pred(Pred::FileOnly));
        }
        if t.folder_only {
            parts.push(Node::Pred(Pred::FolderOnly));
        }
        if let Some(p) = pred {
            parts.push(Node::Pred(p));
        }
        Ok(match parts.len() {
            0 => Node::True,
            1 => parts.pop().unwrap_or(Node::True),
            _ => Node::And(parts),
        })
    }
}

impl Query {
    /// Parses and compiles `text`. Never panics, whatever the input.
    pub fn parse(text: &str, opts: &QueryOptions) -> Result<Query, QueryError> {
        let mut parser = Parser { toks: tokenize(text), pos: 0, globals: TermFlags::default() };
        let mut ast = parser.parse_seq()?;
        // Stray closing brackets: keep parsing what follows.
        while parser.pos < parser.toks.len() {
            parser.pos += 1;
            let more = parser.parse_seq()?;
            if more != Ast::Empty {
                ast = match ast {
                    Ast::Empty => more,
                    Ast::And(mut v) => {
                        v.push(more);
                        Ast::And(v)
                    }
                    other => Ast::And(vec![other, more]),
                };
            }
        }
        let mut c = Compiler {
            globals: parser.globals,
            opts,
            clock: Clock { now_unix: opts.now_unix, tz_offset_secs: opts.tz_offset_secs },
            rank_needles: Vec::new(),
            hl_needles: Vec::new(),
            hl_regex: Vec::new(),
            needs_dir_paths: false,
            has_regex: false,
            has_terms: false,
        };
        let root = c.compile(ast, false)?;
        Ok(Query {
            root,
            rank_needles: c.rank_needles,
            hl_needles: c.hl_needles,
            hl_regex: c.hl_regex,
            needs_dir_paths: c.needs_dir_paths,
            has_regex: c.has_regex,
            has_terms: c.has_terms,
            source: text.to_string(),
        })
    }

    /// The original query text.
    pub fn source(&self) -> &str {
        &self.source
    }
    /// True when matching needs folded folder paths ([`Index::dir_paths`]).
    pub fn needs_dir_paths(&self) -> bool {
        self.needs_dir_paths
    }
    pub fn has_regex(&self) -> bool {
        self.has_regex
    }
    /// True when the query contains text, wildcard or regex terms (not only filters).
    pub fn has_terms(&self) -> bool {
        self.has_terms
    }
    /// True when relevance ranking has something to rank by.
    pub fn can_rank(&self) -> bool {
        !self.rank_needles.is_empty()
    }

    /// Evaluates the query for one entry.
    #[inline]
    pub fn matches(&self, s: &Subject, sc: &mut Scratch) -> bool {
        eval(&self.root, s, sc)
    }

    /// 0 = exact name (or name without extension), 1 = name starts with a term,
    /// 2 = a term starts at a word boundary, 3 = anywhere.
    pub fn rank(&self, name_loose: &[u8]) -> u8 {
        let mut best = 3u8;
        for n in &self.rank_needles {
            if name_loose == n.as_slice() {
                return 0;
            }
            if let Some(dot) = memchr::memrchr(b'.', name_loose) {
                if dot > 0 && &name_loose[..dot] == n.as_slice() {
                    return 0;
                }
            }
            if name_loose.starts_with(n) {
                best = best.min(1);
                continue;
            }
            if best > 2 && memmem::find_iter(name_loose, n).any(|p| !fold::char_before(name_loose, p).is_some_and(fold::is_word_char)) {
                best = 2;
            }
        }
        best
    }

    /// Ranges to show in bold, in UTF-16 code units (JavaScript string indexes).
    pub fn highlight(&self, name: &str) -> Vec<(u32, u32)> {
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        if !self.hl_needles.is_empty() {
            let (folded, map) = fold::fold_with_map(name, FoldMode::LOOSE);
            for n in &self.hl_needles {
                for pos in memmem::find_iter(folded.as_bytes(), n) {
                    if let Some(r) = fold::map_range(name, &map, pos, pos + n.len()) {
                        ranges.push(r);
                    }
                    if ranges.len() > 64 {
                        break;
                    }
                }
            }
        }
        for re in &self.hl_regex {
            for m in re.find_iter(name).take(64) {
                if m.start() < m.end() {
                    ranges.push((m.start(), m.end()));
                }
            }
        }
        if ranges.is_empty() {
            return Vec::new();
        }
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for (s, e) in ranges {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        let utf16 = |byte: usize| name[..byte].encode_utf16().count() as u32;
        merged.into_iter().map(|(s, e)| (utf16(s), utf16(e))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Meta, VolumeKind};
    use crate::time::{days_from_civil, SECS_PER_DAY};

    const NOW_DAY: i64 = 20_781; // 2026-11-24
    const MB: u64 = 1024 * 1024;

    struct Fixture {
        ix: Index,
        ids: Vec<(String, u32)>,
    }

    fn day(y: i64, m: u32, d: u32) -> i64 {
        days_from_civil(y, m, d) * SECS_PER_DAY + 12 * 3600
    }

    fn fixture() -> Fixture {
        assert_eq!(days_from_civil(2026, 11, 24), NOW_DAY);
        let mut ix = Index::new();
        let slot = ix.add_volume("C:", VolumeKind::Standard, 1);
        let root = ix.volumes()[slot as usize].root;
        let s = ix.next_stamp();
        let dir = |m: i64| Meta { size: 0, mtime: m, flags: flags::DIR };
        let file = |size: u64, m: i64| Meta { size, mtime: m, flags: 0 };
        let mut ids = Vec::new();
        let projects = ix.upsert(root, "Projects", None, None, dir(day(2026, 1, 5)), s);
        let p2026 = ix.upsert(projects, "2026", None, None, dir(day(2026, 1, 5)), s);
        let nm = ix.upsert(p2026, "node_modules", None, None, dir(day(2026, 2, 1)), s);
        let docs = ix.upsert(root, "Docs", None, None, dir(day(2025, 6, 1)), s);
        let items: Vec<(u32, &str, u64, i64)> = vec![
            (docs, "Invoice_2024.pdf", 200 * 1024, day(2024, 3, 1)),
            (docs, "Annual Report 2025.docx", 3 * MB, day(2025, 12, 20)),
            (docs, "annual-budget.xlsx", 50 * 1024, day(2026, 11, 24)),
            (docs, "README.md", 1200, day(2026, 11, 20)),
            (docs, "readme.txt", 0, day(2026, 10, 1)),
            (docs, "backup_report.zip", 150 * MB, day(2026, 11, 23)),
            (p2026, "IMG_2041.jpg", 4 * MB, day(2026, 7, 4)),
            (p2026, "IMG_20410.jpg", 5 * MB, day(2026, 7, 5)),
            (p2026, "photo.png", 2 * MB, day(2026, 7, 6)),
            (p2026, "clip.mp4", 800 * MB, day(2026, 7, 7)),
            (p2026, "song.mp3", 6 * MB, day(2026, 7, 8)),
            (p2026, "setup.exe", 70 * MB, day(2026, 7, 9)),
            (p2026, "main.rs", 9000, day(2026, 11, 22)),
            (nm, "index.js", 300, day(2026, 2, 1)),
            (docs, "Документ.txt", 10, day(2026, 1, 1)),
            (docs, "Έγγραφο.txt", 10, day(2026, 1, 1)),
            (docs, "報告書.pdf", 10, day(2026, 1, 1)),
            (docs, "Café menu.pdf", 10, day(2026, 1, 1)),
            (docs, "Cafe\u{301} list.pdf", 10, day(2026, 1, 1)),
            (docs, "party 🎉.png", 10, day(2026, 1, 1)),
            (docs, "ΟΔΟΣ.txt", 10, day(2026, 1, 1)),
        ];
        for (parent, name, size, m) in items {
            let id = ix.upsert(parent, name, None, None, file(size, m), s);
            ids.push((name.to_string(), id));
        }
        ids.push(("Projects".into(), projects));
        ids.push(("2026".into(), p2026));
        ids.push(("node_modules".into(), nm));
        ids.push(("Docs".into(), docs));
        Fixture { ix, ids }
    }

    fn opts() -> QueryOptions {
        QueryOptions { now_unix: NOW_DAY * SECS_PER_DAY + 15 * 3600, tz_offset_secs: 0, ..Default::default() }
    }

    fn run_with(f: &Fixture, q: &str, o: &QueryOptions) -> Vec<String> {
        let query = Query::parse(q, o).unwrap_or_else(|e| panic!("query {q:?} failed: {e}"));
        let dp = f.ix.dir_paths();
        let mut sc = Scratch::default();
        let mut out = Vec::new();
        for (name, id) in &f.ids {
            let e = f.ix.entry(*id).unwrap();
            let s = Subject { index: &f.ix, id: *id, entry: e, name_loose: f.ix.folded_bytes(*id), dir_paths: Some(&dp) };
            sc.reset();
            if query.matches(&s, &mut sc) {
                out.push(name.clone());
            }
        }
        out.sort();
        out
    }

    fn run(f: &Fixture, q: &str) -> Vec<String> {
        run_with(f, q, &opts())
    }

    fn names(v: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    }

    fn err(q: &str) -> String {
        Query::parse(q, &opts()).expect_err(q).message
    }

    // --- AND / OR / NOT / grouping -------------------------------------------------

    #[test]
    fn plain_words_are_and_in_any_order() {
        let f = fixture();
        assert_eq!(run(&f, "inv 2024"), names(&["Invoice_2024.pdf"]));
        assert_eq!(run(&f, "2024 inv"), names(&["Invoice_2024.pdf"]));
    }

    #[test]
    fn and_requires_all_terms() {
        let f = fixture();
        assert_eq!(run(&f, "report budget"), Vec::<String>::new());
        assert_eq!(run(&f, "annual report"), names(&["Annual Report 2025.docx"]));
    }

    #[test]
    fn or_alternatives() {
        let f = fixture();
        assert_eq!(run(&f, "jpg|png"), names(&["IMG_2041.jpg", "IMG_20410.jpg", "party 🎉.png", "photo.png"]));
    }

    #[test]
    fn or_binds_tighter_than_and() {
        let f = fixture();
        assert_eq!(run(&f, "img jpg|png"), names(&["IMG_2041.jpg", "IMG_20410.jpg"]));
    }

    #[test]
    fn not_excludes() {
        let f = fixture();
        assert_eq!(run(&f, "report !backup"), names(&["Annual Report 2025.docx"]));
    }

    #[test]
    fn double_not_cancels() {
        let f = fixture();
        assert_eq!(run(&f, "!!invoice"), names(&["Invoice_2024.pdf"]));
    }

    #[test]
    fn not_group() {
        let f = fixture();
        assert_eq!(run(&f, "readme !<md|zip>"), names(&["readme.txt"]));
    }

    #[test]
    fn group_with_or() {
        let f = fixture();
        assert_eq!(run(&f, "<img 2041.>|song"), names(&["IMG_2041.jpg", "song.mp3"]));
    }

    #[test]
    fn unbalanced_groups_are_tolerated() {
        let f = fixture();
        assert_eq!(run(&f, "<song"), names(&["song.mp3"]));
        assert_eq!(run(&f, "song>"), Vec::<String>::new());
        assert_eq!(run(&f, "> song"), names(&["song.mp3"]));
    }

    #[test]
    fn empty_query_matches_everything() {
        let f = fixture();
        assert_eq!(run(&f, "").len(), f.ids.len());
        assert_eq!(run(&f, "   ").len(), f.ids.len());
    }

    #[test]
    fn stray_operators_do_not_crash() {
        let f = fixture();
        for q in ["|", "||", "!", "!!", "| song", "song |", "<>", "<<>>", "\"", "\"\"", "!|!", ">>>", "<|>"] {
            let _ = run(&f, q);
        }
        assert_eq!(run(&f, "song |"), names(&["song.mp3"]));
    }

    // --- phrases -------------------------------------------------------------------

    #[test]
    fn exact_phrase() {
        let f = fixture();
        assert_eq!(run(&f, "\"annual report\""), names(&["Annual Report 2025.docx"]));
        assert_eq!(run(&f, "\"report annual\""), Vec::<String>::new());
    }

    #[test]
    fn unclosed_quote_runs_to_end() {
        let f = fixture();
        assert_eq!(run(&f, "\"annual report"), names(&["Annual Report 2025.docx"]));
    }

    #[test]
    fn quoted_modifier_is_literal() {
        let f = fixture();
        assert_eq!(run(&f, "\"size:>1\""), Vec::<String>::new());
    }

    // --- wildcards -----------------------------------------------------------------

    #[test]
    fn star_wildcard_matches_whole_name() {
        let f = fixture();
        assert_eq!(run(&f, "*.docx"), names(&["Annual Report 2025.docx"]));
        assert_eq!(run(&f, "read*"), names(&["README.md", "readme.txt"]));
        assert_eq!(run(&f, "*eadm"), Vec::<String>::new());
    }

    #[test]
    fn question_mark_matches_one_char() {
        let f = fixture();
        assert_eq!(run(&f, "IMG_????.jpg"), names(&["IMG_2041.jpg"]));
        assert_eq!(run(&f, "IMG_?????.jpg"), names(&["IMG_20410.jpg"]));
    }

    #[test]
    fn wildcard_with_unicode() {
        let f = fixture();
        assert_eq!(run(&f, "*.txt документ*"), names(&["Документ.txt"]));
        assert_eq!(run(&f, "документ.???"), names(&["Документ.txt"]));
        assert_eq!(run(&f, "\"party ?.png\""), names(&["party 🎉.png"]));
        assert_eq!(run(&f, "party ?.png"), Vec::<String>::new());
    }

    #[test]
    fn wildcard_case_sensitive() {
        let f = fixture();
        assert_eq!(run(&f, "case:README.*"), names(&["README.md"]));
    }

    #[test]
    fn wild_match_backtracks() {
        let p: Vec<char> = "a*b*c".chars().collect();
        assert!(wild_match(&p, "axxbyyc"));
        assert!(wild_match(&p, "abcbc"));
        assert!(!wild_match(&p, "acb"));
        assert!(wild_match(&['*'], ""));
        assert!(!wild_match(&['?'], ""));
    }

    // --- regex ---------------------------------------------------------------------

    #[test]
    fn regex_term() {
        let f = fixture();
        assert_eq!(run(&f, "regex:^IMG_\\d{4}\\."), names(&["IMG_2041.jpg"]));
        assert_eq!(run(&f, "regex:^img_"), names(&["IMG_2041.jpg", "IMG_20410.jpg"]));
    }

    #[test]
    fn regex_respects_case_toggle() {
        let f = fixture();
        assert_eq!(run(&f, "case: regex:^img_"), Vec::<String>::new());
    }

    #[test]
    fn regex_quoted_with_space() {
        let f = fixture();
        assert_eq!(run(&f, "regex:\"^Annual Report \\d+\""), names(&["Annual Report 2025.docx"]));
    }

    #[test]
    fn regex_against_path() {
        let f = fixture();
        assert_eq!(run(&f, "path:regex:node_modules\\\\index"), names(&["index.js"]));
    }

    #[test]
    fn invalid_regex_reports_error() {
        let m = err("regex:(abc");
        assert!(m.starts_with("Invalid regular expression"), "{m}");
        assert!(err("regex:").contains("needs a pattern"));
    }

    #[test]
    fn huge_regex_is_rejected_not_hung() {
        let m = err("regex:(\\w{1000}){1000}");
        assert!(m.starts_with("Invalid regular expression"), "{m}");
    }

    // --- path ------------------------------------------------------------------------

    #[test]
    fn path_modifier() {
        let f = fixture();
        assert_eq!(run(&f, "path:projects\\2026 jpg"), names(&["IMG_2041.jpg", "IMG_20410.jpg"]));
    }

    #[test]
    fn backslash_switches_to_path_matching() {
        let f = fixture();
        assert_eq!(run(&f, "docs\\readme"), names(&["README.md", "readme.txt"]));
        assert_eq!(run(&f, "docs/readme"), names(&["README.md", "readme.txt"]));
    }

    #[test]
    fn match_path_option() {
        let f = fixture();
        let o = QueryOptions { match_path: true, ..opts() };
        assert_eq!(run_with(&f, "node_modules", &o), names(&["index.js", "node_modules"]));
        assert_eq!(run(&f, "node_modules"), names(&["node_modules"]));
    }

    #[test]
    fn path_wildcard() {
        let f = fixture();
        assert_eq!(run(&f, "c:\\docs\\*.md"), names(&["README.md"]));
    }

    #[test]
    fn drive_letter_is_not_a_filter() {
        let f = fixture();
        assert_eq!(run(&f, "c:\\projects\\2026\\setup"), names(&["setup.exe"]));
    }

    // --- ext / type ------------------------------------------------------------------

    #[test]
    fn ext_list() {
        let f = fixture();
        assert_eq!(
            run(&f, "ext:pdf;docx"),
            names(&["Annual Report 2025.docx", "Café menu.pdf", "Cafe\u{301} list.pdf", "Invoice_2024.pdf", "報告書.pdf"])
        );
        assert_eq!(run(&f, "ext:.MD"), names(&["README.md"]));
        assert_eq!(run(&f, "ext:*.zip,exe"), names(&["backup_report.zip", "setup.exe"]));
    }

    #[test]
    fn ext_empty_is_error() {
        assert!(err("ext:").contains("at least one extension"));
        assert!(err("ext:;;").contains("at least one extension"));
    }

    #[test]
    fn type_groups() {
        let f = fixture();
        assert_eq!(run(&f, "type:image"), names(&["IMG_2041.jpg", "IMG_20410.jpg", "party 🎉.png", "photo.png"]));
        assert_eq!(run(&f, "type:video"), names(&["clip.mp4"]));
        assert_eq!(run(&f, "type:audio"), names(&["song.mp3"]));
        assert_eq!(run(&f, "type:archive"), names(&["backup_report.zip"]));
        assert_eq!(run(&f, "type:exe"), names(&["setup.exe"]));
        assert_eq!(run(&f, "type:code"), names(&["index.js", "main.rs"]));
        assert!(run(&f, "type:doc").contains(&"README.md".to_string()));
    }

    #[test]
    fn unknown_type_is_error() {
        assert!(err("type:spaceship").contains("Unknown type"));
    }

    // --- size ------------------------------------------------------------------------

    #[test]
    fn size_comparisons() {
        let f = fixture();
        assert_eq!(run(&f, "size:>100mb"), names(&["backup_report.zip", "clip.mp4"]));
        assert_eq!(
            run(&f, "size:<=1kb"),
            names(&[
                "index.js",
                "readme.txt",
                "party 🎉.png",
                "Café menu.pdf",
                "Cafe\u{301} list.pdf",
                "Документ.txt",
                "Έγγραφο.txt",
                "報告書.pdf",
                "ΟΔΟΣ.txt"
            ])
        );
        assert_eq!(run(&f, "size:1mb..5mb"), names(&["Annual Report 2025.docx", "IMG_2041.jpg", "IMG_20410.jpg", "photo.png"]));
        assert_eq!(run(&f, "size:=1200"), names(&["README.md"]));
    }

    #[test]
    fn size_keywords_and_units() {
        let f = fixture();
        assert_eq!(run(&f, "size:empty"), names(&["readme.txt"]));
        assert_eq!(run(&f, "size:gigantic"), names(&["backup_report.zip", "clip.mp4"]));
        assert_eq!(run(&f, "size:4mb"), names(&["IMG_2041.jpg"]));
        assert_eq!(parse_size_filter("1.5kb").unwrap(), (1536, 2559));
        assert_eq!(parse_size_filter(">=2g").unwrap().0, 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn size_excludes_folders() {
        let f = fixture();
        assert!(!run(&f, "size:<1kb").contains(&"Docs".to_string()));
    }

    #[test]
    fn bad_sizes_are_errors() {
        assert!(err("size:").contains("needs a value"));
        assert!(err("size:big").contains("is not a size"));
        assert!(err("size:10xb").contains("is not a size"));
        assert!(err("size:<0").contains("smaller than 0"));
        assert!(err("size:10mb..1mb").contains("empty"));
    }

    // --- dm ----------------------------------------------------------------------------

    #[test]
    fn date_keywords() {
        let f = fixture();
        assert_eq!(run(&f, "dm:today"), names(&["annual-budget.xlsx"]));
        assert_eq!(run(&f, "dm:yesterday"), names(&["backup_report.zip"]));
        assert_eq!(run(&f, "dm:last7days !folder:"), names(&["README.md", "annual-budget.xlsx", "backup_report.zip", "main.rs"]));
        assert_eq!(run(&f, "dm:thisweek"), names(&["annual-budget.xlsx", "backup_report.zip"]));
    }

    #[test]
    fn date_ranges_and_comparisons() {
        let f = fixture();
        assert_eq!(run(&f, "dm:2026-07-04..2026-07-05"), names(&["IMG_2041.jpg", "IMG_20410.jpg"]));
        assert_eq!(run(&f, "dm:<2025-01-01"), names(&["Invoice_2024.pdf"]));
        assert_eq!(run(&f, "dm:2025 file:"), names(&["Annual Report 2025.docx"]));
    }

    #[test]
    fn bad_dates_are_errors() {
        assert!(err("dm:").contains("needs a value"));
        assert!(err("dm:someday").contains("is not a date"));
        assert!(err("dm:2026-13-01").contains("is not a date"));
    }

    // --- file: / folder: ------------------------------------------------------------

    #[test]
    fn folder_filter() {
        let f = fixture();
        assert_eq!(run(&f, "folder: node_modules"), names(&["node_modules"]));
        assert_eq!(run(&f, "folder:2026"), names(&["2026"]));
        assert_eq!(run(&f, "folder:").len(), 4);
    }

    #[test]
    fn file_filter() {
        let f = fixture();
        assert_eq!(run(&f, "file:2026"), Vec::<String>::new());
        assert_eq!(run(&f, "file: 2025"), names(&["Annual Report 2025.docx"]));
    }

    #[test]
    fn not_folder() {
        let f = fixture();
        assert_eq!(run(&f, "!folder: 2026"), Vec::<String>::new());
    }

    // --- case / word / diacritics ----------------------------------------------------

    #[test]
    fn case_term_modifier() {
        let f = fixture();
        assert_eq!(run(&f, "case:README"), names(&["README.md"]));
        assert_eq!(run(&f, "README"), names(&["README.md", "readme.txt"]));
    }

    #[test]
    fn case_global_toggle() {
        let f = fixture();
        assert_eq!(run(&f, "case: Readme"), Vec::<String>::new());
        assert_eq!(run(&f, "readme case:"), names(&["readme.txt"]));
    }

    #[test]
    fn match_case_option() {
        let f = fixture();
        let o = QueryOptions { match_case: true, ..opts() };
        assert_eq!(run_with(&f, "IMG", &o), names(&["IMG_2041.jpg", "IMG_20410.jpg"]));
        assert_eq!(run_with(&f, "img", &o), Vec::<String>::new());
    }

    #[test]
    fn whole_word() {
        let f = fixture();
        assert_eq!(run(&f, "word:report"), names(&["Annual Report 2025.docx", "backup_report.zip"]));
        assert_eq!(run(&f, "word:repo"), Vec::<String>::new());
        assert_eq!(run(&f, "word: annual"), names(&["Annual Report 2025.docx", "annual-budget.xlsx"]));
    }

    #[test]
    fn whole_word_case_sensitive() {
        let f = fixture();
        assert_eq!(run(&f, "case:word:Report"), names(&["Annual Report 2025.docx"]));
    }

    #[test]
    fn diacritics_insensitive_by_default() {
        let f = fixture();
        assert_eq!(run(&f, "cafe"), names(&["Café menu.pdf", "Cafe\u{301} list.pdf"]));
        assert_eq!(run(&f, "café"), names(&["Café menu.pdf", "Cafe\u{301} list.pdf"]));
    }

    #[test]
    fn diacritics_toggle() {
        let f = fixture();
        assert_eq!(run(&f, "diacritics:cafe"), Vec::<String>::new());
        // Composed and decomposed é are the same character.
        assert_eq!(run(&f, "diacritics:café"), names(&["Café menu.pdf", "Cafe\u{301} list.pdf"]));
    }

    // --- Unicode ------------------------------------------------------------------------

    #[test]
    fn cyrillic_case_insensitive() {
        let f = fixture();
        assert_eq!(run(&f, "документ"), names(&["Документ.txt"]));
        assert_eq!(run(&f, "ДОКУМЕНТ"), names(&["Документ.txt"]));
    }

    #[test]
    fn greek_with_accents_and_sigma() {
        let f = fixture();
        assert_eq!(run(&f, "εγγραφο"), names(&["Έγγραφο.txt"]));
        assert_eq!(run(&f, "οδος"), names(&["ΟΔΟΣ.txt"]));
    }

    #[test]
    fn cjk_and_emoji() {
        let f = fixture();
        assert_eq!(run(&f, "報告"), names(&["報告書.pdf"]));
        assert_eq!(run(&f, "🎉"), names(&["party 🎉.png"]));
    }

    // --- errors ---------------------------------------------------------------------------

    #[test]
    fn unknown_filter_is_error() {
        assert!(err("colour:red").contains("Unknown filter"));
    }

    #[test]
    fn content_is_explained() {
        assert!(err("content:\"invoice number\"").contains("not part of this version"));
    }

    #[test]
    fn fuzz_never_panics() {
        let f = fixture();
        let alphabet = ['a', '|', '!', '<', '>', '"', ':', '*', '?', '\\', ' ', '.', 'é', '🎉', 'd', 'm', 's', 'z'];
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..3000 {
            let len = (seed % 12) as usize;
            let mut q = String::new();
            for _ in 0..len {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                q.push(alphabet[(seed % alphabet.len() as u64) as usize]);
            }
            if let Ok(query) = Query::parse(&q, &opts()) {
                let dp = f.ix.dir_paths();
                let mut sc = Scratch::default();
                for (_, id) in &f.ids {
                    let s = Subject {
                        index: &f.ix,
                        id: *id,
                        entry: f.ix.entry(*id).unwrap(),
                        name_loose: f.ix.folded_bytes(*id),
                        dir_paths: Some(&dp),
                    };
                    sc.reset();
                    let _ = query.matches(&s, &mut sc);
                }
            }
        }
    }

    // --- ranking and highlighting -------------------------------------------------------

    #[test]
    fn ranking_order() {
        let q = Query::parse("readme", &opts()).unwrap();
        assert_eq!(q.rank(b"readme"), 0);
        assert_eq!(q.rank(b"readme.md"), 0);
        assert_eq!(q.rank(b"readme-old.md"), 1);
        assert_eq!(q.rank(b"old readme.md"), 2);
        assert_eq!(q.rank(b"oldreadme.md"), 3);
        let neg = Query::parse("!readme", &opts()).unwrap();
        assert!(!neg.can_rank());
    }

    #[test]
    fn highlight_maps_to_utf16() {
        let q = Query::parse("report", &opts()).unwrap();
        assert_eq!(q.highlight("Annual Report.docx"), vec![(7, 13)]);
        let q = Query::parse("🎉 png", &opts()).unwrap();
        // The emoji is two UTF-16 units.
        assert_eq!(q.highlight("party 🎉.png"), vec![(6, 8), (9, 12)]);
        let q = Query::parse("cafe", &opts()).unwrap();
        assert_eq!(q.highlight("Cafe\u{301} list"), vec![(0, 5)]);
        let q = Query::parse("regex:\\d+", &opts()).unwrap();
        assert_eq!(q.highlight("IMG_2041.jpg"), vec![(4, 8)]);
    }

    #[test]
    fn tokenizer_shapes() {
        assert_eq!(tokenize("a|b"), vec![Tok::Word { text: "a".into(), plain: 1 }, Tok::Or, Tok::Word { text: "b".into(), plain: 1 }]);
        assert_eq!(tokenize("!x"), vec![Tok::Not, Tok::Word { text: "x".into(), plain: 1 }]);
        assert_eq!(tokenize("<a>"), vec![Tok::Open, Tok::Word { text: "a".into(), plain: 1 }, Tok::Close]);
        assert_eq!(tokenize("size:>1mb"), vec![Tok::Word { text: "size:>1mb".into(), plain: 9 }]);
        assert_eq!(tokenize("regex:\"a b\""), vec![Tok::Word { text: "regex:a b".into(), plain: 6 }]);
        assert_eq!(tokenize("hello!"), vec![Tok::Word { text: "hello!".into(), plain: 6 }]);
    }
}
