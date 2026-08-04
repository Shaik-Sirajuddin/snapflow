// Package daemon is snapshotd's core: it owns the registry, session store,
// and process manager, and exposes the daemon.* primitives from
// 06-daemon-mcp-proxy.md's table as plain Go methods. Both the SDP JSON-RPC
// server (internal/sdp) and the MCP adapter (internal/mcpadapter) are thin
// translation layers on top of this same core, per 06's "MCP is one
// access-point adapter, not the protocol itself" correction -- neither
// adapter holds any state of its own beyond what's needed for its transport.
package daemon

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/google/uuid"

	"snapshotd/internal/config"
	"snapshotd/internal/discovery"
	"snapshotd/internal/health"
	"snapshotd/internal/mcpsupervisor"
	"snapshotd/internal/procmgr"
	"snapshotd/internal/registry"
	"snapshotd/internal/sapproxy"
	"snapshotd/internal/session"
)

const externalInstanceLease = 30 * time.Second
const externalSAPReadyTimeout = 5 * time.Second
const externalDiscoveryGrace = 1500 * time.Millisecond
const instanceSaveTimeout = 5 * time.Second

// Daemon is the shared core described above.
type Daemon struct {
	Cfg      config.Config
	Reg      *registry.Registry
	Sessions session.Store
	Proc     *procmgr.Manager
	// SAP is the generic, opaque proxy to project-scoped sap-rust methods
	// (project.*/edit.*/playlist.*/... -- see internal/sapproxy's package
	// doc). Both internal/sdp and internal/mcpadapter route every
	// non-"daemon."-prefixed call through ForwardSAP below, which in turn
	// uses this.
	SAP *sapproxy.Router
	Log *slog.Logger

	// Mcp owns the MCP HTTP listener's live lifecycle (bind address, Basic
	// Auth) -- see internal/mcpsupervisor's package doc for the
	// non-loopback-without-auth refusal this enforces. Callers
	// (cmd/snapshotd's cmdServe) start/stop it explicitly, same as SDP's
	// own server; Dispatch below just forwards daemon.mcp* calls to it.
	Mcp *mcpsupervisor.Supervisor

	// stopCh is closed by RequestStop, which Dispatch's "daemon.stop" case
	// calls. cmdServe's shutdown select loop watches StopRequested() as an
	// alternative to an OS signal: `snapshotd stop` triggers it over the
	// already-open SDP control socket rather than sending SIGTERM to a PID.
	// This is the only cross-platform way to ask this process to shut down
	// gracefully -- (*os.Process).Signal on Windows supports neither
	// SIGTERM nor any other POSIX signal delivered from a separate process,
	// so a PID+signal-based `stop` command can never work there.
	stopCh   chan struct{}
	stopOnce sync.Once
}

type RegisterExternalInstanceParams struct {
	InstanceNonce string         `json:"instanceNonce"`
	PID           int            `json:"pid"`
	ProcessStart  string         `json:"processStart"`
	ProjectPath   string         `json:"projectPath,omitempty"`
	SAPSocketPath string         `json:"sapSocketPath,omitempty"`
	SAPToken      string         `json:"sapToken,omitempty"`
	Capabilities  map[string]any `json:"capabilities,omitempty"`
}

type ExternalInstanceResult struct {
	Instance       registry.ExternalInstance `json:"instance"`
	HeartbeatEvery time.Duration             `json:"heartbeatEvery"`
	LeaseDuration  time.Duration             `json:"leaseDuration"`
}

type discardSink struct{}

func (discardSink) Notify(string, json.RawMessage) {}

func (d *Daemon) RegisterExternalInstance(ctx context.Context, p RegisterExternalInstanceParams) (ExternalInstanceResult, error) {
	if strings.TrimSpace(p.InstanceNonce) == "" || p.PID <= 0 || strings.TrimSpace(p.ProcessStart) == "" {
		return ExternalInstanceResult{}, fmt.Errorf("daemon: registerExternalInstance: instanceNonce, pid, and processStart are required")
	}
	if !health.ProcessIdentityMatches(p.PID, p.ProcessStart) {
		return ExternalInstanceResult{}, fmt.Errorf("daemon: registerExternalInstance: pid/processStart identity does not match a live process")
	}
	if p.SAPSocketPath != "" {
		if err := d.rejectDiscoveryEndpoint(p.SAPSocketPath); err != nil {
			return ExternalInstanceResult{}, fmt.Errorf("daemon: registerExternalInstance: SAP endpoint: %w", err)
		}
	}
	projectPath := ""
	if p.ProjectPath != "" {
		abs, err := canonicalExternalPath(p.ProjectPath)
		if err != nil {
			return ExternalInstanceResult{}, fmt.Errorf("daemon: registerExternalInstance: projectPath: %w", err)
		}
		projectPath = abs
		if _, err := d.Reg.EnsureProjectForPath(projectPath); err != nil {
			return ExternalInstanceResult{}, fmt.Errorf("daemon: registerExternalInstance: project: %w", err)
		}
		// A GUI may register its process identity before its SAP endpoint is
		// listening. In that startup window keep the registration advisory and
		// let the later project-open/update path perform the handoff; attempting
		// a save through a socket that is not live would incorrectly reject GUI
		// startup. Once the endpoint is responsive, drain the daemon owner now.
		if p.SAPSocketPath != "" && health.SocketResponsive(p.SAPSocketPath, time.Second) {
			if err := d.handoffDaemonProjectToGUI(ctx, projectPath); err != nil {
				return ExternalInstanceResult{}, fmt.Errorf("daemon: registerExternalInstance: GUI handoff: %w", err)
			}
		}
	}
	now := time.Now().UTC()
	instance, err := d.Reg.GetExternalInstanceByNonce(p.InstanceNonce)
	if err != nil && !errors.Is(err, registry.ErrNotFound) {
		return ExternalInstanceResult{}, err
	}
	if instance == nil {
		instance = &registry.ExternalInstance{ID: uuid.NewString(), InstanceNonce: p.InstanceNonce, CreatedAt: now}
	}
	capabilities, err := json.Marshal(p.Capabilities)
	if err != nil {
		return ExternalInstanceResult{}, fmt.Errorf("daemon: registerExternalInstance: capabilities: %w", err)
	}
	instance.PID = p.PID
	instance.ProcessStart = p.ProcessStart
	instance.ProjectPath = projectPath
	instance.SAPSocketPath = p.SAPSocketPath
	instance.Token = p.SAPToken
	instance.CapabilitiesJSON = string(capabilities)
	instance.Status = registry.ExternalStatusOpen
	instance.Source = "external_registered"
	instance.LastSeenAt = now
	instance.LeaseExpiresAt = now.Add(externalInstanceLease)
	instance.UpdatedAt = now
	if err := d.Reg.SaveExternalInstance(instance); err != nil {
		return ExternalInstanceResult{}, err
	}
	return ExternalInstanceResult{Instance: *instance, HeartbeatEvery: externalInstanceLease / 3, LeaseDuration: externalInstanceLease}, nil
}

// validateExternalSAPEndpoint prevents the discovery/control endpoint from
// being accidentally advertised as SAP.  The two protocols deliberately use
// different framing and must remain separate: an app-discovery socket is
// never a valid project-control endpoint.  Registration is allowed to omit a
// SAP path while a GUI is still starting; when routing, however, the path
// must be outside the daemon's discovery directory and be responsive.
func (d *Daemon) validateExternalSAPEndpoint(path string) error {
	clean := filepath.Clean(strings.TrimSpace(path))
	if clean == "." || clean == "" {
		return fmt.Errorf("path is empty")
	}
	if err := d.rejectDiscoveryEndpoint(clean); err != nil {
		return err
	}
	if !health.SocketResponsive(clean, time.Second) {
		return fmt.Errorf("path %q is not accepting SAP connections", path)
	}
	return nil
}

func (d *Daemon) rejectDiscoveryEndpoint(path string) error {
	clean := filepath.Clean(strings.TrimSpace(path))
	if clean == "." || clean == "" {
		return fmt.Errorf("path is empty")
	}
	appsDir := filepath.Clean(filepath.Join(d.Cfg.HomeDir, "apps"))
	if clean == appsDir || strings.HasPrefix(clean, appsDir+string(filepath.Separator)) {
		return fmt.Errorf("path %q is an app discovery socket, not a SAP endpoint", path)
	}
	return nil
}

// handoffDaemonProjectToGUI drains a daemon-owned instance before publishing
// the external GUI lease. Registration is the GUI's ownership claim, so the
// old child must be saved and stopped before the claim becomes visible; this
// prevents daemon-first -> GUI-open from ever exposing two live processes for
// one canonical project path.
func (d *Daemon) handoffDaemonProjectToGUI(ctx context.Context, projectPath string) error {
	project, err := d.Reg.EnsureProjectForPath(projectPath)
	if err != nil {
		return err
	}
	instances, err := d.Reg.ListProcessInstancesByProject(project.ID)
	if err != nil {
		return err
	}
	for _, instance := range instances {
		if instance.Status != registry.StatusReady || !health.PIDAlive(instance.PID) {
			continue
		}
		sessionID := "gui-handoff-" + instance.ID
		if _, err := d.SAP.Bind(ctx, sessionID, project.ID, discardSink{}); err != nil {
			return fmt.Errorf("bind old instance %s: %w", instance.ID, err)
		}
		_, saveErr := d.SAP.Call(ctx, sessionID, "project.save", json.RawMessage(`{}`))
		d.SAP.Unbind(sessionID)
		if saveErr != nil {
			return fmt.Errorf("save old instance %s: %w", instance.ID, saveErr)
		}
		if err := d.Proc.Close(instance.ID); err != nil {
			return fmt.Errorf("close old instance %s: %w", instance.ID, err)
		}
	}
	return nil
}

// canonicalExternalPath accepts a path whose final project file may not have
// been created yet (for example during an untitled-to-save transition), while
// still resolving the existing parent and rejecting NUL/control injection.
func canonicalExternalPath(raw string) (string, error) {
	if strings.IndexByte(raw, 0) >= 0 {
		return "", fmt.Errorf("path contains NUL")
	}
	abs, err := filepath.Abs(raw)
	if err != nil {
		return "", err
	}
	abs = filepath.Clean(abs)
	if resolved, err := filepath.EvalSymlinks(abs); err == nil {
		return filepath.Clean(resolved), nil
	}
	parent := filepath.Dir(abs)
	resolvedParent, err := filepath.EvalSymlinks(parent)
	if err != nil {
		return "", fmt.Errorf("path parent is not accessible: %w", err)
	}
	return filepath.Join(resolvedParent, filepath.Base(abs)), nil
}

// validateOpenProjectPath is stricter than canonicalExternalPath. The latter
// intentionally permits a not-yet-created final path during initial untitled
// registration; path-based project.open/project.select/daemon.launch must
// describe a project that exists now, otherwise the daemon would publish an
// open registry row for a typo or deleted project.
func validateOpenProjectPath(path string) error {
	info, err := os.Stat(path)
	if err != nil {
		return fmt.Errorf("project path is not accessible: %w", err)
	}
	if !info.IsDir() {
		if !info.Mode().IsRegular() {
			return fmt.Errorf("project path is not a regular file or directory")
		}
		if !strings.EqualFold(filepath.Ext(path), ".mlt") {
			return fmt.Errorf("project file must use the .mlt extension")
		}
		return nil
	}
	entries, err := os.ReadDir(path)
	if err != nil {
		return fmt.Errorf("project directory is not readable: %w", err)
	}
	for _, entry := range entries {
		if !entry.IsDir() && strings.EqualFold(filepath.Ext(entry.Name()), ".mlt") {
			return nil
		}
	}
	return fmt.Errorf("project directory contains no .mlt file")
}

type UpdateExternalProjectParams struct {
	InstanceID  string `json:"instanceId"`
	ProjectPath string `json:"projectPath,omitempty"`
	Reason      string `json:"reason"`
	Generation  uint64 `json:"generation"`
}

func (d *Daemon) UpdateOpenProject(ctx context.Context, p UpdateExternalProjectParams) (registry.ExternalInstance, error) {
	instance, err := d.Reg.GetExternalInstance(p.InstanceID)
	if err != nil {
		return registry.ExternalInstance{}, err
	}
	if p.Reason == "" {
		return registry.ExternalInstance{}, fmt.Errorf("daemon: updateOpenProject: reason is required")
	}
	if p.ProjectPath != "" {
		path, err := canonicalExternalPath(p.ProjectPath)
		if err != nil {
			return registry.ExternalInstance{}, err
		}
		instance.ProjectPath = path
		if _, err := d.Reg.EnsureProjectForPath(path); err != nil {
			return registry.ExternalInstance{}, fmt.Errorf("daemon: updateOpenProject: project: %w", err)
		}
		if instance.SAPSocketPath != "" && health.SocketResponsive(instance.SAPSocketPath, time.Second) {
			if err := d.handoffDaemonProjectToGUI(ctx, path); err != nil {
				return registry.ExternalInstance{}, fmt.Errorf("daemon: updateOpenProject: GUI handoff: %w", err)
			}
		}
	} else {
		instance.ProjectPath = ""
	}
	instance.Status = registry.ExternalStatusOpen
	instance.Generation = p.Generation
	instance.LifecycleReason = p.Reason
	instance.LastSeenAt = time.Now().UTC()
	instance.LeaseExpiresAt = instance.LastSeenAt.Add(externalInstanceLease)
	instance.UpdatedAt = instance.LastSeenAt
	if err := d.Reg.SaveExternalInstance(instance); err != nil {
		return registry.ExternalInstance{}, err
	}
	_ = d.Reg.Audit("", "external_project_"+p.Reason, fmt.Sprintf("instance=%s generation=%d", p.InstanceID, p.Generation))
	return *instance, nil
}

func (d *Daemon) HeartbeatExternalInstance(ctx context.Context, instanceID string) (ExternalInstanceResult, error) {
	instance, err := d.Reg.GetExternalInstance(instanceID)
	if err != nil {
		return ExternalInstanceResult{}, err
	}
	now := time.Now().UTC()
	instance.Status = registry.ExternalStatusOpen
	instance.LastSeenAt = now
	instance.LeaseExpiresAt = now.Add(externalInstanceLease)
	instance.UpdatedAt = now
	if err := d.Reg.SaveExternalInstance(instance); err != nil {
		return ExternalInstanceResult{}, err
	}
	return ExternalInstanceResult{Instance: *instance, HeartbeatEvery: externalInstanceLease / 3, LeaseDuration: externalInstanceLease}, nil
}

func (d *Daemon) UnregisterExternalInstance(ctx context.Context, instanceID string) error {
	instance, err := d.Reg.GetExternalInstance(instanceID)
	if errors.Is(err, registry.ErrNotFound) {
		return nil
	}
	if err != nil {
		return err
	}
	instance.Status = registry.ExternalStatusClosed
	instance.LeaseExpiresAt = time.Now().UTC()
	instance.UpdatedAt = time.Now().UTC()
	return d.Reg.SaveExternalInstance(instance)
}

func (d *Daemon) DiscoverExternalInstances(ctx context.Context) ([]discovery.Candidate, error) {
	candidates, err := discovery.ScanAndPing(filepath.Join(d.Cfg.HomeDir, "apps"))
	if err != nil {
		return nil, err
	}
	for _, candidate := range candidates {
		// A verified endpoint with no active project is still a healthy
		// process, but it must not create an open project row. The next
		// lifecycle update will register it once a project is opened.
		if !candidate.Verified || strings.TrimSpace(candidate.ProjectPath) == "" {
			continue
		}
		_, err := d.RegisterExternalInstance(ctx, RegisterExternalInstanceParams{
			InstanceNonce: candidate.InstanceNonce,
			PID:           candidate.PID,
			ProcessStart:  candidate.ProcessStart,
			ProjectPath:   candidate.ProjectPath,
			SAPSocketPath: candidate.SAPSocketPath,
			SAPToken:      candidate.SAPToken,
			Capabilities:  map[string]any{"discovery": true, "protocolVersion": candidate.ProtocolVersion},
		})
		if err != nil {
			// Keep the advisory candidate visible even when its project path
			// has become invalid between ping and registration. It is not
			// authoritative until registration succeeds.
			d.Log.Warn("discovered external instance could not be registered", "nonce", candidate.InstanceNonce, "err", err)
		}
	}
	return candidates, nil
}

// ReconcileExternalInstances expires external leases without ever launching
// or killing a GUI process. A PID is only a liveness hint; processStart is
// retained in the record so a future platform-specific identity check can
// reject PID reuse without changing the wire contract.
func (d *Daemon) ReconcileExternalInstances(ctx context.Context) ([]registry.ExternalInstance, error) {
	instances, err := d.Reg.ListExternalInstances()
	if err != nil {
		return nil, err
	}
	now := time.Now().UTC()
	for i := range instances {
		if instances[i].Status == registry.ExternalStatusOpen &&
			(instances[i].LeaseExpiresAt.Before(now) || !health.PIDAlive(instances[i].PID)) {
			instances[i].Status = registry.ExternalStatusStale
			instances[i].UpdatedAt = now
			if err := d.Reg.SaveExternalInstance(&instances[i]); err != nil {
				return nil, err
			}
		}
	}
	contexts, err := d.Reg.ListMcpContexts()
	if err != nil {
		return nil, err
	}
	for _, contextRecord := range contexts {
		if !contextRecord.LeaseExpiresAt.After(now) {
			if err := d.Reg.DeleteMcpContext(contextRecord.ContextToken); err != nil && !errors.Is(err, registry.ErrNotFound) {
				return nil, err
			}
		}
	}
	return d.Reg.ListExternalInstances()
}

type RegisterMcpContextParams struct {
	ContextToken           string `json:"contextToken"`
	ACPSessionID           string `json:"acpSessionId"`
	ChatProjectID          string `json:"chatProjectId"`
	DefaultTargetProjectID string `json:"defaultTargetProjectId"`
}

func (d *Daemon) RegisterMcpContext(ctx context.Context, p RegisterMcpContextParams) (registry.McpContext, error) {
	if strings.TrimSpace(p.ContextToken) == "" || strings.TrimSpace(p.ACPSessionID) == "" || strings.TrimSpace(p.ChatProjectID) == "" {
		return registry.McpContext{}, fmt.Errorf("daemon: registerMcpContext: contextToken, acpSessionId, and chatProjectId are required")
	}
	if _, err := d.Reg.GetProject(p.ChatProjectID); err != nil {
		return registry.McpContext{}, fmt.Errorf("daemon: registerMcpContext: chat project: %w", err)
	}
	if p.DefaultTargetProjectID == "" {
		p.DefaultTargetProjectID = p.ChatProjectID
	}
	if _, err := d.Reg.GetProject(p.DefaultTargetProjectID); err != nil {
		return registry.McpContext{}, fmt.Errorf("daemon: registerMcpContext: target project: %w", err)
	}
	contextRecord, err := d.Reg.GetMcpContext(p.ContextToken)
	if err != nil && !errors.Is(err, registry.ErrNotFound) {
		return registry.McpContext{}, err
	}
	if contextRecord == nil {
		contextRecord = &registry.McpContext{ContextToken: p.ContextToken}
	}
	now := time.Now().UTC()
	contextRecord.ACPSessionID = p.ACPSessionID
	contextRecord.ChatProjectID = p.ChatProjectID
	contextRecord.DefaultTargetProjectID = p.DefaultTargetProjectID
	contextRecord.TargetProjectID = p.DefaultTargetProjectID
	contextRecord.LastSeenAt = now
	contextRecord.LeaseExpiresAt = now.Add(externalInstanceLease)
	if err := d.Reg.SaveMcpContext(contextRecord); err != nil {
		return registry.McpContext{}, err
	}
	return *contextRecord, nil
}

func (d *Daemon) SetMcpProjectTarget(ctx context.Context, token, projectID string) (registry.McpContext, error) {
	contextRecord, err := d.Reg.GetMcpContext(token)
	if err != nil {
		return registry.McpContext{}, err
	}
	if _, err := d.Reg.GetProject(projectID); err != nil {
		return registry.McpContext{}, fmt.Errorf("daemon: setMcpProjectTarget: target project: %w", err)
	}
	contextRecord.TargetProjectID = projectID
	contextRecord.LastSeenAt = time.Now().UTC()
	contextRecord.LeaseExpiresAt = contextRecord.LastSeenAt.Add(externalInstanceLease)
	if err := d.Reg.SaveMcpContext(contextRecord); err != nil {
		return registry.McpContext{}, err
	}
	return *contextRecord, nil
}

// UnregisterMcpContext removes the per-ACP-session MCP binding. It is
// idempotent so panel teardown can race with lease reconciliation or a
// duplicate close without turning normal shutdown into an error.
func (d *Daemon) UnregisterMcpContext(ctx context.Context, token string) error {
	if token == "" {
		return nil
	}
	err := d.Reg.DeleteMcpContext(token)
	if errors.Is(err, registry.ErrNotFound) {
		return nil
	}
	return err
}

// RequestStop signals StopRequested's channel exactly once (idempotent --
// a second call is a no-op, not a panic-on-double-close).
func (d *Daemon) RequestStop() {
	d.stopOnce.Do(func() { close(d.stopCh) })
}

// StopRequested is closed once RequestStop has been called.
func (d *Daemon) StopRequested() <-chan struct{} {
	return d.stopCh
}

// New wires together a Daemon from configuration: opens the registry,
// constructs the in-memory session store, and constructs the process
// manager. It does not start any network listeners -- callers (cmd/snapshotd)
// decide when to start the SDP server / MCP adapter on top of this core.
func New(cfg config.Config, logger *slog.Logger) (*Daemon, error) {
	if logger == nil {
		logger = slog.Default()
	}
	if err := cfg.EnsureDirs(); err != nil {
		return nil, fmt.Errorf("daemon: ensure dirs: %w", err)
	}
	reg, err := registry.Open(cfg.DBPath)
	if err != nil {
		return nil, fmt.Errorf("daemon: open registry: %w", err)
	}
	pm := procmgr.New(reg, cfg.SnapshotBinPath, cfg.RunDir, cfg.LogDir)
	pm.HomeDir = cfg.HomeDir
	if cfg.LaunchConnectTimeout > 0 {
		pm.ConnectTimeout = cfg.LaunchConnectTimeout
	}
	d := &Daemon{
		Cfg:      cfg,
		Reg:      reg,
		Sessions: session.NewMemory(session.DefaultSweepInterval),
		Proc:     pm,
		Log:      logger,
		stopCh:   make(chan struct{}),
	}
	d.SAP = sapproxy.NewRouter(d.resolveProjectInstance)
	d.Mcp = mcpsupervisor.New(d, cfg.HomeDir, cfg.MCPSSEAddr, logger)
	return d, nil
}

// resolveProjectInstance implements sapproxy.Resolver. It first checks
// daemon-owned ProcessInstance rows, then registered external GUI instances.
// The latter is required for a manually launched Shotcut/Snapflow process:
// its SAP socket is authoritative in ExternalInstance, but it is not a child
// represented by ProcessInstance and therefore must not require daemon.launch.
func (d *Daemon) resolveProjectInstance(projectID string) (string, string, error) {
	instances, err := d.Reg.ListProcessInstancesByProject(projectID)
	if err != nil {
		return "", "", err
	}
	for _, in := range instances { // newest first, per ListProcessInstancesByProject's ordering
		if in.Status == registry.StatusReady {
			return in.SocketPath, in.Token, nil
		}
	}

	project, err := d.Reg.GetProject(projectID)
	if err != nil {
		return "", "", err
	}
	externalInstances, err := d.Reg.ListExternalInstances()
	if err != nil {
		return "", "", err
	}
	now := time.Now().UTC()
	for _, in := range externalInstances { // newest first, per ListExternalInstances
		if in.Status != registry.ExternalStatusOpen ||
			in.SAPSocketPath == "" ||
			(!in.LeaseExpiresAt.IsZero() && !in.LeaseExpiresAt.After(now)) ||
			externalProjectRoot(in.ProjectPath) != filepath.Clean(project.RootDir) {
			continue
		}
		if err := d.validateExternalSAPEndpoint(in.SAPSocketPath); err != nil {
			continue
		}
		// External registrations use their own local SAP endpoint and do not
		// have a daemon-generated per-launch hello token.
		return in.SAPSocketPath, in.Token, nil
	}
	return "", "", fmt.Errorf("daemon: no running (ready) process instance for project %s; call daemon.launch first", projectID)
}

func externalProjectRoot(projectPath string) string {
	if projectPath == "" {
		return ""
	}
	if resolved, err := filepath.EvalSymlinks(projectPath); err == nil {
		projectPath = resolved
	}
	projectPath = filepath.Clean(projectPath)
	if filepath.Ext(projectPath) == "" {
		return projectPath
	}
	return filepath.Dir(projectPath)
}

// Reconcile runs the startup reconciliation sweep described in
// 07-daemon-persistence.md. Called once by `snapshotd serve` before opening
// the control socket; also exposed here so tests can call it directly.
func (d *Daemon) Reconcile(ctx context.Context) ([]registry.ReconcileOutcome, error) {
	rc := &registry.Reconciler{
		Reg:           d.Reg,
		PIDAlive:      health.PIDAlive,
		SocketHealthy: health.SocketResponsive,
		HealthTimeout: time.Second,
		// No Relaunch func wired in v1: a crashed instance is left "crashed"
		// for an operator/agent to explicitly daemon.launch again, rather
		// than the daemon silently respawning child processes on its own
		// initiative at startup. This is a conservative default, not a doc
		// requirement either way -- documented in README.md.
	}
	outcomes, err := rc.Reconcile(ctx)
	if err != nil {
		return nil, err
	}
	for _, o := range outcomes {
		d.Log.Info("reconcile", "instance", o.Instance.ID, "action", o.Action, "err", o.Err)
	}
	if _, err := d.ReconcileExternalInstances(ctx); err != nil {
		return nil, fmt.Errorf("daemon: reconcile external instances: %w", err)
	}
	return outcomes, nil
}

// Close releases the daemon's resources (registry connection, session store
// background goroutine). It does not kill already-launched child processes
// (per the reconciliation-on-restart design, they are expected to survive a
// daemon restart).
func (d *Daemon) Close() error {
	_ = d.Sessions.Close()
	return d.Reg.Close()
}

// --- daemon.* primitives, per 06-daemon-mcp-proxy.md's table ---

// CreateProjectParams / CreateProject implement daemon.createProject: create
// a fresh project folder under Cfg.ProjectsRoot, per
// 09-project-folder-layout.md's project.new folder-creation rule (subfolders
// are created lazily on first use, not pre-created here).
type CreateProjectParams struct {
	Name string `json:"name"`
}

func (d *Daemon) CreateProject(ctx context.Context, p CreateProjectParams) (registry.Project, error) {
	// Deprecated thin wrapper: name-only create under Cfg.ProjectsRoot.
	if p.Name == "" {
		return registry.Project{}, fmt.Errorf("daemon: createProject: name is required")
	}
	return d.ProjectCreate(ctx, ProjectCreateParams{
		Path: filepath.Join(d.Cfg.ProjectsRoot, p.Name),
	})
}

// ProjectCreateParams implements project.create (path-first).
type ProjectCreateParams struct {
	// Path is an arbitrary filesystem path for the project folder (or parent
	// for file-type). Required unless Name is set (legacy name-only).
	Path string `json:"path"`
	// Name is only used when Path is empty: creates under Cfg.ProjectsRoot/Name
	// (daemon.createProject compatibility).
	Name string `json:"name,omitempty"`
	// Open when true: caller should chain project.open+project.save after
	// create (MCP tool does this). The daemon method itself only mkdir+register.
	Open *bool `json:"open,omitempty"`
	// MltFileName defaults to project.mlt.
	MltFileName string `json:"mltFileName,omitempty"`
	// ProjectType is "folder" (default) or "file".
	ProjectType string `json:"projectType,omitempty"`
}

// ProjectCreateResult is project.create's response.
type ProjectCreateResult struct {
	Project      registry.Project `json:"project"`
	MltCreated   bool             `json:"mltCreated"`
	ProjectState json.RawMessage  `json:"projectState,omitempty"`
}

// ProjectCreate creates a project folder at an arbitrary path and registers it.
// Does not launch/open; open:true chaining is done by the MCP tool layer.
//
// Fail-closed on duplicate path (registry row or existing filesystem path):
// returns registry.ErrProjectAlreadyExists rather than silently reusing —
// that reuse behavior belongs to project.open, not create.
func (d *Daemon) ProjectCreate(ctx context.Context, p ProjectCreateParams) (registry.Project, error) {
	root := strings.TrimSpace(p.Path)
	if root == "" {
		if p.Name == "" {
			return registry.Project{}, fmt.Errorf("daemon: project.create: path or name is required")
		}
		root = filepath.Join(d.Cfg.ProjectsRoot, p.Name)
	}
	abs, err := filepath.Abs(root)
	if err != nil {
		return registry.Project{}, fmt.Errorf("daemon: project.create: path: %w", err)
	}
	mlt := p.MltFileName
	if mlt == "" {
		mlt = registry.DefaultMltFileName
	}
	ptype := p.ProjectType
	if ptype == "" {
		ptype = registry.ProjectTypeFolder
	}
	if ptype != registry.ProjectTypeFolder && ptype != registry.ProjectTypeFile {
		return registry.Project{}, fmt.Errorf("daemon: project.create: projectType must be folder or file")
	}

	// 1) Registry-level dedup (same query shape as EnsureProjectForPath).
	if existing, err := d.Reg.GetProjectByRootDir(abs); err == nil {
		return registry.Project{}, fmt.Errorf("%w: id=%s rootDir=%s", registry.ErrProjectAlreadyExists, existing.ID, existing.RootDir)
	} else if !errors.Is(err, registry.ErrNotFound) {
		return registry.Project{}, err
	}
	// 2) Filesystem-level: reject if path already exists even when unregistered.
	if _, err := os.Stat(abs); err == nil {
		return registry.Project{}, fmt.Errorf("%w: path already exists on disk: %s", registry.ErrProjectAlreadyExists, abs)
	} else if !os.IsNotExist(err) {
		return registry.Project{}, fmt.Errorf("daemon: project.create: stat %s: %w", abs, err)
	}

	if err := os.MkdirAll(abs, 0o755); err != nil {
		return registry.Project{}, fmt.Errorf("daemon: project.create: mkdir %s: %w", abs, err)
	}
	proj := registry.Project{
		ID:          uuid.NewString(),
		RootDir:     abs,
		MltFileName: mlt,
		ProjectType: ptype,
		Status:      "active",
	}
	if err := d.Reg.CreateProject(&proj); err != nil {
		return registry.Project{}, err
	}
	_ = d.Reg.Audit(proj.ID, registry.AuditCreate, "created project at "+abs)
	registry.FillProjectPathFields(&proj)
	return proj, nil
}

// ProjectCloneParams implements project.clone.
type ProjectCloneParams struct {
	SourcePath string `json:"sourcePath"`
	DestPath   string `json:"destPath"`
	Open       *bool  `json:"open,omitempty"`
}

// ProjectClone copies a project directory tree and registers a new row.
func (d *Daemon) ProjectClone(ctx context.Context, p ProjectCloneParams) (registry.Project, error) {
	if strings.TrimSpace(p.SourcePath) == "" || strings.TrimSpace(p.DestPath) == "" {
		return registry.Project{}, fmt.Errorf("daemon: project.clone: sourcePath and destPath are required")
	}
	src, err := d.resolveOrRegisterProjectByPath(p.SourcePath)
	if err != nil {
		return registry.Project{}, fmt.Errorf("daemon: project.clone: source: %w", err)
	}
	destAbs, err := filepath.Abs(p.DestPath)
	if err != nil {
		return registry.Project{}, err
	}
	if _, err := os.Stat(destAbs); err == nil {
		return registry.Project{}, fmt.Errorf("daemon: project.clone: destPath already exists: %s", destAbs)
	}
	if err := copyDir(src.RootDir, destAbs); err != nil {
		return registry.Project{}, fmt.Errorf("daemon: project.clone: copy: %w", err)
	}
	proj := registry.Project{
		ID:          uuid.NewString(),
		RootDir:     destAbs,
		MltFileName: src.MltFileName,
		ProjectType: src.ProjectType,
		Status:      "active",
	}
	if proj.ProjectType == "" {
		proj.ProjectType = registry.ProjectTypeFolder
	}
	if err := d.Reg.CreateProject(&proj); err != nil {
		return registry.Project{}, err
	}
	_ = d.Reg.Audit(proj.ID, registry.AuditCreate, "cloned from "+src.RootDir+" to "+destAbs)
	registry.FillProjectPathFields(&proj)
	return proj, nil
}

// copyDir recursively copies src directory to dst.
func copyDir(src, dst string) error {
	return filepath.Walk(src, func(path string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(src, path)
		if err != nil {
			return err
		}
		target := filepath.Join(dst, rel)
		if info.IsDir() {
			return os.MkdirAll(target, info.Mode())
		}
		if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
			return err
		}
		in, err := os.Open(path)
		if err != nil {
			return err
		}
		defer in.Close()
		out, err := os.OpenFile(target, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, info.Mode())
		if err != nil {
			return err
		}
		defer out.Close()
		_, err = io.Copy(out, in)
		return err
	})
}

// DeleteProject implements daemon.deleteProject. It removes the registry row
// only -- it deliberately does NOT delete the project folder/files on disk
// (destructive-by-default deletion of a user's media folder is not an
// acceptable default; a real "also delete files" option would need its own
// explicit, separately-confirmed parameter, not implemented here).
func (d *Daemon) DeleteProject(ctx context.Context, projectID string) error {
	if err := d.Reg.DeleteProject(projectID); err != nil {
		return err
	}
	_ = d.Reg.Audit(projectID, registry.AuditDelete, "deleted project row (files left on disk)")
	return nil
}

// ProjectListParams implements project.list optional refresh.
type ProjectListParams struct {
	// Refresh when true re-probes PID+socket liveness for each project's
	// most recent ready ProcessInstance (marks crashed if dead). Default
	// is DB-only (eventually consistent until boot reconcile).
	Refresh bool `json:"refresh"`
}

// ListProjects implements daemon.listProjects (thin wrapper of ProjectList).
func (d *Daemon) ListProjects(ctx context.Context) ([]registry.Project, error) {
	return d.ProjectList(ctx, ProjectListParams{})
}

// ProjectList returns all projects with path-first fields and active/isOpen
// markers. When refresh is true, probes live readiness per project.
func (d *Daemon) ProjectList(ctx context.Context, p ProjectListParams) ([]registry.Project, error) {
	projects, err := d.Reg.ListProjects()
	if err != nil {
		return nil, err
	}
	if !p.Refresh {
		return projects, nil
	}
	for i := range projects {
		projects[i].Active = false
		projects[i].IsOpen = false
		instances, err := d.Reg.ListProcessInstancesByProject(projects[i].ID)
		if err != nil {
			return nil, err
		}
		if len(instances) == 0 {
			continue
		}
		// Newest first from ListProcessInstancesByProject.
		row := instances[0]
		if row.Status != registry.StatusReady {
			continue
		}
		if health.PIDAlive(row.PID) && health.SocketResponsive(row.SocketPath, time.Second) {
			projects[i].Active = true
			projects[i].IsOpen = true
			continue
		}
		// Stale ready row: mark crashed so default list is corrected next time.
		_ = d.Reg.UpdateProcessInstanceStatus(row.ID, registry.StatusCrashed)
		_ = d.Reg.Audit(projects[i].ID, registry.AuditCrash, "project.list refresh: registered ready but not actually alive")
	}
	return projects, nil
}

// ProjectSubscription is the control-plane inventory subscription response.
// The SDP connection remains open after this response and emits
// daemon.projectsChanged notifications when the authoritative inventory
// changes. PollAfter is retained as a client-side fallback hint for older
// clients which do not consume notifications.
type ProjectSubscription struct {
	Projects  []registry.Project `json:"projects"`
	Mode      string             `json:"mode"`
	PollAfter time.Duration      `json:"pollAfter"`
}

func (d *Daemon) SubscribeProjects(ctx context.Context) (ProjectSubscription, error) {
	projects, err := d.ListProjects(ctx)
	if err != nil {
		return ProjectSubscription{}, err
	}
	return ProjectSubscription{Projects: projects, Mode: "push", PollAfter: 5 * time.Second}, nil
}

// LaunchParams / Launch implement daemon.launch.
type LaunchParams struct {
	// ProjectID launches an already-registered project (the common case:
	// after daemon.createProject, or from an MCP session that already knows
	// the project id).
	ProjectID string `json:"projectId"`
	// ProjectPath is the CLI convenience path, per 08-lifecycle-and-cli.md's
	// `snapshotd launch <projectPath>` command and 06's original
	// `launch(projectPath string)` primitive signature: a filesystem path to
	// either a project folder or a legacy bare .mlt file. If no matching
	// Project row exists yet, one is registered on the fly (mirroring
	// project.open's "sandbox root becomes that file's parent directory"
	// rule from 09-project-folder-layout.md for the legacy-file case).
	// Ignored if ProjectID is set.
	ProjectPath string `json:"projectPath,omitempty"`
	// Headless defaults to true (SNAPSHOT_HEADLESS=1) when omitted, per
	// 08-lifecycle-and-cli.md's "GUI-disabled launch mode" being the
	// default for daemon-launched instances -- an agent driving snapshotd
	// has no display to show a GUI on in the first place. Pass an explicit
	// `"headless": false` to opt into a GUI-visible launch. A *bool (rather
	// than bool) is required to distinguish "omitted" from "explicitly
	// false" over JSON.
	Headless *bool `json:"headless,omitempty"`
}

// LaunchResult is daemon.launch's response shape. ProcessInstance is
// embedded (not nested under an `instance` key) so every existing caller
// reading e.g. result.ID/result.Status/result.SocketPath, and every
// existing JSON consumer unmarshaling the response straight into a bare
// registry.ProcessInstance, keeps working unchanged -- Reused is simply an
// additional top-level field. See PISO-9: a caller needs this (together
// with the embedded ProcessInstance.Headless) to tell whether it got a
// freshly spawned instance or was handed the project's already-live one.
type LaunchResult struct {
	registry.ProcessInstance
	Reused bool `json:"reused"`
}

func (d *Daemon) Launch(ctx context.Context, p LaunchParams) (LaunchResult, error) {
	projectID := p.ProjectID
	if projectID == "" {
		if p.ProjectPath == "" {
			return LaunchResult{}, fmt.Errorf("daemon: launch: one of projectId or projectPath is required")
		}
		proj, err := d.resolveOrRegisterProjectByPath(p.ProjectPath)
		if err != nil {
			return LaunchResult{}, err
		}
		projectID = proj.ID
	}
	proj, err := d.Reg.GetProject(projectID)
	if err != nil {
		return LaunchResult{}, fmt.Errorf("daemon: launch: %w", err)
	}
	headless := true
	if p.Headless != nil {
		headless = *p.Headless
	}
	pi, reused, err := d.Proc.Launch(ctx, projectID, procmgr.LaunchOptions{
		Headless:     headless,
		ProjectRoot:  proj.RootDir,
		MltFileName:  proj.MltFileName,
		ProjectType:  proj.ProjectType,
		AudioEnabled: d.Cfg.AudioEnabled,
	})
	if err != nil {
		return LaunchResult{}, err
	}
	return LaunchResult{ProcessInstance: pi, Reused: reused}, nil
}

// AudioNamespaceEnabled exposes the daemon-wide capability toggle to MCP
// adapters without making transport code depend on config internals.
func (d *Daemon) AudioNamespaceEnabled() bool {
	return d.Cfg.AudioEnabled
}

// resolveOrRegisterProjectByPath implements the projectPath side of
// daemon.launch/project.open: find an existing Project by RootDir, or register
// an existing filesystem project. It deliberately does not create a project
// for a missing path; callers must use project.create (and explicitly confirm
// creation) before opening a new project.
func (d *Daemon) resolveOrRegisterProjectByPath(path string) (registry.Project, error) {
	abs, err := filepath.Abs(path)
	if err != nil {
		return registry.Project{}, fmt.Errorf("daemon: resolving path %s: %w", path, err)
	}
	abs = filepath.Clean(abs)
	if err := validateOpenProjectPath(abs); err != nil {
		return registry.Project{}, fmt.Errorf("daemon: project path %s: %w; use project.create first for a new project", abs, err)
	}
	info, err := os.Stat(abs)
	if err != nil {
		return registry.Project{}, fmt.Errorf("daemon: project path %s: %w", abs, err)
	}

	rootDir := abs
	mltFileName := registry.DefaultMltFileName
	if !info.IsDir() {
		rootDir = filepath.Dir(abs)
		mltFileName = filepath.Base(abs)
	}

	projects, err := d.Reg.ListProjects()
	if err != nil {
		return registry.Project{}, err
	}
	for _, p := range projects {
		if p.RootDir == rootDir {
			_ = d.Reg.TouchProjectOpened(p.ID)
			return p, nil
		}
	}

	proj := registry.Project{
		ID:          uuid.NewString(),
		RootDir:     rootDir,
		MltFileName: mltFileName,
		ProjectType: registry.ProjectTypeFolder,
		Status:      "active",
	}
	// Bare .mlt path registration defaults to file-type until open reads the flag.
	if !info.IsDir() {
		proj.ProjectType = registry.ProjectTypeFile
	}
	if err := d.Reg.CreateProject(&proj); err != nil {
		return registry.Project{}, err
	}
	_ = d.Reg.Audit(proj.ID, registry.AuditCreate, "registered from launch path "+rootDir)
	registry.FillProjectPathFields(&proj)
	return proj, nil
}

// InstanceListParams filters the unified daemon.list inventory. A nil Active
// value means "all"; true/false selects live/active or inactive rows.
type InstanceListParams struct {
	Active *bool `json:"active"`
}

// InstanceListItem is the protocol-level union of a daemon-owned process and
// an independently launched GUI. External rows deliberately omit SAP tokens:
// the daemon keeps those credentials in its registry and uses them internally
// for MCP/SAP proxying.
type InstanceListItem struct {
	ID          string    `json:"id"`
	Kind        string    `json:"kind"` // "daemon" or "external"
	ProjectID   string    `json:"projectId,omitempty"`
	ProjectPath string    `json:"projectPath,omitempty"`
	PID         int       `json:"pid"`
	SocketPath  string    `json:"socketPath,omitempty"`
	Status      string    `json:"status"`
	Active      bool      `json:"active"`
	Managed     bool      `json:"managed"`
	Headless    bool      `json:"headless,omitempty"`
	StartedAt   time.Time `json:"startedAt,omitempty"`
	LastSeenAt  time.Time `json:"lastSeenAt,omitempty"`
}

type InstanceListResult struct {
	Active *bool              `json:"active"`
	Items  []InstanceListItem `json:"items"`
}

// List implements daemon.list over both daemon-owned processes and external
// GUI leases. This is the inventory MCP clients need before selecting a live
// project for SAP-backed edits.
func (d *Daemon) List(ctx context.Context, p InstanceListParams) (InstanceListResult, error) {
	owned, err := d.Proc.List()
	if err != nil {
		return InstanceListResult{}, err
	}
	external, err := d.Reg.ListExternalInstances()
	if err != nil {
		return InstanceListResult{}, err
	}
	projects, err := d.Reg.ListProjects()
	if err != nil {
		return InstanceListResult{}, err
	}
	projectForPath := func(path string) string {
		if path == "" {
			return ""
		}
		root := filepath.Clean(path)
		if filepath.Ext(root) == ".mlt" {
			root = filepath.Dir(root)
		} else if info, statErr := os.Stat(root); statErr == nil && !info.IsDir() {
			root = filepath.Dir(root)
		}
		for _, project := range projects {
			if filepath.Clean(project.RootDir) == root {
				return project.ID
			}
		}
		return ""
	}
	items := make([]InstanceListItem, 0, len(owned)+len(external))
	for _, pi := range owned {
		items = append(items, InstanceListItem{
			ID: pi.ID, Kind: "daemon", ProjectID: pi.ProjectID, PID: pi.PID,
			SocketPath: pi.SocketPath, Status: pi.Status,
			Active:  pi.Status == registry.StatusReady && health.PIDAlive(pi.PID) && health.SocketResponsive(pi.SocketPath, time.Second),
			Managed: true, Headless: pi.Headless, StartedAt: pi.StartedAt, LastSeenAt: pi.LastHealthCheckAt,
		})
	}
	now := time.Now().UTC()
	for _, ext := range external {
		active := ext.Status == registry.ExternalStatusOpen &&
			ext.LeaseExpiresAt.After(now) && health.PIDAlive(ext.PID)
		if active {
			// An external lease is only active when it exposes a usable SAP
			// endpoint.  Keep this consistent with resolveProjectInstance,
			// which rejects registrations without SAPSocketPath.
			active = ext.SAPSocketPath != "" && health.SocketResponsive(ext.SAPSocketPath, time.Second)
		}
		items = append(items, InstanceListItem{
			ID: ext.ID, Kind: "external", ProjectID: projectForPath(ext.ProjectPath), ProjectPath: ext.ProjectPath, PID: ext.PID,
			SocketPath: ext.SAPSocketPath, Status: ext.Status, Active: active,
			Managed: false, LastSeenAt: ext.LastSeenAt,
		})
	}
	if p.Active != nil {
		filtered := items[:0]
		for _, item := range items {
			if item.Active == *p.Active {
				filtered = append(filtered, item)
			}
		}
		items = filtered
	}
	return InstanceListResult{Active: p.Active, Items: items}, nil
}

// HealthResult is the daemon.health response shape.
type HealthResult struct {
	Instance registry.ProcessInstance `json:"instance"`
	Healthy  bool                     `json:"healthy"`
}

// Health implements daemon.health for a single process instance id.
func (d *Daemon) Health(ctx context.Context, instanceID string) (HealthResult, error) {
	pi, ok, err := d.Proc.Health(instanceID)
	if err != nil {
		return HealthResult{}, err
	}
	return HealthResult{Instance: pi, Healthy: ok}, nil
}

// saveInstanceBeforeClose persists the live in-memory project before the
// process manager terminates its child. This is intentionally daemon-owned
// instance cleanup only; project.close remains a session-scoped unbind because
// other MCP/GUI leases may still use the same process.
func (d *Daemon) saveInstanceBeforeClose(ctx context.Context, instance *registry.ProcessInstance) error {
	saveCtx, cancel := context.WithTimeout(ctx, instanceSaveTimeout)
	defer cancel()

	sessionID := "daemon-close-save-" + instance.ID
	if _, err := d.SAP.Bind(saveCtx, sessionID, instance.ProjectID, discardSink{}); err != nil {
		return fmt.Errorf("bind instance %s for save: %w", instance.ID, err)
	}
	defer d.SAP.Unbind(sessionID)
	if _, err := d.SAP.Call(saveCtx, sessionID, "project.save", json.RawMessage(`{}`)); err != nil {
		return fmt.Errorf("save instance %s before close: %w", instance.ID, err)
	}
	return nil
}

// CloseInstance implements daemon.close: save, then stop a running process
// instance. A ready/live instance is not killed if its final save fails.
// (Named CloseInstance, not Close, since Daemon.Close already exists for the
// daemon's own lifecycle/resource shutdown -- Go has no overloading.)
func (d *Daemon) CloseInstance(ctx context.Context, instanceID string) error {
	instance, err := d.Reg.GetProcessInstance(instanceID)
	if err != nil {
		return err
	}
	if instance.Status == registry.StatusReady && health.PIDAlive(instance.PID) {
		if err := d.saveInstanceBeforeClose(ctx, instance); err != nil {
			return err
		}
	}
	return d.Proc.Close(instanceID)
}

// --- Generic SAP proxy, per 06-daemon-mcp-proxy.md's proxy requirement ---

// proxySessionTTL is how long an SDP/MCP session's project binding survives
// without an intervening call, per 07's session-TTL model applied to this
// proxy's own session bookkeeping (separate from sap-rust's own connection
// lifetime, which is pooled per-project, not per-session -- see
// internal/sapproxy).
const proxySessionTTL = session.DefaultIdleTTL

// ForwardSAP is the generic, opaque proxy entry point used by both
// internal/sdp.Server and internal/mcpadapter for every method that is NOT
// "daemon."-prefixed: project.select binds sessionID to a project (opening
// or reusing that project's pooled SAP connection, per internal/sapproxy),
// and every other method/params pair is forwarded verbatim to sap-rust with
// no knowledge of what it means. sink receives this project's fanned-out
// notifications for as long as sessionID stays bound.
func (d *Daemon) ForwardSAP(ctx context.Context, sessionID string, sink sapproxy.Sink, method string, params json.RawMessage) (json.RawMessage, error) {
	if _, err := d.Sessions.Lookup(sessionID); err != nil {
		if _, cerr := d.Sessions.Create(sessionID, "proxy", proxySessionTTL); cerr != nil {
			return nil, fmt.Errorf("daemon: create session: %w", cerr)
		}
	} else {
		_ = d.Sessions.Touch(sessionID, proxySessionTTL)
	}

	// mcp-server-side edit log: one line per forwarded sap.* call, so a
	// daemon log file (captured via `snapshotd serve > logfile 2>&1`, or
	// any slog handler this Daemon was constructed with) is an independent,
	// server-side record of "an MCP client asked for this edit" -- separate
	// from and correlatable with the per-instance child-process log
	// (config.LogDir/procmgr.LogDir) that captures the real C++/sap_ffi.cpp
	// side of the same mutation. Kept to a single Info line per call (no
	// full params dump, which can be large/binary) to stay cheap on the hot
	// path; failures get their own Warn line with the error.
	logMethod := func(err error, extra ...any) {
		args := append([]any{"sessionId", sessionID, "method", method}, extra...)
		if err != nil {
			d.Log.Warn("mcp sap edit call failed", append(args, "error", err)...)
			return
		}
		d.Log.Info("mcp sap edit call", args...)
	}

	// project.open is the primary name; project.select is the deprecated alias.
	// Both share this Launch-or-reuse + Bind path.
	if method == "project.select" || method == "project.open" {
		var p struct {
			ProjectID   string `json:"projectId"`
			Path        string `json:"path"`
			ProjectPath string `json:"projectPath"` // alias of path (daemon.launch shape)
		}
		if err := unmarshalParams(params, &p); err != nil {
			logMethod(err)
			return nil, err
		}
		path := p.Path
		if path == "" {
			path = p.ProjectPath
		}
		if p.ProjectID == "" && path != "" {
			proj, err := d.resolveOrRegisterProjectByPath(path)
			if err != nil {
				err = fmt.Errorf("daemon: project.select: path: %w", err)
				logMethod(err)
				return nil, err
			}
			p.ProjectID = proj.ID
		}
		if p.ProjectID == "" {
			err := fmt.Errorf("daemon: project.select: projectId or path is required")
			logMethod(err)
			return nil, err
		}
		if _, err := d.Reg.GetProject(p.ProjectID); err != nil {
			err = fmt.Errorf("daemon: project.select: %w", err)
			logMethod(err, "projectId", p.ProjectID)
			return nil, err
		}
		// A panel publishes its discovery descriptor before the asynchronous
		// registration worker necessarily reaches snapshotd. Promote that
		// verified descriptor here, before the launch decision, so an MCP
		// project.open cannot win the startup race and create a headless copy.
		if _, err := d.DiscoverExternalInstances(ctx); err != nil {
			d.Log.Warn("project.open external discovery failed", "projectId", p.ProjectID, "error", err)
		}
		// Launch-or-reuse before Bind so project.select / project.open do not
		// require a separate daemon.launch. A live external GUI registration
		// is already the project's authoritative process and must be reused;
		// calling Proc.Launch first would create a second headless child before
		// resolveProjectInstance gets a chance to select the GUI socket.
		externalGUI, err := d.awaitLiveExternalProject(ctx, p.ProjectID)
		if err != nil {
			err = fmt.Errorf("daemon: project.select: external ownership: %w", err)
			logMethod(err, "projectId", p.ProjectID)
			return nil, err
		}
		if !externalGUI {
			headless := true
			if _, err := d.Launch(ctx, LaunchParams{ProjectID: p.ProjectID, Headless: &headless}); err != nil {
				err = fmt.Errorf("daemon: project.select: launch: %w", err)
				logMethod(err, "projectId", p.ProjectID)
				return nil, err
			}
		}
		result, err := d.SAP.Bind(ctx, sessionID, p.ProjectID, sink)
		if err != nil {
			logMethod(err, "projectId", p.ProjectID)
			return nil, err
		}
		_ = d.Sessions.BindProject(sessionID, p.ProjectID)
		// Init audit + persist projectType from response.
		var st struct {
			Opened      bool   `json:"opened"`
			ProjectType string `json:"projectType"`
		}
		if json.Unmarshal(result, &st) == nil {
			if st.Opened {
				_ = d.Reg.AuditOnce(p.ProjectID, registry.AuditInit, "project.select opened")
			}
			if st.ProjectType == registry.ProjectTypeFolder || st.ProjectType == registry.ProjectTypeFile {
				_ = d.Reg.UpdateProjectType(p.ProjectID, st.ProjectType)
			}
		}
		logMethod(nil, "projectId", p.ProjectID)
		return result, nil
	}

	// project.close is the primary name; project.exit is the deprecated alias.
	if method == "project.exit" || method == "project.close" {
		// Deliberately NOT forwarded to sap-rust: internal/sapproxy pools one
		// SAP connection per project, shared by every session bound to that
		// project, and sap-rust's own project.select gate lives on that one
		// shared connection (see sap-rust/src/server.rs's per-connection
		// `session.project_id`), not per Go-level session. Forwarding a raw
		// "project.exit" through the shared connection would unselect the
		// project for every OTHER session still bound to it too. Close/exit
		// is therefore purely local bookkeeping: it clears this session's own
		// Router binding (sapproxy.Router.Unbind) so a later project.open
		// -- possibly to a different project -- is no longer rejected by
		// Bind's already-bound guard.
		d.SAP.Unbind(sessionID)
		_ = d.Sessions.BindProject(sessionID, "")
		logMethod(nil)
		return json.RawMessage(`{}`), nil
	}

	result, err := d.SAP.Call(ctx, sessionID, method, params)
	logMethod(err)
	return result, err
}

// awaitLiveExternalProject closes the remaining GUI-start race: panel-rust's
// discovery descriptor can be verified before C++ has finished binding its
// SAP socket. Treat that registration as a pending GUI owner and wait briefly
// for its endpoint instead of either launching a duplicate headless process
// or attempting a connection that can only fail with ENOENT.
func (d *Daemon) awaitLiveExternalProject(ctx context.Context, projectID string) (bool, error) {
	waitCtx, cancel := context.WithTimeout(ctx, externalSAPReadyTimeout)
	defer cancel()
	noCandidateUntil := time.Now().Add(externalDiscoveryGrace)
	for {
		candidates, err := d.DiscoverExternalInstances(ctx)
		if err != nil {
			return false, err
		}
		project, err := d.Reg.GetProject(projectID)
		if err != nil {
			return false, err
		}
		instances, err := d.Reg.ListExternalInstances()
		if err != nil {
			return false, err
		}
		pendingGUI := false
		for _, instance := range instances {
			if instance.Status != registry.ExternalStatusOpen ||
				(!instance.LeaseExpiresAt.IsZero() && !instance.LeaseExpiresAt.After(time.Now().UTC())) ||
				!health.PIDAlive(instance.PID) ||
				externalProjectRoot(instance.ProjectPath) != filepath.Clean(project.RootDir) {
				continue
			}
			if instance.SAPSocketPath == "" {
				return false, fmt.Errorf("daemon: external GUI owns project %s but has no SAP socket", projectID)
			}
			if health.SocketResponsive(instance.SAPSocketPath, 100*time.Millisecond) {
				return d.hasLiveExternalProject(ctx, projectID)
			}
			pendingGUI = true
		}
		if !pendingGUI {
			// A verified panel descriptor can briefly report an empty project
			// while the C++ open notification is crossing into panel-rust. If it
			// already exposes an SAP endpoint, wait for that path update instead
			// of allowing MCP to launch headless in the gap.
			for _, candidate := range candidates {
				if candidate.Verified && candidate.ProjectPath == "" && candidate.SAPSocketPath != "" {
					pendingGUI = true
					break
				}
			}
		}
		if !pendingGUI {
			if time.Now().Before(noCandidateUntil) {
				timer := time.NewTimer(50 * time.Millisecond)
				select {
				case <-waitCtx.Done():
					timer.Stop()
					return false, waitCtx.Err()
				case <-timer.C:
				}
				continue
			}
			return false, nil
		}
		timer := time.NewTimer(50 * time.Millisecond)
		select {
		case <-waitCtx.Done():
			timer.Stop()
			return false, fmt.Errorf("daemon: external GUI SAP socket did not become ready for project %s: %w", projectID, waitCtx.Err())
		case <-timer.C:
		}
	}
}

// hasLiveExternalProject reports whether an external GUI currently owns the
// project. project.open must consult this before daemon.launch: the external
// registration is the GUI's process identity, and launching first would
// briefly create a duplicate headless process even though SAP resolution
// would later prefer the GUI socket. Lease expiry and PID liveness fence stale
// registrations; reconcile will mark them stale on its next sweep.
func (d *Daemon) hasLiveExternalProject(ctx context.Context, projectID string) (bool, error) {
	project, err := d.Reg.GetProject(projectID)
	if err != nil {
		return false, err
	}
	instances, err := d.Reg.ListExternalInstances()
	if err != nil {
		return false, err
	}
	now := time.Now().UTC()
	for _, instance := range instances {
		if instance.Status != registry.ExternalStatusOpen ||
			(!instance.LeaseExpiresAt.IsZero() && !instance.LeaseExpiresAt.After(now)) ||
			!health.PIDAlive(instance.PID) ||
			externalProjectRoot(instance.ProjectPath) != filepath.Clean(project.RootDir) {
			continue
		}
		if instance.SAPSocketPath != "" && health.SocketResponsive(instance.SAPSocketPath, time.Second) {
			if err := d.handoffDaemonProjectToGUI(ctx, instance.ProjectPath); err != nil {
				return false, err
			}
		}
		return true, nil
	}
	return false, nil
}

// ForwardSAPWithContext applies the registered per-ACP-session MCP target
// before using the ordinary MCP connection binding. The MCP transport's own
// session id remains the routing key for notifications; the opaque context
// token only supplies the durable chat-owner/target policy. This prevents a
// daemon-global active project while allowing a target update to take effect
// on the next tool call.
func (d *Daemon) ForwardSAPWithContext(ctx context.Context, sessionID, contextToken string, sink sapproxy.Sink, method string, params json.RawMessage) (json.RawMessage, error) {
	record, err := d.Reg.GetMcpContext(contextToken)
	if err != nil {
		return nil, fmt.Errorf("daemon: MCP context: %w", err)
	}
	if record.LeaseExpiresAt.Before(time.Now().UTC()) {
		return nil, fmt.Errorf("daemon: MCP context lease expired")
	}
	target := record.TargetProjectID
	if method == "project.select" {
		var requested struct {
			ProjectID string `json:"projectId"`
		}
		if err := unmarshalParams(params, &requested); err != nil {
			return nil, err
		}
		if requested.ProjectID == "" {
			return nil, fmt.Errorf("daemon: project.select: projectId is required")
		}
		if _, err := d.SetMcpProjectTarget(ctx, contextToken, requested.ProjectID); err != nil {
			return nil, err
		}
		target = requested.ProjectID
	}
	if target == "" {
		return nil, fmt.Errorf("daemon: MCP context has no target project")
	}
	if bound, ok := d.SAP.BoundProject(sessionID); ok && bound != target {
		if _, err := d.ForwardSAP(ctx, sessionID, sink, "project.exit", nil); err != nil {
			return nil, err
		}
	}
	if _, ok := d.SAP.BoundProject(sessionID); !ok {
		selectParams, _ := json.Marshal(map[string]string{"projectId": target})
		if _, err := d.ForwardSAP(ctx, sessionID, sink, "project.select", selectParams); err != nil {
			return nil, err
		}
	}
	return d.ForwardSAP(ctx, sessionID, sink, method, params)
}

// UnbindSession releases sessionID's SAP project binding/notification sink
// and expires its session-store entry -- called by the SDP server on
// connection close and by the MCP adapter on session teardown (mcp-go's
// OnUnregisterSession hook).
func (d *Daemon) UnbindSession(sessionID string) {
	d.SAP.Unbind(sessionID)
	_ = d.Sessions.Expire(sessionID)
}

// --- JSON-RPC method dispatch, used by internal/sdp.Server ---

// Dispatch implements sdp.Handler: it decodes params for the named
// daemon.* method, calls the corresponding Go method above, and returns a
// JSON-serializable result (or an error).
func (d *Daemon) Dispatch(ctx context.Context, method string, params json.RawMessage) (any, error) {
	switch method {
	case "daemon.createProject":
		var p CreateProjectParams
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return d.CreateProject(ctx, p)

	case "project.create":
		var p ProjectCreateParams
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		proj, err := d.ProjectCreate(ctx, p)
		if err != nil {
			return nil, err
		}
		return ProjectCreateResult{Project: proj, MltCreated: false}, nil

	case "project.clone":
		var p ProjectCloneParams
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return d.ProjectClone(ctx, p)

	case "project.list":
		var p ProjectListParams
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return d.ProjectList(ctx, p)

	case "daemon.deleteProject":
		var p struct {
			ProjectID string `json:"projectId"`
		}
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return nil, d.DeleteProject(ctx, p.ProjectID)

	case "daemon.listProjects":
		return d.ListProjects(ctx)

	case "daemon.subscribeProjects":
		return d.SubscribeProjects(ctx)

	case "daemon.registerExternalInstance":
		var p RegisterExternalInstanceParams
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return d.RegisterExternalInstance(ctx, p)

	case "daemon.updateOpenProject":
		var p UpdateExternalProjectParams
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return d.UpdateOpenProject(ctx, p)

	case "daemon.heartbeat":
		var p struct {
			InstanceID string `json:"instanceId"`
		}
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return d.HeartbeatExternalInstance(ctx, p.InstanceID)

	case "daemon.unregisterExternalInstance":
		var p struct {
			InstanceID string `json:"instanceId"`
		}
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return nil, d.UnregisterExternalInstance(ctx, p.InstanceID)

	case "daemon.registerMcpContext":
		var p RegisterMcpContextParams
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return d.RegisterMcpContext(ctx, p)

	case "daemon.setMcpProjectTarget":
		var p struct {
			ContextToken string `json:"contextToken"`
			ProjectID    string `json:"projectId"`
		}
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return d.SetMcpProjectTarget(ctx, p.ContextToken, p.ProjectID)

	case "daemon.unregisterMcpContext":
		var p struct {
			ContextToken string `json:"contextToken"`
		}
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return nil, d.UnregisterMcpContext(ctx, p.ContextToken)

	case "daemon.discoverExternalInstances":
		return d.DiscoverExternalInstances(ctx)

	case "daemon.launch":
		var p LaunchParams
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return d.Launch(ctx, p)

	// daemon.clientRegister/clientHeartbeat/clientUnregister/projectOpen/
	// projectClose/projectSwitch: a client-lease-pool RPC surface that
	// was scaffolded (these dispatcher cases plus the ClientLease registry
	// rows) but never got its params types or Daemon methods implemented.
	// Stubbed to a clean error rather than silently dropped, so a caller
	// that hits one of these gets a clear "not implemented" instead of the
	// dispatcher's generic unknown-method error, until the feature is
	// actually finished.
	case "daemon.clientRegister", "daemon.clientHeartbeat", "daemon.clientUnregister",
		"daemon.projectOpen", "daemon.processEnsure",
		"daemon.projectClose", "daemon.processRelease",
		"daemon.projectSwitch":
		return nil, fmt.Errorf("daemon: %s: not implemented", method)

	case "daemon.list":
		var p InstanceListParams
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return d.List(ctx, p)

	case "daemon.health":
		var p struct {
			InstanceID string `json:"instanceId"`
		}
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return d.Health(ctx, p.InstanceID)

	case "daemon.close":
		var p struct {
			InstanceID string `json:"instanceId"`
		}
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		return nil, d.CloseInstance(ctx, p.InstanceID)

	// The daemon.mcp* methods below are SDP/CLI-only by design: they are
	// never registered as MCP tools (see internal/mcpadapter.New's tool
	// list) -- letting an MCP client reconfigure its own listener's bind
	// address or auth over that same connection is a foot-gun, not a
	// feature.
	case "daemon.mcpStatus":
		return d.Mcp.Status(), nil

	case "daemon.mcpRestart":
		var p struct {
			Bind string `json:"bind"`
		}
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		if err := d.Mcp.Restart(ctx, p.Bind); err != nil {
			return nil, err
		}
		return d.Mcp.Status(), nil

	case "daemon.mcpAuthSet":
		var p struct {
			User     string `json:"user"`
			Password string `json:"password"`
		}
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		if err := d.Mcp.SetAuth(ctx, p.User, p.Password); err != nil {
			return nil, err
		}
		return d.Mcp.Status(), nil

	case "daemon.mcpInstallConfig":
		return d.Mcp.InstallConfig(), nil

	// daemon.stop is the cross-platform replacement for signaling the
	// serve process's PID directly: it works identically on every
	// platform, since it rides the same already-open SDP control-socket
	// connection `snapshotd stop` used to confirm the daemon is reachable
	// in the first place, instead of relying on OS signal delivery (which
	// (*os.Process).Signal cannot do for SIGTERM on Windows). Not exposed
	// as an MCP tool -- an MCP client should not be able to shut down the
	// whole daemon it's talking through.
	case "daemon.stop":
		d.RequestStop()
		return map[string]any{"stopping": true}, nil

	default:
		return nil, fmt.Errorf("unknown method %q", method)
	}
}

func unmarshalParams(raw json.RawMessage, out any) error {
	if len(raw) == 0 {
		return nil
	}
	if err := json.Unmarshal(raw, out); err != nil {
		return fmt.Errorf("invalid params: %w", err)
	}
	return nil
}
