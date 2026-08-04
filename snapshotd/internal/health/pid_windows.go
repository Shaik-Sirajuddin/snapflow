//go:build windows

package health

import (
	"fmt"
	"strconv"

	"golang.org/x/sys/windows"
)

// PIDAlive on Windows opens a query-limited process handle. Unlike
// os.FindProcess, this verifies that the process can actually be queried by
// the current user and gives ProcessStartIdentity the same handle semantics.
func PIDAlive(pid int) bool {
	if pid <= 0 {
		return false
	}
	handle, err := windows.OpenProcess(windows.PROCESS_QUERY_LIMITED_INFORMATION, false, uint32(pid))
	if err != nil {
		return false
	}
	defer windows.CloseHandle(handle)
	return true
}

// ProcessStartIdentity returns the Windows process creation FILETIME as a
// decimal uint64. It is stable for the lifetime of a process and changes when
// Windows reuses the numeric PID, closing the external-registration PID-reuse
// race without changing the wire schema.
func ProcessStartIdentity(pid int) (string, error) {
	if pid <= 0 {
		return "", fmt.Errorf("invalid pid %d", pid)
	}
	handle, err := windows.OpenProcess(windows.PROCESS_QUERY_LIMITED_INFORMATION, false, uint32(pid))
	if err != nil {
		return "", fmt.Errorf("open process %d: %w", pid, err)
	}
	defer windows.CloseHandle(handle)
	var creation, exit, kernel, user windows.Filetime
	if err := windows.GetProcessTimes(handle, &creation, &exit, &kernel, &user); err != nil {
		return "", fmt.Errorf("get process times %d: %w", pid, err)
	}
	value := uint64(creation.HighDateTime)<<32 | uint64(creation.LowDateTime)
	return strconv.FormatUint(value, 10), nil
}

func ProcessIdentityMatches(pid int, start string) bool {
	actual, err := ProcessStartIdentity(pid)
	return err == nil && actual == start
}
