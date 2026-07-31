package mcpadapter

import (
	"encoding/json"
	"net/http"
	"strings"
	"sync"

	"github.com/google/uuid"
	"github.com/gorilla/websocket"
	"snapshotd/internal/session"
)

type sessionDetailsRequest struct {
	SessionID    string `json:"sessionId"`
	ACPSessionID string `json:"acpSessionId,omitempty"`
}

type sessionSnapshot struct {
	SessionID        string `json:"sessionId"`
	ACPSessionID     string `json:"acpSessionId,omitempty"`
	ProjectID        string `json:"projectId,omitempty"`
	ProjectPath      string `json:"projectPath,omitempty"`
	ConnectionStatus string `json:"connectionStatus"`
	Revision         uint64 `json:"revision"`
	CreatedAt        string `json:"createdAt"`
	ExpiresAt        string `json:"expiresAt"`
}

func makeSessionSnapshot(s session.Session) sessionSnapshot {
	return sessionSnapshot{
		SessionID: s.ID, ACPSessionID: s.ACPSessionID, ProjectID: s.ProjectID,
		ProjectPath: s.ProjectPath, ConnectionStatus: s.ConnectionStatus,
		Revision: s.Revision, CreatedAt: s.CreatedAt.UTC().Format("2006-01-02T15:04:05.999999999Z07:00"),
		ExpiresAt: s.ExpiresAt.UTC().Format("2006-01-02T15:04:05.999999999Z07:00"),
	}
}

func (s *SSEServer) handleSessionDetails(w http.ResponseWriter, r *http.Request, h SessionStatusHandler) {
	if r.Method != http.MethodPost && r.Method != http.MethodGet {
		w.WriteHeader(http.StatusMethodNotAllowed)
		return
	}
	var req sessionDetailsRequest
	if r.Method == http.MethodPost {
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			http.Error(w, "invalid JSON", http.StatusBadRequest)
			return
		}
	} else {
		req.SessionID = r.URL.Query().Get("sessionId")
		req.ACPSessionID = r.URL.Query().Get("acpSessionId")
	}
	if strings.TrimSpace(req.SessionID) == "" {
		http.Error(w, "sessionId is required", http.StatusBadRequest)
		return
	}
	snapshot, err := h.SessionDetails(req.SessionID, req.ACPSessionID)
	if err != nil {
		http.Error(w, err.Error(), http.StatusNotFound)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(makeSessionSnapshot(snapshot))
}

type sessionWSMessage struct {
	Type             string            `json:"type"`
	RequestID        string            `json:"requestId,omitempty"`
	SessionIDs       []string          `json:"sessionIds,omitempty"`
	ACPSessionID     string            `json:"acpSessionId,omitempty"`
	ContextToken     string            `json:"contextToken,omitempty"`
	SessionID        string            `json:"sessionId,omitempty"`
	Error            string            `json:"error,omitempty"`
	Snapshot         *sessionSnapshot  `json:"snapshot,omitempty"`
	Snapshots        []sessionSnapshot `json:"snapshots,omitempty"`
	ClientInstanceID string            `json:"clientInstanceId,omitempty"`
}

var sessionWSUpgrader = websocket.Upgrader{CheckOrigin: func(*http.Request) bool { return true }}

func (s *SSEServer) handleSessionWebSocket(w http.ResponseWriter, r *http.Request, h SessionStatusHandler) {
	if r.Method != http.MethodGet {
		w.WriteHeader(http.StatusMethodNotAllowed)
		return
	}
	conn, err := sessionWSUpgrader.Upgrade(w, r, nil)
	if err != nil {
		return
	}
	defer conn.Close()
	clientInstanceID := uuid.NewString()
	var writeMu sync.Mutex

	type subscription struct{ cancel func() }
	subs := map[string]subscription{}
	defer func() {
		for _, sub := range subs {
			sub.cancel()
		}
	}()

	write := func(msg sessionWSMessage) bool {
		writeMu.Lock()
		defer writeMu.Unlock()
		return conn.WriteJSON(msg) == nil
	}
	for {
		var msg sessionWSMessage
		if err := conn.ReadJSON(&msg); err != nil {
			return
		}
		switch msg.Type {
		case "session.context.register":
			contextHandler, ok := h.(SessionContextHandler)
			if !ok {
				write(sessionWSMessage{Type: "error", RequestID: msg.RequestID, Error: "session context registration is unavailable"})
				continue
			}
			if err := contextHandler.RegisterSessionContext(msg.ContextToken, msg.ACPSessionID); err != nil {
				write(sessionWSMessage{Type: "error", RequestID: msg.RequestID, Error: err.Error()})
				continue
			}
			write(sessionWSMessage{Type: "session.context.registered", RequestID: msg.RequestID, ClientInstanceID: clientInstanceID})
		case "session.subscribe":
			if len(msg.SessionIDs) == 0 {
				write(sessionWSMessage{Type: "error", RequestID: msg.RequestID, Error: "sessionIds is required"})
				continue
			}
			var snapshots []sessionSnapshot
			for _, id := range msg.SessionIDs {
				if strings.TrimSpace(id) == "" {
					continue
				}
				current, updates, cancel, err := h.SubscribeSession(id)
				if err != nil {
					continue
				}
				canonicalID := current.ID
				if old, ok := subs[canonicalID]; ok {
					old.cancel()
				}
				subs[canonicalID] = subscription{cancel: cancel}
				snapshots = append(snapshots, makeSessionSnapshot(current))
				go func(id string, updates <-chan session.Session, cancel func()) {
					for update := range updates {
						if !write(sessionWSMessage{Type: "session.update", SessionID: id, Snapshot: ptr(makeSessionSnapshot(update))}) {
							// Remove the watcher immediately on a broken client
							// connection; otherwise it remains registered until TTL
							// expiry or the daemon reaper runs.
							cancel()
							return
						}
					}
				}(canonicalID, updates, cancel)
			}
			if len(snapshots) == 0 {
				write(sessionWSMessage{Type: "error", RequestID: msg.RequestID, Error: "no requested sessions are available"})
				continue
			}
			write(sessionWSMessage{Type: "session.subscribed", RequestID: msg.RequestID, Snapshots: snapshots, ClientInstanceID: clientInstanceID})
		case "session.resync":
			if msg.SessionID == "" {
				write(sessionWSMessage{Type: "error", RequestID: msg.RequestID, Error: "sessionId is required"})
				continue
			}
			current, err := h.SessionDetails(msg.SessionID, msg.ACPSessionID)
			if err != nil {
				write(sessionWSMessage{Type: "error", RequestID: msg.RequestID, Error: err.Error()})
				continue
			}
			write(sessionWSMessage{Type: "session.snapshot", RequestID: msg.RequestID, SessionID: msg.SessionID, Snapshot: ptr(makeSessionSnapshot(current))})
		default:
			write(sessionWSMessage{Type: "error", RequestID: msg.RequestID, Error: "unsupported message type"})
		}
	}
}

func ptr[T any](v T) *T { return &v }
