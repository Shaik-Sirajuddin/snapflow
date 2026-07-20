package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// generatorSubtitlesTools builds the 2 generator.* producer tools and the 6
// subtitles.* tools.
func generatorSubtitlesTools(s *server.MCPServer, h Handler) []server.ServerTool {
	return []server.ServerTool{
		sapTool(s, h, "generator.createTitle", "generator.createTitle", "Create a title producer for the project playlist. Requires text or html.",
			mcp.WithString("mode", mcp.Enum("simple"), mcp.DefaultString("simple"), mcp.Description("Title generator mode")),
			mcp.WithString("text", mcp.Description("Plain-text title content (text or html is required)")),
			mcp.WithString("html", mcp.Description("Rich-text/HTML title content (text or html is required)")),
			mcp.WithString("fgColour", mcp.Description("Foreground text color")),
			mcp.WithString("bgColour", mcp.Description("Background color")),
			mcp.WithOutputSchema[PlaylistEntry](),
		),
		sapTool(s, h, "generator.createColor", "generator.createColor", "Create a solid-color producer for the project playlist, e.g. a transparent spacer.",
			mcp.WithString("hexColor", mcp.Required(), mcp.Description(`Hex color, e.g. "#00000000" for transparent`)),
			mcp.WithOutputSchema[PlaylistEntry](),
		),
		sapTool(s, h, "subtitles.addTrack", "subtitles.addTrack", "Add a subtitles track.",
			mcp.WithOutputSchema[SubtitleTrackInfo](),
		),
		sapTool(s, h, "subtitles.appendItem", "subtitles.appendItem", "Append a subtitle cue to a subtitles track.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Subtitles track index")),
			mcp.WithInteger("startFrame", mcp.Required(), mcp.Description("Cue start frame")),
			mcp.WithInteger("endFrame", mcp.Required(), mcp.Description("Cue end frame")),
			mcp.WithString("text", mcp.Required(), mcp.Description("Cue text")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "subtitles.removeItems", "subtitles.removeItems", "Remove subtitle cues by 0-based index.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Subtitles track index")),
			mcp.WithArray("itemIndices", mcp.Required(), mcp.WithIntegerItems(), mcp.Description("0-based subtitle cue indices to remove")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "subtitles.importSrt", "subtitles.importSrt", "Import an SRT file into track 0, or a new track.",
			mcp.WithString("path", mcp.Required(), mcp.Description("Filesystem path to the SRT file")),
			mcp.WithBoolean("newTrack", mcp.DefaultBool(false), mcp.Description("Import into a newly created track instead of track 0")),
			mcp.WithOutputSchema[SubtitleTrackInfo](),
		),
		sapTool(s, h, "subtitles.exportSrt", "subtitles.exportSrt", "Export a subtitles track to an SRT file.",
			mcp.WithString("path", mcp.Required(), mcp.Description("Filesystem path to write the SRT file")),
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Subtitles track index")),
			mcp.WithOutputSchema[ExportSrtResult](),
		),
		sapTool(s, h, "subtitles.burnIn", "subtitles.burnIn", "Burn a subtitles track's cues into exported/previewed frames (idempotent per track).",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Subtitles track index")),
			mcp.WithOutputSchema[BurnInResult](),
		),
	}
}
