# Windows test-suite portability

## Goal

Make the snapshotd Windows test gate test the native Windows transport instead
of running Unix-only socket tests and fixtures. Keep the first validation on a
branch; merge to `main` only after the Windows workflow succeeds.

## Findings from run 31480487347

- `sap_fixture` hard-codes `net.Listen("unix", ...)`, but Windows production
  endpoints are named pipes.
- Several integration tests directly call `net.Listen("unix", ...)` without
  Windows build tags or runtime skips.
- The Windows workflow runs advisory `go test ./...`, so failures are hidden
  behind a successful job.
- Independent portability failures exist in ACP Node fixture resolution and
  daemon-lock error normalization.

## Phases

1. Port child fixtures to the shared transport abstraction.
2. Exclude Unix-only integration tests from Windows and keep native transport
   tests in the Windows gate.
3. Fix deterministic Windows-only unit-test assumptions (ACP Node fixture and
   daemon-lock error mapping).
4. Run focused local checks, push this branch, and trigger `build-windows`.
5. Inspect the branch workflow; merge to `main` only after the required Windows
   checks pass.

## Verification matrix

- Linux: `go test ./...` in `snapshotd`.
- Windows-native gate: transport, health, config, plus Windows-safe tests.
- Windows full suite: `go test ./...` must have no failures, or any remaining
  exclusions must be explicit and documented in the workflow.
