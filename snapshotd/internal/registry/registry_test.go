package registry

import (
	"os"
	"path/filepath"
	"testing"
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
