package mcpadapter_test

import (
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	"snapshotd/internal/config"
	"snapshotd/internal/daemon"
	"snapshotd/internal/mcpadapter"
)

func TestSessionAPIRoutesToLiveDaemonAndSupportsIndependentClients(t *testing.T) {
	home := t.TempDir()
	cfg := config.Default()
	cfg.HomeDir = home
	cfg.DBPath = filepath.Join(home, "registry.db")
	cfg.ControlSocketPath = filepath.Join(home, "control.sock")
	cfg.RunDir = filepath.Join(home, "run")
	cfg.LogDir = filepath.Join(home, "logs")
	cfg.ProjectsRoot = filepath.Join(home, "projects")
	d, err := daemon.New(cfg, slog.New(slog.NewTextHandler(io.Discard, &slog.HandlerOptions{Level: slog.LevelError})))
	if err != nil {
		t.Fatal(err)
	}
	defer d.Close()
	defer d.Sessions.Close()

	if _, err := d.Sessions.Create("snap-1", "mcp", time.Minute); err != nil {
		t.Fatal(err)
	}
	if _, err := d.SetSessionCorrelation("snap-1", "acp-1"); err != nil {
		t.Fatal(err)
	}

	srv := mcpadapter.NewSSEServer(d, "127.0.0.1:0")
	srv.SetSessionServiceToken("secret")
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	req, err := http.NewRequest(http.MethodGet, ts.URL+"/session/details?sessionId=acp-1", nil)
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer secret")
	resp, err := ts.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("details status = %d", resp.StatusCode)
	}
	var details map[string]any
	if err := json.NewDecoder(resp.Body).Decode(&details); err != nil {
		t.Fatal(err)
	}
	if details["sessionId"] != "snap-1" || details["acpSessionId"] != "acp-1" {
		t.Fatalf("unexpected live daemon details: %+v", details)
	}

	wsURL := "ws" + strings.TrimPrefix(ts.URL, "http") + "/session/ws"
	header := http.Header{"Authorization": []string{"Bearer secret"}}
	first, _, err := websocket.DefaultDialer.Dial(wsURL, header)
	if err != nil {
		t.Fatal(err)
	}
	defer first.Close()
	if err := first.WriteJSON(map[string]any{"type": "session.subscribe", "requestId": "r1", "sessionIds": []string{"acp-1"}}); err != nil {
		t.Fatal(err)
	}
	var subscribed map[string]any
	if err := first.ReadJSON(&subscribed); err != nil {
		t.Fatal(err)
	}
	if subscribed["type"] != "session.subscribed" || subscribed["clientInstanceId"] == "" {
		t.Fatalf("unexpected first subscription: %+v", subscribed)
	}
	snapshots, ok := subscribed["snapshots"].([]any)
	if !ok || len(snapshots) != 1 {
		t.Fatalf("expected one ACPX-resolved snapshot: %+v", subscribed)
	}

	second, _, err := websocket.DefaultDialer.Dial(wsURL, header)
	if err != nil {
		t.Fatal(err)
	}
	defer second.Close()
	if err := second.WriteJSON(map[string]any{"type": "session.subscribe", "requestId": "r2", "sessionIds": []string{"acp-1"}}); err != nil {
		t.Fatal(err)
	}
	var secondSubscribed map[string]any
	if err := second.ReadJSON(&secondSubscribed); err != nil {
		t.Fatal(err)
	}
	if secondSubscribed["type"] != "session.subscribed" || secondSubscribed["clientInstanceId"] == subscribed["clientInstanceId"] {
		t.Fatalf("same-token clients were not independent: first=%+v second=%+v", subscribed, secondSubscribed)
	}
}
