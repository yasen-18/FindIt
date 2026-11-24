//! Commands the web UI can call.
//!
//! Anything that can take more than a moment runs on a blocking worker thread
//! (`spawn_blocking`); Tauri runs plain synchronous commands on the main thread,
//! which would freeze the window.

use crate::engine::{Engine, Status};
use crate::settings::Settings;
use crate::shell::{self, Shell};
use crate::{strings::t, sysinfo};
use findit_core::query::{Query, QueryOptions};
use findit_core::search::{position_of, search as run_core_search, Chip, SearchOptions, SortColumn};
use findit_core::{flags, Index};
use findit_indexing::drives::{self, DriveKind};
use findit_indexing::win::{self, ATTR_OFFLINE, ATTR_RECALL_ON_DATA_ACCESS, ATTR_RECALL_ON_OPEN};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State, WebviewWindow};

const MAX_QUERY: usize = 4096;
const MAX_ROWS: usize = 500;
const MAX_SELECTION: usize = 100_000;

struct ResultSet {
    id: u64,
    ids: Vec<u32>,
    epoch: u64,
    query: Arc<Query>,
}

/// Shared state behind every command.
pub struct AppCore {
    pub engine: Arc<Engine>,
    pub shell: Shell,
    pub start_minimized: bool,
    pub mica: AtomicBool,
    /// Why the global shortcut could not be registered (shown in the UI).
    pub hotkey_error: Mutex<Option<String>>,
    search_seq: AtomicU64,
    current: Mutex<Option<Arc<ResultSet>>>,
    type_names: Mutex<HashMap<String, String>>,
    icons: Mutex<HashMap<String, Option<String>>>,
}

impl AppCore {
    pub fn new(engine: Arc<Engine>, start_minimized: bool) -> AppCore {
        AppCore {
            engine,
            shell: Shell::start(),
            start_minimized,
            mica: AtomicBool::new(false),
            hotkey_error: Mutex::new(None),
            search_seq: AtomicU64::new(0),
            current: Mutex::new(None),
            type_names: Mutex::new(HashMap::new()),
            icons: Mutex::new(HashMap::new()),
        }
    }

    fn result(&self, search_id: u64) -> Option<Arc<ResultSet>> {
        self.current.lock().clone().filter(|r| r.id == search_id)
    }
}

type Core<'a> = State<'a, Arc<AppCore>>;

/// Window handle as an integer so it can cross threads (raw pointers are not Send).
fn owner(app: &AppHandle) -> usize {
    app.get_webview_window("main").and_then(|w| w.hwnd().ok()).map_or(0, |h| h.0 as usize)
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Result<R, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())
}

// ----- search -------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchArgs {
    pub query: String,
    pub chip: Chip,
    pub sort: SortColumn,
    pub descending: bool,
    pub match_case: bool,
    pub match_path: bool,
    pub whole_word: bool,
    pub match_diacritics: bool,
    pub tz_offset_secs: i64,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub search_id: u64,
    pub total: usize,
    pub shown: usize,
    pub elapsed_ms: f64,
    pub limited: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub error: Option<String>,
}

fn run_search(core: &AppCore, my: u64, args: SearchArgs) -> SearchResponse {
    if args.query.len() > MAX_QUERY {
        return SearchResponse { search_id: my, error: Some(t("errors.queryTooLong")), ..Default::default() };
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let qopts = QueryOptions {
        match_case: args.match_case,
        match_path: args.match_path,
        whole_word: args.whole_word,
        match_diacritics: args.match_diacritics,
        now_unix: now,
        tz_offset_secs: args.tz_offset_secs.clamp(-16 * 3600, 16 * 3600),
    };
    let query = match Query::parse(&args.query, &qopts) {
        Ok(q) => q,
        Err(e) => return SearchResponse { search_id: my, error: Some(e.message), ..Default::default() },
    };
    let (include_hidden, include_system, limit) = {
        let s = core.engine.settings.read();
        (s.include_hidden, s.include_system, s.result_limit)
    };
    let sopts = SearchOptions {
        include_hidden,
        include_system,
        chip: args.chip,
        sort: args.sort,
        descending: args.descending,
        limit,
        regex_budget: Duration::from_secs(3),
    };
    let cancel = || core.search_seq.load(Ordering::Relaxed) != my;
    let ix = core.engine.index.read();
    let Some(out) = run_core_search(&ix, &query, &sopts, &cancel) else {
        return SearchResponse { search_id: my, cancelled: true, ..Default::default() };
    };
    drop(ix);
    let resp = SearchResponse {
        search_id: my,
        total: out.total,
        shown: out.ids.len(),
        elapsed_ms: out.elapsed.as_secs_f64() * 1000.0,
        limited: out.limited,
        timed_out: out.timed_out,
        cancelled: false,
        error: None,
    };
    let mut cur = core.current.lock();
    if core.search_seq.load(Ordering::Relaxed) != my {
        return SearchResponse { search_id: my, cancelled: true, ..Default::default() };
    }
    *cur = Some(Arc::new(ResultSet { id: my, ids: out.ids, epoch: out.epoch, query: Arc::new(query) }));
    resp
}

/// Runs a search. A newer call cancels an older one that is still running.
#[tauri::command]
pub async fn search(core: Core<'_>, args: SearchArgs) -> Result<SearchResponse, String> {
    let core = core.inner().clone();
    let my = core.search_seq.fetch_add(1, Ordering::SeqCst) + 1;
    blocking(move || run_search(&core, my, args)).await
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub id: u32,
    pub name: String,
    pub hl: Vec<(u32, u32)>,
    pub dir: String,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
    pub is_dir: bool,
    pub ext: String,
    pub hidden: bool,
    pub system: bool,
    pub cloud: bool,
    pub link: bool,
    pub deleted: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RowsResponse {
    pub stale: bool,
    pub rows: Vec<Row>,
}

fn ext_of(name: &str, is_dir: bool) -> String {
    if is_dir {
        return String::new();
    }
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() && name.len() - i <= 17 => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

fn build_row(ix: &Index, q: &Query, id: u32) -> Option<Row> {
    let e = *ix.entry(id)?;
    let name = ix.name(id).to_string();
    let no_meta = e.flags & flags::NO_META != 0;
    Some(Row {
        id,
        hl: q.highlight(&name),
        dir: ix.parent_path(id),
        size: (!e.is_dir() && !no_meta).then_some(e.size),
        mtime: (!no_meta && e.mtime != 0).then_some(e.mtime),
        is_dir: e.is_dir(),
        ext: ext_of(&name, e.is_dir()),
        hidden: e.flags & flags::HIDDEN != 0,
        system: e.flags & flags::SYSTEM != 0,
        cloud: e.flags & flags::CLOUD != 0,
        link: e.flags & flags::LINK != 0,
        deleted: e.is_deleted(),
        name,
    })
}

/// Rows `start..start+count` of a search. `stale` means the search was replaced
/// or the index was renumbered: the UI should search again.
#[tauri::command]
pub async fn get_rows(core: Core<'_>, search_id: u64, start: usize, count: usize) -> Result<RowsResponse, String> {
    let core = core.inner().clone();
    blocking(move || {
        let Some(rs) = core.result(search_id) else { return RowsResponse { stale: true, rows: Vec::new() } };
        let ix = core.engine.index.read();
        if ix.epoch() != rs.epoch {
            return RowsResponse { stale: true, rows: Vec::new() };
        }
        let start = start.min(rs.ids.len());
        let end = start.saturating_add(count.min(MAX_ROWS)).min(rs.ids.len());
        let rows = rs.ids[start..end].iter().filter_map(|&id| build_row(&ix, &rs.query, id)).collect();
        RowsResponse { stale: false, rows }
    })
    .await
}

/// Ids of rows `start..end` (for range and select-all selections).
#[tauri::command]
pub async fn ids_in_range(core: Core<'_>, search_id: u64, start: usize, end: usize) -> Result<Option<Vec<u32>>, String> {
    let core = core.inner().clone();
    blocking(move || {
        let rs = core.result(search_id)?;
        let s = start.min(rs.ids.len());
        let e = end.min(rs.ids.len()).max(s);
        (e - s <= MAX_SELECTION).then(|| rs.ids[s..e].to_vec())
    })
    .await
}

/// Row number of an id in a search (to keep the selection after a refresh).
#[tauri::command]
pub async fn index_of(core: Core<'_>, search_id: u64, id: u32) -> Result<Option<usize>, String> {
    let core = core.inner().clone();
    blocking(move || core.result(search_id).and_then(|rs| position_of(&rs.ids, id))).await
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionInfo {
    pub files: usize,
    pub folders: usize,
    pub total_size: u64,
}

#[tauri::command]
pub async fn selection_info(core: Core<'_>, ids: Vec<u32>) -> Result<SelectionInfo, String> {
    let core = core.inner().clone();
    blocking(move || {
        let ix = core.engine.index.read();
        let mut info = SelectionInfo { files: 0, folders: 0, total_size: 0 };
        for id in ids.into_iter().take(MAX_SELECTION) {
            if let Some(e) = ix.entry(id).filter(|e| !e.is_deleted()) {
                if e.is_dir() {
                    info.folders += 1;
                } else {
                    info.files += 1;
                    info.total_size = info.total_size.saturating_add(e.size);
                }
            }
        }
        info
    })
    .await
}

// ----- actions on items ---------------------------------------------------------------

fn wide_paths(core: &AppCore, ids: &[u32], max: usize) -> Vec<(u32, Vec<u16>)> {
    let ix = core.engine.index.read();
    ids.iter()
        .take(max)
        .filter(|&&id| ix.entry(id).is_some_and(|e| !e.is_deleted() && !e.is_root()))
        .map(|&id| (id, ix.wide_path(id)))
        .collect()
}

/// `open`, `openWith`, `reveal` or `properties`.
#[tauri::command]
pub async fn open_items(app: AppHandle, core: Core<'_>, ids: Vec<u32>, action: String) -> Result<(), String> {
    let core = core.inner().clone();
    let hwnd = owner(&app);
    if ids.len() > 20 && action == "open" {
        return Err(t("errors.tooManyToOpen"));
    }
    let max = if action == "open" { 20 } else { 1 };
    let paths = wide_paths(&core, &ids, max);
    if paths.is_empty() {
        return Err(t("errors.itemGone"));
    }
    let core2 = core.clone();
    blocking(move || {
        core2.shell.call(move || {
            let owner = hwnd as windows_sys::Win32::Foundation::HWND;
            for (_, p) in &paths {
                match action.as_str() {
                    "open" => shell::execute(p, None, owner)?,
                    "openWith" => shell::execute(p, Some("openas"), owner)?,
                    "reveal" => shell::reveal(p, owner)?,
                    "properties" => shell::execute(p, Some("properties"), owner)?,
                    _ => return Err("unknown action".to_string()),
                }
            }
            Ok(())
        })?
    })
    .await?
}

/// Copies `files` (for pasting in Explorer), `paths` or `names` to the clipboard.
#[tauri::command]
pub async fn copy_items(app: AppHandle, core: Core<'_>, ids: Vec<u32>, what: String) -> Result<(), String> {
    let core = core.inner().clone();
    let hwnd = owner(&app);
    blocking(move || {
        let items = wide_paths(&core, &ids, 10_000);
        if items.is_empty() {
            return Err(t("errors.itemGone"));
        }
        let text = match what.as_str() {
            "paths" => Some(items.iter().map(|(_, p)| String::from_utf16_lossy(p)).collect::<Vec<_>>().join("\r\n")),
            "names" => {
                let ix = core.engine.index.read();
                Some(items.iter().map(|(id, _)| ix.name(*id).to_string()).collect::<Vec<_>>().join("\r\n"))
            }
            "files" => None,
            _ => return Err("unknown clipboard format".into()),
        };
        let paths: Vec<Vec<u16>> = items.into_iter().map(|(_, p)| p).collect();
        core.shell.call(move || {
            let owner = hwnd as windows_sys::Win32::Foundation::HWND;
            match text {
                Some(t) => shell::copy_text(&t, owner),
                None => shell::copy_files(&paths, owner),
            }
        })?
    })
    .await?
}

#[tauri::command]
pub async fn rename_item(core: Core<'_>, id: u32, new_name: String) -> Result<(), String> {
    let core = core.inner().clone();
    blocking(move || {
        shell::validate_name(&new_name)?;
        let (old, parent, parent_path) = {
            let ix = core.engine.index.read();
            let e = ix.entry(id).filter(|e| !e.is_deleted() && !e.is_root()).ok_or_else(|| t("errors.itemGone"))?;
            (ix.wide_path(id), e.parent, ix.wide_path(e.parent))
        };
        let new_w: Vec<u16> = new_name.encode_utf16().collect();
        let new_path = win::join(&parent_path, &new_w);
        shell::rename(&old, &new_path)?;
        let mut ix = core.engine.index.write();
        ix.rename(id, parent, &new_name, None);
        log::info!(target: "findit::actions", "renamed one item");
        Ok(())
    })
    .await?
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteResult {
    pub deleted: usize,
    pub cancelled: bool,
}

/// Moves items to the Recycle Bin. The UI asks for confirmation first.
#[tauri::command]
pub async fn delete_items(app: AppHandle, core: Core<'_>, ids: Vec<u32>) -> Result<DeleteResult, String> {
    let core = core.inner().clone();
    let hwnd = owner(&app);
    blocking(move || {
        let items = wide_paths(&core, &ids, 10_000);
        if items.is_empty() {
            return Err(t("errors.itemGone"));
        }
        let paths: Vec<Vec<u16>> = items.iter().map(|(_, p)| p.clone()).collect();
        let cancelled = core.shell.call(move || shell::recycle(&paths, hwnd as windows_sys::Win32::Foundation::HWND))??;
        let mut deleted = 0;
        let mut ix = core.engine.index.write();
        for (id, p) in &items {
            if win::stat(p).is_none() {
                ix.remove(*id);
                deleted += 1;
            }
        }
        ix.sweep_orphans();
        log::info!(target: "findit::actions", "moved {deleted} item(s) to the Recycle Bin");
        Ok(DeleteResult { deleted, cancelled })
    })
    .await?
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    /// `image`, `text` or `none`.
    pub kind: String,
    pub data: String,
    pub message: Option<String>,
}

const IMAGE_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("jfif", "image/jpeg"),
    ("gif", "image/gif"),
    ("bmp", "image/bmp"),
    ("webp", "image/webp"),
    ("ico", "image/x-icon"),
    ("svg", "image/svg+xml"),
    ("avif", "image/avif"),
];

const TEXT_TYPES: &[&str] = &[
    "txt",
    "md",
    "log",
    "json",
    "csv",
    "tsv",
    "xml",
    "yml",
    "yaml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "rs",
    "js",
    "mjs",
    "ts",
    "tsx",
    "jsx",
    "py",
    "c",
    "cc",
    "cpp",
    "h",
    "hpp",
    "cs",
    "java",
    "kt",
    "go",
    "rb",
    "php",
    "html",
    "htm",
    "css",
    "scss",
    "sql",
    "sh",
    "bat",
    "cmd",
    "ps1",
    "gitignore",
    "editorconfig",
    "env",
    "properties",
    "lua",
    "swift",
    "vue",
    "svelte",
];

fn none(message: String) -> Preview {
    Preview { kind: "none".into(), data: String::new(), message: Some(message) }
}

/// Content for the preview pane. Cloud placeholders are never read, so they are
/// never downloaded.
#[tauri::command]
pub async fn preview(core: Core<'_>, id: u32) -> Result<Preview, String> {
    let core = core.inner().clone();
    blocking(move || {
        let (path, e, name) = {
            let ix = core.engine.index.read();
            let Some(e) = ix.entry(id).filter(|e| !e.is_deleted()).copied() else { return none(t("preview.gone")) };
            (ix.wide_path(id), e, ix.name(id).to_string())
        };
        if e.is_dir() {
            return none(t("preview.folder"));
        }
        let live_attrs = win::stat(&path).map(|s| s.attrs);
        let Some(attrs) = live_attrs else { return none(t("preview.gone")) };
        if e.flags & flags::CLOUD != 0 || attrs & (ATTR_RECALL_ON_DATA_ACCESS | ATTR_RECALL_ON_OPEN | ATTR_OFFLINE) != 0 {
            return none(t("preview.cloud"));
        }
        let ext = ext_of(&name, false);
        let os_path = {
            use std::os::windows::ffi::OsStringExt;
            let long = win::long_path(&path);
            std::path::PathBuf::from(std::ffi::OsString::from_wide(&long[..long.len() - 1]))
        };
        if let Some((_, mime)) = IMAGE_TYPES.iter().find(|(x, _)| *x == ext) {
            if e.size > 20 << 20 {
                return none(t("preview.tooLarge"));
            }
            return match std::fs::read(&os_path) {
                Ok(bytes) => Preview { kind: "image".into(), data: format!("data:{mime};base64,{}", shell::base64(&bytes)), message: None },
                Err(err) => none(err.to_string()),
            };
        }
        let looks_text = TEXT_TYPES.contains(&ext.as_str()) || (ext.is_empty() && e.size < 64 << 10);
        if !looks_text {
            return none(t("preview.unsupported"));
        }
        use std::io::Read;
        let mut buf = Vec::new();
        match std::fs::File::open(&os_path).and_then(|f| f.take(128 << 10).read_to_end(&mut buf)) {
            Ok(_) => {
                let text = if buf.starts_with(&[0xFF, 0xFE]) {
                    let units: Vec<u16> = buf[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    String::from_utf16_lossy(&units)
                } else {
                    if buf.iter().take(4096).any(|&b| b == 0) {
                        return none(t("preview.binary"));
                    }
                    String::from_utf8_lossy(buf.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&buf)).into_owned()
                };
                Preview { kind: "text".into(), data: text, message: None }
            }
            Err(err) => none(err.to_string()),
        }
    })
    .await
}

fn valid_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 16 && k.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// Friendly type names ("PDF Document") for extension keys (`folder` for folders).
#[tauri::command]
pub async fn type_names(core: Core<'_>, keys: Vec<String>) -> Result<HashMap<String, String>, String> {
    let core = core.inner().clone();
    blocking(move || {
        let mut out = HashMap::new();
        for k in keys.into_iter().take(200).filter(|k| valid_key(k)) {
            if let Some(v) = core.type_names.lock().get(&k) {
                out.insert(k, v.clone());
                continue;
            }
            let key = k.clone();
            let name = core.shell.call(move || shell::type_name(&key)).unwrap_or_default();
            let name = if name.is_empty() { format!("{} {}", k.to_uppercase(), t("grid.fileSuffix")) } else { name };
            core.type_names.lock().insert(k.clone(), name.clone());
            out.insert(k, name);
        }
        out
    })
    .await
}

/// Small shell icons as PNG data URLs, cached per extension key.
#[tauri::command]
pub async fn icons(core: Core<'_>, keys: Vec<String>) -> Result<HashMap<String, String>, String> {
    let core = core.inner().clone();
    blocking(move || {
        let mut out = HashMap::new();
        for k in keys.into_iter().take(200).filter(|k| valid_key(k)) {
            let cached = core.icons.lock().get(&k).cloned();
            let url = match cached {
                Some(u) => u,
                None => {
                    let key = k.clone();
                    let u = core.shell.call(move || shell::icon_data_url(&key)).ok().flatten();
                    core.icons.lock().insert(k.clone(), u.clone());
                    u
                }
            };
            if let Some(u) = url {
                out.insert(k, u);
            }
        }
        out
    })
    .await
}

// ----- status, settings, app ----------------------------------------------------------

#[tauri::command]
pub fn get_status(core: Core<'_>) -> Status {
    core.engine.status()
}

#[tauri::command]
pub fn get_settings(core: Core<'_>) -> Settings {
    core.engine.settings.read().clone()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveResult {
    pub settings: Settings,
    pub hotkey_error: Option<String>,
}

#[tauri::command]
pub async fn save_settings(app: AppHandle, core: Core<'_>, settings: Settings) -> Result<SaveResult, String> {
    let core = core.inner().clone();
    blocking(move || {
        let old = core.engine.settings.read().clone();
        let mut new = settings.sanitize();
        // These are changed only through their own commands.
        new.fast_mode = old.fast_mode;
        new.first_run_done = old.first_run_done;
        let hotkey_error = if new.hotkey != old.hotkey {
            let r = crate::register_hotkey(&app, &new.hotkey).err();
            *core.hotkey_error.lock() = r.clone();
            r
        } else {
            None
        };
        if new.start_with_windows != old.start_with_windows {
            crate::set_autostart(&app, new.start_with_windows);
        }
        if new.tray_icon != old.tray_icon {
            crate::set_tray_visible(&app, new.tray_icon);
        }
        *core.engine.settings.write() = new.clone();
        new.save(&core.engine.paths.settings).map_err(|e| e.to_string())?;
        core.engine.settings_changed(&old, &new);
        Ok(SaveResult { settings: new, hotkey_error })
    })
    .await?
}

/// Remembers a query in the search history.
#[tauri::command]
pub async fn remember_query(core: Core<'_>, query: String) -> Result<Vec<String>, String> {
    let core = core.inner().clone();
    blocking(move || {
        let mut s = core.engine.settings.write();
        s.add_history(&query);
        let _ = s.save(&core.engine.paths.settings);
        s.history.clone()
    })
    .await
}

#[tauri::command]
pub async fn clear_history(core: Core<'_>) -> Result<(), String> {
    let core = core.inner().clone();
    blocking(move || {
        let mut s = core.engine.settings.write();
        s.history.clear();
        s.save(&core.engine.paths.settings).map_err(|e| e.to_string())
    })
    .await?
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveRow {
    pub label: String,
    pub kind: DriveKind,
    pub file_system: String,
    pub volume_name: String,
    pub enabled: bool,
    pub fast_capable: bool,
}

#[tauri::command]
pub async fn list_drives(core: Core<'_>) -> Result<Vec<DriveRow>, String> {
    let core = core.inner().clone();
    blocking(move || {
        let s = core.engine.settings.read().clone();
        drives::list_drives()
            .into_iter()
            .filter(|d| d.kind != DriveKind::Other)
            .map(|d| DriveRow {
                enabled: s.drive_enabled(&d.label, d.kind == DriveKind::Fixed),
                fast_capable: d.kind == DriveKind::Fixed && d.is_ntfs(),
                label: d.label,
                kind: d.kind,
                file_system: d.file_system,
                volume_name: d.volume_name,
            })
            .collect()
    })
    .await
}

/// Finishes the welcome wizard and starts indexing.
#[tauri::command]
pub async fn finish_first_run(core: Core<'_>, drives: BTreeMap<String, bool>, fast_mode: bool) -> Result<(), String> {
    let core = core.inner().clone();
    blocking(move || {
        {
            let mut s = core.engine.settings.write();
            s.first_run_done = true;
            s.drives = drives;
            // Set before indexing starts, so fast-capable drives never get a standard walk too.
            s.fast_mode = fast_mode;
            *s = s.clone().sanitize();
            s.save(&core.engine.paths.settings).map_err(|e| e.to_string())?;
        }
        core.engine.begin_indexing(fast_mode);
        Ok(())
    })
    .await?
}

fn enable_fast(core: &AppCore) -> String {
    let letters = core.engine.fast_capable();
    if letters.is_empty() {
        return "unsupported".into();
    }
    {
        let mut s = core.engine.settings.write();
        s.fast_mode = true;
        let _ = s.save(&core.engine.paths.settings);
    }
    core.engine.start_fast(letters, true);
    "starting".into()
}

/// Switches to NTFS fast mode (shows the UAC prompt). The result arrives through
/// status updates: `on`, `declined` or `failed`.
#[tauri::command]
pub async fn enable_fast_mode(core: Core<'_>) -> Result<String, String> {
    let core = core.inner().clone();
    blocking(move || enable_fast(&core)).await
}

#[tauri::command]
pub async fn disable_fast_mode(core: Core<'_>) -> Result<(), String> {
    let core = core.inner().clone();
    blocking(move || {
        {
            let mut s = core.engine.settings.write();
            s.fast_mode = false;
            let _ = s.save(&core.engine.paths.settings);
        }
        core.engine.disable_fast();
    })
    .await
}

#[tauri::command]
pub fn rebuild_index(core: Core<'_>) {
    core.engine.rebuild();
}

#[tauri::command]
pub fn set_paused(app: AppHandle, core: Core<'_>, paused: bool) {
    core.engine.set_paused(paused);
    crate::sync_tray_pause(&app, paused);
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub data_dir: String,
    pub log_dir: String,
    pub portable: bool,
    pub elevated: bool,
    pub windows_build: u32,
    pub accent_color: Option<String>,
    pub mica: bool,
    pub memory_mb: f64,
    pub hotkey_error: Option<String>,
}

#[tauri::command]
pub async fn app_info(core: Core<'_>) -> Result<AppInfo, String> {
    let core = core.inner().clone();
    blocking(move || AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        data_dir: core.engine.paths.root.to_string_lossy().into_owned(),
        log_dir: core.engine.paths.logs.to_string_lossy().into_owned(),
        portable: core.engine.paths.portable,
        elevated: findit_indexing::pipe::is_elevated(),
        windows_build: sysinfo::windows_build(),
        accent_color: sysinfo::accent_color(),
        mica: core.mica.load(Ordering::Relaxed),
        memory_mb: sysinfo::memory_mb(),
        hotkey_error: core.hotkey_error.lock().clone(),
    })
    .await
}

/// The page finished loading: show the window (unless started in the tray).
#[tauri::command]
pub fn app_ready(window: WebviewWindow, core: Core<'_>) {
    if !core.start_minimized {
        let _ = window.show();
        let _ = window.set_focus();
    }
    if let Some(t) = crate::STARTED.get() {
        log::info!(target: "findit", "window ready {} ms after start", t.elapsed().as_millis());
    }
}

#[tauri::command]
pub fn hide_window(window: WebviewWindow) {
    let _ = window.hide();
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Opens the data or log folder in Explorer.
#[tauri::command]
pub async fn open_folder(app: AppHandle, core: Core<'_>, which: String) -> Result<(), String> {
    let core = core.inner().clone();
    let hwnd = owner(&app);
    let dir = match which.as_str() {
        "logs" => core.engine.paths.logs.clone(),
        _ => core.engine.paths.root.clone(),
    };
    let _ = std::fs::create_dir_all(&dir);
    let w: Vec<u16> = dir.to_string_lossy().encode_utf16().collect();
    blocking(move || core.shell.call(move || shell::execute(&w, None, hwnd as windows_sys::Win32::Foundation::HWND))?).await?
}

/// Opens a release page in the browser. Only GitHub HTTPS links are accepted.
#[tauri::command]
pub async fn open_external(app: AppHandle, core: Core<'_>, url: String) -> Result<(), String> {
    let ok = url.starts_with("https://github.com/") && url.len() < 512 && !url.chars().any(|c| c.is_whitespace() || c == '"');
    if !ok {
        return Err("blocked URL".into());
    }
    let core = core.inner().clone();
    let hwnd = owner(&app);
    let w: Vec<u16> = url.encode_utf16().collect();
    blocking(move || core.shell.call(move || shell::execute(&w, None, hwnd as windows_sys::Win32::Foundation::HWND))?).await?
}
