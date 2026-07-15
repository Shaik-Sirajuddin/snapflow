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
