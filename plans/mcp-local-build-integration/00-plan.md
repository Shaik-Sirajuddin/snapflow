# Local build and MCP integration verification

## Goal

Build the local snapshotd/MCP stack first, then verify the newly added MCP methods with the same Go integration patterns used by existing tools before reconciling into local main.

## Phases

1. Build snapshotd and regenerate its MCP schema.
2. Audit and add focused Go integration coverage for filter.describe, subtitles.setStyle, clip speed/fast-forward, and export completion notifications.
3. Run package, real-FFI, and schema tests; separate environment failures from code failures.
4. Review the changes and record verification evidence.

## Acceptance matrix

- local snapshotd build succeeds
- generated schema includes each new method and typed parameters
- Go MCP unit/schema tests pass
- new-method integration tests pass with mock backend
- real-FFI tests compile; runtime failures are explicitly classified
