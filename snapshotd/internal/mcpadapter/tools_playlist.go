package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// playlistTools builds the 7 playlist.* tools plus the 1
// transitions.addCrossfade tool -- grouped together since
// playlist.addToTimeline and transitions.addCrossfade both operate at the
// timeline/playlist boundary.
func playlistTools(s *server.MCPServer, h Handler) []server.ServerTool {
	sourceOpt := clipSourceOpt()

	return []server.ServerTool{
		sapTool(s, h, "playlist.append", "playlist.append", "Append a source to the project playlist.",
			sourceOpt,
			mcp.WithString("name", mcp.Description("Display name for the playlist entry")),
			mcp.WithOutputSchema[PlaylistEntry](),
		),
		sapTool(s, h, "playlist.list", "playlist.list", "List project playlist entries.",
			mcp.WithOutputSchema[PlaylistEntryList](),
		),
		sapTool(s, h, "playlist.insert", "playlist.insert", "Insert a source into the project playlist at an index.",
			mcp.WithInteger("index", mcp.Required(), mcp.Description("Playlist index to insert at")),
			sourceOpt,
			mcp.WithString("name", mcp.Description("Display name for the playlist entry")),
			mcp.WithOutputSchema[PlaylistEntry](),
		),
		sapTool(s, h, "playlist.remove", "playlist.remove", "Remove a playlist entry by index.",
			mcp.WithInteger("index", mcp.Required(), mcp.Description("Playlist index to remove")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "playlist.move", "playlist.move", "Move a playlist entry from one index to another.",
			mcp.WithInteger("fromIndex", mcp.Required(), mcp.Description("Current playlist index")),
			mcp.WithInteger("toIndex", mcp.Required(), mcp.Description("Destination playlist index")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "playlist.get", "playlist.get", "Get full metadata (including probe data where available) for one playlist entry.",
			mcp.WithInteger("index", mcp.Required(), mcp.Description("Playlist index")),
			mcp.WithOutputSchema[PlaylistEntryDetail](),
		),
		sapTool(s, h, "playlist.addToTimeline", "playlist.addToTimeline", "Append a playlist entry onto a timeline track (equivalent to edit.appendClip by playlistIndex).",
			mcp.WithInteger("index", mcp.Required(), mcp.Description("Playlist index to place")),
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Destination track index")),
			mcp.WithInteger("position", mcp.Description("Reserved for wire-shape parity; currently always appends at the end of the track")),
			mcp.WithOutputSchema[Clip](),
		),
		sapTool(s, h, "transitions.addCrossfade", "transitions.addCrossfade", "Add a crossfade transition between two adjacent clips on a track.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Track index")),
			mcp.WithArray("betweenClips", mcp.Required(), mcp.MinItems(2), mcp.MaxItems(2), mcp.WithIntegerItems(),
				mcp.Description("The two adjacent clip indices to cross-fade between, e.g. [0, 1]")),
			mcp.WithInteger("durationFrames", mcp.Required(), mcp.Description("Crossfade duration in frames")),
			mcp.WithOutputSchema[TransitionInfo](),
		),
	}
}
