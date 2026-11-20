// No console window behind the app on Windows (release builds).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adb;
mod app;
mod pairing;
mod qr;

use std::sync::OnceLock;
use std::time::Instant;
use tauri::Manager;

static LAUNCHED: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since the process started, for the startup timings in the adb log.
pub fn ms_since_launch() -> u128 {
    LAUNCHED.get().map_or(0, |t| t.elapsed().as_millis())
}

fn main() {
    LAUNCHED.get_or_init(Instant::now);
    tauri::Builder::default()
        .setup(|app| {
            let shared = app::Shared::new(app.handle().clone());
            app.manage(shared.clone());
            shared.start();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app::get_state,
            app::get_log,
            app::clear_log,
            app::set_adb,
            app::start_pairing,
            app::cancel_pairing,
            app::disconnect,
            app::reconnect,
            app::restart_server,
            app::refresh,
            app::dismiss_notice,
        ])
        .run(tauri::generate_context!())
        .expect("could not start adbqr");
}
