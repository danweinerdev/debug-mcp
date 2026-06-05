//! `ComApartment` unit coverage (dedicated `src/tests/` module). These run on Windows and need
//! no live engine: they only exercise `CoInitializeEx`/`CoUninitialize` balancing on a thread.

use crate::ComApartment;

#[test]
fn init_and_drop_on_a_fresh_thread_succeeds() {
    // A worker thread starts with no COM initialized, so the first `ComApartment::new` must
    // succeed (S_OK) and `Drop` must balance it cleanly with no panic.
    let joined = std::thread::spawn(|| {
        let guard = ComApartment::new().expect("MTA init on a fresh thread");
        drop(guard);
    })
    .join();
    assert!(joined.is_ok(), "the COM-init thread must not panic");
}

#[test]
fn nested_mta_init_is_compatible() {
    // A second MTA init on the same thread is compatible (S_FALSE) and still succeeds; both
    // guards drop without panicking (each owns a balancing CoUninitialize).
    let joined = std::thread::spawn(|| {
        let outer = ComApartment::new().expect("first MTA init");
        let inner = ComApartment::new().expect("nested MTA init is compatible (S_FALSE)");
        drop(inner);
        drop(outer);
    })
    .join();
    assert!(joined.is_ok(), "nested MTA init must not panic");
}
