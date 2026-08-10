//go:build windows

package acpnode

import (
	"os/exec"
	"syscall"
)

// configureHiddenProcess prevents node/npm probes started by the daemon from
// opening a console window when snapshotd itself was launched by the GUI.
func configureHiddenProcess(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{CreationFlags: 0x08000000}
}
