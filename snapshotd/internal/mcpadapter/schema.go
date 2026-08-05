package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
)

// AllTools returns the complete typed MCP tool set for schema generation.
// It mirrors the tools registered by New and exists to give
// cmd/gen-mcp-schema a complete, byte-for-byte view of the live schemas.
func AllTools(h Handler) []mcp.Tool {
	s := New(h)
	s.AddTools(editTools(s, h)...)
	s.AddTools(playlistTools(s, h)...)
	s.AddTools(filterTools(s, h)...)
	s.AddTools(audioTools(s, h)...)
	s.AddTools(generatorSubtitlesTools(s, h)...)
	s.AddTools(fileJobsTools(s, h)...)
	s.AddTools(markersRecentTools(s, h)...)
	s.AddTools(playbackNotesTools(s, h)...)

	serverTools := s.ListTools()
	tools := make([]mcp.Tool, 0, len(serverTools))
	for _, st := range serverTools {
		tools = append(tools, st.Tool)
	}
	return tools
}
