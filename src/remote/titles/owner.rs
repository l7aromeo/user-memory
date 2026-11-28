//! The connector that answers the hooks' title lookups (#292). Of the
//! connectors running on a machine for one server, the one holding the
//! titles lock answers: it keeps each scope's titles in memory, current to
//! the position its polls found, and listens on 127.0.0.1 for the hooks.
//! The system lets go of the lock when it exits or is killed, and another
//! connector takes it at its next tick.

use super::{
    Fetched,
    channel::{self, Answer, Endpoint, Named},
};
use crate::{
    memory::{Position, delivery_files, served_titles},
    private,
    remote::{Connection, marker},
};
use std::{
    collections::HashMap,
    fs,
    net::Ipv4Addr,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};

/// How long one ask for titles waits for the server, as a poll waits for
/// its answer: one left unanswered is asked again at the next poll, and the
/// hooks meanwhile name ids alone.
const TITLES_CALL_WAIT: Duration = Duration::from_secs(10);

/// How long a hook's connection may take, from accepting it to the answer
/// written: a hook waits at most `channel::TITLE_WAIT`, so a connection
/// still open after this is no hook's, and is dropped.
const CONNECTION_WAIT: Duration = Duration::from_secs(1);

/// The most hook connections answered at once: each hook opens one, and a
/// machine runs a few sessions, so more at once is no hook's; one past it
/// is closed unanswered.
const TITLE_CONNECTIONS: usize = 32;

/// How long the connector waits after an accept fails before it accepts
/// again: ten tries a second cost nothing, and a hook that comes meanwhile
/// waits at most `channel::TITLE_WAIT`, a quarter of this, then names its
/// ids alone, as it would with no connector answering.
const ACCEPT_ERROR_WAIT: Duration = Duration::from_millis(100);

/// Makes a debug build's connector wait this many milliseconds before it
/// answers a title lookup, so tests see an answer come too late.
#[cfg(debug_assertions)]
const DELAY: &str = "USER_MEMORY_TEST_TITLES_DELAY_MS";

/// One scope's titles, at the server's position `position`.
#[derive(Default)]
struct ScopeTitles {
    position: String,
    memories: HashMap<String, served_titles::Titled>,
}

impl ScopeTitles {
    /// Takes `answer`: every memory when it is whole, else the changes.
    fn take(&mut self, answer: served_titles::Answer) {
        if answer.whole {
            self.memories.clear();
        }
        for id in &answer.removed {
            self.memories.remove(id);
        }
        for memory in answer.memories {
            self.memories.insert(memory.id.clone(), memory);
        }
        self.position = answer.position;
    }

    /// The answer to a lookup of `ids` by a hook whose delivery files are
    /// at `at`: none of the titles when these are behind it.
    fn answer(&self, at: &str, ids: &[String]) -> Answer {
        let position = Position::kept(self.position.clone());
        let current = Position::kept(at.to_owned()).within(&position);
        let titles = ids
            .iter()
            .filter(|_| current)
            .filter_map(|id| {
                let memory = self.memories.get(id)?;
                Some((
                    id.clone(),
                    Named {
                        title: memory.title.clone(),
                        source: memory.source.clone(),
                        memory_type: memory.memory_type.clone(),
                    },
                ))
            })
            .collect();
        Answer {
            at: self.position.clone(),
            titles,
        }
    }
}

/// Each scope's titles, by the scope's sorted projects.
type Scopes = Arc<Mutex<HashMap<Vec<String>, ScopeTitles>>>;

/// What this connector holds while it answers the lookups.
struct Owned {
    /// The titles lock, held by this open file.
    _lock: fs::File,
    listening: tokio::task::JoinHandle<()>,
    scopes: Scopes,
    /// The marker's position when the titles were last brought up to it.
    asked_at: Option<Position>,
    /// Asks nothing before this: after a failed ask, a poll later; after a
    /// server without the route, the quiet interval.
    resting_until: Option<tokio::time::Instant>,
}

/// Answers the hooks' title lookups for the server `connection` names
/// while this connector holds the titles lock, trying for it at each tick
/// it doesn't, until `stop` turns true.
pub(in crate::remote) async fn serve(
    connection: Connection,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let Some(files) = marker::titles_files(&connection.url) else {
        return;
    };
    let mut owned: Option<Owned> = None;
    loop {
        if let Some(held) = &mut owned {
            refresh(&connection, held).await;
        } else {
            owned = take(&files).await;
        }
        tokio::select! {
            () = tokio::time::sleep(marker::poll_every() / marker::TICKS_PER_POLL) => {}
            _ = stop.wait_for(|stopped| *stopped) => break,
        }
    }
    if let Some(held) = owned {
        held.listening.abort();
        // The endpoint file is left for the next owner to replace when it
        // names another process: one written after this one's.
        let ours = fs::read(&files.endpoint)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Endpoint>(&bytes).ok())
            .is_some_and(|endpoint| endpoint.pid == std::process::id());
        if ours {
            let _ = fs::remove_file(&files.endpoint);
        }
    }
}

/// Takes the titles lock without waiting, listens on 127.0.0.1 and writes
/// the endpoint file; none when another connector holds the lock or any
/// step fails.
async fn take(files: &marker::TitlesFiles) -> Option<Owned> {
    private::create_dir(files.lock.parent()?).ok()?;
    let lock = private::create_file(
        fs::OpenOptions::new().read(true).write(true).create(true),
        &files.lock,
    )
    .ok()?;
    lock.try_lock().ok()?;
    let listener = listen().await.ok()?;
    let port = listener.local_addr().ok()?.port();
    let (key, key_hex) = channel::new_key();
    let endpoint = Endpoint {
        port,
        key: key_hex,
        pid: std::process::id(),
    };
    channel::write_endpoint(&files.endpoint, &endpoint).ok()?;
    let scopes = Scopes::default();
    let listening = tokio::spawn(accept(listener, Arc::new(key), scopes.clone()));
    Some(Owned {
        _lock: lock,
        listening,
        scopes,
        asked_at: None,
        resting_until: None,
    })
}

/// A listener on a free port of 127.0.0.1, never another address: only
/// programs on this machine can reach it, and Windows asks no firewall
/// question for a loopback listener.
async fn listen() -> std::io::Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await
}

/// Answers each hook that connects, at most [`TITLE_CONNECTIONS`] at once.
async fn accept(listener: TcpListener, key: Arc<channel::Key>, scopes: Scopes) {
    let room = Arc::new(Semaphore::new(TITLE_CONNECTIONS));
    each_accepted(
        || listener.accept(),
        |(stream, _)| {
            let Ok(permit) = room.clone().try_acquire_owned() else {
                return;
            };
            let (key, scopes) = (key.clone(), scopes.clone());
            tokio::spawn(async move {
                let _permit = permit;
                let _ = tokio::time::timeout(CONNECTION_WAIT, answer(stream, &key, &scopes)).await;
            });
        },
    )
    .await;
}

/// Hands each connection `next` accepts to `take`, for good. A failed
/// accept waits [`ACCEPT_ERROR_WAIT`] before the next: an error such as
/// running out of file descriptors lasts, and asking again at once would
/// spin a core in the user's connector for as long as it does.
async fn each_accepted<T, F>(mut next: impl FnMut() -> F, mut take: impl FnMut(T))
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    loop {
        match next().await {
            Ok(accepted) => take(accepted),
            Err(_) => tokio::time::sleep(ACCEPT_ERROR_WAIT).await,
        }
    }
}

/// Reads one lookup from `stream` and answers it; a request that doesn't
/// open under the key gets no answer.
async fn answer(stream: TcpStream, key: &channel::Key, scopes: &Scopes) {
    let _ = stream.set_nodelay(true);
    let (reading, mut writing) = stream.into_split();
    let mut line = Vec::new();
    let read = BufReader::new(reading)
        .take(channel::REQUEST_BYTES as u64)
        .read_until(b'\n', &mut line)
        .await;
    if read.is_err() || !line.ends_with(b"\n") {
        return;
    }
    let Some((nonce, request)) = std::str::from_utf8(&line)
        .ok()
        .and_then(|line| channel::read_request(key, line))
    else {
        return;
    };
    #[cfg(debug_assertions)]
    if let Some(delay) = std::env::var(DELAY).ok().and_then(|ms| ms.parse().ok()) {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let scope = delivery_files::scope(&request.scope);
    let answer = scopes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&scope)
        .map(|titles| titles.answer(&request.at, &request.ids))
        .unwrap_or_default();
    let _ = writing
        .write_all(channel::answer_line(key, &nonce, &answer).as_bytes())
        .await;
}

/// Brings the titles of every scope with delivery files on this machine up
/// to the server, when a poll found it past where they were last asked, or
/// a scope has none yet: the changes since each scope's position, or whole.
async fn refresh(connection: &Connection, held: &mut Owned) {
    if held
        .resting_until
        .is_some_and(|until| tokio::time::Instant::now() < until)
    {
        return;
    }
    let url = connection.url.clone();
    let Ok(bases) = tokio::task::spawn_blocking(move || delivery_files::served_bases(&url)).await
    else {
        return;
    };
    let scopes: Vec<Vec<String>> = bases
        .into_iter()
        .map(|(projects, _)| delivery_files::scope(&projects))
        .collect();
    let polled = marker::polled(&connection.url);
    let missing = {
        let mut held_scopes = held.scopes.lock().unwrap_or_else(PoisonError::into_inner);
        held_scopes.retain(|scope, _| scopes.contains(scope));
        scopes.iter().any(|scope| !held_scopes.contains_key(scope))
    };
    let Some(polled) = polled else {
        return;
    };
    let moved = held
        .asked_at
        .as_ref()
        .is_none_or(|asked| !asked.same_change(&polled));
    if !missing && !moved {
        return;
    }
    for scope in scopes {
        let since = held
            .scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&scope)
            .map(|titles| titles.position.clone());
        let asked = tokio::time::timeout(
            TITLES_CALL_WAIT,
            connection.titles(&scope, since.as_deref()),
        )
        .await;
        match asked {
            Ok(Ok(Fetched::Titles(answer))) => held
                .scopes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(scope)
                .or_default()
                .take(answer),
            Ok(Ok(Fetched::Unsupported)) => {
                held.scopes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clear();
                held.resting_until =
                    Some(tokio::time::Instant::now() + marker::poll_every() * marker::QUIET_POLLS);
                return;
            }
            Ok(Err(_)) | Err(_) => {
                held.resting_until = Some(tokio::time::Instant::now() + marker::poll_every());
                return;
            }
        }
    }
    held.asked_at = Some(polled);
    held.resting_until = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An accept that keeps failing, as one out of file descriptors does,
    /// is tried again only after [`ACCEPT_ERROR_WAIT`], never at once: in
    /// two waits' time it is asked at most three times.
    #[tokio::test]
    async fn a_failing_accept_waits_before_it_is_tried_again() {
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let accepting = each_accepted(
            || async {
                asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Gives way, as a real accept does, so a loop that never
                // waits still lets the timeout below end it.
                tokio::task::yield_now().await;
                Err::<(), _>(std::io::Error::from(std::io::ErrorKind::OutOfMemory))
            },
            |()| unreachable!("nothing is accepted"),
        );
        let _ = tokio::time::timeout(ACCEPT_ERROR_WAIT * 2, accepting).await;
        let asked = asked.load(std::sync::atomic::Ordering::Relaxed);
        assert!((1..=3).contains(&asked), "{asked}");
    }

    /// The listener is on 127.0.0.1, never all addresses, so nothing off
    /// this machine reaches it and Windows asks no firewall question.
    #[tokio::test]
    async fn the_listener_is_bound_to_the_loopback_address_only() {
        let address = listen().await.unwrap().local_addr().unwrap();
        assert_eq!(address.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(!address.ip().is_unspecified());
        assert_ne!(address.port(), 0);
    }

    fn titled(id: &str, title: &str) -> served_titles::Titled {
        served_titles::Titled {
            id: id.into(),
            title: title.into(),
            source: "explicit".into(),
            memory_type: "preference".into(),
        }
    }

    /// Titles answer only a hook whose files are at or before their
    /// position: behind them, none; at or past, each memory held, and a
    /// gone one left out.
    #[test]
    fn titles_answer_only_files_they_are_current_for() {
        const A: &str = "e0000001-0000-4000-8000-000000000001";
        const B: &str = "e0000002-0000-4000-8000-000000000002";
        let mut titles = ScopeTitles::default();
        titles.take(served_titles::Answer {
            position: "1.0123456789abcdef.5.0".into(),
            whole: true,
            memories: vec![titled(A, "Old"), titled(B, "Kept")],
            removed: Vec::new(),
        });
        titles.take(served_titles::Answer {
            position: "1.0123456789abcdef.7.0".into(),
            whole: false,
            memories: Vec::new(),
            removed: vec![A.to_owned()],
        });
        let ids = [A.to_owned(), B.to_owned()];
        let behind = titles.answer("1.0123456789abcdef.8.0", &ids);
        assert!(behind.titles.is_empty());
        let other_store = titles.answer("1.fedcba9876543210.6.0", &ids);
        assert!(other_store.titles.is_empty());
        for at in ["1.0123456789abcdef.7.0", "1.0123456789abcdef.6.0"] {
            let current = titles.answer(at, &ids);
            assert_eq!(current.at, "1.0123456789abcdef.7.0");
            assert_eq!(current.titles.keys().collect::<Vec<_>>(), [B]);
        }
    }
}
