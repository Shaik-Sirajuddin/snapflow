package mcpadapter_test

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"

	"snapshotd/internal/mcpadapter"
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

func TestSSEServer_BasicAuth(t *testing.T) {
	s := mcpadapter.NewSSEServer(noopHandler{}, "127.0.0.1:0")

	// Auth disabled by default: any request reaches the underlying mux
	// (which will 404 for a bare GET /nonexistent, not 401).
	rec := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodGet, "/does-not-exist", nil)
	s.Handler().ServeHTTP(rec, req)
	if rec.Code == http.StatusUnauthorized {
		t.Fatalf("expected no auth challenge while disabled, got 401")
	}

	s.SetCredentials(mcpadapter.Credentials{Enabled: true, User: "alice", Password: "s3cret"})

	rec = httptest.NewRecorder()
	req = httptest.NewRequest(http.MethodGet, "/does-not-exist", nil)
	s.Handler().ServeHTTP(rec, req)
	if rec.Code != http.StatusUnauthorized {
		t.Fatalf("expected 401 with no credentials, got %d", rec.Code)
	}

	rec = httptest.NewRecorder()
	req = httptest.NewRequest(http.MethodGet, "/does-not-exist", nil)
	req.SetBasicAuth("alice", "wrong")
	s.Handler().ServeHTTP(rec, req)
	if rec.Code != http.StatusUnauthorized {
		t.Fatalf("expected 401 with wrong password, got %d", rec.Code)
	}

	rec = httptest.NewRecorder()
	req = httptest.NewRequest(http.MethodGet, "/does-not-exist", nil)
	req.SetBasicAuth("alice", "s3cret")
	s.Handler().ServeHTTP(rec, req)
	if rec.Code == http.StatusUnauthorized {
		t.Fatalf("expected auth to pass with correct credentials, got 401")
	}
}

// TestSSEServer_BasicAuth_FailsClosedOnEmptyCredentials guards against a
// corrupted/hand-edited persisted config (Enabled=true but an empty user or
// password) turning into an open door: without this, a request with blank
// Basic Auth credentials (user="", pass="") would match empty stored
// values via ConstantTimeCompare and be let through.
func TestSSEServer_BasicAuth_FailsClosedOnEmptyCredentials(t *testing.T) {
	s := mcpadapter.NewSSEServer(noopHandler{}, "127.0.0.1:0")
	s.SetCredentials(mcpadapter.Credentials{Enabled: true, User: "", Password: ""})

	rec := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodGet, "/does-not-exist", nil)
	req.SetBasicAuth("", "")
	s.Handler().ServeHTTP(rec, req)
	if rec.Code != http.StatusUnauthorized {
		t.Fatalf("expected 401 for blank credentials against an empty-credential Enabled config, got %d", rec.Code)
	}
}
