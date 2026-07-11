package session

import (
	"sync"
	"time"
)

// Memory is the v1-default Store implementation: a mutex-guarded map with
// TTL-based expiry. Expiry is enforced two ways -- lazily (a Lookup/Touch
// against an expired entry deletes it and returns ErrNotFound) and via a
// periodic background sweep, so idle-but-forgotten sessions don't linger in
// memory indefinitely between lookups (07's Redis sequence relies on Redis's
// own EXPIRE; this is the equivalent for the in-process fallback).
type Memory struct {
	mu       sync.Mutex
	sessions map[string]Session

	sweepInterval time.Duration
	stop          chan struct{}
	stopped       sync.Once
}

// NewMemory constructs a Memory store and starts its background sweep
// goroutine at the given interval (a sensible default is used if <= 0).
func NewMemory(sweepInterval time.Duration) *Memory {
	if sweepInterval <= 0 {
		sweepInterval = 30 * time.Second
	}
	m := &Memory{
		sessions:      make(map[string]Session),
		sweepInterval: sweepInterval,
		stop:          make(chan struct{}),
	}
	go m.sweepLoop()
	return m
}

func (m *Memory) sweepLoop() {
	ticker := time.NewTicker(m.sweepInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ticker.C:
			m.sweepExpired()
		case <-m.stop:
			return
		}
	}
}

func (m *Memory) sweepExpired() {
	now := time.Now()
	m.mu.Lock()
	defer m.mu.Unlock()
	for id, s := range m.sessions {
		if now.After(s.ExpiresAt) {
			delete(m.sessions, id)
		}
	}
}

func (m *Memory) Create(id, clientKind string, ttl time.Duration) (Session, error) {
	now := time.Now()
	s := Session{
		ID:         id,
		ClientKind: clientKind,
		CreatedAt:  now,
		ExpiresAt:  now.Add(ttl),
	}
	m.mu.Lock()
	m.sessions[id] = s
	m.mu.Unlock()
	return s, nil
}

func (m *Memory) Touch(id string, ttl time.Duration) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	s, ok := m.sessions[id]
	if !ok || time.Now().After(s.ExpiresAt) {
		delete(m.sessions, id)
		return ErrNotFound
	}
	s.ExpiresAt = time.Now().Add(ttl)
	m.sessions[id] = s
	return nil
}

func (m *Memory) Lookup(id string) (Session, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	s, ok := m.sessions[id]
	if !ok {
		return Session{}, ErrNotFound
	}
	if time.Now().After(s.ExpiresAt) {
		delete(m.sessions, id)
		return Session{}, ErrNotFound
	}
	return s, nil
}

func (m *Memory) BindProject(id, projectID string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	s, ok := m.sessions[id]
	if !ok || time.Now().After(s.ExpiresAt) {
		delete(m.sessions, id)
		return ErrNotFound
	}
	s.ProjectID = projectID
	m.sessions[id] = s
	return nil
}

func (m *Memory) Expire(id string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.sessions, id)
	return nil
}

func (m *Memory) List() []Session {
	now := time.Now()
	m.mu.Lock()
	defer m.mu.Unlock()
	out := make([]Session, 0, len(m.sessions))
	for _, s := range m.sessions {
		if now.After(s.ExpiresAt) {
			continue
		}
		out = append(out, s)
	}
	return out
}

func (m *Memory) Close() error {
	m.stopped.Do(func() { close(m.stop) })
	return nil
}

var _ Store = (*Memory)(nil)
