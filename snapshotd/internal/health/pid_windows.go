//go:build windows

package health

import (
	"fmt"
	"os"
)

// PIDAlive on Windows: os.FindProcess opens a handle to the process and
// fails if it does not exist; there is no portable signal-0 equivalent, so
// this is a best-effort existence check only. Not exercised by the test suite
// in this sandbox (Linux-only here); documented as a known gap.
func PIDAlive(pid int) bool {
	if pid <= 0 {
		return false
	}
	proc, err := os.FindProcess(pid)
	if err != nil || proc == nil {
		return false
	}
	return true
}

// Windows currently uses the PID string as the processStart wire identity,
// matching panel-rust's non-Linux fallback. This is an existence check rather
// than a creation-time check; a native GetProcessTimes identity can replace
// it later without changing the registration schema.
func ProcessStartIdentity(pid int) (string, error) {
	if !PIDAlive(pid) {
		return "", fmt.Errorf("pid %d is not alive", pid)
	}
	return fmt.Sprintf("%d", pid), nil
}

func ProcessIdentityMatches(pid int, start string) bool {
	actual, err := ProcessStartIdentity(pid)
	return err == nil && actual == start
}
