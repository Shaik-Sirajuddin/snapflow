package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// projectTools builds the 6 project.* tools: select/exit (session
// binding) and getState/save/undo/redo (state introspection and history).
// Every other typed tool in this package (edit.*, playlist.*, filter.*,
// etc.) operates on "whichever project this MCP session is currently
// bound to" -- project.select/project.exit are that binding's only entry
// and exit points, and getState/save/undo/redo are real, actively-used
// SAP primitives (this package's own test suite depends on all four).
// Before sap.call/sap.search were dropped from the live tool surface (see
// mcpadapter.go's New() doc comment), all six were reachable only through
// the generic sap.call passthrough; typed here so project binding and
// history control still have a route now that the passthrough is gone.
func projectTools(s *server.MCPServer, h Handler) []server.ServerTool {
	return []server.ServerTool{
		sapTool(s, h, "project.select", "project.select",
			"Bind this MCP session to a project's pooled SAP connection. Required before any other project-scoped tool call. A session may only be bound to one project at a time -- call project.exit first to switch.",
			mcp.WithString("projectId", mcp.Required(), mcp.Description("Project ID to bind this session to")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "project.exit", "project.exit",
			"Unbind this MCP session from its currently selected project.",
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "project.getState", "project.getState",
			"Read the bound project's state, including undo/redo stack depths.",
			mcp.WithOutputSchema[ProjectState](),
		),
		sapTool(s, h, "project.save", "project.save",
			"Persist the bound project's current in-memory session to its project.mlt file on disk. Mutating calls (edit.*, filter.*, etc.) are not written to disk until this is called.",
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "project.undo", "project.undo",
			"Undo the bound project's latest edit (a real Qt QUndoStack operation, shared across every session bound to this project).",
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "project.redo", "project.redo",
			"Redo the bound project's latest undone edit.",
			mcp.WithOutputSchema[EmptyResult](),
		),
	}
}
