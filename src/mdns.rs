//! A small mDNS client for finding Wireless debugging services without
//! adb's own mDNS, which is sometimes broken or blocked.
//!
//! It asks from an ordinary UDP port instead of 5353 (a "legacy unicast"
//! query, RFC 6762 section 6.7), so it doesn't compete with adb or the OS
//! for port 5353, and responders answer it directly by unicast. The question
//! goes to the multicast group and also straight to the addresses a phone
//! was last seen at, which still works where multicast is filtered.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

pub const SERVICE: &str = "_adb-tls-connect._tcp.local";
const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const PORT: u16 = 5353;

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_SRV: u16 = 33;

#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    A(Ipv4Addr),
    Ptr(String),
    Srv { port: u16, target: String },
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub name: String,
    pub data: Data,
}

fn encode_name(name: &str, out: &mut Vec<u8>) {
    for label in name.trim_end_matches('.').split('.').filter(|l| !l.is_empty()) {
        let bytes = &label.as_bytes()[..label.len().min(63)];
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
    }
    out.push(0);
}

/// A query message with one question per `(name, type)`.
pub fn build_query(id: u16, questions: &[(String, u16)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0, 0]); // flags: standard query
    out.extend_from_slice(&(questions.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    for (name, qtype) in questions {
        encode_name(name, &mut out);
        out.extend_from_slice(&qtype.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes()); // class IN
    }
    out
}

/// Reads a possibly compressed name at `pos`; returns it and the position after it.
fn read_name(buf: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    // Each pointer must go backwards, which also rules out loops.
    let mut limit = pos;
    loop {
        let len = *buf.get(pos)? as usize;
        if len == 0 {
            end.get_or_insert(pos + 1);
            break;
        }
        if len & 0xc0 == 0xc0 {
            let target = ((len & 0x3f) << 8) | *buf.get(pos + 1)? as usize;
            if target >= limit {
                return None;
            }
            end.get_or_insert(pos + 2);
            limit = target;
            pos = target;
            continue;
        }
        if len & 0xc0 != 0 {
            return None;
        }
        let label = buf.get(pos + 1..pos + 1 + len)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        pos += 1 + len;
    }
    Some((labels.join("."), end?))
}

fn u16_at(buf: &[u8], pos: usize) -> Option<u16> {
    Some(u16::from_be_bytes(buf.get(pos..pos + 2)?.try_into().ok()?))
}

/// The answer, authority and additional records of a response.
pub fn parse_response(buf: &[u8]) -> Option<Vec<Record>> {
    let flags = u16_at(buf, 2)?;
    if flags & 0x8000 == 0 {
        return None; // a query, not a response
    }
    let questions = u16_at(buf, 4)?;
    let records = u16_at(buf, 6)? as usize + u16_at(buf, 8)? as usize + u16_at(buf, 10)? as usize;
    let mut pos = 12;
    for _ in 0..questions {
        pos = read_name(buf, pos)?.1 + 4;
    }
    let mut out = Vec::new();
    for _ in 0..records {
        let (name, p) = read_name(buf, pos)?;
        let rtype = u16_at(buf, p)?;
        let len = u16_at(buf, p + 8)? as usize;
        let start = p + 10;
        let rdata = buf.get(start..start + len)?;
        let data = match rtype {
            TYPE_A if len == 4 => Data::A(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3])),
            TYPE_PTR => Data::Ptr(read_name(buf, start)?.0),
            TYPE_SRV if len >= 7 => Data::Srv { port: u16_at(buf, start + 4)?, target: read_name(buf, start + 6)?.0 },
            _ => Data::Other,
        };
        out.push(Record { name, data });
        pos = start + len;
    }
    Some(out)
}

fn instance_of(name: &str) -> Option<&str> {
    let suffix = format!(".{SERVICE}");
    let cut = name.len().checked_sub(suffix.len())?;
    (name.is_char_boundary(cut) && name[cut..].eq_ignore_ascii_case(&suffix)).then(|| &name[..cut])
}

/// Wireless debugging services in one response: instance name and address.
/// The address comes from the A record for the SRV target, or else from
/// who sent the response.
pub fn services(records: &[Record], from: Ipv4Addr) -> Vec<(String, SocketAddrV4)> {
    let mut out = Vec::new();
    for r in records {
        let Data::Srv { port, target } = &r.data else { continue };
        let Some(instance) = instance_of(&r.name) else { continue };
        let ip = records
            .iter()
            .find_map(|a| match &a.data {
                Data::A(ip) if a.name.eq_ignore_ascii_case(target) => Some(*ip),
                _ => None,
            })
            .unwrap_or(from);
        let entry = (instance.to_string(), SocketAddrV4::new(ip, *port));
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

/// Instances named by PTR records, whose SRV may not have come along.
fn pointed(records: &[Record]) -> Vec<String> {
    records
        .iter()
        .filter(|r| r.name.eq_ignore_ascii_case(SERVICE))
        .filter_map(|r| match &r.data {
            Data::Ptr(n) => instance_of(n).map(str::to_string),
            _ => None,
        })
        .collect()
}

/// Asks for Wireless debugging services: all of them, and the `instances`
/// by name. Sends to the mDNS group and to each of `hosts`, and collects
/// answers for `wait`. Instances only named in a PTR answer are asked for
/// again by name.
pub fn query(hosts: &[Ipv4Addr], instances: &[String], wait: Duration) -> Vec<(String, SocketAddrV4)> {
    let mut dests = vec![SocketAddr::V4(SocketAddrV4::new(GROUP, PORT))];
    dests.extend(hosts.iter().map(|h| SocketAddr::V4(SocketAddrV4::new(*h, PORT))));
    query_to(&dests, instances, wait)
}

fn query_to(dests: &[SocketAddr], instances: &[String], wait: Duration) -> Vec<(String, SocketAddrV4)> {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let _ = socket.set_multicast_ttl_v4(255);
    let _ = socket.set_ttl(255);

    let id: u16 = rand::random();
    let srv = |i: &str| (format!("{i}.{SERVICE}"), TYPE_SRV);
    let mut questions: Vec<(String, u16)> = vec![(SERVICE.to_string(), TYPE_PTR)];
    questions.extend(instances.iter().map(|i| srv(i)));
    send(&socket, dests, &build_query(id, &questions));

    let mut found: Vec<(String, SocketAddrV4)> = Vec::new();
    let mut asked: Vec<String> = instances.to_vec();
    let start = Instant::now();
    let mut buf = [0u8; 9000];
    while let Some(left) = wait.checked_sub(start.elapsed()).filter(|d| !d.is_zero()) {
        let _ = socket.set_read_timeout(Some(left));
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(r) => r,
            // Windows reports an ICMP "port unreachable" from an earlier send as an error here.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(_) => break,
        };
        let IpAddr::V4(from) = from.ip() else { continue };
        let Some(records) = parse_response(&buf[..n]) else { continue };
        for s in services(&records, from) {
            if !found.contains(&s) {
                found.push(s);
            }
        }
        let missing: Vec<String> = pointed(&records)
            .into_iter()
            .filter(|i| !asked.contains(i) && !found.iter().any(|(f, _)| f == i))
            .collect();
        if !missing.is_empty() {
            let q: Vec<(String, u16)> = missing.iter().map(|i| srv(i)).collect();
            send(&socket, dests, &build_query(id, &q));
            asked.extend(missing);
        }
    }
    found
}

fn send(socket: &UdpSocket, dests: &[SocketAddr], msg: &[u8]) {
    for d in dests {
        let _ = socket.send_to(msg, d);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A response like an Android phone's: PTR in the answers, SRV and A as
    /// additional records, with compressed names.
    pub fn phone_response(instance: &str, ip: Ipv4Addr, port: u16, with_srv: bool) -> Vec<u8> {
        let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, if with_srv { 2 } else { 0 }];
        // Answer: PTR _adb-tls-connect._tcp.local -> <instance>._adb-tls-connect._tcp.local
        let service_at = m.len() as u16;
        encode_name(SERVICE, &mut m);
        m.extend_from_slice(&[0, 12, 0, 1, 0, 0, 0, 120]);
        let mut rdata = vec![instance.len() as u8];
        rdata.extend_from_slice(instance.as_bytes());
        rdata.extend_from_slice(&(0xc000 | service_at).to_be_bytes());
        let instance_at = m.len() as u16 + 2;
        m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        m.extend_from_slice(&rdata);
        if with_srv {
            // SRV <instance> -> Android.local:port
            m.extend_from_slice(&(0xc000 | instance_at).to_be_bytes());
            m.extend_from_slice(&[0, 33, 0x80, 1, 0, 0, 0, 120]);
            let mut srv = vec![0, 0, 0, 0];
            srv.extend_from_slice(&port.to_be_bytes());
            let target_at = m.len() as u16 + 2 + srv.len() as u16;
            encode_name("Android.local", &mut srv);
            m.extend_from_slice(&(srv.len() as u16).to_be_bytes());
            m.extend_from_slice(&srv);
            // A Android.local
            m.extend_from_slice(&(0xc000 | target_at).to_be_bytes());
            m.extend_from_slice(&[0, 1, 0x80, 1, 0, 0, 0, 120, 0, 4]);
            m.extend_from_slice(&ip.octets());
        }
        m
    }

    #[test]
    fn query_encoding() {
        let q = build_query(7, &[(SERVICE.to_string(), TYPE_PTR)]);
        assert_eq!(&q[0..6], &[0, 7, 0, 0, 0, 1]);
        assert_eq!(&q[12..17], b"\x10_adb");
        assert_eq!(&q[q.len() - 4..], &[0, 12, 0, 1]);
        // Not a response, so it doesn't parse as one.
        assert!(parse_response(&q).is_none());
    }

    #[test]
    fn parses_phone_response() {
        let ip = Ipv4Addr::new(192, 168, 1, 20);
        let r = parse_response(&phone_response("adb-R5CT1234-AbCdEf", ip, 37891, true)).unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].data, Data::Ptr(format!("adb-R5CT1234-AbCdEf.{SERVICE}")));
        assert_eq!(r[1].data, Data::Srv { port: 37891, target: "Android.local".into() });
        let s = services(&r, Ipv4Addr::new(10, 0, 0, 9));
        assert_eq!(s, [("adb-R5CT1234-AbCdEf".to_string(), SocketAddrV4::new(ip, 37891))]);
        assert_eq!(pointed(&r), ["adb-R5CT1234-AbCdEf"]);
    }

    #[test]
    fn uses_sender_without_a_record() {
        let records = [Record {
            name: format!("adb-X-1.{SERVICE}"),
            data: Data::Srv { port: 40000, target: "Android-2.local".into() },
        }];
        let from = Ipv4Addr::new(192, 168, 1, 30);
        assert_eq!(services(&records, from), [("adb-X-1".to_string(), SocketAddrV4::new(from, 40000))]);
    }

    #[test]
    fn rejects_bad_messages() {
        assert!(parse_response(&[]).is_none());
        assert!(parse_response(&[0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0, 5, b'a']).is_none());
        // A pointer to itself.
        let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        m.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 1, 0, 4, 1, 2, 3, 4]);
        assert!(parse_response(&m).is_none());
    }

    /// A fake phone on loopback answers the unicast question; the PTR-only
    /// first answer is followed up with an SRV question.
    #[test]
    fn queries_a_host() {
        let phone = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = phone.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1500];
            let (_, from) = phone.recv_from(&mut buf).unwrap();
            phone.send_to(&phone_response("adb-R5CT1234-AbCdEf", Ipv4Addr::LOCALHOST, 41000, false), from).unwrap();
            let (n, from) = phone.recv_from(&mut buf).unwrap();
            assert!(buf[..n].windows(8).any(|w| w == b"R5CT1234"));
            phone.send_to(&phone_response("adb-R5CT1234-AbCdEf", Ipv4Addr::LOCALHOST, 41000, true), from).unwrap();
        });
        let dest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        let found = query_to(&[dest], &[], Duration::from_millis(800));
        assert_eq!(found, [("adb-R5CT1234-AbCdEf".to_string(), SocketAddrV4::new(Ipv4Addr::LOCALHOST, 41000))]);
    }
}

/// Whether real responders answer a query from an ephemeral port through
/// this PC's firewall: `cargo test -- --ignored --nocapture lan_`.
#[cfg(test)]
mod lan {
    use super::*;

    #[test]
    #[ignore]
    fn lan_mdns_answers() {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        socket.set_multicast_ttl_v4(255).unwrap();
        let q = build_query(0x1234, &[("_services._dns-sd._udp.local".into(), TYPE_PTR), (SERVICE.into(), TYPE_PTR)]);
        socket.send_to(&q, (GROUP, PORT)).unwrap();
        let start = Instant::now();
        let mut buf = [0u8; 9000];
        while start.elapsed() < Duration::from_secs(3) {
            socket.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
            let names: Vec<String> = parse_response(&buf[..n])
                .unwrap_or_default()
                .into_iter()
                .map(|r| format!("{} {:?}", r.name, r.data))
                .collect();
            println!("from {from}: {names:?}");
        }
        println!("adb services: {:?}", query(&[], &[], Duration::from_secs(2)));
    }
}
