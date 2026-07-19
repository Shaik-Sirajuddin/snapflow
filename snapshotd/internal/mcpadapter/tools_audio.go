package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// audioTools builds the 6 audio.* tools. Registration is gated by the
// daemon's audio feature flag at the call site in mcpadapter.go's New().
func audioTools(s *server.MCPServer, h Handler) []server.ServerTool {
	return []server.ServerTool{
		sapTool(s, h, "audio.setGain", "audio.setGain", "Add a volume filter with a gain in dB.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithNumber("db", mcp.Required(), mcp.Description("Gain in decibels")),
			mcp.WithInteger("position", mcp.Description("Frame position; omit to set a static gain")),
			mcp.WithOutputSchema[FilterInfo](),
		),
		sapTool(s, h, "audio.setPan", "audio.setPan", "Add a panner filter (channel 0) with split 0..1.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithNumber("pan", mcp.Required(), mcp.Description("Pan split, 0..1")),
			mcp.WithInteger("position", mcp.Description("Frame position; omit to set a static pan")),
			mcp.WithOutputSchema[FilterInfo](),
		),
		sapTool(s, h, "audio.setBalance", "audio.setBalance", "Add a panner filter (channel -1) for stereo balance 0..1.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithNumber("balance", mcp.Required(), mcp.Description("Stereo balance split, 0..1")),
			mcp.WithInteger("position", mcp.Description("Frame position; omit to set a static balance")),
			mcp.WithOutputSchema[FilterInfo](),
		),
		sapTool(s, h, "audio.setNormalize", "audio.setNormalize", "Add one-pass (dynamic_loudness) or two-pass (loudness) audio normalization.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithString("mode", mcp.Required(), mcp.Enum("1pass", "2pass"), mcp.Description("Normalization pass mode")),
			mcp.WithNumber("targetLevel", mcp.Description("Target loudness level (defaults to -23.0)")),
			mcp.WithOutputSchema[FilterInfo](),
		),
		sapTool(s, h, "audio.setFadeInOut", "audio.setFadeInOut", "Add volume filter(s) with keyframed fade-in/out level envelopes.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithInteger("fadeInFrames", mcp.Description("Fade-in duration in frames")),
			mcp.WithInteger("fadeOutFrames", mcp.Description("Fade-out duration in frames")),
			mcp.WithOutputSchema[FadeInOutResult](),
		),
		sapTool(s, h, "audio.setAutoFade", "audio.setAutoFade", "Enable or disable autofade (500ms fade_duration); disabling removes autofade filters.",
			mcp.WithString("clipId", mcp.Required(), mcp.Description("Clip ID")),
			mcp.WithBoolean("enabled", mcp.Required(), mcp.Description("Enable or disable autofade")),
			mcp.WithOutputSchema[AutoFadeResult](),
		),
	}
}
