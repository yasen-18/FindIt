//! Performance benchmark for section 7 of the spec.
//!
//! Builds a synthetic index of 1,000,000 entries, then measures substring and regex
//! search latency (p95 over many queries), snapshot save/load time and size, and
//! memory. Exits with an error code when a target is missed.
//!
//! Run with: `cargo bench -p findit-core --bench search`

use findit_core::index::{flags, Index, Meta, VolumeKind};
use findit_core::query::{Query, QueryOptions};
use findit_core::search::{search, SearchOptions};
use findit_core::snapshot;
use std::time::{Duration, Instant};

const ENTRIES: usize = 1_000_000;

const WORDS: &[&str] = &[
    "report",
    "invoice",
    "photo",
    "IMG",
    "backup",
    "project",
    "notes",
    "budget",
    "draft",
    "final",
    "summary",
    "video",
    "song",
    "setup",
    "readme",
    "config",
    "data",
    "log",
    "test",
    "build",
    "release",
    "Документ",
    "отчёт",
    "Έγγραφο",
    "報告書",
    "café",
    "résumé",
    "main",
    "index",
    "module",
    "assets",
    "client",
    "server",
    "design",
    "meeting",
];
const EXTS: &[&str] = &["pdf", "docx", "xlsx", "jpg", "png", "mp4", "mp3", "txt", "rs", "js", "json", "zip", "exe", "md", "log", "dll"];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a>(&mut self, v: &[&'a str]) -> &'a str {
        v[(self.next() % v.len() as u64) as usize]
    }
}

fn build() -> Index {
    let mut ix = Index::new();
    let slot = ix.add_volume("C:", VolumeKind::Standard, 1);
    let root = ix.volumes()[slot as usize].root;
    let stamp = ix.next_stamp();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut dirs = vec![root];
    let dir_count = ENTRIES / 12;
    for i in 0..dir_count {
        let parent = dirs[(rng.next() % dirs.len() as u64) as usize];
        let name = format!("{}_{}{}", rng.pick(WORDS), i % 977, if i % 3 == 0 { " (old)" } else { "" });
        let id = ix.upsert(
            parent,
            &name,
            None,
            None,
            Meta { size: 0, mtime: 1_600_000_000 + (i as i64 * 97) % 150_000_000, flags: flags::DIR },
            stamp,
        );
        dirs.push(id);
    }
    let files = ENTRIES - dir_count - 1;
    for i in 0..files {
        let parent = dirs[(rng.next() % dirs.len() as u64) as usize];
        let name = format!("{} {}_{}.{}", rng.pick(WORDS), rng.pick(WORDS), i % 100_003, rng.pick(EXTS));
        let size = rng.next() % (64 << 20);
        let mtime = 1_600_000_000 + (rng.next() % 170_000_000) as i64;
        let fl = if i % 50 == 0 { flags::HIDDEN } else { 0 };
        ix.upsert(parent, &name, None, None, Meta { size, mtime, flags: fl }, stamp);
    }
    ix
}

fn p95(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[((v.len() as f64) * 0.95).ceil() as usize - 1]
}

fn measure(ix: &Index, queries: &[&str], rounds: usize) -> (Duration, usize) {
    let opts = SearchOptions::default();
    let qopts = QueryOptions { now_unix: 1_790_000_000, ..Default::default() };
    let mut times = Vec::new();
    let mut hits = 0;
    for _ in 0..rounds {
        for q in queries {
            let t = Instant::now();
            let query = Query::parse(q, &qopts).expect("valid benchmark query");
            let out = search(ix, &query, &opts, &|| false).expect("not cancelled");
            times.push(t.elapsed());
            hits += out.total;
        }
    }
    (p95(times), hits)
}

fn main() {
    let mut failures = Vec::new();
    let mut check = |name: &str, value: Duration, target: Duration| {
        let ok = value <= target;
        println!(
            "{:<44} {:>10.2} ms   target {:>8.0} ms   {}",
            name,
            value.as_secs_f64() * 1000.0,
            target.as_secs_f64() * 1000.0,
            if ok { "OK" } else { "MISSED" }
        );
        if !ok {
            failures.push(name.to_string());
        }
    };

    println!("FindIt benchmark: {ENTRIES} synthetic entries, {} threads", rayon::current_num_threads());
    let t = Instant::now();
    let ix = build();
    println!("built index in {:.2} s", t.elapsed().as_secs_f64());

    // Warm the cached structures the way the first search after startup would.
    let _ = measure(&ix, &["warm"], 1);

    let substring = [
        "r",
        "re",
        "rep",
        "repo",
        "report",
        "inv 2024",
        "photo final",
        "документ",
        "café",
        "報告",
        "*.pdf",
        "ext:jpg;png",
        "size:>10mb",
        "!test log",
    ];
    let (sub_p95, hits) = measure(&ix, &substring, 20);
    check("substring search p95 (keystroke to results)", sub_p95, Duration::from_millis(50));
    println!("    ({hits} total hits)");

    let regex = ["regex:^IMG", "regex:report_\\d{2}\\.pdf$", "regex:\"(photo|video).*final\""];
    let (re_p95, _) = measure(&ix, &regex, 10);
    check("regex search p95", re_p95, Duration::from_millis(200));

    let (path_p95, _) = measure(&ix, &["path:report_1\\notes", "path:project"], 10);
    check("path search p95", path_p95, Duration::from_millis(200));

    let path = std::env::temp_dir().join(format!("findit-bench-{}.snap", std::process::id()));
    let t = Instant::now();
    let size = snapshot::save(&ix, &path).expect("snapshot save");
    check("snapshot save", t.elapsed(), Duration::from_secs(3));
    let t = Instant::now();
    let loaded = snapshot::load(&path).expect("snapshot load");
    check("snapshot load (launch from snapshot)", t.elapsed(), Duration::from_millis(1500));
    std::fs::remove_file(&path).ok();
    let mb = size as f64 / (1024.0 * 1024.0);
    println!("{:<44} {:>10.1} MB   target {:>8} MB   {}", "snapshot size", mb, 150, if mb <= 150.0 { "OK" } else { "MISSED" });
    if mb > 150.0 {
        failures.push("snapshot size".into());
    }
    let mem = loaded.memory_bytes() as f64 / (1024.0 * 1024.0);
    println!(
        "{:<44} {:>10.1} MB   target {:>8} MB   {}",
        "index memory (after load)",
        mem,
        300,
        if mem <= 300.0 { "OK" } else { "MISSED" }
    );
    if mem > 300.0 {
        failures.push("memory".into());
    }

    if failures.is_empty() {
        println!("\nAll targets met.");
    } else {
        eprintln!("\nMissed targets: {}", failures.join(", "));
        std::process::exit(1);
    }
}
