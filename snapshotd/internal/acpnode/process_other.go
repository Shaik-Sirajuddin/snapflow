//go:build !windows

package acpnode

import "os/exec"

func configureHiddenProcess(_ *exec.Cmd) {}
