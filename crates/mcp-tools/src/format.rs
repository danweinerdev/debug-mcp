//! Pure formatters shared by the inspection/memory/output handlers.
//!
//! - [`format_hex_dump`] is a byte-exact port of Go `memory.go`'s `formatHexDump`
//!   (Spec FR-13.1).
//! - [`format_output_entries`] is a port of Go `output.go`'s `formatOutputEntries`
//!   (Spec FR-12.5), returning a `serde_json::Value` object.

use std::fmt::Write as _;

use mcp_session::OutputEntry;
use serde_json::{json, Map, Value};

/// Format raw bytes as a hex dump with 16 bytes per row (Spec FR-13.1, Go
/// `formatHexDump`). Empty input yields an empty string; rows are joined by `\n`
/// with no trailing newline.
///
/// Each row is `0x%08x: ` (the row's start address), then 16 byte columns — present
/// bytes as `%02x ` and missing bytes as three spaces, with one extra space before
/// column 8 — then ` |`, the ASCII gutter (printable `0x20..=0x7e` as themselves,
/// others `.`, missing positions a single space), then `|`.
pub fn format_hex_dump(data: &[u8], start_addr: u64) -> String {
    let mut sb = String::new();
    let mut offset = 0;
    while offset < data.len() {
        // Address column. `as u64` matches Go's `startAddr + uint64(offset)` (offset
        // is a non-negative slice index, so the cast is lossless on real inputs).
        let _ = write!(sb, "0x{:08x}: ", start_addr.wrapping_add(offset as u64));

        let end = (offset + 16).min(data.len());
        let row = &data[offset..end];

        for i in 0..16 {
            if i == 8 {
                sb.push(' ');
            }
            if i < row.len() {
                let _ = write!(sb, "{:02x} ", row[i]);
            } else {
                sb.push_str("   ");
            }
        }

        sb.push_str(" |");
        for i in 0..16 {
            if i < row.len() {
                let b = row[i];
                if (0x20..=0x7e).contains(&b) {
                    sb.push(b as char);
                } else {
                    sb.push('.');
                }
            } else {
                sb.push(' ');
            }
        }
        sb.push('|');

        if offset + 16 < data.len() {
            sb.push('\n');
        }
        offset += 16;
    }
    sb
}

/// Group output entries by category into a JSON object (Spec FR-12.5, Go
/// `formatOutputEntries`).
///
/// Empty input → `{"output":"","count":0}`. Otherwise the entries are grouped into
/// `stdout`/`stderr`/`console` buckets — every category that is not exactly `stdout`
/// or `stderr` goes to `console` — and the object always carries `count` plus a
/// bucket key only when that bucket is non-empty (no `output` key in this form).
pub fn format_output_entries(entries: &[OutputEntry]) -> Value {
    if entries.is_empty() {
        return json!({ "output": "", "count": 0 });
    }

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut console = String::new();
    for e in entries {
        match e.category.as_str() {
            "stdout" => stdout.push_str(&e.text),
            "stderr" => stderr.push_str(&e.text),
            _ => console.push_str(&e.text),
        }
    }

    let mut map = Map::new();
    map.insert("count".to_string(), Value::from(entries.len()));
    if !stdout.is_empty() {
        map.insert("stdout".to_string(), Value::from(stdout));
    }
    if !stderr.is_empty() {
        map.insert("stderr".to_string(), Value::from(stderr));
    }
    if !console.is_empty() {
        map.insert("console".to_string(), Value::from(console));
    }
    Value::Object(map)
}
