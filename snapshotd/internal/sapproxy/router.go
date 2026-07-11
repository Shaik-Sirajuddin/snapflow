package sapproxy

import (
	"context"
	"encoding/json"
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

// Bind implements the project.select side of the proxy: it (re)selects
// projectID on the shared pooled connection for that project, registers
// sink to receive that project's fanned-out notifications for sessionID
// (replacing any previous project binding that sessionID had), and returns
// sap-rust's real project.select result verbatim.
func (r *Router) Bind(ctx context.Context, sessionID, projectID string, sink Sink) (json.RawMessage, error) {
	pc, err := r.getOrDial(ctx, projectID)
	if err != nil {
		return nil, err
	}

	r.mu.Lock()
	if prevProject, ok := r.sessionProject[sessionID]; ok && prevProject != projectID {
		if prevConn, ok := r.conns[prevProject]; ok {
			prevConn.mu.Lock()
			delete(prevConn.sinks, sessionID)
			prevConn.mu.Unlock()
		}
	}
	r.sessionProject[sessionID] = projectID
	r.mu.Unlock()

	pc.mu.Lock()
	pc.sinks[sessionID] = sink
	pc.mu.Unlock()

	params, _ := json.Marshal(map[string]string{"projectId": projectID})
	return pc.conn.Call(ctx, "project.select", params)
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
