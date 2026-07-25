package mcpsupervisor

import (
	"context"
	"encoding/json"
	"net"
	"net/http"
	"strings"
	"testing"
	"time"

	"snapshotd/internal/mcpauth"
	"snapshotd/internal/sapproxy"
)

type noopHandler struct{}

func (noopHandler) Dispatch(ctx context.Context, method string, params json.RawMessage) (any, error) {
	return nil, nil
}
func (noopHandler) ForwardSAP(ctx context.Context, sessionID string, sink sapproxy.Sink, method string, params json.RawMessage) (json.RawMessage, error) {
	return nil, nil
}
func (noopHandler) UnbindSession(sessionID string) {}

func TestSupervisor_StartThenRestartRefusesNonLoopbackWithoutAuth(t *testing.T) {
	home := t.TempDir()
	s := New(noopHandler{}, home, "127.0.0.1:0", nil)
	ctx := context.Background()

	if err := s.Start(ctx); err != nil {
		t.Fatalf("start: %v", err)
	}
	t.Cleanup(func() { _ = s.Stop(context.Background()) })

	status := s.Status()
	if !status.Listening || status.AuthEnabled {
		t.Fatalf("unexpected initial status: %+v", status)
	}

	if err := s.Restart(ctx, "0.0.0.0:0"); err == nil {
		t.Fatalf("expected restart to a non-loopback addr without auth to be refused")
	} else if !strings.Contains(err.Error(), "auth") {
		t.Fatalf("expected refusal error to mention auth, got: %v", err)
	}

	// The refusal must not have torn down the previous listener.
	if status2 := s.Status(); !status2.Listening {
		t.Fatalf("expected listener to remain up after a refused restart, got: %+v", status2)
	}
}

func TestSupervisor_SetAuthThenRestartToNonLoopbackSucceeds(t *testing.T) {
	home := t.TempDir()
	s := New(noopHandler{}, home, "127.0.0.1:0", nil)
	ctx := context.Background()

	if err := s.Start(ctx); err != nil {
		t.Fatalf("start: %v", err)
	}
	t.Cleanup(func() { _ = s.Stop(context.Background()) })

	if err := s.SetAuth(ctx, "alice", "s3cret"); err != nil {
		t.Fatalf("set auth: %v", err)
	}

	if err := s.Restart(ctx, "0.0.0.0:0"); err != nil {
		t.Fatalf("expected restart to succeed once auth is enabled: %v", err)
	}

	status := s.Status()
	if !status.Listening || !status.AuthEnabled || status.AuthUser != "alice" {
		t.Fatalf("unexpected status after restart: %+v", status)
	}

	persisted, err := mcpauth.Load(home)
	if err != nil {
		t.Fatalf("load persisted config: %v", err)
	}
	if !persisted.AuthEnabled || persisted.AuthUser != "alice" || persisted.AuthPassword != "s3cret" {
		t.Fatalf("expected auth persisted, got %+v", persisted)
	}
	if persisted.BindAddr == "" {
		t.Fatalf("expected bind addr persisted")
	}
}

// TestSupervisor_RestartToUnbindableAddrLeavesOldListenerRunning guards
// against a bad --bind value (typo, port already taken, permission denied)
// taking down a perfectly good existing listener: Restart must bind the
// new address successfully before it retires the old one.
func TestSupervisor_RestartToUnbindableAddrLeavesOldListenerRunning(t *testing.T) {
	home := t.TempDir()
	s := New(noopHandler{}, home, "127.0.0.1:0", nil)
	ctx := context.Background()

	if err := s.Start(ctx); err != nil {
		t.Fatalf("start: %v", err)
	}
	t.Cleanup(func() { _ = s.Stop(context.Background()) })
	if err := s.SetAuth(ctx, "alice", "s3cret"); err != nil {
		t.Fatalf("set auth: %v", err)
	}
	before := s.Status()

	// Occupy a port so restarting onto it fails to bind.
	occupied, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("occupy a port: %v", err)
	}
	defer occupied.Close()
	occupiedAddr := occupied.Addr().String()

	if err := s.Restart(ctx, occupiedAddr); err == nil {
		t.Fatalf("expected restart onto an already-bound port to fail")
	}

	after := s.Status()
	if !after.Listening {
		t.Fatalf("expected original listener to remain up after a failed restart, got: %+v", after)
	}
	if after.Addr != before.Addr {
		t.Fatalf("expected original address to be unchanged after a failed restart: before=%q after=%q", before.Addr, after.Addr)
	}

	// And the original listener genuinely still serves requests.
	client := &http.Client{Timeout: 2 * time.Second}
	req, _ := http.NewRequest(http.MethodGet, "http://"+after.Addr+"/mcp", nil)
	req.SetBasicAuth("alice", "s3cret")
	resp, err := client.Do(req)
	if err != nil {
		t.Fatalf("request to original listener after failed restart: %v", err)
	}
	resp.Body.Close()
	if resp.StatusCode == http.StatusUnauthorized {
		t.Fatalf("expected original listener to still work with its own credentials, got 401")
	}
}

func TestSupervisor_LiveAuthEnforcedOverRealHTTP(t *testing.T) {
	home := t.TempDir()
	s := New(noopHandler{}, home, "127.0.0.1:0", nil)
	ctx := context.Background()

	if err := s.Start(ctx); err != nil {
		t.Fatalf("start: %v", err)
	}
	t.Cleanup(func() { _ = s.Stop(context.Background()) })

	if err := s.SetAuth(ctx, "alice", "s3cret"); err != nil {
		t.Fatalf("set auth: %v", err)
	}

	addr := s.Status().Addr
	client := &http.Client{Timeout: 2 * time.Second}

	resp, err := client.Get("http://" + addr + "/mcp")
	if err != nil {
		t.Fatalf("unauthenticated request: %v", err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("expected 401 without credentials, got %d", resp.StatusCode)
	}

	req, _ := http.NewRequest(http.MethodGet, "http://"+addr+"/mcp", nil)
	req.SetBasicAuth("alice", "s3cret")
	resp, err = client.Do(req)
	if err != nil {
		t.Fatalf("authenticated request: %v", err)
	}
	resp.Body.Close()
	if resp.StatusCode == http.StatusUnauthorized {
		t.Fatalf("expected auth to pass with correct credentials, got 401")
	}
}
