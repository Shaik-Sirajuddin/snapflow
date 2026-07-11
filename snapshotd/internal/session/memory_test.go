package session

import (
	"testing"
	"time"
)

func TestMemory_CreateLookupTouch(t *testing.T) {
	m := NewMemory(10 * time.Millisecond)
	defer m.Close()

	s, err := m.Create("sess-1", "mcp", 50*time.Millisecond)
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	if s.ID != "sess-1" {
		t.Fatalf("unexpected id %q", s.ID)
	}

	got, err := m.Lookup("sess-1")
	if err != nil {
		t.Fatalf("lookup: %v", err)
	}
	if got.ClientKind != "mcp" {
		t.Fatalf("unexpected client kind %q", got.ClientKind)
	}

	if err := m.BindProject("sess-1", "proj-a"); err != nil {
		t.Fatalf("bind: %v", err)
	}
	got, _ = m.Lookup("sess-1")
	if got.ProjectID != "proj-a" {
		t.Fatalf("expected bound project proj-a, got %q", got.ProjectID)
	}

	if err := m.Touch("sess-1", 50*time.Millisecond); err != nil {
		t.Fatalf("touch: %v", err)
	}
}

func TestMemory_LazyExpiry(t *testing.T) {
	m := NewMemory(time.Hour) // sweep effectively disabled; force lazy path
	defer m.Close()

	if _, err := m.Create("sess-2", "cli", 10*time.Millisecond); err != nil {
		t.Fatalf("create: %v", err)
	}
	time.Sleep(30 * time.Millisecond)

	if _, err := m.Lookup("sess-2"); err != ErrNotFound {
		t.Fatalf("expected ErrNotFound after expiry, got %v", err)
	}
	if err := m.Touch("sess-2", time.Second); err != ErrNotFound {
		t.Fatalf("expected ErrNotFound on touch of expired session, got %v", err)
	}
}

func TestMemory_BackgroundSweepRemovesExpired(t *testing.T) {
	m := NewMemory(15 * time.Millisecond)
	defer m.Close()

	if _, err := m.Create("sess-3", "cli", 5*time.Millisecond); err != nil {
		t.Fatalf("create: %v", err)
	}

	deadline := time.Now().Add(500 * time.Millisecond)
	for time.Now().Before(deadline) {
		m.mu.Lock()
		_, present := m.sessions["sess-3"]
		m.mu.Unlock()
		if !present {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("expected background sweep to remove expired session")
}

func TestMemory_ExpireRemovesImmediately(t *testing.T) {
	m := NewMemory(time.Hour)
	defer m.Close()

	if _, err := m.Create("sess-4", "cli", time.Minute); err != nil {
		t.Fatalf("create: %v", err)
	}
	if err := m.Expire("sess-4"); err != nil {
		t.Fatalf("expire: %v", err)
	}
	if _, err := m.Lookup("sess-4"); err != ErrNotFound {
		t.Fatalf("expected ErrNotFound after explicit expire, got %v", err)
	}
}

func TestMemory_List(t *testing.T) {
	m := NewMemory(time.Hour)
	defer m.Close()

	_, _ = m.Create("a", "cli", time.Minute)
	_, _ = m.Create("b", "mcp", time.Minute)

	sessions := m.List()
	if len(sessions) != 2 {
		t.Fatalf("expected 2 live sessions, got %d", len(sessions))
	}
}
