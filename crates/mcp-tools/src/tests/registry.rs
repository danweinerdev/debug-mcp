//! Unit tests for [`BackendRegistry`]: selection precedence, the unknown-backend error,
//! and the cached capability union. The precedence is exercised through the pure
//! `select_with_env(requested, env_value)` so the `DEBUG_BACKEND` env is **injected**, not
//! mutated on the process — mutating `std::env` in parallel tests is racy and would force
//! a serialization guard. `select(None)` itself reads the real env; the precedence logic
//! it delegates to is what these tests pin down.

use std::sync::Arc;

use async_trait::async_trait;
use debugger_core::{
    BackendCapabilities, BackendError, BackendFactory, Connection, DebuggerBackend,
};
use futures::stream::{self, BoxStream, StreamExt};

use crate::registry::BackendRegistry;
use crate::tests::fake::{FakeBackend, FakeState};

/// A stub factory with a fixed name + capability descriptor; `connect()` returns a fake
/// backend over a fresh empty state. Two of these (distinct names/capabilities) drive the
/// precedence + union tests.
struct StubFactory {
    name: &'static str,
    capabilities: BackendCapabilities,
}

impl StubFactory {
    fn arc(name: &'static str, capabilities: BackendCapabilities) -> Arc<dyn BackendFactory> {
        Arc::new(StubFactory { name, capabilities })
    }
}

#[async_trait]
impl BackendFactory for StubFactory {
    fn name(&self) -> &'static str {
        self.name
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.capabilities
    }

    async fn connect(&self) -> Result<Connection, BackendError> {
        let state = Arc::new(std::sync::Mutex::new(FakeState::default()));
        let backend: Arc<dyn DebuggerBackend> = Arc::new(FakeBackend::new(state));
        let events: BoxStream<'static, debugger_core::BackendEvent> = stream::empty().boxed();
        Ok(Connection { backend, events })
    }
}

/// Two stub factories `"a"` and `"b"` with different capabilities, default `"a"`.
fn two_backend_registry() -> BackendRegistry {
    let mut registry = BackendRegistry::new("a");
    registry.register(StubFactory::arc(
        "a",
        BackendCapabilities {
            crash_dump: true,
            ..BackendCapabilities::default()
        },
    ));
    registry.register(StubFactory::arc(
        "b",
        BackendCapabilities {
            modules: true,
            ..BackendCapabilities::default()
        },
    ));
    registry
}

#[test]
fn explicit_arg_wins_over_env_and_default() {
    let registry = two_backend_registry();
    // requested="b" beats env="a" and default="a".
    let f = registry
        .select_with_env(Some("b"), Some("a"))
        .expect("b is registered");
    assert_eq!(f.name(), "b");
}

#[test]
fn env_overrides_default_when_no_arg() {
    let registry = two_backend_registry();
    // no arg, env="b" → "b" (overrides default "a").
    let f = registry
        .select_with_env(None, Some("b"))
        .expect("b is registered");
    assert_eq!(f.name(), "b");
}

#[test]
fn default_used_when_neither_arg_nor_env() {
    let registry = two_backend_registry();
    let f = registry
        .select_with_env(None, None)
        .expect("default a is registered");
    assert_eq!(f.name(), "a");
}

/// Pull the error string out of a `select`/`select_with_env` result (the `Ok` arm holds
/// an `Arc<dyn BackendFactory>`, which is not `Debug`, so `expect_err` is unavailable).
fn select_err(result: Result<Arc<dyn BackendFactory>, String>) -> String {
    match result {
        Ok(f) => panic!("expected an error, got factory '{}'", f.name()),
        Err(e) => e,
    }
}

#[test]
fn unknown_name_errors_listing_available() {
    let registry = two_backend_registry();
    let err = select_err(registry.select_with_env(Some("zzz"), None));
    // Actionable: names the bad backend and lists what is available (sorted).
    assert_eq!(err, "unknown backend 'zzz'; available: a, b");
}

#[test]
fn env_unknown_name_errors_listing_available() {
    // An env-supplied (not arg-supplied) unknown name takes the same error path.
    let registry = two_backend_registry();
    let err = select_err(registry.select_with_env(None, Some("zzz")));
    assert_eq!(err, "unknown backend 'zzz'; available: a, b");
}

#[test]
fn empty_env_falls_through_to_default() {
    // `DEBUG_BACKEND=` (empty) is treated as unset, not as a request for a backend named "".
    let registry = two_backend_registry();
    let f = registry
        .select_with_env(None, Some(""))
        .expect("default a is used when env is empty");
    assert_eq!(f.name(), "a");
}

#[test]
fn unregistered_default_surfaces_unknown_backend_error() {
    // The per-OS windows default ("windbg") is not registered this phase: selecting it
    // (no arg, no env) yields the actionable unknown-backend error — the documented,
    // acceptable behavior until Phase 3 registers the windbg factory.
    let mut registry = BackendRegistry::new("windbg");
    registry.register(StubFactory::arc("lldb", BackendCapabilities::default()));
    let err = select_err(registry.select_with_env(None, None));
    assert_eq!(err, "unknown backend 'windbg'; available: lldb");
}

#[test]
fn capability_union_is_or_of_registered_factories() {
    let registry = two_backend_registry();
    let caps = registry.capabilities();
    // "a" contributes crash_dump, "b" contributes modules; kernel/analyze stay false.
    assert_eq!(
        caps,
        BackendCapabilities {
            crash_dump: true,
            kernel: false,
            analyze: false,
            modules: true,
        }
    );
}

#[test]
fn empty_registry_has_all_false_capabilities() {
    let registry = BackendRegistry::new("lldb");
    assert_eq!(registry.capabilities(), BackendCapabilities::default());
    assert!(registry.available_names().is_empty());
    assert_eq!(registry.default_name(), "lldb");
}

#[test]
fn available_names_are_sorted() {
    let mut registry = BackendRegistry::new("a");
    registry.register(StubFactory::arc("b", BackendCapabilities::default()));
    registry.register(StubFactory::arc("a", BackendCapabilities::default()));
    assert_eq!(registry.available_names(), vec!["a", "b"]);
}
