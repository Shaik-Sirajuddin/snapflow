# acpx docs

Living, code-verified documentation for the `acpx` workspace. Each file
here is derived from the current source, not the original design draft --
for the historical design/planning trail (goal, original phased plan,
open-risks list as first written) see
`../../memory/acpx/gen/plans/acp-gateway-daemon/`. For the authoritative,
phase-by-phase implementation/coverage/gap log (what's actually been
built and tested, phase by phase), see [`../COVERAGE.md`](../COVERAGE.md)
-- that file remains the single source of truth for "is X implemented
and tested"; the docs here are the "how does this work / how do I run
it" companion.

- [`architecture.md`](./architecture.md) -- what acpx is, the request
  lifecycle, the process/concurrency model, transports, sessions,
  persistence, notifications.
- [`file-structure.md`](./file-structure.md) -- crate-by-crate,
  file-by-file map of the workspace.
- [`setup.md`](./setup.md) -- installing prerequisites, configuring, and
  running `acpx-server` (env vars, provisioning file schema, examples).
- [`development.md`](./development.md) -- building, testing (unit, e2e,
  real-adapter, black-box selftest), formatting/linting, and the
  conventions this codebase's phase-by-phase history has established.
- [`schema/README.md`](./schema/README.md) -- the generated,
  server-side-derived JSON Schema for acpx's wire-protocol additions
  (`schema/acpx-wire.schema.json`), how it's regenerated, and where to
  get the raw-ACP schema it deliberately doesn't duplicate.
