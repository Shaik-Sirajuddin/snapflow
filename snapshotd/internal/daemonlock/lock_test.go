package daemonlock

import (
	"errors"
	"os"
	"path/filepath"
	"testing"
)

func TestAcquireRejectsSecondOwnerAndAllowsReacquire(t *testing.T) {
	home := t.TempDir()
	first, err := Acquire(home)
	if err != nil {
		t.Fatalf("first acquire: %v", err)
	}
	second, err := Acquire(home)
	if second != nil {
		_ = second.Close()
		t.Fatal("second acquire unexpectedly succeeded")
	}
	if !errors.Is(err, ErrAlreadyRunning) {
		t.Fatalf("second acquire error = %v, want ErrAlreadyRunning", err)
	}
	if _, err := os.Stat(filepath.Join(home, "daemon.lock")); err != nil {
		t.Fatalf("lock metadata missing while owner is active: %v", err)
	}
	if err := first.Close(); err != nil {
		t.Fatalf("release: %v", err)
	}
	if _, err := os.Stat(filepath.Join(home, "daemon.lock")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("lock metadata remains after release: %v", err)
	}
	third, err := Acquire(home)
	if err != nil {
		t.Fatalf("reacquire after release: %v", err)
	}
	_ = third.Close()
}

func TestAcquireReplacesStaleLockMetadata(t *testing.T) {
	home := t.TempDir()
	path := filepath.Join(home, "daemon.lock")
	if err := os.WriteFile(path, []byte("pid=999999\nstarted_at=1970-01-01T00:00:00Z\n"), 0o644); err != nil {
		t.Fatalf("write stale metadata: %v", err)
	}
	lock, err := Acquire(home)
	if err != nil {
		t.Fatalf("acquire with stale metadata: %v", err)
	}
	defer lock.Close()
	contents, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read refreshed metadata: %v", err)
	}
	if string(contents) == "pid=999999\nstarted_at=1970-01-01T00:00:00Z\n" {
		t.Fatal("stale lock metadata was not replaced")
	}
}
