package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// filterTools builds the 8 filter.* MLT-filter/keyframe tools.
func filterTools(s *server.MCPServer, h Handler) []server.ServerTool {
	return []server.ServerTool{
		sapTool(s, h, "filter.add", "filter.add", "Attach an MLT filter to a clip.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithString("mltService", mcp.Required(), mcp.Description("MLT filter service name, e.g. \"volume\", \"panner\"")),
			mcp.WithObject("properties", mcp.Description("Initial MLT filter property key/value map")),
			mcp.WithOutputSchema[FilterInfo](),
		),
		sapTool(s, h, "filter.list", "filter.list", "List filters attached to a clip.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithOutputSchema[FilterListEntryList](),
		),
		sapTool(s, h, "filter.remove", "filter.remove", "Detach a filter from a clip.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithInteger("filterIndex", mcp.Required(), mcp.Description("Filter index on the clip")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "filter.reorder", "filter.reorder", "Reorder a filter in a clip's filter chain.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithInteger("filterIndex", mcp.Required(), mcp.Description("Current filter index")),
			mcp.WithInteger("newIndex", mcp.Required(), mcp.Description("Destination filter index")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "filter.addKeyframe", "filter.addKeyframe", "Add a filter-property keyframe.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithInteger("filterIndex", mcp.Required(), mcp.Description("Filter index on the clip")),
			mcp.WithString("property", mcp.Required(), mcp.Description("Filter property name")),
			mcp.WithInteger("position", mcp.Required(), mcp.Description("Frame position of the keyframe")),
			mcp.WithAny("value", mcp.Required(), mcp.Description("Keyframe value; type depends on the filter property (number, string, etc.)")),
			mcp.WithString("interpolation", mcp.Enum("linear", "smooth", "discrete", "hold"), mcp.DefaultString("linear"), mcp.Description("Keyframe interpolation mode")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "filter.listKeyframes", "filter.listKeyframes", "List keyframes for a filter property.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithInteger("filterIndex", mcp.Required(), mcp.Description("Filter index on the clip")),
			mcp.WithString("property", mcp.Required(), mcp.Description("Filter property name")),
			mcp.WithOutputSchema[KeyframeInfoList](),
		),
		sapTool(s, h, "filter.removeKeyframe", "filter.removeKeyframe", "Remove a keyframe from a filter property.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithInteger("filterIndex", mcp.Required(), mcp.Description("Filter index on the clip")),
			mcp.WithString("property", mcp.Required(), mcp.Description("Filter property name")),
			mcp.WithInteger("position", mcp.Required(), mcp.Description("Frame position of the keyframe to remove")),
			mcp.WithOutputSchema[EmptyResult](),
		),
		sapTool(s, h, "filter.setProperty", "filter.setProperty", "Set a filter property, either statically or at a specific keyframe position.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithInteger("filterIndex", mcp.Required(), mcp.Description("Filter index on the clip")),
			mcp.WithString("property", mcp.Required(), mcp.Description("Filter property name")),
			mcp.WithAny("value", mcp.Required(), mcp.Description("Property value; type depends on the filter property (number, string, etc.)")),
			mcp.WithInteger("position", mcp.Description("Frame position; omit for a static (non-keyframed) property")),
			mcp.WithOutputSchema[EmptyResult](),
		),
	}
}
