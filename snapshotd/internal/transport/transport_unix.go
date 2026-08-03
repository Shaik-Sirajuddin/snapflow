//go:build !windows

package transport

import (
	"context"
	"net"
	"os"
	"time"
)

func listen(endpoint string) (net.Listener, error) { return net.Listen("unix", endpoint) }
func dialTimeout(endpoint string, timeout time.Duration) (net.Conn, error) {
	return net.DialTimeout("unix", endpoint, timeout)
}
func dialContext(ctx context.Context, endpoint string) (net.Conn, error) {
	return (&net.Dialer{}).DialContext(ctx, "unix", endpoint)
}
func removeStale(endpoint string) error { return os.Remove(endpoint) }
func defaultControlEndpoint(home string) string {
	return home + string(os.PathSeparator) + "control.sock"
}
func defaultSAPEndpoint(runDir, instanceID string) string {
	return defaultSAPEndpointFallback(runDir, instanceID)
}
