//! Test ports stay apart across processes, as when every test runs in a
//! process of its own.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/port.rs"]
mod port;
#[path = "support/time.rs"]
mod time;

use std::{
    collections::BTreeSet,
    io::{BufRead, BufReader},
    process::{Command, Stdio},
};

/// Ports each process takes at once.
const EACH: usize = 20;
/// Set in the child processes: the folder whose `release` file lets them end.
const CHILD: &str = "USER_MEMORY_TEST_PORT_CHILD";

// Run as a child of the test below: takes its ports, prints them, and
// keeps them until the parent says so. On its own it does nothing.
#[test]
fn child_takes_ports() {
    let Some(folder) = std::env::var_os(CHILD) else {
        return;
    };
    let ports: Vec<String> = (0..EACH).map(|_| port::closed_port().to_string()).collect();
    // On a line of its own: the test harness prints the test's name first.
    println!("\nport-set {}", ports.join(" "));
    let release = std::path::Path::new(&folder).join("release");
    let deadline = std::time::Instant::now() + time::STUCK;
    while !release.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// Two processes starting at the same place in the range, both holding
// their ports, never get the same one.
#[test]
fn processes_side_by_side_never_share_a_port() {
    let folder = tempfile::tempdir().unwrap();
    let children: Vec<_> = (0..2)
        .map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "child_takes_ports",
                    "--nocapture",
                    "--test-threads",
                    "1",
                ])
                .env(CHILD, folder.path())
                .env("USER_MEMORY_TEST_PORT_START", "7")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut sets: Vec<BTreeSet<u16>> = Vec::new();
    let mut running = Vec::new();
    for mut child in children {
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = lines
            .by_ref()
            .map(Result::unwrap)
            .find(|line| line.starts_with("port-set "))
            .unwrap();
        sets.push(
            line["port-set ".len()..]
                .split(' ')
                .map(|port| port.parse().unwrap())
                .collect(),
        );
        running.push((child, lines));
    }
    std::fs::write(folder.path().join("release"), "").unwrap();
    for (mut child, lines) in running {
        // Read to the end, so the child never writes into a closed pipe.
        lines.for_each(drop);
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(sets[0].len(), EACH);
    assert_eq!(sets[1].len(), EACH);
    let shared: Vec<_> = sets[0].intersection(&sets[1]).collect();
    assert!(shared.is_empty(), "both processes got {shared:?}");
}

// Windows reports its range as a start and a count, in a table netsh prints
// for each protocol; anything else falls back to the default.
#[test]
fn the_windows_range_is_read_from_netsh() {
    let report = "\r\nProtocol tcp Dynamic Port Range\r\n---------------------------------\r\nStart Port      : 49152\r\nNumber of Ports : 16384\r\n";
    assert_eq!(port::windows_range(report), Some(49152..=65535));
    let moved = "Start Port      : 10000\nNumber of Ports : 1000\n";
    assert_eq!(port::windows_range(moved), Some(10000..=10999));
    for broken in [
        "",
        "Start Port      : 49152\n",
        "Number of Ports : 16384\n",
        "Start Port      : x\nNumber of Ports : 16384\n",
        "Start Port      : 49152\nNumber of Ports : 0\n",
        "Start Port      : 60000\nNumber of Ports : 16384\n",
    ] {
        assert_eq!(port::windows_range(broken), None, "{broken:?}");
    }
    assert_eq!(port::WINDOWS_DEFAULT, 49152..=65535);
}
