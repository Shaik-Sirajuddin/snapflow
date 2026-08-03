//go:build windows

package transport

import (
	"context"
	"net"
	"os"
	"strings"
	"time"

	"github.com/Microsoft/go-winio"
)

func listen(endpoint string) (net.Listener, error) {
	// Byte mode is intentional: SDP framing is newline-delimited and must not
	// depend on Windows message boundaries.
	return winio.ListenPipe(endpoint, &winio.PipeConfig{
		MessageMode:        false,
		InputBufferSize:    64 * 1024,
		OutputBufferSize:   64 * 1024,
		SecurityDescriptor: "D:P(A;;GA;;;OW)",
	})
}

func dialTimeout(endpoint string, timeout time.Duration) (net.Conn, error) {
	return winio.DialPipe(endpoint, &timeout)
}
func dialContext(ctx context.Context, endpoint string) (net.Conn, error) {
	return winio.DialPipeContext(ctx, endpoint)
}

func removeStale(string) error { return nil }

func defaultControlEndpoint(_ string) string {
	return `\\.\pipe\snapflow-` + windowsUserScope() + `-control`
}

func defaultSAPEndpoint(_, instanceID string) string {
	return `\\.\pipe\snapflow-` + windowsUserScope() + `-sap-` + pipeComponent(instanceID)
}

func windowsUserScope() string {
	scope := os.Getenv("USERNAME")
	if scope == "" {
		scope = os.Getenv("USER")
	}
	if scope == "" {
		scope = "default"
	}
	return pipeComponent(scope)
}

func pipeComponent(value string) string {
	var b strings.Builder
	for _, c := range value {
		if (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '-' {
			b.WriteRune(c)
		} else {
			b.WriteByte('-')
		}
	}
	return b.String()
}
