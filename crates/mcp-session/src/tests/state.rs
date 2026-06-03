//! State machine + guards + generation + reset (Spec FR-4).
//!
//! Mirrors Go `session_test.go`: `TestStateString`, `TestStateTransitions`,
//! `TestCheckStateAllowed`, `TestCheckStateErrorMessages`, `TestReset`. The
//! generation-bump assertion is the Rust-only addition (design Decision 6).

use std::collections::HashMap;

use debugger_core::StopInfo;

use crate::{SessionManager, State};

#[test]
fn state_string() {
    // Go `TestStateString`.
    assert_eq!(State::Idle.to_string(), "idle");
    assert_eq!(State::Configuring.to_string(), "configuring");
    assert_eq!(State::Stopped.to_string(), "stopped");
    assert_eq!(State::Running.to_string(), "running");
    assert_eq!(State::Terminated.to_string(), "terminated");
}

#[test]
fn state_transitions_any_to_any() {
    // Go `TestStateTransitions` — transitions are unconditional (no validation).
    let sm = SessionManager::new();
    assert_eq!(sm.state(), State::Idle);

    sm.set_state(State::Configuring);
    assert_eq!(sm.state(), State::Configuring);

    sm.set_state(State::Stopped);
    assert_eq!(sm.state(), State::Stopped);

    sm.set_state(State::Running);
    assert_eq!(sm.state(), State::Running);

    sm.set_state(State::Stopped);
    assert_eq!(sm.state(), State::Stopped);

    sm.set_state(State::Terminated);
    assert_eq!(sm.state(), State::Terminated);

    sm.reset();
    assert_eq!(sm.state(), State::Idle);
}

#[test]
fn check_state_allowed() {
    // Go `TestCheckStateAllowed`.
    let sm = SessionManager::new();
    sm.set_state(State::Stopped);

    assert!(sm.check_state(&[State::Stopped]).is_ok());
    assert!(sm.check_state(&[State::Running]).is_err());
}

#[test]
fn check_state_idle_message() {
    // Go `TestCheckStateErrorMessages` — the idle-specific string (exact).
    let sm = SessionManager::new();
    let err = sm
        .check_state(&[State::Stopped, State::Running])
        .expect_err("idle should fail the stopped/running guard");
    assert_eq!(
        err,
        "no debug session active. Use 'launch' or 'attach' first."
    );
}

#[test]
fn check_state_running_message() {
    // Go `TestCheckStateErrorMessages` — the running-specific string (exact).
    let sm = SessionManager::new();
    sm.set_state(State::Running);
    let err = sm
        .check_state(&[State::Stopped])
        .expect_err("running should fail the stopped guard");
    assert_eq!(err, "process is running. Use 'pause' first.");
}

#[test]
fn check_state_generic_message() {
    // Go `TestCheckStateErrorMessages` — the generic unquoted list, joined by ", ".
    let sm = SessionManager::new();
    sm.set_state(State::Terminated);
    let err = sm
        .check_state(&[State::Stopped, State::Running])
        .expect_err("terminated should fail the stopped/running guard");
    assert_eq!(
        err,
        "invalid state: terminated, expected one of: stopped, running"
    );
}

#[test]
fn check_state_generic_single_allowed() {
    // The generic form with a single allowed state — no trailing separator.
    let sm = SessionManager::new();
    sm.set_state(State::Configuring);
    let err = sm
        .check_state(&[State::Stopped])
        .expect_err("configuring should fail the stopped guard");
    assert_eq!(err, "invalid state: configuring, expected one of: stopped");
}

#[test]
fn reset_restores_idle_baseline() {
    // Go `TestReset` — every field returns to its idle baseline.
    let sm = SessionManager::new();

    sm.set_state(State::Stopped);
    sm.set_program("/usr/bin/test".to_string());
    sm.set_pid(12345);
    sm.set_exit_code(1);
    sm.set_last_stopped(StopInfo {
        reason: "breakpoint".to_string(),
        thread_id: 1,
        description: String::new(),
        hit_breakpoint_ids: vec![],
    });
    sm.set_frame_mapping(HashMap::from([(0, 100), (1, 200)]));
    sm.output_buffer().append("stdout", "hello");

    sm.reset();

    assert_eq!(sm.state(), State::Idle);
    assert_eq!(sm.program(), "");
    assert_eq!(sm.pid(), 0);
    assert_eq!(sm.exit_code(), None);
    assert_eq!(sm.last_stopped(), None);
    assert!(sm.frame_mapping().is_empty());
    assert!(sm.output_buffer().drain().is_empty());
}

#[test]
fn reset_bumps_generation() {
    // Rust-only (design Decision 6): every reset advances the generation epoch.
    let sm = SessionManager::new();
    let g0 = sm.generation();

    sm.reset();
    let g1 = sm.generation();
    assert_ne!(g1, g0, "reset must bump the generation");

    sm.reset();
    let g2 = sm.generation();
    assert_ne!(g2, g1, "a second reset must bump the generation again");
}
