//! [`BackendRegistry`] — the runtime backend switcher `ToolServer` holds in place of a
//! single factory (design §"Wiring changes to `ToolServer`", Decision 7).
//!
//! Both debugger factories (lldb everywhere; windbg under `cfg(windows)`) are registered
//! by name into one registry; each connect point selects a factory per call. Selection
//! precedence is **explicit `backend` arg → `DEBUG_BACKEND` env → per-OS default**. The
//! registry also caches the **union** of every registered factory's
//! [`BackendCapabilities`] so `list_tools` can advertise the capability-gated tools
//! without connecting (task 1.3 consumes the cache via [`BackendRegistry::capabilities`]).
//!
//! SEAM: this layer names only the neutral `debugger-core` `BackendFactory`/
//! `BackendCapabilities`; it never names a DAP, lldb, or DbgEng type. The concrete
//! factories are constructed in the `debug-mcp` bin crate (which already depends on the
//! backend crates) and handed in as `Arc<dyn BackendFactory>`.

use std::collections::HashMap;
use std::sync::Arc;

use debugger_core::{BackendCapabilities, BackendFactory};

/// The default backend name for the host OS (design Decision 7: WinDbg on Windows, lldb
/// on Mac/Linux).
///
/// On Windows the windbg factory may **not** be registered yet — Phase 3 of the WinDbg
/// port adds `WinDbgFactory`. Until then, selecting the unregistered windows default
/// surfaces the actionable unknown-backend error from [`BackendRegistry::select`], which
/// is acceptable for this phase (the only registered factory is lldb, reachable via an
/// explicit `backend` arg or `DEBUG_BACKEND=lldb`).
pub fn default_backend_for_os() -> &'static str {
    #[cfg(windows)]
    {
        "windbg"
    }
    #[cfg(not(windows))]
    {
        "lldb"
    }
}

/// Name→factory map plus the resolved per-OS default and the cached capability union.
/// Replaces `ToolServer`'s former single `Arc<dyn BackendFactory>`.
pub struct BackendRegistry {
    factories: HashMap<&'static str, Arc<dyn BackendFactory>>,
    default_name: &'static str,
    /// UNION across all registered factories (cached at `register()` time for
    /// `list_tools`, consumed in task 1.3). On Mac/Linux (lldb only) this stays all-false
    /// ⇒ exactly the 21 base tools; on Windows with windbg registered it is all-true.
    capabilities: BackendCapabilities,
}

impl BackendRegistry {
    /// An empty registry whose default is `default_name` (typically
    /// [`default_backend_for_os`]). No factories registered yet ⇒ all-false capabilities.
    pub fn new(default_name: &'static str) -> Self {
        BackendRegistry {
            factories: HashMap::new(),
            default_name,
            capabilities: BackendCapabilities::default(),
        }
    }

    /// Register a factory by its `name()`, ORing its capabilities into the cached union.
    ///
    /// Registering two factories with the same `name()` would silently replace the first in
    /// the map while leaving its already-OR-ed capabilities in the (non-subtractable) union;
    /// the debug-assert catches that misuse in tests (zero-cost in release).
    pub fn register(&mut self, f: Arc<dyn BackendFactory>) {
        debug_assert!(
            !self.factories.contains_key(f.name()),
            "backend '{}' registered twice",
            f.name()
        );
        let caps = f.capabilities();
        self.capabilities.crash_dump |= caps.crash_dump;
        self.capabilities.kernel |= caps.kernel;
        self.capabilities.analyze |= caps.analyze;
        self.capabilities.modules |= caps.modules;
        self.factories.insert(f.name(), f);
    }

    /// Resolve the factory for a connect call. Precedence: explicit `requested` →
    /// `DEBUG_BACKEND` env → the registry's per-OS default. Returns the selected factory
    /// or a tool-error `String` (an unknown/unregistered name) listing what *is*
    /// available.
    pub fn select(&self, requested: Option<&str>) -> Result<Arc<dyn BackendFactory>, String> {
        let env_value = std::env::var("DEBUG_BACKEND").ok();
        self.select_with_env(requested, env_value.as_deref())
    }

    /// The pure precedence + lookup logic, with the `DEBUG_BACKEND` value injected so the
    /// precedence is unit-testable without mutating the process environment (mutating
    /// `std::env` is racy across parallel tests). [`select`](Self::select) reads the real
    /// env and delegates here.
    pub(crate) fn select_with_env(
        &self,
        requested: Option<&str>,
        env_value: Option<&str>,
    ) -> Result<Arc<dyn BackendFactory>, String> {
        // An empty `DEBUG_BACKEND=` (or empty explicit arg) is treated as unset — fall
        // through to the next precedence level rather than requesting a backend named "".
        let requested = requested.filter(|s| !s.is_empty());
        let env_value = env_value.filter(|s| !s.is_empty());
        let name = requested.or(env_value).unwrap_or(self.default_name);
        match self.factories.get(name) {
            Some(f) => Ok(Arc::clone(f)),
            None => Err(format!(
                "unknown backend '{name}'; available: {}",
                self.comma_list()
            )),
        }
    }

    /// The cached capability union across all registered factories (task 1.3:
    /// `list_tools` advertising). Computed at `register()` time, not per call.
    pub fn capabilities(&self) -> BackendCapabilities {
        self.capabilities
    }

    /// The resolved per-OS default backend name (task 1.3: the `status` `backend` field).
    pub fn default_name(&self) -> &'static str {
        self.default_name
    }

    /// The registered backend names, sorted (task 1.3: the `status` `available_backends`
    /// array).
    pub fn available_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.factories.keys().copied().collect();
        names.sort_unstable();
        names
    }

    /// The registered names as a comma-separated list for the unknown-backend error.
    fn comma_list(&self) -> String {
        self.available_names().join(", ")
    }
}
