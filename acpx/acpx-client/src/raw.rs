//! Thin re-export of raw ACP client primitives.
//!
//! Intentionally near-zero logic (see
//! `03-crate-and-folder-layout.md`): the "unmodified raw primitives"
//! guarantee from the goal doc is structurally enforced by keeping this
//! file free of acpx-specific behavior -- extensions only ever live in
//! `ext/`.

pub use acpx_proto::jsonrpc::{Request, RequestId, Response};
