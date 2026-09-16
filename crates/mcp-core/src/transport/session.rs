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
    ///
    /// If the previous stream's receiver was dropped — the SSE connection
    /// died without a `DELETE /mcp` (client crash, network cut) — the channel
    /// is rebuilt so the client can re-establish its event stream instead of
    /// being refused `409 AlreadyStreaming` until process restart.
    pub fn take_stream(&self, id: &str) -> Result<mpsc::Receiver<Value>, StreamError> {
        let mut sessions = self.sessions.lock().expect("session registry");
        let session = sessions.get_mut(id).ok_or(StreamError::UnknownSession)?;
        if session.receiver.is_none() && session.sender.is_closed() {
            // The old stream is gone for good; swap in a fresh channel.
            let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
            session.sender = sender;
            session.receiver = Some(receiver);
        }
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

        // The first stream must still be open for the refusal to hold; once
        // its receiver drops the session is allowed to re-stream.
        let _open_stream = registry.take_stream(&id).expect("first stream");
        assert_eq!(
            registry.take_stream(&id).unwrap_err(),
            StreamError::AlreadyStreaming
        );
    }

    #[test]
    fn a_session_whose_stream_died_can_restream() {
        let registry = SessionRegistry::new();
        let id = registry.create();

        // First stream opens, then dies without DELETE /mcp (client crash).
        let stream = registry.take_stream(&id).expect("first stream");
        drop(stream);
        assert!(registry.sender(&id).expect("sender").is_closed());

        // The session is orphaned; a new GET /mcp must not hit 409 forever.
        let restreamed = registry.take_stream(&id).expect("re-stream after loss");
        drop(restreamed);
    }

    #[tokio::test]
    async fn messages_flow_on_the_replacement_channel() {
        let registry = SessionRegistry::new();
        let id = registry.create();

        drop(registry.take_stream(&id).expect("first stream"));
        let mut restreamed = registry.take_stream(&id).expect("re-stream");

        registry
            .sender(&id)
            .expect("sender")
            .send(json!({"method": "notifications/message"}))
            .await
            .expect("send");

        let message = restreamed.recv().await.expect("message");
        assert_eq!(message["method"], "notifications/message");
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
