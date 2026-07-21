package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// CurrentView mirrors sap-rust's currentView result: the bound project's
// active selection state. clipId is a pointer since no clip may be
// selected (track-only selection) -- a bare string can't represent that
// absence the way an optional field can.
type CurrentView struct {
	TrackIndex int     `json:"trackIndex"`
	ClipID     *string `json:"clipId,omitempty"`
}

// selectionTools builds the 3 selection-state tools: track.enter,
// clip.enter, and currentView (no fixed "n tools per category" comment
// count elsewhere in this package applies neatly here since this trio has
// no dedicated section in the tool-category schema report -- these methods
// were added after that report was generated and only ever reachable
// through the generic sap.call passthrough, see mcpadapter.go's New() doc
// comment for why that passthrough is now gone). Every mutating edit.*/
// filter.* tool that takes an implicit "current selection" (rather than
// an explicit trackIndex/clipId) depends on track.enter/clip.enter having
// been called first; an explicitly-passed trackIndex/clipId is never
// honored as an override over the real selection (see this package's own
// realsaprust end-to-end test for the exact contract).
func selectionTools(s *server.MCPServer, h Handler) []server.ServerTool {
	return []server.ServerTool{
		sapTool(s, h, "track.enter", "track.enter",
			"Select a timeline track as the current selection scope. Mutating tools that omit an explicit trackIndex act on this track; an explicitly-passed trackIndex on those tools is never honored as an override.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Track index to select")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "clip.enter", "clip.enter",
			"Select a clip (within the currently selected track) as the current selection scope, for tools that act on a clip implicitly rather than via an explicit clipId.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID to select")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "currentView", "currentView",
			"Read the bound project's current selection state (selected track/clip). Selection indices stay mapped to the same logical track/clip even after a reorder/remove changes their numeric index.",
			mcp.WithOutputSchema[CurrentView](),
		),
	}
}
