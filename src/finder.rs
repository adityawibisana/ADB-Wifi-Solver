//! Finds a phone that was paired with this PC and connects to it, when adb's
//! mDNS doesn't show it. Wireless debugging listens on a random port that
//! changes each time it's turned on, and DHCP may give the phone a new IP,
//! so this tries, cheapest first:
//!
//! 1. the address it was last connected at,
//! 2. asking for it over mDNS directly (multicast, and unicast to its last IP),
//! 3. its MAC address in the ARP table, in case it got a new IP,
//! 4. (deep only) port 5555 in case it was put in `adb tcpip` mode, then
//!    scanning its IP for the Wireless debugging port.
//!
//! Every candidate port is first checked with an adb handshake probe. Ports
//! found by the scan get `adb connect` only if they answer like adb; the
//! others (an address it was seen at, one mDNS advertised) also get it when
//! they accept a connection but answer differently, in case some Android
//! version answers the probe in a way this doesn't recognize. If no scanned
//! port answers like adb, the few that were open are tried the same way.

use crate::adb::{self, Adb, Device};
use crate::{mdns, net};
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_millis(700);
const MDNS_WAIT: Duration = Duration::from_millis(1500);
// Long enough for a phone in Wi-Fi power save. On Windows a closed port costs the
// whole timeout (it retries after a reset), so the full range takes about 14 s.
const SCAN_TIMEOUT: Duration = Duration::from_millis(500);
const SCAN_WORKERS: usize = 1024;
const TCPIP_PORT: u16 = 5555;
const ONLINE_WAIT: Duration = Duration::from_secs(8);
/// If no scanned port answers like adb, `adb connect` is tried on the open
/// ones anyway, if there are at most this many.
const LENIENT_MAX: usize = 4;

pub struct Target {
    pub serialno: String,
    pub guid: Option<String>,
    pub addr: Option<String>,
    pub mac: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct Found {
    /// The serial adb lists it by now.
    pub serial: String,
    /// How it was found, for the message.
    pub how: &'static str,
}

pub type Status<'a> = &'a (dyn Fn(&str) + Sync);

/// What the search needs from adb; a fake one in the tests.
pub trait Connector {
    /// Runs `adb connect` and returns what it printed.
    fn connect(&self, addr: &str) -> Result<String, String>;
    fn disconnect(&self, addr: &str);
    fn devices(&self) -> Vec<Device>;
    /// The `ro.serialno` of a connected device.
    fn serialno(&self, serial: &str) -> Result<String, String>;
    fn note(&self, line: String);
}

impl Connector for Adb {
    fn connect(&self, addr: &str) -> Result<String, String> {
        Ok(self.run(&["connect", addr], Duration::from_secs(15))?.text.trim().to_string())
    }
    fn disconnect(&self, addr: &str) {
        let _ = self.quiet().run(&["disconnect", addr], Duration::from_secs(5));
    }
    fn devices(&self) -> Vec<Device> {
        self.quiet().devices().unwrap_or_default()
    }
    fn serialno(&self, serial: &str) -> Result<String, String> {
        self.quiet().shell(serial, "getprop ro.serialno")
    }
    fn note(&self, line: String) {
        Adb::note(self, line)
    }
}

/// Looks for the phone and connects to it. `deep` adds the port scan, which
/// takes several seconds and touches every port on the phone's IP, so it's
/// only done when asked. `local` is this PC's IP, for the subnet sweep.
pub fn find(adb: &impl Connector, t: &Target, deep: bool, local: Option<Ipv4Addr>, status: Status) -> Result<Found, String> {
    let ports: Vec<u16> = if deep { net::WIRELESS_DEBUGGING_PORTS.collect() } else { Vec::new() };
    find_with(adb, t, &ports, local, status)
}

/// [`find`], scanning `ports` (none: a quick search).
fn find_with(c: &impl Connector, t: &Target, ports: &[u16], local: Option<Ipv4Addr>, status: Status) -> Result<Found, String> {
    let deep = !ports.is_empty();
    let mut s = Search { c, t, tried: HashSet::new(), refused: None };
    let last: Option<SocketAddrV4> = t.addr.as_deref().and_then(|a| a.parse().ok());

    if let Some(a) = last {
        status(&format!("Trying {a}..."));
        if let Some(f) = s.attempt(a, "at its last address", true)? {
            return Ok(f);
        }
    }

    status("Asking the network for it over mDNS...");
    let hosts: Vec<Ipv4Addr> = last.map(|a| *a.ip()).into_iter().collect();
    let instances: Vec<String> = t.guid.iter().cloned().collect();
    for (instance, a) in mdns::query(&hosts, &instances, MDNS_WAIT) {
        if adb::instance_is_phone(&instance, &t.serialno) {
            if let Some(f) = s.attempt(a, "over mDNS", true)? {
                return Ok(f);
            }
        }
    }

    // IPs it may be at now: wherever its MAC address is, then its last IP.
    let mut ips: Vec<Ipv4Addr> = Vec::new();
    if let Some(mac) = &t.mac {
        if deep {
            status("Looking for its Wi-Fi address...");
            let mut nets: Vec<[u8; 3]> = Vec::new();
            for ip in local.into_iter().chain(last.map(|a| *a.ip())) {
                let o = ip.octets();
                if !nets.contains(&[o[0], o[1], o[2]]) {
                    nets.push([o[0], o[1], o[2]]);
                    net::sweep(ip);
                }
            }
            thread::sleep(Duration::from_millis(1500));
        }
        for (ip, m) in net::arp_table() {
            if &m == mac && !ips.contains(&ip) {
                ips.push(ip);
            }
        }
        if let Some(port) = last.map(|a| a.port()) {
            for &ip in &ips {
                status(&format!("It's at {ip} now; trying its last port..."));
                if let Some(f) = s.attempt(SocketAddrV4::new(ip, port), "at a new IP", true)? {
                    return Ok(f);
                }
            }
        }
    }
    if let Some(a) = last {
        if !ips.contains(a.ip()) {
            ips.push(*a.ip());
        }
    }

    if deep {
        for &ip in &ips {
            if let Some(f) = s.attempt(SocketAddrV4::new(ip, TCPIP_PORT), "on the adb tcpip port", false)? {
                return Ok(f);
            }
            let total = ports.len();
            let started = Instant::now();
            status(&format!("Scanning {ip} for Wireless debugging's port..."));
            let report = |n: usize| status(&format!("Scanning {ip} for Wireless debugging's port ({}%)...", n * 100 / total));
            let open = net::open_ports(ip, ports, SCAN_TIMEOUT, SCAN_WORKERS, &AtomicBool::new(false), &report);
            c.note(format!("  scanned {total} ports on {ip} in {} ms; open: {open:?}", started.elapsed().as_millis()));
            for port in &open {
                if let Some(f) = s.attempt(SocketAddrV4::new(ip, *port), "by scanning its ports", false)? {
                    return Ok(f);
                }
            }
            if open.len() <= LENIENT_MAX {
                for port in &open {
                    s.tried.remove(&SocketAddrV4::new(ip, *port));
                    if let Some(f) = s.attempt(SocketAddrV4::new(ip, *port), "by scanning its ports", true)? {
                        return Ok(f);
                    }
                }
            }
        }
    }

    if let Some(why) = s.refused {
        return Err(why);
    }
    let name = t.serialno.as_str();
    Err(if !deep {
        format!("{name} isn't at its last address and didn't answer over mDNS")
    } else if ips.is_empty() {
        format!(
            "Couldn't find {name}. adbqr hasn't seen it connected yet, so it has no address to look at, and it didn't \
             answer over mDNS. Check that Wireless debugging is on and the phone is on this Wi-Fi."
        )
    } else {
        format!(
            "Couldn't find {name}. Check that Wireless debugging is on and the phone is on the same Wi-Fi as this PC. \
             Some networks, guest Wi-Fi for one, don't let devices reach each other."
        )
    })
}

struct Search<'a, C> {
    c: &'a C,
    t: &'a Target,
    tried: HashSet<SocketAddrV4>,
    /// Why something that answered as adb didn't work out, if it didn't.
    refused: Option<String>,
}

impl<C: Connector> Search<'_, C> {
    /// Connects to `addr` if it's an adb daemon, or with `lenient` if it's
    /// anything that accepts a connection. Ok(None) means try the next one.
    fn attempt(&mut self, addr: SocketAddrV4, how: &'static str, lenient: bool) -> Result<Option<Found>, String> {
        if !self.tried.insert(addr) {
            return Ok(None);
        }
        match net::probe(addr, PROBE_TIMEOUT) {
            net::Probe::Adb => {}
            net::Probe::Other if lenient => {
                self.c.note(format!("  {addr} is open but didn't answer like adb; trying adb connect anyway"));
            }
            _ => return Ok(None),
        }
        let a = addr.to_string();
        let text = self.c.connect(&a)?;
        if !adb::connect_succeeded(&text) {
            // A failed connect can leave an offline entry behind.
            self.c.disconnect(&a);
            self.refused.get_or_insert(format!(
                "Found adb at {a}, but it didn't let this PC connect ({text}). If that is the phone, it may no longer \
                 be paired with this PC; pair it again with the QR code."
            ));
            return Ok(None);
        }
        let serialno = &self.t.serialno;
        let start = Instant::now();
        loop {
            let devices = self.c.devices();
            // adb may have connected it through mDNS at the same moment.
            let by_mdns = devices.iter().find(|d| {
                d.state == "device"
                    && d.serial.strip_suffix("._adb-tls-connect._tcp").is_some_and(|i| adb::instance_is_phone(i, serialno))
            });
            if let Some(d) = by_mdns {
                return Ok(Some(Found { serial: d.serial.clone(), how }));
            }
            match devices.iter().find(|d| d.serial == a).map(|d| d.state.as_str()) {
                Some("device") => break,
                Some("unauthorized") => return Err(format!("{a} is waiting for you to allow debugging on the phone")),
                _ if start.elapsed() > ONLINE_WAIT => break,
                _ => thread::sleep(Duration::from_millis(300)),
            }
        }
        // Another of the user's phones may have this address now.
        match self.c.serialno(&a) {
            Ok(sn) if sn == *serialno => Ok(Some(Found { serial: a, how })),
            Ok(sn) => {
                self.c.note(format!("  {a} is {sn}, not {serialno}; disconnecting it"));
                self.c.disconnect(&a);
                Ok(None)
            }
            Err(e) => {
                self.c.disconnect(&a);
                self.refused.get_or_insert(format!("Connected to {a}, but the phone didn't come online ({e})"));
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    /// adb as far as the search sees it: `phones` maps address -> serial
    /// number of the phone that accepts a connection there.
    struct FakeAdb {
        phones: Vec<(String, String)>,
        connected: Mutex<Vec<String>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeAdb {
        fn new(phones: &[(&str, &str)]) -> Self {
            FakeAdb {
                phones: phones.iter().map(|(a, s)| (a.to_string(), s.to_string())).collect(),
                connected: Mutex::default(),
                calls: Mutex::default(),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Connector for FakeAdb {
        fn connect(&self, addr: &str) -> Result<String, String> {
            self.calls.lock().unwrap().push(format!("connect {addr}"));
            if self.phones.iter().any(|(a, _)| a == addr) {
                self.connected.lock().unwrap().push(addr.to_string());
                Ok(format!("connected to {addr}"))
            } else {
                Ok(format!("failed to connect to {addr}"))
            }
        }
        fn disconnect(&self, addr: &str) {
            self.calls.lock().unwrap().push(format!("disconnect {addr}"));
            self.connected.lock().unwrap().retain(|a| a != addr);
        }
        fn devices(&self) -> Vec<Device> {
            let connected = self.connected.lock().unwrap();
            connected.iter().map(|a| Device { serial: a.clone(), state: "device".into(), model: None }).collect()
        }
        fn serialno(&self, serial: &str) -> Result<String, String> {
            self.phones.iter().find(|(a, _)| a == serial).map(|(_, s)| s.clone()).ok_or("offline".into())
        }
        fn note(&self, _: String) {}
    }

    /// Something answering the adb handshake on loopback, like Wireless debugging does.
    fn fake_adbd() -> SocketAddrV4 {
        // A port in the range the scan covers.
        let listener = (45000..50000).find_map(|p| TcpListener::bind(("127.0.0.1", p)).ok()).unwrap();
        let addr = match listener.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            _ => unreachable!(),
        };
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut h = [0u8; 31];
                if s.read_exact(&mut h).is_ok() {
                    let _ = s.write_all(&net::adb_message(0x534c_5453, 0x0100_0000, 0, b""));
                }
            }
        });
        addr
    }

    fn closed_port() -> u16 {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn target(addr: &str) -> Target {
        Target { serialno: "R5CT1234".into(), guid: None, addr: Some(addr.into()), mac: None }
    }

    #[test]
    fn last_address() {
        let adbd = fake_adbd();
        let adb = FakeAdb::new(&[(&adbd.to_string(), "R5CT1234")]);
        let f = find_with(&adb, &target(&adbd.to_string()), &[], None, &|_| {}).unwrap();
        assert_eq!(f, Found { serial: adbd.to_string(), how: "at its last address" });
        assert_eq!(adb.calls(), [format!("connect {adbd}")]);
    }

    /// Wireless debugging came back on another port: the quick search gives
    /// up, the deep one scans, and `adb connect` only runs against the port
    /// that answered the handshake, not the one that's open but not adb.
    #[test]
    fn scans_for_new_port() {
        let adbd = fake_adbd();
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let other_port = other.local_addr().unwrap().port();
        let adb = FakeAdb::new(&[(&adbd.to_string(), "R5CT1234")]);
        let old = format!("127.0.0.1:{}", closed_port());

        let quick = find_with(&adb, &target(&old), &[], None, &|_| {});
        assert!(quick.unwrap_err().contains("isn't at its last address"));
        assert!(adb.calls().is_empty(), "nothing answered, so adb connect never ran");

        let ports = [closed_port(), other_port, adbd.port()];
        let f = find_with(&adb, &target(&old), &ports, None, &|_| {}).unwrap();
        assert_eq!(f, Found { serial: adbd.to_string(), how: "by scanning its ports" });
        assert_eq!(adb.calls(), [format!("connect {adbd}")]);
    }

    /// If the probe doesn't recognize Wireless debugging (some Android
    /// version answering differently), the few open ports are still tried.
    #[test]
    fn unrecognized_answer() {
        let quiet = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = quiet.local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{port}");
        let adb = FakeAdb::new(&[(&addr, "R5CT1234")]);
        let old = format!("127.0.0.1:{}", closed_port());
        let f = find_with(&adb, &target(&old), &[closed_port(), port], None, &|_| {}).unwrap();
        assert_eq!(f, Found { serial: addr.clone(), how: "by scanning its ports" });
        assert_eq!(adb.calls(), [format!("connect {addr}")]);

        // And at its last address, straight away.
        let adb = FakeAdb::new(&[(&addr, "R5CT1234")]);
        assert_eq!(find_with(&adb, &target(&addr), &[], None, &|_| {}).unwrap().how, "at its last address");
    }

    /// A different phone answers at the address: it is disconnected again,
    /// and the search goes on.
    #[test]
    fn other_phone_at_address() {
        let adbd = fake_adbd();
        let adb = FakeAdb::new(&[(&adbd.to_string(), "SOMEONE-ELSE")]);
        let err = find_with(&adb, &target(&adbd.to_string()), &[], None, &|_| {}).unwrap_err();
        assert!(err.contains("isn't at its last address"), "{err}");
        assert_eq!(adb.calls(), [format!("connect {adbd}"), format!("disconnect {adbd}")]);
        assert!(adb.connected.lock().unwrap().is_empty());
    }

    /// adb answers but won't connect, as an unpaired phone does.
    #[test]
    fn refused() {
        let adbd = fake_adbd();
        let adb = FakeAdb::new(&[]);
        let err = find_with(&adb, &target(&adbd.to_string()), &[], None, &|_| {}).unwrap_err();
        assert!(err.contains("pair it again"), "{err}");
    }
}
