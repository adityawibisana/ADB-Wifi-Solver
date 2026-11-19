// No console window behind the app on Windows (release builds).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adb;
mod app;
mod pairing;
mod qr;

use std::sync::OnceLock;
use std::time::Instant;

static LAUNCHED: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since the process started, for the startup timings in the adb log.
pub fn ms_since_launch() -> u128 {
    LAUNCHED.get().map_or(0, |t| t.elapsed().as_millis())
}

fn main() -> eframe::Result {
    LAUNCHED.get_or_init(Instant::now);
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("adbqr")
        .with_inner_size([820.0, 620.0])
        .with_min_inner_size([620.0, 480.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("adbqr", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
