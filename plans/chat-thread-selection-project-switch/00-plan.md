# Chat thread selection and project-switch isolation

## Problem

The panel-rust chat view can restore an archived thread as the initial view instead of opening the last explicitly selected thread or an empty new-thread view. Project switching must also preserve unrelated thread state rather than closing threads belonging to other projects.

## Approach

1. Trace startup/restoration selection and define an explicit priority: valid last user-selected thread, otherwise a new unselected view; archived/deleted threads are never selected implicitly.
2. Audit project-switch teardown and thread ownership. Keep unrelated threads alive and only detach state owned by the project being closed.
3. Add focused model/integration regressions covering archived-first startup, last-selection restoration, empty startup, and switching between projects with unrelated threads.
4. Run review and verification gates, then targeted Rust tests and any available runtime harness.

## Test matrix

- persisted selection points to archived thread -> new/unselected view
- persisted selection points to active thread -> that thread opens
- no persisted selection -> new/unselected view
- switch project A to B -> A's unrelated threads remain represented and B opens without selecting A's archived thread
- close project A -> only A-owned resources are detached

## Risks

- Existing persistence may intentionally restore a thread for compatibility; selection must distinguish user selection from stale/automatic selection.
- UI callback timing may require a runtime check beyond unit tests.
