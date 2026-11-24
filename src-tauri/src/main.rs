// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // The same executable doubles as the elevated fast-mode helper. It must not
    // start any UI, so this check runs before Tauri is touched.
    if findit_app::helper::is_helper_invocation() {
        std::process::exit(findit_app::helper::helper_main());
    }
    findit_app::run();
}
