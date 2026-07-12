//! Transport implementations. Phase 1: `stdio` only. Phase 2 step 11 adds
//! `http`/`ws` alongside it -- see `04-phased-plan.md`.

pub mod http;
pub mod stdio;
pub mod ws;

// Re-exported so `main.rs` can call `transport::serve(...)` / build a
// `transport::SharedRouter` without reaching into `transport::http`
// directly.
pub use http::{serve, SharedRouter};
