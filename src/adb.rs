use std::env;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

/// How long a command may take when it has to start the adb server first.
/// Starting one takes a few seconds; a running one answers at once.
pub const COLD_START: Duration = Duration::from_secs(15);

/// Receives each adb command line and its output, for the log view.
pub type Logger = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone)]
pub struct Adb {
    path: PathBuf,
    log: Option<Logger>,
}

pub struct Output {
    pub success: bool,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MdnsService {
    pub instance: String,
    pub service_type: String,
    pub addr: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub serial: String,
    pub state: String,
    /// From `adb devices -l`, with underscores turned back into spaces.
    pub model: Option<String>,
}

impl Device {
    /// Connected over Wi-Fi: an `ip:port` serial or an mDNS service name.
    pub fn is_wireless(&self) -> bool {
        is_mdns_serial(&self.serial) || is_ip_port(&self.serial)
    }

    /// Connected over USB: anything that isn't Wi-Fi or an emulator.
    pub fn is_usb(&self) -> bool {
        !self.is_wireless() && !self.serial.starts_with("emulator-")
    }
}

/// Port `adb tcpip` makes the phone listen on.
const TCPIP_PORT: u16 = 5555;
/// How long to wait for Wireless debugging to come up, which can include the
/// phone asking "Allow wireless debugging on this network?".
const WIRELESS_DEBUGGING_WAIT: Duration = Duration::from_secs(30);

/// Whether an mDNS instance name (`adb-<serialno>-<random>`) belongs to the phone with that serial number.
pub fn instance_is_phone(instance: &str, serialno: &str) -> bool {
    instance
        .strip_prefix("adb-")
        .and_then(|r| r.strip_prefix(serialno))
        .is_some_and(|r| r.starts_with('-'))
}

/// The phone's Wi-Fi IPv4 address from `ip -o -4 addr show`.
pub fn parse_wlan_ip(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let _index = parts.next()?;
        let iface = parts.next()?;
        if !iface.starts_with("wlan") || parts.next()? != "inet" {
            return None;
        }
        let ip = parts.next()?.split('/').next()?;
        ip.parse::<std::net::Ipv4Addr>().ok().map(|_| ip.to_string())
    })
}

/// The serial number in an mDNS instance name: `adb-R5CT1234-AbCdEf` -> `R5CT1234`.
pub fn serial_of_instance(instance: &str) -> Option<&str> {
    let rest = instance.strip_prefix("adb-")?;
    let (serial, random) = rest.rsplit_once('-')?;
    (!serial.is_empty() && !random.is_empty()).then_some(serial)
}

/// What a phone says about itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PhoneInfo {
    pub serialno: String,
    /// `ro.product.model`, often a code like `V2413` or `SM-A075F`.
    pub model: Option<String>,
    /// Its name as sold, like `vivo X200 Pro`, when the phone says.
    pub name: Option<String>,
    pub brand: Option<String>,
    /// Android version, like `16`.
    pub android: Option<String>,
    /// Wireless debugging's port, while it is on.
    pub tls_port: Option<u16>,
    pub wlan_ip: Option<String>,
}

/// Properties some makers keep the name a phone is sold as in, best first.
const NAME_PROPS: &[&str] = &[
    "ro.product.marketname",        // Xiaomi, Redmi, POCO
    "ro.product.vendor.marketname", // Xiaomi, older
    "ro.vendor.oplus.market.name",  // OPPO, OnePlus, realme
    "ro.oppo.market.name",          // OPPO, older
    "ro.config.marketing_name",     // Honor, Huawei
    "ro.vivo.market.name",          // vivo, older
    "ro.product.odm.marketname",
];

/// Settings that hold the phone's name: the maker's own first, then names the
/// user can change, which default to the name it's sold as on many phones.
const NAME_SETTINGS: &[(&str, &str)] =
    &[("global", "vivo_device_name"), ("global", "device_name"), ("secure", "bluetooth_name")];

/// One shell command that prints everything [`parse_phone_info`] reads, as
/// `key=value` lines, plus `ip -o -4 addr show`.
fn info_command() -> String {
    let mut cmd = String::from(
        "echo sn=$(getprop ro.serialno); echo port=$(getprop service.adb.tls.port); \
         echo model=$(getprop ro.product.model); echo brand=$(getprop ro.product.brand); \
         echo android=$(getprop ro.build.version.release); ",
    );
    for p in NAME_PROPS {
        cmd += &format!("echo {p}=$(getprop {p}); ");
    }
    for (ns, key) in NAME_SETTINGS {
        cmd += &format!("echo {ns}.{key}=$(settings get {ns} {key} 2>/dev/null); ");
    }
    cmd + "ip -o -4 addr show"
}

pub fn parse_phone_info(text: &str) -> PhoneInfo {
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().to_string())
            // `settings get` prints `null` for a setting that isn't there.
            .filter(|v| !v.is_empty() && v != "null")
    };
    let model = field("model");
    let brand = field("brand");
    let sold_as = NAME_PROPS.iter().chain(&["global.vivo_device_name"]).find_map(|p| field(p));
    let user_set = NAME_SETTINGS[1..].iter().find_map(|(ns, key)| field(&format!("{ns}.{key}")));
    PhoneInfo {
        serialno: field("sn").unwrap_or_default(),
        name: phone_name(brand.as_deref(), model.as_deref(), sold_as.as_deref(), user_set.as_deref()),
        model,
        brand,
        android: field("android"),
        tls_port: field("port").and_then(|p| p.parse().ok()).filter(|&p| p != 0),
        wlan_ip: parse_wlan_ip(text),
    }
}

/// The name to show for a phone, or None if it says nothing better than its model.
///
/// `sold_as` comes from the maker and is a product name, so it gets the brand
/// in front when it lacks it (`X200 Pro` -> `vivo X200 Pro`). `user_set` is
/// `device_name` or `bluetooth_name`, which the user may have renamed, so it
/// is shown as is.
pub fn phone_name(brand: Option<&str>, model: Option<&str>, sold_as: Option<&str>, user_set: Option<&str>) -> Option<String> {
    let model = model.unwrap_or("");
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    // Not a name: the model again, with or without the brand, or a generic default.
    let useless = |n: &str| {
        n.trim().is_empty()
            || same(n, model)
            || brand.is_some_and(|b| same(n, &format!("{b} {model}")))
            || same(n, "Android")
            || same(n, "unknown")
    };
    if let Some(n) = sold_as.filter(|n| !useless(n)) {
        let n = n.trim();
        return Some(match brand.map(brand_label) {
            Some(b) if !b.is_empty() && !n.to_lowercase().starts_with(&b.to_lowercase()) => format!("{b} {n}"),
            _ => n.to_string(),
        });
    }
    user_set.filter(|n| !useless(n)).map(|n| n.trim().to_string())
}

/// A brand as its maker writes it: `samsung` -> `Samsung`, `oneplus` -> `OnePlus`.
pub fn brand_label(brand: &str) -> String {
    const KNOWN: &[&str] = &[
        "Samsung", "Google", "Xiaomi", "Redmi", "POCO", "OnePlus", "OPPO", "realme", "vivo", "iQOO", "HONOR",
        "HUAWEI", "Motorola", "Nothing", "Sony", "ASUS", "Nokia", "Lenovo", "ZTE", "nubia", "TECNO", "Infinix",
        "itel", "Fairphone", "LG", "HTC", "Sharp", "Meizu", "Blackview", "Ulefone", "Doogee", "OUKITEL",
    ];
    let b = brand.trim();
    if let Some(k) = KNOWN.iter().find(|k| k.eq_ignore_ascii_case(b)) {
        return k.to_string();
    }
    if b.chars().all(|c| !c.is_uppercase()) {
        let mut c = b.chars();
        return c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default();
    }
    b.to_string()
}

/// Whether `adb connect` output means it connected.
pub fn connect_succeeded(text: &str) -> bool {
    let t = text.trim();
    t.starts_with("connected to") || t.starts_with("already connected to")
}

/// Serials adb gives devices it connected to through mDNS.
pub fn is_mdns_serial(serial: &str) -> bool {
    serial.contains("._adb-tls-connect._tcp")
}

pub fn is_ip_port(serial: &str) -> bool {
    serial.rsplit_once(':').is_some_and(|(host, port)| {
        port.parse::<u16>().is_ok()
            && host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_ok()
    })
}

/// The adb executable's file name on this platform (`adb.exe` on Windows).
fn adb_file_name() -> String {
    format!("adb{}", env::consts::EXE_SUFFIX)
}

/// Finds adb: an explicit path, then PATH, then the usual SDK locations.
pub fn locate(explicit: Option<&Path>) -> Result<PathBuf, String> {
    let exe = adb_file_name();
    if let Some(p) = explicit {
        for candidate in [p.to_path_buf(), p.join(&exe)] {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        return Err(format!("adb not found at {}", p.display()));
    }

    let mut sdk_roots = Vec::new();
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(v) = env::var_os(var) {
            sdk_roots.push(PathBuf::from(v));
        }
    }
    if let Some(v) = env::var_os("LOCALAPPDATA") {
        sdk_roots.push(PathBuf::from(v).join("Android").join("Sdk"));
    }
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        sdk_roots.push(home.join("Library").join("Android").join("sdk")); // macOS
        sdk_roots.push(home.join("Android").join("Sdk")); // Linux
    }

    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).map(|d| d.join(&exe)));
    }
    candidates.extend(sdk_roots.into_iter().map(|r| r.join("platform-tools").join(&exe)));

    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        "adb not found. Install Android SDK platform-tools and add it to PATH, \
         set ANDROID_HOME, or enter the path to adb."
            .to_string()
    })
}

impl Adb {
    pub fn new(path: PathBuf, log: Option<Logger>) -> Self {
        Adb { path, log }
    }

    /// The same adb without logging, for background polling.
    pub fn quiet(&self) -> Self {
        Adb { path: self.path.clone(), log: None }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs adb with a timeout and returns its combined stdout and stderr.
    pub fn run(&self, args: &[&str], timeout: Duration) -> Result<Output, String> {
        if let Some(log) = &self.log {
            log(format!("> adb {}", args.join(" ")));
        }
        let mut command = Command::new(&self.path);
        hide_console(&mut command);
        let mut child = command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", self.path.display()))?;

        // Read the pipes on threads so a full pipe cannot block the child. The
        // results come back over a channel with a bounded wait, because a
        // freshly spawned adb server can keep inherited pipe handles open.
        let (tx, rx) = mpsc::channel();
        for mut pipe in [
            child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
            child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
            });
        }
        drop(tx);

        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if start.elapsed() > timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("adb {} timed out", args.join(" ")));
                }
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(format!("adb {} failed: {e}", args.join(" "))),
            }
        };

        let mut text = String::new();
        while let Ok(part) = rx.recv_timeout(Duration::from_secs(1)) {
            text.push_str(&part);
        }
        if let Some(log) = &self.log {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                log(format!("  {line}"));
            }
            log(format!("  ({} ms)", start.elapsed().as_millis()));
        }
        Ok(Output { success: status.success(), text })
    }

    pub fn note(&self, line: String) {
        if let Some(log) = &self.log {
            log(line);
        }
    }

    // Both answer instantly from a healthy server, so a short timeout spots a
    // hung one quickly.
    pub fn mdns_services(&self) -> Result<Vec<MdnsService>, String> {
        Ok(parse_mdns_services(&self.run(&["mdns", "services"], Duration::from_secs(5))?.text))
    }

    pub fn devices(&self) -> Result<Vec<Device>, String> {
        self.devices_within(Duration::from_secs(5))
    }

    pub fn devices_within(&self, timeout: Duration) -> Result<Vec<Device>, String> {
        Ok(parse_devices(&self.run(&["devices", "-l"], timeout)?.text))
    }

    /// Restarts a hung adb server. Tries `adb kill-server` first; a hung
    /// server usually ignores that too, so then it kills the adb process
    /// listening on the server port. Other programs using adb (Android
    /// Studio, scrcpy) lose their connections and reconnect on their own.
    ///
    /// Success is judged by whether a server answers, not by whether the port
    /// is free: any adb call (this app's own polling included) starts a fresh
    /// server as soon as the old one is gone, and that one must not be killed.
    pub fn restart_server(&self) -> Result<String, String> {
        let _ = self.run(&["kill-server"], Duration::from_secs(5));
        // Long enough for this call to cold-start a server if kill-server worked.
        if self.run(&["devices"], Duration::from_secs(10)).is_err() {
            let port = server_port();
            let pid = listener_pid(port).ok_or_else(|| {
                format!("adb kill-server did not stop the server, and no process listening on port {port} was found")
            })?;
            self.note(format!("> kill adb server process {pid}"));
            kill_adb_process(pid)?;
            thread::sleep(Duration::from_millis(500));
        }
        let out = self.run(&["start-server"], Duration::from_secs(20))?;
        if !out.success {
            return Err(format!("adb start-server failed: {}", out.text.trim()));
        }
        self.run(&["devices"], Duration::from_secs(5))
            .map_err(|e| format!("Restarted adb, but it still isn't responding: {e}"))?;
        Ok("Restarted the adb server".into())
    }

    /// Connects to a phone's `ip:port` connection service. Works without a QR
    /// code only if the phone was paired with this PC before.
    pub fn connect(&self, addr: &str) -> Result<String, String> {
        let out = self.run(&["connect", addr], Duration::from_secs(15))?;
        let text = out.text.trim();
        if text.contains("connected to") && !text.contains("failed") {
            Ok(format!("Reconnected {addr}"))
        } else if text.contains("authenticat") || text.contains("protocol fault") {
            Err(format!(
                "{addr} refused the connection. It is probably not paired with this PC; pair it with the QR code. ({text})"
            ))
        } else {
            Err(text.to_string())
        }
    }

    /// Runs a shell command on a device and returns its trimmed output.
    pub fn shell(&self, serial: &str, command: &str) -> Result<String, String> {
        let out = self.run(&["-s", serial, "shell", command], Duration::from_secs(10))?;
        if out.success {
            Ok(out.text.trim().to_string())
        } else {
            Err(out.text.trim().to_string())
        }
    }

    pub fn phone_info(&self, serial: &str) -> Result<PhoneInfo, String> {
        Ok(parse_phone_info(&self.shell(serial, &info_command())?))
    }

    /// Moves a USB-connected phone to Wi-Fi without a QR code, since it
    /// already trusts this PC over USB. Returns the Wi-Fi address and a
    /// message saying what was done.
    ///
    /// On Android 11+ it turns on Wireless debugging and connects to its TLS
    /// port. If that doesn't work it falls back to `adb tcpip`, which works
    /// on any Android version but leaves the phone listening on port 5555
    /// until it restarts. The USB connection keeps working either way.
    pub fn switch_to_wifi(&self, serial: &str, status: &dyn Fn(&str)) -> Result<(String, String), String> {
        status("Reading the phone's Wi-Fi address...");
        let ip = parse_wlan_ip(&self.shell(serial, "ip -o -4 addr show")?)
            .ok_or("The phone isn't connected to Wi-Fi. Connect it to the same network as this PC.")?;
        let sdk: u32 = self.shell(serial, "getprop ro.build.version.sdk")?.parse().unwrap_or(0);

        let why = if sdk >= 30 {
            match self.switch_by_wireless_debugging(serial, &ip, status) {
                Ok(addr) => {
                    let msg = format!(
                        "Switched {serial} to Wi-Fi ({addr}) with Wireless debugging. You can unplug the cable."
                    );
                    return Ok((addr, msg));
                }
                Err(e) => e,
            }
        } else {
            "it needs Android 11 or newer".to_string()
        };
        self.note(format!("  Wireless debugging didn't work ({why}); using adb tcpip"));
        status("Using adb tcpip instead...");
        let addr = self.switch_by_tcpip(serial, &ip)?;
        let msg = format!(
            "Switched {serial} to Wi-Fi ({addr}) with adb tcpip, because Wireless debugging didn't work: {why}. \
             You can unplug the cable. The phone accepts adb connections on port {TCPIP_PORT} until it restarts."
        );
        Ok((addr, msg))
    }

    /// Turns on Wireless debugging and connects to it. Turns it back off on
    /// failure if it was off before.
    fn switch_by_wireless_debugging(&self, serial: &str, ip: &str, status: &dyn Fn(&str)) -> Result<String, String> {
        // The mDNS name uses the phone's serial number, which is usually the USB serial too.
        let serialno = self.shell(serial, "getprop ro.serialno").ok().filter(|s| !s.is_empty());
        let serialno = serialno.as_deref().unwrap_or(serial);
        let was_on = self.shell(serial, "settings get global adb_wifi_enabled")? == "1";
        if !was_on {
            status("Turning on Wireless debugging...");
            self.shell(serial, "settings put global adb_wifi_enabled 1")?;
        }
        let result = self.connect_wireless_debugging(serialno, ip, status);
        if result.is_err() && !was_on {
            let _ = self.shell(serial, "settings put global adb_wifi_enabled 0");
        }
        result
    }

    /// Waits for the phone's Wireless debugging service to show up over mDNS
    /// and connects to it, unless adb connects to it on its own first (it
    /// does for phones paired with this PC).
    fn connect_wireless_debugging(&self, serialno: &str, ip: &str, status: &dyn Fn(&str)) -> Result<String, String> {
        let quiet = self.quiet();
        let start = Instant::now();
        let mut tried: Option<String> = None;
        while start.elapsed() < WIRELESS_DEBUGGING_WAIT {
            if let Ok(devices) = quiet.devices() {
                let found = devices.iter().find(|d| {
                    // Only the Wireless debugging connection: by its mDNS name, or
                    // the address connected to below. Not, say, an old tcpip one.
                    d.state == "device"
                        && (tried.as_deref() == Some(d.serial.as_str())
                            || d.serial.strip_suffix("._adb-tls-connect._tcp").is_some_and(|i| instance_is_phone(i, serialno)))
                });
                if let Some(d) = found {
                    return Ok(d.serial.clone());
                }
            }
            let services = quiet.mdns_services().unwrap_or_default();
            let mut ours = services
                .iter()
                .filter(|s| s.service_type.contains("_adb-tls-connect") && instance_is_phone(&s.instance, serialno));
            let service = ours.clone().find(|s| host_of(&s.addr) == ip).or_else(|| ours.next());
            match service {
                Some(s) if tried.as_deref() != Some(&s.addr) => {
                    status("Connecting...");
                    let out = self.run(&["connect", &s.addr], Duration::from_secs(15))?;
                    let text = out.text.trim();
                    if !connect_succeeded(text) {
                        return Err(format!("adb connect {} said: {text}", s.addr));
                    }
                    // Then wait for it to show up as `device` above.
                    tried = Some(s.addr.clone());
                }
                Some(_) => {}
                None if start.elapsed() > Duration::from_secs(3) => {
                    status("Waiting for Wireless debugging. If the phone asks, tap Allow.");
                }
                None => {}
            }
            thread::sleep(Duration::from_millis(500));
        }
        Err(match tried {
            Some(addr) => format!("connected to {addr}, but the phone didn't come online"),
            None => "it didn't come up within 30 seconds (the phone may still be asking to allow it on this network, \
                     or mDNS is blocked on this PC)"
                .to_string(),
        })
    }

    /// `adb tcpip` then `adb connect ip:5555`.
    fn switch_by_tcpip(&self, serial: &str, ip: &str) -> Result<String, String> {
        let port = TCPIP_PORT.to_string();
        let out = self.run(&["-s", serial, "tcpip", &port], Duration::from_secs(15))?;
        if !out.text.contains("restarting in TCP mode") {
            return Err(format!("adb tcpip failed: {}", out.text.trim()));
        }
        let addr = format!("{ip}:{port}");
        let start = Instant::now();
        let mut last = String::new();
        // adbd restarts first, so the first tries may be refused.
        while start.elapsed() < Duration::from_secs(15) {
            thread::sleep(Duration::from_secs(1));
            let out = self.run(&["connect", &addr], Duration::from_secs(10))?;
            last = out.text.trim().to_string();
            if !connect_succeeded(&last) {
                continue;
            }
            let wait = Instant::now();
            while wait.elapsed() < Duration::from_secs(5) {
                match self.quiet().devices()?.iter().find(|d| d.serial == addr).map(|d| d.state.as_str()) {
                    Some("device") => return Ok(addr),
                    Some("unauthorized") => return Err(format!("{addr} is waiting for you to allow USB debugging on the phone")),
                    _ => thread::sleep(Duration::from_millis(300)),
                }
            }
            return Err(format!("connected to {addr}, but the phone didn't come online"));
        }
        Err(format!("could not connect to {addr} ({last}). Is this PC on the same network as the phone?"))
    }

    /// Ends a Wi-Fi connection and returns a message describing what was done.
    ///
    /// `adb disconnect` only accepts `ip:port`, and the adb server reconnects
    /// paired phones it sees over mDNS by itself. So for mDNS serials this
    /// turns off Wireless debugging on the phone instead, which drops the
    /// connection and keeps it dropped until it is turned back on.
    pub fn disconnect(&self, serial: &str) -> Result<String, String> {
        if !is_mdns_serial(serial) {
            let out = self.run(&["disconnect", serial], Duration::from_secs(10))?;
            return if out.success && out.text.contains("disconnected") {
                Ok(format!("Disconnected {serial}"))
            } else {
                Err(out.text.trim().to_string())
            };
        }

        // Turning Wireless debugging off drops the very connection the shell
        // command runs over, and a command still running at that moment can
        // hang the adb server. So the phone runs it in the background a
        // second later, and the shell returns right away.
        let result = self.run(
            &[
                "-s",
                serial,
                "shell",
                "nohup sh -c 'sleep 1; settings put global adb_wifi_enabled 0' >/dev/null 2>&1 &",
            ],
            Duration::from_secs(10),
        );
        // Judge by whether the device actually goes away.
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(6) {
            match self.quiet().devices() {
                Ok(devices) if !devices.iter().any(|d| d.serial == serial) => {
                    return Ok(format!("Turned off Wireless debugging on {serial}"));
                }
                _ => thread::sleep(Duration::from_millis(300)),
            }
        }
        match result {
            Ok(out) if out.success => Ok(format!(
                "Turned off Wireless debugging on {serial}; adb may list it for a few more seconds"
            )),
            Ok(out) => Err(out.text.trim().to_string()),
            Err(e) => Err(e),
        }
    }
}

/// Whether an error from [`Adb::run`] is a timeout. A timeout on a command
/// that should answer instantly means the adb server is hung.
pub fn is_timeout(err: &str) -> bool {
    err.ends_with(" timed out")
}

/// Don't flash a console window for every command on Windows.
pub(crate) fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// Runs a helper program (netstat, taskkill, ...) and returns its stdout.
pub fn capture(program: &str, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(program);
    hide_console(&mut command);
    let out = command
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run {program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("{program} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn server_port() -> u16 {
    env::var("ADB_SERVER_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5037)
}

/// The process listening on a local TCP port.
fn listener_pid(port: u16) -> Option<u32> {
    if cfg!(windows) {
        parse_netstat_listener(&capture("netstat", &["-ano", "-p", "TCP"]).ok()?, port)
    } else {
        let out = capture("lsof", &["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"]).ok()?;
        out.lines().next()?.trim().parse().ok()
    }
}

/// Finds the listener on `port` in `netstat -ano` output. Listening sockets
/// have a foreign port of 0; matching on that avoids the state column, whose
/// text ("LISTENING") is translated on non-English Windows.
fn parse_netstat_listener(text: &str, port: u16) -> Option<u32> {
    let suffix = format!(":{port}");
    text.lines().find_map(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() == 5
            && parts[0].eq_ignore_ascii_case("TCP")
            && parts[1].ends_with(&suffix)
            && parts[2].ends_with(":0")
        {
            parts[4].parse().ok()
        } else {
            None
        }
    })
}

/// Force-kills `pid`, but only if it is adb, so another program on the port is left alone.
fn kill_adb_process(pid: u32) -> Result<(), String> {
    let pid_s = pid.to_string();
    if cfg!(windows) {
        let name = capture("tasklist", &["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])?;
        if !name.trim_start().to_ascii_lowercase().starts_with("\"adb") {
            return Err(format!("PID {pid} on the adb port is not adb: {}", name.trim()));
        }
        capture("taskkill", &["/F", "/PID", &pid_s]).map(|_| ())
    } else {
        let name = capture("ps", &["-p", &pid_s, "-o", "comm="])?;
        if !name.trim().rsplit('/').next().unwrap_or("").starts_with("adb") {
            return Err(format!("PID {pid} on the adb port is not adb: {}", name.trim()));
        }
        capture("kill", &["-9", &pid_s]).map(|_| ())
    }
}

/// Parses `adb mdns services` lines: `<instance>\t<type>\t<ip:port>`.
pub fn parse_mdns_services(text: &str) -> Vec<MdnsService> {
    text.lines()
        .filter(|l| !l.starts_with("List of") && !l.trim().is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = if line.contains('\t') {
                line.split('\t').map(str::trim).filter(|s| !s.is_empty()).collect()
            } else {
                line.split_whitespace().collect()
            };
            if parts.len() < 3 {
                return None;
            }
            Some(MdnsService {
                instance: parts[0].to_string(),
                service_type: parts[1].to_string(),
                addr: parts[parts.len() - 1].to_string(),
            })
        })
        .collect()
}

/// Phones advertising Wireless debugging that adb isn't connected to, one per
/// phone. adb only lets them connect if they were paired with this PC before.
pub fn reconnectable(services: &[MdnsService], devices: &[Device]) -> Vec<MdnsService> {
    let mut out: Vec<MdnsService> = Vec::new();
    for s in services.iter().filter(|s| s.service_type.contains("_adb-tls-connect")) {
        let connected = devices.iter().any(|d| {
            d.serial == s.addr || d.serial.strip_prefix(s.instance.as_str()).is_some_and(|r| r.starts_with('.'))
        });
        if !connected && !out.iter().any(|o| o.instance == s.instance) {
            out.push(s.clone());
        }
    }
    out
}

/// Parses `adb devices [-l]` output: serial, state, then optional `key:value` pairs.
pub fn parse_devices(text: &str) -> Vec<Device> {
    text.lines()
        .filter(|l| !l.starts_with("List of") && !l.starts_with('*'))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?.to_string();
            let state = parts.next()?.to_string();
            let model = parts.find_map(|p| p.strip_prefix("model:")).map(|m| m.replace('_', " "));
            Some(Device { serial, state, model })
        })
        .collect()
}

pub fn pair_succeeded(text: &str) -> bool {
    text.contains("Successfully paired")
}

/// Extracts the device guid from `Successfully paired to ... [guid=adb-XXXX-yyyy]`.
pub fn pair_guid(text: &str) -> Option<String> {
    let start = text.find("[guid=")? + "[guid=".len();
    let end = text[start..].find(']')? + start;
    Some(text[start..end].to_string()).filter(|g| !g.is_empty())
}

/// The host part of `ip:port`.
pub fn host_of(addr: &str) -> &str {
    addr.rsplit_once(':').map_or(addr, |(h, _)| h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mdns_services() {
        let text = "List of discovered mdns services\n\
                    adb-R5CT1234-AbCdEf\t_adb-tls-connect._tcp\t192.168.1.20:37891\n\
                    adbqr-abcd1234\t_adb-tls-pairing._tcp\t192.168.1.20:41234\n";
        let s = parse_mdns_services(text);
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].instance, "adbqr-abcd1234");
        assert_eq!(s[1].service_type, "_adb-tls-pairing._tcp");
        assert_eq!(s[1].addr, "192.168.1.20:41234");
    }

    #[test]
    fn parses_empty_mdns_list() {
        assert!(parse_mdns_services("List of discovered mdns services\n").is_empty());
    }

    #[test]
    fn parses_devices() {
        let text = "* daemon started successfully\nList of devices attached\n\
                    adb-R5CT1234-AbCdEf._adb-tls-connect._tcp\tdevice\n\
                    192.168.1.20:37891\toffline\n\n";
        let d = parse_devices(text);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].state, "device");
        assert_eq!(d[0].model, None);
        assert_eq!(d[1].serial, "192.168.1.20:37891");
    }

    #[test]
    fn parses_long_devices() {
        let text = "List of devices attached\n\
                    adb-R9RYC00849A-ZxcWiN._adb-tls-connect._tcp device product:a07xx model:SM_A075F device:a07 transport_id:672\n\
                    emulator-5554          device product:sdk_gphone64_x86_64 model:sdk_gphone64_x86_64 transport_id:3\n";
        let d = parse_devices(text);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].model.as_deref(), Some("SM A075F"));
        assert!(d[0].is_wireless());
        assert!(!d[1].is_wireless());
    }

    #[test]
    fn wireless_serials() {
        let dev = |s: &str| Device { serial: s.into(), state: "device".into(), model: None };
        assert!(dev("192.168.1.20:37891").is_wireless());
        assert!(dev("[fe80::1]:5555").is_wireless());
        assert!(dev("adb-X-y._adb-tls-connect._tcp").is_wireless());
        assert!(!dev("emulator-5554").is_wireless());
        assert!(!dev("R5CT1234ABC").is_wireless());
        assert!(!is_mdns_serial("192.168.1.20:37891"));
    }

    #[test]
    fn pair_output() {
        let ok = "Successfully paired to 192.168.1.20:41234 [guid=adb-R5CT1234-AbCdEf]\n";
        assert!(pair_succeeded(ok));
        assert_eq!(pair_guid(ok).as_deref(), Some("adb-R5CT1234-AbCdEf"));
        assert!(!pair_succeeded("Failed: Unable to start pairing client.\n"));
        assert_eq!(pair_guid("no guid here"), None);
    }

    #[test]
    fn reconnectable_services() {
        let svc = |i: &str, t: &str, a: &str| MdnsService {
            instance: i.into(),
            service_type: t.into(),
            addr: a.into(),
        };
        let dev = |s: &str| Device { serial: s.into(), state: "device".into(), model: None };
        let services = [
            svc("adb-A-1", "_adb-tls-connect._tcp", "192.168.1.20:37891"),
            svc("adb-A-1", "_adb-tls-connect._tcp", "[fe80::1]:37891"),
            svc("adb-B-2", "_adb-tls-connect._tcp", "192.168.1.21:40000"),
            svc("adb-C-3", "_adb-tls-connect._tcp", "192.168.1.22:40001"),
            svc("adb-D-4", "_adb-tls-connect._tcp", "192.168.1.23:40002"),
            svc("adbqr-x", "_adb-tls-pairing._tcp", "192.168.1.20:41234"),
        ];
        let devices = [
            dev("adb-B-2._adb-tls-connect._tcp"),
            dev("192.168.1.22:40001"),
            dev("adb-D-40._adb-tls-connect._tcp"),
        ];
        let r = reconnectable(&services, &devices);
        let names: Vec<&str> = r.iter().map(|s| s.instance.as_str()).collect();
        assert_eq!(names, ["adb-A-1", "adb-D-4"]);
        assert_eq!(r[0].addr, "192.168.1.20:37891");
    }

    #[test]
    fn netstat_listener() {
        let text = "\nActive Connections\n\n  Proto  Local Address          Foreign Address        State           PID\n  TCP    127.0.0.1:5037         127.0.0.1:49262        ESTABLISHED     10516\n  TCP    0.0.0.0:15037          0.0.0.0:0              LISTENING       4\n  TCP    127.0.0.1:5037         0.0.0.0:0              ABHOEREN        10516\n";
        assert_eq!(parse_netstat_listener(text, 5037), Some(10516));
        assert_eq!(parse_netstat_listener(text, 5038), None);
    }

    #[test]
    fn timeouts() {
        assert!(is_timeout("adb devices -l timed out"));
        assert!(!is_timeout("adb devices -l failed: oops"));
    }

    #[test]
    fn usb_devices() {
        let dev = |s: &str| Device { serial: s.into(), state: "device".into(), model: None };
        assert!(dev("R9RYC00849A").is_usb());
        assert!(!dev("emulator-5554").is_usb());
        assert!(!dev("192.168.1.29:5555").is_usb());
        assert!(!dev("adb-R9RYC00849A-ZxcWiN._adb-tls-connect._tcp").is_usb());
    }

    #[test]
    fn wlan_ip() {
        let text = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever\n\
                    20: rmnet_data0    inet 10.20.30.40/30 scope global rmnet_data0\n\
                    53: wlan0    inet 192.168.1.29/24 brd 192.168.1.255 scope global wlan0\\       valid_lft forever\n";
        assert_eq!(parse_wlan_ip(text).as_deref(), Some("192.168.1.29"));
        assert_eq!(parse_wlan_ip("1: lo    inet 127.0.0.1/8 scope host lo\n"), None);
        assert_eq!(parse_wlan_ip(""), None);
    }

    #[test]
    fn instance_names() {
        assert!(instance_is_phone("adb-R9RYC00849A-ZxcWiN", "R9RYC00849A"));
        assert!(!instance_is_phone("adb-R9RYC00849AB-ZxcWiN", "R9RYC00849A"));
        assert!(!instance_is_phone("adbqr-R9RYC00849A-x", "R9RYC00849A"));
    }

    #[test]
    fn connect_output() {
        assert!(connect_succeeded("connected to 192.168.1.29:5555\n"));
        assert!(connect_succeeded("already connected to 192.168.1.29:5555"));
        assert!(!connect_succeeded("failed to connect to 192.168.1.29:5555"));
        assert!(!connect_succeeded("cannot connect to 192.168.1.29:5555: No route to host"));
    }

    #[test]
    fn instance_serials() {
        assert_eq!(serial_of_instance("adb-R5CT1234-AbCdEf"), Some("R5CT1234"));
        assert_eq!(serial_of_instance("adb-10DECH0FCF00037-8FlQmV"), Some("10DECH0FCF00037"));
        assert_eq!(serial_of_instance("adb-R5CT1234"), None);
        assert_eq!(serial_of_instance("adbqr-abc-def"), None);
    }

    #[test]
    fn phone_info() {
        let text = "sn=R9RYC00849A
port=37891
model=SM-A075F
                    1: lo    inet 127.0.0.1/8 scope host lo
                    53: wlan0    inet 192.168.1.29/24 brd 192.168.1.255 scope global wlan0
";
        let i = parse_phone_info(text);
        assert_eq!(i.serialno, "R9RYC00849A");
        assert_eq!(i.tls_port, Some(37891));
        assert_eq!(i.model.as_deref(), Some("SM-A075F"));
        assert_eq!(i.wlan_ip.as_deref(), Some("192.168.1.29"));
        let off = parse_phone_info("sn=X
port=
model=
");
        assert_eq!(off, PhoneInfo { serialno: "X".into(), ..PhoneInfo::default() });
        assert!(info_command().contains("settings get global vivo_device_name"));
        assert_eq!(parse_phone_info("port=0").tls_port, None);
    }

    #[test]
    fn phone_info_names() {
        // What a vivo X200 Pro prints: no name property, but vivo's own setting.
        let vivo = "sn=10DECH0FCF00037
port=41235
model=V2413
brand=vivo
android=16
ro.product.marketname=
ro.vivo.market.name=
global.vivo_device_name=vivo X200 Pro
global.device_name=V2413
secure.bluetooth_name=vivo X200 Pro
";
        let i = parse_phone_info(vivo);
        assert_eq!(i.name.as_deref(), Some("vivo X200 Pro"));
        assert_eq!(i.model.as_deref(), Some("V2413"));
        assert_eq!(i.brand.as_deref(), Some("vivo"));
        assert_eq!(i.android.as_deref(), Some("16"));

        let xiaomi = "model=2311DRK48G\nbrand=POCO\nro.product.marketname=F6 Pro\nglobal.vivo_device_name=null\n";
        assert_eq!(parse_phone_info(xiaomi).name.as_deref(), Some("POCO F6 Pro"));

        let samsung = "model=SM-S928B\nbrand=samsung\nglobal.vivo_device_name=null\nglobal.device_name=Galaxy S24 Ultra\n";
        assert_eq!(parse_phone_info(samsung).name.as_deref(), Some("Galaxy S24 Ultra"));

        let pixel = "model=Pixel 8\nbrand=google\nglobal.device_name=Pixel 8\nsecure.bluetooth_name=Pixel 8\n";
        assert_eq!(parse_phone_info(pixel).name, None, "the model already is the name");
    }

    #[test]
    fn names() {
        let n = |b, m, s, u| phone_name(b, m, s, u);
        assert_eq!(n(Some("vivo"), Some("V2413"), Some("X200 Pro"), None).as_deref(), Some("vivo X200 Pro"));
        assert_eq!(n(Some("oneplus"), Some("CPH2581"), Some("OnePlus 12"), None).as_deref(), Some("OnePlus 12"));
        assert_eq!(n(Some("samsung"), Some("SM-A075F"), None, Some("Sam's A07")).as_deref(), Some("Sam's A07"));
        assert_eq!(n(Some("vivo"), Some("V2413"), Some("vivo V2413"), Some("Android")), None);
        assert_eq!(n(None, None, None, None), None);
    }

    #[test]
    fn brands() {
        assert_eq!(brand_label("samsung"), "Samsung");
        assert_eq!(brand_label("oneplus"), "OnePlus");
        assert_eq!(brand_label("vivo"), "vivo");
        assert_eq!(brand_label("acme"), "Acme");
        assert_eq!(brand_label("ACME"), "ACME");
    }

    #[test]
    fn host_split() {
        assert_eq!(host_of("192.168.1.20:5555"), "192.168.1.20");
        assert_eq!(host_of("nohost"), "nohost");
    }
}
