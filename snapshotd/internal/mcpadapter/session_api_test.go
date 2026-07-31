package mcpadapter

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	"snapshotd/internal/sapproxy"
	"snapshotd/internal/session"
)

type sessionAPIHandler struct {
	store    *session.Memory
	mu       sync.Mutex
	watchers map[string][]chan session.Session
}

func (h *sessionAPIHandler) Dispatch(context.Context, string, json.RawMessage) (any, error) {
	return nil, nil
}
func (h *sessionAPIHandler) ForwardSAP(context.Context, string, sapproxy.Sink, string, json.RawMessage) (json.RawMessage, error) {
	return nil, nil
}
func (h *sessionAPIHandler) UnbindSession(string) {}
func (h *sessionAPIHandler) SessionDetails(id, acp string) (session.Session, error) {
	if acp != "" {
		return h.store.Update(id, func(s *session.Session) { s.ACPSessionID = acp })
	}
	return h.store.Lookup(id)
}
func (h *sessionAPIHandler) RegisterSessionContext(token, acp string) error {
	if token == "" || acp == "" {
		return fmt.Errorf("missing context registration fields")
	}
	_, err := h.store.Update("snap-1", func(s *session.Session) { s.ACPSessionID = acp })
	return err
}
func (h *sessionAPIHandler) SubscribeSession(id string) (session.Session, <-chan session.Session, func(), error) {
	current, err := h.store.Lookup(id)
	if err != nil {
		return session.Session{}, nil, func() {}, err
	}
	updates := make(chan session.Session, 1)
	h.mu.Lock()
	if h.watchers == nil {
		h.watchers = make(map[string][]chan session.Session)
	}
	h.watchers[id] = append(h.watchers[id], updates)
	h.mu.Unlock()
	var once sync.Once
	return current, updates, func() { once.Do(func() { close(updates) }) }, nil
}

func TestSessionAPIUsesBearerAuthAndMultiplexesSubscriptions(t *testing.T) {
	store := session.NewMemory(time.Hour)
	defer store.Close()
	if _, err := store.Create("snap-1", "mcp", time.Minute); err != nil {
		t.Fatal(err)
	}
	h := &sessionAPIHandler{store: store, watchers: make(map[string][]chan session.Session)}
	srv := NewSSEServer(h, "127.0.0.1:0")
	srv.SetSessionServiceToken("secret")
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	resp, err := ts.Client().Get(ts.URL + "/session/details?sessionId=snap-1")
	if err != nil {
		t.Fatal(err)
	}
	if resp.StatusCode != 401 {
		t.Fatalf("unauthorized status = %d", resp.StatusCode)
	}
	req, err := http.NewRequest(http.MethodGet, ts.URL+"/session/details?sessionId=snap-1", nil)
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer secret")
	resp, err = ts.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("authorized details status = %d", resp.StatusCode)
	}
	var details sessionSnapshot
	if err := json.NewDecoder(resp.Body).Decode(&details); err != nil {
		t.Fatal(err)
	}
	if details.SessionID != "snap-1" {
		t.Fatalf("details session id = %q", details.SessionID)
	}

	wsURL := "ws" + strings.TrimPrefix(ts.URL, "http") + "/session/ws"
	header := make(map[string][]string)
	header["Authorization"] = []string{"Bearer secret"}
	conn, _, err := websocket.DefaultDialer.Dial(wsURL, header)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	if err := conn.WriteJSON(map[string]any{
		"type": "session.context.register", "requestId": "r0",
		"contextToken": "ctx-token", "acpSessionId": "acp-1",
	}); err != nil {
		t.Fatal(err)
	}
	var registered sessionWSMessage
	if err := conn.ReadJSON(&registered); err != nil {
		t.Fatal(err)
	}
	if registered.Type != "session.context.registered" || registered.ClientInstanceID == "" {
		t.Fatalf("unexpected context registration response: %+v", registered)
	}
	if err := conn.WriteJSON(map[string]any{"type": "session.subscribe", "requestId": "r1", "sessionIds": []string{"snap-1"}}); err != nil {
		t.Fatal(err)
	}
	var subscribed sessionWSMessage
	if err := conn.ReadJSON(&subscribed); err != nil {
		t.Fatal(err)
	}
	if subscribed.Type != "session.subscribed" || len(subscribed.Snapshots) != 1 {
		t.Fatalf("unexpected subscribe response: %+v", subscribed)
	}
	if subscribed.ClientInstanceID == "" {
		t.Fatal("expected per-connection client instance id")
	}
	if err := conn.WriteJSON(map[string]any{
		"type":      "session.resync",
		"requestId": "r2",
		"sessionId": "snap-1",
	}); err != nil {
		t.Fatal(err)
	}
	var resynced sessionWSMessage
	if err := conn.ReadJSON(&resynced); err != nil {
		t.Fatal(err)
	}
	if resynced.Type != "session.snapshot" || resynced.Snapshot == nil || resynced.Snapshot.SessionID != "snap-1" {
		t.Fatalf("unexpected resync response: %+v", resynced)
	}
	second, _, err := websocket.DefaultDialer.Dial(wsURL, header)
	if err != nil {
		t.Fatal(err)
	}
	defer second.Close()
	if err := second.WriteJSON(map[string]any{"type": "session.subscribe", "requestId": "r3", "sessionIds": []string{"snap-1"}}); err != nil {
		t.Fatal(err)
	}
	var secondSubscribed sessionWSMessage
	if err := second.ReadJSON(&secondSubscribed); err != nil {
		t.Fatal(err)
	}
	if secondSubscribed.Type != "session.subscribed" || secondSubscribed.ClientInstanceID == subscribed.ClientInstanceID {
		t.Fatalf("second client was not independently subscribed: %+v", secondSubscribed)
	}
}
