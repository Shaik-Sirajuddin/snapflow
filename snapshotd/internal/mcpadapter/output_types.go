// Response ("output") schema types for the typed sap-rust-derived MCP tool
// surface. Every type here mirrors, field-for-field, either a real
// sap-rust/src/backend.rs response struct (serde `rename_all = "camelCase"`,
// so Go's exported-field-name-defaults-to-camelCase-json-tag matches it
// exactly) or one of server.rs's ad hoc `json!({...})` response shapes for
// methods that don't return a named struct. These are consumed purely via
// mcp.WithOutputSchema[T]() at tool-registration time (see mcpadapter.go
// and tools_*.go) -- github.com/google/jsonschema-go's reflection-based
// `jsonschema.For[T]` (which mcp-go's WithOutputSchema wraps) is what
// actually turns each of these into a JSON Schema: struct fields without
// `omitempty` become required properties, structs get
// `additionalProperties: false`, and `any`/`interface{}` fields become an
// unconstrained ("anything goes") sub-schema -- exactly the fields below
// that correspond to sap-rust's own `serde_json::Value` (deliberately
// dynamic; see e.g. Clip.Source, FilterListEntry.Properties,
// KeyframeInfo.Value) get typed `any` for that reason, matching the same
// "type what's concretely knowable, leave genuinely dynamic shapes open"
// rule already applied to this package's *input* schemas (filter.add's
// `properties`, filter.addKeyframe/setProperty's `value`).
//
// Wrapper *List types exist because wrapArrayResult (mcpadapter.go) always
// re-wraps a top-level JSON array result as {"items": [...]} before handing
// it to mcp.NewToolResultJSON -- the actual on-the-wire structured content
// for e.g. "edit.listTracks" is {"items": [Track, ...]}, not a bare array,
// so that's what the output schema must describe.
package mcpadapter

import "snapshotd/internal/registry"

// ProjectList is the {"items": [...]} wrapper for daemon.listProjects'
// []registry.Project response. registry.Project has no `json` struct
// tags of its own, so its schema (and wire JSON) uses Go's default
// exported-field-name-as-key behavior (e.g. "ID", "RootDir") -- preserved
// here rather than "fixed" to camelCase, since that's the real, existing
// wire shape every daemon.* caller already depends on.
type ProjectList struct {
	Items []registry.Project `json:"items"`
}

// ProcessInstanceList is the {"items": [...]} wrapper for daemon.list's
// []registry.ProcessInstance response -- same no-json-tags caveat as
// ProjectList.
type ProcessInstanceList struct {
	Items []registry.ProcessInstance `json:"items"`
}

// Track mirrors sap-rust's backend::Track.
type Track struct {
	Index     int    `json:"index"`
	Kind      string `json:"kind"`
	Muted     bool   `json:"muted"`
	Hidden    bool   `json:"hidden"`
	Locked    bool   `json:"locked"`
	BlendMode string `json:"blendMode"`
}

// TrackList is the {"items": [...]} wrapper for Vec<Track> responses
// (edit.listTracks, edit.reorderTrack).
type TrackList struct {
	Items []Track `json:"items"`
}

// Clip mirrors sap-rust's backend::Clip. Source is the same tagged-union
// shape as the appendClip/insertClip/overwriteClip request's "source"
// field (deliberately untyped on both the request and response side).
type Clip struct {
	ClipID   string `json:"clipId"`
	Index    int    `json:"index"`
	Source   any    `json:"source"`
	InFrame  int64  `json:"inFrame"`
	OutFrame int64  `json:"outFrame"`
}

// ClipList is the {"items": [...]} wrapper for Vec<Clip> (edit.listClips).
type ClipList struct {
	Items []Clip `json:"items"`
}

// PlaylistEntry mirrors sap-rust's backend::PlaylistEntry.
type PlaylistEntry struct {
	Index          int    `json:"index"`
	Name           string `json:"name"`
	Source         any    `json:"source"`
	DurationFrames int64  `json:"durationFrames"`
}

// PlaylistEntryList is the {"items": [...]} wrapper for Vec<PlaylistEntry>
// (playlist.list).
type PlaylistEntryList struct {
	Items []PlaylistEntry `json:"items"`
}

// PlaylistEntryDetail mirrors sap-rust's backend::PlaylistEntryDetail
// (playlist.get). Probe is omitted from the wire entirely (not just
// null) when unavailable -- serde's `skip_serializing_if = "Option::is_none"`
// -- hence `omitempty` here rather than a plain pointer field.
type PlaylistEntryDetail struct {
	Index          int        `json:"index"`
	Name           string     `json:"name"`
	Source         any        `json:"source"`
	DurationFrames int64      `json:"durationFrames"`
	Probe          *FileProbe `json:"probe,omitempty"`
}

// TransitionInfo mirrors sap-rust's backend::TransitionInfo
// (transitions.addCrossfade). BetweenClips is a fixed 2-tuple on the Rust
// side; modeled as a slice here since this package's JSON Schema generator
// has no fixed-length-array/tuple construct to reach for.
type TransitionInfo struct {
	TrackIndex      int   `json:"trackIndex"`
	TransitionIndex int   `json:"transitionIndex"`
	BetweenClips    []int `json:"betweenClips"`
	DurationFrames  int64 `json:"durationFrames"`
}

// FilterInfo mirrors sap-rust's backend::FilterInfo (filter.add and every
// audio.* convenience method built on top of it).
type FilterInfo struct {
	FilterIndex int    `json:"filterIndex"`
	MltService  string `json:"mltService"`
}

// FilterListEntry mirrors sap-rust's backend::FilterListEntry (filter.list
// entries). Properties is the filter's arbitrary MLT property map --
// deliberately untyped, same reasoning as filter.add's request-side
// "properties" field.
type FilterListEntry struct {
	Index      int    `json:"index"`
	MltService string `json:"mltService"`
	Properties any    `json:"properties"`
}

// FilterListEntryList is the {"items": [...]} wrapper for
// Vec<FilterListEntry> (filter.list).
type FilterListEntryList struct {
	Items []FilterListEntry `json:"items"`
}

// KeyframeInfo mirrors sap-rust's backend::KeyframeInfo
// (filter.listKeyframes entries). Value is the keyframed property's value,
// whose type depends on which filter/property this keyframe belongs to --
// deliberately untyped, same reasoning as filter.addKeyframe/setProperty's
// request-side "value" field.
type KeyframeInfo struct {
	Position      int64  `json:"position"`
	Value         any    `json:"value"`
	Interpolation string `json:"interpolation"`
}

// KeyframeInfoList is the {"items": [...]} wrapper for Vec<KeyframeInfo>
// (filter.listKeyframes).
type KeyframeInfoList struct {
	Items []KeyframeInfo `json:"items"`
}

// SplitClipResult mirrors sap-rust's backend::SplitClipResult
// (edit.splitClip).
type SplitClipResult struct {
	LeftClipID  string `json:"leftClipId"`
	RightClipID string `json:"rightClipId"`
	LeftIndex   int    `json:"leftIndex"`
	RightIndex  int    `json:"rightIndex"`
}

// SubtitleTrackInfo mirrors sap-rust's backend::SubtitleTrackInfo
// (subtitles.addTrack, subtitles.importSrt).
type SubtitleTrackInfo struct {
	TrackIndex int `json:"trackIndex"`
}

// Marker mirrors sap-rust's backend::Marker. EndFrame is
// skip_serializing_if-omitted on the wire when the marker has no range end,
// hence `omitempty` on a pointer here rather than a plain field.
type Marker struct {
	Index    int    `json:"index"`
	Frame    int64  `json:"frame"`
	Text     string `json:"text"`
	Color    string `json:"color"`
	EndFrame *int64 `json:"endFrame,omitempty"`
}

// MarkerList is the {"items": [...]} wrapper for Vec<Marker>
// (markers.list).
type MarkerList struct {
	Items []Marker `json:"items"`
}

// JobStatus mirrors sap-rust's backend::JobStatus (jobs.get/jobs.list
// entries). ResultPath/Error are skip_serializing_if-omitted on the wire
// when absent, hence `omitempty` pointers.
type JobStatus struct {
	JobID      string  `json:"jobId"`
	Status     string  `json:"status"`
	Percent    float64 `json:"percent"`
	ResultPath *string `json:"resultPath,omitempty"`
	Error      *string `json:"error,omitempty"`
}

// JobStatusList is the {"items": [...]} wrapper for Vec<JobStatus>
// (jobs.list).
type JobStatusList struct {
	Items []JobStatus `json:"items"`
}

// FileProbe mirrors sap-rust's backend::FileProbe (file.probe).
type FileProbe struct {
	Path            string  `json:"path"`
	DurationSeconds float64 `json:"durationSeconds"`
	DurationFrames  int64   `json:"durationFrames"`
	Codec           string  `json:"codec"`
}

// ProjectState mirrors sap-rust's backend::ProjectState (project_open/
// project.getState).
type ProjectState struct {
	ProjectID string `json:"projectId"`
	Dirty     bool   `json:"dirty"`
	UndoDepth int    `json:"undoDepth"`
	RedoDepth int    `json:"redoDepth"`
}

// EmptyResult is the output schema for every sap-rust method whose success
// result is the bare `json!({})` sentinel (all `()`-returning Backend trait
// methods: project.save/undo/redo, edit.removeTrack/setTrackHeight/
// removeClip/trimClipIn/trimClipOut, playlist.remove/move,
// filter.setProperty/addKeyframe/remove/reorder/removeKeyframe,
// subtitles.appendItem/removeItems, playback.seek, notes.setText,
// markers.remove/clear, recent.add, and project_close's local
// project.exit handling) -- a strictly empty JSON object, no properties
// allowed.
type EmptyResult struct{}

// TextResult is notes.getText's response shape ({"text": "..."}).
type TextResult struct {
	Text string `json:"text"`
}

// PathResult is recent.remove's response shape ({"path": "..."}) --
// distinct from ExportSrtResult only in field meaning, kept separate so
// each tool's schema documents its own field's intent.
type PathResult struct {
	Path string `json:"path"`
}

// ExportSrtResult is subtitles.exportSrt's response shape
// ({"path": "<resolved path>"}).
type ExportSrtResult struct {
	Path string `json:"path"`
}

// BurnInResult is subtitles.burnIn's response shape
// ({"trackIndex": N}).
type BurnInResult struct {
	TrackIndex int `json:"trackIndex"`
}

// ExportJobResult is file.export's response shape ({"jobId": "..."}).
type ExportJobResult struct {
	JobID string `json:"jobId"`
}

// FrameDataResult is playback.getFrame's response shape
// ({"format": "...", "data": "<base64>"}).
type FrameDataResult struct {
	Format string `json:"format"`
	Data   string `json:"data"`
}

// FadeInOutResult is audio.setFadeInOut's response shape: whichever of
// fadeIn/fadeOut frame counts were requested get a FilterInfo back under
// the matching key; both are optional since the request only requires at
// least one of fadeInFrames/fadeOutFrames.
type FadeInOutResult struct {
	FadeIn  *FilterInfo `json:"fadeIn,omitempty"`
	FadeOut *FilterInfo `json:"fadeOut,omitempty"`
}

// AutoFadeResult is audio.setAutoFade's response shape -- genuinely two
// different shapes depending on the request's "enabled" flag (enabling
// returns the new autofade FilterInfo; disabling returns how many autofade
// filters were removed instead), and this mcp-go version has no oneOf/
// anyOf to express that precisely (see this package's doc comment on
// dynamic input fields for the same limitation). Modeled as one
// all-fields-optional struct covering the union of both shapes' fields --
// strictly rejects any field beyond these four, but can't by itself
// enforce "exactly one of these two field sets".
type AutoFadeResult struct {
	FilterIndex *int    `json:"filterIndex,omitempty"`
	MltService  *string `json:"mltService,omitempty"`
	Enabled     *bool   `json:"enabled,omitempty"`
	Removed     *int    `json:"removed,omitempty"`
}

// ProjectCurrentResult is project_current's (Go-side-only) response shape.
type ProjectCurrentResult struct {
	ProjectID string `json:"projectId"`
	Bound     bool   `json:"bound"`
}

// StringList is the {"items": [...]} wrapper for Vec<String> responses
// (recent.list).
type StringList struct {
	Items []string `json:"items"`
}

// markers.next/markers.prev return a bare JSON integer or a bare JSON null.
// oneOf expresses that top-level union without using a JSON-Schema `type`
// array, which would not round-trip through mcp-go v0.56.0's client-side
// ToolOutputSchema.Type string field.
const nullableFrameOutputSchema = `{"oneOf":[{"type":"integer"},{"type":"null"}]}`
