// SPDX-License-Identifier: GPL-3.0-or-later

//! Session bookkeeping for the MCP Streamable HTTP transport.
//!
//! The transport hands every `initialize` a `Mcp-Session-Id` and keeps a
//! per-session queue of server-to-client messages. The queue is drained by the
//! `GET /mcp` SSE stream, which is what MCP clients (e.g. `mcp`'s
//! `streamable_http` used by `google-adk`) require to keep a session alive
//! across calls.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::mpsc;

/// Depth of a session's server-to-client queue. Messages are only produced by
/// the server itself, so a shallow queue is enough; a client that never drains
/// it is a client that has gone away.
const CHANNEL_CAPACITY: usize = 64;

struct Session {
    sender: mpsc::Sender<Value>,
    /// Taken by the first `GET /mcp` for the session; a second concurrent
    /// stream is refused rather than silently splitting the message flow.
    receiver: Option<mpsc::Receiver<Value>>,
}

/// Registry of live Streamable HTTP sessions, keyed by `Mcp-Session-Id`.
#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, Session>>,
}

/// Why a session's SSE stream could not be opened.
#[derive(Debug, PartialEq, Eq)]
pub enum StreamError {
    /// No session with that id (client should re-`initialize`).
    UnknownSession,
    /// The session already has an open SSE stream.
    AlreadyStreaming,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new session and returns its id.
    pub fn create(&self) -> String {
        let id = new_session_id();
        let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        self.sessions.lock().expect("session registry").insert(
            id.clone(),
            Session {
                sender,
                receiver: Some(receiver),
            },
        );
        id
    }

    pub fn contains(&self, id: &str) -> bool {
        self.sessions
            .lock()
            .expect("session registry")
            .contains_key(id)
    }

    /// Claims the session's server-to-client stream for an SSE response.
    pub fn take_stream(&self, id: &str) -> Result<mpsc::Receiver<Value>, StreamError> {
        let mut sessions = self.sessions.lock().expect("session registry");
        let session = sessions.get_mut(id).ok_or(StreamError::UnknownSession)?;
        session.receiver.take().ok_or(StreamError::AlreadyStreaming)
    }

    /// Handle for pushing a server-initiated message to a session.
    pub fn sender(&self, id: &str) -> Option<mpsc::Sender<Value>> {
        self.sessions
            .lock()
            .expect("session registry")
            .get(id)
            .map(|session| session.sender.clone())
    }

    /// Drops a session; any open SSE stream ends when its sender is dropped.
    pub fn remove(&self, id: &str) -> bool {
        self.sessions
            .lock()
            .expect("session registry")
            .remove(id)
            .is_some()
    }
}

/// Opaque session id. The spec only requires it to be visible-ASCII and
/// globally unique.
fn new_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_ids_are_unique() {
        let registry = SessionRegistry::new();
        let first = registry.create();
        let second = registry.create();

        assert_ne!(first, second);
        assert_eq!(first.len(), 32);
        assert!(registry.contains(&first));
        assert!(registry.contains(&second));
    }

    #[tokio::test]
    async fn stream_receives_pushed_messages() {
        let registry = SessionRegistry::new();
        let id = registry.create();

        let mut stream = registry.take_stream(&id).expect("stream");
        registry
            .sender(&id)
            .expect("sender")
            .send(json!({"method": "notifications/message"}))
            .await
            .expect("send");

        let message = stream.recv().await.expect("message");
        assert_eq!(message["method"], "notifications/message");
    }

    #[test]
    fn second_stream_for_a_session_is_refused() {
        let registry = SessionRegistry::new();
        let id = registry.create();

        assert!(registry.take_stream(&id).is_ok());
        assert_eq!(
            registry.take_stream(&id).unwrap_err(),
            StreamError::AlreadyStreaming
        );
    }

    #[test]
    fn unknown_session_has_no_stream() {
        let registry = SessionRegistry::new();

        assert_eq!(
            registry.take_stream("nope").unwrap_err(),
            StreamError::UnknownSession
        );
        assert!(!registry.remove("nope"));
    }

    #[tokio::test]
    async fn removing_a_session_closes_its_stream() {
        let registry = SessionRegistry::new();
        let id = registry.create();
        let mut stream = registry.take_stream(&id).expect("stream");

        assert!(registry.remove(&id));
        assert!(!registry.contains(&id));
        assert!(stream.recv().await.is_none());
    }
}
