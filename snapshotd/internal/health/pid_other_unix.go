//go:build !darwin && !linux

package health

import "strconv"

// Other Unix targets retain the conservative PID fallback until their native
// process-start API is wired. Linux and Darwin use kernel start identities.
func processStartIdentityNonLinux(pid int) (string, error) {
	return strconv.Itoa(pid), nil
}
