//! Standard-mode scanner: a parallel folder walk with `FindFirstFileExW`.
//!
//! * `FindExInfoBasic` skips the 8.3 short name, `FIND_FIRST_EX_LARGE_FETCH` asks
//!   for bigger directory reads.
//! * The walk is iterative: folders wait on an explicit shared stack, so deep trees
//!   cannot overflow the call stack. Several threads pop from the stack at once.
//! * Junctions and symbolic links are indexed but never followed (no loops).
//! * Access denied is counted, not reported as an error.
//! * The index is updated after every folder, so results appear while scanning.

use crate::win::{find_item, join, last_error, long_path, FindItem};
use findit_core::fold::{self, FoldMode};
use findit_core::{flags, SharedIndex};
use parking_lot::{Condvar, Mutex};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
};

/// Folders (and files) the user does not want indexed.
#[derive(Clone, Debug, Default)]
pub struct Exclusions {
    names: Vec<String>,
    prefixes: Vec<String>,
}

fn upper(s: &str) -> String {
    s.chars().map(fold::simple_upper).collect()
}

impl Exclusions {
    /// Entries without a backslash match a folder name anywhere (`node_modules`);
    /// entries with one match a full path prefix (`C:\Windows\WinSxS`).
    pub fn new(list: &[String]) -> Exclusions {
        let mut e = Exclusions::default();
        for item in list {
            let t = item.trim().trim_end_matches('\\');
            if t.is_empty() {
                continue;
            }
            if t.contains('\\') {
                e.prefixes.push(upper(t));
            } else {
                e.names.push(upper(t));
            }
        }
        e
    }

    /// Adds a path prefix that is always skipped (the app's own data folder).
    pub fn add_prefix(&mut self, path: &str) {
        let t = path.trim_end_matches('\\');
        if !t.is_empty() {
            self.prefixes.push(upper(t));
        }
    }

    pub fn excludes_name(&self, name: &str) -> bool {
        !self.names.is_empty() && self.names.iter().any(|n| fold::eq_ignore_case(n, name))
    }

    /// True when `path` equals or lies below an excluded prefix.
    pub fn excludes_path(&self, path: &str) -> bool {
        if self.prefixes.is_empty() {
            return false;
        }
        let p = upper(path);
        self.prefixes.iter().any(|x| p == *x || (p.starts_with(x.as_str()) && p.as_bytes().get(x.len()) == Some(&b'\\')))
    }

    pub fn has_prefixes(&self) -> bool {
        !self.prefixes.is_empty()
    }

    /// True when any component of a relative path is an excluded name.
    pub fn excludes_any_component(&self, rel: &str) -> bool {
        !self.names.is_empty() && rel.split('\\').any(|c| self.excludes_name(c))
    }
}

/// Pause and cancel switches shared with the UI.
#[derive(Debug, Default)]
pub struct ScanControl {
    pub cancel: AtomicBool,
    pub paused: AtomicBool,
}

/// Counters for the status bar and the log.
#[derive(Debug, Default)]
pub struct ScanStats {
    pub entries: AtomicU64,
    pub folders: AtomicU64,
    pub denied: AtomicU64,
    pub errors: AtomicU64,
}

/// An entry ready for insertion: folded outside the index lock.
struct Prepared {
    item: FindItem,
    folded: String,
}

/// Lists one folder. `Err` carries the Win32 error code.
pub fn list_dir(path: &[u16]) -> Result<Vec<FindItem>, u32> {
    let mut pattern = long_path(path);
    pattern.pop();
    if pattern.last() != Some(&u16::from(b'\\')) {
        pattern.push(u16::from(b'\\'));
    }
    pattern.push(u16::from(b'*'));
    pattern.push(0);
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let h = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FindExInfoBasic,
            &mut data as *mut WIN32_FIND_DATAW as *mut c_void,
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    let mut items = Vec::new();
    loop {
        let n = &data.cFileName;
        let is_dot = n[0] == u16::from(b'.') && (n[1] == 0 || (n[1] == u16::from(b'.') && n[2] == 0));
        if !is_dot {
            items.push(find_item(&data));
        }
        if unsafe { FindNextFileW(h, &mut data) } == 0 {
            break;
        }
    }
    unsafe { FindClose(h) };
    Ok(items)
}

struct Queue {
    jobs: Vec<(u32, Vec<u16>)>,
    active: usize,
    done: bool,
}

/// Walks the tree below index entry `start` (whose display path is `start_path`,
/// for example `C:` or `C:\Users`) and upserts everything with `stamp`.
#[allow(clippy::too_many_arguments)]
pub fn walk(
    index: &SharedIndex,
    start: u32,
    start_path: &[u16],
    stamp: u32,
    excl: &Exclusions,
    ctl: &ScanControl,
    stats: &ScanStats,
    threads: usize,
) {
    let queue = Mutex::new(Queue { jobs: vec![(start, start_path.to_vec())], active: 0, done: false });
    let cv = Condvar::new();
    let threads = threads.clamp(1, 32);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| worker(index, stamp, excl, ctl, stats, &queue, &cv));
        }
    });
}

fn worker(index: &SharedIndex, stamp: u32, excl: &Exclusions, ctl: &ScanControl, stats: &ScanStats, queue: &Mutex<Queue>, cv: &Condvar) {
    loop {
        let (parent, path) = {
            let mut q = queue.lock();
            loop {
                if q.done || ctl.cancel.load(Ordering::Relaxed) {
                    q.done = true;
                    cv.notify_all();
                    return;
                }
                if let Some(job) = q.jobs.pop() {
                    q.active += 1;
                    break job;
                }
                if q.active == 0 {
                    q.done = true;
                    cv.notify_all();
                    return;
                }
                cv.wait(&mut q);
            }
        };
        while ctl.paused.load(Ordering::Relaxed) && !ctl.cancel.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
        let mut subdirs = Vec::new();
        match list_dir(&path) {
            Ok(items) => {
                let prepared: Vec<Prepared> = items
                    .into_iter()
                    .filter(|it| !excl.excludes_name(&it.name))
                    .filter(|it| !excl.has_prefixes() || !excl.excludes_path(&String::from_utf16_lossy(&join(&path, &it.wide_name))))
                    .map(|item| {
                        let folded = fold::fold(&item.name, FoldMode::LOOSE);
                        Prepared { item, folded }
                    })
                    .collect();
                let count = prepared.len() as u64;
                {
                    let mut ix = index.write();
                    let alive = ix.entry(parent).is_some_and(|e| !e.is_deleted());
                    if alive {
                        ix.touch(parent, stamp);
                        for p in &prepared {
                            let it = &p.item;
                            let id = ix.upsert(parent, &it.name, Some(&p.folded), it.raw.as_deref(), it.meta, stamp);
                            let f = it.meta.flags;
                            if f & flags::DIR != 0 && f & flags::LINK == 0 {
                                subdirs.push((id, join(&path, &it.wide_name)));
                            }
                        }
                    }
                }
                stats.entries.fetch_add(count, Ordering::Relaxed);
                stats.folders.fetch_add(1, Ordering::Relaxed);
            }
            Err(code) if code == ERROR_ACCESS_DENIED => {
                stats.denied.fetch_add(1, Ordering::Relaxed);
            }
            Err(code) if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND => {}
            Err(_) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        let mut q = queue.lock();
        q.jobs.extend(subdirs);
        q.active -= 1;
        cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions() {
        let e = Exclusions::new(&["$Recycle.Bin".into(), "node_modules".into(), "C:\\Windows\\WinSxS\\".into()]);
        assert!(e.excludes_name("$RECYCLE.BIN"));
        assert!(e.excludes_name("Node_Modules"));
        assert!(!e.excludes_name("modules"));
        assert!(e.excludes_path("c:\\windows\\winsxs"));
        assert!(e.excludes_path("C:\\Windows\\WinSxS\\x86"));
        assert!(!e.excludes_path("C:\\Windows\\WinSxSBackup"));
        assert!(e.excludes_any_component("a\\node_modules\\b"));
    }
}
