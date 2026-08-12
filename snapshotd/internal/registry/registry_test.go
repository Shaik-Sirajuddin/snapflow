package registry

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestOpen_NoBackupOnFreshInstall(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "registry.db")

	reg, err := Open(path)
	if err != nil {
		t.Fatalf("open registry: %v", err)
	}
	reg.Close()

	if _, err := os.Stat(path + ".prev"); !os.IsNotExist(err) {
		t.Fatalf("expected no .prev backup for a fresh install, stat err = %v", err)
	}
}

func TestOpen_BacksUpExistingDBBeforeMigrating(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "registry.db")

	reg, err := Open(path)
	if err != nil {
		t.Fatalf("open registry: %v", err)
	}
	if err := reg.CreateProject(&Project{ID: "p1", RootDir: dir}); err != nil {
		t.Fatalf("create project: %v", err)
	}
	reg.Close()

	reg2, err := Open(path)
	if err != nil {
		t.Fatalf("reopen registry: %v", err)
	}
	defer reg2.Close()

	if _, err := os.Stat(path + ".prev"); err != nil {
		t.Fatalf("expected .prev backup after reopening an existing db: %v", err)
	}

	projects, err := reg2.ListProjects()
	if err != nil {
		t.Fatalf("list projects: %v", err)
	}
	if len(projects) != 1 || projects[0].ID != "p1" {
		t.Fatalf("expected data to survive backup+reopen, got %+v", projects)
	}
}

func TestOpen_SetsWALJournalModeAndBusyTimeout(t *testing.T) {
	reg := openTestRegistry(t)

	var mode string
	if err := reg.DB().Raw("PRAGMA journal_mode").Scan(&mode).Error; err != nil {
		t.Fatalf("query journal_mode: %v", err)
	}
	if mode != "wal" {
		t.Fatalf("expected journal_mode=wal, got %q", mode)
	}

	var busyTimeout int
	if err := reg.DB().Raw("PRAGMA busy_timeout").Scan(&busyTimeout).Error; err != nil {
		t.Fatalf("query busy_timeout: %v", err)
	}
	if busyTimeout != 5000 {
		t.Fatalf("expected busy_timeout=5000, got %d", busyTimeout)
	}
}

func TestAuditOnce_InitKindIdempotent(t *testing.T) {
	reg := openTestRegistry(t)
	if err := reg.AuditOnce("proj-1", AuditInit, "first open"); err != nil {
		t.Fatalf("AuditOnce: %v", err)
	}
	if err := reg.AuditOnce("proj-1", AuditInit, "second open must not insert"); err != nil {
		t.Fatalf("AuditOnce second: %v", err)
	}
	// Different project still gets its own init row.
	if err := reg.AuditOnce("proj-2", AuditInit, "other project"); err != nil {
		t.Fatalf("AuditOnce other: %v", err)
	}
	events, err := reg.ListAuditEvents("proj-1")
	if err != nil {
		t.Fatalf("ListAuditEvents: %v", err)
	}
	initCount := 0
	for _, e := range events {
		if e.Kind == AuditInit {
			initCount++
		}
	}
	if initCount != 1 {
		t.Fatalf("expected exactly 1 init audit for proj-1, got %d (%+v)", initCount, events)
	}
	events2, err := reg.ListAuditEvents("proj-2")
	if err != nil {
		t.Fatalf("ListAuditEvents proj-2: %v", err)
	}
	if len(events2) != 1 || events2[0].Kind != AuditInit {
		t.Fatalf("expected one init for proj-2, got %+v", events2)
	}
}

func TestEnsureProjectForPath_RefreshesSelectedMltFilename(t *testing.T) {
	reg := openTestRegistry(t)
	root := t.TempDir()
	first, err := reg.EnsureProjectForPath(filepath.Join(root, "d.mlt"))
	if err != nil {
		t.Fatalf("first path: %v", err)
	}
	second, err := reg.EnsureProjectForPath(filepath.Join(root, "ds.mlt"))
	if err != nil {
		t.Fatalf("second path: %v", err)
	}
	if first.ID != second.ID || second.MltFileName != "ds.mlt" {
		t.Fatalf("same folder should retain id and refresh filename: first=%+v second=%+v", first, second)
	}
}

func TestListProjects_ExternalActiveOwnerSetsStatusFlags(t *testing.T) {
	reg := openTestRegistry(t)
	root := t.TempDir()
	project, err := reg.EnsureProjectForPath(filepath.Join(root, "ds.mlt"))
	if err != nil {
		t.Fatal(err)
	}
	now := time.Now().UTC()
	instance := &ExternalInstance{ID: "external-1", InstanceNonce: "nonce-1", PID: 1, ProcessStart: "start-1", ProjectPath: filepath.Join(root, "ds.mlt"), Status: ExternalStatusOpen, LastSeenAt: now, LeaseExpiresAt: now.Add(time.Minute)}
	if err := reg.SaveExternalInstance(instance); err != nil {
		t.Fatal(err)
	}
	if err := reg.SaveProjectActiveOwner(&ProjectActiveOwner{ProjectID: project.ID, Owner: OwnershipExternal, InstanceID: instance.ID, InstanceNonce: instance.InstanceNonce, PID: instance.PID, ProcessStart: instance.ProcessStart, LeaseExpiresAt: now.Add(time.Minute)}); err != nil {
		t.Fatal(err)
	}
	projects, err := reg.ListProjects()
	if err != nil {
		t.Fatal(err)
	}
	if len(projects) != 1 || !projects[0].Active || !projects[0].IsOpen || !projects[0].Open {
		t.Fatalf("expected external owner to set active/open flags: %+v", projects)
	}
}
