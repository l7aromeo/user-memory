//! GitHub Releases in miniature, served over loopback for the installer
//! tests: this checkout's install.sh and this build's binary as the release
//! assets.

#![allow(dead_code, reason = "each test file uses part of this module")]

use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};
use tempfile::{TempDir, tempdir};

/// The release tag the fake server serves under download/.
pub const TAG: &str = "v9.9.9";

/// The client build this machine installs.
pub fn native_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        other => panic!("no client target for {other:?}"),
    }
}

fn repo_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

/// The SHA-256 of `path` in hex, as the release's `.sha256` files give it.
pub fn sha256(path: &Path) -> String {
    let out = if cfg!(windows) {
        Command::new("certutil")
            .args(["-hashfile"])
            .arg(path)
            .arg("SHA256")
            .output()
    } else if Path::new("/usr/bin/shasum").exists() {
        Command::new("shasum")
            .args(["-a", "256"])
            .arg(path)
            .output()
    } else {
        Command::new("sha256sum").arg(path).output()
    }
    .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    // certutil prints a header line first, and older versions space the bytes.
    let hex: String = if cfg!(windows) {
        text.lines()
            .nth(1)
            .unwrap_or_default()
            .split_whitespace()
            .collect()
    } else {
        text[..64].to_owned()
    };
    hex.to_lowercase()
}

/// GitHub Releases in miniature: `<base>/latest/download/<asset>` and
/// `<base>/download/<tag>/<asset>`, served from a directory over loopback.
pub struct Releases {
    dir: TempDir,
    pub base: String,
    requests: Arc<Mutex<Vec<String>>>,
    flaky: Arc<Mutex<HashSet<String>>>,
}

impl Releases {
    pub fn start() -> Self {
        let dir = tempdir().unwrap();
        let target = native_target();
        for layout in ["latest/download".to_owned(), format!("download/{TAG}")] {
            let assets = dir.path().join(&layout);
            fs::create_dir_all(&assets).unwrap();
            fs::copy(repo_file("install.sh"), assets.join("install.sh")).unwrap();
            let binary = assets.join(format!("user-memory-{target}"));
            fs::copy(env!("CARGO_BIN_EXE_user-memory"), &binary).unwrap();
            fs::write(
                assets.join(format!("user-memory-{target}.sha256")),
                format!("{}  user-memory-{target}\n", sha256(&binary)),
            )
            .unwrap();
            // Server builds for Docker, which the fake docker never runs. On a
            // Linux machine one of them is the client build copied above.
            for linux in ["aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"]
                .into_iter()
                .filter(|linux| *linux != target)
            {
                let server = assets.join(format!("user-memory-{linux}"));
                fs::write(&server, format!("linux build {linux}")).unwrap();
                fs::write(
                    assets.join(format!("user-memory-{linux}.sha256")),
                    format!("{}  user-memory-{linux}\n", sha256(&server)),
                )
                .unwrap();
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!(
            "http://{}/l7aromeo/user-memory/releases",
            listener.local_addr().unwrap()
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let flaky = Arc::new(Mutex::new(HashSet::new()));
        let (root, log, broken) = (dir.path().to_path_buf(), requests.clone(), flaky.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (root, log, broken) = (root.clone(), log.clone(), broken.clone());
                std::thread::spawn(move || serve(stream, &root, &log, &broken));
            }
        });
        Self {
            dir,
            base,
            requests,
            flaky,
        }
    }
    /// Cuts the next transfer of `name` under `layout` off halfway, as a
    /// CDN that resets the stream does.
    pub fn drop_once(&self, layout: &str, name: &str) {
        let path = format!("/l7aromeo/user-memory/releases/{layout}/{name}");
        self.flaky.lock().unwrap().insert(path);
    }
    /// Serves this machine's build as every target's, for an installer told
    /// it runs on another system.
    pub fn native_build_for_every_target(&self) {
        for target in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
        ] {
            let asset = self.asset("latest/download", &format!("user-memory-{target}"));
            fs::copy(env!("CARGO_BIN_EXE_user-memory"), &asset).unwrap();
            fs::write(
                self.asset("latest/download", &format!("user-memory-{target}.sha256")),
                format!("{}  user-memory-{target}\n", sha256(&asset)),
            )
            .unwrap();
        }
    }
    pub fn asset(&self, layout: &str, name: &str) -> PathBuf {
        self.dir.path().join(layout).join(name)
    }
    pub fn requested(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn serve(
    mut stream: std::net::TcpStream,
    root: &Path,
    log: &Mutex<Vec<String>>,
    flaky: &Mutex<HashSet<String>>,
) {
    let mut request = Vec::new();
    let mut byte = [0; 1];
    while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
        request.push(byte[0]);
    }
    let request = String::from_utf8_lossy(&request);
    let path = request.split(' ').nth(1).unwrap_or("").to_owned();
    log.lock().unwrap().push(path.clone());
    let file = path
        .strip_prefix("/l7aromeo/user-memory/releases/")
        .filter(|rest| !rest.contains(".."))
        .map(|rest| root.join(rest));
    let dropped = flaky.lock().unwrap().remove(&path);
    let response = match file.and_then(|file| fs::read(file).ok()) {
        Some(body) if dropped => [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes(),
            body[..body.len() / 2].to_vec(),
        ]
        .concat(),
        Some(body) => [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes(),
            body,
        ]
        .concat(),
        None => {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
    };
    let _ = stream.write_all(&response);
}
