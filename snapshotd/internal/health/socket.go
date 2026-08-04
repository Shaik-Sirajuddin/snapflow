// Package health contains process- and endpoint-liveness primitives shared by
// the startup reconciler (internal/registry/reconcile.go) and the process
// manager (internal/procmgr).
package health

import (
	"time"

	"snapshotd/internal/transport"
)

// SocketResponsive reports whether a local endpoint accepts a connection
// within timeout. This is the "pragmatic v1 simplification"
// health check described throughout snapshotd: rather than a full app-level
// heartbeat RPC (08-lifecycle-and-cli.md's daemon.heartbeat, which requires
// the SAP child to implement that method), we simply confirm something is
// listening and accepting connections on the socket path. That distinguishes
// "process crashed / never started" from "process is up," though per 08's
// two-liveness-signal table it would NOT catch a hung-but-still-accepting
// child -- a real app-level heartbeat is the documented follow-up.
func SocketResponsive(path string, timeout time.Duration) bool {
	conn, err := transport.DialTimeout(path, timeout)
	if err != nil {
		return false
	}
	_ = conn.Close()
	return true
}
