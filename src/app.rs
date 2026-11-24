//! App state shared by the window and the background threads, and the
//! commands the window calls. The window renders from [`Shared::snapshot`]
//! and refetches it whenever a `changed` event arrives.

use crate::adb::{self, Adb, Device, Logger, MdnsService};
use crate::finder::{self, Target};
use crate::firewall;
use crate::history::{self, Observation, Store};
use crate::net::{self, Network};
use crate::pairing::{Help, Session, MAX_ATTEMPTS};
use crate::qr;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddrV4;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

const DEVICE_POLL: Duration = Duration::from_secs(2);
const LOG_LINES: usize = 1000;
/// At most one `changed` event per this interval; the window refetches on each.
const EMIT_INTERVAL: Duration = Duration::from_millis(40);
/// How often to re-read a connected phone's address for the history.
const OBSERVE_EVERY: Duration = Duration::from_secs(60);
/// How often to check the network and try reconnecting paired phones.
const WATCH_EVERY: Duration = Duration::from_secs(10);
/// Waits between automatic tries for a phone that wasn't found, doubling up to the maximum.
const RETRY_FIRST: Duration = Duration::from_secs(20);
const RETRY_MAX: Duration = Duration::from_secs(300);
const MDNS_SUFFIX: &str = "._adb-tls-connect._tcp";

#[derive(Default)]
struct DeviceList {
    devices: Vec<Device>,
    /// Phones with Wireless debugging on that adb isn't connected to.
    available: Vec<MdnsService>,
    error: Option<String>,
    /// `adb devices` timed out: the server is running but not answering.
    server_hung: bool,
    loaded: bool,
}

#[derive(Default)]
struct Log {
    lines: VecDeque<String>,
    /// Number of lines ever logged, so the window can fetch only new ones.
    total: u64,
}

/// A message shown until dismissed: its text and whether it is an error.
type Notice = Option<(String, bool)>;

pub struct Shared {
    handle: AppHandle,
    log: Mutex<Log>,
    adb: Mutex<Result<Adb, String>>,
    timeout_secs: AtomicU64,
    session: Mutex<Option<Session>>,
    devices: Mutex<DeviceList>,
    refresh_now: AtomicBool,
    /// Serials being disconnected and addresses being reconnected.
    busy: Mutex<HashSet<String>>,
    /// What a USB phone being switched to Wi-Fi is doing, by serial.
    progress: Mutex<HashMap<String, String>>,
    /// USB serial -> Wi-Fi serial of phones switched to Wi-Fi, to mark them as on Wi-Fi.
    switched: Mutex<HashMap<String, String>>,
    /// Result of the last disconnect, reconnect or switch to Wi-Fi.
    notice: Mutex<Notice>,
    restarting: AtomicBool,
    /// Result of the last server restart.
    restart_notice: Mutex<Notice>,
    dirty: Mutex<bool>,
    dirty_cv: Condvar,
    qr_shown: AtomicBool,
    /// Phones connected over Wi-Fi before, and where.
    history: Mutex<Store>,
    history_path: Mutex<Option<PathBuf>>,
    /// Serial numbers of phones in adb's own pairing list.
    paired: Mutex<HashSet<String>>,
    /// adb serial -> when its details were last read, and its serial number.
    observed: Mutex<HashMap<String, (Instant, String)>>,
    /// What a search for a paired phone is doing, by serial number.
    finding: Mutex<HashMap<String, String>>,
    network: Mutex<Option<Network>>,
    /// Serial number -> when to try reconnecting it automatically next, and the wait after that.
    retry: Mutex<HashMap<String, (Instant, Duration)>>,
}

impl Shared {
    pub fn new(handle: AppHandle) -> Arc<Shared> {
        Arc::new(Shared {
            handle,
            log: Mutex::default(),
            adb: Mutex::new(Err(String::new())),
            timeout_secs: AtomicU64::new(120),
            session: Mutex::default(),
            devices: Mutex::default(),
            refresh_now: AtomicBool::new(false),
            busy: Mutex::default(),
            progress: Mutex::default(),
            switched: Mutex::default(),
            notice: Mutex::default(),
            restarting: AtomicBool::new(false),
            restart_notice: Mutex::default(),
            dirty: Mutex::new(false),
            dirty_cv: Condvar::new(),
            qr_shown: AtomicBool::new(false),
            history: Mutex::default(),
            history_path: Mutex::default(),
            paired: Mutex::default(),
            observed: Mutex::default(),
            finding: Mutex::default(),
            network: Mutex::default(),
            retry: Mutex::default(),
        })
    }

    /// Finds adb, starts the background threads and shows the first QR code.
    pub fn start(self: &Arc<Self>) {
        self.log(format!("startup: window created ({} ms)", crate::ms_since_launch()));
        self.load_history();
        spawn_emitter(self.clone());
        spawn_device_poller(self.clone());
        spawn_watcher(self.clone());
        let _ = self.set_adb(None);
        self.log(format!("startup: adb located ({} ms)", crate::ms_since_launch()));
        // Show a QR code right away; most of the time that's why the app was opened.
        self.start_pairing();
    }

    /// Tells the window to refetch the state.
    fn changed(&self) {
        *self.dirty.lock().unwrap() = true;
        self.dirty_cv.notify_one();
    }

    fn log(&self, line: String) {
        // Debug builds also print the log, so it can be followed from a terminal.
        #[cfg(debug_assertions)]
        eprintln!("[adbqr] {line}");
        let mut log = self.log.lock().unwrap();
        if log.lines.len() >= LOG_LINES {
            log.lines.pop_front();
        }
        log.lines.push_back(line);
        log.total += 1;
        drop(log);
        self.changed();
    }

    fn refresh(&self) {
        self.refresh_now.store(true, Ordering::Relaxed);
    }

    fn adb(&self) -> Option<Adb> {
        self.adb.lock().unwrap().as_ref().ok().cloned()
    }

    fn set_adb(self: &Arc<Self>, explicit: Option<PathBuf>) -> Result<(), String> {
        let shared = self.clone();
        let logger: Logger = Arc::new(move |line| shared.log(line));
        let found = adb::locate(explicit.as_deref()).map(|p| Adb::new(p, Some(logger)));
        let result = found.as_ref().map(|_| ()).map_err(Clone::clone);
        *self.adb.lock().unwrap() = found;
        *self.devices.lock().unwrap() = DeviceList::default();
        self.refresh();
        self.changed();
        result
    }

    fn start_pairing(self: &Arc<Self>) {
        let Some(adb) = self.adb() else { return };
        let shared = self.clone();
        let timeout = Duration::from_secs(self.timeout_secs.load(Ordering::Relaxed));
        let session = Session::start(adb, timeout, move || {
            shared.refresh();
            shared.changed();
        });
        *self.session.lock().unwrap() = Some(session);
        self.changed();
    }

    fn restart_server(self: &Arc<Self>) {
        let Some(adb) = self.adb() else { return };
        if self.restarting.swap(true, Ordering::Relaxed) {
            return;
        }
        *self.restart_notice.lock().unwrap() = None;
        self.changed();
        let shared = self.clone();
        thread::spawn(move || {
            let result = adb.restart_server();
            let ok = result.is_ok();
            *shared.restart_notice.lock().unwrap() = Some(match result {
                Ok(m) => (m, false),
                Err(e) => (e, true),
            });
            shared.restarting.store(false, Ordering::Relaxed);
            // The pairing session was talking to the dead server; start over
            // unless it already connected a phone.
            if ok {
                let stale = shared
                    .session
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|s| !matches!(s.progress.lock().unwrap().outcome, Some(Ok(_))));
                if stale {
                    shared.start_pairing();
                }
            }
            shared.refresh();
            shared.changed();
        });
    }

    /// Runs `action` on each target in the background, marking it busy until
    /// done, then shows the combined result as a notice.
    fn run_each(
        self: &Arc<Self>,
        targets: Vec<String>,
        action: impl Fn(&Adb, &str) -> Result<String, String> + Send + 'static,
    ) {
        let Some(adb) = self.adb() else { return };
        if targets.is_empty() {
            return;
        }
        *self.notice.lock().unwrap() = None;
        self.busy.lock().unwrap().extend(targets.iter().cloned());
        self.changed();
        let shared = self.clone();
        thread::spawn(move || {
            let mut messages = Vec::new();
            let mut failed = false;
            for target in targets {
                match action(&adb, &target) {
                    Ok(m) => messages.push(m),
                    Err(e) => {
                        failed = true;
                        messages.push(e);
                    }
                }
                shared.busy.lock().unwrap().remove(&target);
                shared.refresh();
                shared.changed();
            }
            *shared.notice.lock().unwrap() = Some((messages.join("\n"), failed));
            shared.changed();
        });
    }

    fn load_history(&self) {
        let path = self.handle.path().app_config_dir().ok().map(|d| d.join("devices.json"));
        if let Some(p) = &path {
            let store = Store::load(p);
            self.log(format!("history: {} known phone(s) in {}", store.devices.len(), p.display()));
            *self.history.lock().unwrap() = store;
        }
        *self.history_path.lock().unwrap() = path;
    }

    fn save_history(&self) {
        let Some(path) = self.history_path.lock().unwrap().clone() else { return };
        let result = self.history.lock().unwrap().save(&path);
        if let Err(e) = result {
            self.log(format!("history: could not save {}: {e}", path.display()));
        }
    }

    /// Adds phones from adb's pairing list to the history.
    fn sync_paired(&self) {
        let guids = history::paired_guids();
        let serials: HashSet<String> =
            guids.iter().filter_map(|g| adb::serial_of_instance(g)).map(str::to_string).collect();
        *self.paired.lock().unwrap() = serials;
        let added = self.history.lock().unwrap().add_paired(&guids);
        if added {
            self.save_history();
            self.changed();
        }
    }

    /// The serial number of a device by its adb serial, if known.
    fn serialno_of(&self, serial: &str) -> Option<String> {
        if let Some(i) = serial.strip_suffix(MDNS_SUFFIX) {
            return adb::serial_of_instance(i).map(str::to_string);
        }
        if let Some((_, sn)) = self.observed.lock().unwrap().get(serial) {
            if !sn.is_empty() {
                return Some(sn.clone());
            }
        }
        if adb::is_ip_port(serial) {
            return self.history.lock().unwrap().by_addr(serial).map(|k| k.serialno.clone());
        }
        Some(serial.to_string()) // USB serials are the serial number
    }

    /// Serial numbers of phones adb is connected to, or sees over mDNS.
    fn present(&self, list: &DeviceList) -> HashSet<String> {
        let mut out = HashSet::new();
        for d in &list.devices {
            if d.is_usb() || d.state == "device" {
                out.extend(self.serialno_of(&d.serial));
            }
        }
        for s in &list.available {
            out.extend(adb::serial_of_instance(&s.instance).map(str::to_string));
        }
        out
    }

    /// Records where the phones connected over Wi-Fi are, to find them again later.
    fn observe(&self, adb: &Adb, devices: &[Device], services: &[MdnsService]) {
        if !devices.iter().any(|d| d.is_wireless() && d.state == "device") {
            return;
        }
        // The watcher may not have looked yet, right after startup.
        if self.network.lock().unwrap().is_none() {
            self.check_network();
        }
        let network = self.network.lock().unwrap().as_ref().map(|n| n.id.clone());
        let mut arp: Option<Vec<_>> = None;
        let mut changed = false;
        for d in devices.iter().filter(|d| d.is_wireless() && d.state == "device") {
            let due = self.observed.lock().unwrap().get(&d.serial).is_none_or(|(t, _)| t.elapsed() > OBSERVE_EVERY);
            if !due {
                continue;
            }
            let info = match adb.phone_info(&d.serial) {
                Ok(i) if !i.serialno.is_empty() => i,
                _ => {
                    // Try again next round, not on every poll.
                    let sn = self.serialno_of(&d.serial).unwrap_or_default();
                    self.observed.lock().unwrap().insert(d.serial.clone(), (Instant::now(), sn));
                    continue;
                }
            };
            let mut guid = d.serial.strip_suffix(MDNS_SUFFIX).map(str::to_string);
            let (addr, tcpip) = if adb::is_ip_port(&d.serial) {
                if guid.is_none() {
                    guid = services.iter().find(|s| s.addr == d.serial).map(|s| s.instance.clone());
                }
                let port = d.serial.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok());
                (Some(d.serial.clone()), port != info.tls_port)
            } else {
                // Listed by mDNS name: its service's address, else its Wi-Fi IP and Wireless debugging port.
                let from_mdns = services
                    .iter()
                    .find(|s| Some(&s.instance) == guid.as_ref() && s.addr.parse::<SocketAddrV4>().is_ok())
                    .map(|s| s.addr.clone());
                let from_phone = match (&info.wlan_ip, info.tls_port) {
                    (Some(ip), Some(port)) => Some(format!("{ip}:{port}")),
                    _ => None,
                };
                (from_mdns.or(from_phone), false)
            };
            let mac = addr
                .as_deref()
                .and_then(|a| a.parse::<SocketAddrV4>().ok())
                .and_then(|a| net::mac_of(*a.ip(), arp.get_or_insert_with(net::arp_table)));
            self.observed.lock().unwrap().insert(d.serial.clone(), (Instant::now(), info.serialno.clone()));
            self.retry.lock().unwrap().remove(&info.serialno);
            let o = Observation {
                serialno: info.serialno,
                guid,
                model: info.model.or_else(|| d.model.clone()),
                addr,
                tcpip,
                mac,
                network: network.clone(),
            };
            self.history.lock().unwrap().observe(o, history::now());
            changed = true;
        }
        if changed {
            self.save_history();
            self.changed();
        }
    }

    /// Notices when this PC moves to another network.
    fn check_network(&self) {
        let now = net::current_network();
        let mut cur = self.network.lock().unwrap();
        if *cur != now {
            let text = now.as_ref().map_or("none".to_string(), |n| n.id.clone());
            *cur = now;
            drop(cur);
            self.log(format!("network: {text}"));
            // New network, new chances: try every phone again right away.
            self.retry.lock().unwrap().clear();
            self.changed();
        }
    }

    fn target(&self, serialno: &str) -> Option<Target> {
        let h = self.history.lock().unwrap();
        let k = h.get(serialno)?;
        Some(Target { serialno: k.serialno.clone(), guid: k.guid.clone(), addr: k.addr.clone(), mac: k.mac.clone() })
    }

    /// Quietly tries to reconnect phones last seen on this network that
    /// aren't connected, waiting longer between tries for ones not found.
    fn auto_reconnect(&self) {
        let Some(adb) = self.adb() else { return };
        let Some(network) = self.network.lock().unwrap().clone() else { return };
        let present = {
            let list = self.devices.lock().unwrap();
            if !list.loaded || list.error.is_some() {
                return;
            }
            self.present(&list)
        };
        let candidates: Vec<String> = {
            let h = self.history.lock().unwrap();
            if !h.auto_reconnect {
                return;
            }
            h.devices
                .iter()
                .filter(|k| {
                    k.network.as_deref() == Some(network.id.as_str())
                        && !k.paused
                        && k.addr.is_some()
                        && !present.contains(&k.serialno)
                })
                .map(|k| k.serialno.clone())
                .collect()
        };
        for serialno in candidates {
            let due = self.retry.lock().unwrap().get(&serialno).is_none_or(|(at, _)| Instant::now() >= *at);
            if !due || self.finding.lock().unwrap().contains_key(&serialno) {
                continue;
            }
            let Some(target) = self.target(&serialno) else { continue };
            self.finding.lock().unwrap().insert(serialno.clone(), "Reconnecting automatically...".into());
            self.changed();
            let result = finder::find(&adb, &target, false, network.local, &|_| {});
            self.finding.lock().unwrap().remove(&serialno);
            match result {
                Ok(f) => {
                    self.log(format!("auto-reconnect: connected {serialno} {} ({})", f.how, f.serial));
                    self.retry.lock().unwrap().remove(&serialno);
                }
                Err(_) => {
                    let mut retry = self.retry.lock().unwrap();
                    let wait = retry.get(&serialno).map_or(RETRY_FIRST, |(_, w)| (*w * 2).min(RETRY_MAX));
                    retry.insert(serialno, (Instant::now() + wait, wait));
                }
            }
            self.refresh();
            self.changed();
        }
    }

    /// Searches for a phone connected before, port scan included, and shows the result.
    fn find_device(self: &Arc<Self>, serialno: String) {
        let Some(adb) = self.adb() else { return };
        let Some(target) = self.target(&serialno) else { return };
        {
            let mut finding = self.finding.lock().unwrap();
            if finding.contains_key(&serialno) {
                return;
            }
            finding.insert(serialno.clone(), "Starting...".into());
        }
        *self.notice.lock().unwrap() = None;
        self.history.lock().unwrap().set_paused(&serialno, false);
        self.changed();
        let shared = self.clone();
        thread::spawn(move || {
            let local = shared.network.lock().unwrap().as_ref().and_then(|n| n.local);
            let status = |text: &str| {
                shared.finding.lock().unwrap().insert(serialno.clone(), text.to_string());
                shared.changed();
            };
            let result = finder::find(&adb, &target, true, local, &status);
            shared.finding.lock().unwrap().remove(&serialno);
            let model = shared.history.lock().unwrap().get(&serialno).and_then(|k| k.model.clone());
            let name = model.unwrap_or_else(|| serialno.clone());
            let notice = match result {
                Ok(f) => {
                    shared.retry.lock().unwrap().remove(&serialno);
                    (format!("Reconnected {name} {} ({})", f.how, f.serial), false)
                }
                Err(e) => (e, true),
            };
            *shared.notice.lock().unwrap() = Some(notice);
            shared.save_history();
            shared.refresh();
            shared.changed();
        });
    }

    fn snapshot(&self) -> Snapshot {
        let (adb_path, adb_error) = match &*self.adb.lock().unwrap() {
            Ok(adb) => (Some(adb.path().display().to_string()), None),
            Err(e) => (None, Some(e.clone()).filter(|e| !e.is_empty())),
        };

        let pairing = self.session.lock().unwrap().as_ref().map(|s| {
            let p = s.progress.lock().unwrap();
            let qr_svg = p.qr.as_ref().map(qr::to_svg);
            if qr_svg.is_some() && !self.qr_shown.swap(true, Ordering::Relaxed) {
                self.log(format!("startup: QR code ready ({} ms)", crate::ms_since_launch()));
            }
            PairingDto {
                qr_svg,
                attempt: p.attempt,
                max_attempts: MAX_ATTEMPTS,
                expires_in_ms: p.deadline.map(|d| d.saturating_duration_since(Instant::now()).as_millis() as u64),
                status: p.status.clone(),
                warning: p.warning.clone(),
                help: p.help,
                expired: p.expired,
                outcome: p.outcome.as_ref().map(|o| match o {
                    Ok(device) => OutcomeDto { ok: true, text: device.clone() },
                    Err(e) => OutcomeDto { ok: false, text: e.clone() },
                }),
            }
        });

        let busy = self.busy.lock().unwrap().clone();
        let progress = self.progress.lock().unwrap().clone();
        let switched = self.switched.lock().unwrap().clone();
        let list = self.devices.lock().unwrap();
        let (wireless, other): (Vec<&Device>, Vec<&Device>) = list.devices.iter().partition(|d| d.is_wireless());
        let (usb, other): (Vec<&Device>, Vec<&Device>) = other.into_iter().partition(|d| d.is_usb());
        let online_wireless: Vec<&str> =
            wireless.iter().filter(|d| d.state == "device").map(|d| d.serial.as_str()).collect();
        let devices = DevicesDto {
            loaded: list.loaded,
            error: list.error.clone(),
            wireless: wireless
                .into_iter()
                .map(|d| DeviceDto {
                    serial: d.serial.clone(),
                    model: d.model.clone(),
                    state: d.state.clone(),
                    mdns: adb::is_mdns_serial(&d.serial),
                    busy: busy.contains(&d.serial),
                })
                .collect(),
            available: list
                .available
                .iter()
                .map(|s| AvailableDto { name: s.instance.clone(), addr: s.addr.clone(), busy: busy.contains(&s.addr) })
                .collect(),
            usb: usb
                .into_iter()
                .map(|d| UsbDto {
                    serial: d.serial.clone(),
                    model: d.model.clone(),
                    state: d.state.clone(),
                    busy: busy.contains(&d.serial),
                    progress: progress.get(&d.serial).cloned(),
                    on_wifi: online_wireless.iter().any(|w| {
                        switched.get(&d.serial).is_some_and(|s| s == w)
                            || w.strip_suffix("._adb-tls-connect._tcp")
                                .is_some_and(|i| adb::instance_is_phone(i, &d.serial))
                    }),
                })
                .collect(),
            other_count: other.len(),
            notice: self.notice.lock().unwrap().as_ref().map(notice_dto),
            known: Vec::new(),
            auto_reconnect: false,
        };
        let server = ServerDto {
            hung: list.server_hung,
            restarting: self.restarting.load(Ordering::Relaxed),
            notice: self.restart_notice.lock().unwrap().as_ref().map(notice_dto),
        };
        let present = (list.loaded && list.error.is_none()).then(|| self.present(&list));
        drop(list);

        let network = self.network.lock().unwrap().as_ref().map(|n| n.id.clone());
        let finding = self.finding.lock().unwrap().clone();
        let paired = self.paired.lock().unwrap().clone();
        let h = self.history.lock().unwrap();
        let mut known: Vec<KnownDto> = h
            .devices
            .iter()
            .filter(|k| present.as_ref().is_some_and(|p| !p.contains(&k.serialno)))
            .map(|k| KnownDto {
                serialno: k.serialno.clone(),
                model: k.model.clone(),
                addr: k.addr.clone(),
                last_seen: k.last_seen,
                other_network: k.network.is_some() && network.is_some() && k.network != network,
                paired: paired.contains(&k.serialno),
                paused: k.paused,
                progress: finding.get(&k.serialno).cloned(),
            })
            .collect();
        known.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then_with(|| a.serialno.cmp(&b.serialno)));
        let devices = DevicesDto { known, auto_reconnect: h.auto_reconnect, ..devices };
        drop(h);

        Snapshot {
            os: std::env::consts::OS,
            adb_path,
            adb_error,
            timeout_secs: self.timeout_secs.load(Ordering::Relaxed),
            pairing,
            devices,
            server,
            log_total: self.log.lock().unwrap().total,
        }
    }
}

fn notice_dto((text, failed): &(String, bool)) -> NoticeDto {
    NoticeDto { text: text.clone(), error: *failed }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// `windows`, `linux`, `macos`, ...: the firewall help is Windows only.
    os: &'static str,
    adb_path: Option<String>,
    adb_error: Option<String>,
    timeout_secs: u64,
    pairing: Option<PairingDto>,
    devices: DevicesDto,
    server: ServerDto,
    log_total: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairingDto {
    qr_svg: Option<String>,
    attempt: u32,
    max_attempts: u32,
    /// Until the QR code expires; `None` while adb is still starting.
    expires_in_ms: Option<u64>,
    status: String,
    warning: Option<String>,
    /// Help offered with the warning or the failure: `firewall` or `restartServer`.
    help: Option<Help>,
    expired: bool,
    outcome: Option<OutcomeDto>,
}

#[derive(Serialize)]
struct OutcomeDto {
    ok: bool,
    text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DevicesDto {
    loaded: bool,
    error: Option<String>,
    wireless: Vec<DeviceDto>,
    available: Vec<AvailableDto>,
    usb: Vec<UsbDto>,
    /// Emulators, which aren't listed.
    other_count: usize,
    notice: Option<NoticeDto>,
    /// Phones connected before that adb isn't connected to and doesn't see now.
    known: Vec<KnownDto>,
    auto_reconnect: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KnownDto {
    serialno: String,
    model: Option<String>,
    addr: Option<String>,
    /// Unix seconds; 0 if adbqr never saw it connected.
    last_seen: u64,
    /// Last seen on a network other than this PC's.
    other_network: bool,
    /// In adb's own pairing list.
    paired: bool,
    /// Disconnected from adbqr, so not reconnected automatically.
    paused: bool,
    /// What a search for it is doing.
    progress: Option<String>,
}

#[derive(Serialize)]
struct DeviceDto {
    serial: String,
    model: Option<String>,
    state: String,
    /// Listed by mDNS name, so Disconnect turns off Wireless debugging.
    mdns: bool,
    busy: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UsbDto {
    serial: String,
    model: Option<String>,
    state: String,
    busy: bool,
    /// What a switch to Wi-Fi in progress is doing.
    progress: Option<String>,
    /// Also connected over Wi-Fi.
    on_wifi: bool,
}

#[derive(Serialize)]
struct AvailableDto {
    name: String,
    addr: String,
    busy: bool,
}

#[derive(Serialize)]
struct ServerDto {
    hung: bool,
    restarting: bool,
    notice: Option<NoticeDto>,
}

#[derive(Serialize)]
struct NoticeDto {
    text: String,
    error: bool,
}

#[derive(Serialize)]
pub struct LogChunk {
    lines: Vec<String>,
    total: u64,
}

type S<'a> = State<'a, Arc<Shared>>;

#[tauri::command]
pub fn get_state(shared: S) -> Snapshot {
    shared.snapshot()
}

/// Log lines after the first `from` ever logged (as many as are still kept).
#[tauri::command]
pub fn get_log(shared: S, from: u64) -> LogChunk {
    let log = shared.log.lock().unwrap();
    let first_kept = log.total - log.lines.len() as u64;
    let skip = from.saturating_sub(first_kept).min(log.lines.len() as u64) as usize;
    LogChunk { lines: log.lines.iter().skip(skip).cloned().collect(), total: log.total }
}

#[tauri::command]
pub fn clear_log(shared: S) {
    shared.log.lock().unwrap().lines.clear();
    shared.changed();
}

/// Uses the adb at `path` (the file or its folder), or finds it again if empty.
#[tauri::command]
pub fn set_adb(shared: S, path: String) -> Result<(), String> {
    let path = path.trim().trim_matches('"');
    let result = shared.set_adb((!path.is_empty()).then(|| PathBuf::from(path)));
    let has_session = shared.session.lock().unwrap().is_some();
    if result.is_ok() && !has_session {
        shared.start_pairing();
    }
    result
}

#[tauri::command]
pub fn start_pairing(shared: S, timeout_secs: Option<u64>) {
    if let Some(t) = timeout_secs {
        shared.timeout_secs.store(t.clamp(10, 900), Ordering::Relaxed);
    }
    shared.start_pairing();
}

#[tauri::command]
pub fn cancel_pairing(shared: S) {
    shared.session.lock().unwrap().take();
    shared.changed();
}

#[tauri::command]
pub fn disconnect(shared: S, serials: Vec<String>) {
    // Disconnected on purpose: don't reconnect it automatically.
    let mut paused = false;
    for serial in &serials {
        if let Some(sn) = shared.serialno_of(serial) {
            paused |= shared.history.lock().unwrap().set_paused(&sn, true);
        }
    }
    if paused {
        shared.save_history();
    }
    shared.run_each(serials, |adb, serial| {
        adb.disconnect(serial).map_err(|e| format!("Could not disconnect {serial}: {e}"))
    });
}

#[tauri::command]
pub fn reconnect(shared: S, addrs: Vec<String>) {
    shared.run_each(addrs, |adb, addr| adb.connect(addr).map_err(|e| format!("Could not reconnect {addr}: {e}")));
}

/// Moves USB-connected phones to Wi-Fi without a QR code.
#[tauri::command]
pub fn switch_to_wifi(shared: S, serials: Vec<String>) {
    let s = shared.inner().clone();
    shared.run_each(serials, move |adb, serial| {
        let report = |text: &str| {
            s.progress.lock().unwrap().insert(serial.to_string(), text.to_string());
            s.changed();
        };
        let result = adb.switch_to_wifi(serial, &report);
        s.progress.lock().unwrap().remove(serial);
        let (addr, msg) = result.map_err(|e| format!("Could not switch {serial} to Wi-Fi: {e}"))?;
        s.switched.lock().unwrap().insert(serial.to_string(), addr);
        Ok(msg)
    });
}

/// Looks for a phone connected before and reconnects it, without a QR code.
#[tauri::command]
pub fn find_device(shared: S, serialno: String) {
    shared.find_device(serialno);
}

/// Removes a phone from the list of phones connected before.
#[tauri::command]
pub fn forget_device(shared: S, serialno: String) {
    shared.history.lock().unwrap().forget(&serialno);
    shared.save_history();
    shared.changed();
}

#[tauri::command]
pub fn set_auto_reconnect(shared: S, on: bool) {
    shared.history.lock().unwrap().auto_reconnect = on;
    shared.retry.lock().unwrap().clear();
    shared.save_history();
    shared.changed();
}

#[tauri::command]
pub fn restart_server(shared: S) {
    shared.restart_server();
}

#[tauri::command]
pub fn refresh(shared: S) {
    shared.refresh();
}

/// Hides a notice: `"devices"` for disconnect/reconnect/switch results, `"server"` for restarts.
#[tauri::command]
pub fn dismiss_notice(shared: S, which: String) {
    match which.as_str() {
        "server" => *shared.restart_notice.lock().unwrap() = None,
        _ => *shared.notice.lock().unwrap() = None,
    }
    shared.changed();
}

/// Opens a Windows settings page: `"firewall"` or `"network"`.
#[tauri::command]
pub fn open_settings(page: String) -> Result<(), String> {
    firewall::open_settings(&page)
}

/// The networks Windows is connected to and whether each is Public or Private.
#[tauri::command]
pub async fn firewall_networks() -> Result<Vec<firewall::Network>, String> {
    tauri::async_runtime::spawn_blocking(firewall::networks).await.map_err(|e| e.to_string())?
}

/// Lets the current adb through Windows Defender Firewall (asks for admin rights).
#[tauri::command]
pub async fn fix_firewall(shared: S<'_>) -> Result<String, String> {
    let path = match &*shared.adb.lock().unwrap() {
        Ok(adb) => adb.path().to_path_buf(),
        Err(_) => return Err("adb not found, so there's nothing to allow yet.".into()),
    };
    let s = shared.inner().clone();
    s.log(format!("firewall: allowing {}", path.display()));
    let result = tauri::async_runtime::spawn_blocking(move || firewall::allow(&path))
        .await
        .map_err(|e| e.to_string())?;
    match &result {
        Ok(msg) => s.log(format!("firewall: {msg}")),
        Err(e) => s.log(format!("firewall: {e}")),
    }
    result
}

/// Sends `changed` to the window when something changed, at most every [`EMIT_INTERVAL`].
fn spawn_emitter(shared: Arc<Shared>) {
    thread::spawn(move || loop {
        {
            let mut dirty = shared.dirty.lock().unwrap();
            while !*dirty {
                dirty = shared.dirty_cv.wait(dirty).unwrap();
            }
            *dirty = false;
        }
        let _ = shared.handle.emit("changed", ());
        thread::sleep(EMIT_INTERVAL);
    });
}

/// Refreshes the device list every couple of seconds, or right away on request.
fn spawn_device_poller(shared: Arc<Shared>) {
    // The adb that has answered at least once. Until then the server may be
    // starting, which takes seconds, so a short timeout would wrongly call it hung.
    let mut answered: Option<PathBuf> = None;
    thread::spawn(move || loop {
        if let Some(adb) = shared.adb().map(|a| a.quiet()) {
            let warm = answered.as_deref() == Some(adb.path());
            let result = adb.devices_within(if warm { Duration::from_secs(5) } else { adb::COLD_START });
            if result.is_ok() && !warm {
                answered = Some(adb.path().to_path_buf());
                shared.log(format!("startup: device list loaded ({} ms)", crate::ms_since_launch()));
            }
            // No point asking a server that just failed to answer.
            let services = if result.is_ok() { adb.mdns_services().unwrap_or_default() } else { Vec::new() };
            if let Ok(devices) = &result {
                shared.observe(&adb, devices, &services);
            }
            let mut list = shared.devices.lock().unwrap();
            match result {
                Ok(devices) => {
                    list.available = adb::reconnectable(&services, &devices);
                    list.devices = devices;
                    list.error = None;
                    list.server_hung = false;
                }
                Err(e) => {
                    list.server_hung = adb::is_timeout(&e);
                    list.error = Some(if list.server_hung { "The adb server isn't responding.".into() } else { e });
                }
            }
            list.loaded = true;
            drop(list);
            shared.changed();
        }
        let start = Instant::now();
        while start.elapsed() < DEVICE_POLL && !shared.refresh_now.swap(false, Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
        }
    });
}

/// Every few seconds: notices network changes, picks up newly paired phones
/// and tries to reconnect phones that dropped off.
fn spawn_watcher(shared: Arc<Shared>) {
    thread::spawn(move || loop {
        shared.check_network();
        shared.sync_paired();
        shared.auto_reconnect();
        thread::sleep(WATCH_EVERY);
    });
}
