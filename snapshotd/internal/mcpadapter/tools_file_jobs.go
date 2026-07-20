package mcpadapter

import (
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"
)

// fileJobsTools builds file.import/file.export and the 3 jobs.* export-job
// tools. file.probe is built separately in mcpadapter.go's projectTools,
// since it is deliberately not project-bound.
func fileJobsTools(s *server.MCPServer, h Handler) []server.ServerTool {
	return []server.ServerTool{
		sapTool(s, h, "file.import", "file.import", "Import a media file inside the current project's root.",
			mcp.WithString("path", mcp.Required(), mcp.Description("Filesystem path to the media file")),
			mcp.WithOutputSchema[PlaylistEntry](),
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
