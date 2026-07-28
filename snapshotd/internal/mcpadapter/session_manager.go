package mcpadapter

import (
	"fmt"
	"sync"
	"time"

	"github.com/google/uuid"
)

const streamableSessionIDPrefix = "mcp-session-"

// streamableSessionManager is the server-owned identity registry for the
// Streamable HTTP transport. mcp-go's default manager only validates ID
// syntax, which would allow an expired ID to recreate ephemeral transport
// state instead of producing the protocol-required 404.
type streamableSessionManager struct {
	mu       sync.Mutex
	sessions map[string]time.Time
	ttl      time.Duration
	now      func() time.Time
}

func newStreamableSessionManager(ttl time.Duration) *streamableSessionManager {
	return &streamableSessionManager{
		sessions: make(map[string]time.Time),
		ttl:      ttl,
		now:      time.Now,
	}
}

func (m *streamableSessionManager) Generate() string {
	id := streamableSessionIDPrefix + uuid.NewString()
	m.mu.Lock()
	m.sessions[id] = m.now().Add(m.ttl)
	m.mu.Unlock()
	return id
}

func (m *streamableSessionManager) Validate(id string) (bool, error) {
	m.mu.Lock()
	defer m.mu.Unlock()

	expiresAt, ok := m.sessions[id]
	now := m.now()
	if !ok || !now.Before(expiresAt) {
		delete(m.sessions, id)
		return false, fmt.Errorf("session not found: %s", id)
	}
	// Validation represents activity for POST/DELETE requests. GET requests
	// are touched by SSEServer's wrapper because mcp-go does not validate GET
	// session IDs itself.
	m.sessions[id] = now.Add(m.ttl)
	return false, nil
}

func (m *streamableSessionManager) Terminate(id string) (bool, error) {
	m.mu.Lock()
	delete(m.sessions, id)
	m.mu.Unlock()
	return false, nil
}

func (m *streamableSessionManager) validateGET(id string) error {
	_, err := m.Validate(id)
	return err
}
