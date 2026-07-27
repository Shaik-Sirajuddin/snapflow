package mcpadapter

import (
	"context"
	"encoding/json"

	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"

	"snapshotd/internal/sapproxy"
)

// projectTools builds the project.* surface (path-first follow-on):
//
//	Primary: project.open / project.close / project.create / project.list /
//	         project.clone / getState/save/undo/redo
//	Deprecated wrappers: project.select / project.exit (same ForwardSAP
//	methods as open/close)
//
// Open/init/close behavior lives in daemon.ForwardSAP (Launch-or-reuse +
// Bind; Unbind). create/list/clone use Handler.Dispatch for registry work;
// create with open:true chains open+save in this package.
func projectTools(s *server.MCPServer, h Handler) []server.ServerTool {
	return []server.ServerTool{
		// --- primary agent-facing names ---
		sapTool(s, h, "project.open", "project.select",
			"Open or attach this session to a project. Accepts projectId and/or path (filesystem folder or .mlt). Launch-or-reuses the child process, loads saved .mlt on first open (safe no-op attach for later sessions). Preferred over project.select.",
			mcp.WithString("projectId", mcp.Description("Project ID (from project.create / project.list)")),
			mcp.WithString("path", mcp.Description("Filesystem path to a project folder or .mlt file (preferred addressing)")),
			dynamicOutputSchema[ProjectState](),
		),
		sapTool(s, h, "project.close", "project.exit",
			"Close this session's binding to its project (session-scoped unbind only). Other sessions and the running process are unaffected. Preferred over project.exit.",
			dynamicOutputSchema[EmptyResult](),
		),
		projectCreateTool(s, h),
		tool("project.list",
			"List known projects. Each row leads with path and projectType; projectId is secondary.",
			nil,
			h),
		projectCloneTool(s, h),
		// --- deprecated aliases (same behavior) ---
		sapTool(s, h, "project.select", "project.select",
			"[Deprecated: use project.open] Bind this MCP session to a project. Accepts projectId and/or path.",
			mcp.WithString("projectId", mcp.Description("Project ID")),
			mcp.WithString("path", mcp.Description("Filesystem path to project folder or .mlt")),
			dynamicOutputSchema[ProjectState](),
		),
		sapTool(s, h, "project.exit", "project.exit",
			"[Deprecated: use project.close] Unbind this MCP session from its project.",
			dynamicOutputSchema[EmptyResult](),
		),
		// --- state / history ---
		sapTool(s, h, "project.getState", "project.getState",
			"Read the bound project's state, including undo/redo stack depths and projectType.",
			dynamicOutputSchema[ProjectState](),
		),
		sapTool(s, h, "project.save", "project.save",
			"Persist the bound project's current in-memory session to its project.mlt file on disk.",
			dynamicOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "project.undo", "project.undo",
			"Undo the bound project's latest edit.",
			dynamicOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "project.redo", "project.redo",
			"Redo the bound project's latest undone edit.",
			dynamicOutputSchema[EmptyResult](),
		),
	}
}

// projectCreateTool handles project.create, optionally chaining open+save
// when open:true so a real .mlt is materialized via saveXML (no hand-written XML).
func projectCreateTool(s *server.MCPServer, h Handler) server.ServerTool {
	return server.ServerTool{
		Tool: mcp.NewTool("project.create",
			mcp.WithDescription("Create a project at an arbitrary path (or under the daemon projects root via name). When open:true, opens the project and saves once so a real .mlt exists on disk. projectType defaults to folder."),
			mcp.WithString("path", mcp.Description("Absolute or relative project folder path (preferred)")),
			mcp.WithString("name", mcp.Description("Legacy: folder name under daemon projects root when path is omitted")),
			mcp.WithBoolean("open", mcp.Description("If true, open then project.save so .mlt is written immediately")),
			mcp.WithString("mltFileName", mcp.Description("MLT filename inside the project folder (default project.mlt)")),
			mcp.WithString("projectType", mcp.Description("folder (default) or file")),
		),
		Handler: func(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
			raw, err := json.Marshal(req.GetArguments())
			if err != nil {
				return mcp.NewToolResultErrorFromErr("marshaling arguments", err), nil
			}
			created, err := h.Dispatch(ctx, "project.create", raw)
			if err != nil {
				return mcp.NewToolResultError(err.Error()), nil
			}
			var args struct {
				Open *bool `json:"open"`
			}
			_ = json.Unmarshal(raw, &args)
			wantOpen := args.Open != nil && *args.Open

			createdRaw, _ := json.Marshal(created)
			var createdObj struct {
				Project struct {
					ID string `json:"id"`
				} `json:"project"`
				MltCreated bool `json:"mltCreated"`
			}
			_ = json.Unmarshal(createdRaw, &createdObj)
			projectID := createdObj.Project.ID
			if projectID == "" {
				// Bare Project shape
				var bare struct {
					ID string `json:"id"`
				}
				_ = json.Unmarshal(createdRaw, &bare)
				projectID = bare.ID
			}

			out := map[string]any{}
			_ = json.Unmarshal(createdRaw, &out)
			if _, ok := out["project"]; !ok {
				out = map[string]any{"project": created, "mltCreated": false}
			}

			if !wantOpen || projectID == "" {
				resultRaw, _ := json.Marshal(out)
				return mcp.NewToolResultJSON(wrapArrayResult(resultRaw))
			}

			cs := server.ClientSessionFromContext(ctx)
			if cs == nil {
				return mcp.NewToolResultError("project.create: open:true requires MCP session"), nil
			}
			sink := &mcpSink{server: s, sessionID: cs.SessionID()}
			selectParams, _ := json.Marshal(map[string]string{"projectId": projectID})
			state, err := forwardSAP(h, ctx, cs.SessionID(), sink, "project.select", selectParams)
			if err != nil {
				return mcp.NewToolResultError("project.create open: " + err.Error()), nil
			}
			if _, err := forwardSAP(h, ctx, cs.SessionID(), sink, "project.save", json.RawMessage(`{}`)); err != nil {
				return mcp.NewToolResultError("project.create save: " + err.Error()), nil
			}
			out["mltCreated"] = true
			var stateObj any
			_ = json.Unmarshal(state, &stateObj)
			out["projectState"] = stateObj
			resultRaw, _ := json.Marshal(out)
			return mcp.NewToolResultJSON(wrapArrayResult(resultRaw))
		},
	}
}

// projectCloneTool copies a project tree; optional open attaches this session.
func projectCloneTool(s *server.MCPServer, h Handler) server.ServerTool {
	return server.ServerTool{
		Tool: mcp.NewTool("project.clone",
			mcp.WithDescription("Clone a project by recursively copying its folder to destPath and registering a new project. Optional open attaches this session to the clone."),
			mcp.WithString("sourcePath", mcp.Required(), mcp.Description("Source project folder or .mlt path")),
			mcp.WithString("destPath", mcp.Required(), mcp.Description("Destination directory (must not exist)")),
			mcp.WithBoolean("open", mcp.Description("If true, open the clone after copy (default false)")),
		),
		Handler: func(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
			raw, err := json.Marshal(req.GetArguments())
			if err != nil {
				return mcp.NewToolResultErrorFromErr("marshaling arguments", err), nil
			}
			cloned, err := h.Dispatch(ctx, "project.clone", raw)
			if err != nil {
				return mcp.NewToolResultError(err.Error()), nil
			}
			var args struct {
				Open *bool `json:"open"`
			}
			_ = json.Unmarshal(raw, &args)
			clonedRaw, _ := json.Marshal(cloned)
			var proj struct {
				ID string `json:"id"`
			}
			_ = json.Unmarshal(clonedRaw, &proj)
			out := map[string]any{"project": cloned}
			if args.Open != nil && *args.Open && proj.ID != "" {
				cs := server.ClientSessionFromContext(ctx)
				if cs == nil {
					return mcp.NewToolResultError("project.clone: open:true requires MCP session"), nil
				}
				sink := &mcpSink{server: s, sessionID: cs.SessionID()}
				selectParams, _ := json.Marshal(map[string]string{"projectId": proj.ID})
				state, err := forwardSAP(h, ctx, cs.SessionID(), sink, "project.select", selectParams)
				if err != nil {
					return mcp.NewToolResultError("project.clone open: " + err.Error()), nil
				}
				var stateObj any
				_ = json.Unmarshal(state, &stateObj)
				out["projectState"] = stateObj
			}
			resultRaw, _ := json.Marshal(out)
			return mcp.NewToolResultJSON(wrapArrayResult(resultRaw))
		},
	}
}

// Ensure sapproxy import used (sink type).
var _ sapproxy.Sink = (*mcpSink)(nil)
