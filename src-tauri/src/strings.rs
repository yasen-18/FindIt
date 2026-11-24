//! User-visible strings for the Rust side (tray menu, dialogs), read from the same
//! resource file as the web UI so everything can be translated in one place.

use serde_json::Value;
use std::sync::OnceLock;

static STRINGS: OnceLock<Value> = OnceLock::new();

fn table() -> &'static Value {
    STRINGS.get_or_init(|| serde_json::from_str(include_str!("../../src/locales/en.json")).unwrap_or(Value::Null))
}

/// Looks up a dotted key such as `tray.open`. Falls back to the key itself.
pub fn t(key: &str) -> String {
    let mut v = table();
    for part in key.split('.') {
        match v.get(part) {
            Some(next) => v = next,
            None => return key.to_string(),
        }
    }
    v.as_str().map_or_else(|| key.to_string(), str::to_string)
}
