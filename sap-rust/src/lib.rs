//! SAP (Snapshot App Protocol) JSON-RPC 2.0 server layer.
//!
//! See `README.md` for what's real vs. mocked, and
//! `memory/head/gen/rust-fork/{01-jsonrpc-spec,02-rust-embedding,05-multi-client-concurrency}.md`
//! for the design this crate implements.

pub mod backend;
pub mod framing;
pub mod protocol;
pub mod server;

/// Third `Backend` implementor: real MLT XML + `melt`/`ffprobe` shellouts,
/// no Qt/live-Shotcut-process dependency. See `mlt_backend.rs` for what's
/// real vs. simulated.
pub mod mlt_backend;

#[cfg(feature = "real_ffi")]
pub mod ffi;

#[cfg(feature = "real_ffi")]
pub mod ffi_backend;
