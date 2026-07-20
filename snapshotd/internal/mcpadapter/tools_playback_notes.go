package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// playbackNotesTools builds the 2 playback.* tools and the 2 notes.* tools.
func playbackNotesTools(s *server.MCPServer, h Handler) []server.ServerTool {
	return []server.ServerTool{
		sapTool(s, h, "playback.seek", "playback.seek", "Seek the current project's playhead.",
			mcp.WithInteger("frame", mcp.Required(), mcp.Description("Frame to seek to")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "playback.getFrame", "playback.getFrame", "Read a rendered frame from the current project as base64 image data.",
			mcp.WithInteger("frame", mcp.Required(), mcp.Description("Frame to render")),
			mcp.WithString("format", mcp.DefaultString("jpeg"), mcp.Description("Image format")),
			mcp.WithOutputSchema[FrameDataResult](),
		),
		sapTool(s, h, "notes.getText", "notes.getText", "Read the current project's notes.",
			mcp.WithOutputSchema[TextResult](),
		),
		sapTool(s, h, "notes.setText", "notes.setText", "Replace the current project's notes.",
			mcp.WithString("text", mcp.Required(), mcp.Description("New notes text")),
			mcp.WithOutputSchema[EmptyResult](),
		),
	}
}
