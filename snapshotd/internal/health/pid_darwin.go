//go:build darwin

package health

import (
	"fmt"

	"golang.org/x/sys/unix"
)

// processStartIdentityNonLinux uses Darwin's kernel process start time rather
// than a PID-only fallback, preventing PID reuse from impersonating an older
// external GUI registration.
func processStartIdentityNonLinux(pid int) (string, error) {
	if pid <= 0 {
		return "", fmt.Errorf("invalid pid %d", pid)
	}
	proc, err := unix.SysctlKinfoProc("kern.proc.pid", pid)
	if err != nil {
		return "", fmt.Errorf("sysctl process %d: %w", pid, err)
	}
	return fmt.Sprintf("%d.%d", proc.Proc.P_starttime.Sec, proc.Proc.P_starttime.Usec), nil
}
