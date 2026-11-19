use std::env;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

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

#[derive(Debug, PartialEq)]
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
}

/// Serials adb gives devices it connected to through mDNS.
pub fn is_mdns_serial(serial: &str) -> bool {
    serial.contains("._adb-tls-connect._tcp")
}

fn is_ip_port(serial: &str) -> bool {
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
        #[cfg(windows)]
        {
            // Don't flash a console window for every adb call.
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
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
        }
        Ok(Output { success: status.success(), text })
    }

    pub fn mdns_services(&self) -> Result<Vec<MdnsService>, String> {
        Ok(parse_mdns_services(&self.run(&["mdns", "services"], Duration::from_secs(10))?.text))
    }

    pub fn devices(&self) -> Result<Vec<Device>, String> {
        Ok(parse_devices(&self.run(&["devices", "-l"], Duration::from_secs(10))?.text))
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

        let result = self.run(
            &["-s", serial, "shell", "settings", "put", "global", "adb_wifi_enabled", "0"],
            Duration::from_secs(10),
        );
        // The phone can drop the connection before the command reports back,
        // so judge by whether the device actually goes away.
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
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
    fn host_split() {
        assert_eq!(host_of("192.168.1.20:5555"), "192.168.1.20");
        assert_eq!(host_of("nohost"), "nohost");
    }
}
