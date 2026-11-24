//! Parallel search over the index.
//!
//! The entry array is split into fixed-size chunks that rayon scans on all cores.
//! Each chunk checks the cancel flag first, so a new keystroke stops an old search
//! within one chunk (a fraction of a millisecond).

use crate::index::{flags, Index};
use crate::query::{Query, Scratch, Subject};
use crate::types;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

const CHUNK: usize = 16 * 1024;

/// Quick filter buttons above the results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Chip {
    #[default]
    All,
    Files,
    Folders,
    Documents,
    Images,
    Video,
    Audio,
    Archives,
    Apps,
}

/// Result ordering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SortColumn {
    /// Exact name, then prefix, then word start, then anywhere; ties by shorter
    /// path, then newer date. Queries without text terms sort by name.
    #[default]
    Relevance,
    Name,
    Path,
    Size,
    Modified,
    Type,
}

#[derive(Clone, Debug)]
pub struct SearchOptions {
    pub include_hidden: bool,
    pub include_system: bool,
    pub chip: Chip,
    pub sort: SortColumn,
    pub descending: bool,
    /// 0 = no limit.
    pub limit: usize,
    /// Regex searches stop (with partial results) after this long.
    pub regex_budget: Duration,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            include_hidden: false,
            include_system: false,
            chip: Chip::All,
            sort: SortColumn::Relevance,
            descending: false,
            limit: 0,
            regex_budget: Duration::from_secs(3),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SearchOutcome {
    pub ids: Vec<u32>,
    /// Matches before the result limit was applied.
    pub total: usize,
    pub limited: bool,
    /// A regex search ran out of time and returned what it had.
    pub timed_out: bool,
    pub epoch: u64,
    pub generation: u64,
    pub elapsed: Duration,
}

#[inline]
fn chip_ok(chip: Chip, is_dir: bool, name_loose: &[u8]) -> bool {
    let group = match chip {
        Chip::All => return true,
        Chip::Files => return !is_dir,
        Chip::Folders => return is_dir,
        Chip::Documents => types::DOC,
        Chip::Images => types::IMAGE,
        Chip::Video => types::VIDEO,
        Chip::Audio => types::AUDIO,
        Chip::Archives => types::ARCHIVE,
        Chip::Apps => types::EXE,
    };
    !is_dir && types::ext_of(name_loose).is_some_and(|x| types::in_list(x, group))
}

/// Packs (rank, path length, newer-first date) into one sortable integer.
#[inline]
fn relevance_key(rank: u8, path_len: u32, mtime: i64) -> u64 {
    const MT_MAX: u64 = (1 << 46) - 1;
    let mt = (mtime.max(0) as u64).min(MT_MAX);
    (u64::from(rank.min(3)) << 62) | (u64::from(path_len.min(0xFFFF)) << 46) | (MT_MAX - mt)
}

/// Runs `query` over `index`. Returns `None` when `cancel` fired.
pub fn search(index: &Index, query: &Query, opts: &SearchOptions, cancel: &(dyn Fn() -> bool + Sync)) -> Option<SearchOutcome> {
    let started = Instant::now();
    let n = index.len();
    let entries = index.entries();
    let dir_paths = if query.needs_dir_paths() || opts.sort == SortColumn::Path { Some(index.dir_paths()) } else { None };
    let rank = opts.sort != SortColumn::Name && query.can_rank();
    let path_lens = if rank { Some(index.path_lens()) } else { None };
    let deadline = query.has_regex().then(|| started + opts.regex_budget);
    let cancelled = AtomicBool::new(false);
    let timed_out = AtomicBool::new(false);
    let hidden_mask = (if opts.include_hidden { 0 } else { flags::HIDDEN }) | (if opts.include_system { 0 } else { flags::SYSTEM });
    let skip_mask = flags::DELETED | flags::ROOT | flags::PENDING | hidden_mask;

    let chunks: Vec<Vec<(u64, u32)>> = (0..n.div_ceil(CHUNK))
        .into_par_iter()
        .map(|c| {
            if cancelled.load(AtomicOrdering::Relaxed) || cancel() {
                cancelled.store(true, AtomicOrdering::Relaxed);
                return Vec::new();
            }
            if deadline.is_some_and(|d| Instant::now() > d) {
                timed_out.store(true, AtomicOrdering::Relaxed);
                return Vec::new();
            }
            let mut out = Vec::new();
            let mut scratch = Scratch::default();
            let dp = dir_paths.as_deref();
            let start = c * CHUNK;
            for (offset, e) in entries[start..((c + 1) * CHUNK).min(n)].iter().enumerate() {
                if e.flags & skip_mask != 0 {
                    continue;
                }
                let id = (start + offset) as u32;
                let name_loose = index.folded_bytes(id);
                if !chip_ok(opts.chip, e.is_dir(), name_loose) {
                    continue;
                }
                scratch.reset();
                let subject = Subject { index, id, entry: e, name_loose, dir_paths: dp };
                if !query.matches(&subject, &mut scratch) {
                    continue;
                }
                let key = match &path_lens {
                    Some(pl) => relevance_key(query.rank(name_loose), pl[id as usize], e.mtime),
                    None => 0,
                };
                out.push((key, id));
            }
            out
        })
        .collect();
    if cancelled.load(AtomicOrdering::Relaxed) || cancel() {
        return None;
    }
    let total: usize = chunks.iter().map(Vec::len).sum();
    let mut hits: Vec<(u64, u32)> = Vec::with_capacity(total);
    for c in chunks {
        hits.extend(c);
    }

    let ids = order(index, hits, rank, opts, dir_paths.as_deref());
    if cancel() {
        return None;
    }
    let mut ids = ids;
    let limited = opts.limit > 0 && ids.len() > opts.limit;
    if limited {
        ids.truncate(opts.limit);
    }
    Some(SearchOutcome {
        ids,
        total,
        limited,
        timed_out: timed_out.load(AtomicOrdering::Relaxed),
        epoch: index.epoch(),
        generation: index.generation(),
        elapsed: started.elapsed(),
    })
}

fn by_name_from_cache(index: &Index, hits: &[(u64, u32)]) -> Vec<u32> {
    let mut member = vec![false; index.len()];
    for &(_, id) in hits {
        member[id as usize] = true;
    }
    index.name_order().iter().copied().filter(|&id| member[id as usize]).collect()
}

fn order(index: &Index, mut hits: Vec<(u64, u32)>, ranked: bool, opts: &SearchOptions, dp: Option<&crate::index::DirPaths>) -> Vec<u32> {
    let by_name = opts.sort == SortColumn::Name || (opts.sort == SortColumn::Relevance && !ranked);
    if by_name {
        // Large result sets reuse the cached name order (one pass instead of a sort).
        let mut ids = if hits.len() > 200_000 {
            by_name_from_cache(index, &hits)
        } else {
            hits.par_sort_unstable_by(|a, b| index.cmp_names(a.1, b.1));
            hits.into_iter().map(|(_, id)| id).collect()
        };
        if opts.descending {
            ids.reverse();
        }
        return ids;
    }
    if opts.sort == SortColumn::Relevance {
        hits.par_sort_unstable();
        let mut ids: Vec<u32> = hits.into_iter().map(|(_, id)| id).collect();
        if opts.descending {
            ids.reverse();
        }
        return ids;
    }
    let entries = index.entries();
    let column = |a: u32, b: u32| -> Ordering {
        let (ea, eb) = (&entries[a as usize], &entries[b as usize]);
        match opts.sort {
            SortColumn::Size => ea.size.cmp(&eb.size).then_with(|| ea.is_dir().cmp(&eb.is_dir()).reverse()),
            SortColumn::Modified => ea.mtime.cmp(&eb.mtime),
            SortColumn::Type => eb.is_dir().cmp(&ea.is_dir()).then_with(|| {
                let xa = if ea.is_dir() { None } else { types::ext_of(index.folded_bytes(a)) };
                let xb = if eb.is_dir() { None } else { types::ext_of(index.folded_bytes(b)) };
                xa.cmp(&xb).then_with(|| index.folded_bytes(a).cmp(index.folded_bytes(b)))
            }),
            SortColumn::Path => {
                let pa = dp.and_then(|d| d.get(ea.parent)).unwrap_or_default();
                let pb = dp.and_then(|d| d.get(eb.parent)).unwrap_or_default();
                pa.cmp(pb).then_with(|| index.folded_bytes(a).cmp(index.folded_bytes(b)))
            }
            SortColumn::Name | SortColumn::Relevance => index.folded_bytes(a).cmp(index.folded_bytes(b)),
        }
    };
    // The relevance key and the id break ties, so the order is total and repeatable:
    // rows never jump around when the same query runs again.
    hits.par_sort_unstable_by(|a, b| {
        let primary = column(a.1, b.1);
        let primary = if opts.descending { primary.reverse() } else { primary };
        primary.then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1))
    });
    hits.into_iter().map(|(_, id)| id).collect()
}

/// Position of `id` in a result list (linear scan; fast enough for millions).
pub fn position_of(ids: &[u32], id: u32) -> Option<usize> {
    ids.par_iter().position_first(|&x| x == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Meta, VolumeKind};
    use crate::query::QueryOptions;

    fn build() -> Index {
        let mut ix = Index::new();
        let slot = ix.add_volume("C:", VolumeKind::Standard, 1);
        let root = ix.volumes()[slot as usize].root;
        let s = ix.next_stamp();
        let deep = ix.upsert(root, "very", None, None, Meta { flags: flags::DIR, ..Default::default() }, s);
        let deeper = ix.upsert(deep, "deep", None, None, Meta { flags: flags::DIR, ..Default::default() }, s);
        for (p, name, size, mtime, fl) in [
            (root, "readme", 10, 100, 0),
            (deeper, "readme.md", 20, 300, 0),
            (root, "readme.md", 30, 200, 0),
            (root, "my readme.txt", 40, 400, 0),
            (root, "oldreadme.txt", 50, 500, 0),
            (root, "readme-notes.txt", 60, 600, 0),
            (root, "hidden readme.txt", 70, 700, flags::HIDDEN),
            (root, "system readme.txt", 80, 800, flags::SYSTEM),
        ] {
            ix.upsert(p, name, None, None, Meta { size, mtime, flags: fl }, s);
        }
        ix
    }

    fn names(ix: &Index, q: &str, o: &SearchOptions) -> Vec<String> {
        let query = Query::parse(q, &QueryOptions::default()).unwrap();
        let out = search(ix, &query, o, &|| false).unwrap();
        out.ids.iter().map(|&id| ix.name(id).to_string()).collect()
    }

    #[test]
    fn relevance_order() {
        let ix = build();
        let got = names(&ix, "readme", &SearchOptions::default());
        assert_eq!(got, vec!["readme", "readme.md", "readme.md", "readme-notes.txt", "my readme.txt", "oldreadme.txt"]);
        // Same rank: the shorter path comes first.
        let paths: Vec<String> = {
            let q = Query::parse("readme.md", &QueryOptions::default()).unwrap();
            search(&ix, &q, &SearchOptions::default(), &|| false).unwrap().ids.iter().map(|&i| ix.path(i)).collect()
        };
        assert_eq!(paths, vec!["C:\\readme.md", "C:\\very\\deep\\readme.md"]);
    }

    #[test]
    fn hidden_and_system_toggles() {
        let ix = build();
        let o = SearchOptions { include_hidden: true, include_system: true, ..Default::default() };
        assert_eq!(names(&ix, "readme", &o).len(), 8);
    }

    #[test]
    fn column_sorts_are_total_and_reversible() {
        let ix = build();
        let o = SearchOptions { sort: SortColumn::Size, ..Default::default() };
        let asc = names(&ix, "readme", &o);
        assert_eq!(asc.first().unwrap(), "readme");
        let desc = names(&ix, "readme", &SearchOptions { descending: true, ..o.clone() });
        assert_eq!(desc.first().unwrap(), "readme-notes.txt");
        let by_date = names(&ix, "readme", &SearchOptions { sort: SortColumn::Modified, descending: true, ..Default::default() });
        assert_eq!(by_date.first().unwrap(), "readme-notes.txt");
        let by_name = names(&ix, "readme", &SearchOptions { sort: SortColumn::Name, ..Default::default() });
        assert_eq!(by_name.first().unwrap(), "my readme.txt");
        let by_type = names(&ix, "", &SearchOptions { sort: SortColumn::Type, ..Default::default() });
        assert_eq!(&by_type[..2], &["deep", "very"]);
        let by_path = names(&ix, "readme.md", &SearchOptions { sort: SortColumn::Path, descending: true, ..Default::default() });
        assert_eq!(by_path.len(), 2);
        // Running the same sort twice gives the same order.
        assert_eq!(names(&ix, "readme", &o), asc);
    }

    #[test]
    fn chips_filter() {
        let ix = build();
        assert_eq!(names(&ix, "", &SearchOptions { chip: Chip::Folders, ..Default::default() }), vec!["deep", "very"]);
        assert_eq!(names(&ix, "readme", &SearchOptions { chip: Chip::Documents, ..Default::default() }).len(), 5);
    }

    #[test]
    fn limit_and_cancel() {
        let ix = build();
        let q = Query::parse("readme", &QueryOptions::default()).unwrap();
        let out = search(&ix, &q, &SearchOptions { limit: 2, ..Default::default() }, &|| false).unwrap();
        assert_eq!(out.ids.len(), 2);
        assert_eq!(out.total, 6);
        assert!(out.limited);
        assert!(search(&ix, &q, &SearchOptions::default(), &|| true).is_none());
        assert_eq!(position_of(&out.ids, out.ids[1]), Some(1));
    }

    #[test]
    fn empty_query_lists_everything_by_name() {
        let ix = build();
        let got = names(&ix, "", &SearchOptions::default());
        assert_eq!(got.len(), 8);
        assert_eq!(got[0], "deep");
    }
}
