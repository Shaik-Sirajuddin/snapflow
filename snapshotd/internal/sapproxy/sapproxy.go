// Package sapproxy implements snapshotd's generic proxy to a project's
// running sap-rust process, per 06-daemon-mcp-proxy.md's "MCP is one
// access-point adapter" correction extended to raw SDP clients too: both
// adapters forward project-scoped method calls through this one package.
//
// Wire protocol: a Content-Length-framed ("LSP-style") JSON-RPC 2.0 client,
// matching sap-rust's own transport exactly (see sap-rust/src/framing.rs) --
// this is deliberately NOT the newline-delimited framing internal/sdp uses
// for the daemon's own control socket. sap-rust speaks Content-Length
// framing to its clients; snapshotd's control socket speaks
// newline-delimited JSON to *its* clients (CLI, raw SDP clients, the MCP
// adapter) -- this package is the seam where the daemon, acting as a SAP
// client itself, switches wire formats to talk to the child process it
// launched.
//
// Genericity: every method name and params shape that passes through this
// package is completely opaque. Router.Call and Conn.Call never inspect,
// validate, or special-case anything about method (other than routing
// "project.select" through Bind so the connection pool and session binding
// stay consistent) -- this is what lets the proxy forward sap-rust's full
// project.*/edit.*/playlist.*/filter.*/transitions.*/generator.*/file.*/
// jobs.*/playback.*/subtitles.* surface (01-jsonrpc-spec.md) without this
// package (or snapshotd generally) needing to know that surface at all, or
// be updated when it grows.
//
// Pooling: Router holds exactly one Conn per project (not per session),
// shared by every session bound to that project, per 06's pooling
// requirement -- opening a second OS-level connection to the same
// sap-rust instance for a second concurrently-connected agent would be
// wasteful and would also fragment sap-rust's own per-project notification
// broadcast (each of *our* SAP connections gets every notification for the
// project it selected; fanning that back out to N snapshotd-side sessions
// is this package's job, not sap-rust's).
package sapproxy
