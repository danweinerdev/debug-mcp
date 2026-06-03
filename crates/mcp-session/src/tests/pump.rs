//! Frame-map cache + last-stop cache + the event-pump task (Spec FR-12, FR-17.7;
//! design Decision 5/6).
//!
//! The frame-map clone test mirrors the Go live-map-aliasing fix (returned by clone).
//! The pump tests cover Go's `SetOutputHandler` (output append) and `onTerminated`
//! (state → terminated), plus the Rust-only generation guard that drops a stale
//! terminated after a concurrent disconnect.

use std::collections::HashMap;
use std::sync::Arc;

use debugger_core::{BackendEvent, StopInfo};
use futures::stream::{self, BoxStream, StreamExt};

use crate::{spawn_event_pump, SessionManager, State};

#[test]
fn frame_mapping_set_and_clone() {
    // Frame-map store + get-by-clone: mutating the returned map must not affect the
    // session, and replacing fully overwrites.
    let sm = SessionManager::new();
    assert!(sm.frame_mapping().is_empty());

    sm.set_frame_mapping(HashMap::from([(0, 100), (1, 200)]));

    let mut got = sm.frame_mapping();
    assert_eq!(got.get(&0), Some(&100));
    assert_eq!(got.get(&1), Some(&200));

    // Mutating the clone does not leak back into the session.
    got.insert(2, 999);
    assert_eq!(sm.frame_mapping().get(&2), None);

    // Replace fully overwrites.
    sm.set_frame_mapping(HashMap::from([(5, 50)]));
    let replaced = sm.frame_mapping();
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced.get(&5), Some(&50));
}

#[test]
fn last_stopped_and_exit_code_accessors() {
    let sm = SessionManager::new();
    assert_eq!(sm.last_stopped(), None);
    assert_eq!(sm.exit_code(), None);

    let info = StopInfo {
        reason: "breakpoint".to_string(),
        thread_id: 2,
        description: "hit".to_string(),
        hit_breakpoint_ids: vec![7],
    };
    sm.set_last_stopped(info.clone());
    assert_eq!(sm.last_stopped(), Some(info));

    sm.set_exit_code(42);
    assert_eq!(sm.exit_code(), Some(42));
}

#[tokio::test]
async fn pump_appends_output() {
    // Go `SetOutputHandler` — output events land in the OutputBuffer in order.
    let sm = Arc::new(SessionManager::new());
    let gen = sm.generation();

    let events: BoxStream<'static, BackendEvent> = stream::iter(vec![
        BackendEvent::Output {
            category: "stdout".to_string(),
            text: "hello ".to_string(),
        },
        BackendEvent::Output {
            category: "stderr".to_string(),
            text: "oops".to_string(),
        },
    ])
    .boxed();

    let handle = spawn_event_pump(events, Arc::clone(&sm), gen);
    handle.await.expect("pump task");

    let entries = sm.output_buffer().drain();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        (entries[0].category.as_str(), entries[0].text.as_str()),
        ("stdout", "hello ")
    );
    assert_eq!(
        (entries[1].category.as_str(), entries[1].text.as_str()),
        ("stderr", "oops")
    );
}

#[tokio::test]
async fn pump_sets_terminated_with_exit_code() {
    // Go `onTerminated` — a Terminated event records the exit code and flips state.
    let sm = Arc::new(SessionManager::new());
    sm.set_state(State::Running);
    let gen = sm.generation();

    let events: BoxStream<'static, BackendEvent> =
        stream::iter(vec![BackendEvent::Terminated { code: Some(3) }]).boxed();

    let handle = spawn_event_pump(events, Arc::clone(&sm), gen);
    handle.await.expect("pump task");

    assert_eq!(sm.state(), State::Terminated);
    assert_eq!(sm.exit_code(), Some(3));
}

#[tokio::test]
async fn pump_terminated_without_exit_code_leaves_exit_code_unset() {
    // A Terminated with no code still flips state but does not record an exit code.
    let sm = Arc::new(SessionManager::new());
    sm.set_state(State::Running);
    let gen = sm.generation();

    let events: BoxStream<'static, BackendEvent> =
        stream::iter(vec![BackendEvent::Terminated { code: None }]).boxed();

    spawn_event_pump(events, Arc::clone(&sm), gen)
        .await
        .expect("pump task");

    assert_eq!(sm.state(), State::Terminated);
    assert_eq!(sm.exit_code(), None);
}

#[tokio::test]
async fn pump_generation_guard_drops_stale_terminated() {
    // Design Decision 6: a Terminated arriving after a concurrent disconnect (which
    // bumped the generation + reset to idle) must NOT flip state back to terminated.
    //
    // Deterministic ordering: capture the spawn-time generation, then simulate the
    // disconnect (reset bumps the generation) BEFORE the terminated event is delivered.
    let sm = Arc::new(SessionManager::new());
    sm.set_state(State::Running);
    let stale_gen = sm.generation();

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<BackendEvent>();
    let events: BoxStream<'static, BackendEvent> = futures::stream::unfold(
        rx,
        |mut rx| async move { rx.recv().await.map(|ev| (ev, rx)) },
    )
    .boxed();

    let handle = spawn_event_pump(events, Arc::clone(&sm), stale_gen);

    // Concurrent disconnect: reset bumps the generation and returns to idle.
    sm.reset();
    assert_eq!(sm.state(), State::Idle);
    assert_ne!(sm.generation(), stale_gen);

    // Now deliver the stale terminated event and end the stream.
    tx.send(BackendEvent::Terminated { code: Some(9) })
        .expect("send terminated");
    drop(tx);

    handle.await.expect("pump task");

    // The guard dropped the stale event: state stays idle, no exit code recorded.
    assert_eq!(sm.state(), State::Idle);
    assert_eq!(sm.exit_code(), None);
}

#[tokio::test]
async fn pump_processes_output_then_terminated() {
    // The pump drains a mixed stream in order: output appended, then state terminated.
    let sm = Arc::new(SessionManager::new());
    sm.set_state(State::Running);
    let gen = sm.generation();

    let events: BoxStream<'static, BackendEvent> = stream::iter(vec![
        BackendEvent::Output {
            category: "console".to_string(),
            text: "starting\n".to_string(),
        },
        BackendEvent::Terminated { code: Some(0) },
    ])
    .boxed();

    spawn_event_pump(events, Arc::clone(&sm), gen)
        .await
        .expect("pump task");

    assert_eq!(sm.state(), State::Terminated);
    assert_eq!(sm.exit_code(), Some(0));
    let entries = sm.output_buffer().drain();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, "starting\n");
}
