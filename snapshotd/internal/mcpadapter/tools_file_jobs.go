package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// fileJobsTools builds file.import/file.export/file.probe and the 3
// jobs.* export-job tools. file.probe is deliberately not project-bound
// (shells out to real ffprobe directly, stateless, independent of any
// live MLT project profile -- see sapcall_export_realsaprust_test.go's
// own doc comment for the concrete contrast against playlist.append's
// profile-relative frame count), but it lives here rather than in
// tools_project.go since its own name is already file.*-prefixed and it
// needs no special project-binding awareness the way project.* does.
func fileJobsTools(s *server.MCPServer, h Handler) []server.ServerTool {
	return []server.ServerTool{
		sapTool(s, h, "file.import", "file.import", "Import a media file inside the current project's root.",
			mcp.WithString("path", mcp.Required(), mcp.Description("Filesystem path to the media file")),
			mcp.WithOutputSchema[PlaylistEntry](),
		),
		sapTool(s, h, "file.probe", "file.probe", "Probe a media file's own native metadata (codec, duration) directly via ffprobe, without needing any project bound -- independent of any live project's profile.",
			mcp.WithString("path", mcp.Required(), mcp.Description("Filesystem path to the media file")),
			mcp.WithOutputSchema[FileProbe](),
		),
		sapTool(s, h, "file.export", "file.export", "Start an export job for the current project.",
			mcp.WithString("outputPath", mcp.Required(), mcp.Description("Filesystem path to write the exported file")),
			mcp.WithString("codec", mcp.DefaultString("h264"), mcp.Description("Video codec")),
			mcp.WithString("container", mcp.DefaultString("mp4"), mcp.Description("Output container format")),
			mcp.WithOutputSchema[ExportJobResult](),
		),
		sapTool(s, h, "jobs.list", "jobs.list", "List export jobs for the current project.",
			mcp.WithOutputSchema[JobStatusList](),
		),
		sapTool(s, h, "jobs.get", "jobs.get", "Read an export job's status.",
			mcp.WithString("jobId", mcp.Required(), mcp.Description("Export job ID")),
			mcp.WithOutputSchema[JobStatus](),
		),
		sapTool(s, h, "jobs.stop", "jobs.stop", "Stop a running export job.",
			mcp.WithString("jobId", mcp.Required(), mcp.Description("Export job ID")),
			mcp.WithOutputSchema[EmptyResult](),
		),
	}
}
