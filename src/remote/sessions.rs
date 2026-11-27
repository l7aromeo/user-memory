//! The memory server's MCP sessions: rmcp's [`LocalSessionManager`], with
//! idle sessions closed by what they do rather than by the time since the
//! session last sent or received a message.
//!
//! rmcp closes a session that has seen no message for its `keep_alive`
//! (5 minutes by default), counted in its worker from the last message. A
//! tool call still running sends none, so a call past 5 minutes lost its
//! session and its answer. Here rmcp's timer is off, and a session is
//! closed once it has had no request for [`SESSION_IDLE`] and no stream
//! open: a call in flight keeps its answer's stream open, and a client
//! whose connection dropped has none, since the HTTP server drops a
//! stream whose connection it can no longer write to.

use futures_core::Stream;
use rmcp::{
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::streamable_http_server::{
        RestoreOutcome, SessionId, SessionManager,
        session::{
            EventStore, ServerSseMessage,
            local::{LocalSessionManager, LocalSessionManagerError},
        },
    },
};
use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex, PoisonError, Weak},
    task::{Context, Poll},
    time::{Duration, Instant},
};

/// How long a session may go with no request and no stream open before it
/// is closed: rmcp's own default for an idle session. A session with no
/// stream is one whose client has stopped asking and listening; 5 minutes
/// is far past any pause between an agent's calls, and short enough that
/// sessions clients left without closing don't pile up.
const SESSION_IDLE: Duration = Duration::from_secs(300);

/// How often idle sessions are looked for. Short against [`SESSION_IDLE`],
/// so a session is closed at most this long after it became idle; each
/// look is one pass over the open sessions under a lock.
const REAP_EVERY: Duration = Duration::from_secs(10);

/// [`SESSION_IDLE`], which debug builds take in milliseconds from
/// `USER_MEMORY_SESSION_IDLE_MS` so tests can see a session closed without
/// waiting minutes.
fn session_idle() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("USER_MEMORY_SESSION_IDLE_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    SESSION_IDLE
}

/// What a session has done: when it last made a request or let go of a
/// stream, and how many of its streams are open.
struct Activity {
    last: Instant,
    open: usize,
}

impl Activity {
    fn new() -> Self {
        Self {
            last: Instant::now(),
            open: 0,
        }
    }
}

type Activities = Arc<Mutex<HashMap<SessionId, Activity>>>;

fn lock(activities: &Activities) -> std::sync::MutexGuard<'_, HashMap<SessionId, Activity>> {
    activities.lock().unwrap_or_else(PoisonError::into_inner)
}

/// rmcp's [`LocalSessionManager`] with its idle timer off, closing a
/// session once it has had no request for [`SESSION_IDLE`] and no stream
/// open.
pub(super) struct Sessions {
    inner: LocalSessionManager,
    activities: Activities,
    idle: Duration,
}

impl Sessions {
    /// The session manager, with its task that closes idle sessions, which
    /// ends once the manager is dropped.
    pub(super) fn start() -> Arc<Self> {
        Self::start_with(session_idle())
    }

    /// [`Sessions::start`], closing sessions idle for `idle`.
    fn start_with(idle: Duration) -> Arc<Self> {
        let mut inner = LocalSessionManager::default();
        inner.session_config.keep_alive = None;
        let sessions = Arc::new(Self {
            inner,
            activities: Arc::default(),
            idle,
        });
        let every = REAP_EVERY.min(sessions.idle / 4);
        let weak: Weak<Self> = Arc::downgrade(&sessions);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            loop {
                tick.tick().await;
                let Some(sessions) = weak.upgrade() else {
                    break;
                };
                sessions.close_idle().await;
            }
        });
        sessions
    }

    /// Notes a request in session `id`, if it is one this manager keeps.
    fn requested(&self, id: &SessionId) {
        if let Some(activity) = lock(&self.activities).get_mut(id) {
            activity.last = Instant::now();
        }
    }

    /// `stream`, counted as open in session `id` until it is dropped.
    fn counted<S>(&self, id: &SessionId, stream: S) -> Counted<S> {
        if let Some(activity) = lock(&self.activities).get_mut(id) {
            activity.open += 1;
        }
        Counted {
            stream: Box::pin(stream),
            _open: Open {
                activities: Arc::clone(&self.activities),
                id: id.clone(),
            },
        }
    }

    /// Closes every session with no stream open and no request for
    /// [`SESSION_IDLE`].
    async fn close_idle(&self) {
        let idle: Vec<SessionId> = lock(&self.activities)
            .iter()
            .filter(|(_, activity)| activity.open == 0 && activity.last.elapsed() >= self.idle)
            .map(|(id, _)| id.clone())
            .collect();
        for id in idle {
            lock(&self.activities).remove(&id);
            // A session that ended on its own is already gone.
            let _ = self.inner.close_session(&id).await;
        }
    }
}

/// Counts a stream as open while it lives; once it is dropped, the
/// session's idle time starts again from then.
struct Open {
    activities: Activities,
    id: SessionId,
}

impl Drop for Open {
    fn drop(&mut self) {
        if let Some(activity) = lock(&self.activities).get_mut(&self.id) {
            activity.open = activity.open.saturating_sub(1);
            activity.last = Instant::now();
        }
    }
}

/// A session's stream, counted as open while it lives.
struct Counted<S> {
    stream: Pin<Box<S>>,
    _open: Open,
}

impl<S: Stream> Stream for Counted<S> {
    type Item = S::Item;
    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<S::Item>> {
        self.stream.as_mut().poll_next(context)
    }
}

impl SessionManager for Sessions {
    type Error = LocalSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let (id, transport) = self.inner.create_session().await?;
        lock(&self.activities).insert(id.clone(), Activity::new());
        Ok((id, transport))
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        self.requested(id);
        self.inner.initialize_session(id, message).await
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        self.inner.has_session(id).await
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        lock(&self.activities).remove(id);
        self.inner.close_session(id).await
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.requested(id);
        let stream = self.inner.create_stream(id, message).await?;
        Ok(self.counted(id, stream))
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<(), Self::Error> {
        self.requested(id);
        self.inner.accept_message(id, message).await
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.requested(id);
        let stream = self.inner.create_standalone_stream(id).await?;
        Ok(self.counted(id, stream))
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.requested(id);
        let stream = self.inner.resume(id, last_event_id).await?;
        Ok(self.counted(id, stream))
    }

    async fn restore_session(
        &self,
        id: SessionId,
    ) -> Result<RestoreOutcome<Self::Transport>, Self::Error> {
        let restored = self.inner.restore_session(id.clone()).await?;
        if matches!(restored, RestoreOutcome::Restored(_)) {
            lock(&self.activities).insert(id, Activity::new());
        }
        Ok(restored)
    }

    fn event_store(&self) -> Option<Arc<dyn EventStore>> {
        self.inner.event_store()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The idle bound these tests give their sessions.
    const IDLE: Duration = Duration::from_millis(100);

    // Sessions opened and left are dropped from both lists, this
    // manager's and rmcp's, so neither grows.
    #[tokio::test]
    async fn abandoned_sessions_leave_nothing_behind() {
        let sessions = Sessions::start_with(IDLE);
        for _ in 0..5 {
            sessions.create_session().await.unwrap();
        }
        assert_eq!(lock(&sessions.activities).len(), 5);
        assert_eq!(sessions.inner.sessions.read().await.len(), 5);
        tokio::time::sleep(IDLE * 4).await;
        assert!(lock(&sessions.activities).is_empty());
        assert!(sessions.inner.sessions.read().await.is_empty());
    }
}
