//go:build windows

package acpxmgr

import (
	"os"
	"syscall"
)

// setpgidAttr is a no-op on Windows: exec.Cmd.SysProcAttr has no Setpgid
// field there (job objects are the Windows equivalent, not implemented
// here yet -- see acpxmgr's package doc). Windows is not the production
// path currently exercised by this repository (see daemonlock's own
// Windows placeholder for the same caveat).
func setpgidAttr() *syscall.SysProcAttr {
	return nil
}

// killProcessGroup falls back to a single-process kill on Windows -- there
// is no process-group semantics to fall back on without job objects.
func killProcessGroup(pid int, _ syscall.Signal) error {
	proc, err := os.FindProcess(pid)
	if err != nil {
		return err
	}
	return proc.Kill()
}

// isProcessAlive is a best-effort existence check on Windows: opening the
// process handle is the closest equivalent to POSIX kill(pid, 0).
func isProcessAlive(pid int) bool {
	proc, err := os.FindProcess(pid)
	return err == nil && proc != nil
}

// processCmdlineContains has no /proc to read on Windows; always report
// "unknown" so the stale-pidfile cleanup path never kills on guesswork.
func processCmdlineContains(pid int, needle string) bool {
	return false
}
