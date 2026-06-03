//! OutputBuffer append/drain/eviction (Spec FR-12).
//!
//! Mirrors Go `session_test.go`: `TestOutputBufferAppendDrain`,
//! `TestOutputBufferTruncation`, `TestOutputBufferConcurrent`. The single-oversize-entry
//! vector is the plan's explicit addition (append-before-evict edge case).

use std::sync::Arc;
use std::thread;

use crate::{OutputBuffer, OutputEntry};

const MAX_SIZE: usize = 1_048_576;

#[test]
fn append_then_drain_fifo() {
    // Go `TestOutputBufferAppendDrain`.
    let buf = OutputBuffer::new();

    buf.append("stdout", "line 1\n");
    buf.append("stderr", "error\n");
    buf.append("console", "info\n");

    let entries = buf.drain();
    assert_eq!(
        entries,
        vec![
            OutputEntry {
                category: "stdout".to_string(),
                text: "line 1\n".to_string()
            },
            OutputEntry {
                category: "stderr".to_string(),
                text: "error\n".to_string()
            },
            OutputEntry {
                category: "console".to_string(),
                text: "info\n".to_string()
            },
        ]
    );

    // A second drain is empty.
    assert!(buf.drain().is_empty());
}

#[test]
fn truncation_marker_and_size_bound() {
    // Go `TestOutputBufferTruncation` — append >1 MiB, expect a prepended marker and a
    // post-marker total at/under the cap.
    let buf = OutputBuffer::new();

    let chunk = "x".repeat(1000);
    for _ in 0..1100 {
        buf.append("stdout", &chunk);
    }

    let entries = buf.drain();
    assert!(!entries.is_empty());

    assert_eq!(entries[0].category, "console");
    assert_eq!(entries[0].text, "[output truncated]");

    let total: usize = entries[1..]
        .iter()
        .map(|e| e.category.len() + e.text.len())
        .sum();
    assert!(total <= MAX_SIZE, "post-truncation total {total} > cap");

    // A subsequent drain is empty with no marker (the flag was cleared).
    assert!(buf.drain().is_empty());
}

#[test]
fn single_oversize_entry_is_appended_then_evicted() {
    // Plan addition: a single entry larger than the cap is appended (append-before-evict)
    // and then immediately evicted, leaving the buffer empty with truncated=true.
    let buf = OutputBuffer::new();

    let huge = "y".repeat(MAX_SIZE + 1);
    buf.append("stdout", &huge);

    // The only surviving content on drain is the truncation marker.
    let entries = buf.drain();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].category, "console");
    assert_eq!(entries[0].text, "[output truncated]");

    // Idempotent: the next drain is empty.
    assert!(buf.drain().is_empty());
}

#[test]
fn drain_empty_non_truncated_returns_nothing() {
    // Spec FR-12.3 — draining an empty, non-truncated buffer returns nothing; idempotent.
    let buf = OutputBuffer::new();
    assert!(buf.drain().is_empty());
    assert!(buf.drain().is_empty());
}

#[test]
fn entry_exactly_at_cap_is_retained() {
    // Strict `>` eviction: an entry whose size equals the cap is not evicted.
    let buf = OutputBuffer::new();
    let exact = "z".repeat(MAX_SIZE);
    buf.append("", &exact);

    let entries = buf.drain();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text.len(), MAX_SIZE);
}

#[test]
fn concurrent_append_and_drain_is_race_clean() {
    // Go `TestOutputBufferConcurrent` — concurrent writers + drainers must not race.
    // (Run under ThreadSanitizer for the race-clean assertion; without it this still
    // exercises the locking and must complete without panic.)
    let buf = Arc::new(OutputBuffer::new());

    const WRITERS: usize = 10;
    const ENTRIES_PER_WRITER: usize = 100;
    const DRAINERS: usize = 3;

    let mut handles = Vec::new();

    for _ in 0..WRITERS {
        let b = Arc::clone(&buf);
        handles.push(thread::spawn(move || {
            for _ in 0..ENTRIES_PER_WRITER {
                b.append("stdout", "data");
            }
        }));
    }

    for _ in 0..DRAINERS {
        let b = Arc::clone(&buf);
        handles.push(thread::spawn(move || {
            for _ in 0..50 {
                let _ = b.drain();
            }
        }));
    }

    for h in handles {
        h.join().expect("thread panicked");
    }
}
