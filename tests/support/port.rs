//! Ports for servers a test starts itself after choosing the port.

#![allow(dead_code, reason = "each test file uses part of this module")]

use std::{
    fs::{self, File},
    net::TcpListener,
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
};

/// Where test ports start: lower ones are often other software's defaults.
const FIRST: u16 = 20_000;
/// Room the tests need below the system's ephemeral range.
const ROOM: u16 = 1_000;

/// Folder of the lock files that reserve test ports for one process each.
fn locks() -> PathBuf {
    std::env::temp_dir().join("user-memory-test-ports")
}

/// Reserves `port` for this process until it exits: an exclusive lock on
/// its lock file, which another process's tests skip. `false` when another
/// process holds it.
fn reserve(port: u16) -> bool {
    static HELD: Mutex<Vec<File>> = Mutex::new(Vec::new());
    let folder = locks();
    let _ = fs::create_dir_all(&folder);
    let Ok(file) = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(folder.join(format!("{port}.lock")))
    else {
        return false;
    };
    if file.try_lock().is_err() {
        return false;
    }
    HELD.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(file);
    true
}

/// A port nothing listens on, for a server a test starts after choosing
/// it. A test running beside it cannot take it in between: the port is
/// below the range the system hands out by itself (to a connection's own
/// end or a listener on port 0), this process gives each port once, and a
/// lock file reserves it from other processes until this one exits, as
/// when every test runs in a process of its own. Each process starts at its
/// own place in the range; a port reserved elsewhere or already in use is
/// skipped.
pub fn closed_port() -> u16 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let end = *ephemeral_range().start();
    assert!(
        end >= FIRST + ROOM,
        "the system hands out ports from {end}, leaving tests no room above {FIRST}"
    );
    let span = u32::from(end - FIRST);
    // The tests of this module make two processes start at one place.
    let start = std::env::var("USER_MEMORY_TEST_PORT_START")
        .ok()
        .and_then(|start| start.parse::<u32>().ok())
        .unwrap_or_else(std::process::id)
        % span;
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(n < span, "every test port from {FIRST} to {end} is in use");
        let port = FIRST + u16::try_from((start + n) % span).unwrap();
        if reserve(port) && TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// The ports the system hands out by itself: macOS
/// `net.inet.ip.portrange.first`..`last`, Linux
/// `net.ipv4.ip_local_port_range`, Windows `netsh int ipv4 show dynamicport
/// tcp`.
pub fn ephemeral_range() -> std::ops::RangeInclusive<u16> {
    static RANGE: OnceLock<std::ops::RangeInclusive<u16>> = OnceLock::new();
    RANGE.get_or_init(read_ephemeral_range).clone()
}

fn read_ephemeral_range() -> std::ops::RangeInclusive<u16> {
    let numbers = |text: &str| -> Vec<u16> {
        text.split_whitespace()
            .map(|number| number.parse().unwrap())
            .collect()
    };
    if cfg!(windows) {
        let out = std::process::Command::new("netsh")
            .args(["int", "ipv4", "show", "dynamicport", "tcp"])
            .output()
            .ok();
        out.and_then(|out| windows_range(&String::from_utf8_lossy(&out.stdout)))
            .unwrap_or(WINDOWS_DEFAULT)
    } else if cfg!(target_os = "linux") {
        let range =
            numbers(&std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap());
        range[0]..=range[1]
    } else {
        let read = |name: &str| {
            let out = std::process::Command::new("sysctl")
                .args(["-n", name])
                .output()
                .unwrap();
            numbers(&String::from_utf8_lossy(&out.stdout))[0]
        };
        read("net.inet.ip.portrange.first")..=read("net.inet.ip.portrange.last")
    }
}

/// Windows' dynamic port range unless changed (RFC 6335's), for when netsh
/// cannot be read.
pub const WINDOWS_DEFAULT: std::ops::RangeInclusive<u16> = 49152..=65535;

/// The range in netsh's report: its `Start Port` and `Number of Ports`
/// lines, each `<label> : <number>`. None when either is missing.
pub fn windows_range(report: &str) -> Option<std::ops::RangeInclusive<u16>> {
    let value = |label: &str| -> Option<u32> {
        report.lines().find_map(|line| {
            let (name, number) = line.split_once(':')?;
            (name.trim() == label).then(|| number.trim().parse().ok())?
        })
    };
    let (start, count) = (value("Start Port")?, value("Number of Ports")?);
    let last = start.checked_add(count.checked_sub(1)?)?;
    Some(u16::try_from(start).ok()?..=u16::try_from(last).ok()?)
}
