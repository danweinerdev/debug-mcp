//! Static, per-backend capability descriptor — the contract piece that drives the
//! capability-gated tool surface (design §Trait extension, Decision 3).
//!
//! A backend's [`BackendCapabilities`] is known *without connecting* (it is returned by
//! [`crate::BackendFactory::capabilities`]); the tool layer takes the **union** across
//! all registered factories to decide which optional tools `list_tools` advertises. Each
//! flag corresponds to one WinDbg-only neutral tool; lldb leaves them all `false`.

/// Which capability-gated tools a backend supports. Default: all-false (lldb). The
/// WinDbg factory reports all-true. C++ origin: the WinDbg-only verbs in the
/// `windbg-mcp` plugin that have no lldb analog.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities {
    /// `open_crash_dump` — open a crash/minidump (`IDebugClient::OpenDumpFile`).
    pub crash_dump: bool,
    /// `attach_kernel` — attach to a kernel target (`IDebugClient::AttachKernel`).
    pub kernel: bool,
    /// `analyze_crash` — automated crash analysis (`!analyze -v`).
    pub analyze: bool,
    /// `get_modules` — list loaded modules (`IDebugSymbols::GetNumberModules`).
    pub modules: bool,
}
