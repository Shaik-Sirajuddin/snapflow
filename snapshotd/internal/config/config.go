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
	"time"
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

	// LaunchConnectTimeout is how long daemon.launch waits for a newly
	// spawned instance to open its own SAP control socket before giving
	// up and killing it (procmgr.Manager.ConnectTimeout, default 5s).
	// Configurable via SNAPSHOTD_LAUNCH_TIMEOUT (a Go duration string,
	// e.g. "30s") since a real Qt/MLT cold start -- font/plugin
	// enumeration, first-paint layout -- can genuinely take longer than
	// 5s under software rendering (observed directly: a real launch on
	// this checkout's Xvfb+software-renderer setup took 15-20s to open
	// its socket, well past the previous hardcoded default, causing
	// every `daemon.launch --gui` call to fail with a misleading
	// "child did not open ... within 5s" error even though the child
	// process itself was healthy and would have opened it shortly after
	// being killed for "taking too long").
	LaunchConnectTimeout time.Duration

	// MCPSSEAddr is the address the always-on MCP SSE adapter listens on, per
	// 08's "SSE MCP enabled by default" decision.
	MCPSSEAddr string

	// AudioEnabled controls the optional audio.* convenience namespace. It is
	// disabled by default so agents cannot discover or call audio operations
	// until an operator explicitly enables it.
	AudioEnabled bool

	// AcpxEnabled starts an optional long-lived acpx-server child under serve
	// (snapshotd-bundled-acpx-gateway). Default: on when AcpxBinPath is found
	// and SNAPSHOTD_ACPX_ENABLED is unset; explicit 0/1 always wins.
	AcpxEnabled bool

	// AcpxBinPath is the acpx-server binary (SNAPSHOTD_ACPX_BIN or discovery).
	AcpxBinPath string

	// AcpxHttpBind is ACPX_HTTP_BIND for the child (default 127.0.0.1:8790).
	AcpxHttpBind string

	// AcpxConfigPath is the generated ACPX_CONFIG_FILE path.
	AcpxConfigPath string

	// AcpxBackendCmd is optional ACPX_BACKEND_CMD for the bundled
	// acpx-server's "default" profile (SNAPSHOTD_ACPX_BACKEND_CMD).
	// Empty means acpx-server picks its own built-in default backend
	// (a real, auth-requiring adapter) -- set this to point the bundled
	// gateway at a no-auth stdio ACP agent (e.g. panel-rust's
	// `rui-mock-agent` dev/test binary) for local verification flows
	// that shouldn't need real provider credentials.
	AcpxBackendCmd string
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

	launchTimeout := 30 * time.Second
	if v := os.Getenv("SNAPSHOTD_LAUNCH_TIMEOUT"); v != "" {
		if d, err := time.ParseDuration(v); err == nil {
			launchTimeout = d
		}
	}

	mcpAddr := os.Getenv("SNAPSHOTD_MCP_SSE_ADDR")
	if mcpAddr == "" {
		mcpAddr = "127.0.0.1:7777"
	}
	audioEnabled, _ := strconv.ParseBool(os.Getenv("SNAPSHOTD_AUDIO_ENABLED"))

	acpxBin := os.Getenv("SNAPSHOTD_ACPX_BIN")
	if acpxBin == "" {
		acpxBin = discoverAcpxServerBinPath()
	}
	acpxBind := os.Getenv("SNAPSHOTD_ACPX_HTTP_BIND")
	if acpxBind == "" {
		acpxBind = "127.0.0.1:8790"
	}
	acpxConfig := os.Getenv("SNAPSHOTD_ACPX_CONFIG")
	if acpxConfig == "" {
		acpxConfig = filepath.Join(home, "acpx-config.json")
	}
	acpxEnabled := false
	if v, ok := os.LookupEnv("SNAPSHOTD_ACPX_ENABLED"); ok {
		acpxEnabled, _ = strconv.ParseBool(v)
	} else {
		// Default on only when a binary is discoverable so plain serve stays quiet.
		acpxEnabled = acpxBin != ""
	}
	acpxBackendCmd := os.Getenv("SNAPSHOTD_ACPX_BACKEND_CMD")

	return Config{
		HomeDir:              home,
		DBPath:               filepath.Join(home, "registry.db"),
		ControlSocketPath:    filepath.Join(home, "control.sock"),
		RunDir:               filepath.Join(home, "run"),
		LogDir:               filepath.Join(home, "logs"),
		ProjectsRoot:         filepath.Join(home, "projects"),
		SnapshotBinPath:      binPath,
		LaunchConnectTimeout: launchTimeout,
		MCPSSEAddr:           mcpAddr,
		AudioEnabled:         audioEnabled,
		AcpxEnabled:          acpxEnabled,
		AcpxBinPath:          acpxBin,
		AcpxHttpBind:         acpxBind,
		AcpxConfigPath:       acpxConfig,
		AcpxBackendCmd:       acpxBackendCmd,
	}
}

// discoverAcpxServerBinPath looks for acpx-server next to the running
// executable or under a checkout's acpx/target/{release,debug}/.
func discoverAcpxServerBinPath() string {
	var roots []string
	if cwd, err := os.Getwd(); err == nil {
		roots = append(roots, cwd)
		p := cwd
		for i := 0; i < 6; i++ {
			p = filepath.Dir(p)
			roots = append(roots, p)
		}
	}
	if execPath, err := os.Executable(); err == nil {
		execDir := filepath.Dir(execPath)
		roots = append(roots, execDir)
		// Same directory as snapshotd binary (packaging layout).
		sibling := filepath.Join(execDir, "acpx-server")
		if st, err := os.Stat(sibling); err == nil && !st.IsDir() {
			return sibling
		}
	}
	var fallback string
	for _, root := range roots {
		candidates := []string{
			filepath.Join(root, "acpx", "target", "release", "acpx-server"),
			filepath.Join(root, "acpx", "target", "debug", "acpx-server"),
		}
		for _, candidate := range candidates {
			st, err := os.Stat(candidate)
			if err != nil || st.IsDir() {
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
