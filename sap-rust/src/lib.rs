//! SAP (Snapshot App Protocol) JSON-RPC 2.0 server layer.
//!
//! See `README.md` for what's real vs. mocked, and
//! `memory/head/gen/rust-fork/{01-jsonrpc-spec,02-rust-embedding,05-multi-client-concurrency}.md`
//! for the design this crate implements.

pub mod backend;
pub mod framing;
pub mod protocol;
pub mod server;

/// Generic media-tooling helpers (ffprobe probing, melt binary resolution,
/// codec normalization, job-map pruning) shared by `FfiBackend`. Formerly
/// lived inside a since-removed standalone `MltBackend` (a Qt/Shotcut-free
/// reimplementation of the project model); only the parts with no
/// dependency on that model survived the removal. See `media_tools.rs`.
pub mod media_tools;

#[cfg(feature = "real_ffi")]
pub mod ffi;

#[cfg(feature = "real_ffi")]
pub mod ffi_backend;
