//! Where FindIt keeps its index, settings and logs.
//!
//! Installed: `%LOCALAPPDATA%\FindIt` (a per-user folder other accounts cannot read).
//! Portable: a `FindIt-data` folder next to the exe when `portable.flag` exists there.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct DataPaths {
    pub root: PathBuf,
    pub logs: PathBuf,
    pub snapshot: PathBuf,
    pub settings: PathBuf,
    pub portable: bool,
}

impl DataPaths {
    pub fn resolve() -> DataPaths {
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
        if let Some(dir) = exe_dir.as_ref().filter(|d| d.join("portable.flag").is_file()) {
            return DataPaths::at(dir.join("FindIt-data"), true);
        }
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("AppData").join("Local")))
            .unwrap_or_else(std::env::temp_dir);
        DataPaths::at(base.join("FindIt"), false)
    }

    pub fn at(root: PathBuf, portable: bool) -> DataPaths {
        DataPaths { logs: root.join("logs"), snapshot: root.join("index.snapshot"), settings: root.join("settings.json"), root, portable }
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.logs)
    }
}
