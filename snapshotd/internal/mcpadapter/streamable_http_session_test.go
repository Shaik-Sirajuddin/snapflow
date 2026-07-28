package mcpadapter_test

import (
	"bytes"
	"net/http"
	"net/http/httptest"
	"testing"

	"snapshotd/internal/mcpadapter"
)

func TestStreamableHTTPServerSessionLifecycle(t *testing.T) {
	s := mcpadapter.NewSSEServer(noopHandler{}, "127.0.0.1:0")
	h := s.Handler()

	initialize := []byte(`{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}`)
	rec := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodPost, "/mcp", bytes.NewReader(initialize))
	req.Header.Set("Content-Type", "application/json")
	if got := req.Header.Get("Mcp-Session-Id"); got != "" {
		t.Fatalf("initialize must not carry a session ID, got %q", got)
	}
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("initialize status = %d, want 200: %s", rec.Code, rec.Body.String())
	}
	sessionID := rec.Header().Get("Mcp-Session-Id")
	if sessionID == "" {
		t.Fatal("initialize did not return Mcp-Session-Id")
	}

	listTools := []byte(`{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}`)
	rec = httptest.NewRecorder()
	req = httptest.NewRequest(http.MethodPost, "/mcp", bytes.NewReader(listTools))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Mcp-Session-Id", sessionID)
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("tools/list status = %d, want 200: %s", rec.Code, rec.Body.String())
	}

	rec = httptest.NewRecorder()
	req = httptest.NewRequest(http.MethodPost, "/mcp", bytes.NewReader(listTools))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Mcp-Session-Id", "mcp-session-does-not-exist")
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusNotFound {
		t.Fatalf("unknown session status = %d, want 404: %s", rec.Code, rec.Body.String())
	}

	rec = httptest.NewRecorder()
	req = httptest.NewRequest(http.MethodDelete, "/mcp", nil)
	req.Header.Set("Mcp-Session-Id", sessionID)
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("DELETE status = %d, want 200: %s", rec.Code, rec.Body.String())
	}

	rec = httptest.NewRecorder()
	req = httptest.NewRequest(http.MethodPost, "/mcp", bytes.NewReader(listTools))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Mcp-Session-Id", sessionID)
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusNotFound {
		t.Fatalf("terminated session status = %d, want 404: %s", rec.Code, rec.Body.String())
	}
}
