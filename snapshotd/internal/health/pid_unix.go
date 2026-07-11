//go:build unix

package health

import (
	"os"
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
