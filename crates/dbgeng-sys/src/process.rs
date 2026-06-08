//! Process enumeration — the Toolhelp32 snapshot walk behind `attach`'s `wait_for` mode.
//!
//! Ports the C++ `findProcessByName` (`session_tools.cpp`): take a process snapshot, walk it, and
//! return the first pid whose executable base name matches `name` (case-insensitive, with or
//! without a trailing `.exe`). This does **no** DbgEng/COM work — it is a plain Win32 Toolhelp32
//! call — so it is callable from any thread (the `windbg-backend` async side polls it directly for
//! `wait_for`, off the engine thread). The `unsafe` is confined here per the crate contract.

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

/// Find the first running process whose executable base name matches `name`, returning its pid.
///
/// Matching is case-insensitive and ignores a trailing `.exe`, so `"notepad"`, `"Notepad"`, and
/// `"notepad.exe"` all match `notepad.exe`. Returns `None` when no process matches (or the snapshot
/// could not be taken). The first match in enumeration order wins (Toolhelp32's order), matching the
/// C++ `findProcessByName` first-hit semantics.
pub fn find_process_by_name(name: &str) -> Option<u32> {
    // Normalize the query once: lowercase, strip a trailing `.exe`.
    let want = base_name(name);

    // SAFETY: `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)` returns an owned snapshot HANDLE
    // (or an error). We close it on every return path below. No pointers are retained past the call.
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(handle) => handle,
        Err(_) => return None,
    };

    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    // SAFETY: `snapshot` is the live snapshot just taken; `entry` is a valid `&mut PROCESSENTRY32W`
    // whose `dwSize` is initialized as the API requires. `Process32FirstW` fills `entry` for the
    // first process. A failure (no processes / snapshot races) means nothing to walk — close and
    // return `None`.
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_err() {
        // SAFETY: `snapshot` is the live handle from `CreateToolhelp32Snapshot`; closing it once is
        // correct and the handle is not used afterward.
        unsafe {
            let _ = CloseHandle(snapshot);
        }
        return None;
    }

    let mut found = None;
    loop {
        // `szExeFile` is a NUL-terminated wide buffer holding the process's base exe name.
        let exe = wide_to_string(&entry.szExeFile);
        if base_name(&exe) == want {
            found = Some(entry.th32ProcessID);
            break;
        }
        // SAFETY: `snapshot` is live; `entry` is the valid buffer the API advances. `Process32NextW`
        // returns an error when enumeration is exhausted (the loop ends), which is the documented
        // termination condition.
        if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
            break;
        }
    }

    // SAFETY: `snapshot` is the live handle; closed exactly once here on the success/exhaustion
    // path and never used afterward.
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    found
}

/// The comparison key for a process name: lowercased, with a trailing `.exe` stripped. So both the
/// query and each enumerated exe name fold to the same form before comparison.
pub(crate) fn base_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    lower
        .strip_suffix(".exe")
        .map(|s| s.to_string())
        .unwrap_or(lower)
}

/// Decode a NUL-terminated UTF-16 buffer (as filled in `PROCESSENTRY32W::szExeFile`) into a
/// `String`, stopping at the first NUL.
pub(crate) fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
