//! Sqlite-backed persistence for session metadata + transcripts, written
//! asynchronously off the routing hot path. Phase 2 step 10.
//!
//! ## Driver choice: `rusqlite` (with the `bundled` feature)
//!
//! Picked over `sqlx` for three reasons:
//! 1. `bundled` vendors sqlite itself, so there's no system libsqlite3
//!    dependency to install on dev machines or in CI -- matches the task's
//!    explicit callout of this concern.
//! 2. `sqlx`'s sqlite driver is a single-writer connection wrapped in an
//!    async mutex internally anyway (sqlite has no real concurrent-writer
//!    story), and its compile-time query-checking macros need a
//!    `DATABASE_URL` / offline query cache at build time, which is
//!    unnecessary ceremony for this crate's small, hand-written query set.
//! 3. `rusqlite` is a synchronous driver, which is actually the right fit
//!    here: see [`store`] for how the sync connection is combined with
//!    `tokio::task::spawn_blocking` to expose an async, non-blocking,
//!    cheaply-`Clone`-able API without ever holding the tokio runtime up on
//!    file I/O.

pub mod error;
pub mod sessions;
pub mod store;
pub mod transcripts;

pub use error::PersistenceError;
pub use sessions::{RecoveryStatusCounts, SessionRecord};
pub use store::PersistenceStore;
pub use transcripts::{Direction, TranscriptRecord};

/// Single source of truth for the sqlite schema, loaded via `include_str!`
/// per `03-crate-and-folder-layout.md`. Applied via `execute_batch` on every
/// [`PersistenceStore::open`]/[`PersistenceStore::open_in_memory`] call --
/// idempotent because every statement is `CREATE TABLE/INDEX IF NOT EXISTS`.
pub const SCHEMA_SQL: &str = include_str!("schema.sql");
