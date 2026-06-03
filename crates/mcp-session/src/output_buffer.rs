//! The session's output buffer (Spec FR-12). Thread-safe behind its own `Mutex`, kept
//! separate from the session `RwLock` to avoid lock coupling (Go parity).
//!
//! Go origin: `internal/session/session.go` `OutputBuffer` + `OutputEntry`.

use std::sync::Mutex;

/// The 1 MiB cap on total buffered bytes (Spec FR-12.2).
const MAX_SIZE: usize = 1_048_576;

/// One captured output line with its category. `category` is opaque pass-through
/// (`"stdout"`, `"stderr"`, `"console"`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputEntry {
    pub category: String,
    pub text: String,
}

#[derive(Debug, Default)]
struct Buffered {
    entries: Vec<OutputEntry>,
    /// Total bytes across all entries: sum of `len(category) + len(text)` (Spec FR-12.2).
    size: usize,
    truncated: bool,
}

/// A thread-safe buffer capturing debug output. Enforces a 1 MiB cap by evicting oldest
/// entries (FIFO) once the total **exceeds** the cap (strict `>`), recording a
/// truncation flag that drains surface as a `[output truncated]` marker.
#[derive(Debug, Default)]
pub struct OutputBuffer {
    inner: Mutex<Buffered>,
}

impl OutputBuffer {
    /// Create an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one entry, then evict oldest while over the cap (Spec FR-12.2,
    /// append-before-evict). A single entry larger than the cap is appended and then
    /// immediately evicted, leaving the buffer empty with `truncated=true`.
    pub fn append(&self, category: &str, text: &str) {
        let mut b = self.inner.lock().expect("output buffer mutex poisoned");

        let entry_size = category.len() + text.len();
        b.entries.push(OutputEntry {
            category: category.to_string(),
            text: text.to_string(),
        });
        b.size += entry_size;

        while b.size > MAX_SIZE && !b.entries.is_empty() {
            let dropped = b.entries.remove(0);
            b.size -= dropped.category.len() + dropped.text.len();
            b.truncated = true;
        }
    }

    /// Return all buffered entries and clear the buffer (Spec FR-12.3). When entries
    /// were evicted, a `(console, "[output truncated]")` marker is prepended and the
    /// flag reset. Returns an empty vector when the buffer is empty and not truncated;
    /// idempotent (a second drain returns nothing).
    pub fn drain(&self) -> Vec<OutputEntry> {
        let mut b = self.inner.lock().expect("output buffer mutex poisoned");

        if b.entries.is_empty() && !b.truncated {
            return Vec::new();
        }

        let mut result = Vec::with_capacity(b.entries.len() + 1);
        if b.truncated {
            result.push(OutputEntry {
                category: "console".to_string(),
                text: "[output truncated]".to_string(),
            });
            b.truncated = false;
        }
        result.append(&mut b.entries);

        b.size = 0;
        result
    }
}
