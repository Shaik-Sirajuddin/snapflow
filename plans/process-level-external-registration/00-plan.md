# Process-level external registration

## Problem

Nitro showed a live Snapflow GUI and SAP socket while snapshotd reported the
GUI external instance as closed. The registration and discovery descriptor are
currently owned by `PanelSingleton`; panel/plugin teardown therefore unregisters
the process even when the GUI process remains alive.

## Decision

Keep one external registration per Snapflow GUI process. Project selection is
mutable state on that registration. A project switch updates the path, reason,
and generation; it does not create or destroy the process registration.

Registration recovery must be immediate when project selection finds no current
instance id. Heartbeat recovery remains as a secondary self-healing path.

## Implementation

1. Make registration ownership process-scoped at the application integration
   boundary; panel teardown must not send `Stop` to the registration worker.
2. Add an explicit ensure/register operation before project updates so a closed
   or missing external row is recreated immediately.
3. Keep explicit unregister available for an owned shutdown handle; when the
   process exits unexpectedly, daemon lease/PID reconciliation removes the
   row and descriptor because a Rust process-global singleton cannot run a
   destructor at OS exit.
4. Add focused tests for lifecycle coalescing, project switching, and closed-row
   recovery; daemon ownership tests cover independent same-path processes.

## Local verification matrix

- Unit: registration worker re-registers when `current_id` is absent.
- Unit: project switch sends `updateOpenProject` with the same process identity.
- Integration: panel recreation does not remove the discovery descriptor.
- Real build: build the worktree's panel/GUI and SAP components with the local
  production-compatible build commands.
- Runtime A: launch the local VNC/X11 stack through `dev.make`, start one real
  Snapflow GUI, and confirm one process-level external registration.
- Runtime B: open project A, switch to project B, and verify logs show the same
  PID/instance identity with only `projectPath`/generation changing.
- Runtime C: tear down/recreate the panel surface without exiting Snapflow and
  verify the discovery descriptor and external lease remain present.
- Runtime D: close the GUI process and verify unregister when graceful teardown
  is available, otherwise verify lease/PID reconciliation cleanup.

The runtime checks are local-only. The `dev.make` file is supplied by the
current development checkout rather than upstream `main`; invoke it with the
upstream-based worktree as the explicit `WORKTREE` target:

```bash
make -f /home/siraj/Desktop/codebases/prv/multimedia_agent/multi_media_main/dev.make \
  REPO_ROOT=/home/siraj/Desktop/codebases/prv/multimedia_agent/multi_media_main-live-propagation \
  WORKTREE=/home/siraj/Desktop/codebases/prv/multimedia_agent/multi_media_main-live-propagation \
  vnc-up
```

Capture daemon and child logs under the worktree's runtime state directory,
then use `snapflowd status`, process/PID inspection, and registry rows as the
pass/fail evidence. Do not use the remote Nitro host for this verification.

## Risks

- The current branch may not contain a clean application-level owner for the
  registration; if so, introduce a process-lifetime host rather than retaining
  it in a panel object.
- Runtime verification requires a running GUI/Nitro host and must not kill the
  user’s active GUI process.
