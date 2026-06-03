//! Hex-dump + output-entry formatter parity (Go `memory_test.go` / `output_test.go`).

use mcp_session::OutputEntry;
use serde_json::json;

use crate::{format_hex_dump, format_output_entries};

fn entry(category: &str, text: &str) -> OutputEntry {
    OutputEntry {
        category: category.to_string(),
        text: text.to_string(),
    }
}

#[test]
fn hex_dump_full_row() {
    // Go TestFormatHexDumpFullRow — exact string.
    let data = b"Hello World!\x00\x00\x00\x00";
    let result = format_hex_dump(data, 0x7fff_5000);
    let expected =
        "0x7fff5000: 48 65 6c 6c 6f 20 57 6f  72 6c 64 21 00 00 00 00  |Hello World!....|";
    assert_eq!(result, expected);
}

#[test]
fn hex_dump_partial_row() {
    // Go TestFormatHexDumpPartialRow.
    let data = [0x41u8, 0x42, 0x43, 0x00, 0xff];
    let result = format_hex_dump(&data, 0x1000);
    assert!(result.starts_with("0x00001000: "), "got: {result:?}");
    assert!(result.contains("41 42 43 00 ff"), "got: {result:?}");
    assert!(result.ends_with("|ABC..           |"), "got: {result:?}");
}

#[test]
fn hex_dump_partial_row_exact() {
    // Full byte-exact pin of the partial-row layout (5 bytes): 11 missing hex columns
    // (3 spaces each, with the extra group-separator space before column 8) and the
    // ASCII gutter padded with single spaces.
    let data = [0x41u8, 0x42, 0x43, 0x00, 0xff];
    let result = format_hex_dump(&data, 0x1000);
    let expected =
        "0x00001000: 41 42 43 00 ff                                    |ABC..           |";
    assert_eq!(result, expected);
}

#[test]
fn hex_dump_multiple_rows() {
    // Go TestFormatHexDumpMultipleRows — 20 bytes = 1 full + 1 partial row.
    let data: Vec<u8> = (0u8..20).collect();
    let result = format_hex_dump(&data, 0x0);
    let lines: Vec<&str> = result.split('\n').collect();
    assert_eq!(lines.len(), 2, "expected 2 lines, got: {result:?}");
    assert!(lines[0].starts_with("0x00000000: "), "got: {:?}", lines[0]);
    assert!(lines[1].starts_with("0x00000010: "), "got: {:?}", lines[1]);
}

#[test]
fn hex_dump_empty() {
    // Go TestFormatHexDumpEmpty.
    assert_eq!(format_hex_dump(&[], 0x0), "");
}

#[test]
fn output_entries_empty() {
    // Go TestFormatOutputEntriesEmpty.
    let result = format_output_entries(&[]);
    assert_eq!(result, json!({ "output": "", "count": 0 }));
}

#[test]
fn output_entries_groups_by_category() {
    // Go TestFormatOutputEntriesGroupsByCategory.
    let entries = [
        entry("stdout", "line1\n"),
        entry("stderr", "err1\n"),
        entry("console", "dbg1\n"),
        entry("stdout", "line2\n"),
    ];
    let result = format_output_entries(&entries);
    assert_eq!(result["count"], json!(4));
    assert_eq!(result["stdout"], json!("line1\nline2\n"));
    assert_eq!(result["stderr"], json!("err1\n"));
    assert_eq!(result["console"], json!("dbg1\n"));
    assert!(result.get("output").is_none());
}

#[test]
fn output_entries_omits_missing_categories() {
    // Go TestFormatOutputEntriesOmitsMissingCategories.
    let entries = [entry("stdout", "only stdout\n")];
    let result = format_output_entries(&entries);
    assert_eq!(result["count"], json!(1));
    assert_eq!(result["stdout"], json!("only stdout\n"));
    assert!(result.get("stderr").is_none());
    assert!(result.get("console").is_none());
}

#[test]
fn output_entries_unknown_category_goes_to_console() {
    // Default bucket = console: any category that is not exactly stdout/stderr.
    let entries = [entry("telemetry", "x\n"), entry("important", "y\n")];
    let result = format_output_entries(&entries);
    assert_eq!(result["count"], json!(2));
    assert_eq!(result["console"], json!("x\ny\n"));
    assert!(result.get("stdout").is_none());
    assert!(result.get("stderr").is_none());
}
