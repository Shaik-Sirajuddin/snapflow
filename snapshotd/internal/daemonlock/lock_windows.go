//go:build windows

package daemonlock

import (
	"crypto/sha256"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"golang.org/x/sys/windows"
)

var ErrAlreadyRunning = errors.New("snapshotd daemon is already running")

type Lock struct {
	file  *os.File
	path  string
	mutex windows.Handle
}

func Acquire(homeDir string) (*Lock, error) {
	if homeDir == "" {
		return nil, fmt.Errorf("daemonlock: home directory is required")
	}
	if err := os.MkdirAll(homeDir, 0o755); err != nil {
		return nil, fmt.Errorf("daemonlock: create home directory: %w", err)
	}
	path := filepath.Join(homeDir, "daemon.lock")
	// A named mutex is released by Windows when the owning process exits, so
	// a crash cannot strand a lock file and block the next install/start.
	// Windows volume paths are case-insensitive. Normalize case before
	// deriving the mutex name so `C:\Users\Alice` and `c:\users\alice`
	// cannot start two daemons for the same profile.
	digest := sha256.Sum256([]byte(strings.ToLower(filepath.Clean(homeDir))))
	name := fmt.Sprintf("Local\\SnapflowSnapshotd-%x", digest[:12])
	mutex, err := windows.CreateMutex(nil, false, windows.StringToUTF16Ptr(name))
	if err != nil && !errors.Is(err, windows.ERROR_ALREADY_EXISTS) {
		return nil, fmt.Errorf("daemonlock: create mutex: %w", err)
	}
	if errors.Is(err, windows.ERROR_ALREADY_EXISTS) || windows.GetLastError() == windows.ERROR_ALREADY_EXISTS {
		_ = windows.CloseHandle(mutex)
		return nil, fmt.Errorf("%w: %s", ErrAlreadyRunning, homeDir)
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0o644)
	if err != nil {
		_ = windows.CloseHandle(mutex)
		return nil, fmt.Errorf("daemonlock: open %s: %w", path, err)
	}
	_, _ = fmt.Fprintf(f, "pid=%d\nstarted_at=%s\n", os.Getpid(), time.Now().UTC().Format(time.RFC3339Nano))
	return &Lock{file: f, path: path, mutex: mutex}, nil
}

func (l *Lock) Close() error {
	if l == nil || l.file == nil {
		return nil
	}
	err := l.file.Close()
	if l.mutex != 0 {
		if closeErr := windows.CloseHandle(l.mutex); err == nil {
			err = closeErr
		}
	}
	if removeErr := os.Remove(l.path); err == nil {
		err = removeErr
	}
	l.file = nil
	return err
}
