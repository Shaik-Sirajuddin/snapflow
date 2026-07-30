//go:build unix

package health

import (
	"fmt"
	"os"
	"runtime"
	"strconv"
	"strings"
	"syscall"
)

// PIDAlive reports whether pid refers to a live process, per 07's
// reconciliation sequence ("is PID still alive?"). On Unix this is
// os.FindProcess (which never fails on Unix -- it just wraps the pid) followed
// by sending signal 0, which performs existence/permission checks without
// actually delivering a signal.
func PIDAlive(pid int) bool {
	if pid <= 0 {
		return false
	}
	proc, err := os.FindProcess(pid)
	if err != nil {
		return false
	}
	err = proc.Signal(syscall.Signal(0))
	if err == nil {
		return true
	}
	return err == syscall.EPERM
}

// ProcessStartIdentity returns the kernel start-time identity used with a PID
// to prevent PID reuse from impersonating a previously registered process.
// Linux exposes this in /proc; other Unix platforms retain a conservative
// PID-only fallback until their native process identity is wired in.
func ProcessStartIdentity(pid int) (string, error) {
	if pid <= 0 {
		return "", fmt.Errorf("invalid pid %d", pid)
	}
	if runtime.GOOS == "linux" {
		stat, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
		if err != nil {
			return "", err
		}
		fields := strings.Fields(string(stat))
		if len(fields) <= 21 {
			return "", fmt.Errorf("/proc/%d/stat has no start time", pid)
		}
		return fields[21], nil
	}
	return strconv.Itoa(pid), nil
}

// ProcessIdentityMatches verifies both liveness and process start identity.
func ProcessIdentityMatches(pid int, start string) bool {
	if !PIDAlive(pid) || !SameUser(pid) || strings.TrimSpace(start) == "" {
		return false
	}
	actual, err := ProcessStartIdentity(pid)
	return err == nil && actual == start
}

// SameUser prevents a local client from registering another user's process.
// Linux exposes the real uid in /proc/<pid>/status; for other Unix targets
// this conservatively accepts only the current process until a native uid
// query is added.
func SameUser(pid int) bool {
	if pid == os.Getpid() {
		return true
	}
	if runtime.GOOS != "linux" {
		return false
	}
	data, err := os.ReadFile(fmt.Sprintf("/proc/%d/status", pid))
	if err != nil {
		return false
	}
	for _, line := range strings.Split(string(data), "\n") {
		if !strings.HasPrefix(line, "Uid:") {
			continue
		}
		fields := strings.Fields(line)
		return len(fields) >= 2 && fields[1] == strconv.Itoa(os.Geteuid())
	}
	return false
}
