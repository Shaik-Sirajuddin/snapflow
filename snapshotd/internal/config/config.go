// Package config holds snapshotd's daemon-wide configuration: file paths,
// socket locations, and the child-process binary location. Values can be
// overridden via environment variables so `snapshotd serve` is configurable
// without a config file for v1.
package config

import (
	"os"
	"path/filepath"
	"strconv"
	"strings"
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

	// LogDir is where each launched child process's stdout/stderr is
	// captured, one file per ProcessInstance (named "<instanceID>.log").
	// This is the only place the real Qt/C++ path's own diagnostics (e.g.
	// sap_ffi.cpp's "[sap_ffi] event: ..." lines, Qt/MLT warnings) end up --
	// previously discarded entirely (cmd.Stdout/Stderr were left nil).
	LogDir string

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
		LogDir:            filepath.Join(home, "logs"),
		ProjectsRoot:      filepath.Join(home, "projects"),
		SnapshotBinPath:   binPath,
		MCPSSEAddr:        mcpAddr,
		AudioEnabled:      audioEnabled,
	}
}

// discoverShotcutBinPath is the primary lookup: the real, production
// FfiBackend-linked Qt binary (`shotcut`, built by
// `cmake -S shotcut -B <builddir> && ninja` per shotcut/CMakeLists.txt's
// corrosion_import_crate(... FEATURES real_ffi) integration -- see
// sap-rust/README.md's "Real FFI" section). Searches a small set of
// `shotcut/build*/src/shotcut` glob candidates under each ancestor root,
// preferring a build dir name containing "release" over any other match,
// so a plain `cmake -B shotcut/build-real-ffi` (or similarly named)
// checkout is found without extra configuration.
func discoverShotcutBinPath(roots []string) string {
	var fallback string
	for _, root := range roots {
		matches, err := filepath.Glob(filepath.Join(root, "shotcut", "build*", "src", "shotcut"))
		if err != nil {
			continue
		}
		for _, candidate := range matches {
			info, err := os.Stat(candidate)
			if err != nil || info.IsDir() {
				continue
			}
			if strings.Contains(candidate, "release") {
				return candidate
			}
			if fallback == "" {
				fallback = candidate
			}
		}
	}
	return fallback
}

// discoverSnapshotBinPath implements the "default/dev config points at the
// real, production child binary" requirement. It searches a handful of
// directories derived from both the current working directory and the
// running executable's own location (each walked a few levels up).
//
// Preference order: the real Qt/`real_ffi` `shotcut` binary
// (discoverShotcutBinPath) first -- that is the one production backend
// (FfiBackend, calling into a real live Shotcut process; see
// sap-rust/README.md). Only if no such build is found does this fall back
// to the standalone `sap-rust/target/{release,debug}/sap-rust` binary,
// which as of the MltBackend removal only runs MockBackend (no real
// media/editing) -- suitable for wire-protocol smoke testing, not a
// substitute for the real Qt build in production.
//
// If neither is found, this falls back to the original
// relative-to-executable guess -- procmgr.Launch treats a missing binary
// at that path as a normal, clean "not found" error, never a startup
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

	if shotcutBin := discoverShotcutBinPath(roots); shotcutBin != "" {
		return shotcutBin
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
// EnsureDirs creates the daemon's on-disk directories (idempotent). Empty
// entries (e.g. LogDir left unset by a test-constructed Config literal, per
// its "empty means discard" doc comment) are skipped rather than treated
// as an error.
func (c Config) EnsureDirs() error {
	for _, d := range []string{c.HomeDir, c.RunDir, c.ProjectsRoot, c.LogDir} {
		if d == "" {
			continue
		}
		if err := os.MkdirAll(d, 0o755); err != nil {
			return err
		}
	}
	return nil
}
