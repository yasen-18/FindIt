//! FindIt desktop app: Tauri setup, tray, global hotkey, and the command surface.

pub mod commands;
pub mod engine;
pub mod helper;
pub mod logging;
pub mod paths;
pub mod settings;
pub mod shell;
pub mod strings;
pub mod sysinfo;

use commands::AppCore;
use engine::Engine;
use parking_lot::RwLock;
use paths::DataPaths;
use settings::Settings;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::window::{Effect, EffectsBuilder};
use tauri::{AppHandle, Emitter, Manager, RunEvent, Wry};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

const TRAY_ID: &str = "main";

/// The tray's "Pause indexing" item, kept so its check mark can follow the UI.
struct TrayPause(CheckMenuItem<Wry>);

/// Shows the window and focuses the search box (hotkey, tray, second launch).
pub fn summon(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        let _ = app.emit("summon", ());
    }
}

/// Registers the global hotkey, replacing the previous one.
pub fn register_hotkey(app: &AppHandle, combo: &str) -> Result<(), String> {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let combo = combo.trim();
    if combo.is_empty() {
        return Ok(());
    }
    let shortcut: Shortcut = combo.parse().map_err(|_| strings::t("errors.hotkeyInvalid").replace("{combo}", combo))?;
    gs.register(shortcut).map_err(|e| {
        log::warn!(target: "findit", "hotkey {combo} not available: {e}");
        strings::t("errors.hotkeyTaken").replace("{combo}", combo)
    })
}

pub fn set_autostart(app: &AppHandle, on: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let al = app.autolaunch();
    let r = if on { al.enable() } else { al.disable() };
    if let Err(e) = r {
        log::warn!(target: "findit", "could not change start with Windows: {e}");
    }
}

pub fn set_tray_visible(app: &AppHandle, visible: bool) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_visible(visible);
    }
}

pub fn sync_tray_pause(app: &AppHandle, paused: bool) {
    if let Some(p) = app.try_state::<TrayPause>() {
        let _ = p.0.set_checked(paused);
    }
}

fn create_tray(app: &tauri::App, visible: bool) -> tauri::Result<()> {
    let t = strings::t;
    let open = MenuItem::with_id(app, "open", t("tray.open"), true, None::<&str>)?;
    let pause = CheckMenuItem::with_id(app, "pause", t("tray.pause"), true, false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", t("tray.exit"), true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &sep1, &pause, &sep2, &quit])?;
    let pause_item = pause.clone();
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("FindIt")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => summon(app),
            "pause" => {
                let paused = pause_item.is_checked().unwrap_or(false);
                if let Some(core) = app.try_state::<Arc<AppCore>>() {
                    core.engine.set_paused(paused);
                }
                let _ = app.emit("paused-changed", paused);
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                summon(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    let tray = builder.build(app)?;
    tray.set_visible(visible)?;
    app.manage(TrayPause(pause));
    Ok(())
}

/// When `run` started; used to log how long the window took to appear.
pub static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

pub fn run() {
    STARTED.get_or_init(std::time::Instant::now);
    helper::wait_for_previous_instance();
    let paths = DataPaths::resolve();
    let _ = paths.ensure();
    logging::init(&paths.logs);
    helper::install_crash_handler();
    log::info!(target: "findit", "FindIt {} starting{}", env!("CARGO_PKG_VERSION"), if paths.portable { " (portable)" } else { "" });
    let settings = Arc::new(RwLock::new(Settings::load(&paths.settings)));
    let start_minimized = std::env::args().any(|a| a == "--minimized");

    let app = tauri::Builder::default()
        // Must be the first plugin: a second launch hands over to the running app.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| summon(app)))
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec!["--minimized"])))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        summon(app);
                    }
                })
                .build(),
        )
        .setup(move |app| {
            let handle = app.handle().clone();
            let engine = Engine::new(handle.clone(), paths.clone(), settings.clone());
            let core = Arc::new(AppCore::new(engine.clone(), start_minimized));
            app.manage(core.clone());
            if let Some(w) = app.get_webview_window("main") {
                // Mica backdrop on Windows 11; the page draws a solid background elsewhere.
                if sysinfo::windows_build() >= 22_000 && w.set_effects(EffectsBuilder::new().effect(Effect::Mica).build()).is_ok() {
                    core.mica.store(true, Ordering::Relaxed);
                }
            }
            let (tray, hotkey, autostart) = {
                let s = settings.read();
                (s.tray_icon, s.hotkey.clone(), s.start_with_windows)
            };
            if let Err(e) = create_tray(app, tray) {
                log::warn!(target: "findit", "tray icon unavailable: {e}");
            }
            if let Err(e) = register_hotkey(&handle, &hotkey) {
                *core.hotkey_error.lock() = Some(e);
            }
            {
                use tauri_plugin_autostart::ManagerExt;
                if handle.autolaunch().is_enabled().unwrap_or(false) != autostart {
                    set_autostart(&handle, autostart);
                }
            }
            engine.start();
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if let Some(core) = window.try_state::<Arc<AppCore>>() {
                    let s = core.engine.settings.read();
                    if s.close_to_tray && s.tray_icon {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::search,
            commands::get_rows,
            commands::ids_in_range,
            commands::index_of,
            commands::selection_info,
            commands::open_items,
            commands::copy_items,
            commands::rename_item,
            commands::delete_items,
            commands::preview,
            commands::type_names,
            commands::icons,
            commands::get_status,
            commands::get_settings,
            commands::save_settings,
            commands::remember_query,
            commands::clear_history,
            commands::list_drives,
            commands::finish_first_run,
            commands::enable_fast_mode,
            commands::disable_fast_mode,
            commands::rebuild_index,
            commands::set_paused,
            commands::app_info,
            commands::app_ready,
            commands::hide_window,
            commands::quit_app,
            commands::open_folder,
            commands::open_external,
        ])
        .build(tauri::generate_context!());

    let app = match app {
        Ok(a) => a,
        Err(e) => {
            log::error!(target: "findit", "could not start: {e}");
            log::logger().flush();
            std::process::exit(1);
        }
    };
    app.run(|app, event| {
        if let RunEvent::Exit = event {
            if let Some(core) = app.try_state::<Arc<AppCore>>() {
                core.engine.shutdown();
            }
            log::info!(target: "findit", "FindIt stopped");
            log::logger().flush();
        }
    });
}
