package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// generatorSubtitlesTools builds the 2 generator.* producer tools and the 7
// subtitles.* tools.
func generatorSubtitlesTools(s *server.MCPServer, h Handler) []server.ServerTool {
	return []server.ServerTool{
		sapTool(s, h, "generator.createTitle", "generator.createTitle", "Create a title producer for the project playlist. Requires text or html.",
			mcp.WithString("mode", mcp.Enum("simple"), mcp.DefaultString("simple"), mcp.Description("Title generator mode")),
			mcp.WithString("text", mcp.Description("Plain-text title content (text or html is required)")),
			mcp.WithString("html", mcp.Description("Rich-text/HTML title content (text or html is required)")),
			mcp.WithString("fgColour", mcp.Description("Foreground text color")),
			mcp.WithString("bgColour", mcp.Description("Background color")),
			dynamicOutputSchema[PlaylistEntry](),
		),
		sapTool(s, h, "generator.createColor", "generator.createColor", "Create a solid-color producer for the project playlist, e.g. a transparent spacer.",
			mcp.WithString("hexColor", mcp.Required(), mcp.Description(`Hex color, alpha always LAST on input: "#RGB", "#RRGGBB", "#RRGGBBAA", "0xRRGGBB" or "0xRRGGBBAA" (case-insensitive; alpha defaults to ff/opaque when omitted). Examples: "#ff0000" opaque red, "#00000000" fully transparent. The value is canonicalized to MLT's internal #AARRGGBB form for you, so callers never need to account for MLT parsing "#" as alpha-first and "0x" as alpha-last. Anything else is rejected.`)),
			dynamicOutputSchema[PlaylistEntry](),
		),
		sapTool(s, h, "subtitles.addTrack", "subtitles.addTrack", "Add a subtitles track.",
			dynamicOutputSchema[SubtitleTrackInfo](),
		),
		sapTool(s, h, "subtitles.appendItem", "subtitles.appendItem", "Append a subtitle cue to a subtitles track.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Subtitles track index")),
			mcp.WithInteger("startFrame", mcp.Required(), mcp.Description("Cue start frame")),
			mcp.WithInteger("endFrame", mcp.Required(), mcp.Description("Cue end frame")),
			mcp.WithString("text", mcp.Required(), mcp.Description("Cue text")),
			dynamicOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "subtitles.removeItems", "subtitles.removeItems", "Remove subtitle cues by 0-based index.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Subtitles track index")),
			mcp.WithArray("itemIndices", mcp.Required(), mcp.WithIntegerItems(), mcp.Description("0-based subtitle cue indices to remove")),
			dynamicOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "subtitles.importSrt", "subtitles.importSrt", "Import an SRT file into track 0, or a new track.",
			mcp.WithString("path", mcp.Required(), mcp.Description("Filesystem path to the SRT file")),
			mcp.WithBoolean("newTrack", mcp.DefaultBool(false), mcp.Description("Import into a newly created track instead of track 0")),
			dynamicOutputSchema[SubtitleTrackInfo](),
		),
		sapTool(s, h, "subtitles.exportSrt", "subtitles.exportSrt", "Export a subtitles track to an SRT file.",
			mcp.WithString("path", mcp.Required(), mcp.Description("Filesystem path to write the SRT file")),
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Subtitles track index")),
			dynamicOutputSchema[ExportSrtResult](),
		),
		sapTool(s, h, "subtitles.burnIn", "subtitles.burnIn", "Burn a subtitles track's cues into exported/previewed frames (idempotent per track).",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Subtitles track index")),
			dynamicOutputSchema[BurnInResult](),
		),
		sapTool(s, h, "subtitles.setStyle", "subtitles.setStyle", "Style subtitle filters attached to the output tractor.",
			mcp.WithString("fgcolour", mcp.Description("Foreground color, e.g. #ffffffff")),
			mcp.WithString("bgcolour", mcp.Description("Background color, e.g. #00000000")),
			mcp.WithString("olcolour", mcp.Description("Outline color")),
			mcp.WithInteger("outline", mcp.Description("Outline width")),
			mcp.WithInteger("weight", mcp.Description("Font weight")),
			mcp.WithString("style", mcp.Enum("normal", "italic"), mcp.Description("Font style")),
			mcp.WithInteger("size", mcp.Description("Font size in points")),
			mcp.WithString("geometry", mcp.Description("MLT geometry string")),
			mcp.WithString("valign", mcp.Enum("top", "middle", "bottom"), mcp.Description("Vertical alignment")),
			mcp.WithString("halign", mcp.Enum("left", "center", "right"), mcp.Description("Horizontal alignment")),
			dynamicOutputSchema[EmptyResult](),
		),
	}
}
