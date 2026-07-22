//go:build !windows

package acpxmgr

import (
	"os"
	"strconv"
	"strings"
	"syscall"
)

// setpgidAttr puts the spawned acpx-server in its own process group (pgid ==
// its own pid, since Pgid is left at 0) instead of inheriting snapshotd's.
// This is what makes killProcessGroup below able to reach any of acpx-
// server's own child processes (real coding-agent subprocesses it spawns)
// that don't detach into a group of their own -- a plain Process.Kill only
// ever reaches the single direct child, never its descendants.
func setpgidAttr() *syscall.SysProcAttr {
	return &syscall.SysProcAttr{Setpgid: true}
}

// killProcessGroup sends sig to every process in pid's process group (a
// negative pid targets the group, per kill(2)) -- the real fix for the
// "acpx-server left as a stale process" gap: a bare Process.Kill(pid) only
// ever terminates that one process, never anything it spawned.
func killProcessGroup(pid int, sig syscall.Signal) error {
	err := syscall.Kill(-pid, sig)
	if err == syscall.ESRCH {
		// Already gone -- not an error for our purposes.
		return nil
	}
	return err
}

// isProcessAlive reports whether pid currently exists, using the POSIX
// kill(pid, 0) existence-check convention (sends no actual signal).
func isProcessAlive(pid int) bool {
	return syscall.Kill(pid, 0) == nil
}

// processCmdlineContains reports whether pid's argv (as recorded by the
// kernel via /proc, not user-spoofable the way a process's own self-
// reported name might be) contains needle. Used as a pid-reuse guard
// before we killpg a pid recorded in a stale pidfile from a prior unclean
// shutdown -- without this check, an unrelated process that happened to
// reuse acpx-server's old pid could be killed instead.
func processCmdlineContains(pid int, needle string) bool {
	raw, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/cmdline")
	if err != nil {
		// /proc unavailable (non-Linux unix, or pid already gone) --
		// caller decides how to treat "unknown"; we can't positively
		// confirm identity, so report false (don't kill on guesswork).
		return false
	}
	return strings.Contains(string(raw), needle)
}
