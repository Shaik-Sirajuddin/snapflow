//go:build linux

package health

import "strconv"

// The Linux implementation returns /proc start time before reaching this
// helper; keep the symbol available for compile-time selection.
func processStartIdentityNonLinux(pid int) (string, error) {
	return strconv.Itoa(pid), nil
}
