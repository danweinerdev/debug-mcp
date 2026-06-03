//! The event-pump task (Spec FR-12.1, FR-17.7; design Decision 5/6).
//!
//! Drains a backend's neutral [`BackendEvent`] stream into the session: output chunks
//! append to the [`OutputBuffer`]; an async `Terminated` records the exit code and flips
//! state to `terminated` — but **only** while the session generation still matches the
//! one captured when the pump was spawned. A `Terminated` that arrives after a concurrent
//! `disconnect` (which bumped the generation and reset to idle) is dropped, so it cannot
//! clobber the reset state (design Decision 6).
//!
//! Go origin: `SetOutputHandler` (output append) + `onExit`/`onTerminated` (async death),
//! expressed as one runtime-neutral stream consumer.

use std::sync::Arc;

use debugger_core::BackendEvent;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::task::JoinHandle;

use crate::manager::SessionManager;

/// Spawn a task that drains `events` into `session` until the stream ends (the backend
/// was dropped on disconnect). `generation` is the session's generation epoch captured
/// at spawn time; the terminated transition is applied only while it still matches.
pub fn spawn_event_pump(
    mut events: BoxStream<'static, BackendEvent>,
    session: Arc<SessionManager>,
    generation: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            match event {
                BackendEvent::Output { category, text } => {
                    session.output_buffer().append(&category, &text);
                }
                BackendEvent::Terminated { code } => {
                    session.terminate_if_generation(generation, code);
                }
            }
        }
    })
}
