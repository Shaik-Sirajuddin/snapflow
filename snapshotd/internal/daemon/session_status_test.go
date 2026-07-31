package daemon

import (
	"testing"
	"time"

	"snapshotd/internal/session"
)

func TestSessionSubscriptionFansOutToIndependentClients(t *testing.T) {
	d := &Daemon{
		Sessions:        session.NewMemory(time.Hour),
		sessionWatchers: make(map[string]map[uint64]chan session.Session),
	}
	defer d.Sessions.Close()
	if _, err := d.Sessions.Create("session-1", "mcp", time.Minute); err != nil {
		t.Fatal(err)
	}
	if _, err := d.SetSessionCorrelation("session-1", "acp-1"); err != nil {
		t.Fatal(err)
	}
	first, firstUpdates, cancelFirst, err := d.SubscribeSession("acp-1")
	if err != nil {
		t.Fatal(err)
	}
	second, secondUpdates, cancelSecond, err := d.SubscribeSession("session-1")
	if err != nil {
		t.Fatal(err)
	}
	defer cancelFirst()
	defer cancelSecond()
	if first.ID != second.ID {
		t.Fatalf("subscriptions returned different sessions")
	}

	d.updateSessionDerived("session-1", "project-1", "/projects/demo", "connected")
	for name, updates := range map[string]<-chan session.Session{"first": firstUpdates, "second": secondUpdates} {
		select {
		case got := <-updates:
			if got.ProjectPath != "/projects/demo" || got.Revision == 0 {
				t.Errorf("%s got %+v", name, got)
			}
		case <-time.After(time.Second):
			t.Errorf("%s did not receive update", name)
		}
	}

	cancelFirst()
	d.updateSessionDerived("session-1", "project-2", "/projects/next", "connected")
	select {
	case _, ok := <-firstUpdates:
		if ok {
			t.Error("first subscription should be closed after cancel")
		}
	case <-time.After(time.Second):
		t.Error("first subscription was not closed")
	}
	select {
	case got := <-secondUpdates:
		if got.ProjectID != "project-2" {
			t.Errorf("second got %+v", got)
		}
	case <-time.After(time.Second):
		t.Error("second subscription did not remain active")
	}
}

func TestSessionStatusPublishesOnlyTransitions(t *testing.T) {
	d := &Daemon{
		Sessions:        session.NewMemory(time.Hour),
		sessionWatchers: make(map[string]map[uint64]chan session.Session),
	}
	defer d.Sessions.Close()
	if _, err := d.Sessions.Create("session-1", "mcp", time.Minute); err != nil {
		t.Fatal(err)
	}
	_, updates, cancel, err := d.SubscribeSession("session-1")
	if err != nil {
		t.Fatal(err)
	}
	defer cancel()

	d.updateSessionStatus("session-1", "error")
	select {
	case got := <-updates:
		if got.ConnectionStatus != "error" {
			t.Fatalf("status = %q, want error", got.ConnectionStatus)
		}
	case <-time.After(time.Second):
		t.Fatal("error transition was not published")
	}

	d.updateSessionStatus("session-1", "error")
	select {
	case got := <-updates:
		t.Fatalf("duplicate status transition published: %+v", got)
	case <-time.After(20 * time.Millisecond):
	}

	d.updateSessionStatus("session-1", "connected")
	select {
	case got := <-updates:
		if got.ConnectionStatus != "connected" || got.Revision < 2 {
			t.Fatalf("recovery = %+v", got)
		}
	case <-time.After(time.Second):
		t.Fatal("recovery transition was not published")
	}
}

func TestExpiredSessionClosesAndRemovesWatchers(t *testing.T) {
	d := &Daemon{
		Sessions:        session.NewMemory(time.Hour),
		sessionWatchers: make(map[string]map[uint64]chan session.Session),
	}
	defer d.Sessions.Close()
	if _, err := d.Sessions.Create("session-expiring", "mcp", 20*time.Millisecond); err != nil {
		t.Fatal(err)
	}
	_, updates, cancel, err := d.SubscribeSession("session-expiring")
	if err != nil {
		t.Fatal(err)
	}
	time.Sleep(40 * time.Millisecond)
	d.reapExpiredSessionWatchers()
	select {
	case _, ok := <-updates:
		if ok {
			t.Fatal("expired watcher delivered an update instead of closing")
		}
	case <-time.After(time.Second):
		t.Fatal("expired watcher was not closed")
	}
	if len(d.sessionWatchers) != 0 {
		t.Fatalf("expired watcher registry = %+v", d.sessionWatchers)
	}
	// The caller-side cleanup remains safe after the reaper has removed it.
	cancel()
}

func TestRegisterSessionContextPersistsAcpIdentityBeforeProjectSelection(t *testing.T) {
	d := newTestDaemon(t, buildFixture(t))
	if err := d.RegisterSessionContext("ctx-token", "acp-1"); err != nil {
		t.Fatal(err)
	}
	record, err := d.Reg.GetMcpContext("ctx-token")
	if err != nil {
		t.Fatal(err)
	}
	if record.ACPSessionID != "acp-1" || record.TargetProjectID != "" {
		t.Fatalf("unexpected pre-project context: %+v", record)
	}
}
