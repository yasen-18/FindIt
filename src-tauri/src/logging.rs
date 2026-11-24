//! Rolling file log: `findit.log`, rotated at 10 MB, three files kept.
//!
//! The default level is `info`, and nothing at that level contains file paths
//! (spec section 8). Set `FINDIT_LOG=debug` for more detail.

use findit_core::time::civil_from_days;
use log::{LevelFilter, Log, Metadata, Record};
use parking_lot::Mutex;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 10 << 20;
const KEEP: usize = 3;

struct FileLog {
    dir: PathBuf,
    level: LevelFilter,
    state: Mutex<(Option<File>, u64)>,
}

fn file_name(i: usize) -> String {
    if i == 0 {
        "findit.log".to_string()
    } else {
        format!("findit.{i}.log")
    }
}

impl FileLog {
    fn open(&self) -> (Option<File>, u64) {
        let path = self.dir.join(file_name(0));
        let f = OpenOptions::new().create(true).append(true).open(&path).ok();
        let size = f.as_ref().and_then(|f| f.metadata().ok()).map_or(0, |m| m.len());
        (f, size)
    }

    fn rotate(&self) {
        for i in (0..KEEP - 1).rev() {
            let from = self.dir.join(file_name(i));
            let to = self.dir.join(file_name(i + 1));
            if from.exists() {
                let _ = fs::rename(&from, &to);
            }
        }
    }
}

fn timestamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (y, m, d) = civil_from_days(now.div_euclid(86_400));
    let s = now.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, (s / 60) % 60, s % 60)
}

impl Log for FileLog {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= self.level && (m.target().starts_with("findit") || m.level() <= log::Level::Warn)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!("{} [{}] {}: {}\n", timestamp(), record.level(), record.target(), record.args());
        let mut st = self.state.lock();
        if st.0.is_none() {
            *st = self.open();
        }
        if st.1 + line.len() as u64 > MAX_BYTES {
            st.0 = None;
            self.rotate();
            *st = self.open();
        }
        if let Some(f) = st.0.as_mut() {
            if f.write_all(line.as_bytes()).is_ok() {
                st.1 += line.len() as u64;
            }
        }
    }

    fn flush(&self) {
        if let Some(f) = self.state.lock().0.as_mut() {
            let _ = f.flush();
        }
    }
}

/// Installs the file logger. Safe to call once; later calls are ignored.
pub fn init(dir: &Path) {
    let level = match std::env::var("FINDIT_LOG").map(|v| v.to_ascii_lowercase()).as_deref() {
        Ok("trace") => LevelFilter::Trace,
        Ok("debug") => LevelFilter::Debug,
        Ok("warn") => LevelFilter::Warn,
        Ok("error") => LevelFilter::Error,
        _ => LevelFilter::Info,
    };
    let _ = fs::create_dir_all(dir);
    let logger = FileLog { dir: dir.to_path_buf(), level, state: Mutex::new((None, 0)) };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
}
