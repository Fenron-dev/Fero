//! Tray icon and the timer that checks subscriptions on its own.
//!
//! A downloader is only useful if it runs when nobody is looking. Closing the
//! window therefore hides it instead of quitting; the tray is what remains, and
//! the timer keeps working behind it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WindowEvent};

use super::*;

/// How often subscriptions are checked when nothing else is configured.
const DEFAULT_INTERVAL_HOURS: u64 = 6;

/// Granularity of the timer.
///
/// The thread wakes once a minute and compares timestamps rather than sleeping
/// for the whole interval: a machine that was asleep for four hours should run
/// its due check shortly after waking, not four hours later.
const TICK: Duration = Duration::from_secs(60);

/// Whether the timer is currently allowed to start runs.
static SCHEDULE_PAUSED: AtomicBool = AtomicBool::new(false);

/// UNIX timestamp of the last scheduled run.
static LAST_SCHEDULED_RUN: AtomicU64 = AtomicU64::new(0);

/// Label of the main window, as declared in `tauri.conf.json`.
const MAIN_WINDOW: &str = "main";

/// Builds the tray icon and starts the timer.
///
/// # Errors
/// [`FeroError::AppStartup`] when the tray cannot be created.
pub(super) fn install(app: &AppHandle) -> Result<()> {
    let open = MenuItem::with_id(app, "open", "Fero öffnen", true, None::<&str>)
        .map_err(|error| FeroError::AppStartup(error.to_string()))?;
    let check = MenuItem::with_id(app, "check", "Alle Abos prüfen", true, None::<&str>)
        .map_err(|error| FeroError::AppStartup(error.to_string()))?;
    let pause = MenuItem::with_id(app, "pause", "Zeitplan pausieren", true, None::<&str>)
        .map_err(|error| FeroError::AppStartup(error.to_string()))?;
    let separator =
        PredefinedMenuItem::separator(app).map_err(|error| FeroError::AppStartup(error.to_string()))?;
    let quit = MenuItem::with_id(app, "quit", "Beenden", true, None::<&str>)
        .map_err(|error| FeroError::AppStartup(error.to_string()))?;

    let menu = Menu::with_items(app, &[&open, &check, &pause, &separator, &quit])
        .map_err(|error| FeroError::AppStartup(error.to_string()))?;

    TrayIconBuilder::with_id("fero-tray")
        .tooltip("Fero")
        .icon(
            app.default_window_icon()
                .cloned()
                .ok_or_else(|| FeroError::AppStartup("Tray-Icon fehlt".to_string()))?,
        )
        .menu(&menu)
        .on_menu_event(handle_menu_event)
        .build(app)
        .map_err(|error| FeroError::AppStartup(error.to_string()))?;

    start_timer();
    Ok(())
}

/// Reacts to a click in the tray menu.
fn handle_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "open" => show_main_window(app),
        "check" => {
            if !webnovel::start_scheduled_check() {
                debug_log("Tray: Prüfung angefordert, es läuft bereits eine.");
            }
        }
        "pause" => {
            let paused = !SCHEDULE_PAUSED.fetch_xor(true, Ordering::SeqCst);
            debug_log(&format!(
                "Tray: Zeitplan {}",
                if paused { "pausiert" } else { "fortgesetzt" }
            ));
        }
        "quit" => app.exit(0),
        _ => {}
    }
}

/// Brings the main window back to the front.
fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Hides the window instead of quitting when it is closed.
///
/// Without this, closing the window would end the process and with it every
/// scheduled check — the one thing a downloader must keep doing.
pub(super) fn handle_window_event(window: &tauri::Window, event: &WindowEvent) {
    if let WindowEvent::CloseRequested { api, .. } = event {
        if window.label() == MAIN_WINDOW {
            api.prevent_close();
            let _ = window.hide();
            debug_log("Fenster geschlossen — Fero läuft im Hintergrund weiter.");
        }
    }
}

/// Runs the timer until the application exits.
///
/// Deliberately takes no `AppHandle`: a scheduled check resolves its own
/// workspace and reports through the job registry, so the timer needs nothing
/// from the window layer.
fn start_timer() {
    std::thread::spawn(|| {
        // Give the first request a chance to set up the data directory before
        // the first check runs.
        std::thread::sleep(Duration::from_secs(30));
        loop {
            if is_due() && !SCHEDULE_PAUSED.load(Ordering::SeqCst) {
                LAST_SCHEDULED_RUN.store(unix_now(), Ordering::SeqCst);
                if webnovel::start_scheduled_check() {
                    debug_log("Zeitplan: Prüflauf gestartet.");
                }
            }
            std::thread::sleep(TICK);
        }
    });
}

/// Whether the configured interval has elapsed.
fn is_due() -> bool {
    let interval = interval_hours().saturating_mul(3600);
    let last = LAST_SCHEDULED_RUN.load(Ordering::SeqCst);
    if last == 0 {
        // Never run in this session — the sleep above already delayed the first
        // check, so run it now rather than waiting a full interval.
        return true;
    }
    unix_now().saturating_sub(last) >= interval
}

/// Configured interval, in hours.
fn interval_hours() -> u64 {
    resolve_workspace(None)
        .ok()
        .map(|ws| load_schedule_settings(&ws.store).interval_hours)
        .unwrap_or(DEFAULT_INTERVAL_HOURS)
}

/// Whether the scheduler is paused right now.
pub(super) fn is_paused() -> bool {
    SCHEDULE_PAUSED.load(Ordering::SeqCst)
}

/// Sets the paused state from the settings page.
pub(super) fn set_paused(paused: bool) {
    SCHEDULE_PAUSED.store(paused, Ordering::SeqCst);
}
