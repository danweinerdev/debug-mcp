//! Shared helpers for the handler tests: build a [`ToolServer`] over a fake backend, set
//! a session state, build an arguments `Map`, and pull text out of a [`ToolOutcome`].

use std::sync::{Arc, Mutex};

use mcp_session::{SessionManager, State};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::server::ToolServer;
use crate::tests::fake::{FakeBackend, FakeFactory, FakeState};
use crate::ToolOutcome;

/// A test harness: the server, the shared session, and the shared fake-backend state.
pub struct Harness {
    pub server: ToolServer,
    pub session: Arc<SessionManager>,
    pub state: Arc<Mutex<FakeState>>,
}

impl Harness {
    /// Build a server over a fresh session + fake factory, with **no** backend connected
    /// (for guard/connect tests). `state` is shared so tests can script responses and read
    /// recorded calls.
    pub fn new() -> Harness {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let session = Arc::new(SessionManager::new());
        let factory = Arc::new(FakeFactory::new(Arc::clone(&state)));
        let server = ToolServer::new(Arc::clone(&session), factory);
        Harness {
            server,
            session,
            state,
        }
    }

    /// Build a server already in `state` with a fake backend installed (for stopped-mode op
    /// tests that don't go through `connect`).
    pub async fn connected(session_state: State) -> Harness {
        let h = Harness::new();
        h.session.set_state(session_state);
        let backend = Arc::new(FakeBackend::new(Arc::clone(&h.state)));
        h.server.set_backend(backend).await;
        h
    }

    /// Set the session state.
    pub fn set_state(&self, state: State) {
        self.session.set_state(state);
    }

    /// The recorded backend calls.
    pub fn calls(&self) -> Vec<crate::tests::fake::Call> {
        self.state.lock().unwrap().calls.clone()
    }
}

/// A fresh (never-cancelled) cancellation token for handlers that take one.
pub fn token() -> CancellationToken {
    CancellationToken::new()
}

/// Build an arguments map from `(key, value)` pairs.
pub fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

/// Assert the outcome is a JSON object and return it.
pub fn expect_json(outcome: &ToolOutcome) -> &Value {
    match outcome {
        ToolOutcome::Json(v) => v,
        other => panic!("expected Json outcome, got {other:?}"),
    }
}

/// Assert the outcome is an error and return the message.
pub fn expect_error(outcome: &ToolOutcome) -> &str {
    match outcome {
        ToolOutcome::Error(m) => m,
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

/// Assert the outcome is plain text and return it.
pub fn expect_text(outcome: &ToolOutcome) -> &str {
    match outcome {
        ToolOutcome::Text(t) => t,
        other => panic!("expected Text outcome, got {other:?}"),
    }
}
