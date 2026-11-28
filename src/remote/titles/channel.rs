//! The exchange between a hook and the connector that answers title
//! lookups (#292): one line each way over a TCP connection to 127.0.0.1.
//! The connector names its port and a fresh key in an endpoint file only
//! this user can read (`private.rs`). Each line is sealed under that key
//! with ChaCha20-Poly1305, so no one else on the machine reads what it
//! asks or answers (project keys, memory ids, titles), even with access to
//! loopback traffic, and no one else can ask (the connector drops a request
//! that doesn't open) or answer (the hook drops such an answer and names
//! the id alone, as before). The key never crosses the socket. Before it
//! connects, a hook checks that a connector holds the titles lock, so a
//! port a killed owner left in the endpoint file, which another program
//! may hold by now, is never sent a request. Another local user can still
//! fill the connector's [`super::owner`] slots for connections, which
//! costs the hooks their titles, never more: they name ids alone.

use crate::{memory::Position, private};
use aws_lc_rs::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::Path,
    time::{Duration, Instant},
};

/// The longest a hook waits for titles, within its own deadline: a lookup
/// is one loopback round trip (0.14 ms median, 4.5 ms the slowest of 2,000
/// on macOS), so 25 ms leaves a loaded machine room while one stuck
/// connector costs a hook little. A late answer names the id alone.
pub(crate) const TITLE_WAIT: Duration = Duration::from_millis(25);

/// What a lookup leaves of its hook's deadline for the work after it, so a
/// hook whose lookup runs out still ends within its limit: the OS can end
/// a read's wait one timer tick late (15.6 ms on Windows; 1.7 ms at the
/// median, 2.1 ms the most of 200 on macOS), and the work after is writing
/// the session's state and the push, which the delivery bench times at
/// 2.7 ms at 10,000 memories on macOS. A later end costs the hook its limit,
/// never its lines: the hook checks its deadline before the lookup, not
/// after.
const ROOM_AFTER_LOOKUP: Duration = Duration::from_millis(20);

/// [`TITLE_WAIT`], which debug builds take in milliseconds from
/// `USER_MEMORY_TEST_TITLE_WAIT_MS`, so tests can see a lookup that would
/// run to its hook's deadline.
fn title_wait() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("USER_MEMORY_TEST_TITLE_WAIT_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    TITLE_WAIT
}

/// When a lookup started `now`, for a hook whose time is up at
/// `deadline`, gives up: [`title_wait`] after it starts, and never later
/// than [`ROOM_AFTER_LOOKUP`] before the deadline. None when that has
/// passed already.
fn lookup_until(now: Instant, deadline: Instant) -> Option<Instant> {
    let until = deadline
        .checked_sub(ROOM_AFTER_LOOKUP)?
        .min(now + title_wait());
    (until > now).then_some(until)
}

/// Bytes of the key a connector makes for each time it starts answering:
/// ChaCha20-Poly1305's key size.
const KEY_BYTES: usize = 32;

/// The longest request line the connector reads: [`LOOKUP_IDS`] ids and a
/// scope's projects come to about 22 KB sealed, in hex.
pub(super) const REQUEST_BYTES: usize = 32 * 1024;

/// The most ids one lookup asks for: a working set's, each about 40 bytes
/// in the request, so a lookup's request stays well under
/// [`REQUEST_BYTES`]; ids past it are named alone.
const LOOKUP_IDS: usize = 256;

/// The longest answer line a hook reads: [`LOOKUP_IDS`] titles of at most
/// 120 characters with their fields come to about 150 KB sealed, in hex.
const ANSWER_BYTES: usize = 512 * 1024;

/// What a request is sealed with as associated data, so one is never
/// taken for an answer.
const REQUEST_LABEL: &[u8] = b"request";
/// What an answer is sealed with, before its request's nonce: it opens
/// only as the answer to that request.
const ANSWER_LABEL: &[u8] = b"answer";

/// The key a connector seals with.
pub(super) type Key = LessSafeKey;

/// The endpoint file's content: where the connector listens, the key, in
/// hex, and its process.
#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Endpoint {
    pub(super) port: u16,
    pub(super) key: String,
    pub(super) pid: u32,
}

/// What a hook asks: the titles of `ids` in the scope `scope`, current at
/// least to the position `at` of the delivery files it matched.
#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Request {
    pub(super) scope: Vec<String>,
    pub(super) at: String,
    pub(super) ids: Vec<String>,
}

/// A memory as a title line names it: its title, and its source and type,
/// which say how the line marks it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Named {
    pub(crate) title: String,
    pub(crate) source: String,
    #[serde(rename = "type")]
    pub(crate) memory_type: String,
}

/// The connector's answer: the position its titles are current to, and
/// each asked memory it holds then. An asked id it doesn't hold is gone
/// from the scope at that position.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(super) struct Answer {
    pub(super) at: String,
    pub(super) titles: BTreeMap<String, Named>,
}

/// The titles a hook got, current at least to the position it asked for:
/// a memory not here is gone from its scope.
#[derive(Debug, Default)]
pub(crate) struct Titles(BTreeMap<String, Named>);

impl Titles {
    /// Titles of tests' own.
    #[cfg(test)]
    pub(crate) fn of(titles: impl IntoIterator<Item = (String, Named)>) -> Self {
        Self(titles.into_iter().collect())
    }

    /// Memory `id`'s title and fields; none when it is gone from the scope.
    pub(crate) fn get(&self, id: &str) -> Option<&Named> {
        self.0.get(id)
    }
}

/// The key whose bytes are `bytes`; none when they are not a key's length.
fn key(bytes: &[u8]) -> Option<Key> {
    UnboundKey::new(&CHACHA20_POLY1305, bytes)
        .ok()
        .map(LessSafeKey::new)
}

/// A new key, and its bytes in hex for the endpoint file.
pub(super) fn new_key() -> (Key, String) {
    let bytes: [u8; KEY_BYTES] = rand::random();
    let key = key(&bytes).expect("KEY_BYTES is ChaCha20-Poly1305's key length");
    (key, hex(&bytes))
}

/// `body` sealed under `key` with `aad`, as a line: a fresh random nonce
/// and the sealed bytes, in hex, then a line break; and the nonce. A key
/// lives for one connector's turn and seals a few lines a hook, so 96
/// random bits never repeat a nonce under it.
fn seal(key: &Key, aad: &[u8], body: &[u8]) -> ([u8; NONCE_LEN], String) {
    let nonce: [u8; NONCE_LEN] = rand::random();
    let mut sealed = body.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(aad),
        &mut sealed,
    )
    .expect("a line is far below ChaCha20-Poly1305's length limit");
    (nonce, format!("{} {}\n", hex(&nonce), hex(&sealed)))
}

/// The nonce and body of `line`, when it opens under `key` with `aad`.
fn open(key: &Key, aad: &[u8], line: &str) -> Option<([u8; NONCE_LEN], Vec<u8>)> {
    let (nonce, sealed) = line.trim_end().split_once(' ')?;
    let nonce: [u8; NONCE_LEN] = unhex(nonce)?.try_into().ok()?;
    let mut sealed = unhex(sealed)?;
    let body = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut sealed,
        )
        .ok()?;
    Some((nonce, body.to_vec()))
}

/// What an answer to the request of `nonce` is sealed with.
fn answer_aad(nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    [ANSWER_LABEL, nonce.as_slice()].concat()
}

/// A request line for `request`, and its nonce, which its answer is bound
/// to.
fn request_line(key: &Key, request: &Request) -> Option<([u8; NONCE_LEN], String)> {
    Some(seal(key, REQUEST_LABEL, &serde_json::to_vec(request).ok()?))
}

/// The nonce and request in `line`, when it opens under this key as a
/// request.
pub(super) fn read_request(key: &Key, line: &str) -> Option<([u8; NONCE_LEN], Request)> {
    let (nonce, body) = open(key, REQUEST_LABEL, line)?;
    Some((nonce, serde_json::from_slice(&body).ok()?))
}

/// An answer line for the request of `nonce`.
pub(super) fn answer_line(key: &Key, nonce: &[u8; NONCE_LEN], answer: &Answer) -> String {
    let body = serde_json::to_vec(answer).unwrap_or_default();
    seal(key, &answer_aad(nonce), &body).1
}

/// The answer in `line` to the request of `nonce`, when it opens under this
/// key as that request's answer.
fn read_answer(key: &Key, nonce: &[u8; NONCE_LEN], line: &str) -> Option<Answer> {
    let (_, body) = open(key, &answer_aad(nonce), line)?;
    serde_json::from_slice(&body).ok()
}

/// Whether a connector holds the titles lock at `lock`, which the system
/// lets go when it exits or is killed. Tried without waiting: a lock had
/// here is let go at once, so a connector whose take meets this check
/// takes the lock at its next tick.
fn answering(lock: &Path) -> bool {
    let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(lock) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            // Windows lets a lock go on close only as resources allow, so a
            // connector's take could wait past its tick; unlocking says so
            // now. A failed unlock changes nothing: the file closes anyway.
            let _ = file.unlock();
            false
        }
        Err(fs::TryLockError::WouldBlock) => true,
        Err(fs::TryLockError::Error(_)) => false,
    }
}

/// Writes `endpoint` to `path` whole, readable only by this user: a hook
/// never reads half.
pub(super) fn write_endpoint(path: &Path, endpoint: &Endpoint) -> io::Result<()> {
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    private::create_dir(dir)?;
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}", std::process::id()));
    let temporary = std::path::PathBuf::from(name);
    let written = (|| {
        let mut file = private::create_file(
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
            &temporary,
        )?;
        file.write_all(serde_json::to_string(endpoint)?.as_bytes())?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// For a hook: the titles of `ids` in the scope `scope` of the server at
/// `url`, from the connector that answers title lookups, within
/// [`TITLE_WAIT`] and leaving [`ROOM_AFTER_LOOKUP`] of `deadline`; none
/// when no connector holds the titles lock, none answers in time, its
/// answer doesn't open under the key, or its titles are behind `at`, the
/// position of the delivery files the hook matched. The hook then names
/// the ids alone. The OS can end the wait past `deadline`, so a hook checks
/// its deadline before the lookup, never after: a late end costs only the
/// titles.
pub(crate) fn lookup(
    url: &str,
    scope: &[String],
    at: &Position,
    ids: &[String],
    deadline: Instant,
) -> Option<Titles> {
    let titles = ask(url, scope, at, ids, deadline);
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var(LOOKUP_OVERRUN)
        .ok()
        .and_then(|ms| ms.parse().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
    titles
}

/// Makes a debug build's lookup return this many milliseconds late, as
/// when the OS ends a read's wait past its timeout, so tests see a lookup
/// end past its hook's deadline on any OS.
#[cfg(debug_assertions)]
const LOOKUP_OVERRUN: &str = "USER_MEMORY_TEST_LOOKUP_OVERRUN_MS";

/// [`lookup`], without a debug build's overrun.
fn ask(
    url: &str,
    scope: &[String],
    at: &Position,
    ids: &[String],
    deadline: Instant,
) -> Option<Titles> {
    let until = lookup_until(Instant::now(), deadline)?;
    let files = crate::remote::marker::titles_files(url)?;
    if !answering(&files.lock) {
        return None;
    }
    let endpoint: Endpoint = serde_json::from_slice(&fs::read(&files.endpoint).ok()?).ok()?;
    let key = key(&unhex(&endpoint.key)?)?;
    let request = Request {
        scope: scope.to_vec(),
        at: at.as_str().to_owned(),
        ids: ids.iter().take(LOOKUP_IDS).cloned().collect(),
    };
    let (nonce, line) = request_line(&key, &request)?;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
    let mut stream = TcpStream::connect_timeout(&address, left(until)?).ok()?;
    stream.set_nodelay(true).ok()?;
    stream.set_write_timeout(Some(left(until)?)).ok()?;
    stream.write_all(line.as_bytes()).ok()?;
    let mut answer = Vec::new();
    let mut chunk = [0u8; 4096];
    while !answer.ends_with(b"\n") {
        stream.set_read_timeout(Some(left(until)?)).ok()?;
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 || answer.len() + read > ANSWER_BYTES {
            return None;
        }
        answer.extend_from_slice(&chunk[..read]);
    }
    let answer = read_answer(&key, &nonce, std::str::from_utf8(&answer).ok()?)?;
    at.within(&Position::kept(answer.at))
        .then_some(Titles(answer.titles))
}

/// The time left until `until`; none once it has passed, since a zero
/// timeout means none at all to the socket.
fn left(until: Instant) -> Option<Duration> {
    let left = until.saturating_duration_since(Instant::now());
    (!left.is_zero()).then_some(left)
}

/// `bytes` in lowercase hex.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

/// The bytes the hex `text` spells; none for anything else.
fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digit = |b: u8| char::from(b).to_digit(16);
            u8::try_from(digit(pair[0])? * 16 + digit(pair[1])?).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked() -> Request {
        Request {
            scope: vec!["github.com/example/app".into()],
            at: "1.0123456789abcdef.7.0".into(),
            ids: vec!["e0000001-0000-4000-8000-000000000001".into()],
        }
    }

    /// The hex of `line`'s sealed part with its first byte changed.
    fn tampered(line: &str) -> String {
        let (nonce, sealed) = line.trim_end().split_once(' ').unwrap();
        let mut bytes = unhex(sealed).unwrap();
        bytes[0] ^= 1;
        format!("{nonce} {}\n", hex(&bytes))
    }

    /// A request and an answer each open under their key, and neither
    /// opens with another key, another nonce, a changed body, as the other
    /// direction, or (an answer) for another request. Neither line holds
    /// what it carries in the clear.
    #[test]
    fn a_line_opens_only_under_its_key_nonce_body_and_direction() {
        let (key, _) = new_key();
        let (other, _) = new_key();
        let (nonce, line) = request_line(&key, &asked()).unwrap();
        let (read_nonce, request) = read_request(&key, &line).unwrap();
        assert_eq!(read_nonce, nonce);
        assert_eq!(request.ids, asked().ids);
        assert!(
            !line.contains("example") && !line.contains("e0000001"),
            "{line}"
        );
        assert!(read_request(&other, &line).is_none(), "another key");
        assert!(
            read_request(&key, &tampered(&line)).is_none(),
            "a changed body"
        );
        let renonced = format!("{}{}", hex(&[8u8; NONCE_LEN]), &line[NONCE_LEN * 2..]);
        assert!(read_request(&key, &renonced).is_none(), "another nonce");

        let answer = Answer {
            at: "1.0123456789abcdef.7.0".into(),
            titles: BTreeMap::from([(
                "e0000001-0000-4000-8000-000000000001".to_owned(),
                Named {
                    title: "Publish on Tuesdays".into(),
                    source: "explicit".into(),
                    memory_type: "decision".into(),
                },
            )]),
        };
        let line = answer_line(&key, &nonce, &answer);
        assert!(!line.contains("Tuesdays"), "{line}");
        assert_eq!(
            read_answer(&key, &nonce, &line).unwrap().titles,
            answer.titles
        );
        assert!(read_answer(&other, &nonce, &line).is_none(), "another key");
        assert!(
            read_answer(&key, &[8u8; NONCE_LEN], &line).is_none(),
            "another request"
        );
        assert!(
            read_answer(&key, &nonce, &tampered(&line)).is_none(),
            "a changed body"
        );
        // A request's line is no answer, and an answer's no request.
        let (request_nonce, request) = request_line(&key, &asked()).unwrap();
        assert!(
            read_answer(&key, &request_nonce, &request).is_none(),
            "a request as an answer"
        );
        assert!(
            read_request(&key, &line).is_none(),
            "an answer as a request"
        );
    }

    /// A lookup gives up [`TITLE_WAIT`] after it starts, or earlier, to leave
    /// [`ROOM_AFTER_LOOKUP`] of its hook's deadline; with less left, it
    /// asks nothing.
    #[test]
    fn a_lookup_leaves_its_hook_room_for_the_push() {
        let now = Instant::now();
        let far = now + Duration::from_secs(10);
        assert_eq!(lookup_until(now, far), Some(now + TITLE_WAIT));
        let near = now + ROOM_AFTER_LOOKUP + Duration::from_millis(5);
        assert_eq!(lookup_until(now, near), Some(near - ROOM_AFTER_LOOKUP));
        assert_eq!(lookup_until(now, now + ROOM_AFTER_LOOKUP), None);
        assert_eq!(lookup_until(now, now), None);
    }

    /// A lock no connector holds says no one answers, and the check lets it
    /// go at once, so a connector takes it right after; a held one says a
    /// connector answers. No lock file says no one does.
    #[test]
    fn a_hook_asks_only_while_a_connector_holds_the_titles_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("key.titles.lock");
        assert!(!answering(&lock), "no lock file");
        let held = fs::File::create(&lock).unwrap();
        assert!(!answering(&lock), "a lock no one holds");
        held.try_lock().expect("the check let the lock go");
        assert!(answering(&lock), "a connector holds it");
    }

    #[test]
    fn hex_round_trips_and_refuses_what_is_not_hex() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(unhex(&hex(&bytes)).unwrap(), bytes);
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }
}
