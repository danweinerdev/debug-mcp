//! Non-live unit tests for the `engine` module's task-2.4 surface that need no DbgEng session.
//!
//! Live go/step/break_in/interrupt coverage lives in `tests/execution.rs`; here we assert the
//! static guarantees that do not require a target — chiefly that [`InterruptHandle`] is `Send`
//! (the one type in this crate intended to cross a thread boundary; the R4 flag-only design rests
//! on it being soundly `Send` with no `unsafe`).

use crate::InterruptHandle;

/// Compile-time proof that `InterruptHandle` is `Send`. It is the only piece of this crate that is
/// moved to another thread (the off-thread interrupter); were it accidentally made `!Send` (e.g.
/// by holding a COM interface), the off-thread interrupt seam would not compile. This is a
/// type-level assertion — it has no runtime body.
#[test]
fn interrupt_handle_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<InterruptHandle>();
}
