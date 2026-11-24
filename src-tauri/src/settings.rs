//! User settings, stored as JSON in the data folder.

use findit_core::search::SortColumn;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const DEFAULT_EXCLUSIONS: [&str; 2] = ["$Recycle.Bin", "System Volume Information"];
const COLUMN_IDS: [&str; 5] = ["name", "path", "size", "modified", "type"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnSetting {
    pub id: String,
    pub width: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub first_run_done: bool,
    pub hotkey: String,
    pub tray_icon: bool,
    pub close_to_tray: bool,
    pub start_with_windows: bool,
    /// `system`, `light` or `dark`.
    pub theme: String,
    /// Explicit per-drive choices (`"D:" -> true`). Drives without an entry use the
    /// default: fixed drives on, removable and network drives off.
    pub drives: BTreeMap<String, bool>,
    pub exclusions: Vec<String>,
    pub include_hidden: bool,
    pub include_system: bool,
    pub fast_mode: bool,
    pub match_case: bool,
    pub match_path: bool,
    pub whole_word: bool,
    pub match_diacritics: bool,
    /// 0 = show every match.
    pub result_limit: usize,
    pub columns: Vec<ColumnSetting>,
    pub sort_column: SortColumn,
    pub sort_descending: bool,
    pub show_preview: bool,
    pub history: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            first_run_done: false,
            hotkey: "Ctrl+Alt+Space".into(),
            tray_icon: true,
            close_to_tray: true,
            start_with_windows: false,
            theme: "system".into(),
            drives: BTreeMap::new(),
            exclusions: DEFAULT_EXCLUSIONS.iter().map(|s| s.to_string()).collect(),
            include_hidden: false,
            include_system: false,
            fast_mode: false,
            match_case: false,
            match_path: false,
            whole_word: false,
            match_diacritics: false,
            result_limit: 0,
            columns: vec![
                ColumnSetting { id: "name".into(), width: 320 },
                ColumnSetting { id: "path".into(), width: 420 },
                ColumnSetting { id: "size".into(), width: 96 },
                ColumnSetting { id: "modified".into(), width: 150 },
                ColumnSetting { id: "type".into(), width: 150 },
            ],
            sort_column: SortColumn::Relevance,
            sort_descending: false,
            show_preview: false,
            history: Vec::new(),
        }
    }
}

impl Settings {
    /// Clamps every value into a safe range (the page that sends settings is ours,
    /// but the file on disk could have been edited by hand).
    pub fn sanitize(mut self) -> Settings {
        let d = Settings::default();
        self.hotkey = self.hotkey.chars().take(64).collect::<String>().trim().to_string();
        if !matches!(self.theme.as_str(), "system" | "light" | "dark") {
            self.theme = d.theme;
        }
        self.drives.retain(|k, _| {
            let b = k.as_bytes();
            b.len() == 2 && b[0].is_ascii_uppercase() && b[1] == b':'
        });
        self.exclusions =
            self.exclusions.into_iter().map(|e| e.trim().to_string()).filter(|e| !e.is_empty() && e.len() <= 1024).take(500).collect();
        self.result_limit = self.result_limit.min(100_000_000);
        let mut cols: Vec<ColumnSetting> = self
            .columns
            .into_iter()
            .filter(|c| COLUMN_IDS.contains(&c.id.as_str()))
            .map(|c| ColumnSetting { width: c.width.clamp(48, 2000), ..c })
            .collect();
        cols.dedup_by(|a, b| a.id == b.id);
        for id in COLUMN_IDS {
            if !cols.iter().any(|c| c.id == id) {
                if let Some(def) = d.columns.iter().find(|c| c.id == id) {
                    cols.push(def.clone());
                }
            }
        }
        self.columns = cols;
        self.history = self.history.into_iter().filter(|h| !h.trim().is_empty() && h.len() <= 512).take(50).collect();
        self
    }

    pub fn load(path: &Path) -> Settings {
        match std::fs::read(path) {
            Ok(bytes) => match Self::parse(&bytes) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!(target: "findit::settings", "settings file unreadable ({e}); using defaults");
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    /// Parses settings JSON. A UTF-8 byte order mark (added by some editors and by
    /// Windows PowerShell) is accepted.
    pub fn parse(bytes: &[u8]) -> Result<Settings, serde_json::Error> {
        let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
        serde_json::from_slice::<Settings>(bytes).map(Settings::sanitize)
    }

    /// Writes atomically (temp file, then rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    }

    /// Is this drive selected for indexing?
    pub fn drive_enabled(&self, label: &str, fixed: bool) -> bool {
        self.drives.get(label).copied().unwrap_or(fixed)
    }

    pub fn add_history(&mut self, query: &str) {
        let q = query.trim();
        if q.is_empty() || q.len() > 512 {
            return;
        }
        self.history.retain(|h| h != q);
        self.history.insert(0, q.to_string());
        self.history.truncate(50);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_fixes_bad_values() {
        let mut s = Settings { theme: "neon".into(), result_limit: usize::MAX, ..Default::default() };
        s.drives.insert("C:".into(), true);
        s.drives.insert("../x".into(), true);
        s.columns = vec![ColumnSetting { id: "size".into(), width: 1 }, ColumnSetting { id: "evil".into(), width: 9 }];
        let s = s.sanitize();
        assert_eq!(s.theme, "system");
        assert_eq!(s.drives.len(), 1);
        assert_eq!(s.columns.len(), 5);
        assert_eq!(s.columns[0], ColumnSetting { id: "size".into(), width: 48 });
        assert_eq!(s.result_limit, 100_000_000);
    }

    #[test]
    fn history_is_deduplicated() {
        let mut s = Settings::default();
        s.add_history("a");
        s.add_history("b");
        s.add_history("a");
        assert_eq!(s.history, vec!["a", "b"]);
    }

    #[test]
    fn byte_order_mark_is_accepted() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(br#"{"firstRunDone":true,"theme":"light"}"#);
        let s = Settings::parse(&bytes).unwrap();
        assert!(s.first_run_done);
        assert_eq!(s.theme, "light");
    }

    #[test]
    fn unknown_fields_and_missing_fields_are_fine() {
        let s: Settings = serde_json::from_str(r#"{"theme":"dark","futureOption":1}"#).unwrap();
        assert_eq!(s.theme, "dark");
        assert_eq!(s.hotkey, "Ctrl+Alt+Space");
    }
}
