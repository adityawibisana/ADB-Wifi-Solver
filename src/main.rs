mod adb;
mod console;
mod qr;

use adb::Adb;
use rand::{distributions::Alphanumeric, Rng};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::net::UdpSocket;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(500);
const MAX_ATTEMPTS: u32 = 3;
const FIREWALL_HINT_AFTER: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

const HELP: &str = "\
adbqr - pair and connect an Android phone to ADB over Wi-Fi by scanning a QR code

USAGE: adbqr [options]

On the phone: Developer options > Wireless debugging > Pair device with QR code.

OPTIONS:
  --timeout <sec>  Seconds to wait for the phone to scan each QR code (default 120)
  --adb <path>     Path to adb or its folder (default: PATH, then the Android SDK)
  --keep-open      Wait for Enter before exiting
  --invert         Invert the QR colors (only with --no-color on a light terminal)
  --no-color       Don't force white-on-black colors with ANSI escapes
  --verbose        Print every adb command and its output
  -h, --help       Show this help

EXIT CODES: 0 connected, 1 adb not found, 2 timeout, 3 pairing failed,
            4 connect failed, 5 bad arguments, 6 other error";

struct Args {
    timeout: Duration,
    adb: Option<PathBuf>,
    keep_open: bool,
    invert: bool,
    color: bool,
    verbose: bool,
}

enum Fail {
    AdbNotFound(String),
    Timeout(String),
    PairFailed(String),
    ConnectFailed(String),
    BadArgs(String),
    Other(String),
}

impl Fail {
    fn code(&self) -> i32 {
        match self {
            Fail::AdbNotFound(_) => 1,
            Fail::Timeout(_) => 2,
            Fail::PairFailed(_) => 3,
            Fail::ConnectFailed(_) => 4,
            Fail::BadArgs(_) => 5,
            Fail::Other(_) => 6,
        }
    }

    fn message(&self) -> &str {
        match self {
            Fail::AdbNotFound(m)
            | Fail::Timeout(m)
            | Fail::PairFailed(m)
            | Fail::ConnectFailed(m)
            | Fail::BadArgs(m)
            | Fail::Other(m) => m,
        }
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let keep_open = raw.iter().any(|a| a == "--keep-open");

    let code = match parse_args(&raw) {
        Ok(None) => {
            println!("{HELP}");
            0
        }
        Ok(Some(args)) => match run(&args) {
            Ok(()) => 0,
            Err(f) => report(f),
        },
        Err(f) => report(f),
    };

    if keep_open {
        print!("\nPress Enter to exit...");
        let _ = io::stdout().flush();
        let _ = io::stdin().lock().read_line(&mut String::new());
    }
    std::process::exit(code);
}

fn report(f: Fail) -> i32 {
    eprintln!("\nError: {}", f.message());
    f.code()
}

fn parse_args(raw: &[String]) -> Result<Option<Args>, Fail> {
    let mut args = Args {
        timeout: Duration::from_secs(120),
        adb: None,
        keep_open: false,
        invert: false,
        color: true,
        verbose: false,
    };
    let mut it = raw.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" | "/?" => return Ok(None),
            "--timeout" => {
                let v = it.next().ok_or_else(|| Fail::BadArgs("--timeout needs a value".into()))?;
                let secs: u64 = v
                    .parse()
                    .ok()
                    .filter(|s| *s > 0)
                    .ok_or_else(|| Fail::BadArgs(format!("invalid --timeout value: {v}")))?;
                args.timeout = Duration::from_secs(secs);
            }
            "--adb" => {
                let v = it.next().ok_or_else(|| Fail::BadArgs("--adb needs a path".into()))?;
                args.adb = Some(PathBuf::from(v));
            }
            "--keep-open" => args.keep_open = true,
            "--invert" => args.invert = true,
            "--no-color" => args.color = false,
            "--verbose" => args.verbose = true,
            other => return Err(Fail::BadArgs(format!("unknown option: {other} (see --help)"))),
        }
    }
    Ok(Some(args))
}

fn run(args: &Args) -> Result<(), Fail> {
    let path = adb::locate(args.adb.as_deref()).map_err(Fail::AdbNotFound)?;
    let adb = Adb::new(path, args.verbose);
    println!("Using adb: {}", adb.path().display());

    let started = adb.run(&["start-server"], Duration::from_secs(30)).map_err(Fail::Other)?;
    if !started.success {
        return Err(Fail::Other(format!("adb start-server failed: {}", started.text.trim())));
    }
    match adb.run(&["mdns", "check"], Duration::from_secs(10)) {
        Ok(out) if out.text.contains("mdns daemon version") => {}
        _ => eprintln!(
            "Warning: adb reports mDNS is unavailable, so the phone may not be found. \
             Update platform-tools to version 34 or newer."
        ),
    }

    let (phone_ip, guid) = pair(&adb, args)?;
    let serial = connect(&adb, &phone_ip, guid.as_deref())?;

    let model = adb
        .run(&["-s", &serial, "shell", "getprop", "ro.product.model"], Duration::from_secs(10))
        .map(|o| o.text.trim().to_string())
        .unwrap_or_default();
    if model.is_empty() {
        println!("\nConnected: {serial}");
    } else {
        println!("\nConnected: {serial} ({model})");
    }
    Ok(())
}

/// Shows QR codes until one pairs, then returns the phone's IP and guid.
fn pair(adb: &Adb, args: &Args) -> Result<(String, Option<String>), Fail> {
    let mut last_error = String::new();
    for attempt in 1..=MAX_ATTEMPTS {
        let name = format!("adbqr-{}", random_alnum(8));
        let password = random_alnum(10);
        show_qr(&qr::pairing_payload(&name, &password), args)?;
        println!(
            "Waiting for the phone to scan (attempt {attempt}/{MAX_ATTEMPTS}, {}s)...",
            args.timeout.as_secs()
        );

        let addr = wait_for_pairing_service(adb, &name, args.timeout)?;
        let ip = adb::host_of(&addr).to_string();
        println!("Found phone at {addr}, pairing...");

        match adb.run(&["pair", &addr, &password], Duration::from_secs(30)) {
            Ok(out) if adb::pair_succeeded(&out.text) => {
                println!("Paired.");
                return Ok((ip, adb::pair_guid(&out.text)));
            }
            Ok(out) => last_error = out.text.trim().to_string(),
            Err(e) => last_error = e,
        }
        eprintln!("Pairing failed: {last_error}");
        if attempt < MAX_ATTEMPTS {
            println!("Generating a new QR code...\n");
        }
    }
    Err(Fail::PairFailed(format!("pairing failed after {MAX_ATTEMPTS} attempts: {last_error}")))
}

/// Polls mDNS for the pairing service the phone advertises after the scan.
fn wait_for_pairing_service(adb: &Adb, name: &str, timeout: Duration) -> Result<String, Fail> {
    let start = Instant::now();
    let mut hinted = false;
    loop {
        if let Ok(services) = adb.mdns_services() {
            if let Some(s) = services
                .iter()
                .find(|s| s.instance == name && s.service_type.contains("_adb-tls-pairing"))
            {
                return Ok(s.addr.clone());
            }
        }
        let elapsed = start.elapsed();
        if !hinted && elapsed > FIREWALL_HINT_AFTER {
            hinted = true;
            println!(
                "Still waiting. If you already scanned, check that your firewall allows \
                 adb on this network (mDNS uses UDP 5353)."
            );
        }
        if elapsed > timeout {
            return Err(Fail::Timeout(timeout_hint(timeout)));
        }
        sleep(POLL);
    }
}

/// Waits for the paired phone to appear in `adb devices`, connecting it if adb doesn't.
fn connect(adb: &Adb, ip: &str, guid: Option<&str>) -> Result<String, Fail> {
    println!("Waiting for the phone's connection service...");
    let matches = |serial: &str| {
        serial.starts_with(&format!("{ip}:")) || guid.is_some_and(|g| serial.contains(g))
    };
    let start = Instant::now();
    let mut tried = HashSet::new();
    let mut last_error = String::from("the phone never advertised its connection service");

    loop {
        if let Ok(devices) = adb.devices() {
            if let Some(d) = devices.iter().find(|d| d.state == "device" && matches(&d.serial)) {
                return Ok(d.serial.clone());
            }
        }
        if let Ok(services) = adb.mdns_services() {
            for s in services.iter().filter(|s| {
                s.service_type.contains("_adb-tls-connect")
                    && (adb::host_of(&s.addr) == ip || guid == Some(s.instance.as_str()))
            }) {
                if tried.insert(s.addr.clone()) {
                    println!("Connecting to {}...", s.addr);
                    match adb.run(&["connect", &s.addr], Duration::from_secs(15)) {
                        Ok(out) if out.text.contains("connected to") => {}
                        Ok(out) => last_error = out.text.trim().to_string(),
                        Err(e) => last_error = e,
                    }
                }
            }
        }
        if start.elapsed() > CONNECT_TIMEOUT {
            return Err(Fail::ConnectFailed(format!("paired, but could not connect: {last_error}")));
        }
        sleep(POLL);
    }
}

fn show_qr(payload: &str, args: &Args) -> Result<(), Fail> {
    let ansi = args.color && console::enable_ansi();
    let rendered = qr::render(payload, ansi, args.invert && !ansi).map_err(Fail::Other)?;

    println!();
    print!("{}", rendered.text);
    println!("Scan on the phone: Developer options > Wireless debugging > Pair device with QR code");

    if let Some((cols, rows)) = console::window_size() {
        let needed_rows = rendered.columns.div_ceil(2);
        if cols < rendered.columns || rows < needed_rows {
            eprintln!(
                "Warning: the console window is too small to show the whole QR code \
                 (needs {}x{}). Enlarge the window and run again if it won't scan.",
                rendered.columns, needed_rows
            );
        }
    }
    Ok(())
}

fn timeout_hint(timeout: Duration) -> String {
    let local = local_ip().map(|ip| format!(" (this PC: {ip})")).unwrap_or_default();
    format!(
        "no phone responded within {}s. Check that:\n  \
         - the phone and this PC are on the same Wi-Fi network{local}\n  \
         - your firewall allows adb on this network (mDNS, UDP 5353)\n  \
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
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}
