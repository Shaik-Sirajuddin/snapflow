package mcpadapter

import (
	"testing"
	"time"
)

func TestStreamableSessionManagerExpiresFromLastActivity(t *testing.T) {
	now := time.Date(2026, time.July, 28, 12, 0, 0, 0, time.UTC)
	m := newStreamableSessionManager(time.Hour)
	m.now = func() time.Time { return now }

	id := m.Generate()
	now = now.Add(59 * time.Minute)
	if terminated, err := m.Validate(id); terminated || err != nil {
		t.Fatalf("session should remain live after activity: terminated=%v err=%v", terminated, err)
	}
	now = now.Add(59*time.Minute + time.Minute)
	if _, err := m.Validate(id); err == nil {
		t.Fatal("expected session to expire one hour after its last activity")
	}
	if _, err := m.Validate(id); err == nil {
		t.Fatal("expected expired session to remain unknown")
	}
}

func TestStreamableSessionManagerTerminate(t *testing.T) {
	m := newStreamableSessionManager(time.Hour)
	id := m.Generate()
	if _, err := m.Terminate(id); err != nil {
		t.Fatalf("terminate: %v", err)
	}
	if _, err := m.Validate(id); err == nil {
		t.Fatal("terminated session was accepted")
	}
}
