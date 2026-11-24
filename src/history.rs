//! Phones this PC has connected to over Wi-Fi, remembered between runs, so
//! they can be found again when mDNS doesn't show them: where each was last
//! seen, its MAC address there, and which network that was. Also reads the
//! list of phones adb has paired with, kept by adb itself.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Known {
    /// `ro.serialno`, which mDNS instance names are made from. The key.
    pub serialno: String,
    /// Last Wireless debugging mDNS instance name (`adb-<serialno>-<random>`).
    pub guid: Option<String>,
    pub model: Option<String>,
    /// Its name as sold, like `vivo X200 Pro`; see [`crate::adb::phone_name`].
    pub name: Option<String>,
    /// Android version, like `16`.
    pub android: Option<String>,
    /// Last `ip:port` adb was connected to it at.
    pub addr: Option<String>,
    /// That address was `adb tcpip`, not Wireless debugging.
    pub tcpip: bool,
    /// Its MAC address on that network, from this PC's ARP table.
    pub mac: Option<String>,
    /// [`crate::net::Network::id`] of the network it was last seen on.
    pub network: Option<String>,
    /// Unix seconds; 0 if adbqr never saw it (only adb's pairing list has it).
    pub last_seen: u64,
    /// Disconnected from adbqr, so not reconnected automatically until it
    /// connects again some other way.
    pub paused: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(default)]
pub struct Store {
    pub auto_reconnect: bool,
    pub devices: Vec<Known>,
    /// Serial numbers the user removed, kept so adb's pairing list doesn't bring them back.
    pub forgotten: Vec<String>,
}

impl Default for Store {
    fn default() -> Self {
        Store { auto_reconnect: true, devices: Vec::new(), forgotten: Vec::new() }
    }
}

/// What was learned about a phone connected over Wi-Fi.
#[derive(Debug, Clone, Default)]
pub struct Observation {
    pub serialno: String,
    pub guid: Option<String>,
    pub model: Option<String>,
    pub name: Option<String>,
    pub android: Option<String>,
    pub addr: Option<String>,
    pub tcpip: bool,
    pub mac: Option<String>,
    pub network: Option<String>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Store {
    pub fn load(path: &Path) -> Store {
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    /// Writes to a temporary file first, so a crash can't leave half a file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    pub fn get(&self, serialno: &str) -> Option<&Known> {
        self.devices.iter().find(|d| d.serialno == serialno)
    }

    fn entry(&mut self, serialno: &str) -> &mut Known {
        match self.devices.iter().position(|d| d.serialno == serialno) {
            Some(i) => &mut self.devices[i],
            None => {
                self.devices.push(Known { serialno: serialno.to_string(), ..Known::default() });
                self.devices.last_mut().unwrap()
            }
        }
    }

    /// Records a phone seen connected over Wi-Fi. Fields that weren't
    /// learned this time keep their old value, except that a new address
    /// on a new network drops the old MAC, which belonged to that network.
    pub fn observe(&mut self, o: Observation, at: u64) {
        if o.serialno.is_empty() {
            return;
        }
        self.forgotten.retain(|f| f != &o.serialno);
        let k = self.entry(&o.serialno);
        let moved = o.network.is_some() && o.network != k.network;
        if o.guid.is_some() {
            k.guid = o.guid;
        }
        if o.model.is_some() {
            k.model = o.model;
        }
        if o.name.is_some() {
            k.name = o.name;
        }
        if o.android.is_some() {
            k.android = o.android;
        }
        if let Some(addr) = o.addr {
            k.addr = Some(addr);
            k.tcpip = o.tcpip;
        }
        if o.mac.is_some() || moved {
            k.mac = o.mac;
        }
        if o.network.is_some() {
            k.network = o.network;
        }
        k.last_seen = at;
        k.paused = false;
    }

    /// Adds phones from adb's pairing list that aren't known yet.
    pub fn add_paired(&mut self, guids: &[String]) -> bool {
        let mut changed = false;
        for guid in guids {
            let Some(serialno) = crate::adb::serial_of_instance(guid) else { continue };
            if self.forgotten.iter().any(|f| f == serialno) {
                continue;
            }
            let k = self.entry(serialno);
            if k.guid.is_none() {
                k.guid = Some(guid.clone());
                changed = true;
            }
        }
        changed
    }

    pub fn forget(&mut self, serialno: &str) {
        self.devices.retain(|d| d.serialno != serialno);
        if !self.forgotten.iter().any(|f| f == serialno) {
            self.forgotten.push(serialno.to_string());
        }
    }

    pub fn set_paused(&mut self, serialno: &str, paused: bool) -> bool {
        match self.devices.iter_mut().find(|d| d.serialno == serialno) {
            Some(k) if k.paused != paused => {
                k.paused = paused;
                true
            }
            _ => false,
        }
    }

    /// The phone last seen at `addr`.
    pub fn by_addr(&self, addr: &str) -> Option<&Known> {
        self.devices.iter().filter(|d| d.addr.as_deref() == Some(addr)).max_by_key(|d| d.last_seen)
    }
}

/// Where adb keeps its files: `$ANDROID_USER_HOME`, else `~/.android`.
fn android_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("ANDROID_USER_HOME") {
        return Some(PathBuf::from(d));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join(".android"))
}

/// GUIDs (mDNS instance names) of phones adb paired with, from
/// `adb_known_hosts.pb`, without duplicates.
pub fn paired_guids() -> Vec<String> {
    android_dir()
        .and_then(|d| std::fs::read(d.join("adb_known_hosts.pb")).ok())
        .map(|b| parse_known_hosts(&b))
        .unwrap_or_default()
}

fn varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*pos)?;
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// Calls `f` with each length-delimited field 1 of a protobuf message;
/// skips other fields. None if the message is malformed.
fn each_field1(buf: &[u8], mut f: impl FnMut(&[u8])) -> Option<()> {
    let mut pos = 0;
    while pos < buf.len() {
        let key = varint(buf, &mut pos)?;
        match key & 7 {
            0 => {
                varint(buf, &mut pos)?;
            }
            1 => pos += 8,
            2 => {
                let len = varint(buf, &mut pos)? as usize;
                let body = buf.get(pos..pos.checked_add(len)?)?;
                if key >> 3 == 1 {
                    f(body);
                }
                pos += len;
            }
            5 => pos += 4,
            _ => return None,
        }
    }
    (pos == buf.len()).then_some(())
}

/// `AdbKnownHosts { repeated HostInfo known_hosts = 1; }` where
/// `HostInfo { string guid = 1; }`.
pub fn parse_known_hosts(buf: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let _ = each_field1(buf, |host| {
        let _ = each_field1(host, |guid| {
            if let Ok(g) = std::str::from_utf8(guid) {
                if !g.is_empty() && !out.iter().any(|o| o == g) {
                    out.push(g.to_string());
                }
            }
        });
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(guid: &str) -> Vec<u8> {
        let mut inner = vec![0x0a, guid.len() as u8];
        inner.extend_from_slice(guid.as_bytes());
        let mut out = vec![0x0a, inner.len() as u8];
        out.extend(inner);
        out
    }

    #[test]
    fn known_hosts() {
        let mut buf = host("adb-10DECH0FCF00037-8FlQmV");
        buf.extend(host("adb-R9RYC00849A-ZxcWiN"));
        buf.extend(host("adb-10DECH0FCF00037-8FlQmV"));
        assert_eq!(parse_known_hosts(&buf), ["adb-10DECH0FCF00037-8FlQmV", "adb-R9RYC00849A-ZxcWiN"]);
        assert!(parse_known_hosts(&[]).is_empty());
        // Truncated: keeps what came before.
        let mut cut = host("adb-A-1");
        cut.extend_from_slice(&[0x0a, 0x30, 0x0a]);
        assert_eq!(parse_known_hosts(&cut), ["adb-A-1"]);
    }

    fn seen(serialno: &str, addr: &str, network: &str, mac: Option<&str>) -> Observation {
        Observation {
            serialno: serialno.into(),
            addr: Some(addr.into()),
            network: Some(network.into()),
            mac: mac.map(Into::into),
            ..Observation::default()
        }
    }

    #[test]
    fn observes() {
        let mut s = Store::default();
        s.observe(Observation { model: Some("V2413".into()), name: Some("vivo X200 Pro".into()), ..seen("A", "192.168.1.20:37891", "home", Some("aa:bb:cc:dd:ee:01")) }, 10);
        s.observe(seen("A", "192.168.1.20:40000", "home", None), 20);
        let k = s.get("A").unwrap();
        assert_eq!(k.addr.as_deref(), Some("192.168.1.20:40000"));
        assert_eq!(k.mac.as_deref(), Some("aa:bb:cc:dd:ee:01"), "kept: same network");
        assert_eq!(k.model.as_deref(), Some("V2413"));
        assert_eq!(k.name.as_deref(), Some("vivo X200 Pro"), "kept when not read again");
        assert_eq!(k.last_seen, 20);

        s.observe(seen("A", "10.0.0.5:41000", "office", None), 30);
        let k = s.get("A").unwrap();
        assert_eq!(k.mac, None, "dropped: it belonged to the old network");
        assert_eq!(k.network.as_deref(), Some("office"));
        assert_eq!(s.devices.len(), 1);
        assert_eq!(s.by_addr("10.0.0.5:41000").map(|k| k.serialno.as_str()), Some("A"));

        s.observe(Observation::default(), 40);
        assert_eq!(s.devices.len(), 1, "no serial number, nothing recorded");
    }

    #[test]
    fn pause_forget_and_pairing_list() {
        let mut s = Store::default();
        s.observe(seen("A", "192.168.1.20:37891", "home", None), 10);
        assert!(s.set_paused("A", true));
        assert!(!s.set_paused("A", true));
        assert!(!s.set_paused("B", true));
        s.observe(seen("A", "192.168.1.20:37891", "home", None), 11);
        assert!(!s.get("A").unwrap().paused, "connecting again resumes it");

        assert!(s.add_paired(&["adb-A-x".into(), "adb-B-y".into(), "nonsense".into()]));
        assert_eq!(s.get("A").unwrap().guid.as_deref(), Some("adb-A-x"));
        assert_eq!(s.get("B").unwrap().last_seen, 0);
        assert!(!s.add_paired(&["adb-B-y".into()]));

        s.forget("B");
        assert!(s.get("B").is_none());
        assert!(!s.add_paired(&["adb-B-y".into()]));
        assert!(s.get("B").is_none(), "forgotten stays forgotten");
        s.observe(seen("B", "192.168.1.21:5555", "home", None), 12);
        assert!(s.forgotten.is_empty(), "until it connects again");
    }

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("adbqr-test-{}", std::process::id()));
        let path = dir.join("devices.json");
        let mut s = Store { auto_reconnect: false, ..Store::default() };
        s.observe(seen("A", "192.168.1.20:37891", "home", Some("aa:bb:cc:dd:ee:01")), 10);
        s.save(&path).unwrap();
        assert_eq!(Store::load(&path), s);
        std::fs::write(&path, "{\"devices\":[{\"serialno\":\"Z\"}]}").unwrap();
        let old = Store::load(&path);
        assert!(old.auto_reconnect, "missing fields take defaults");
        assert_eq!(old.devices[0].serialno, "Z");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Store::load(&path), Store::default());
        let _ = std::fs::remove_dir_all(dir);
    }
}
