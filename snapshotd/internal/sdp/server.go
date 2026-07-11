package sdp

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"log/slog"
	"net"
	"os"
	"strings"
	"sync"

	"github.com/google/uuid"

	"snapshotd/internal/sapproxy"
)

// Handler is implemented by the daemon core (internal/daemon.Daemon).
// Keeping this as a small interface (rather than importing the daemon
// package directly) is what keeps the SDP transport layer honestly
// protocol-only, per 06-daemon-mcp-proxy.md's adapter-boundary correction --
// this package doesn't know or care what's on the other side of Dispatch.
type Handler interface {
	// Dispatch handles the "daemon."-prefixed control-plane primitives
	// (06's primitives table): createProject/deleteProject/listProjects/
	// launch/list/health/close.
	Dispatch(ctx context.Context, method string, params json.RawMessage) (any, error)

	// ForwardSAP handles every method that is NOT "daemon."-prefixed:
	// project.select binds sessionID (via sink) to that project's pooled
	// SAP connection; every other method/params pair is forwarded
	// opaquely, per 06's generic-proxy requirement (internal/sapproxy).
	// sink receives that project's fanned-out notifications for as long as
	// sessionID stays bound.
	ForwardSAP(ctx context.Context, sessionID string, sink sapproxy.Sink, method string, params json.RawMessage) (json.RawMessage, error)

	// UnbindSession releases sessionID's SAP project binding/notification
	// sink, called once this connection closes.
	UnbindSession(sessionID string)
}

// Server is the SDP control-socket JSON-RPC 2.0 server: one Unix socket
// listener, newline-delimited JSON framing (see protocol.go's doc comment),
// one goroutine per connection, sequential request handling per connection
// (no pipelining assumed -- simple and sufficient for the CLI/MCP-adapter
// clients this serves).
type Server struct {
	SocketPath string
	Handler    Handler
	Log        *slog.Logger

	mu       sync.Mutex
	listener net.Listener
	wg       sync.WaitGroup
}

// ListenAndServe binds the control socket (removing any stale socket file
// first) and serves connections until Shutdown is called or an unrecoverable
// Accept error occurs.
func (s *Server) ListenAndServe() error {
	if s.Log == nil {
		s.Log = slog.Default()
	}
	// Remove a stale socket file from a previous, uncleanly-terminated run --
	// otherwise net.Listen("unix", ...) fails with "address already in use".
	_ = os.Remove(s.SocketPath)

	ln, err := net.Listen("unix", s.SocketPath)
	if err != nil {
		return err
	}
	s.mu.Lock()
	s.listener = ln
	s.mu.Unlock()

	for {
		conn, err := ln.Accept()
		if err != nil {
			if errors.Is(err, net.ErrClosed) {
				return nil
			}
			return err
		}
		s.wg.Add(1)
		go func() {
			defer s.wg.Done()
			s.serveConn(conn)
		}()
	}
}

// Shutdown stops accepting new connections and waits for in-flight
// connections to finish their current request.
func (s *Server) Shutdown() error {
	s.mu.Lock()
	ln := s.listener
	s.mu.Unlock()
	var err error
	if ln != nil {
		err = ln.Close()
	}
	s.wg.Wait()
	_ = os.Remove(s.SocketPath)
	return err
}

// connSink implements sapproxy.Sink for one raw SDP connection: notifications
// are written onto the same outbound channel as ordinary responses, so the
// writer goroutine below interleaves them on the wire as they arrive (per
// protocol.go's Notification type -- same newline-delimited JSON framing,
// just without an "id").
//
// close()/send() share one mutex specifically so a Notify racing a
// connection teardown can never panic on a send to a closed channel: whichever
// acquires the mutex first either enqueues the value or observes `closed`
// and drops it -- there is no window where a send can land after close().
type connSink struct {
	out chan any

	mu     sync.Mutex
	closed bool
}

func (s *connSink) Notify(method string, params json.RawMessage) {
	s.send(Notification{JSONRPC: "2.0", Method: method, Params: params})
}

func (s *connSink) sendResponse(r Response) {
	s.send(r)
}

func (s *connSink) send(v any) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return
	}
	select {
	case s.out <- v:
	default:
		// Outbound buffer full (a stuck/slow client) -- drop rather than
		// block this connection's request loop or another project's
		// notification fan-out indefinitely.
	}
}

func (s *connSink) closeSink() {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return
	}
	s.closed = true
	close(s.out)
}

func (s *Server) serveConn(conn net.Conn) {
	defer conn.Close()
	sessionID := uuid.NewString()

	sink := &connSink{out: make(chan any, 64)}
	writerDone := make(chan struct{})
	go func() {
		defer close(writerDone)
		enc := json.NewEncoder(conn)
		for v := range sink.out {
			if err := enc.Encode(v); err != nil {
				return
			}
		}
	}()

	scanner := bufio.NewScanner(conn)
	scanner.Buffer(make([]byte, 0, 64*1024), 8*1024*1024)

	for scanner.Scan() {
		line := scanner.Bytes()
		if len(line) == 0 {
			continue
		}
		var req Request
		if err := json.Unmarshal(line, &req); err != nil {
			sink.sendResponse(errorResponse(nil, CodeParseError, "parse error: "+err.Error()))
			continue
		}
		resp := s.handle(context.Background(), sessionID, sink, req)
		sink.sendResponse(resp)
	}

	// Unregister the session (and its sink) before closing the outbound
	// channel, so no in-flight Notify call from another goroutine can race
	// closeSink() -- see connSink's doc comment above.
	s.Handler.UnbindSession(sessionID)
	sink.closeSink()
	<-writerDone
}

func (s *Server) handle(ctx context.Context, sessionID string, sink sapproxy.Sink, req Request) Response {
	if req.Method == "" {
		return errorResponse(req.ID, CodeInvalidRequest, "missing method")
	}

	if strings.HasPrefix(req.Method, "daemon.") {
		result, err := s.Handler.Dispatch(ctx, req.Method, req.Params)
		if err != nil {
			return errorResponse(req.ID, CodeInternalError, err.Error())
		}
		return resultResponse(req.ID, result)
	}

	// Every other method is the generic, opaque SAP proxy path (06's
	// project.*/edit.*/playlist.*/... surface), forwarded verbatim.
	result, err := s.Handler.ForwardSAP(ctx, sessionID, sink, req.Method, req.Params)
	if err != nil {
		if rpcErr, ok := err.(*sapproxy.RPCError); ok {
			return Response{JSONRPC: "2.0", ID: req.ID, Error: &Error{Code: int(rpcErr.Code), Message: rpcErr.Message}}
		}
		return errorResponse(req.ID, CodeInternalError, err.Error())
	}
	return Response{JSONRPC: "2.0", ID: req.ID, Result: result}
}
