//! `EngineError` mapping: construct one from a known failing `windows::core::Error` and assert
//! its `Display` carries the context label and the `HRESULT`. Needs no live engine.

use crate::EngineError;
use windows::core::{Error, HRESULT};
use windows::Win32::Foundation::E_NOINTERFACE;

#[test]
fn display_includes_context_and_hresult() {
    let err = EngineError::op(
        "QueryInterface(IDebugControl4)",
        Error::from_hresult(E_NOINTERFACE),
    );

    let shown = err.to_string();
    assert!(
        shown.contains("QueryInterface(IDebugControl4)"),
        "Display should carry the context label: {shown}"
    );
    // E_NOINTERFACE is 0x80004002 — assert the formatted HRESULT is present.
    assert!(
        shown.contains("0x80004002"),
        "Display should carry the hresult: {shown}"
    );
    assert!(shown.contains("failed:"), "Display wording: {shown}");
}

#[test]
fn accessors_round_trip() {
    let err = EngineError::op(
        "DebugCreate",
        Error::from_hresult(HRESULT(0x8000_4005u32 as i32)),
    );
    assert_eq!(err.context(), "DebugCreate");
    assert_eq!(err.hresult(), HRESULT(0x8000_4005u32 as i32));
}
