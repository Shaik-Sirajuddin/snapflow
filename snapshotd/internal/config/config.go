// Package config holds snapshotd's daemon-wide configuration: file paths,
// socket locations, and the child-process binary location. Values can be
// overridden via environment variables so `snapshotd serve` is configurable
// without a config file for v1.
package config

import (
	"os"
	"path/filepath"
	"strconv"
)

// Config is the daemon's runtime configuration. Defaults follow the
// "~/.snapshotd/..." layout used throughout 06-daemon-mcp-proxy.md and
// 08-lifecycle-and-cli.md.
type Config struct {
	// HomeDir is the daemon's own state directory, default ~/.snapshotd.
	HomeDir string

	// DBPath is the SQLite file backing the GORM registry (07-daemon-persistence.md).
	DBPath string

	// ControlSocketPath is the SDP JSON-RPC 2.0 control socket
	// (06-daemon-mcp-proxy.md's "SDPServer", 08's docker.sock analogy).
	ControlSocketPath string

	// RunDir is where per-project SAP socket files are created by default,
	// mirroring the "/run/snapshot/*.sock" convention from 03/06.
	RunDir string

	// ProjectsRoot is the default parent directory for daemon-created project
	// folders (project.new / daemon.createProject), per
	// 09-project-folder-layout.md's "<projectsRoot>/<name>/" convention.
	ProjectsRoot string

	// SnapshotBinPath is the child (sap-rust) binary snapshotd launches per
	// project. Configurable via SNAPSHOT_BIN_PATH; defaults to whatever
	// discoverSnapshotBinPath finds by searching this checkout for a built
	// sibling sap-rust crate (preferring a release build over debug). That
	// crate is developed independently and is NOT assumed to exist here --
	// Launch() must fail cleanly with a clear error if the binary is
	// missing, never panic or hang.
	SnapshotBinPath string

	// MCPSSEAddr is the address the always-on MCP SSE adapter listens on, per
	// 08's "SSE MCP enabled by default" decision.
	MCPSSEAddr string

	// AudioEnabled controls the optional audio.* convenience namespace. It is
	// disabled by default so agents cannot discover or call audio operations
	// until an operator explicitly enables it.
	AudioEnabled bool
}

// Default returns the v1 default configuration, honoring the handful of
// environment-variable overrides described in the docs (SNAPSHOT_BIN_PATH for
// the child binary; SNAPSHOTD_HOME for relocating all daemon state, mainly
// useful in tests).
func Default() Config {
	home := os.Getenv("SNAPSHOTD_HOME")
	if home == "" {
		if uh, err := os.UserHomeDir(); err == nil {
			home = filepath.Join(uh, ".snapshotd")
		} else {
			home = ".snapshotd"
		}
	}

	binPath := os.Getenv("SNAPSHOT_BIN_PATH")
	if binPath == "" {
		binPath = discoverSnapshotBinPath()
	}

	mcpAddr := os.Getenv("SNAPSHOTD_MCP_SSE_ADDR")
	if mcpAddr == "" {
		mcpAddr = "127.0.0.1:7777"
	}
	audioEnabled, _ := strconv.ParseBool(os.Getenv("SNAPSHOTD_AUDIO_ENABLED"))

	return Config{
		HomeDir:           home,
		DBPath:            filepath.Join(home, "registry.db"),
		ControlSocketPath: filepath.Join(home, "control.sock"),
		RunDir:            filepath.Join(home, "run"),
		ProjectsRoot:      filepath.Join(home, "projects"),
		SnapshotBinPath:   binPath,
		MCPSSEAddr:        mcpAddr,
		AudioEnabled:      audioEnabled,
	}
}

// discoverSnapshotBinPath implements the "default/dev config points at the
// real sap-rust binary" requirement: it searches a handful of directories
// derived from both the current working directory and the running
// executable's own location (each walked a few levels up) for
// "sap-rust/target/{release,debug}/sap-rust", preferring a release build
// over a debug build wherever both exist under the same candidate root.
// This makes `snapshotd serve`, run from either this repo's root or from
// inside snapshotd/ (the two places someone would actually run it from in
// this checkout), find the sibling sap-rust crate's build output without
// any extra configuration.
//
// If nothing is found (sap-rust not built yet, or snapshotd installed
// somewhere with no sap-rust checkout nearby), this falls back to the
// original relative-to-executable guess -- procmgr.Launch treats a missing
// binary at that path as a normal, clean "not found" error, never a startup
// failure. SNAPSHOT_BIN_PATH always overrides this entirely, per the
// env-var documented above and in README.md.
func discoverSnapshotBinPath() string {
	var roots []string
	if cwd, err := os.Getwd(); err == nil {
		roots = append(roots, ancestors(cwd, 4)...)
	}
	if exe, err := os.Executable(); err == nil {
		roots = append(roots, ancestors(filepath.Dir(exe), 4)...)
	}

	for _, root := range roots {
		for _, variant := range []string{"release", "debug"} {
			candidate := filepath.Join(root, "sap-rust", "target", variant, "sap-rust")
			if info, err := os.Stat(candidate); err == nil && !info.IsDir() {
				return candidate
			}
		}
	}

	// Fallback: the old relative-to-executable guess, kept so the resulting
	// error message (from procmgr.Launch's os.Stat check) still names a
	// sensible, repo-shaped path instead of an empty string.
	if exe, err := os.Executable(); err == nil {
		return filepath.Join(filepath.Dir(exe), "..", "sap-rust", "target", "debug", "sap-rust")
	}
	return filepath.Join("..", "sap-rust", "target", "debug", "sap-rust")
}

// ancestors returns dir followed by up to depth of its parent directories.
func ancestors(dir string, depth int) []string {
	out := []string{dir}
	for i := 0; i < depth; i++ {
		parent := filepath.Dir(dir)
		if parent == dir {
			break
		}
		dir = parent
		out = append(out, dir)
	}
	return out
}

// EnsureDirs creates the daemon's on-disk directories (idempotent).
func (c Config) EnsureDirs() error {
	for _, d := range []string{c.HomeDir, c.RunDir, c.ProjectsRoot} {
		if err := os.MkdirAll(d, 0o755); err != nil {
			return err
		}
	}
	return nil
}
