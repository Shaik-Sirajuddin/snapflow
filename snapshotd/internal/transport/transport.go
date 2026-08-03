// Package transport contains the platform-specific local IPC adapters used by
// snapshotd. Callers exchange endpoint strings with this package, but never
// select an OS transport themselves.
package transport

import (
	"context"
	"net"
	"path/filepath"
	"time"
)

// Listen binds a local endpoint using the native transport for this build.
func Listen(endpoint string) (net.Listener, error) { return listen(endpoint) }

// DialTimeout connects to a local endpoint using the native transport.
func DialTimeout(endpoint string, timeout time.Duration) (net.Conn, error) {
	return dialTimeout(endpoint, timeout)
}

func DialContext(ctx context.Context, endpoint string) (net.Conn, error) {
	return dialContext(ctx, endpoint)
}

// RemoveStale removes a stale Unix endpoint. Named pipes have no filesystem
// entry and therefore deliberately require no cleanup.
func RemoveStale(endpoint string) error { return removeStale(endpoint) }

// DefaultControlEndpoint returns the stable per-user control endpoint.
func DefaultControlEndpoint(home string) string { return defaultControlEndpoint(home) }

// DefaultSAPEndpoint returns an opaque per-instance endpoint.  The caller
// supplies a stable instance identifier; the platform adapter chooses a
// filesystem socket or named pipe representation.
func DefaultSAPEndpoint(runDir, instanceID string) string {
	return defaultSAPEndpoint(runDir, instanceID)
}

// defaultSAPEndpointFallback is only used by platforms that do not need a
// special endpoint representation.
func defaultSAPEndpointFallback(runDir, instanceID string) string {
	return filepath.Join(runDir, instanceID+".sock")
}
