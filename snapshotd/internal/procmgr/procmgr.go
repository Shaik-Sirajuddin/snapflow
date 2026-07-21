// Package procmgr implements the daemon's process manager: launching,
// listing, health-checking, and closing per-project child (real Qt/
// real_ffi `shotcut` binary in production; a MockBackend-only `sap-rust`
// standalone binary as a weaker fallback -- see
// config.discoverSnapshotBinPath)
// processes, per 06-daemon-mcp-proxy.md's ProcessManager primitives and
// 08-lifecycle-and-cli.md's launch sequence.
//
// Simplification vs. the docs, noted explicitly: 08-lifecycle-and-cli.md
// specifies a shim-mediated design where the child process itself pushes
// registration (daemon.registerInstance) and periodic heartbeats back to the
// daemon over a *separate* control socket, with a `snapshot-shim` process
// decoupling OS-level reaping from the daemon's own lifetime. None of that
// shim/push-registration machinery is implemented here. Instead, Launch
// poll-connects to the child's SAP socket path after spawning it directly as
// snapshotd's own child (via exec.Command) -- if a connection succeeds within
// the timeout, the child is considered "ready". This is a pragmatic v1
// stand-in: it proves the env-var wiring and gives a real (if weaker)
// liveness signal, but it does not get the shim's two-independent-liveness-
// signal benefit (an OS-level wait()/SIGCHLD signal distinct from the
// socket-connect check) or survive a snapshotd restart without the
// reconciliation sweep in internal/registry taking over.
package procmgr

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"snapshotd/internal/health"
	"snapshotd/internal/registry"
)

// ErrBinaryNotFound is returned by Launch when the configured child binary
// does not exist on disk. sap-rust (the child binary this points at) is
// developed independently and may simply not be built yet in this
// environment -- that is treated as a normal, clean error, not a panic or an
// indefinite hang.
var ErrBinaryNotFound = fmt.Errorf("procmgr: snapshot binary not found")

// LaunchOptions configures a single Launch call.
type LaunchOptions struct {
	// Headless, per 08's "GUI-disabled launch mode": sets SNAPSHOT_HEADLESS=1
	// so the child (once it exists) knows to run with `-platform offscreen`
	// instead of a real display. snapshotd itself has no Qt/display
	// dependency either way; it only forwards the env var.
	Headless bool

	// ProjectRoot is the bound project's sandbox root directory (per
	// 09-project-folder-layout.md), forwarded to the child as
	// SNAPSHOT_PROJECT_ROOT. The child's MltBackend (sap-rust) reads this to
	// know where to write project.mlt/exports//assets/ -- without it, the
	// child has no way to know which project folder it is serving.
	ProjectRoot string

	// AudioEnabled is forwarded to sap-rust so server-side dispatch matches
	// the MCP adapter's discoverability policy.
	AudioEnabled bool

	// MltFileName is the bound project's MLT filename (usually
	// "project.mlt", registry.DefaultMltFileName, but honors a legacy
	// project.open's existing filename), forwarded as
	// SNAPSHOT_PROJECT_MLT_FILENAME so FfiBackend::new can bind
	// MainWindow's current file to the exact real path project.save must
	// persist to. Empty means "let the child default to project.mlt"
	// (kept optional so callers that only care about ProjectRoot, e.g.
	// older tests, don't need updating).
	MltFileName string
}

// Manager launches and tracks per-project child processes.
type Manager struct {
	Reg *registry.Registry

	// BinPath is the child binary to exec (SNAPSHOT_BIN_PATH / config.Config.SnapshotBinPath).
	BinPath string

	// RunDir holds per-instance SAP socket files.
	RunDir string

	// LogDir holds per-instance stdout/stderr capture files (one
	// "<instanceID>.log" per launched child), config.Config.LogDir. Empty
	// means "discard" (kept for callers/tests that don't care about logs),
	// matching the old nil-Stdout/Stderr behavior.
	LogDir string

	// ConnectTimeout bounds the poll-connect health check performed right
	// after spawning (the v1 simplification described in the package doc).
	ConnectTimeout time.Duration
	// PollInterval is how often Launch retries connecting during ConnectTimeout.
	PollInterval time.Duration

	// DaemonInstanceID tags ProcessInstance rows this manager creates, per
	// 07's multi-instance note (unused for coordination in v1, kept for
	// schema/forward-compat).
	DaemonInstanceID string

	mu   sync.Mutex
	cmds map[string]*exec.Cmd // instance id -> running command, for Close()
}

// New constructs a Manager with sane defaults for unset fields.
func New(reg *registry.Registry, binPath, runDir, logDir string) *Manager {
	return &Manager{
		Reg:      reg,
		BinPath:  binPath,
		RunDir:   runDir,
		LogDir:   logDir,
		// 5s (the original default here) only ever worked for headless/
		// MockBackend-style cold starts. A real Qt/MLT GUI cold start
		// (real_ffi's FfiBackend, the only backend `daemon serve`'s
		// production path actually launches for a GUI-visible instance)
		// takes ~15-20s in this sandbox -- this repo's own real-process
		// Go tests already work around the old 5s default by manually
		// overriding `d.Proc.ConnectTimeout` to 60s (see e.g.
		// icon_render_realsaprust_test.go), but `daemon.New` (the
		// `serve` command's own construction path, used by every real
		// `daemon_launch` MCP call) never did the same override, so a
		// real non-headless launch through the daemon always failed
		// with "child did not open <sock> within 5s" even though the
		// child was still mid-startup, not actually stuck. 30s gives
		// real headroom without making a genuinely-stuck child take
		// forever to report failure.
		ConnectTimeout: 30 * time.Second,
		PollInterval:   25 * time.Millisecond,
		cmds:           make(map[string]*exec.Cmd),
	}
}

func randomToken() (string, error) {
	b := make([]byte, 24)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return hex.EncodeToString(b), nil
}

// filterEnvKeys returns a copy of env (an os.Environ()-style "KEY=VALUE"
// slice) with any entries matching the given keys removed.
func filterEnvKeys(env []string, keys ...string) []string {
	out := make([]string, 0, len(env))
	for _, kv := range env {
		skip := false
		for _, k := range keys {
			if strings.HasPrefix(kv, k+"=") {
				skip = true
				break
			}
		}
		if !skip {
			out = append(out, kv)
		}
	}
	return out
}

// randomShortID returns a short (16 hex char) random identifier, used for
// both the ProcessInstance row id and the socket filename -- see the
// sun_path length comment in Launch for why this needs to stay short.
func randomShortID() (string, error) {
	b := make([]byte, 8)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return hex.EncodeToString(b), nil
}

// Launch resolves the child binary, spawns it with the SAP env vars set,
// poll-connects to its socket to confirm it is listening, and persists a
// ProcessInstance row.
func (m *Manager) Launch(ctx context.Context, projectID string, opts LaunchOptions) (registry.ProcessInstance, error) {
	if _, err := os.Stat(m.BinPath); err != nil {
		if os.IsNotExist(err) {
			return registry.ProcessInstance{}, fmt.Errorf("%w: %s", ErrBinaryNotFound, m.BinPath)
		}
		return registry.ProcessInstance{}, fmt.Errorf("procmgr: stat %s: %w", m.BinPath, err)
	}

	if err := os.MkdirAll(m.RunDir, 0o755); err != nil {
		return registry.ProcessInstance{}, fmt.Errorf("procmgr: mkdir run dir: %w", err)
	}

	token, err := randomToken()
	if err != nil {
		return registry.ProcessInstance{}, fmt.Errorf("procmgr: generate token: %w", err)
	}

	// Instance/socket ids are a short random hex string, not e.g. the full
	// project UUID plus a timestamp -- Unix domain socket paths are capped at
	// ~108 bytes (sun_path) on Linux, and RunDir can already be a fairly deep
	// path (e.g. under a user's home directory), so keeping the generated
	// portion short matters in practice, not just in theory.
	shortID, err := randomShortID()
	if err != nil {
		return registry.ProcessInstance{}, fmt.Errorf("procmgr: generate instance id: %w", err)
	}
	instanceID := shortID
	sockPath := filepath.Join(m.RunDir, shortID+".sock")
	const maxSockPathLen = 100 // conservative margin under the ~108-byte sun_path limit
	if len(sockPath) > maxSockPathLen {
		return registry.ProcessInstance{}, fmt.Errorf("procmgr: socket path %q exceeds Unix domain socket path length limits; configure a shorter RunDir", sockPath)
	}

	// The real Qt binary reads $HOME to locate its QSettings config
	// (~/.config/Meltytech/Shotcut.conf), qmlcache, and -- critically for
	// this daemon's own operator -- the FilesDock's default browse root
	// (ShotcutSettings::filesCurrentDir(), which defaults to
	// QStandardPaths::HomeLocation and is remembered across runs). Handing
	// a headless launch the daemon operator's *real* $HOME lets a
	// long-lived, large real home directory (e.g. a dev checkout with
	// thousands of files under it) make the FilesDock's background
	// QFileSystemModel/QFileInfoGather scan take tens of seconds to
	// minutes on first paint -- easily blowing past ConnectTimeout even
	// though the SAP socket itself would otherwise be ready in ~1-2s. It
	// also means concurrently launched instances would share and corrupt
	// each other's Shotcut.conf/autosave/log state. Giving each *project*
	// (not each launch -- reused across relaunches so Qt's qmlcache/font
	// caches stay warm) its own isolated HOME under RunDir avoids both
	// problems.
	qtHomeDir := filepath.Join(m.RunDir, "homes", projectID)
	if err := os.MkdirAll(qtHomeDir, 0o755); err != nil {
		return registry.ProcessInstance{}, fmt.Errorf("procmgr: mkdir qt home dir: %w", err)
	}

	// Deliberately exec.Command, not exec.CommandContext(ctx, ...): ctx here
	// is the inbound RPC request's context, which is cancelled the moment
	// daemon.launch's response is sent. CommandContext kills the process
	// group as soon as its context is done, which would SIGKILL this child
	// right after Launch returns success. The child is meant to outlive the
	// request that spawned it -- explicit cleanup (Close, or the
	// ConnectTimeout failure path below) is the only thing that should kill
	// it.
	cmd := exec.Command(m.BinPath)
	headlessVal := "0"
	if opts.Headless {
		headlessVal = "1"
	}
	audioEnabledVal := "0"
	if opts.AudioEnabled {
		audioEnabledVal = "1"
	}
	// Filter any pre-existing HOME from the inherited environment before
	// appending our own: glibc's getenv returns the *first* match in
	// envp, so simply appending a second "HOME=..." after os.Environ()'s
	// original one would silently lose to it.
	cmd.Env = append(filterEnvKeys(os.Environ(), "HOME"),
		"HOME="+qtHomeDir,
		"SNAPSHOT_SAP_SOCKET="+sockPath,
		"SNAPSHOT_SAP_TOKEN="+token,
		"SNAPSHOT_HEADLESS="+headlessVal,
		"SNAPSHOT_PROJECT_ROOT="+opts.ProjectRoot,
		"SNAPSHOT_PROJECT_MLT_FILENAME="+opts.MltFileName,
		"SNAPSHOT_AUDIO_ENABLED="+audioEnabledVal,
	)
	// The child's own embedded chat panel (panel-rust's agent_bridge.rs)
	// spawns its own per-provider acpx-server gateways and, for the
	// "codex" provider, tries to read $HOME/.codex/auth.json for
	// noninteractive api-key auth (falling back to codex-acp's
	// interactive chat-gpt device flow, which cannot complete headlessly,
	// otherwise -- found live: real per-project launches through this
	// exact Launch crash-looped on that fallback). $HOME here is
	// qtHomeDir, this launch's sandboxed per-project directory (see its
	// own doc comment above for why it must stay sandboxed) -- it has no
	// .codex of its own. ACPX_CODEX_AUTH_FILE is the override
	// read_codex_api_key_from_auth_file already checks first, before
	// $HOME/.codex/auth.json -- pointing it at the real, unsandboxed
	// user's own auth.json here fixes the auth lookup without touching
	// the sandbox itself or routing this chat panel's two providers
	// (codex and claude) through one shared single-backend gateway, which
	// would silently reintroduce the "claude thread gets codex-acp"
	// class of bug this session already found and fixed once.
	if realHome, err := os.UserHomeDir(); err == nil && realHome != "" {
		cmd.Env = append(cmd.Env, "ACPX_CODEX_AUTH_FILE="+filepath.Join(realHome, ".codex", "auth.json"))
	}
	var logFile *os.File
	if m.LogDir != "" {
		if err := os.MkdirAll(m.LogDir, 0o755); err != nil {
			return registry.ProcessInstance{}, fmt.Errorf("procmgr: mkdir log dir: %w", err)
		}
		logPath := filepath.Join(m.LogDir, instanceID+".log")
		f, err := os.OpenFile(logPath, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o644)
		if err != nil {
			return registry.ProcessInstance{}, fmt.Errorf("procmgr: open log file %s: %w", logPath, err)
		}
		logFile = f
		// Both streams into one file: the real child's own diagnostics
		// (e.g. shotcut/src/rustbridge/sap_ffi.cpp's "[sap_ffi] event: ..."
		// lines proving a real edit reached the real C++ path, plus
		// Qt/MLT/ffmpeg startup noise) all go to stderr; stdout carries
		// only "sap-rust: ..." startup lines. Interleaving both in one
		// file, in real chronological order, is more useful for debugging
		// than two separate files.
		cmd.Stdout = logFile
		cmd.Stderr = logFile
	} else {
		cmd.Stdout = nil
		cmd.Stderr = nil
	}

	if err := cmd.Start(); err != nil {
		return registry.ProcessInstance{}, fmt.Errorf("procmgr: start child: %w", err)
	}

	if !m.waitForSocket(ctx, sockPath) {
		_ = cmd.Process.Kill()
		_, _ = cmd.Process.Wait()
		if logFile != nil {
			_ = logFile.Close()
		}
		return registry.ProcessInstance{}, fmt.Errorf("procmgr: child did not open %s within %s", sockPath, m.ConnectTimeout)
	}

	pi := registry.ProcessInstance{
		ID:               instanceID,
		ProjectID:        projectID,
		PID:              cmd.Process.Pid,
		SocketPath:       sockPath,
		Token:            token,
		DaemonInstanceID: m.DaemonInstanceID,
		Status:           registry.StatusReady,
	}
	if err := m.Reg.CreateProcessInstance(&pi); err != nil {
		_ = cmd.Process.Kill()
		_, _ = cmd.Process.Wait()
		if logFile != nil {
			_ = logFile.Close()
		}
		return registry.ProcessInstance{}, fmt.Errorf("procmgr: persist instance: %w", err)
	}
	_ = m.Reg.Audit(projectID, registry.AuditLaunch, "launched pid="+fmt.Sprint(pi.PID))

	m.mu.Lock()
	m.cmds[instanceID] = cmd
	m.mu.Unlock()

	// Reap in the background so the process doesn't become a zombie; Launch
	// itself doesn't block on process exit.
	go func() {
		_ = cmd.Wait()
		if logFile != nil {
			_ = logFile.Close()
		}
	}()

	return pi, nil
}

func (m *Manager) waitForSocket(ctx context.Context, path string) bool {
	deadline := time.Now().Add(m.ConnectTimeout)
	for time.Now().Before(deadline) {
		select {
		case <-ctx.Done():
			return false
		default:
		}
		if health.SocketResponsive(path, 100*time.Millisecond) {
			return true
		}
		time.Sleep(m.PollInterval)
	}
	return false
}

// List returns every ProcessInstance row known to the registry.
func (m *Manager) List() ([]registry.ProcessInstance, error) {
	return m.Reg.ListProcessInstances()
}

// Health re-checks a single instance's socket responsiveness and returns its
// current row (status is not mutated here; that's the reconciler's job at
// startup -- Health is a point-in-time read used by daemon.health).
func (m *Manager) Health(instanceID string) (registry.ProcessInstance, bool, error) {
	pi, err := m.Reg.GetProcessInstance(instanceID)
	if err != nil {
		return registry.ProcessInstance{}, false, err
	}
	ok := health.SocketResponsive(pi.SocketPath, 500*time.Millisecond)
	if ok {
		_ = m.Reg.TouchHealthCheck(instanceID)
	}
	return *pi, ok, nil
}

// Close terminates a running instance (if we hold its handle) and marks its
// registry row closed.
func (m *Manager) Close(instanceID string) error {
	m.mu.Lock()
	cmd, ok := m.cmds[instanceID]
	delete(m.cmds, instanceID)
	m.mu.Unlock()

	if ok && cmd.Process != nil {
		_ = cmd.Process.Kill()
	}

	pi, err := m.Reg.GetProcessInstance(instanceID)
	if err != nil {
		return err
	}
	if err := m.Reg.UpdateProcessInstanceStatus(instanceID, registry.StatusClosed); err != nil {
		return err
	}
	_ = m.Reg.Audit(pi.ProjectID, registry.AuditClose, "closed instance "+instanceID)
	return nil
}
