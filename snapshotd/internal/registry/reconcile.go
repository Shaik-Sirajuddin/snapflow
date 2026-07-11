package registry

import (
	"context"
	"time"
)

// PIDAliveFunc and SocketHealthyFunc are injected so tests can substitute
// fake liveness signals without spawning real processes; production callers
// use internal/health.PIDAlive / internal/health.SocketResponsive.
type PIDAliveFunc func(pid int) bool
type SocketHealthyFunc func(socketPath string, timeout time.Duration) bool

// RelaunchFunc relaunches a project's child process, returning a fresh
// ProcessInstance row (not yet persisted -- the reconciler persists it).
// Reconcile() tolerates a nil RelaunchFunc: rows that fail liveness/health
// checks are simply marked "crashed" with no relaunch attempted, matching
// the task's "mark crashed and relaunch (or just mark crashed if no relaunch
// source available)" requirement.
type RelaunchFunc func(ctx context.Context, project Project, prior ProcessInstance) (ProcessInstance, error)

// ReconcileOutcome describes what the reconciler did with a single row.
type ReconcileOutcome struct {
	Instance ProcessInstance
	Action   string // "reconnected" | "marked_crashed" | "relaunched" | "skipped_no_project"
	Err      error
}

// Reconciler implements the startup reconciliation sequence from
// 07-daemon-persistence.md: for every ProcessInstance row with status
// "ready", check PID liveness, then socket health; reconnect (stay "ready")
// if both pass, otherwise mark "crashed" and optionally relaunch.
type Reconciler struct {
	Reg           *Registry
	PIDAlive      PIDAliveFunc
	SocketHealthy SocketHealthyFunc
	Relaunch      RelaunchFunc // optional
	HealthTimeout time.Duration
}

// Reconcile runs the sequence once, synchronously, and returns one outcome
// per "ready" row found at the start of the call.
func (rc *Reconciler) Reconcile(ctx context.Context) ([]ReconcileOutcome, error) {
	timeout := rc.HealthTimeout
	if timeout <= 0 {
		timeout = 500 * time.Millisecond
	}

	rows, err := rc.Reg.ListByStatus(StatusReady)
	if err != nil {
		return nil, err
	}

	var outcomes []ReconcileOutcome
	for _, row := range rows {
		outcome := rc.reconcileOne(ctx, row, timeout)
		outcomes = append(outcomes, outcome)
	}
	return outcomes, nil
}

func (rc *Reconciler) reconcileOne(ctx context.Context, row ProcessInstance, timeout time.Duration) ReconcileOutcome {
	pidOK := rc.PIDAlive != nil && rc.PIDAlive(row.PID)
	if pidOK {
		sockOK := rc.SocketHealthy != nil && rc.SocketHealthy(row.SocketPath, timeout)
		if sockOK {
			// PID alive + socket responsive: reconnect, stay ready.
			_ = rc.Reg.TouchHealthCheck(row.ID)
			return ReconcileOutcome{Instance: row, Action: "reconnected"}
		}
	}

	// Either PID is dead, or PID is alive but the socket doesn't respond --
	// both are treated as crashed per 07's sequence diagram (both branches of
	// the alt/else lead to "mark row crashed").
	if err := rc.Reg.UpdateProcessInstanceStatus(row.ID, StatusCrashed); err != nil {
		return ReconcileOutcome{Instance: row, Action: "marked_crashed", Err: err}
	}
	_ = rc.Reg.Audit(row.ProjectID, AuditCrash, "reconciliation: pid_alive="+boolStr(pidOK))
	row.Status = StatusCrashed

	if rc.Relaunch == nil {
		return ReconcileOutcome{Instance: row, Action: "marked_crashed"}
	}

	project, err := rc.Reg.GetProject(row.ProjectID)
	if err != nil {
		return ReconcileOutcome{Instance: row, Action: "skipped_no_project", Err: err}
	}

	fresh, err := rc.Relaunch(ctx, *project, row)
	if err != nil {
		return ReconcileOutcome{Instance: row, Action: "marked_crashed", Err: err}
	}
	if err := rc.Reg.CreateProcessInstance(&fresh); err != nil {
		return ReconcileOutcome{Instance: fresh, Action: "relaunched", Err: err}
	}
	_ = rc.Reg.Audit(fresh.ProjectID, AuditRestart, "reconciliation relaunch of "+row.ID)
	return ReconcileOutcome{Instance: fresh, Action: "relaunched"}
}

func boolStr(b bool) string {
	if b {
		return "true"
	}
	return "false"
}
