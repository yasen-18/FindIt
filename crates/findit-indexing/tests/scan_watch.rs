//! Integration test: build a real folder tree, scan it, change it, and check that
//! the watcher brings the index up to date within about a second.

#![cfg(windows)]

use findit_core::query::{Query, QueryOptions};
use findit_core::search::{search, SearchOptions};
use findit_core::{shared, Index, SharedIndex, VolumeKind};
use findit_indexing::apply::apply_events;
use findit_indexing::walk::{walk, Exclusions, ScanControl, ScanStats};
use findit_indexing::watch::Watcher;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn temp_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    std::env::temp_dir().join(format!("findit-it-{tag}-{}-{nanos}", std::process::id()))
}

fn wide(p: &Path) -> Vec<u16> {
    p.to_string_lossy().encode_utf16().collect()
}

/// `\\?\` prefixed path, needed to create files beyond 260 characters with std.
fn verbatim(p: &Path) -> PathBuf {
    PathBuf::from(format!("\\\\?\\{}", p.display()))
}

fn names(index: &SharedIndex, q: &str) -> Vec<String> {
    let ix = index.read();
    let query = Query::parse(q, &QueryOptions::default()).unwrap();
    let out = search(&ix, &query, &SearchOptions { include_hidden: true, ..Default::default() }, &|| false).unwrap();
    let mut v: Vec<String> = out.ids.iter().map(|&id| ix.name(id).to_string()).collect();
    v.sort();
    v
}

struct Fixture {
    root: PathBuf,
    index: SharedIndex,
    slot: u8,
    excl: Exclusions,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(verbatim(&self.root));
    }
}

fn scan(fx: &Fixture, id: u32, path: &[u16]) {
    let stamp = fx.index.write().next_stamp();
    let ctl = ScanControl::default();
    let stats = ScanStats::default();
    walk(&fx.index, id, path, stamp, &fx.excl, &ctl, &stats, 4);
    fx.index.write().sweep_unstamped(id, stamp);
}

fn setup(tag: &str) -> Fixture {
    let root = temp_root(tag);
    fs::create_dir_all(root.join("a").join("b")).unwrap();
    fs::create_dir_all(root.join("Документы")).unwrap();
    fs::create_dir_all(root.join("node_modules").join("pkg")).unwrap();
    fs::write(root.join("a").join("one.txt"), b"1").unwrap();
    fs::write(root.join("a").join("b").join("two.log"), b"22").unwrap();
    fs::write(root.join("Документы").join("отчёт 2026 🎉.docx"), b"333").unwrap();
    fs::write(root.join("node_modules").join("pkg").join("index.js"), b"x").unwrap();
    // A path longer than 260 characters.
    let mut deep = root.clone();
    for i in 0..12 {
        deep.push(format!("very_long_folder_name_number_{i:02}"));
    }
    fs::create_dir_all(verbatim(&deep)).unwrap();
    fs::write(verbatim(&deep.join("deep-file.txt")), b"deep").unwrap();
    assert!(deep.join("deep-file.txt").to_string_lossy().len() > 260);

    let index = shared(Index::new());
    let label = root.to_string_lossy().to_string();
    let slot = index.write().add_volume(&label, VolumeKind::Standard, 0);
    let fx = Fixture { root, index, slot, excl: Exclusions::new(&["node_modules".to_string()]) };
    let root_id = fx.index.read().volumes()[slot as usize].root;
    scan(&fx, root_id, &wide(&fx.root));
    fx
}

#[test]
fn scan_finds_everything_except_exclusions() {
    let fx = setup("scan");
    assert_eq!(names(&fx.index, "two"), vec!["two.log"]);
    assert_eq!(names(&fx.index, "ОТЧЁТ"), vec!["отчёт 2026 🎉.docx"]);
    assert!(names(&fx.index, "index.js").is_empty(), "node_modules is excluded");
    let ix = fx.index.read();
    let id = ix.resolve(fx.slot, "a\\b\\two.log").unwrap();
    assert_eq!(ix.entry(id).unwrap().size, 2);
    assert_eq!(ix.path(id), fx.root.join("a").join("b").join("two.log").to_string_lossy());
    let deep = names(&fx.index, "deep-file");
    assert_eq!(deep, vec!["deep-file.txt"]);
}

#[test]
fn rescan_merges_and_removes_stale_entries() {
    let fx = setup("rescan");
    fs::remove_file(fx.root.join("a").join("one.txt")).unwrap();
    fs::write(fx.root.join("a").join("three.txt"), b"3").unwrap();
    let root_id = fx.index.read().volumes()[fx.slot as usize].root;
    scan(&fx, root_id, &wide(&fx.root));
    assert!(names(&fx.index, "one.txt").is_empty());
    assert_eq!(names(&fx.index, "three"), vec!["three.txt"]);
}

#[test]
fn watcher_tracks_create_rename_move_delete() {
    let fx = setup("watch");
    let (tx, rx) = mpsc::channel();
    let root_w = wide(&fx.root);
    let watcher = Watcher::start(&root_w, 256 * 1024, move |ev| {
        let _ = tx.send(ev);
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    fs::write(fx.root.join("a").join("created.txt"), b"new").unwrap();
    fs::rename(fx.root.join("a").join("one.txt"), fx.root.join("a").join("b").join("moved.txt")).unwrap();
    fs::remove_file(fx.root.join("a").join("b").join("two.log")).unwrap();
    fs::rename(fx.root.join("a").join("b"), fx.root.join("a").join("renamed_dir")).unwrap();
    fs::create_dir(fx.root.join("brand_new")).unwrap();
    fs::write(fx.root.join("brand_new").join("inside.md"), b"#").unwrap();

    let done = |fx: &Fixture| {
        let ix = fx.index.read();
        ix.resolve(fx.slot, "a\\created.txt").is_some()
            && ix.resolve(fx.slot, "a\\renamed_dir\\moved.txt").is_some()
            && ix.resolve(fx.slot, "a\\one.txt").is_none()
            && ix.resolve(fx.slot, "a\\b").is_none()
            && names(&fx.index, "two.log").is_empty()
            && ix.resolve(fx.slot, "brand_new\\inside.md").is_some()
    };
    while !done(&fx) && started.elapsed() < Duration::from_secs(5) {
        if let Ok(events) = rx.recv_timeout(Duration::from_millis(50)) {
            let out = apply_events(&fx.index, fx.slot, &root_w, &events, &fx.excl);
            assert!(!out.overflow);
            for (id, path) in out.rescan {
                scan(&fx, id, &path);
            }
        }
    }
    let latency = started.elapsed();
    watcher.stop();
    assert!(done(&fx), "index did not catch up");
    println!("changes visible after {latency:?}");
    assert!(latency < Duration::from_secs(2), "took {latency:?}");
}
