//! The desktop window: pairing on the left, Wi-Fi devices on the right, adb log below.

use crate::adb::{self, Adb, Device, Logger};
use crate::pairing::{Session, MAX_ATTEMPTS};
use crate::qr;
use eframe::egui::{self, Color32, RichText, Sense, Vec2};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const DEVICE_POLL: Duration = Duration::from_secs(2);
const LOG_LINES: usize = 1000;
const OK_GREEN: Color32 = Color32::from_rgb(0x2e, 0xa0, 0x43);

#[derive(Default)]
struct DeviceList {
    devices: Vec<Device>,
    error: Option<String>,
    loaded: bool,
}

/// State shared with background threads.
struct Shared {
    ctx: egui::Context,
    log: Mutex<VecDeque<String>>,
    /// The adb the device poller uses; `None` until adb is found.
    adb: Mutex<Option<Adb>>,
    devices: Mutex<DeviceList>,
    refresh_now: AtomicBool,
    disconnecting: Mutex<HashSet<String>>,
    /// Result of the last disconnect: message and whether it failed.
    notice: Mutex<Option<(String, bool)>>,
}

impl Shared {
    fn log(&self, line: String) {
        let mut log = self.log.lock().unwrap();
        if log.len() >= LOG_LINES {
            log.pop_front();
        }
        log.push_back(line);
        drop(log);
        self.ctx.request_repaint();
    }

    fn refresh(&self) {
        self.refresh_now.store(true, Ordering::Relaxed);
    }
}

pub struct App {
    shared: Arc<Shared>,
    adb: Result<Adb, String>,
    adb_input: String,
    editing_adb: bool,
    timeout_secs: u64,
    session: Option<Session>,
    show_log: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        let shared = Arc::new(Shared {
            ctx: cc.egui_ctx.clone(),
            log: Mutex::default(),
            adb: Mutex::default(),
            devices: Mutex::default(),
            refresh_now: AtomicBool::new(false),
            disconnecting: Mutex::default(),
            notice: Mutex::default(),
        });
        spawn_device_poller(shared.clone());

        let mut app = App {
            shared,
            adb: Err(String::new()),
            adb_input: String::new(),
            editing_adb: false,
            timeout_secs: 120,
            session: None,
            show_log: false,
        };
        app.set_adb(None);
        // Show a QR code right away; most of the time that's why the app was opened.
        app.start_pairing();
        app
    }

    fn set_adb(&mut self, explicit: Option<PathBuf>) {
        let shared = self.shared.clone();
        let logger: Logger = Arc::new(move |line| shared.log(line));
        self.adb = adb::locate(explicit.as_deref()).map(|p| Adb::new(p, Some(logger)));
        match &self.adb {
            Ok(adb) => {
                self.adb_input = adb.path().display().to_string();
                self.editing_adb = false;
            }
            Err(_) => self.editing_adb = true,
        }
        *self.shared.adb.lock().unwrap() = self.adb.as_ref().ok().map(Adb::quiet);
        *self.shared.devices.lock().unwrap() = DeviceList::default();
        self.shared.refresh();
    }

    fn start_pairing(&mut self) {
        if let Ok(adb) = &self.adb {
            let ctx = self.shared.ctx.clone();
            let shared = self.shared.clone();
            self.session = Some(Session::start(
                adb.clone(),
                Duration::from_secs(self.timeout_secs),
                move || {
                    shared.refresh();
                    ctx.request_repaint();
                },
            ));
        }
    }

    fn disconnect(&self, serials: Vec<String>) {
        let Ok(adb) = self.adb.clone() else { return };
        let shared = self.shared.clone();
        {
            let mut busy = shared.disconnecting.lock().unwrap();
            busy.extend(serials.iter().cloned());
        }
        thread::spawn(move || {
            let mut messages = Vec::new();
            let mut failed = false;
            for serial in serials {
                match adb.disconnect(&serial) {
                    Ok(m) => messages.push(m),
                    Err(e) => {
                        failed = true;
                        messages.push(format!("Could not disconnect {serial}: {e}"));
                    }
                }
                shared.disconnecting.lock().unwrap().remove(&serial);
                shared.refresh();
                shared.ctx.request_repaint();
            }
            *shared.notice.lock().unwrap() = Some((messages.join("\n"), failed));
            shared.ctx.request_repaint();
        });
    }

    fn adb_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("adb").strong());
            if self.editing_adb {
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.adb_input)
                        .hint_text("Path to adb or the platform-tools folder")
                        .desired_width(ui.available_width() - 150.0),
                );
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Use").clicked() || enter {
                    let path = self.adb_input.trim().trim_matches('"').to_string();
                    self.set_adb((!path.is_empty()).then(|| PathBuf::from(path)));
                }
                if ui.button("Auto-detect").clicked() {
                    self.set_adb(None);
                }
            } else {
                let change = ui
                    .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let clicked = ui.small_button("Change").clicked();
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(RichText::new(&self.adb_input).monospace()).truncate())
                                .on_hover_text(&self.adb_input);
                        });
                        clicked
                    })
                    .inner;
                if change {
                    self.editing_adb = true;
                }
            }
        });
        if let Err(e) = &self.adb {
            if !e.is_empty() {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
        }
    }

    fn pairing_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Pair a phone");
        ui.add_space(4.0);
        ui.label(
            "On the phone: Developer options > Wireless debugging > Pair device with QR code. \
             The phone and this PC must be on the same Wi-Fi network.",
        );
        ui.add_space(12.0);

        let Some(session) = &self.session else {
            ui.horizontal(|ui| {
                if primary_button(ui, "Show pairing QR code", self.adb.is_ok()).clicked() {
                    self.start_pairing();
                }
                ui.label("Scan timeout");
                ui.add(egui::DragValue::new(&mut self.timeout_secs).range(10..=900).suffix(" s"));
            });
            return;
        };

        let mut cancel = false;
        let mut again = false;
        {
            let p = session.progress.lock().unwrap();
            if let Some(matrix) = &p.qr {
                let side = ui.available_width().min(ui.available_height() - 90.0).clamp(160.0, 380.0);
                draw_qr(ui, matrix, side);
                ui.add_space(6.0);
                let left = p.deadline.map_or(0, |d| d.saturating_duration_since(Instant::now()).as_secs());
                ui.label(format!(
                    "Attempt {}/{MAX_ATTEMPTS} · QR code expires in {left}s",
                    p.attempt
                ));
            }
            match &p.outcome {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(&p.status);
                    });
                    if let Some(w) = &p.warning {
                        ui.colored_label(ui.visuals().warn_fg_color, w);
                    }
                    ui.add_space(6.0);
                    cancel = ui.button("Cancel").clicked();
                    ui.ctx().request_repaint_after(Duration::from_millis(500));
                }
                Some(Ok(device)) => {
                    ui.label(RichText::new(format!("Connected: {device}")).color(OK_GREEN).size(16.0));
                    ui.add_space(8.0);
                    again = ui.button("Pair another phone").clicked();
                }
                Some(Err(e)) if p.expired => {
                    ui.label(RichText::new("The QR code expired.").size(18.0).strong());
                    ui.add_space(10.0);
                    again = primary_button(ui, "Show new QR code", true).clicked();
                    ui.add_space(14.0);
                    ui.label(RichText::new(e).weak());
                }
                Some(Err(e)) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        again = primary_button(ui, "Try again", true).clicked();
                        cancel = ui.button("Close").clicked();
                    });
                }
            }
        }
        if cancel {
            self.session = None;
        } else if again {
            self.start_pairing();
        }
    }

    fn devices_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Wi-Fi devices");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Refresh").clicked() {
                    self.shared.refresh();
                }
            });
        });
        ui.add_space(4.0);

        let (wireless, other, error, loaded) = {
            let list = self.shared.devices.lock().unwrap();
            let (w, o): (Vec<Device>, Vec<Device>) =
                list.devices.iter().cloned().partition(Device::is_wireless);
            (w, o.len(), list.error.clone(), list.loaded)
        };
        let busy = self.shared.disconnecting.lock().unwrap().clone();

        if let Some(e) = error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        } else if !loaded {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Looking for devices...");
            });
        } else if wireless.is_empty() {
            ui.label(RichText::new("No devices connected over Wi-Fi.").weak());
        }

        let mut to_disconnect = Vec::new();
        egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(ui.available_height() - 80.0).show(ui, |ui| {
            for d in &wireless {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(d.model.as_deref().unwrap_or("Unknown device")).strong());
                            ui.label(RichText::new(&d.serial).small().weak());
                            if d.state != "device" {
                                ui.colored_label(ui.visuals().warn_fg_color, &d.state);
                            }
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if busy.contains(&d.serial) {
                                ui.spinner();
                            } else if ui
                                .button("Disconnect")
                                .on_hover_text(disconnect_tooltip(&d.serial))
                                .clicked()
                            {
                                to_disconnect.push(d.serial.clone());
                            }
                        });
                    });
                });
            }
        });

        if wireless.len() > 1 {
            ui.add_space(4.0);
            let idle: Vec<String> =
                wireless.iter().map(|d| d.serial.clone()).filter(|s| !busy.contains(s)).collect();
            if ui.add_enabled(!idle.is_empty(), egui::Button::new("Disconnect all")).clicked() {
                to_disconnect.extend(idle);
            }
        }
        if other > 0 {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("{other} USB or emulator device(s) not shown.")).small().weak());
        }

        let notice = self.shared.notice.lock().unwrap().clone();
        if let Some((msg, failed)) = notice {
            ui.add_space(6.0);
            let color = if failed { ui.visuals().error_fg_color } else { OK_GREEN };
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(color, msg);
                if ui.small_button("✕").clicked() {
                    *self.shared.notice.lock().unwrap() = None;
                }
            });
        }

        if !to_disconnect.is_empty() {
            *self.shared.notice.lock().unwrap() = None;
            self.disconnect(to_disconnect);
        }
    }

    fn log_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let label = if self.show_log { "Hide adb log" } else { "Show adb log" };
            if ui.small_button(label).clicked() {
                self.show_log = !self.show_log;
            }
            if self.show_log && ui.small_button("Clear").clicked() {
                self.shared.log.lock().unwrap().clear();
            }
        });
        if self.show_log {
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                let log = self.shared.log.lock().unwrap();
                for line in log.iter() {
                    ui.label(RichText::new(line).monospace().small());
                }
            });
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("adb").show(ui, |ui| {
            ui.add_space(4.0);
            self.adb_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("log")
            .resizable(self.show_log)
            .default_size(180.0)
            .show(ui, |ui| self.log_ui(ui));
        egui::Panel::right("devices")
            .resizable(true)
            .default_size(340.0)
            .min_size(260.0)
            .show(ui, |ui| {
                ui.add_space(8.0);
                self.devices_ui(ui);
            });
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(4.0);
            self.pairing_ui(ui);
        });
    }
}

/// A large, filled button for the main action.
fn primary_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let button = egui::Button::new(RichText::new(text).size(18.0).strong().color(Color32::WHITE))
        .fill(Color32::from_rgb(0x25, 0x63, 0xeb))
        .corner_radius(8.0)
        .min_size(Vec2::new(240.0, 48.0));
    ui.add_enabled(enabled, button).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Paints the QR code with a white quiet zone, so it scans in light and dark themes.
fn draw_qr(ui: &mut egui::Ui, matrix: &qr::Matrix, side: f32) {
    let modules = matrix.width + 2 * qr::QUIET;
    // Whole physical pixels per module keep the edges crisp.
    let ppp = ui.ctx().pixels_per_point();
    let module = ((side * ppp / modules as f32).floor().max(1.0)) / ppp;
    let size = module * modules as f32;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::WHITE);
    let origin = rect.min + Vec2::splat(module * qr::QUIET as f32);
    for y in 0..matrix.width {
        for x in 0..matrix.width {
            if matrix.is_dark(x, y) {
                let min = origin + Vec2::new(x as f32, y as f32) * module;
                painter.rect_filled(egui::Rect::from_min_size(min, Vec2::splat(module)), 0.0, Color32::BLACK);
            }
        }
    }
}

fn disconnect_tooltip(serial: &str) -> &'static str {
    if adb::is_mdns_serial(serial) {
        "Turns off Wireless debugging on the phone. adb reconnects paired phones on its own, \
         so this is what keeps it disconnected. Turn it back on in Developer options to reconnect."
    } else {
        "Runs adb disconnect for this address."
    }
}

/// Refreshes the device list every couple of seconds, or right away on request.
fn spawn_device_poller(shared: Arc<Shared>) {
    thread::spawn(move || loop {
        let adb = shared.adb.lock().unwrap().clone();
        if let Some(adb) = adb {
            let result = adb.devices();
            let mut list = shared.devices.lock().unwrap();
            match result {
                Ok(devices) => {
                    list.devices = devices;
                    list.error = None;
                }
                Err(e) => list.error = Some(e),
            }
            list.loaded = true;
            drop(list);
            shared.ctx.request_repaint();
        }
        let start = Instant::now();
        while start.elapsed() < DEVICE_POLL && !shared.refresh_now.swap(false, Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
        }
    });
}
