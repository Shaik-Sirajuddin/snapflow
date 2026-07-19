package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// editTools builds the 15 edit.* timeline-editing tools: track and clip
// mutation/listing on the current session's bound project.
func editTools(s *server.MCPServer, h Handler) []server.ServerTool {
	sourceOpt := clipSourceOpt()

	return []server.ServerTool{
		sapTool(s, h, "edit.addTrack", "edit.addTrack", "Add a video or audio timeline track.",
			mcp.WithString("kind", mcp.Required(), mcp.Enum("video", "audio"), mcp.Description("Track kind")),
			mcp.WithOutputSchema[Track](),
		),
		sapTool(s, h, "edit.removeTrack", "edit.removeTrack", "Remove a timeline track.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Track index")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "edit.listTracks", "edit.listTracks", "List timeline tracks.",
			mcp.WithOutputSchema[TrackList](),
		),
		sapTool(s, h, "edit.reorderTrack", "edit.reorderTrack", "Move a timeline track from one index to another, carrying its clips with it.",
			mcp.WithInteger("fromIndex", mcp.Required(), mcp.Description("Current track index")),
			mcp.WithInteger("toIndex", mcp.Required(), mcp.Description("Destination track index")),
			mcp.WithOutputSchema[TrackList](),
		),
		sapTool(s, h, "edit.setTrackProperties", "edit.setTrackProperties", "Partially update a track's muted/hidden/locked/blendMode; omitted fields are left unchanged.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Track index")),
			mcp.WithBoolean("muted", mcp.Description("Mute the track")),
			mcp.WithBoolean("hidden", mcp.Description("Hide the track")),
			mcp.WithBoolean("locked", mcp.Description("Lock the track")),
			mcp.WithString("blendMode", mcp.Description("MLT blend mode identifier")),
			mcp.WithOutputSchema[Track](),
		),
		sapTool(s, h, "edit.setTrackHeight", "edit.setTrackHeight", "Set the project-wide timeline row height (applies to all tracks).",
			mcp.WithInteger("height", mcp.Required(), mcp.Description("Row height in pixels")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "edit.removeClip", "edit.removeClip", "Remove a clip from a timeline track.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Track index")),
			mcp.WithInteger("clipIndex", mcp.Required(), mcp.Description("Clip index on the track")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "edit.moveClip", "edit.moveClip", "Move/reposition a clip within a track or across tracks.",
			mcp.WithInteger("fromTrackIndex", mcp.Required(), mcp.Description("Source track index")),
			mcp.WithInteger("fromClipIndex", mcp.Required(), mcp.Description("Source clip index")),
			mcp.WithInteger("toTrackIndex", mcp.Required(), mcp.Description("Destination track index")),
			mcp.WithInteger("toClipIndex", mcp.Required(), mcp.Description("Destination clip index")),
			mcp.WithOutputSchema[Clip](),
		),
		sapTool(s, h, "edit.appendClip", "edit.appendClip", "Append a source clip to a timeline track.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Track index")),
			sourceOpt,
			mcp.WithOutputSchema[Clip](),
		),
		sapTool(s, h, "edit.insertClip", "edit.insertClip", "Insert a source clip before clip-slot clipIndex, rippling downstream clips forward.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Track index")),
			mcp.WithInteger("clipIndex", mcp.Required(), mcp.Description("Clip slot to insert before")),
			sourceOpt,
			mcp.WithOutputSchema[Clip](),
		),
		sapTool(s, h, "edit.overwriteClip", "edit.overwriteClip", "Replace whatever occupies clip-slot clipIndex with a source clip; does not ripple downstream clips.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Track index")),
			mcp.WithInteger("clipIndex", mcp.Required(), mcp.Description("Clip slot to overwrite")),
			sourceOpt,
			mcp.WithOutputSchema[Clip](),
		),
		sapTool(s, h, "edit.listClips", "edit.listClips", "List clips on a timeline track.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Track index")),
			mcp.WithOutputSchema[ClipList](),
		),
		sapTool(s, h, "edit.trimClipIn", "edit.trimClipIn", "Trim a clip's in point; ripple shifts downstream clips to close/open the gap.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Track index")),
			mcp.WithInteger("clipIndex", mcp.Required(), mcp.Description("Clip index on the track")),
			mcp.WithInteger("newFrame", mcp.Required(), mcp.Description("New in-point frame")),
			mcp.WithBoolean("ripple", mcp.DefaultBool(false), mcp.Description("Shift downstream clips instead of leaving a blank")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "edit.trimClipOut", "edit.trimClipOut", "Trim a clip's out point; ripple shifts downstream clips to close/open the gap.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Track index")),
			mcp.WithInteger("clipIndex", mcp.Required(), mcp.Description("Clip index on the track")),
			mcp.WithInteger("newFrame", mcp.Required(), mcp.Description("New out-point frame")),
			mcp.WithBoolean("ripple", mcp.DefaultBool(false), mcp.Description("Shift downstream clips instead of leaving a blank")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "edit.splitClip", "edit.splitClip", "Split a clip at a source frame into two adjacent clips.",
			mcp.WithInteger("trackIndex", mcp.Required(), mcp.Description("Track index")),
			mcp.WithInteger("clipIndex", mcp.Required(), mcp.Description("Clip index on the track")),
			mcp.WithInteger("position", mcp.Required(), mcp.Description("Source frame to split at")),
			mcp.WithOutputSchema[SplitClipResult](),
		),
	}
}

// clipSourceOpt builds the "source" clip-descriptor property shared by
// edit.appendClip/insertClip/overwriteClip and playlist.append/insert:
// exactly one of "path" (a filesystem path) or "playlistIndex" (an existing
// playlist entry) per resolve_clip_source in sap-rust/src/ffi_backend.rs.
// Nested properties/additionalProperties are declared explicitly (not left
// as a bare mcp.WithObject) so mcp-go's schema validation actually checks
// this object's contents -- an untyped WithObject has no "properties"/
// "additionalProperties" in its JSON Schema, so jsonschema/v6 accepts any
// shape inside it, silently defeating WithStrictInputSchemaDefault for this
// field.
func clipSourceOpt() mcp.ToolOption {
	return mcp.WithObject("source", mcp.Required(),
		mcp.Description(`Clip source descriptor: exactly one of {"path": "..."} or {"playlistIndex": N}`),
		mcp.Properties(map[string]any{
			"path": map[string]any{
				"type":        "string",
				"description": "Filesystem path to a media file",
			},
			"playlistIndex": map[string]any{
				"type":        "integer",
				"description": "Index of an existing project playlist entry",
			},
		}),
		mcp.AdditionalProperties(false),
	)
}
