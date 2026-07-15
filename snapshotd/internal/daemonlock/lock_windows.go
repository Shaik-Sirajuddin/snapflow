//go:build windows

package daemonlock

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"time"
)

var ErrAlreadyRunning = errors.New("snapshotd daemon is already running")

// Lock is the Windows placeholder. Windows support should use an OS named
// mutex before snapshotd is shipped there; the Unix implementation is the
// production path currently exercised by this repository.
type Lock struct {
	file *os.File
	path string
}

func Acquire(homeDir string) (*Lock, error) {
	if homeDir == "" {
		return nil, fmt.Errorf("daemonlock: home directory is required")
	}
	if err := os.MkdirAll(homeDir, 0o755); err != nil {
		return nil, fmt.Errorf("daemonlock: create home directory: %w", err)
	}
	path := filepath.Join(homeDir, "daemon.lock")
	f, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_RDWR, 0o644)
	if err != nil {
		if errors.Is(err, os.ErrExist) {
			return nil, fmt.Errorf("%w: %s", ErrAlreadyRunning, homeDir)
		}
		return nil, fmt.Errorf("daemonlock: open %s: %w", path, err)
	}
	_, _ = fmt.Fprintf(f, "pid=%d\nstarted_at=%s\n", os.Getpid(), time.Now().UTC().Format(time.RFC3339Nano))
	return &Lock{file: f, path: path}, nil
}

func (l *Lock) Close() error {
	if l == nil || l.file == nil {
		return nil
	}
	err := l.file.Close()
	if removeErr := os.Remove(l.path); err == nil {
		err = removeErr
	}
	l.file = nil
	return err
}
