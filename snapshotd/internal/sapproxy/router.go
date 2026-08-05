package sapproxy

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"sync"
)

// Resolver locates the currently running sap-rust instance for a project:
// the Unix socket path to connect to and the per-launch token to present in
// sap.hello. Implemented by internal/daemon using the registry's most
// recent "ready" ProcessInstance row for the project (see
// daemon.resolveProjectInstance).
type Resolver func(projectID string) (socketPath, token string, err error)

// Router owns the daemon-wide pool of SAP connections (one per project,
// shared by every session bound to that project -- never one per session,
// per 06-daemon-mcp-proxy.md) and each session's current project binding.
// Safe for concurrent use.
type Router struct {
	resolve Resolver

	mu             sync.Mutex
	conns          map[string]*pooledConn // projectID -> connection
	sessionProject map[string]string      // sessionID -> bound projectID
}

// NewRouter constructs a Router. resolve is called (at most once per
// project, until that project's connection needs to be redialed) to find
// where to connect.
func NewRouter(resolve Resolver) *Router {
	return &Router{
		resolve:        resolve,
		conns:          make(map[string]*pooledConn),
		sessionProject: make(map[string]string),
	}
}

// pooledConn is one project's shared SAP connection plus the set of
// sessions currently bound to it (each with its own notification Sink).
type pooledConn struct {
	conn *Conn

	mu    sync.Mutex
	sinks map[string]Sink // sessionID -> sink
}

// notify fans one sap-rust notification out to every session currently
// bound to this project, per the doc 05/06/11 Phase B concurrency
// requirement. Called from Conn's single read-loop goroutine.
func (pc *pooledConn) notify(method string, params json.RawMessage) {
	pc.mu.Lock()
	sinks := make([]Sink, 0, len(pc.sinks))
	for _, s := range pc.sinks {
		sinks = append(sinks, s)
	}
	pc.mu.Unlock()
	for _, s := range sinks {
		s.Notify(method, params)
	}
}

// getOrDial returns the pooled connection for projectID, resolving,
// dialing, and performing the sap.hello handshake if none exists yet or the
// previous one has died (child crashed/restarted).
func (r *Router) getOrDial(ctx context.Context, projectID string) (*pooledConn, error) {
	r.mu.Lock()
	if pc, ok := r.conns[projectID]; ok && !pc.conn.IsClosed() {
		r.mu.Unlock()
		return pc, nil
	}
	rebind := false
	for _, boundProject := range r.sessionProject {
		if boundProject == projectID {
			rebind = true
			break
		}
	}
	r.mu.Unlock()

	socketPath, token, err := r.resolve(projectID)
	if err != nil {
		return nil, err
	}
	conn, err := Dial(ctx, socketPath, token)
	if err != nil {
		return nil, err
	}
	pc := &pooledConn{conn: conn, sinks: make(map[string]Sink)}
	conn.onNotification = pc.notify
	if rebind {
		params, _ := json.Marshal(map[string]string{"projectId": projectID})
		if _, err := conn.Call(ctx, "project.select", params); err != nil {
			_ = conn.Close()
			return nil, fmt.Errorf("sapproxy: rebind project %s after reconnect: %w", projectID, err)
		}
	}

	r.mu.Lock()
	defer r.mu.Unlock()
	if existing, ok := r.conns[projectID]; ok && !existing.conn.IsClosed() {
		// Lost a race with another goroutine dialing the same project
		// concurrently -- keep the one already published, discard ours.
		_ = conn.Close()
		return existing, nil
	}
	r.conns[projectID] = pc
	return pc, nil
}

// ErrAlreadyBound is (wrapped and) returned by Bind when sessionID is
// already bound to a different project than the one being requested,
// without an intervening Unbind (driven by the caller handling
// "project.exit" -- see internal/daemon.Daemon.ForwardSAP). Reselecting the
// SAME project a session is already bound to is always allowed and stays
// the pre-existing idempotent no-op success; only a genuinely different
// target project trips this guard.
var ErrAlreadyBound = errors.New("sapproxy: session already bound to a project")

// Bind implements the project.select side of the proxy: it (re)selects
// projectID on the shared pooled connection for that project, registers
// sink to receive that project's fanned-out notifications for sessionID
// (replacing any previous project binding that sessionID had), and returns
// sap-rust's real project.select result verbatim. The binding is committed
// only after project.select succeeds; a failed/expired select must not leave
// a session looking bound or retain its notification sink.
func (r *Router) Bind(ctx context.Context, sessionID, projectID string, sink Sink) (json.RawMessage, error) {
	r.mu.Lock()
	if prevProject, ok := r.sessionProject[sessionID]; ok && prevProject != projectID {
		r.mu.Unlock()
		return nil, fmt.Errorf("%w: bound to %q, requested %q; call project.exit first before selecting a different project", ErrAlreadyBound, prevProject, projectID)
	}
	r.mu.Unlock()

	pc, err := r.getOrDial(ctx, projectID)
	if err != nil {
		return nil, err
	}

	params, _ := json.Marshal(map[string]string{"projectId": projectID})
	result, err := pc.conn.Call(ctx, "project.select", params)
	if err != nil {
		return nil, err
	}

	// Publish the session binding only after the server has accepted the
	// selection. This ordering prevents a timeout from leaking state into the
	// router when the caller retries or closes the session.
	r.mu.Lock()
	r.sessionProject[sessionID] = projectID
	r.mu.Unlock()

	pc.mu.Lock()
	pc.sinks[sessionID] = sink
	pc.mu.Unlock()
	return result, nil
}

// Call forwards an opaque, already-bound method call to the SAP connection
// for whatever project sessionID is currently bound to. method/params are
// never inspected -- see the package doc comment.
func (r *Router) Call(ctx context.Context, sessionID, method string, params json.RawMessage) (json.RawMessage, error) {
	r.mu.Lock()
	projectID, ok := r.sessionProject[sessionID]
	r.mu.Unlock()
	if !ok {
		return nil, fmt.Errorf("sapproxy: session is not bound to a project; call project.select first")
	}
	pc, err := r.getOrDial(ctx, projectID)
	if err != nil {
		return nil, err
	}
	return pc.conn.Call(ctx, method, params)
}

// Unbind removes sessionID's project binding and notification sink, e.g. on
// client disconnect. Safe to call even if sessionID was never bound. Does
// not close the pooled connection itself -- other sessions may still be
// bound to the same project.
func (r *Router) Unbind(sessionID string) {
	r.mu.Lock()
	projectID, ok := r.sessionProject[sessionID]
	delete(r.sessionProject, sessionID)
	var pc *pooledConn
	if ok {
		pc = r.conns[projectID]
	}
	r.mu.Unlock()
	if pc != nil {
		pc.mu.Lock()
		delete(pc.sinks, sessionID)
		pc.mu.Unlock()
	}
}

// InvalidateProject drops the pooled SAP connection for projectID and closes
// it immediately. Registry lifecycle transitions are authoritative even when
// the child socket has not reported EOF yet; retaining the old connection
// would let an already-bound MCP session keep mutating a closed/in-memory
// project instance. Future calls will resolve the current instance and dial
// it afresh.
func (r *Router) InvalidateProject(projectID string) {
	r.mu.Lock()
	pc, ok := r.conns[projectID]
	if ok {
		delete(r.conns, projectID)
	}
	r.mu.Unlock()
	if ok {
		_ = pc.conn.Close()
	}
}

// BoundProject reports the current binding without exposing Router's mutable
// maps to callers. It is used by the daemon's context-aware MCP path to
// reconcile a per-agent target update before forwarding the next call.
func (r *Router) BoundProject(sessionID string) (string, bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	projectID, ok := r.sessionProject[sessionID]
	return projectID, ok
}
