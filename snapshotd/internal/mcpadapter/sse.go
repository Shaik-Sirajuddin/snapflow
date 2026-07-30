package mcpadapter

import (
	"context"
	"crypto/subtle"
	"net"
	"net/http"
	"sync"
	"time"

	"github.com/mark3labs/mcp-go/server"
)

type contextTokenKey struct{}

// ContextTokenFromContext returns the opaque per-ACP-session token supplied
// by panel-rust as an HTTP MCP header.
func ContextTokenFromContext(ctx context.Context) string {
	token, _ := ctx.Value(contextTokenKey{}).(string)
	return token
}

func contextWithToken(ctx context.Context, r *http.Request) context.Context {
	return context.WithValue(ctx, contextTokenKey{}, r.Header.Get("X-Snapshotd-Context-Token"))
}

// Credentials is the current HTTP Basic Auth check for an SSEServer. A
// zero-value Credentials (Enabled == false) disables auth entirely -- the
// pre-existing, unauthenticated default.
type Credentials struct {
	Enabled  bool
	User     string
	Password string
}

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
	sessions   *streamableSessionManager
	httpServer *http.Server

	credMu sync.RWMutex
	creds  Credentials
}

// NewSSEServer builds an SSE-served MCP adapter listening on addr (e.g.
// "127.0.0.1:7777"), exposing the daemon.* tools backed by h over both the
// legacy SSE transport (GET /sse, POST /message) and the Streamable HTTP
// transport (POST/GET/DELETE /mcp).
func NewSSEServer(h Handler, addr string) *SSEServer {
	mcpServer := New(h)
	sessions := newStreamableSessionManager(time.Hour)
	return &SSEServer{
		addr: addr,
		sse:  server.NewSSEServer(mcpServer, server.WithSSEContextFunc(contextWithToken)),
		streamable: server.NewStreamableHTTPServer(mcpServer,
			server.WithEndpointPath("/mcp"),
			server.WithSessionIdManager(sessions),
			server.WithSessionIdleTTL(time.Hour),
			server.WithHTTPContextFunc(contextWithToken),
		),
		sessions: sessions,
	}
}

// SetCredentials updates the Basic Auth check applied to every request,
// effective immediately (no restart needed) -- checked fresh on each
// request via credMu, so a supervisor can call this on a live server.
func (s *SSEServer) SetCredentials(c Credentials) {
	s.credMu.Lock()
	s.creds = c
	s.credMu.Unlock()
}

func (s *SSEServer) checkAuth(r *http.Request) bool {
	s.credMu.RLock()
	c := s.creds
	s.credMu.RUnlock()
	if !c.Enabled {
		return true
	}
	// Fail closed on a malformed/corrupted credential state rather than
	// silently accepting blank Basic Auth credentials: a config with
	// Enabled=true but an empty user or password should never occur via
	// SetAuth (which requires both non-empty), but a hand-edited or
	// partially-written mcp_config.json could produce it, and without this
	// check that state would let an empty-user/empty-password request
	// through ConstantTimeCompare's equal-empty-slices match.
	if c.User == "" || c.Password == "" {
		return false
	}
	user, pass, ok := r.BasicAuth()
	if !ok {
		return false
	}
	userOK := subtle.ConstantTimeCompare([]byte(user), []byte(c.User)) == 1
	passOK := subtle.ConstantTimeCompare([]byte(pass), []byte(c.Password)) == 1
	return userOK && passOK
}

func (s *SSEServer) requireAuth(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !s.checkAuth(r) {
			w.Header().Set("WWW-Authenticate", `Basic realm="snapshotd MCP"`)
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		next.ServeHTTP(w, r)
	})
}

// Handler returns the full auth-wrapped HTTP handler this server would
// listen with, without binding a socket -- used by Start and directly by
// tests that want to exercise the auth check via httptest.
func (s *SSEServer) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.Handle("/mcp", s.streamableHandler())
	mux.Handle("/", s.sse)
	return s.requireAuth(mux)
}

func (s *SSEServer) streamableHandler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// mcp-go validates POST session IDs, but intentionally does not
		// validate GET IDs. Validate an explicitly supplied GET ID here so an
		// expired session cannot silently create a new notification session.
		if r.Method == http.MethodGet {
			if id := r.Header.Get("Mcp-Session-Id"); id != "" {
				if err := s.sessions.validateGET(id); err != nil {
					http.Error(w, "session not found", http.StatusNotFound)
					return
				}
			}
		}
		s.streamable.ServeHTTP(w, r)
	})
}

// Listen binds the configured address without serving yet, so a caller
// (e.g. internal/mcpsupervisor) can detect a bind failure (address already
// in use, permission denied, ...) synchronously before committing to a
// restart, instead of only finding out asynchronously inside Serve/Start's
// goroutine.
func (s *SSEServer) Listen() (net.Listener, error) {
	return net.Listen("tcp", s.addr)
}

// Serve blocks serving SSE and Streamable HTTP connections over an
// already-bound listener (see Listen) until Shutdown is called.
func (s *SSEServer) Serve(ln net.Listener) error {
	s.httpServer = &http.Server{Handler: s.Handler()}
	err := s.httpServer.Serve(ln)
	if err == http.ErrServerClosed {
		return nil
	}
	return err
}

// Start binds and blocks serving SSE and Streamable HTTP connections until
// Shutdown is called. Equivalent to Listen followed by Serve.
func (s *SSEServer) Start() error {
	ln, err := s.Listen()
	if err != nil {
		return err
	}
	return s.Serve(ln)
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
