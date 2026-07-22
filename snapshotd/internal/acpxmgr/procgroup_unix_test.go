//go:build !windows

package acpxmgr

import (
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"
)

// waitUntilNotAlive polls isProcessAlive until it reports false or the
// deadline passes, failing the test on timeout -- process teardown after a
// signal is not instantaneous, so a single immediate check would be flaky.
func waitUntilNotAlive(t *testing.T, pid int) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if !isProcessAlive(pid) {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("pid %d still alive after waiting for it to exit", pid)
}

// TestKillProcessGroupReachesGrandchildren is the core proof behind this
// plan's acpx_daemon_orphan_process_cleanup fix: a real acpx-server spawns
// its own child processes (coding-agent subprocesses); a bare
// Process.Kill(acpxServerPid) never reaches those descendants, only
// killProcessGroup does. This spawns a real parent+grandchild pair under
// setpgidAttr (the same attribute Start() now sets on acpx-server) and
// proves one killProcessGroup call takes down both.
func TestKillProcessGroupReachesGrandchildren(t *testing.T) {
	dir := t.TempDir()
	childPidFile := filepath.Join(dir, "grandchild.pid")
	// The parent backgrounds a real grandchild (`sleep 30`), records its
	// pid, then waits -- so this test's cmd.Process.Pid is the parent,
	// and the grandchild is a distinct real OS process the parent alone
	// spawned, never registered with this test directly.
	script := "sleep 30 & echo $! > " + shellQuote(childPidFile) + "; wait"
	cmd := exec.Command("sh", "-c", script)
	cmd.SysProcAttr = setpgidAttr()
	if err := cmd.Start(); err != nil {
		t.Fatalf("spawn parent+grandchild fixture: %v", err)
	}
	// Reap promptly in the background, same as Start()'s own goroutine --
	// without this, a killed-but-unreaped process stays a zombie, which
	// kill(pid, 0) (isProcessAlive) still reports as "alive".
	go func() { _ = cmd.Wait() }()
	t.Cleanup(func() { _ = cmd.Process.Kill() })

	var grandchildPid int
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		raw, err := os.ReadFile(childPidFile)
		if err == nil && strings.TrimSpace(string(raw)) != "" {
			grandchildPid, err = strconv.Atoi(strings.TrimSpace(string(raw)))
			if err == nil {
				break
			}
		}
		time.Sleep(20 * time.Millisecond)
	}
	if grandchildPid == 0 {
		t.Fatal("grandchild never reported its pid in time")
	}
	if !isProcessAlive(grandchildPid) {
		t.Fatal("grandchild should be alive before the kill")
	}

	if err := killProcessGroup(cmd.Process.Pid, syscall.SIGKILL); err != nil {
		t.Fatalf("killProcessGroup: %v", err)
	}

	waitUntilNotAlive(t, cmd.Process.Pid)
	waitUntilNotAlive(t, grandchildPid)
}

// waitForCmdlinePopulated polls until pid's /proc/<pid>/cmdline is
// non-empty, or fails the test on timeout. There's a brief real window
// right after Start() returns where the child has forked but the kernel
// hasn't yet finished populating /proc/<pid>/cmdline from its execve --
// harmless in production (a stale pidfile's pid has been running for a
// while by the time it's ever read), but this test's "spawn, then
// immediately check cmdline" ordering needs to wait it out explicitly.
func waitForCmdlinePopulated(t *testing.T, pid int) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		raw, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/cmdline")
		if err == nil && len(raw) > 0 {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("pid %d's /proc cmdline never populated in time", pid)
}

// TestReapStalePidFileKillsMatchingOrphan proves the "daemon restarted
// after an unclean exit" half of the fix: a pidfile left behind from a
// prior run (snapshotd itself was hard-killed, so acpx-server was never
// reaped) gets killed on the next Start-time check, once its cmdline is
// confirmed to still match the configured binary.
func TestReapStalePidFileKillsMatchingOrphan(t *testing.T) {
	sleepBin, err := exec.LookPath("sleep")
	if err != nil {
		t.Skip("no `sleep` binary on PATH")
	}
	dir := t.TempDir()
	cfg := Config{
		BinPath:    sleepBin,
		ConfigPath: filepath.Join(dir, "acpx-config.json"),
	}
	cmd := exec.Command(sleepBin, "30")
	cmd.SysProcAttr = setpgidAttr()
	if err := cmd.Start(); err != nil {
		t.Fatalf("spawn orphan fixture: %v", err)
	}
	go func() { _ = cmd.Wait() }()
	t.Cleanup(func() { _ = cmd.Process.Kill() })
	waitForCmdlinePopulated(t, cmd.Process.Pid)
	writePidFile(cfg, cmd.Process.Pid)

	reapStalePidFile(cfg, slog.Default())

	waitUntilNotAlive(t, cmd.Process.Pid)
	if _, err := os.Stat(pidFilePath(cfg)); !os.IsNotExist(err) {
		t.Fatalf("expected the stale pidfile to be removed, stat err=%v", err)
	}
}

// TestReapStalePidFileLeavesMismatchedProcessAlone is the pid-reuse guard's
// own proof: a pidfile pointing at a real, alive process whose cmdline does
// NOT match the configured binary must be left running, not killed --
// otherwise pid reuse after a long-idle stale pidfile could kill an
// unrelated process.
func TestReapStalePidFileLeavesMismatchedProcessAlone(t *testing.T) {
	sleepBin, err := exec.LookPath("sleep")
	if err != nil {
		t.Skip("no `sleep` binary on PATH")
	}
	dir := t.TempDir()
	cfg := Config{
		// Deliberately does not match "sleep" -- reapStalePidFile must
		// refuse to kill on this mismatch.
		BinPath:    "/definitely/not/the/real/acpx-server",
		ConfigPath: filepath.Join(dir, "acpx-config.json"),
	}
	cmd := exec.Command(sleepBin, "30")
	cmd.SysProcAttr = setpgidAttr()
	if err := cmd.Start(); err != nil {
		t.Fatalf("spawn unrelated-process fixture: %v", err)
	}
	defer func() { _ = cmd.Process.Kill(); _ = cmd.Wait() }()
	writePidFile(cfg, cmd.Process.Pid)

	reapStalePidFile(cfg, slog.Default())

	if !isProcessAlive(cmd.Process.Pid) {
		t.Fatal("expected the mismatched process to be left running, but it was killed")
	}
}

func TestReapStalePidFileNoFileIsANoOp(t *testing.T) {
	dir := t.TempDir()
	cfg := Config{BinPath: "/bin/does-not-matter", ConfigPath: filepath.Join(dir, "acpx-config.json")}
	// Must not panic or error when there is nothing to reap.
	reapStalePidFile(cfg, slog.Default())
}

func shellQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}
