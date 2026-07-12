package sapproxy

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"net"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

// fakeSapServer is a minimal, in-process stand-in for sap-rust's own
// server.rs: Content-Length framing, a sap.hello token gate, project.select
// binding, one mutating method ("edit.addTrack") that fans a notification
// out to every connection currently selected onto the same project, and one
// read method ("edit.listTracks"). It exists so this package's Conn/Router
// logic can be tested without depending on the real (independently
// developed) sap-rust binary -- see internal/daemon and internal/mcpadapter
// for tests against the real binary when it's built.
type fakeSapServer struct {
	token string

	mu       sync.Mutex
	tracks   map[string][]string      // projectID -> track kinds
	watchers map[string][]chan []byte // projectID -> connections' outbound queues
}

func newFakeSapServer(token string) *fakeSapServer {
	return &fakeSapServer{
		token:    token,
		tracks:   make(map[string][]string),
		watchers: make(map[string][]chan []byte),
	}
}

func (s *fakeSapServer) serve(t *testing.T, socketPath string) {
	t.Helper()
	ln, err := net.Listen("unix", socketPath)
	if err != nil {
		t.Fatalf("fakeSapServer: listen: %v", err)
	}
	t.Cleanup(func() { _ = ln.Close() })
	go func() {
		for {
			conn, err := ln.Accept()
			if err != nil {
				return
			}
			go s.handleConn(conn)
		}
	}()
}

func (s *fakeSapServer) handleConn(nc net.Conn) {
	defer nc.Close()
	r := bufio.NewReader(nc)
	var writeMu sync.Mutex
	write := func(v any) {
		body, _ := json.Marshal(v)
		writeMu.Lock()
		_ = writeFramed(nc, body)
		writeMu.Unlock()
	}

	authenticated := false
	var boundProject string
	var myCh chan []byte

	for {
		raw, err := readFramed(r)
		if err != nil {
			return
		}
		var req struct {
			ID     json.RawMessage `json:"id"`
			Method string          `json:"method"`
			Params json.RawMessage `json:"params"`
		}
		if err := json.Unmarshal(raw, &req); err != nil {
			continue
		}

		respond := func(result any, errMsg string) {
			if errMsg != "" {
				write(map[string]any{"jsonrpc": "2.0", "id": json.RawMessage(req.ID), "error": map[string]any{"code": -32000, "message": errMsg}})
				return
			}
			write(map[string]any{"jsonrpc": "2.0", "id": json.RawMessage(req.ID), "result": result})
		}

		switch req.Method {
		case "sap.hello":
			var p struct {
				Token string `json:"token"`
			}
			_ = json.Unmarshal(req.Params, &p)
			if p.Token != s.token {
				respond(nil, "bad token")
				continue
			}
			authenticated = true
			respond(map[string]any{"ok": true}, "")
		case "project.select":
			if !authenticated {
				respond(nil, "unauthenticated")
				continue
			}
			var p struct {
				ProjectID string `json:"projectId"`
			}
			_ = json.Unmarshal(req.Params, &p)
			boundProject = p.ProjectID
			myCh = make(chan []byte, 16)
			s.mu.Lock()
			s.watchers[boundProject] = append(s.watchers[boundProject], myCh)
			s.mu.Unlock()
			go func(ch chan []byte) {
				for body := range ch {
					writeMu.Lock()
					_ = writeFramed(nc, body)
					writeMu.Unlock()
				}
			}(myCh)
			respond(map[string]any{"projectId": boundProject, "dirty": false}, "")
		case "edit.addTrack":
			if boundProject == "" {
				respond(nil, "no project bound")
				continue
			}
			var p struct {
				Kind string `json:"kind"`
			}
			_ = json.Unmarshal(req.Params, &p)
			s.mu.Lock()
			s.tracks[boundProject] = append(s.tracks[boundProject], p.Kind)
			idx := len(s.tracks[boundProject]) - 1
			watchers := append([]chan []byte(nil), s.watchers[boundProject]...)
			s.mu.Unlock()
			respond(map[string]any{"index": idx, "kind": p.Kind}, "")
			notif, _ := json.Marshal(map[string]any{"jsonrpc": "2.0", "method": "edit.changed", "params": map[string]any{"reason": "addTrack"}})
			for _, w := range watchers {
				select {
				case w <- notif:
				default:
				}
			}
		case "edit.listTracks":
			if boundProject == "" {
				respond(nil, "no project bound")
				continue
			}
			s.mu.Lock()
			tracks := append([]string(nil), s.tracks[boundProject]...)
			s.mu.Unlock()
			respond(tracks, "")
		default:
			respond(nil, "method not found: "+req.Method)
		}
	}
}

type recordingSink struct {
	mu     sync.Mutex
	events []string
}

func (r *recordingSink) Notify(method string, params json.RawMessage) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.events = append(r.events, method)
}

func (r *recordingSink) count() int {
	r.mu.Lock()
	defer r.mu.Unlock()
	return len(r.events)
}

func TestRouter_BindAndCall_ForwardsOpaquely(t *testing.T) {
	sock := filepath.Join(t.TempDir(), "fake.sock")
	srv := newFakeSapServer("tok-123")
	srv.serve(t, sock)

	resolved := 0
	router := NewRouter(func(projectID string) (string, string, error) {
		resolved++
		return sock, "tok-123", nil
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	sinkA := &recordingSink{}
	selectResult, err := router.Bind(ctx, "session-a", "proj-1", sinkA)
	if err != nil {
		t.Fatalf("bind: %v", err)
	}
	var sel map[string]any
	if err := json.Unmarshal(selectResult, &sel); err != nil {
		t.Fatalf("unmarshal select result: %v", err)
	}
	if sel["projectId"] != "proj-1" {
		t.Fatalf("unexpected select result: %+v", sel)
	}

	// A second session bound to the same project must reuse the pooled
	// connection (Resolver called once per project's live connection, not
	// once per session).
	sinkB := &recordingSink{}
	if _, err := router.Bind(ctx, "session-b", "proj-1", sinkB); err != nil {
		t.Fatalf("bind session-b: %v", err)
	}
	if resolved != 1 {
		t.Fatalf("expected resolver called once for one shared pooled connection, got %d calls", resolved)
	}

	// A generic, opaque forwarded call.
	addResult, err := router.Call(ctx, "session-a", "edit.addTrack", mustJSON(t, map[string]any{"kind": "video"}))
	if err != nil {
		t.Fatalf("call edit.addTrack: %v", err)
	}
	var track map[string]any
	if err := json.Unmarshal(addResult, &track); err != nil {
		t.Fatalf("unmarshal track: %v", err)
	}
	if track["kind"] != "video" {
		t.Fatalf("unexpected track result: %+v", track)
	}

	listResult, err := router.Call(ctx, "session-b", "edit.listTracks", nil)
	if err != nil {
		t.Fatalf("call edit.listTracks: %v", err)
	}
	var tracks []string
	if err := json.Unmarshal(listResult, &tracks); err != nil {
		t.Fatalf("unmarshal tracks: %v", err)
	}
	if len(tracks) != 1 || tracks[0] != "video" {
		t.Fatalf("expected real mutated state visible to a second session, got %+v", tracks)
	}

	// Both sessions, bound to the same project, must see the fanned-out
	// notification -- even session-b, which didn't make the mutating call.
	deadline := time.Now().Add(2 * time.Second)
	for (sinkA.count() == 0 || sinkB.count() == 0) && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if sinkA.count() == 0 || sinkB.count() == 0 {
		t.Fatalf("expected both sessions to receive the fanned-out notification, got sinkA=%d sinkB=%d", sinkA.count(), sinkB.count())
	}

	// Call without ever binding a session should fail cleanly.
	if _, err := router.Call(ctx, "session-unbound", "edit.listTracks", nil); err == nil {
		t.Fatalf("expected error calling before project.select")
	}

	// Unbind removes the session's sink; subsequent notifications must not
	// reach it (best-effort assertion: count stays put after unbind + a
	// further mutation).
	router.Unbind("session-b")
	before := sinkB.count()
	if _, err := router.Call(ctx, "session-a", "edit.addTrack", mustJSON(t, map[string]any{"kind": "audio"})); err != nil {
		t.Fatalf("call edit.addTrack #2: %v", err)
	}
	time.Sleep(100 * time.Millisecond)
	if sinkB.count() != before {
		t.Fatalf("expected unbound session to stop receiving notifications, got %d -> %d", before, sinkB.count())
	}
}

func TestRouter_BadToken_ReturnsError(t *testing.T) {
	sock := filepath.Join(t.TempDir(), "fake.sock")
	srv := newFakeSapServer("expected-token")
	srv.serve(t, sock)

	router := NewRouter(func(projectID string) (string, string, error) {
		return sock, "wrong-token", nil
	})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	if _, err := router.Bind(ctx, "s1", "proj-1", &recordingSink{}); err == nil {
		t.Fatalf("expected sap.hello failure with a bad token")
	}
}

func TestRouter_Bind_RejectsSwitchingProjectWithoutUnbind(t *testing.T) {
	sock := filepath.Join(t.TempDir(), "fake.sock")
	srv := newFakeSapServer("tok-123")
	srv.serve(t, sock)

	router := NewRouter(func(projectID string) (string, string, error) {
		return sock, "tok-123", nil
	})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	if _, err := router.Bind(ctx, "session-a", "proj-1", &recordingSink{}); err != nil {
		t.Fatalf("first bind: %v", err)
	}

	// Reselecting the SAME project must stay an idempotent no-op success.
	if _, err := router.Bind(ctx, "session-a", "proj-1", &recordingSink{}); err != nil {
		t.Fatalf("reselecting the same project must succeed: %v", err)
	}

	// Switching to a different project without an intervening Unbind must
	// be rejected -- this is the Go-layer half of the harness guard
	// (internal/daemon.Daemon.ForwardSAP handles "project.exit" by calling
	// Router.Unbind before a later Bind, which is exercised below).
	if _, err := router.Bind(ctx, "session-a", "proj-2", &recordingSink{}); !errors.Is(err, ErrAlreadyBound) {
		t.Fatalf("expected ErrAlreadyBound switching projects without unbind, got %v", err)
	}

	// The rejected attempt must not have disturbed the existing binding --
	// session-a should still be able to call against proj-1.
	if _, err := router.Call(ctx, "session-a", "edit.listTracks", nil); err != nil {
		t.Fatalf("session-a should still be bound to proj-1 after the rejected switch: %v", err)
	}

	// After Unbind (what ForwardSAP's "project.exit" handling does), a
	// switch to a different project must succeed.
	router.Unbind("session-a")
	if _, err := router.Bind(ctx, "session-a", "proj-2", &recordingSink{}); err != nil {
		t.Fatalf("bind to a different project after unbind should succeed: %v", err)
	}
	if _, err := router.Call(ctx, "session-a", "edit.listTracks", nil); err != nil {
		t.Fatalf("session-a should now be bound to proj-2: %v", err)
	}
}

func mustJSON(t *testing.T, v any) json.RawMessage {
	t.Helper()
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	return b
}
