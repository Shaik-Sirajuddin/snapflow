//! Sqlite-backed persistence for session metadata + transcripts, written
//! asynchronously off the routing hot path. Phase 2 step 10 -- stub for now.

pub mod transcripts;

pub const SCHEMA_SQL: &str = include_str!("schema.sql");
