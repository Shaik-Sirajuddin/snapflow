//go:build windows

package health

import "os"

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
