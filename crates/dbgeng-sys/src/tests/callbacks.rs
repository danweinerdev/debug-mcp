//! Unit + live tests for the task-2.2 callbacks.
//!
//! - [`exception_breaks`] truth table — pure, no live target (the first/second-chance decision).
//! - [`OutputKind::from_mask`] classification — pure.
//! - Live output capture — creates a real [`Engine`], installs a sink, runs a no-target command
//!   (`version`) and a `.echo`, and asserts the OutputCallbacks wiring delivered the text both to
//!   the captured buffer and to the sink. Proves `SetOutputCallbacks` end-to-end.
//!
//! Live crash/exit *event* observation (the EventCallbacks path) is deferred to task 2.3, which
//! introduces launch and can drive `testdata/win/test_target.exe` to a breakpoint/exception/exit.

use crate::callbacks::exception_breaks;
use crate::{Engine, OutputKind};

use windows::core::s;

#[test]
fn exception_breaks_truth_table() {
    // First-chance access violation passes through (does NOT break).
    assert!(
        !exception_breaks(true, 0xC000_0005),
        "first-chance AV should pass through"
    );
    // Second-chance access violation breaks.
    assert!(
        exception_breaks(false, 0xC000_0005),
        "second-chance AV should break"
    );
    // The initial process breakpoint (0x80000003) breaks even first-chance.
    assert!(
        exception_breaks(true, 0x8000_0003),
        "first-chance initial breakpoint should break"
    );
    // And of course second-chance for the same code breaks too.
    assert!(
        exception_breaks(false, 0x8000_0003),
        "second-chance initial breakpoint should break"
    );
}

#[test]
fn output_kind_from_mask_classifies() {
    use windows::Win32::System::Diagnostics::Debug::Extensions::{
        DEBUG_OUTPUT_ERROR, DEBUG_OUTPUT_NORMAL, DEBUG_OUTPUT_PROMPT, DEBUG_OUTPUT_WARNING,
    };
    assert_eq!(
        OutputKind::from_mask(DEBUG_OUTPUT_NORMAL),
        OutputKind::Normal
    );
    assert_eq!(OutputKind::from_mask(DEBUG_OUTPUT_ERROR), OutputKind::Error);
    assert_eq!(
        OutputKind::from_mask(DEBUG_OUTPUT_WARNING),
        OutputKind::Warning
    );
    assert_eq!(
        OutputKind::from_mask(DEBUG_OUTPUT_PROMPT),
        OutputKind::Prompt
    );
    // An unrecognized bit falls back to Other carrying the raw mask.
    assert_eq!(
        OutputKind::from_mask(0x8000_0000),
        OutputKind::Other(0x8000_0000)
    );
}

#[test]
fn output_callbacks_capture_command_output() {
    let mut engine = match Engine::create() {
        Ok(e) => e,
        Err(e) => panic!("Engine::create failed: {e}"),
    };

    // `version` is a no-target DbgEng meta-command that prints the engine version banner — it
    // needs no debuggee, so it exercises the OutputCallbacks wiring without launch (task 2.3).
    engine
        .execute_raw(s!("version"))
        .expect("execute `version`");

    let captured = engine.take_output();
    assert!(
        !captured.is_empty(),
        "the `version` command should have produced captured output via OutputCallbacks"
    );
    // take_output drains, so a second drain is empty.
    assert!(
        engine.take_output().is_empty(),
        "take_output should drain the buffer"
    );

    // Now prove the sink path: install a sink that records lines, run `.echo <marker>`, and
    // assert both the buffer and the sink saw the marker. Use a shared collector behind a Mutex
    // because the sink closure must be `Send` (DbgEng may dispatch output from another thread).
    use std::sync::{Arc, Mutex};
    let sink_lines: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let sink_lines_for_closure = sink_lines.clone();
    engine.set_output_sink(Box::new(move |_kind: OutputKind, text: &str| {
        sink_lines_for_closure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push_str(text);
    }));

    let marker = "dbgeng-sys-probe-22";
    engine
        .execute_raw(s!(".echo dbgeng-sys-probe-22"))
        .expect("execute `.echo`");

    let echoed = engine.take_output();
    assert!(
        echoed.contains(marker),
        "the captured buffer should contain the echoed marker, got: {echoed:?}"
    );
    let sink_seen = sink_lines.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert!(
        sink_seen.contains(marker),
        "the sink should have received the echoed marker, got: {sink_seen:?}"
    );
}
