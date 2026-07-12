package mcpadapter

import (
	"context"
	"net/http"

	"github.com/mark3labs/mcp-go/server"
)

// SSEServer wraps mark3labs/mcp-go's SSE and Streamable HTTP transports
// behind a single listener, with a Start/Shutdown pair matching the rest of
// this codebase's lifecycle style (internal/sdp.Server).
//
// Two transports are served on the same addr because MCP clients disagree
// on which one they speak: the legacy "HTTP+SSE" transport (GET /sse then
// POST /message?sessionId=...) is what this adapter originally shipped, but
// newer clients (e.g. Codex CLI's rmcp client) only implement the 2025-03-26
// "Streamable HTTP" transport, which POSTs JSON-RPC directly to a single
// endpoint (/mcp here). Serving both avoids a client-specific config split.
type SSEServer struct {
	addr       string
	sse        *server.SSEServer
	streamable *server.StreamableHTTPServer
	httpServer *http.Server
}

// NewSSEServer builds an SSE-served MCP adapter listening on addr (e.g.
// "127.0.0.1:7777"), exposing the daemon.* tools backed by h over both the
// legacy SSE transport (GET /sse, POST /message) and the Streamable HTTP
// transport (POST/GET/DELETE /mcp).
func NewSSEServer(h Handler, addr string) *SSEServer {
	mcpServer := New(h)
	return &SSEServer{
		addr:       addr,
		sse:        server.NewSSEServer(mcpServer),
		streamable: server.NewStreamableHTTPServer(mcpServer, server.WithEndpointPath("/mcp")),
	}
}

// Start blocks serving SSE and Streamable HTTP connections until Shutdown is
// called.
func (s *SSEServer) Start() error {
	mux := http.NewServeMux()
	mux.Handle("/mcp", s.streamable)
	mux.Handle("/", s.sse)
	s.httpServer = &http.Server{Addr: s.addr, Handler: mux}
	err := s.httpServer.ListenAndServe()
	if err == http.ErrServerClosed {
		return nil
	}
	return err
}

// Shutdown gracefully stops both transports' listener.
func (s *SSEServer) Shutdown(ctx context.Context) error {
	s.sse.CloseSessions()
	if s.httpServer == nil {
		return nil
	}
	return s.httpServer.Shutdown(ctx)
}

// StreamableHTTPPath returns the URL path Streamable HTTP clients should
// POST to (mounted alongside the legacy SSE transport on the same addr).
func (s *SSEServer) StreamableHTTPPath() string {
	return "/mcp"
}
