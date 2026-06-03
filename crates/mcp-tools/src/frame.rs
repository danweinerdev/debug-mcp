//! `resolve_frame_id` — Go `inspection.go`'s `resolveFrameID` (Spec FR-10.3).
//!
//! Maps a user-facing frame index to a backend frame id. On a frame-map hit it returns
//! immediately; on a miss it issues an implicit `stack_trace(thread, start=0, levels=20)`
//! (always `levels=20`, regardless of any `backtrace` `levels` arg), rebuilds and stores
//! the frame mapping, and re-looks-up. The frame map is cached across stops and only
//! cleared by `reset` (lldb-dap frame ids are index-stable), so this never clears it.
//!
//! The implicit-stackTrace error variants carry the inner Go prefixes — `implicit
//! stackTrace request failed: <e>`, `unexpected stackTrace response type: <type>`,
//! `implicit stackTrace failed: <msg>` — which the caller wraps as `failed to resolve
//! frame: <err>` (Spec FR-10.3 / task 5.4).

use std::collections::HashMap;
use std::sync::Arc;

use debugger_core::{BackendError, DebuggerBackend};
use mcp_session::SessionManager;

/// Resolve the backend frame id for `frame_index`, rebuilding the frame map on a miss.
/// Errors are the inner `implicit stackTrace …` / out-of-range strings; the caller wraps
/// them as `failed to resolve frame: <err>`.
pub async fn resolve_frame_id(
    backend: &Arc<dyn DebuggerBackend>,
    session: &SessionManager,
    frame_index: i64,
) -> Result<i64, String> {
    if let Some(&frame_id) = session.frame_mapping().get(&frame_index) {
        return Ok(frame_id);
    }

    // Frame-map miss: implicit stackTrace to populate it. Always levels=20.
    let thread_id = session.last_stopped().map(|e| e.thread_id).unwrap_or(1);

    let (frames, _total) = backend
        .stack_trace(thread_id, 0, 20)
        .await
        .map_err(map_implicit_error)?;

    let mut new_mapping = HashMap::with_capacity(frames.len());
    for frame in &frames {
        new_mapping.insert(frame.index, frame.id);
    }
    session.set_frame_mapping(new_mapping.clone());

    match new_mapping.get(&frame_index) {
        Some(&frame_id) => Ok(frame_id),
        None => Err(format!(
            "frame index {frame_index} out of range (stack has {} frames)",
            frames.len()
        )),
    }
}

/// Map a backend error from the implicit stackTrace into Go's inner wording (Go
/// `inspection.go` `resolveFrameID`): `implicit stackTrace request failed: <e>` (send),
/// `unexpected stackTrace response type: <type>` (wrong type), `implicit stackTrace
/// failed: <msg>` (`success=false`).
fn map_implicit_error(err: BackendError) -> String {
    match err {
        BackendError::Send(inner) => format!("implicit stackTrace request failed: {inner}"),
        BackendError::Closed => "implicit stackTrace request failed: connection closed".to_string(),
        BackendError::Timeout => {
            "implicit stackTrace request failed: operation timed out".to_string()
        }
        BackendError::Detect(m) | BackendError::Spawn(m) => {
            format!("implicit stackTrace request failed: {m}")
        }
        BackendError::Protocol { ty } => {
            let label = ty.split_once(':').map(|(_, rest)| rest).unwrap_or(&ty);
            format!("unexpected stackTrace response type: {label}")
        }
        BackendError::Dap { message } => {
            // The backend phrased a send failure as `stackTrace request failed: <e>` and a
            // success=false as `stackTrace failed: <msg>`; re-shape both under the
            // implicit-stackTrace wording.
            if let Some(inner) = message
                .find(" request failed: ")
                .map(|i| &message[i + " request failed: ".len()..])
            {
                format!("implicit stackTrace request failed: {inner}")
            } else if let Some(inner) = message
                .find(" failed: ")
                .map(|i| &message[i + " failed: ".len()..])
            {
                format!("implicit stackTrace failed: {inner}")
            } else {
                message
            }
        }
    }
}
