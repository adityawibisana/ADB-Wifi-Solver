//! App state shared by the window and the background threads, and the
//! commands the window calls. The window renders from [`Shared::snapshot`]
//! and refetches it whenever a `changed` event arrives.

use crate::adb::{self, Adb, Device, Logger, MdnsService};
use crate::pairing::{Session, MAX_ATTEMPTS};
use crate::qr;
use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};

const DEVICE_POLL: Duration = Duration::from_secs(2);
const LOG_LINES: usize = 1000;
/// At most one `changed` event per this interval; the window refetches on each.
const EMIT_INTERVAL: Duration = Duration::from_millis(40);

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
    /// Result of the last disconnect or reconnect.
    notice: Mutex<Notice>,
    restarting: AtomicBool,
    /// Result of the last server restart.
    restart_notice: Mutex<Notice>,
    dirty: Mutex<bool>,
    dirty_cv: Condvar,
    qr_shown: AtomicBool,
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
            notice: Mutex::default(),
            restarting: AtomicBool::new(false),
            restart_notice: Mutex::default(),
            dirty: Mutex::new(false),
            dirty_cv: Condvar::new(),
            qr_shown: AtomicBool::new(false),
        })
    }

    /// Finds adb, starts the background threads and shows the first QR code.
    pub fn start(self: &Arc<Self>) {
        self.log(format!("startup: window created ({} ms)", crate::ms_since_launch()));
        spawn_emitter(self.clone());
        spawn_device_poller(self.clone());
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
                expired: p.expired,
                outcome: p.outcome.as_ref().map(|o| match o {
                    Ok(device) => OutcomeDto { ok: true, text: device.clone() },
                    Err(e) => OutcomeDto { ok: false, text: e.clone() },
                }),
            }
        });

        let busy = self.busy.lock().unwrap().clone();
        let list = self.devices.lock().unwrap();
        let (wireless, other): (Vec<&Device>, Vec<&Device>) = list.devices.iter().partition(|d| d.is_wireless());
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
            other_count: other.len(),
            notice: self.notice.lock().unwrap().as_ref().map(notice_dto),
        };
        let server = ServerDto {
            hung: list.server_hung,
            restarting: self.restarting.load(Ordering::Relaxed),
            notice: self.restart_notice.lock().unwrap().as_ref().map(notice_dto),
        };
        drop(list);

        Snapshot {
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
    other_count: usize,
    notice: Option<NoticeDto>,
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
    shared.run_each(serials, |adb, serial| {
        adb.disconnect(serial).map_err(|e| format!("Could not disconnect {serial}: {e}"))
    });
}

#[tauri::command]
pub fn reconnect(shared: S, addrs: Vec<String>) {
    shared.run_each(addrs, |adb, addr| adb.connect(addr).map_err(|e| format!("Could not reconnect {addr}: {e}")));
}

#[tauri::command]
pub fn restart_server(shared: S) {
    shared.restart_server();
}

#[tauri::command]
pub fn refresh(shared: S) {
    shared.refresh();
}

/// Hides a notice: `"devices"` for disconnect/reconnect results, `"server"` for restarts.
#[tauri::command]
pub fn dismiss_notice(shared: S, which: String) {
    match which.as_str() {
        "server" => *shared.restart_notice.lock().unwrap() = None,
        _ => *shared.notice.lock().unwrap() = None,
    }
    shared.changed();
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
