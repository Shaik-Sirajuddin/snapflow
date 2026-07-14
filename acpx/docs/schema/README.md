# acpx generated schema artifacts

Three machine-readable documents live in this directory, all generated
directly from Rust source (never hand-written), so none of them can
silently drift from what `acpx-server` actually does -- each has a
committed-file-vs-freshly-generated drift-guard test in
`acpx-proto/tests/` that fails the build the moment source and
committed file disagree.

**One invariant across all three**: raw ACP method shapes
(`session/prompt`, `fs/*`, `terminal/*`, ...) are never re-authored
here. Wherever one of these documents needs to describe a raw-ACP
params/result shape, it `$ref`s straight into the upstream
`agent-client-protocol` crate's own `#[derive(JsonSchema)]` output
(confirmed present on every v1 request/response struct in
`agent-client-protocol-schema`, the version pinned in this workspace's
`Cargo.lock` -- check the lockfile, not just `Cargo.toml`'s loose `"1"`
range, for the exact resolved version). acpx only ever adds schema for
the parts raw ACP has no opinion on: its own JSON-RPC envelope, the
`_acpx` extension channel, and every gateway-native method
(`agents/*`, `profiles/*`, `mcp_servers/*`, `session/list`'s
gateway-aggregate branch).

## `acpx.openrpc.json` -- start here

An OpenRPC 1.3.2 document covering **every one of the 32 methods**
`acpx-server` dispatches, in both directions: the 24 a client sends to
acpx (`initialize`, `session/*`, `agents/*`, `profiles/*`,
`mcp_servers/*`) and the 8 acpx itself calls back out to a connected
client (`session/request_permission`, `fs/*`, `terminal/*`), tagged
`x-acpx-side` on each Method Object so the two are distinguishable.
This is the single most complete artifact -- if you only look at one
file, look at this one.

Chosen as the *primary* format (over OpenAPI) because acpx-server is
JSON-RPC method-dispatch over one logical endpoint per transport: the
method name embedded in the request body selects the shape, not a URL
path. OpenAPI's object model is fundamentally per-path-per-verb and
can't cleanly express that; OpenRPC's `methods: []` array (one
Content Descriptor per method's `params`/`result`) is the format
actually designed for this. See
`memory/acpx/gen/plans/acpx-openrpc-schema/00-goal.md` for the full
rationale this workspace settled on.

Two acpx-specific extensions beyond bare OpenRPC 1.3.2:
- `x-acpx-side`: `"client-to-agent"` or `"agent-to-client"` -- which of
  acpx-server's two duplex roles calls this method.
- `x-acpx-alternate-result` (`session/list` only): the real backend
  `ListSessionsResponse` shape returned when the caller supplies a
  selector, alongside `result`'s gateway-native aggregate shape used
  when no selector is given -- a genuine dual-shape method, not a
  simplification.
- `x-acpx-notification` (`session/cancel` only): `true` in place of a
  `result` -- this method is a true JSON-RPC notification (`id`
  optional, no reply ever sent), not a request with a null result.

`servers` lists all three real transports acpx-server exposes: stdio,
`http://127.0.0.1:8790/rpc`, `ws://127.0.0.1:8790/ws` (the actual
default `ACPX_HTTP_BIND`, see `setup.md`).

Regenerate: `bash scripts/gen_openrpc.sh`. Source:
`acpx-proto/src/openrpc.rs` + the method table in
`acpx-proto/src/methods.rs`.

## `acpx-http.openapi.json` -- the HTTP transport envelope and headers

A small, hand-composed OpenAPI 3.1 document for exactly the two fixed
HTTP paths `acpx-server` exposes (`POST /rpc`, `GET /ws` upgrade) --
not derived from a router walk, since there are only ever these two
paths, not worth building path-discovery tooling for.

This is the document to read for the **header-level contracts** no
bare JSON Schema body document or OpenRPC document has a place to
describe:
- `Authorization: Bearer <token>` -- gated on `ACPX_AUTH_TOKEN` being
  set server-side; unset (the default) means auth is disabled
  entirely and the header is ignored either way.
- `X-Acpx-Tenant` -- self-declared tenant partition key (see
  `memory/acpx/gen/plans/acpx-tenant-isolation/`), not an
  authentication mechanism; absent defaults to tenant `"default"`.
- `X-Acpx-Profile` -- `POST /rpc` only (no WS equivalent, correctly
  omitted from that path's parameter list); explicit managed-mode
  profile selection for `session/new`, highest precedence over an
  inline `params._acpx.profile` field.

stdio has no headers at all and is out of scope for this document --
it's still listed in `acpx.openrpc.json`'s `servers` array as a
reachable transport.

Regenerate: `bash scripts/gen_openapi.sh`. Source:
`acpx-proto/src/openapi.rs`.

## `acpx-wire.schema.json` -- acpx-native additions only, as a bare JSON Schema

The original (phase 20) artifact: a single JSON Schema (draft
2020-12) document covering the JSON-RPC 2.0 envelope every transport
frames messages in (`Request`/`Response`/`RpcError`/`RequestId`), the
`_acpx` extension object, and every gateway-native method's
params/result shape (`agents/*`, `profiles/*`, `mcp_servers/*`,
`session/list`'s default branch). Deliberately scoped narrower than
the other two documents -- it does **not** `$ref` raw ACP types at all
(unlike `acpx.openrpc.json`/`acpx-http.openapi.json`, which share a
generator registration that also includes upstream types) -- kept this
way so a tool that only cares about acpx's own additions doesn't have
to filter a much larger document down. Also has no notion of transport
headers (a JSON Schema document describes message bodies only) --
see `acpx-http.openapi.json` above for those.

Useful if you want *just* the acpx-native additions layered on top of
whatever raw-ACP JSON Schema you're already validating against
separately (see below).

Regenerate: `bash scripts/gen_schema.sh`. Source:
`acpx-proto/src/schema.rs`'s `build_wire_schema_document`.

## Getting the raw ACP schema (not bundled here, by design)

None of the three documents above re-publish raw ACP method shapes as
their own standalone artifact (they only `$ref` into upstream's
schema via `subschema_for`, which is not the same as vendoring a
copy). The `agent-client-protocol-schema` crate this workspace depends
on (transitively, via `agent-client-protocol` -- check `Cargo.lock`
for the exact pinned version, `Cargo.toml`'s `"1"` range alone isn't
enough) publishes its own generated `schema.json` per release on the
[agent-client-protocol GitHub releases page](https://github.com/agentclientprotocol/agent-client-protocol/releases)
-- fetch the release matching the exact version in your `Cargo.lock`
if you need the full, standalone raw-ACP schema rather than the
`$ref`-only view these three documents give you.

## Regenerating everything at once

```sh
bash scripts/gen_schema.sh
bash scripts/gen_openrpc.sh
bash scripts/gen_openapi.sh
cargo test -p acpx-proto   # each document's own drift-guard test
```
