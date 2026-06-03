//! Unit tests needing crate internals (the detect [`Env`] seam). Behavioral tests that
//! only need the public API (subprocess ring/spawn, handshake, ops) live in `tests/`.

mod detect;
mod event_stream;
