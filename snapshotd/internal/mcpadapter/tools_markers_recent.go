package mcpadapter

import (
	"encoding/json"

	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// markersRecentTools builds the 10 markers.* timeline-marker tools and the
// 3 recent.* project-scoped recent-files-list tools.
func markersRecentTools(s *server.MCPServer, h Handler) []server.ServerTool {
	return []server.ServerTool{
		sapTool(s, h, "markers.append", "markers.append", "Append a timeline marker.",
			mcp.WithInteger("frame", mcp.Required(), mcp.Description("Marker frame")),
			mcp.WithString("text", mcp.Description("Marker text")),
			mcp.WithString("color", mcp.Description("Marker color")),
			mcp.WithOutputSchema[Marker](),
		),
		sapTool(s, h, "markers.remove", "markers.remove", "Remove a timeline marker by index.",
			mcp.WithInteger("markerIndex", mcp.Required(), mcp.Description("Marker index")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "markers.update", "markers.update", "Update a timeline marker's frame/text/color.",
			mcp.WithInteger("markerIndex", mcp.Required(), mcp.Description("Marker index")),
			mcp.WithInteger("frame", mcp.Description("New marker frame")),
			mcp.WithString("text", mcp.Description("New marker text")),
			mcp.WithString("color", mcp.Description("New marker color")),
			mcp.WithOutputSchema[Marker](),
		),
		sapTool(s, h, "markers.move", "markers.move", "Move a timeline marker to a new frame range.",
			mcp.WithInteger("markerIndex", mcp.Required(), mcp.Description("Marker index")),
			mcp.WithInteger("start", mcp.Required(), mcp.Description("New range start frame")),
			mcp.WithInteger("end", mcp.Required(), mcp.Description("New range end frame")),
			mcp.WithOutputSchema[Marker](),
		),
		sapTool(s, h, "markers.setColor", "markers.setColor", "Set a timeline marker's color.",
			mcp.WithInteger("markerIndex", mcp.Required(), mcp.Description("Marker index")),
			mcp.WithString("color", mcp.Required(), mcp.Description("New marker color")),
			mcp.WithOutputSchema[Marker](),
		),
		sapTool(s, h, "markers.clear", "markers.clear", "Clear all timeline markers.",
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "markers.list", "markers.list", "List all timeline markers.",
			mcp.WithOutputSchema[MarkerList](),
		),
		sapTool(s, h, "markers.get", "markers.get", "Get one timeline marker by index.",
			mcp.WithInteger("markerIndex", mcp.Required(), mcp.Description("Marker index")),
			mcp.WithOutputSchema[Marker](),
		),
		sapTool(s, h, "markers.next", "markers.next", "Find the next marker frame after fromFrame, or null if none.",
			mcp.WithInteger("fromFrame", mcp.Required(), mcp.Description("Frame to search after")),
			mcp.WithRawOutputSchema(json.RawMessage(nullableFrameOutputSchema)),
		),
		sapTool(s, h, "markers.prev", "markers.prev", "Find the previous marker frame before fromFrame, or null if none.",
			mcp.WithInteger("fromFrame", mcp.Required(), mcp.Description("Frame to search before")),
			mcp.WithRawOutputSchema(json.RawMessage(nullableFrameOutputSchema)),
		),
		sapTool(s, h, "recent.add", "recent.add", "Add a path to the project-scoped recent-files list.",
			mcp.WithString("path", mcp.Required(), mcp.Description("Filesystem path")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "recent.remove", "recent.remove", "Remove a path from the recent-files list.",
			mcp.WithString("path", mcp.Required(), mcp.Description("Filesystem path")),
			mcp.WithOutputSchema[PathResult](),
		),
		sapTool(s, h, "recent.list", "recent.list", "List recent paths, newest first.",
			mcp.WithOutputSchema[StringList](),
		),
	}
}
