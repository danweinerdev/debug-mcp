//! Unit tests for the pure helpers behind `find_process_by_name` (the live Toolhelp32 walk itself
//! is exercised by `windbg-backend`'s attach/wait_for tests). No Win32 needed.

use crate::process::{base_name, wide_to_string};

#[test]
fn base_name_lowercases_and_strips_one_exe_suffix() {
    assert_eq!(base_name("notepad.exe"), "notepad");
    assert_eq!(base_name("NOTEPAD.EXE"), "notepad");
    assert_eq!(base_name("Notepad"), "notepad");
    assert_eq!(base_name("notepad"), "notepad");
    // Only a single trailing `.exe` is stripped.
    assert_eq!(base_name("notepad.exe.exe"), "notepad.exe");
    // A non-`.exe` extension is preserved (just lowercased).
    assert_eq!(base_name("foo.bar"), "foo.bar");
    assert_eq!(base_name(""), "");
}

#[test]
fn wide_to_string_stops_at_nul_and_handles_unterminated() {
    // "ab\0cd" → "ab" (stops at the first NUL).
    let buf = [b'a' as u16, b'b' as u16, 0, b'c' as u16, b'd' as u16];
    assert_eq!(wide_to_string(&buf), "ab");
    // No NUL → the whole buffer is decoded.
    let buf = [b'h' as u16, b'i' as u16];
    assert_eq!(wide_to_string(&buf), "hi");
    // Empty buffer → empty string.
    assert_eq!(wide_to_string(&[]), "");
}
