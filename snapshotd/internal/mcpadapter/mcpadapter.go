// Package mcpadapter is the MCP access-point adapter described in
// 06-daemon-mcp-proxy.md's correction: it translates MCP tool calls into
// calls against the same daemon core (internal/daemon.Daemon) used by the
// SDP JSON-RPC server (internal/sdp) -- it holds no state of its own beyond
// the mcp-go server/transport plumbing.
//
// Transport: SSE, served by default, per 08-lifecycle-and-cli.md's "SSE MCP
// enabled by default" decision -- `snapshotd serve` starts this listener
// automatically, no flag required.
//
// Deferred/lazy tool listing: 10-testing-plan.md's Phase 2 calls for tools
// to be "deferred/lazily-searchable" rather than eagerly dumped into an
// agent's context, matching this very environment's own ToolSearch pattern.
// mark3labs/mcp-go v0.56.0 (the version pulled by this module) does not
// offer an equivalent first-class mechanism -- it has WithToolFilter
// (per-session visibility/access control) and WithToolCapabilities(listChanged)
// (list-changed-notification support), but no built-in "register tools as
// lazily-searchable, full schemas fetched on demand" primitive. Given that,
// this adapter registers all 7 daemon.* tools normally via AddTools; a real
// deferred-listing gap only matters once the daemon-side proxy grows to the
// full ~70+ method project/edit/playback/etc. surface mentioned in
// 01-jsonrpc-spec.md, which is out of scope for this package. See
// snapshotd/README.md for this noted as an explicit, honest gap rather than
// something invented to paper over it.
//
// Generic SAP passthrough: rather than trying to keep up with sap-rust's
// growing project.*/edit.*/playlist.*/filter.*/transitions.*/generator.*/
// file.*/jobs.*/playback.*/subtitles.* surface (01-jsonrpc-spec.md) as
// individually typed MCP tools, this adapter exposes exactly one additional
// tool, "sap.call", that forwards method+params opaquely to
// Handler.ForwardSAP (internal/daemon.Daemon.ForwardSAP ->
// internal/sapproxy.Router), the same generic proxy internal/sdp uses for
// raw clients. This makes every current and future sap-rust method callable
// over MCP today without this package needing to know its schema. See
// snapshotd/README.md for the tradeoffs (no per-method typed schema/
// validation/description over MCP -- only over the generic tool's own
// method/params shape) and for what a later, fuller typed-tool-surface pass
// would look like.
package mcpadapter

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"

	"snapshotd/internal/sapproxy"
)

// serverInstructions is the MCP server's top-level `instructions` field: a
// short, self-contained usage summary shown to a client connecting cold, so
// it doesn't need any out-of-band documentation to get productive. Kept free
// of absolute filesystem paths and internal plan-doc filenames on purpose --
// this text must stay meaningful for any deployment, not just this repo's
// own checkout.
const serverInstructions = "Video/media editing MCP server. Typical flow: " +
	"(1) use the daemon.* tools (createProject, listProjects, launch, list, health, close) " +
	"to manage project folders and start a project's editing process; " +
	"(2) call sap.call with method=\"project.select\" and params={\"projectId\": ...} to bind " +
	"this session to one project (required before any other project-scoped call); " +
	"(3) call sap.search, optionally with a text query, to discover the live SAP method " +
	"surface for that project -- project.*, edit.*, playlist.*, filter.*, transitions.*, " +
	"generator.*, file.*, jobs.*, playback.*, subtitles.*, and audio.* when that namespace is " +
	"enabled; (4) call sap.call with any discovered method name and matching params to perform " +
	"the edit. A session may only be bound to one project at a time: call sap.call with " +
	"method=\"project.exit\" before selecting a different project, or project.select will be " +
	"rejected with an already-bound error."

// Handler is the subset of internal/daemon.Daemon this adapter depends on --
// kept as a small interface for the same reason internal/sdp.Handler is: the
// adapter should not need to import daemon-internal types beyond what it
// actually calls, and it makes the tool handlers straightforwardly testable
// with a fake.
type Handler interface {
	// Dispatch handles the "daemon."-prefixed control-plane primitives.
	Dispatch(ctx context.Context, method string, params json.RawMessage) (any, error)

	// ForwardSAP handles the generic "sap.call" tool below: project.select
	// binds this MCP session (via sink) to a project's pooled SAP
	// connection; every other method/params pair is forwarded opaquely.
	ForwardSAP(ctx context.Context, sessionID string, sink sapproxy.Sink, method string, params json.RawMessage) (json.RawMessage, error)

	// UnbindSession releases a session's SAP project binding/notification
	// sink -- wired to mcp-go's OnUnregisterSession hook below.
	UnbindSession(sessionID string)
}

// New constructs an MCP server exposing the daemon.* SDP methods as tools,
// per 06's primitives table.
func New(h Handler) *server.MCPServer {
	audioEnabled := false
	if capability, ok := h.(interface{ AudioNamespaceEnabled() bool }); ok {
		audioEnabled = capability.AudioNamespaceEnabled()
	}
	hooks := &server.Hooks{}
	s := server.NewMCPServer(
		"snapshotd",
		"0.1.0",
		server.WithToolCapabilities(false),
		server.WithHooks(hooks),
		server.WithInstructions(serverInstructions),
	)
	// Mirrors internal/sdp.Server's own connection-close cleanup: whenever
	// an MCP client session ends (SSE stream closes, etc), release whatever
	// SAP project binding/notification sink it held, per 06's fan-out
	// requirement not leaking stale sinks onto a still-live pooled
	// connection.
	hooks.AddOnUnregisterSession(func(ctx context.Context, session server.ClientSession) {
		h.UnbindSession(session.SessionID())
	})

	s.AddTools(
		tool("daemon.createProject",
			"Create a new project folder under the daemon's projects root and register it in the registry.",
			mcp.WithString("name", mcp.Required(), mcp.Description("Project folder name to create")),
			h),
		tool("daemon.deleteProject",
			"Delete a project's registry row (does not delete files on disk).",
			mcp.WithString("projectId", mcp.Required(), mcp.Description("Project ID to delete")),
			h),
		tool("daemon.listProjects",
			"List all known projects.",
			nil,
			h),
		tool("daemon.launch",
			"Launch (spawn) a Snapshot child process for a project.",
			mcp.WithString("projectId", mcp.Description("Project ID to launch (use this or projectPath)")),
			h,
			mcp.WithString("projectPath", mcp.Description("Filesystem path to a project folder or legacy .mlt file (use this or projectId)")),
			mcp.WithBoolean("headless", mcp.Description("Launch headless (offscreen), no GUI display needed")),
		),
		tool("daemon.list",
			"List known process instances (running and previously running).",
			nil,
			h),
		tool("daemon.health",
			"Check whether a process instance's SAP socket is responsive.",
			mcp.WithString("instanceId", mcp.Required(), mcp.Description("Process instance ID")),
			h),
		tool("daemon.close",
			"Stop a running process instance.",
			mcp.WithString("instanceId", mcp.Required(), mcp.Description("Process instance ID")),
			h),
	)

	s.AddTools(sapCallTool(s, h))
	s.AddTools(sapSearchTool(audioEnabled))

	return s
}

type sapMethodMetadata struct {
	Method      string `json:"method"`
	Description string `json:"description"`
	Params      string `json:"params"`
}

// Keep this list limited to methods implemented by the current SAP server.
// audio.* entries are filtered from results unless the daemon enables that
// namespace.
var supportedSAPMethods = []sapMethodMetadata{
	{Method: "project.getState", Description: "Read the bound project's state.", Params: "{}"},
	{Method: "project.save", Description: "Save the bound project.", Params: "{}"},
	{Method: "project.undo", Description: "Undo the latest bound-project edit.", Params: "{}"},
	{Method: "project.redo", Description: "Redo the latest undone bound-project edit.", Params: "{}"},
	{Method: "edit.addTrack", Description: "Add a video or audio timeline track.", Params: `{kind}`},
	{Method: "edit.removeTrack", Description: "Remove a timeline track.", Params: `{trackIndex}`},
	{Method: "edit.listTracks", Description: "List timeline tracks.", Params: "{}"},
	{Method: "edit.reorderTrack", Description: "Move a timeline track from one index to another, carrying its clips with it.", Params: `{fromIndex, toIndex}`},
	{Method: "edit.setTrackProperties", Description: "Partially update a track's mute/hidden/locked/blendMode; omitted fields are left unchanged.", Params: `{trackIndex, muted?, hidden?, locked?, blendMode?}`},
	{Method: "edit.setTrackHeight", Description: "Set the project-wide timeline row height (applies to all tracks).", Params: `{height}`},
	{Method: "edit.removeClip", Description: "Remove a clip from a timeline track.", Params: `{trackIndex, clipIndex}`},
	{Method: "edit.moveClip", Description: "Move/reposition a clip within a track or across tracks.", Params: `{fromTrackIndex, fromClipIndex, toTrackIndex, toClipIndex}`},
	{Method: "edit.appendClip", Description: "Append a source clip to a timeline track.", Params: `{trackIndex, source}`},
	{Method: "edit.insertClip", Description: "Insert a source clip on a track BEFORE clip-slot clipIndex, rippling downstream clips forward to make room (clipIndex == clip count is append-equivalent).", Params: `{trackIndex, clipIndex, source}`},
	{Method: "edit.overwriteClip", Description: "Place a source clip on a track starting at clip-slot clipIndex, replacing whatever occupies that slot without rippling downstream clips (clipIndex == clip count is append-equivalent).", Params: `{trackIndex, clipIndex, source}`},
	{Method: "edit.listClips", Description: "List clips on a timeline track.", Params: `{trackIndex}`},
	{Method: "edit.trimClipIn", Description: "Trim a clip's in point. ripple (default false) shifts downstream clips on the track to close/open the gap instead of leaving a blank.", Params: `{trackIndex, clipIndex, newFrame, ripple?}`},
	{Method: "edit.trimClipOut", Description: "Trim a clip's out point. ripple (default false) shifts downstream clips on the track to close/open the gap instead of leaving a blank.", Params: `{trackIndex, clipIndex, newFrame, ripple?}`},
	{Method: "edit.splitClip", Description: "Split a clip at a source frame into two adjacent clips.", Params: `{trackIndex, clipIndex, position}`},
	{Method: "playlist.append", Description: "Append a source to the project playlist.", Params: `{source, name?}`},
	{Method: "playlist.list", Description: "List project playlist entries.", Params: "{}"},
	{Method: "playlist.insert", Description: "Insert a source into the project playlist at an index.", Params: `{index, source, name?}`},
	{Method: "playlist.remove", Description: "Remove a playlist entry by index.", Params: `{index}`},
	{Method: "playlist.move", Description: "Move a playlist entry from one index to another.", Params: `{fromIndex, toIndex}`},
	{Method: "playlist.get", Description: "Get full metadata (incl. probe data where available) for one playlist entry.", Params: `{index}`},
	{Method: "playlist.addToTimeline", Description: "Append a playlist entry onto a timeline track (equivalent to edit.appendClip by playlistIndex).", Params: `{index, trackIndex, position?}`},
	{Method: "transitions.addCrossfade", Description: "Add a crossfade between adjacent clips.", Params: `{trackIndex, betweenClips, durationFrames}`},
	{Method: "filter.add", Description: "Attach an MLT filter to a clip.", Params: `{clipId, mltService, properties?}`},
	{Method: "filter.list", Description: "List filters attached to a clip.", Params: `{clipId}`},
	{Method: "filter.remove", Description: "Detach a filter from a clip.", Params: `{clipId, filterIndex}`},
	{Method: "filter.reorder", Description: "Reorder a filter in a clip's filter chain.", Params: `{clipId, filterIndex, newIndex}`},
	{Method: "filter.addKeyframe", Description: "Add a filter-property keyframe.", Params: `{clipId, filterIndex, property, position, value, interpolation?}`},
	{Method: "filter.listKeyframes", Description: "List keyframes for a filter property.", Params: `{clipId, filterIndex, property}`},
	{Method: "filter.removeKeyframe", Description: "Remove a keyframe from a filter property.", Params: `{clipId, filterIndex, property, position}`},
	{Method: "filter.setProperty", Description: "Set a filter property (static or positioned).", Params: `{clipId, filterIndex, property, value, position?}`},
	{Method: "audio.setGain", Description: "Add a volume filter with a gain in dB.", Params: `{clipId, db, position?}`},
	{Method: "audio.setPan", Description: "Add a panner filter (channel 0) with split 0..1.", Params: `{clipId, pan, position?}`},
	{Method: "audio.setBalance", Description: "Add a panner filter (channel -1) for stereo balance 0..1.", Params: `{clipId, balance, position?}`},
	{Method: "audio.setNormalize", Description: "Add one-pass (dynamic_loudness) or two-pass (loudness) normalize.", Params: `{clipId, mode: "1pass"|"2pass", targetLevel?}`},
	{Method: "audio.setFadeInOut", Description: "Add volume filter(s) with keyframed fade-in/out level envelopes.", Params: `{clipId, fadeInFrames?, fadeOutFrames?}`},
	{Method: "audio.setAutoFade", Description: "Enable/disable autofade (fade_duration 500ms; disable removes autofade filters).", Params: `{clipId, enabled}`},

	{Method: "generator.createTitle", Description: "Create a title producer for the project playlist.", Params: `{mode, text|html, ...}`},
	{Method: "generator.createColor", Description: "Create a color producer for the project playlist (e.g. #00000000 for a transparent spacer).", Params: `{hexColor}`},
	{Method: "subtitles.addTrack", Description: "Add a subtitles track.", Params: "{}"},
	{Method: "subtitles.appendItem", Description: "Append a subtitle item.", Params: `{trackIndex, startFrame, endFrame, text}`},
	{Method: "subtitles.removeItems", Description: "Remove subtitle cues by 0-based indices.", Params: `{trackIndex, itemIndices}`},
	{Method: "subtitles.importSrt", Description: "Import an SRT file into track 0 (or a new track).", Params: `{path, newTrack?}`},
	{Method: "subtitles.exportSrt", Description: "Export a subtitle track to an SRT file.", Params: `{path, trackIndex}`},
	{Method: "subtitles.burnIn", Description: "Attach a burn-in filter on the timeline output so a subtitle track's cues render into exported/previewed frames (idempotent per track).", Params: `{trackIndex}`},
	{Method: "file.import", Description: "Import a media file inside the bound project's root.", Params: `{path}`},
	{Method: "file.probe", Description: "Probe a media file without project binding.", Params: `{path}`},
	{Method: "file.export", Description: "Start an export job for the bound project.", Params: `{outputPath, codec?, container?}`},
	{Method: "jobs.list", Description: "List export jobs for the bound project.", Params: "{}"},
	{Method: "jobs.get", Description: "Read an export job's status.", Params: `{jobId}`},
	{Method: "jobs.stop", Description: "Stop a running export job.", Params: `{jobId}`},
	{Method: "playback.seek", Description: "Seek the bound project's playhead.", Params: `{frame}`},
	{Method: "playback.getFrame", Description: "Read a rendered frame from the bound project.", Params: `{frame, format?}`},
	{Method: "notes.getText", Description: "Read the bound project's notes.", Params: "{}"},
	{Method: "notes.setText", Description: "Replace the bound project's notes.", Params: `{text}`},
	{Method: "markers.append", Description: "Append a timeline marker.", Params: `{frame, text?, color?}`},
	{Method: "markers.remove", Description: "Remove a timeline marker by index.", Params: `{markerIndex}`},
	{Method: "markers.update", Description: "Update a timeline marker.", Params: `{markerIndex, frame?, text?, color?}`},
	{Method: "markers.move", Description: "Move a timeline marker to a frame range.", Params: `{markerIndex, start, end}`},
	{Method: "markers.setColor", Description: "Set a timeline marker's color.", Params: `{markerIndex, color}`},
	{Method: "markers.clear", Description: "Clear all timeline markers.", Params: "{}"},
	{Method: "markers.list", Description: "List all timeline markers.", Params: "{}"},
	{Method: "markers.get", Description: "Get one timeline marker by index.", Params: `{markerIndex}`},
	{Method: "markers.next", Description: "Next marker frame after fromFrame, or null.", Params: `{fromFrame}`},
	{Method: "markers.prev", Description: "Previous marker frame before fromFrame, or null.", Params: `{fromFrame}`},
	{Method: "recent.add", Description: "Add a path to the project-scoped recent list.", Params: `{path}`},
	{Method: "recent.remove", Description: "Remove a path from the recent list.", Params: `{path}`},
	{Method: "recent.list", Description: "List recent paths (newest first).", Params: "{}"},
}

func sapSearchTool(audioEnabled bool) server.ServerTool {
	return server.ServerTool{
		Tool: mcp.NewTool("sap.search",
			mcp.WithDescription("Search supported SAP methods and return concise metadata for use with sap.call."),
			mcp.WithString("query", mcp.Description("Case-insensitive text to match against method names and descriptions; empty returns all supported methods.")),
		),
		Handler: func(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
			query, _ := req.GetArguments()["query"].(string)
			query = strings.ToLower(strings.TrimSpace(query))
			matches := make([]sapMethodMetadata, 0, len(supportedSAPMethods))
			for _, method := range supportedSAPMethods {
				if !audioEnabled && strings.HasPrefix(method.Method, "audio.") {
					continue
				}
				if query == "" ||
					strings.Contains(strings.ToLower(method.Method), query) ||
					strings.Contains(strings.ToLower(method.Description), query) {
					matches = append(matches, method)
				}
			}
			raw, err := json.Marshal(matches)
			if err != nil {
				return mcp.NewToolResultErrorFromErr("marshaling result", err), nil
			}
			return mcp.NewToolResultJSON(wrapArrayResult(raw))
		},
	}
}

// sapCallTool builds the "sap.call" generic passthrough tool described in
// the package doc comment above.
func sapCallTool(s *server.MCPServer, h Handler) server.ServerTool {
	return server.ServerTool{
		Tool: mcp.NewTool("sap.call",
			mcp.WithDescription(
				"Generic passthrough to the project's live sap-rust process. "+
					"Call with method=\"project.select\" and params={\"projectId\": ...} first "+
					"to bind this MCP session to a project (opens or reuses one pooled SAP "+
					"connection per project). Every other opaque SAP method -- project.*, "+
					"edit.*, playlist.*, filter.*, transitions.*, generator.*, file.*, jobs.*, "+
					"playback.*, subtitles.*, ... -- is then forwarded verbatim to sap-rust and "+
					"its raw result (or error) is returned unchanged. This tool exists because "+
					"mark3labs/mcp-go v0.56.0 has no deferred/lazy tool-listing primitive to "+
					"expose sap-rust's full method surface as individually typed MCP tools -- "+
					"see snapshotd/README.md.",
			),
			mcp.WithString("method", mcp.Required(),
				mcp.Description(`SAP JSON-RPC method name, e.g. "project.select", "edit.addTrack", "playlist.append"`)),
			mcp.WithObject("params",
				mcp.Description("Method params, forwarded verbatim as the SAP call's params object (schema depends entirely on `method`; opaque to snapshotd)")),
		),
		Handler: func(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
			args := req.GetArguments()
			method, _ := args["method"].(string)
			if method == "" {
				return mcp.NewToolResultError(`sap.call: "method" is required`), nil
			}

			var paramsRaw json.RawMessage
			if p, ok := args["params"]; ok && p != nil {
				raw, err := json.Marshal(p)
				if err != nil {
					return mcp.NewToolResultErrorFromErr("marshaling params", err), nil
				}
				paramsRaw = raw
			}

			cs := server.ClientSessionFromContext(ctx)
			if cs == nil {
				return mcp.NewToolResultError("sap.call: no MCP client session in context"), nil
			}
			sink := &mcpSink{server: s, sessionID: cs.SessionID()}

			result, err := h.ForwardSAP(ctx, cs.SessionID(), sink, method, paramsRaw)
			if err != nil {
				return mcp.NewToolResultError(err.Error()), nil
			}
			if len(result) == 0 {
				return mcp.NewToolResultText(fmt.Sprintf("%s: ok", method)), nil
			}
			return mcp.NewToolResultJSON(wrapArrayResult(result))
		},
	}
}

// sapTool builds one typed MCP tool for a single fixed sap-rust method,
// used by the tools_*.go files (audio/filter/generator/subtitles/...) for
// the typed-tool-surface pass this package's own doc comment describes as
// a future possibility beyond the generic sap.call passthrough above.
// Forwarding logic mirrors sapCallTool exactly (same project.select-bound
// ForwardSAP path, same mcpSink notification relay) except the SAP method
// is fixed at registration time instead of read from the call's own
// arguments, and the call's arguments are marshaled verbatim as that
// method's params (every params object here already matches its sap-rust
// method's expected shape 1:1, same convention `tool` above documents for
// daemon.* methods). mcpName and sapMethod are passed separately (even
// though every current call site sets them equal) so this stays usable if
// a tool's public MCP name and its underlying SAP method name ever diverge.
func sapTool(s *server.MCPServer, h Handler, mcpName, sapMethod, description string, opts ...mcp.ToolOption) server.ServerTool {
	toolOpts := append([]mcp.ToolOption{mcp.WithDescription(description)}, opts...)
	return server.ServerTool{
		Tool: mcp.NewTool(mcpName, toolOpts...),
		Handler: func(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
			paramsRaw, err := json.Marshal(req.GetArguments())
			if err != nil {
				return mcp.NewToolResultErrorFromErr("marshaling arguments", err), nil
			}

			cs := server.ClientSessionFromContext(ctx)
			if cs == nil {
				return mcp.NewToolResultError(mcpName + ": no MCP client session in context"), nil
			}
			sink := &mcpSink{server: s, sessionID: cs.SessionID()}

			result, err := h.ForwardSAP(ctx, cs.SessionID(), sink, sapMethod, paramsRaw)
			if err != nil {
				return mcp.NewToolResultError(err.Error()), nil
			}
			if len(result) == 0 {
				return mcp.NewToolResultText(fmt.Sprintf("%s: ok", mcpName)), nil
			}
			return mcp.NewToolResultJSON(wrapArrayResult(result))
		},
	}
}

// mcpSink relays a project's fanned-out SAP notifications (edit.changed,
// project.dirty, etc -- opaque to this package, see internal/sapproxy) to
// one MCP client session over its existing transport (SSE by default), via
// mark3labs/mcp-go's SendNotificationToSpecificClient. The method/params
// pair is wrapped under a stable "sap.notification" MCP notification method
// so clients can recognize these without this package needing to know
// sap-rust's specific method names.
type mcpSink struct {
	server    *server.MCPServer
	sessionID string
}

func (s *mcpSink) Notify(method string, params json.RawMessage) {
	var fields map[string]any
	if len(params) > 0 {
		_ = json.Unmarshal(params, &fields)
	}
	_ = s.server.SendNotificationToSpecificClient(s.sessionID, "sap.notification", map[string]any{
		"method": method,
		"params": fields,
	})
}

// tool builds a ServerTool that forwards to Handler.Dispatch: mcpOpts are
// applied on top of the base name/description, and the handler binds the
// MCP call's arguments straight into JSON and dispatches it as the SDP
// method of the same name, since the on-wire JSON shape of every daemon.*
// method's params already matches its MCP tool's arguments 1:1 (both are
// just JSON objects with the same field names).
// wrapArrayResult ensures a JSON tool result's StructuredContent is an
// object, per the MCP spec (structuredContent must be a JSON object, not a
// bare array) -- several of this package's results are naturally
// top-level arrays (daemon.list, daemon.listProjects, sap.search, and
// whatever array-shaped results sap-rust itself returns through the
// sap.call passthrough). Rather than typing this per call site, wrap any
// top-level JSON array as {"items": [...]} right before handing it to
// mcp.NewToolResultJSON; objects and scalars pass through unchanged.
func wrapArrayResult(raw json.RawMessage) json.RawMessage {
	trimmed := bytes.TrimLeft(raw, " \t\r\n")
	if len(trimmed) == 0 || trimmed[0] != '[' {
		return raw
	}
	wrapped, err := json.Marshal(struct {
		Items json.RawMessage `json:"items"`
	}{Items: raw})
	if err != nil {
		return raw
	}
	return wrapped
}

func tool(name, description string, primaryOpt mcp.ToolOption, h Handler, extraOpts ...mcp.ToolOption) server.ServerTool {
	opts := []mcp.ToolOption{mcp.WithDescription(description)}
	if primaryOpt != nil {
		opts = append(opts, primaryOpt)
	}
	opts = append(opts, extraOpts...)

	return server.ServerTool{
		Tool: mcp.NewTool(name, opts...),
		Handler: func(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
			raw, err := json.Marshal(req.GetArguments())
			if err != nil {
				return mcp.NewToolResultErrorFromErr("marshaling arguments", err), nil
			}
			result, err := h.Dispatch(ctx, name, raw)
			if err != nil {
				return mcp.NewToolResultError(err.Error()), nil
			}
			if result == nil {
				return mcp.NewToolResultText(fmt.Sprintf("%s: ok", name)), nil
			}
			resultRaw, err := json.Marshal(result)
			if err != nil {
				return mcp.NewToolResultErrorFromErr("marshaling result", err), nil
			}
			res, err := mcp.NewToolResultJSON(wrapArrayResult(resultRaw))
			if err != nil {
				return mcp.NewToolResultErrorFromErr("marshaling result", err), nil
			}
			return res, nil
		},
	}
}
