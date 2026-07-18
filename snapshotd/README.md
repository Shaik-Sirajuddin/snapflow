# snapshotd

`snapshotd` is the Snapshot Daemon Protocol (SDP) daemon: a Go process
manager + JSON-RPC 2.0 proxy that launches and tracks per-project child
processes (each exposing a SAP JSON-RPC socket) and exposes an MCP interface
to agents. This implements the design in
`memory/head/gen/rust-fork/06-daemon-mcp-proxy.md`,
`07-daemon-persistence.md`, `08-lifecycle-and-cli.md`, and
`09-project-folder-layout.md`.

This module is standalone (its own `go.mod`, module name `snapshotd`) and
lives as a sibling directory to `shotcut/` (the Qt/C++ fork) and `sap-rust/`
(the Rust SAP protocol layer, developed independently). Nothing in
`snapshotd/` depends on either of those actually existing or being built at
*build* time -- `sap-rust` is only referenced by a configurable binary path
(`SNAPSHOT_BIN_PATH`, or auto-discovered, see below) that is resolved at
`Launch` time. In this checkout `sap-rust` **is** built
(`sap-rust/target/debug/sap-rust`), and the tests/examples below run against
the real binary, not just a fixture.

## Architecture

```
cmd/snapshotd/main.go     CLI: serve / status / stop / launch / install
internal/config           Config struct + env-var driven defaults, incl.
                           auto-discovery of a sibling sap-rust build
internal/registry         GORM models (Project, ProcessInstance, AuditEvent)
                           + startup reconciliation sweep
internal/health            PID-liveness (Unix signal-0) + Unix-socket-connect
                           health check primitives, shared by registry+procmgr
internal/session           SessionStore interface + in-memory (map+mutex+TTL)
                           implementation -- v1 default, Redis is NOT built
internal/procmgr           Process manager: Launch/List/Health/Close for the
                           real sap-rust child processes
internal/sapproxy          Generic, opaque SAP proxy: a Content-Length-framed
                           JSON-RPC 2.0 client matching sap-rust's own wire
                           format, a per-project connection pool (one SAP
                           connection per project, shared by every session
                           bound to it), and notification fan-out
internal/sdp               Hand-rolled JSON-RPC 2.0 over a Unix control
                           socket (newline-delimited framing) -- the "SDP
                           server" from 06's corrected diagram. Routes
                           "daemon.*" to internal/daemon.Dispatch and every
                           other method through internal/sapproxy
internal/daemon            The daemon core: wires registry+session+procmgr+
                           sapproxy together, implements the daemon.*
                           primitives and the generic ForwardSAP proxy entry
                           point, used by both the SDP server and the MCP
                           adapter
internal/mcpadapter         MCP access-point adapter (mark3labs/mcp-go),
                           served over SSE + Streamable HTTP (/mcp),
                           translating MCP tool calls into the same
                           daemon.*/ForwardSAP calls
internal/acpxmgr            Optional long-lived acpx-server child under
                           `serve`: writes ACPX_CONFIG_FILE with the live
                           snapshotd MCP URL, spawns, health-polls, stops
                           on SIGTERM (see docs/acpx-bundled-gateway.md)
```

This mirrors 06-daemon-mcp-proxy.md's corrected picture: the daemon core
(`internal/daemon.Daemon`) has no idea what MCP or SDP-over-a-socket are --
both `internal/sdp` and `internal/mcpadapter` are thin, swappable
translation layers on top of the same `Dispatch`/`ForwardSAP` entry points.

### Two wire protocols, on purpose

snapshotd's own control socket (`internal/sdp`, `~/.snapshotd/control.sock`
by default) speaks **newline-delimited** JSON-RPC 2.0 to *its* clients (the
CLI, raw SDP clients, the MCP adapter). sap-rust speaks **Content-Length**
("LSP-style") framed JSON-RPC 2.0 to *its* clients (see
`sap-rust/src/framing.rs`). `internal/sapproxy` is the seam where the daemon,
acting as a SAP client itself, switches wire formats -- it is not a bug that
these two layers use different framing; it is what the real
`sap-rust/src/framing.rs` file actually specifies, and this package's own
`framing.go` mirrors it byte-for-byte.

### The SAP proxy (`internal/sapproxy`) and typed MCP tools

Neither `internal/sapproxy` nor the rest of snapshotd has any compiled-in
knowledge of sap-rust's method surface (`project.*`, `edit.*`, `playlist.*`,
`filter.*`, `transitions.*`, `generator.*`, `file.*`, `jobs.*`,
`playback.*`, `subtitles.*`, ...). The flow:

1. A session (an SDP raw client connection, or one MCP/SSE client) calls
   `project.select` with `{"projectId": ...}`.
2. `internal/daemon.Daemon.ForwardSAP` records that binding and calls
   `sapproxy.Router.Bind`, which opens (or reuses) **one pooled SAP
   connection per project** -- never one per session -- performing
   `sap.hello` (with that instance's per-launch token, persisted on the
   `ProcessInstance` row by `internal/procmgr`) and the real
   `project.select` on it, and returns sap-rust's real result verbatim.
3. Every subsequent non-`"daemon."`-prefixed call from that session is
   forwarded byte-for-byte (method + opaque params) over that same pooled
   connection; the raw result or error comes back unchanged.
4. Every notification sap-rust broadcasts on that connection (`edit.changed`,
   `project.dirty`, ...) is fanned out to *every* session currently bound to
   that project, over whichever transport that session is using: an async
   `sdp.Notification` frame interleaved on the raw socket, or an MCP
   `"sap.notification"` notification pushed over SSE
   (`server.SendNotificationToSpecificClient`).

On the raw SDP side this is still one opaque `method`/`params` passthrough
(`internal/sapproxy.Router.Call` never inspects either). On the MCP side,
`internal/mcpadapter` wraps that same passthrough behind one individually
named, strictly-schema'd tool per sap-rust method (`edit.addTrack`,
`playlist.append`, `filter.setProperty`, ... -- see `tools_*.go`), plus
three Go-side session-lifecycle tools that don't exist on the wire at all:
`project_open` (wraps `project.select`), `project_close` (wraps
`project.exit`), and `project_current` (reads `internal/daemon.Daemon`'s
session store directly, no SAP round trip). None of the ~70 sap-rust-derived
tools take a `projectId` argument -- `Router.Call` already resolves each
session's bound project internally, so a session calls `project_open` once
and every subsequent tool call acts on that binding. See gap #9 below for
how this replaced the earlier single generic `sap.call` tool.

## Driver choice: `github.com/glebarez/sqlite`

07-daemon-persistence.md's proposed schema says "GORM's driver-swap is the
whole point" -- `sqlite.Open(...)` for local/single-node. The obvious
candidate, `gorm.io/driver/sqlite` (backed by `mattn/go-sqlite3`), requires
CGO and therefore a C compiler on the host. `github.com/glebarez/sqlite` is a
drop-in GORM dialector backed by `modernc.org/sqlite`, a pure-Go SQLite
implementation -- no CGO, no gcc, builds in any sandbox with only the Go
toolchain. That's why it was picked here; if a future deployment needs
`mattn/go-sqlite3`'s (marginally faster) native implementation and CGO is
available, swapping the one `gorm.Open(...)` call in
`internal/registry/registry.go` is the entire migration.

## What's simplified vs. the design docs (read this before assuming a gap is a bug)

1. **Health-check-by-polling, not push self-registration.** 08's shim-
   mediated design has each Snapshot process push-register itself
   (`daemon.registerInstance`) and send periodic app-level heartbeats over a
   *separate* control-socket connection, with a `snapshot-shim` process
   giving an independent OS-level exit signal decoupled from the daemon's
   own lifetime. None of the shim or push-registration protocol is
   implemented. Instead: `internal/procmgr.Manager.Launch` spawns the real
   `sap-rust` binary directly as snapshotd's own OS child (`exec.Command`)
   and poll-connects to its Unix socket path (`health.SocketResponsive`,
   with a configurable timeout -- `Manager.ConnectTimeout`, 5s by default,
   since sap-rust's async runtime needs a moment to bind) to confirm it's
   listening before marking the instance `ready`. `internal/health` and
   `internal/registry.Reconciler`'s startup sweep both check "is the socket
   accepting connections", not a real `project.getState`/heartbeat RPC --
   this happily distinguishes crashed/never-started from running, but per
   08's own two-liveness-signal table it would **not** catch a hung-but-
   still-accepting child. A real app-level heartbeat (once sap-rust
   implements one) is the documented follow-up, not implemented here.
2. **In-memory session store, not Redis.** `internal/session.Store` is
   defined as an interface (Create/Touch/Lookup/BindProject/Expire/List/
   Close) exactly so a Redis-backed implementation is an *additive* swap
   later, per 07-daemon-persistence.md caveat 1. Only `internal/session.Memory`
   (map + mutex + TTL, lazy expiry on lookup/touch plus a periodic background
   sweep) is implemented. This is fine for a single-instance deployment
   (the realistic v1 default) and does not support the multi-instance
   coordination story 07 sketches (`DaemonInstanceID`, ownership leases) --
   that column exists in the schema but nothing actually uses it yet. Note:
   this store also now backs `ForwardSAP`'s session bookkeeping
   (`sessionID -> boundProjectID`) -- a Redis swap here needs no changes
   anywhere else.
3. **SQLite only.** No MySQL driver wiring, no `golang-migrate`/`atlas` --
   `AutoMigrate` runs on every `daemon.New`. 07's caveat 2 (AutoMigrate isn't
   a real migration story for production) is unresolved here, same as the
   docs leave it.
4. **No `snapshot-shim` process.** The daemon exec's the child directly; a
   `snapshotd` crash mid-flight would leave orphaned children whose exit the
   daemon can't `wait()` on cleanly (the code does still reap in a
   background goroutine per launch to avoid zombies while the daemon itself
   is alive). The reconciliation sweep at next startup is what recovers from
   this, per 07's sequence -- there is no independent OS-level exit signal
   distinct from the socket-connect check.
5. **`daemon.launch` accepts either `projectId` or `projectPath`.** 06's
   original primitive signature was `launch(projectPath string)`; 07/09
   later moved to a registry keyed by `Project.ID`. Both are supported:
   `projectId` is the normal path once a project is registered (e.g. via
   `daemon.createProject`); `projectPath` is the CLI convenience
   (`snapshotd launch <projectPath>`, per 08's CLI table) that resolves an
   existing `Project` row by `RootDir` or registers a new one on the fly,
   following 09-project-folder-layout.md's two root-resolution rules (a
   directory is the root itself; a bare `.mlt` file's parent directory is
   the root). **Headless defaults to `true`** (`SNAPSHOT_HEADLESS=1`) for
   both paths when the caller doesn't specify it, per 08's "GUI-disabled
   launch mode" being the default for daemon-launched instances; pass an
   explicit `"headless": false` (or `snapshotd launch --gui <path>` on the
   CLI) to opt into a GUI-visible launch.
6. **No control-socket auth.** The control socket is Unix-domain and
   filesystem-permission-scoped (matches `docker.sock`'s own default
   posture per 08's analogy), but there's no additional token/capability
   check on top of that in this build.
7. **`snapshotd install` is an honest stub.** It prints what a real
   implementation would do (write a systemd unit / launchd plist / Windows
   Service) and exits 0 -- it does not touch the host's actual service
   manager. This is deliberate for this sandboxed build, not an oversight.
8. **`snapshotd stop` uses a pidfile + SIGTERM, not an SDP `daemon.stop`
   method.** 06's primitives table has no `daemon.stop` entry; `serve` writes
   `<control-socket-path>.pid` on startup and removes it on clean shutdown,
   and `stop` reads that file and sends SIGTERM. It still verifies a daemon
   is actually reachable (dials the control socket first) before doing so,
   matching the "CLI never silently no-ops if nothing is running" rule from
   09's summary table. Startup ownership is separate: `serve` also holds an
   OS-level exclusive lock at `<SNAPSHOTD_HOME>/daemon.lock`, so a second
   daemon using the same home fails immediately with "already running".
   Kernel lock ownership is released automatically if the daemon crashes;
   the file is only metadata and is removed on clean shutdown.
9. **MCP exposes ~82 individually typed tools, not a generic `sap.call`
   passthrough, and tool listing is not deferred/lazy.** An earlier build of
   this package took the opposite tradeoff -- registering 7 `daemon.*` tools
   plus one generic `sap.call` tool (`{method: string, params: object}`)
   that forwarded opaquely through `internal/sapproxy`, so every current and
   future sap-rust method was callable without per-method Go code, at the
   cost of no per-method schema/validation/description over MCP (an agent
   had to already know sap-rust's method names and param shapes itself).
   That tradeoff was reversed: every sap-rust method now gets its own named
   MCP tool with a real JSON Schema (`tools_*.go`), and
   `server.NewMCPServer` is constructed with `WithInputSchemaValidation`,
   `WithStrictInputSchemaDefault`, and `WithOutputSchemaValidation`
   (`mcp-go` v0.56.0's server-side schema enforcement, both directions), so
   a malformed call is rejected by `mcp-go` itself -- unknown top-level
   argument name, missing required field, out-of-enum value -- before
   `Handler.ForwardSAP` is ever invoked, on top of sap-rust's own
   `serde`/`INVALID_PARAMS` validation on the wire. `mcp-go` still has no
   built-in "deferred/lazily-searchable" tool-listing primitive (only
   `server.WithToolFilter` and `server.WithToolCapabilities(listChanged)`),
   so all ~82 tools are listed eagerly. A later pass adding real deferred/
   lazy tool listing (once mcp-go supports it, or via a custom search-based
   tool like this sandbox's own `tool_search`) is out of scope here and
   noted as a real, honest gap.

## Toolchain note

`github.com/mark3labs/mcp-go` (even the earliest available minor versions)
requires Go >= 1.23; the latest (`v0.56.0`, what this module pulled) requires
Go >= 1.25.5, which bumped this module's `go.mod` `go` directive accordingly.
If your installed `go` binary is older, the Go toolchain's own auto-switch
mechanism (`GOTOOLCHAIN=auto`, the default since Go 1.21) transparently
downloads and uses the 1.25.5 toolchain the first time you build/test this
module, and caches it in your module cache -- no manual action needed,
provided network access is available. This is standard Go behavior, not a
workaround.

## Build / test / run

```sh
cd snapshotd
go build ./...           # build everything
go vet ./...              # static checks
go test ./...             # unit + integration tests (see below)

# The *_realsaprust_test.go / phase_b_/phase_c_ tests exercise a real
# sap-rust binary end to end and shell out to real `melt`/`ffmpeg`/
# `ffprobe` -- all three must be on PATH for those tests to run instead of
# skip (all built/available in this checkout: melt at ~/.local/bin/melt,
# ffmpeg/ffprobe under /usr/bin). No display server is actually required --
# daemon-launched instances get SNAPSHOT_HEADLESS=1 and melt itself is a
# headless CLI encoder -- but if your environment's PATH doesn't already
# include melt's install location, export it explicitly, e.g.:
#   PATH="$HOME/.local/bin:$PATH" go test ./...

# Build the CLI binary:
go build -o snapshotd ./cmd/snapshotd

# Run the daemon (foreground). SNAPSHOT_BIN_PATH is optional: if unset,
# snapshotd auto-discovers a sibling sap-rust build (release preferred over
# debug) by walking up from both the current working directory and the
# running binary's own location -- in this checkout that finds
# sap-rust/target/debug/sap-rust automatically, so this "just works" run
# from either the repo root or from inside snapshotd/:
./snapshotd serve
# ...or override explicitly:
SNAPSHOT_BIN_PATH=/path/to/sap-rust ./snapshotd serve

# In another terminal, once serve is running:
./snapshotd status
./snapshotd launch /path/to/some/project/folder       # headless by default
./snapshotd launch --gui /path/to/some/project/folder  # opt into a GUI launch
./snapshotd stop
./snapshotd install   # prints what it *would* do; does not touch the host
```

Daemon state defaults to `~/.snapshotd/` (override with the `SNAPSHOTD_HOME`
env var): `registry.db` (SQLite), `control.sock` (+ `control.sock.pid`),
`run/` (per-instance SAP sockets), `projects/` (daemon-created project
folders). `SNAPSHOT_BIN_PATH` overrides the auto-discovered child binary
location. `SNAPSHOTD_MCP_SSE_ADDR` overrides the MCP SSE listen address
(default `127.0.0.1:7777`).

The SAP proxy is exercised over MCP via the typed per-method tools, e.g.
(pseudocode for an MCP client):

```json
{"name": "project_open", "arguments": {"projectId": "<id>"}}
{"name": "edit.addTrack", "arguments": {"kind": "video"}}
{"name": "edit.listTracks", "arguments": {}}
```

and over raw SDP by connecting to the control socket and sending the same
`method`/`params` shape (no `"daemon."` prefix) as a newline-delimited
JSON-RPC 2.0 request.

### Test coverage notes

- `internal/registry`: 4 reconciliation tests against a real temp-file
  SQLite DB, covering PID-alive+socket-responsive (stays ready), PID-dead
  (marked crashed), PID-alive+socket-unresponsive (marked crashed), and
  crashed-row-gets-relaunched-when-a-Relaunch-func-is-provided.
- `internal/session`: create/lookup/touch/bind, lazy expiry, background
  sweep expiry, explicit `Expire`, and `List`.
- `internal/procmgr`: builds a small throwaway fixture Go binary on the fly
  (`testdata/fixture`, a bare Unix-socket listener standing in for
  `sap-rust`) and exercises `Launch`'s real env-var wiring
  (`SNAPSHOT_SAP_SOCKET`/`SNAPSHOT_SAP_TOKEN`/`SNAPSHOT_HEADLESS`), spawn,
  and health-check-by-connecting logic end to end, plus a clean-error test
  for a missing binary and a `Close` test.
- `internal/sapproxy`: a self-contained fake Content-Length-framed SAP
  server (hello/select/mutate/notify) proves `Router.Bind`/`Call`/`Unbind`
  in isolation -- one pooled connection shared across two sessions bound to
  the same project, opaque method forwarding, notification fan-out to both
  sessions, and clean errors for a bad token or an unbound session.
- `internal/sdp`: a round-trip test of the real Unix-socket JSON-RPC 2.0
  server/client against a fake handler for `daemon.*` methods (success,
  handler error, unknown method), plus a second test proving non-`daemon.`
  methods route to `ForwardSAP`, that a `*sapproxy.RPCError`'s code survives
  the round trip, and that an async notification frame is correctly
  interleaved with ordinary responses on the same connection.
- `internal/daemon`: an integration test running the full
  create-project -> launch -> list -> health -> close -> delete lifecycle
  against a fixture binary, a test that `Dispatch` correctly routes all 7
  `daemon.*` methods, and **`TestForwardSAP_RealSapRust_EndToEnd`**, which
  launches the *real* `sap-rust` binary (built in this checkout) and drives
  `project.select` + `edit.addTrack` + `edit.listTracks` through
  `ForwardSAP`, asserting real (mutated) `MockBackend` state and real
  cross-session notification fan-out -- skipped, not failed, if
  `sap-rust/target/{release,debug}/sap-rust` isn't built.
- `internal/mcpadapter`: spins up a real `httptest` SSE MCP server backed by
  a fake handler, connects a real `mcp-go` SSE client, lists tools (asserts
  the exact 76-tool typed surface with the audio.* namespace disabled, per
  `TestMCPAdapter_ToolsListedAndCallable`), calls tools (success and
  handler-error cases), exercises the `project_open`/`project_close`/
  `project_current` session-lifecycle tools and an ordinary typed tool's
  opaque forwarding/error surfacing against the fake handler
  (`TestMCPAdapter_ProjectLifecycleAndTypedToolForwarding`), and proves
  `WithInputSchemaValidation`/`WithStrictInputSchemaDefault` actually reject
  bad arguments -- unknown top-level field, missing required field,
  out-of-enum value -- before `ForwardSAP` is ever called
  (`TestMCPAdapter_StrictSchemaRejectsBadArguments`). **
  `TestMCPAdapter_SapCallTool_RealSapRust_EndToEnd`** repeats the forwarding
  proof against the real `sap-rust` binary end to end: a real MCP/SSE
  client calls `project_open` then `edit.addTrack` then `edit.listTracks`,
  asserting real mutated backend state, and asserts the resulting
  `edit.changed` notification is delivered back over the live SSE
  connection as a real `"sap.notification"` MCP notification (via
  `client.OnNotification`). Also skipped, not failed, if sap-rust isn't
  built.
- **`internal/mcpadapter/phase_b_concurrency_test.go` /
  `phase_c_isolation_test.go`** implement `11-e2e-scenario-tests.md`'s
  Phase B and Phase C against the real `sap-rust` binary (real `MltBackend`,
  real `melt` exports) -- two real, independent MCP/SSE client connections
  driving the full daemon -> `sapproxy` -> sap-rust proxy path, not a
  simulation. Also skipped, not failed, if `sap-rust`/`ffmpeg`/`ffprobe`
  aren't available.
  - **Phase B** (`TestMCPAdapter_PhaseB_SameProjectConcurrency`): one
    project, one launched sap-rust process, two MCP sessions both
    `project.select`-ed into it. Proves: (1) a mutation one agent requests
    (`edit.addTrack`) is fanned out to the *other* agent's live SSE stream
    as a real `sap.notification`, even though it never asked for it; (2)
    last-write-wins on a shared resource -- since sap-rust has no
    `filter.setProperty` yet, this uses `edit.trimClipIn` called twice on
    the same clip (agent 2 writes, agent 1 immediately writes again) and
    reads back via `edit.listClips` to confirm the *second* write's value
    persisted, not corrupted state; (3) the shared `undoDepth`/`redoDepth`
    counters in `project.getState` -- agent 1 calls `project.undo`, agent 2
    (who never called undo) observes the new value via its own
    `project.getState`, proving project state is shared across sessions.
    Honesty note carried over from `mlt_backend.rs`'s own doc comment:
    `project.undo` is a plain depth counter, not real timeline rewind --
    this test proves the counter is *shared*, not that full undo/redo
    semantics exist; (4) a `file.export` job started by agent 1 is visible
    to agent 2 via `jobs.get` (sap-rust has no `jobs.list` yet), proving job
    visibility is project-scoped, not session-scoped.
  - **Phase C** (`TestMCPAdapter_PhaseC_DifferentProjectsIsolation`): two
    real projects, two real launched sap-rust processes (distinct PIDs and
    sockets), two MCP sessions each bound to a different one. Proves: (1)
    referencing a real `clipId` that only exists in the *other* project's
    process (via `filter.add`) fails with a clean `NotFound`-style SAP
    error, not a hang or crash -- sap-rust has no `file.import` yet, so this
    uses the cross-project-clip-reference fallback the task named
    explicitly; (2) an agent bound to project A receives *zero*
    notifications when the other agent mutates project B, over a real
    2-second wait window; (3) two concurrent `file.export` calls (real
    `melt` subprocesses) against two different real projects both complete
    independently and produce distinct, correct output files (verified with
    real `ffprobe`, distinct durations from distinct source clips).

Not covered by automated tests (verified manually instead, see below): the
`cmd/snapshotd` CLI's `serve`/`status`/`stop`/`launch`/`install` subcommands
end to end against a real running daemon process and the real `sap-rust`
binary.

### Manual CLI verification performed (against the real sap-rust binary)

```sh
go build -o /tmp/snapshotd-smoke ./cmd/snapshotd
SNAPSHOTD_HOME=/tmp/snapshotd-smoke-home /tmp/snapshotd-smoke serve &
# -> logs: "SDP control socket listening" + "MCP SSE endpoint listening"

SNAPSHOTD_HOME=/tmp/snapshotd-smoke-home \
  /tmp/snapshotd-smoke launch /tmp/snapshotd-demo-project
# -> {"ID": "...", "PID": <real sap-rust pid>, "Status": "ready",
#     "Token": "<random>", "SocketPath": ".../run/<id>.sock", ...}

# Confirmed via /proc/<pid>/cmdline and /proc/<pid>/environ that the spawned
# process is the real .../sap-rust/target/debug/sap-rust binary (not the
# test fixture) with SNAPSHOT_HEADLESS=1 set by default.

SNAPSHOTD_HOME=/tmp/snapshotd-smoke-home /tmp/snapshotd-smoke status
# -> 1 project, 1 ready process instance (matches the launch output)

SNAPSHOTD_HOME=/tmp/snapshotd-smoke-home /tmp/snapshotd-smoke stop
# -> "sent SIGTERM to snapshotd (pid ...)"; daemon exits cleanly

SNAPSHOTD_HOME=/tmp/snapshotd-smoke-home /tmp/snapshotd-smoke status
# -> clean "could not connect to daemon control socket" error (no daemon
#    running) -- the spawned sap-rust child process itself is left running
#    (by design, see simplification #4: children survive a daemon restart
#    and are picked back up by the reconciliation sweep) and was reaped
#    manually for this smoke test.
```
