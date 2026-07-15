//go:build !windows

// Package daemonlock provides the process singleton guard for snapshotd.
package daemonlock

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"syscall"
	"time"
)

var ErrAlreadyRunning = errors.New("snapshotd daemon is already running")

// Lock is held for the lifetime of one snapshotd serve process.
type Lock struct {
	file *os.File
	path string
}

// Acquire obtains an advisory, non-blocking lock in homeDir. The kernel
// releases the lock if the owner exits unexpectedly, so a stale lock file
// never prevents a later daemon from starting.
func Acquire(homeDir string) (*Lock, error) {
	if homeDir == "" {
		return nil, fmt.Errorf("daemonlock: home directory is required")
	}
	if err := os.MkdirAll(homeDir, 0o755); err != nil {
		return nil, fmt.Errorf("daemonlock: create home directory: %w", err)
	}

	path := filepath.Join(homeDir, "daemon.lock")
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0o644)
	if err != nil {
		return nil, fmt.Errorf("daemonlock: open %s: %w", path, err)
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		_ = f.Close()
		if errors.Is(err, syscall.EWOULDBLOCK) || errors.Is(err, syscall.EAGAIN) {
			return nil, fmt.Errorf("%w: %s", ErrAlreadyRunning, homeDir)
		}
		return nil, fmt.Errorf("daemonlock: lock %s: %w", path, err)
	}

	if err := f.Truncate(0); err == nil {
		_, _ = fmt.Fprintf(f, "pid=%d\nstarted_at=%s\n", os.Getpid(), time.Now().UTC().Format(time.RFC3339Nano))
		_ = f.Sync()
	}
	return &Lock{file: f, path: path}, nil
}

// Close releases the kernel lock and removes the metadata file. Removing the
// pathname is only cosmetic; ownership is provided by the held file lock.
func (l *Lock) Close() error {
	if l == nil || l.file == nil {
		return nil
	}
	errUnlock := syscall.Flock(int(l.file.Fd()), syscall.LOCK_UN)
	errClose := l.file.Close()
	errRemove := os.Remove(l.path)
	if errUnlock != nil {
		return errUnlock
	}
	if errClose != nil {
		return errClose
	}
	if errRemove != nil && !errors.Is(errRemove, os.ErrNotExist) {
		return errRemove
	}
	l.file = nil
	return nil
}
