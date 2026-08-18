//! Shared helpers for the handler tests: build a [`ToolServer`] over a fake backend, set
//! a session state, build an arguments `Map`, and pull text out of a [`ToolOutcome`].

use std::sync::{Arc, Mutex};

use mcp_session::{SessionManager, State};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::server::ToolServer;
use debugger_core::BackendError;

use crate::ToolOutcome;
use crate::tests::fake::{
    FakeBackend, FakeFactory, FakeState, single_factory_registry,
    windbg_like_connect_error_factory, windbg_like_factory,
};

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
        let server = ToolServer::new(Arc::clone(&session), single_factory_registry(factory));
        Harness {
            server,
            session,
            state,
        }
    }

    /// Build a server over a fresh session whose ONLY registered factory is named
    /// `"windbg"` (all-true capabilities), with **no** backend connected. The
    /// `open_crash_dump`/`attach_kernel` connect-point tools force-select `"windbg"`, so this
    /// lets a cross-platform test drive their full connect → backend-call → response path
    /// (scripting the outcome via the shared `state`) without a live DbgEng engine.
    pub fn new_windbg() -> Harness {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let session = Arc::new(SessionManager::new());
        let factory = windbg_like_factory(Arc::clone(&state));
        let server = ToolServer::new(Arc::clone(&session), single_factory_registry(factory));
        Harness {
            server,
            session,
            state,
        }
    }

    /// Build a server whose ONLY registered factory is named `"windbg"` but whose
    /// `connect()` fails with `err`. The force-select succeeds (name-based), so the FAILURE
    /// lands at the connect phase — exercising the `open_crash_dump`/`attach_kernel`
    /// connect-error branch (`session.reset()` + `clear_backend()` + `connect_error`).
    pub fn new_windbg_connect_error(err: BackendError) -> Harness {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let session = Arc::new(SessionManager::new());
        let factory = windbg_like_connect_error_factory(Arc::clone(&state), err);
        let server = ToolServer::new(Arc::clone(&session), single_factory_registry(factory));
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
        h.server.set_backend(backend, "fake").await;
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
