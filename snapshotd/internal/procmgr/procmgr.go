// Package procmgr implements the daemon's process manager: launching,
// listing, health-checking, and closing per-project child (sap-rust)
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
}

// Manager launches and tracks per-project child processes.
type Manager struct {
	Reg *registry.Registry

	// BinPath is the child binary to exec (SNAPSHOT_BIN_PATH / config.Config.SnapshotBinPath).
	BinPath string

	// RunDir holds per-instance SAP socket files.
	RunDir string

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
func New(reg *registry.Registry, binPath, runDir string) *Manager {
	return &Manager{
		Reg:            reg,
		BinPath:        binPath,
		RunDir:         runDir,
		ConnectTimeout: 5 * time.Second,
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
	cmd.Env = append(os.Environ(),
		"SNAPSHOT_SAP_SOCKET="+sockPath,
		"SNAPSHOT_SAP_TOKEN="+token,
		"SNAPSHOT_HEADLESS="+headlessVal,
		"SNAPSHOT_PROJECT_ROOT="+opts.ProjectRoot,
		"SNAPSHOT_AUDIO_ENABLED="+audioEnabledVal,
	)
	cmd.Stdout = nil
	cmd.Stderr = nil

	if err := cmd.Start(); err != nil {
		return registry.ProcessInstance{}, fmt.Errorf("procmgr: start child: %w", err)
	}

	if !m.waitForSocket(ctx, sockPath) {
		_ = cmd.Process.Kill()
		_, _ = cmd.Process.Wait()
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
