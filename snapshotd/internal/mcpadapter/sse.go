package mcpadapter

import (
	"context"

	"github.com/mark3labs/mcp-go/server"
)

// SSEServer wraps mark3labs/mcp-go's SSE transport with a Start/Shutdown
// pair matching the rest of this codebase's lifecycle style
// (internal/sdp.Server).
type SSEServer struct {
	addr string
	sse  *server.SSEServer
}

// NewSSEServer builds an SSE-served MCP adapter listening on addr (e.g.
// "127.0.0.1:7777"), exposing the daemon.* tools backed by h.
func NewSSEServer(h Handler, addr string) *SSEServer {
	mcpServer := New(h)
	return &SSEServer{
		addr: addr,
		sse:  server.NewSSEServer(mcpServer),
	}
}

// Start blocks serving SSE connections until Shutdown is called.
func (s *SSEServer) Start() error {
	return s.sse.Start(s.addr)
}

// Shutdown gracefully stops the SSE listener.
func (s *SSEServer) Shutdown(ctx context.Context) error {
	return s.sse.Shutdown(ctx)
}
