//! Pairing and connecting a phone on a background thread.

use crate::adb::{self, Adb};
use crate::qr;
use rand::{distributions::Alphanumeric, Rng};
use std::collections::HashSet;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(500);
pub const MAX_ATTEMPTS: u32 = 3;
const FIREWALL_HINT_AFTER: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
pub const SERVER_HUNG: &str = "The adb server isn't responding. Restart it with \"Restart adb server\" at the top, \
                               then try again.";

/// What the UI shows while a pairing session runs.
#[derive(Default)]
pub struct Progress {
    /// The QR code to scan, while waiting for a scan.
    pub qr: Option<qr::Matrix>,
    pub attempt: u32,
    /// When the current QR code expires.
    pub deadline: Option<Instant>,
    pub status: String,
    pub warning: Option<String>,
    /// The QR code timed out without being scanned.
    pub expired: bool,
    /// Set once the session ends: the connected device, or why it failed.
    pub outcome: Option<Result<String, String>>,
}

pub struct Session {
    pub progress: Arc<Mutex<Progress>>,
    cancel: Arc<AtomicBool>,
}

impl Session {
    /// Starts pairing on a new thread. `notify` is called whenever progress changes.
    pub fn start(adb: Adb, timeout: Duration, notify: impl Fn() + Send + 'static) -> Session {
        let progress = Arc::new(Mutex::new(Progress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            adb,
            timeout,
            progress: progress.clone(),
            cancel: cancel.clone(),
            notify: Box::new(notify),
        };
        thread::spawn(move || {
            let outcome = worker.run();
            if !worker.cancelled() {
                worker.update(|p| {
                    p.qr = None;
                    p.deadline = None;
                    p.outcome = Some(outcome);
                });
            }
        });
        Session { progress, cancel }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct Worker {
    adb: Adb,
    timeout: Duration,
    progress: Arc<Mutex<Progress>>,
    cancel: Arc<AtomicBool>,
    notify: Box<dyn Fn() + Send>,
}

impl Worker {
    fn update(&self, f: impl FnOnce(&mut Progress)) {
        f(&mut self.progress.lock().unwrap());
        (self.notify)();
    }

    fn status(&self, text: impl Into<String>) {
        let text = text.into();
        self.update(|p| p.status = text);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn check_cancel(&self) -> Result<(), String> {
        if self.cancelled() {
            Err("Cancelled.".into())
        } else {
            Ok(())
        }
    }

    fn run(&self) -> Result<String, String> {
        let (phone_ip, guid) = self.pair()?;
        let serial = self.connect(&phone_ip, guid.as_deref())?;

        let model = self
            .adb
            .run(&["-s", &serial, "shell", "getprop", "ro.product.model"], Duration::from_secs(10))
            .map(|o| o.text.trim().to_string())
            .unwrap_or_default();
        Ok(if model.is_empty() { serial } else { format!("{model} ({serial})") })
    }

    fn start_server(&self) -> Result<(), String> {
        self.status("Starting adb...");
        // Waiting longer than a cold start only delays telling the user it is hung.
        let started = self.adb.run(&["start-server"], adb::COLD_START).map_err(|e| {
            if adb::is_timeout(&e) {
                SERVER_HUNG.to_string()
            } else {
                e
            }
        })?;
        if !started.success {
            return Err(format!("adb start-server failed: {}", started.text.trim()));
        }
        Ok(())
    }

    /// Warns if adb has no mDNS, since then the phone is never found. Only a
    /// warning, so it runs after the QR code is up rather than before.
    fn check_mdns(&self) {
        match self.adb.run(&["mdns", "check"], Duration::from_secs(10)) {
            Ok(out) if out.text.contains("mdns daemon version") => {}
            _ => self.update(|p| {
                p.warning = Some(
                    "adb reports mDNS is unavailable, so the phone may not be found. \
                     Update platform-tools to version 34 or newer."
                        .into(),
                )
            }),
        }
    }

    /// Shows QR codes until one pairs, then returns the phone's IP and guid.
    fn pair(&self) -> Result<(String, Option<String>), String> {
        let mut last_error = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            let name = format!("adbqr-{}", random_alnum(8));
            let password = random_alnum(10);
            let matrix = qr::encode(&qr::pairing_payload(&name, &password))?;
            // The QR code needs nothing from adb, so show it before the
            // server is up; a cold server start takes a few seconds.
            self.update(|p| {
                p.qr = Some(matrix);
                p.attempt = attempt;
                p.deadline = None;
            });
            if attempt == 1 {
                self.start_server()?;
            }

            let addr = self.wait_for_pairing_service(&name, attempt == 1)?;
            let ip = adb::host_of(&addr).to_string();
            self.update(|p| {
                p.qr = None;
                p.deadline = None;
                p.status = format!("Found phone at {addr}, pairing...");
            });

            match self.adb.run(&["pair", &addr, &password], Duration::from_secs(30)) {
                Ok(out) if adb::pair_succeeded(&out.text) => {
                    self.status("Paired. Connecting...");
                    return Ok((ip, adb::pair_guid(&out.text)));
                }
                Ok(out) => last_error = out.text.trim().to_string(),
                Err(e) => last_error = e,
            }
            self.check_cancel()?;
            self.update(|p| p.warning = Some(format!("Pairing failed: {last_error}")));
        }
        Err(format!("Pairing failed after {MAX_ATTEMPTS} attempts: {last_error}"))
    }

    /// Polls mDNS for the pairing service the phone advertises after the scan.
    /// The QR code's countdown starts here, once adb is up to see the phone.
    fn wait_for_pairing_service(&self, name: &str, check_mdns: bool) -> Result<String, String> {
        let start = Instant::now();
        self.update(|p| {
            p.deadline = Some(start + self.timeout);
            p.status = "Waiting for the phone to scan the QR code...".into();
        });
        let mut hinted = false;
        let mut mdns_checked = !check_mdns;
        loop {
            self.check_cancel()?;
            if let Ok(services) = self.adb.quiet().mdns_services() {
                if let Some(s) = services
                    .iter()
                    .find(|s| s.instance == name && s.service_type.contains("_adb-tls-pairing"))
                {
                    return Ok(s.addr.clone());
                }
            }
            if !mdns_checked {
                mdns_checked = true;
                self.check_mdns();
            }
            let elapsed = start.elapsed();
            if !hinted && elapsed > FIREWALL_HINT_AFTER {
                hinted = true;
                self.update(|p| {
                    p.warning = Some(
                        "Still waiting. If you already scanned, check that your firewall \
                         allows adb on this network (mDNS uses UDP 5353)."
                            .into(),
                    )
                });
            }
            if elapsed > self.timeout {
                self.update(|p| p.expired = true);
                return Err(timeout_hint(self.timeout));
            }
            sleep(POLL);
        }
    }

    /// Waits for the paired phone to appear in `adb devices`, connecting it if adb doesn't.
    fn connect(&self, ip: &str, guid: Option<&str>) -> Result<String, String> {
        let matches = |serial: &str| {
            serial.starts_with(&format!("{ip}:")) || guid.is_some_and(|g| serial.contains(g))
        };
        let start = Instant::now();
        let mut tried = HashSet::new();
        let mut last_error = String::from("the phone never advertised its connection service");

        loop {
            self.check_cancel()?;
            if let Ok(devices) = self.adb.quiet().devices() {
                if let Some(d) = devices.iter().find(|d| d.state == "device" && matches(&d.serial)) {
                    return Ok(d.serial.clone());
                }
            }
            if let Ok(services) = self.adb.quiet().mdns_services() {
                for s in services.iter().filter(|s| {
                    s.service_type.contains("_adb-tls-connect")
                        && (adb::host_of(&s.addr) == ip || guid == Some(s.instance.as_str()))
                }) {
                    if tried.insert(s.addr.clone()) {
                        self.status(format!("Connecting to {}...", s.addr));
                        match self.adb.run(&["connect", &s.addr], Duration::from_secs(15)) {
                            Ok(out) if out.text.contains("connected to") => {}
                            Ok(out) => last_error = out.text.trim().to_string(),
                            Err(e) => last_error = e,
                        }
                    }
                }
            }
            if start.elapsed() > CONNECT_TIMEOUT {
                return Err(format!("Paired, but could not connect: {last_error}"));
            }
            sleep(POLL);
        }
    }
}

fn timeout_hint(timeout: Duration) -> String {
    let local = local_ip().map(|ip| format!(" (this PC: {ip})")).unwrap_or_default();
    format!(
        "No phone responded within {}s. Check that:\n\
         - the phone and this PC are on the same Wi-Fi network{local}\n\
         - your firewall allows adb on this network (mDNS, UDP 5353)\n\
         - you used \"Pair device with QR code\" under Developer options > Wireless debugging",
        timeout.as_secs()
    )
}

/// The IPv4 address of the default route. No packet is sent.
fn local_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

fn random_alnum(len: usize) -> String {
    rand::thread_rng().sample_iter(&Alphanumeric).take(len).map(char::from).collect()
}
