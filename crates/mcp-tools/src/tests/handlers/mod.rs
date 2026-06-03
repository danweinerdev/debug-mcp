//! Handler/server tests over the fake backend (task 5.3/5.4/5.5). Each module mirrors the
//! Go `internal/tools/*_test.go` guard/shape/error assertions plus the design's
//! load-bearing details (event-pump-before-launch, generation guard, pause-during-continue
//! concurrency, the 21-tool registration).

mod breakpoints;
mod errors;
mod execution;
mod guards;
mod inspection;
mod lifecycle;
mod memory;
mod schema;
pub mod support;
