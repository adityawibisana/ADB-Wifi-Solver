//! Local network helpers for finding a paired phone when mDNS doesn't show
//! it: the ARP table (to follow a phone's MAC address to a new IP), the
//! default gateway (to tell which network this PC is on), a subnet sweep, a
//! port scan, and a probe that tells whether a port is adb.

use crate::adb::capture;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

/// Ports Android picks Wireless debugging's port from (Linux's default
/// ephemeral range).
pub const WIRELESS_DEBUGGING_PORTS: std::ops::RangeInclusive<u16> = 32768..=60999;

/// Normalizes a MAC address (`AA-BB-CC-DD-EE-FF`, `a:b:c:d:e:f`) to
/// `aa:bb:cc:dd:ee:ff`. None for anything else, and for all-zero, broadcast
/// and multicast addresses, which never belong to a phone.
pub fn normalize_mac(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split(['-', ':']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut bytes = [0u8; 6];
    for (b, p) in bytes.iter_mut().zip(&parts) {
        if p.is_empty() || p.len() > 2 {
            return None;
        }
        *b = u8::from_str_radix(p, 16).ok()?;
    }
    // Bit 0 of the first byte marks multicast, broadcast included.
    if bytes == [0; 6] || bytes[0] & 1 == 1 {
        return None;
    }
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"))
}

/// Parses an ARP table: `arp -a` (Windows), `arp -an` (macOS) or
/// `/proc/net/arp` (Linux). Each has an IPv4 address and a MAC on the line
/// of an entry, so one parser takes the first of each.
pub fn parse_arp(text: &str) -> Vec<(Ipv4Addr, String)> {
    text.lines()
        .filter_map(|line| {
            let mut ip = None;
            let mut mac = None;
            for tok in line.split_whitespace() {
                let tok = tok.trim_matches(['(', ')']);
                if ip.is_none() {
                    if let Ok(a) = tok.parse::<Ipv4Addr>() {
                        ip = Some(a);
                        continue;
                    }
                }
                if mac.is_none() {
                    mac = normalize_mac(tok);
                }
            }
            Some((ip?, mac?))
        })
        .collect()
}

pub fn arp_table() -> Vec<(Ipv4Addr, String)> {
    let text = if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/net/arp").unwrap_or_default()
    } else if cfg!(windows) {
        capture("arp", &["-a"]).unwrap_or_default()
    } else {
        capture("arp", &["-an"]).unwrap_or_default()
    };
    parse_arp(&text)
}

pub fn mac_of(ip: Ipv4Addr, table: &[(Ipv4Addr, String)]) -> Option<String> {
    table.iter().find(|(a, _)| *a == ip).map(|(_, m)| m.clone())
}

/// The default gateway from `route print -4` (Windows): the `0.0.0.0
/// 0.0.0.0 <gateway> <interface> <metric>` row with the lowest metric.
pub fn parse_route_print(text: &str) -> Option<Ipv4Addr> {
    text.lines()
        .filter_map(|line| {
            let t: Vec<&str> = line.split_whitespace().collect();
            if t.len() != 5 || t[0] != "0.0.0.0" || t[1] != "0.0.0.0" {
                return None;
            }
            let gw = t[2].parse::<Ipv4Addr>().ok()?;
            t[3].parse::<Ipv4Addr>().ok()?;
            Some((t[4].parse::<u32>().ok()?, gw))
        })
        .min_by_key(|(metric, _)| *metric)
        .map(|(_, gw)| gw)
}

/// The default gateway from `/proc/net/route` (Linux), where addresses are
/// little-endian hex.
pub fn parse_proc_route(text: &str) -> Option<Ipv4Addr> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let t: Vec<&str> = line.split_whitespace().collect();
            if t.len() < 8 || t[1] != "00000000" || t[7] != "00000000" {
                return None;
            }
            let flags = u32::from_str_radix(t[3], 16).ok()?;
            if flags & 0x2 == 0 {
                return None; // not RTF_GATEWAY
            }
            let gw = Ipv4Addr::from(u32::from_str_radix(t[2], 16).ok()?.to_le_bytes());
            Some((t[6].parse::<u32>().unwrap_or(0), gw))
        })
        .min_by_key(|(metric, _)| *metric)
        .map(|(_, gw)| gw)
}

/// The default gateway from `route -n get default` (macOS).
pub fn parse_route_get(text: &str) -> Option<Ipv4Addr> {
    text.lines().find_map(|l| l.trim().strip_prefix("gateway:")?.trim().parse().ok())
}

pub fn default_gateway() -> Option<Ipv4Addr> {
    if cfg!(target_os = "linux") {
        parse_proc_route(&std::fs::read_to_string("/proc/net/route").ok()?)
    } else if cfg!(windows) {
        parse_route_print(&capture("route", &["print", "-4"]).ok()?)
    } else {
        parse_route_get(&capture("route", &["-n", "get", "default"]).ok()?)
    }
}

/// This PC's address on the way to `dst`. Connecting a UDP socket sends
/// nothing; it only picks the route.
pub fn local_ip_towards(dst: Ipv4Addr) -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect((dst, 9)).ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(a) => Some(a),
        IpAddr::V6(_) => None,
    }
}

/// The network this PC is on, identified by its gateway's MAC address, so
/// two networks that both use 192.168.1.1 aren't taken for the same one.
#[derive(Clone, Debug, PartialEq)]
pub struct Network {
    pub id: String,
    pub local: Option<Ipv4Addr>,
}

pub fn current_network() -> Option<Network> {
    let gw = default_gateway()?;
    let local = local_ip_towards(gw);
    let mut mac = mac_of(gw, &arp_table());
    if mac.is_none() {
        poke(gw);
        thread::sleep(Duration::from_millis(300));
        mac = mac_of(gw, &arp_table());
    }
    let id = match mac {
        Some(m) => format!("gateway {m}"),
        None => format!("gateway {gw}"),
    };
    Some(Network { id, local })
}

/// Sends one UDP packet to the discard port, which makes the OS look up the
/// host's MAC address and put it in the ARP table.
fn poke(ip: Ipv4Addr) {
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        let _ = s.send_to(&[0], (ip, 9));
    }
}

/// Pokes every host in `around`'s /24, so the ARP table lists every device
/// that is on. Callers wait a moment before reading the table.
pub fn sweep(around: Ipv4Addr) {
    let Ok(s) = UdpSocket::bind("0.0.0.0:0") else { return };
    let o = around.octets();
    for h in 1..=254u8 {
        let _ = s.send_to(&[0], (Ipv4Addr::new(o[0], o[1], o[2], h), 9));
    }
}

/// The ports among `ports` that accept a TCP connection. Calls `progress`
/// with how many were checked, now and then, and stops early when `cancel`
/// is set.
pub fn open_ports(
    ip: Ipv4Addr,
    ports: &[u16],
    timeout: Duration,
    workers: usize,
    cancel: &AtomicBool,
    progress: &(dyn Fn(usize) + Sync),
) -> Vec<u16> {
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let open = Mutex::new(Vec::new());
    thread::scope(|scope| {
        for _ in 0..workers.max(1).min(ports.len().max(1)) {
            let worker = || loop {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(&port) = ports.get(i) else { break };
                let addr = SocketAddr::V4(SocketAddrV4::new(ip, port));
                if TcpStream::connect_timeout(&addr, timeout).is_ok() {
                    open.lock().unwrap().push(port);
                }
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_multiple_of(500) {
                    progress(n);
                }
            };
            // Small stacks: there can be hundreds of these.
            let spawned = thread::Builder::new().stack_size(64 * 1024).spawn_scoped(scope, worker);
            if spawned.is_err() {
                break;
            }
        }
    });
    let mut open = open.into_inner().unwrap();
    open.sort_unstable();
    open
}

const A_CNXN: u32 = 0x4e58_4e43;
const A_AUTH: u32 = 0x4854_5541;
const A_STLS: u32 = 0x534c_5453;

/// An adb protocol message: a 24-byte header, then the payload.
pub fn adb_message(command: u32, arg0: u32, arg1: u32, payload: &[u8]) -> Vec<u8> {
    let check: u32 = payload.iter().map(|&b| b as u32).sum();
    let mut out = Vec::with_capacity(24 + payload.len());
    for word in [command, arg0, arg1, payload.len() as u32, check, command ^ 0xffff_ffff] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// Whether a 24-byte header is adbd answering a connect: STLS from
/// Wireless debugging, AUTH (or CNXN) from `adb tcpip`.
pub fn is_adb_reply(header: &[u8]) -> bool {
    if header.len() < 24 {
        return false;
    }
    let word = |i: usize| u32::from_le_bytes(header[i..i + 4].try_into().unwrap());
    let command = word(0);
    [A_CNXN, A_AUTH, A_STLS].contains(&command) && word(20) == command ^ 0xffff_ffff
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Probe {
    /// Nothing accepted the connection.
    Closed,
    /// Answered like adbd.
    Adb,
    /// Accepted the connection but didn't answer like adbd.
    Other,
}

/// Whether `addr` is an adb daemon. Sends the CNXN message adb starts every
/// connection with and checks the reply's header. Nothing shows on the
/// phone: it is waiting for a TLS handshake or a key when the socket closes.
pub fn probe(addr: SocketAddrV4, timeout: Duration) -> Probe {
    let Ok(mut s) = TcpStream::connect_timeout(&SocketAddr::V4(addr), timeout) else { return Probe::Closed };
    let _ = s.set_read_timeout(Some(timeout * 2));
    let _ = s.set_write_timeout(Some(timeout));
    const VERSION: u32 = 0x0100_0001;
    const MAX_PAYLOAD: u32 = 256 * 1024;
    let mut header = [0u8; 24];
    let adb = s.write_all(&adb_message(A_CNXN, VERSION, MAX_PAYLOAD, b"host::\0")).is_ok()
        && s.read_exact(&mut header).is_ok()
        && is_adb_reply(&header);
    if adb {
        Probe::Adb
    } else {
        Probe::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn macs() {
        assert_eq!(normalize_mac("F4-E8-4F-05-2F-08").as_deref(), Some("f4:e8:4f:05:2f:08"));
        assert_eq!(normalize_mac("a:b:c:d:e:f").as_deref(), Some("0a:0b:0c:0d:0e:0f"));
        assert_eq!(normalize_mac("ff-ff-ff-ff-ff-ff"), None);
        assert_eq!(normalize_mac("01-00-5e-00-00-fb"), None);
        assert_eq!(normalize_mac("00:00:00:00:00:00"), None);
        assert_eq!(normalize_mac("0x2"), None);
        assert_eq!(normalize_mac("192.168.1.1"), None);
    }

    #[test]
    fn windows_arp() {
        let text = "\nInterface: 192.168.1.27 --- 0x5\n  Internet Address      Physical Address      Type\n  \
                    192.168.1.1           f4-e8-4f-05-2f-08     dynamic   \n  \
                    192.168.1.76          3c-4e-56-6c-3f-16     dynamisch \n  \
                    192.168.1.255         ff-ff-ff-ff-ff-ff     static    \n  \
                    224.0.0.251           01-00-5e-00-00-fb     static    \n";
        let t = parse_arp(text);
        assert_eq!(
            t,
            [
                ("192.168.1.1".parse().unwrap(), "f4:e8:4f:05:2f:08".to_string()),
                ("192.168.1.76".parse().unwrap(), "3c:4e:56:6c:3f:16".to_string()),
            ]
        );
    }

    #[test]
    fn linux_and_mac_arp() {
        let proc = "IP address       HW type     Flags       HW address            Mask     Device\n\
                    192.168.1.20     0x1         0x2         aa:bb:cc:dd:ee:01     *        wlan0\n\
                    192.168.1.21     0x1         0x0         00:00:00:00:00:00     *        wlan0\n";
        assert_eq!(parse_arp(proc), [("192.168.1.20".parse().unwrap(), "aa:bb:cc:dd:ee:01".to_string())]);
        let mac = "? (192.168.1.20) at aa:b:cc:d:ee:1 on en0 ifscope [ethernet]\n\
                   ? (192.168.1.22) at (incomplete) on en0 ifscope [ethernet]\n";
        assert_eq!(parse_arp(mac), [("192.168.1.20".parse().unwrap(), "aa:0b:cc:0d:ee:01".to_string())]);
    }

    #[test]
    fn gateways() {
        let win = "IPv4 Route Table\nActive Routes:\nNetwork Destination        Netmask          Gateway       Interface  Metric\n\
                   0.0.0.0          0.0.0.0      10.0.0.1     10.0.0.7     80\n\
                   0.0.0.0          0.0.0.0      192.168.1.1     192.168.1.27     55\n\
                   127.0.0.0        255.0.0.0         On-link         127.0.0.1    331\n\
                   Persistent Routes:\n  0.0.0.0          0.0.0.0      172.16.0.1  Default\n";
        assert_eq!(parse_route_print(win), Some(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(parse_route_print("Active Routes:\n  None\n"), None);

        let linux = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                     wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
                     wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(parse_proc_route(linux), Some(Ipv4Addr::new(192, 168, 1, 1)));

        let mac = "   route to: default\ndestination: default\n       mask: default\n    gateway: 192.168.0.1\n  interface: en0\n";
        assert_eq!(parse_route_get(mac), Some(Ipv4Addr::new(192, 168, 0, 1)));
    }

    #[test]
    fn adb_messages() {
        let m = adb_message(A_CNXN, 0x0100_0001, 256 * 1024, b"host::\0");
        assert_eq!(m.len(), 24 + 7);
        assert_eq!(&m[0..4], b"CNXN");
        assert!(is_adb_reply(&m[..24]));
        assert!(is_adb_reply(&adb_message(A_STLS, 0x0100_0000, 0, b"")));
        assert!(is_adb_reply(&adb_message(A_AUTH, 1, 0, &[0; 20])[..24]));
        let mut bad = adb_message(A_STLS, 0, 0, b"");
        bad[20] ^= 1;
        assert!(!is_adb_reply(&bad));
        assert!(!is_adb_reply(b"HTTP/1.1 400 Bad Request\r\n"));
    }

    /// A fake Wireless debugging port answers CNXN with STLS; an HTTP-ish
    /// server and a closed port don't count as adb.
    #[test]
    fn probes() {
        let adbd = TcpListener::bind("127.0.0.1:0").unwrap();
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let adbd_addr = adbd.local_addr().unwrap();
        let other_addr = other.local_addr().unwrap();
        thread::spawn(move || {
            let (mut s, _) = adbd.accept().unwrap();
            let mut h = [0u8; 31];
            s.read_exact(&mut h).unwrap();
            assert_eq!(&h[0..4], b"CNXN");
            s.write_all(&adb_message(A_STLS, 0x0100_0000, 0, b"")).unwrap();
        });
        thread::spawn(move || {
            let (mut s, _) = other.accept().unwrap();
            s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").unwrap();
        });
        let v4 = |a: SocketAddr| match a {
            SocketAddr::V4(a) => a,
            _ => unreachable!(),
        };
        let t = Duration::from_millis(500);
        assert_eq!(probe(v4(adbd_addr), t), Probe::Adb);
        assert_eq!(probe(v4(other_addr), t), Probe::Other);
        // Nothing listens on a port just released.
        let closed = v4(TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap());
        assert_eq!(probe(closed, t), Probe::Closed);
    }

    #[test]
    fn scans_ports() {
        let a = TcpListener::bind("127.0.0.1:0").unwrap();
        let b = TcpListener::bind("127.0.0.1:0").unwrap();
        let pa = a.local_addr().unwrap().port();
        let pb = b.local_addr().unwrap().port();
        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let cancel = AtomicBool::new(false);
        let open = open_ports(Ipv4Addr::LOCALHOST, &[pa, closed, pb], Duration::from_millis(300), 8, &cancel, &|_| {});
        let mut want = vec![pa, pb];
        want.sort_unstable();
        assert_eq!(open, want);
    }
}

/// Checks against the real network this PC is on: `cargo test -- --ignored --nocapture lan_`.
#[cfg(test)]
mod lan {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore]
    fn lan_network() {
        let n = current_network().expect("no network");
        println!("network: {n:?}");
        println!("arp: {:?}", arp_table());
        assert!(n.id.starts_with("gateway "));
    }

    #[test]
    #[ignore]
    fn lan_scan_speed() {
        let gw = default_gateway().unwrap();
        let ports: Vec<u16> = (40000..46000).collect();
        let start = Instant::now();
        let open = open_ports(gw, &ports, Duration::from_millis(500), 1024, &AtomicBool::new(false), &|_| {});
        let ms = start.elapsed().as_millis();
        println!("{} ports on {gw} in {ms} ms, open {open:?}; full range would take ~{} s",
            ports.len(), ms as usize * WIRELESS_DEBUGGING_PORTS.len() / ports.len() / 1000);
    }
}
