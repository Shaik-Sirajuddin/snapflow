//! Cargo integration-test entrypoint for the real snapflow session E2E.
//!
//! The scenario lives in `panel-rust/e2e` so support files and Cargo-discovered
//! integration tests have separate homes. This wrapper keeps standard
//! `cargo test --test ...` discovery and reporting.

#[path = "../e2e/snapflow_session_derived_state_e2e.rs"]
mod snapflow_session_derived_state_e2e;
