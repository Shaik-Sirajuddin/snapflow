# acpx wire schema

[`acpx-wire.schema.json`](./acpx-wire.schema.json) is a JSON Schema
(draft 2020-12) document generated **from the server-side Rust source**
-- not hand-written -- describing every acpx-*native* addition to the
wire protocol:

- The JSON-RPC 2.0 envelope every transport (stdio, `POST /rpc`, `GET
  /ws`) frames messages in (`Request`/`Response`/`RpcError`/`RequestId`),
  defined in [`../../acpx-proto/src/jsonrpc.rs`](../../acpx-proto/src/jsonrpc.rs).
- The `_acpx` sibling-extension object carried by `session/new`
  (`AcpxExt`/`NewSessionParams`), defined in
  [`../../acpx-proto/src/session.rs`](../../acpx-proto/src/session.rs).
- The gateway-native `agents/*` methods that have no raw-ACP equivalent
  (`AgentStatus`/`AgentListEntry`), defined in
  [`../../acpx-proto/src/agent.rs`](../../acpx-proto/src/agent.rs).

## What this does *not* cover

Raw ACP method param/result shapes (`session/prompt`, `session/load`,
`fs/read_text_file`, ...) are **not** redefined here. Per
`acpx-proto/src/lib.rs`'s doc comment, `acpx-proto` re-exports the
official [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol)
crate as the single source of truth for those, so acpx never risks a
second, drifted copy of the raw ACP shapes. The upstream project
publishes its own generated `schema.json` (via the `generate` binary in
`agent-client-protocol-schema`) attached to each
[GitHub release](https://github.com/agentclientprotocol/agent-client-protocol/releases) --
use the one matching the version currently resolved in this workspace's
`Cargo.lock` (`agent-client-protocol = "1.2.0"` as of this writing; check
`Cargo.lock` for the version actually in use, since `Cargo.toml` pins a
loose `"1"` range shared by every acpx crate that touches raw ACP
types -- see that dependency's comment in the workspace `Cargo.toml`).

As upstream's own README notes: don't infer wire compatibility from the
crate/schema *release* version alone -- use the negotiated
`protocolVersion` and exchanged `capabilities` at runtime. The pinned
crate version only guarantees acpx-proto and acpx-client never drift onto
different *Rust type* shapes from each other.

## Regenerating

```bash
bash scripts/gen_schema.sh
```

Regenerates [`acpx-wire.schema.json`](./acpx-wire.schema.json) from the
current `#[derive(JsonSchema)]` types in `acpx-proto`. Run this any time
you add/change a field on `Request`, `Response`, `RpcError`, `RequestId`,
`AcpxExt`, `NewSessionParams`, `GatewaySessionId`, `AgentStatus`, or
`AgentListEntry`.

`acpx-proto/tests/schema_test.rs`'s
`committed_schema_file_matches_current_wire_types` fails the build if the
committed file and the derived types disagree, so this can't silently
drift the way a hand-maintained schema file would.

## Layout

- Root `oneOf`: every framed message is either a `Request` (covers plain
  requests and notifications -- `id` is optional) or a `Response`. This
  is the one invariant true across every transport.
- `$defs`: every acpx-native type, `$ref`-linked from the root union and
  from each other (e.g. `NewSessionParams._acpx` references `#/$defs/AcpxExt`).
  Shared substructure (like `RequestId`, used by both `Request` and
  `Response`) is emitted once and referenced, never duplicated inline.
