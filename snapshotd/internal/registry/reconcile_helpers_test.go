package registry

import (
	"net"
	"time"

	"snapshotd/internal/health"
)

// net_listenUnix and acceptLoop set up a bare-bones Unix socket listener
// standing in for a real SAP child process, so tests can exercise the
// socket-health-check branch of the reconciler without needing sap-rust.
func net_listenUnix(path string) (net.Listener, error) {
	return net.Listen("unix", path)
}

func acceptLoop(ln net.Listener) {
	for {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		conn.Close()
	}
}

func realSocketHealthy(path string, timeout time.Duration) bool {
	return health.SocketResponsive(path, timeout)
}
