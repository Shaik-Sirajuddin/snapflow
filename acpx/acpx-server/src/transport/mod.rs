//! Transport implementations. Phase 1: `stdio` only. Phase 2 step 11 adds
//! `http`/`ws` alongside it -- see `04-phased-plan.md`.

pub mod http;
pub mod stdio;
pub mod ws;
