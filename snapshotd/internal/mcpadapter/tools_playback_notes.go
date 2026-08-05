package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// playbackNotesTools builds playback.* transport/inspect tools and notes.*.
func playbackNotesTools(s *server.MCPServer, h Handler) []server.ServerTool {
	return []server.ServerTool{
		sapTool(s, h, "playback.seek", "playback.seek", "Seek the current project's playhead.",
			mcp.WithInteger("frame", mcp.Required(), mcp.Description("Frame to seek to")),
			dynamicOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "playback.play", "playback.play", "Start timeline playback (same as the editor Play button).",
			mcp.WithNumber("speed", mcp.Description("Playback speed; default 1.0")),
			dynamicOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "playback.pause", "playback.pause", "Pause timeline playback.",
			mcp.WithInteger("position", mcp.Description("Optional frame to pause at; omit to keep current")),
			dynamicOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "playback.stop", "playback.stop", "Stop timeline playback and reset transport UI state.",
			dynamicOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "playback.getState", "playback.getState", "Read transport state: playing, position, duration.",
			dynamicOutputSchema[PlaybackState](),
		),
		sapTool(s, h, "playback.getFrame", "playback.getFrame", "Read a rendered frame from the current project as base64 image data.",
			mcp.WithInteger("frame", mcp.Required(), mcp.Description("Frame to render")),
			mcp.WithString("format", mcp.DefaultString("jpeg"), mcp.Description("Image format")),
			dynamicOutputSchema[FrameDataResult](),
		),
		sapTool(s, h, "notes.getText", "notes.getText", "Read the current project's notes.",
			dynamicOutputSchema[TextResult](),
		),
		sapTool(s, h, "notes.setText", "notes.setText", "Replace the current project's notes.",
			mcp.WithString("text", mcp.Required(), mcp.Description("New notes text")),
			dynamicOutputSchema[EmptyResult](),
		),
	}
}
