// Package config holds snapshotd's daemon-wide configuration: file paths,
// socket locations, and the child-process binary location. Explicit
// environment variables override the persisted Snapflow runtime config.
package config

import (
	"bufio"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"time"

	"snapshotd/internal/transport"
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

	// AcpxAdminBind is the loopback address for acpx-server's admin plane.
	// Defaults to acpxmgr.AdminBind; worktree instances may override it.
	AcpxAdminBind string

	// AcpxDefaultAcpCommand is optional ACPX_DEFAULT_ACP_COMMAND for the
	// bundled acpx-server native-mode default agent
	// (SNAPSHOTD_ACPX_DEFAULT_ACP_COMMAND; legacy alias
	// SNAPSHOTD_ACPX_BACKEND_CMD). Empty means acpx-server picks its own
	// built-in default (a real, auth-requiring adapter) -- set this to
	// point the bundled gateway at a no-auth stdio ACP agent (e.g.
	// panel-rust's `rui-mock-agent` dev/test binary) for local
	// verification flows that shouldn't need real provider credentials.
	AcpxDefaultAcpCommand string
}

// Default returns the v1 default configuration, honoring the handful of
// environment-variable overrides described in the docs (SNAPSHOT_BIN_PATH for
// the child binary; SNAPSHOTD_HOME for relocating all daemon state, mainly
// useful in tests).
func Default() Config {
	persisted := readPersistedRuntimeConfig()
	home := os.Getenv("SNAPSHOTD_HOME")
	if home == "" {
		home = persisted["SNAPSHOTD_HOME"]
	}
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
	controlEndpoint := os.Getenv("SNAPSHOTD_CONTROL_ENDPOINT")
	if controlEndpoint == "" {
		controlEndpoint = os.Getenv("SNAPSHOTD_CONTROL_SOCKET")
	}
	if controlEndpoint == "" {
		controlEndpoint = persisted["SNAPSHOTD_CONTROL_ENDPOINT"]
	}
	if controlEndpoint == "" {
		controlEndpoint = persisted["SNAPSHOTD_CONTROL_SOCKET"]
	}
	if controlEndpoint == "" {
		controlEndpoint = transport.DefaultControlEndpoint(home)
	}

	launchTimeout := 30 * time.Second
	if v := os.Getenv("SNAPSHOTD_LAUNCH_TIMEOUT"); v != "" {
		if d, err := time.ParseDuration(v); err == nil {
			launchTimeout = d
		}
	}

	mcpAddr := os.Getenv("SNAPSHOTD_MCP_SSE_ADDR")
	if mcpAddr == "" {
		mcpAddr = persisted["SNAPSHOTD_MCP_SSE_ADDR"]
	}
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
		// Must mirror acpx-proto's DEFAULT_ACPX_HTTP_ADDR (the Rust single
		// source of truth acpx-server and panel-rust share) -- cross-language,
		// so keep this literal in sync with that constant.
		acpxBind = "127.0.0.1:8790"
	}
	acpxConfig := os.Getenv("SNAPSHOTD_ACPX_CONFIG")
	if acpxConfig == "" {
		acpxConfig = filepath.Join(home, "acpx-config.json")
	}
	acpxAdminBind := os.Getenv("SNAPSHOTD_ACPX_ADMIN_BIND")
	if acpxAdminBind == "" {
		acpxAdminBind = "127.0.0.1:8791"
	}
	acpxEnabled := false
	if v, ok := os.LookupEnv("SNAPSHOTD_ACPX_ENABLED"); ok {
		acpxEnabled, _ = strconv.ParseBool(v)
	} else {
		// Default on only when a binary is discoverable so plain serve stays quiet.
		acpxEnabled = acpxBin != ""
	}
	acpxDefaultAcpCommand := os.Getenv("SNAPSHOTD_ACPX_DEFAULT_ACP_COMMAND")
	if acpxDefaultAcpCommand == "" {
		// Legacy alias kept so existing operator env still works.
		acpxDefaultAcpCommand = os.Getenv("SNAPSHOTD_ACPX_BACKEND_CMD")
	}

	return Config{
		HomeDir:               home,
		DBPath:                filepath.Join(home, "registry.db"),
		ControlSocketPath:     controlEndpoint,
		RunDir:                filepath.Join(home, "run"),
		LogDir:                filepath.Join(home, "logs"),
		ProjectsRoot:          filepath.Join(home, "projects"),
		SnapshotBinPath:       binPath,
		LaunchConnectTimeout:  launchTimeout,
		MCPSSEAddr:            mcpAddr,
		AudioEnabled:          audioEnabled,
		AcpxEnabled:           acpxEnabled,
		AcpxBinPath:           acpxBin,
		AcpxHttpBind:          acpxBind,
		AcpxConfigPath:        acpxConfig,
		AcpxAdminBind:         acpxAdminBind,
		AcpxDefaultAcpCommand: acpxDefaultAcpCommand,
	}
}

// persistedRuntimeConfigPath is shared conceptually with panel-rust's
// runtime config lookup. Keep this file outside SNAPSHOTD_HOME so relocating
// daemon state does not make the panel lose the path needed to find it.
func persistedRuntimeConfigPath() string {
	if path := os.Getenv("SNAPFLOW_CONFIG_FILE"); path != "" {
		return path
	}
	if dir := os.Getenv("SNAPFLOW_CONFIG_DIR"); dir != "" {
		return filepath.Join(dir, "runtime.env")
	}
	home, err := os.UserHomeDir()
	if err != nil {
		home = "."
	}
	if configHome := os.Getenv("XDG_CONFIG_HOME"); configHome != "" && runtimeGOOS != "darwin" {
		return filepath.Join(configHome, "snapflow", "runtime.env")
	}
	switch runtimeGOOS {
	case "darwin":
		return filepath.Join(home, "Library", "Application Support", "Snapflow", "runtime.env")
	case "windows":
		if appData := os.Getenv("APPDATA"); appData != "" {
			return filepath.Join(appData, "Snapflow", "runtime.env")
		}
		return filepath.Join(home, "AppData", "Roaming", "Snapflow", "runtime.env")
	default:
		return filepath.Join(home, ".config", "snapflow", "runtime.env")
	}
}

// runtimeGOOS is a variable to keep path-selection tests independent from
// the host platform without making the public Config API platform-specific.
var runtimeGOOS = runtime.GOOS

func readPersistedRuntimeConfig() map[string]string {
	file, err := os.Open(persistedRuntimeConfigPath())
	if err != nil {
		return nil
	}
	defer file.Close()
	values := make(map[string]string)
	scanner := bufio.NewScanner(file)
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		if strings.HasPrefix(line, "export ") {
			line = strings.TrimSpace(strings.TrimPrefix(line, "export "))
		}
		key, value, ok := strings.Cut(line, "=")
		if !ok {
			continue
		}
		key = strings.TrimSpace(key)
		value = strings.TrimSpace(value)
		if len(value) >= 2 && ((value[0] == '"' && value[len(value)-1] == '"') || (value[0] == '\'' && value[len(value)-1] == '\'')) {
			value = value[1 : len(value)-1]
		}
		if key != "" && value != "" {
			values[key] = value
		}
	}
	return values
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

// exeSuffix is the platform executable suffix used by packaged binaries.
func exeSuffix() string {
	if runtime.GOOS == "windows" {
		return ".exe"
	}
	return ""
}

// discoverShotcutBinPath locates the real production Snapflow executable,
// including the Linux packaged layout: bin/snapflowd alongside
// Snapflow.app/snapflow (the environment-setting wrapper).
func discoverShotcutBinPath(roots []string) string {
	suffix := exeSuffix()
	if execPath, err := os.Executable(); err == nil {
		execDir := filepath.Dir(execPath)
		for _, candidate := range []string{
			filepath.Join(execDir, "snapflow"+suffix),
			filepath.Join(execDir, "..", "Snapflow", "snapflow"+suffix),
			filepath.Join(execDir, "..", "Snapflow.app", "snapflow"+suffix),
			filepath.Join(execDir, "..", "Snapflow.app", "bin", "snapflow"+suffix),
		} {
			if fileExists(candidate) {
				return candidate
			}
		}
		if matches, err := filepath.Glob(filepath.Join(execDir, "..", "*.app", "Contents", "MacOS", "Snapflow")); err == nil {
			for _, candidate := range matches {
				if fileExists(candidate) {
					return candidate
				}
			}
		}
	}
	var fallback string
	for _, root := range roots {
		matches, err := filepath.Glob(filepath.Join(root, "shotcut", "build*", "src", "snapflow"+suffix))
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
	// Installed release bundles keep the production real_ffi GUI beside
	// snapflowd rather than under a repository's shotcut/build*/ tree.  Prefer
	// the Linux wrapper (it establishes the bundled loader/plugin environment)
	// over the raw binary; on macOS and Windows use the app's executable.
	for _, root := range roots {
		candidates := []string{
			filepath.Join(root, "Snapflow.app", "snapflow"),
			filepath.Join(root, "Snapflow.app", "Contents", "MacOS", "Snapflow"),
			filepath.Join(root, "Snapflow.app", "Contents", "MacOS", "snapflow"),
			filepath.Join(root, "Snapflow", "snapflow.exe"),
			filepath.Join(root, "Snapflow", "snapflow"),
		}
		for _, candidate := range candidates {
			info, err := os.Stat(candidate)
			if err != nil || info.IsDir() {
				continue
			}
			return candidate
		}
	}
	return fallback
}

func fileExists(path string) bool {
	info, err := os.Stat(path)
	return err == nil && !info.IsDir()
}

// discoverSnapshotBinPath implements the "default/dev config points at the
// real, production child binary" requirement. It searches a handful of
// directories derived from both the current working directory and the
// running executable's own location (each walked a few levels up).
//
// Preference order: the real Qt/`real_ffi` `snapflow` binary
// (discoverShotcutBinPath) first -- that is the one production backend
// (FfiBackend, calling into a real live Snapflow process; see
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

	if snapflowBin := discoverShotcutBinPath(roots); snapflowBin != "" {
		return snapflowBin
	}

	suffix := exeSuffix()
	for _, root := range roots {
		for _, variant := range []string{"release", "debug"} {
			candidate := filepath.Join(root, "sap-rust", "target", variant, "sap-rust"+suffix)
			if info, err := os.Stat(candidate); err == nil && !info.IsDir() {
				return candidate
			}
		}
	}

	// Fallback: the old relative-to-executable guess, kept so the resulting
	// error message (from procmgr.Launch's os.Stat check) still names a
	// sensible, repo-shaped path instead of an empty string.
	if exe, err := os.Executable(); err == nil {
		return filepath.Join(filepath.Dir(exe), "..", "sap-rust", "target", "debug", "sap-rust"+suffix)
	}
	return filepath.Join("..", "sap-rust", "target", "debug", "sap-rust"+suffix)
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
