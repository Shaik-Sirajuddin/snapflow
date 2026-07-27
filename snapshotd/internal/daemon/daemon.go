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
	Capabilities  map[string]any `json:"capabilities,omitempty"`
}

type ExternalInstanceResult struct {
	Instance       registry.ExternalInstance `json:"instance"`
	HeartbeatEvery time.Duration             `json:"heartbeatEvery"`
	LeaseDuration  time.Duration             `json:"leaseDuration"`
}

func (d *Daemon) RegisterExternalInstance(ctx context.Context, p RegisterExternalInstanceParams) (ExternalInstanceResult, error) {
	if strings.TrimSpace(p.InstanceNonce) == "" || p.PID <= 0 || strings.TrimSpace(p.ProcessStart) == "" {
		return ExternalInstanceResult{}, fmt.Errorf("daemon: registerExternalInstance: instanceNonce, pid, and processStart are required")
	}
	if !health.ProcessIdentityMatches(p.PID, p.ProcessStart) {
		return ExternalInstanceResult{}, fmt.Errorf("daemon: registerExternalInstance: pid/processStart identity does not match a live process")
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
	return discovery.ScanAndPing(filepath.Join(d.Cfg.HomeDir, "apps"))
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
	if cfg.LaunchConnectTimeout > 0 {
		pm.ConnectTimeout = cfg.LaunchConnectTimeout
	}
	d := &Daemon{
		Cfg:      cfg,
		Reg:      reg,
		Sessions: session.NewMemory(30 * time.Second),
		Proc:     pm,
		Log:      logger,
		stopCh:   make(chan struct{}),
	}
	d.SAP = sapproxy.NewRouter(d.resolveProjectInstance)
	d.Mcp = mcpsupervisor.New(d, cfg.HomeDir, cfg.MCPSSEAddr, logger)
	return d, nil
}

// resolveProjectInstance implements sapproxy.Resolver: it finds the most
// recently launched "ready" ProcessInstance for a project and returns the
// socket path + per-launch token a new SAP connection should present to
// sap.hello -- exactly what a direct SAP client would need to look up
// itself to connect to that project's running instance.
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
	return "", "", fmt.Errorf("daemon: no running (ready) process instance for project %s; call daemon.launch first", projectID)
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
	if p.Name == "" {
		return registry.Project{}, fmt.Errorf("daemon: createProject: name is required")
	}
	root := filepath.Join(d.Cfg.ProjectsRoot, p.Name)
	if err := os.MkdirAll(root, 0o755); err != nil {
		return registry.Project{}, fmt.Errorf("daemon: createProject: mkdir %s: %w", root, err)
	}
	proj := registry.Project{
		ID:          uuid.NewString(),
		RootDir:     root,
		MltFileName: registry.DefaultMltFileName,
		Status:      "active",
	}
	if err := d.Reg.CreateProject(&proj); err != nil {
		return registry.Project{}, err
	}
	_ = d.Reg.Audit(proj.ID, registry.AuditCreate, "created project folder "+root)
	return proj, nil
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

// ListProjects implements daemon.listProjects.
func (d *Daemon) ListProjects(ctx context.Context) ([]registry.Project, error) {
	return d.Reg.ListProjects()
}

// ProjectSubscription is the v1 control-plane fallback for clients that
// cannot keep a notification stream. It returns an authoritative snapshot
// and a bounded poll interval; a future multiplexed SDP connection can
// upgrade the same method to push deltas without changing the payload.
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
	return ProjectSubscription{Projects: projects, Mode: "poll", PollAfter: 5 * time.Second}, nil
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
// daemon.launch: find an existing Project by RootDir, or register a new one,
// per 09-project-folder-layout.md's two root-resolution rules (a directory
// is the root directly; a bare .mlt file's parent directory is the root).
func (d *Daemon) resolveOrRegisterProjectByPath(path string) (registry.Project, error) {
	abs, err := filepath.Abs(path)
	if err != nil {
		return registry.Project{}, fmt.Errorf("daemon: resolving path %s: %w", path, err)
	}
	info, err := os.Stat(abs)
	if err != nil {
		return registry.Project{}, fmt.Errorf("daemon: launch: project path %s: %w", abs, err)
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
		Status:      "active",
	}
	if err := d.Reg.CreateProject(&proj); err != nil {
		return registry.Project{}, err
	}
	_ = d.Reg.Audit(proj.ID, registry.AuditCreate, "registered from launch path "+rootDir)
	return proj, nil
}

// List implements daemon.list (list of running/known process instances).
func (d *Daemon) List(ctx context.Context) ([]registry.ProcessInstance, error) {
	return d.Proc.List()
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

// CloseInstance implements daemon.close: stop a running process instance.
// (Named CloseInstance, not Close, since Daemon.Close already exists for the
// daemon's own lifecycle/resource shutdown -- Go has no overloading.)
func (d *Daemon) CloseInstance(ctx context.Context, instanceID string) error {
	return d.Proc.Close(instanceID)
}

// --- Generic SAP proxy, per 06-daemon-mcp-proxy.md's proxy requirement ---

// proxySessionTTL is how long an SDP/MCP session's project binding survives
// without an intervening call, per 07's session-TTL model applied to this
// proxy's own session bookkeeping (separate from sap-rust's own connection
// lifetime, which is pooled per-project, not per-session -- see
// internal/sapproxy).
const proxySessionTTL = 10 * time.Minute

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

	if method == "project.select" {
		var p struct {
			ProjectID string `json:"projectId"`
		}
		if err := unmarshalParams(params, &p); err != nil {
			logMethod(err)
			return nil, err
		}
		if p.ProjectID == "" {
			err := fmt.Errorf("daemon: project.select: projectId is required")
			logMethod(err)
			return nil, err
		}
		if _, err := d.Reg.GetProject(p.ProjectID); err != nil {
			err = fmt.Errorf("daemon: project.select: %w", err)
			logMethod(err, "projectId", p.ProjectID)
			return nil, err
		}
		result, err := d.SAP.Bind(ctx, sessionID, p.ProjectID, sink)
		if err != nil {
			logMethod(err, "projectId", p.ProjectID)
			return nil, err
		}
		_ = d.Sessions.BindProject(sessionID, p.ProjectID)
		logMethod(nil, "projectId", p.ProjectID)
		return result, nil
	}

	if method == "project.exit" {
		// Deliberately NOT forwarded to sap-rust: internal/sapproxy pools one
		// SAP connection per project, shared by every session bound to that
		// project, and sap-rust's own project.select gate lives on that one
		// shared connection (see sap-rust/src/server.rs's per-connection
		// `session.project_id`), not per Go-level session. Forwarding a raw
		// "project.exit" through the shared connection would unselect the
		// project for every OTHER session still bound to it too. "Exit" is
		// therefore purely local bookkeeping: it clears this session's own
		// Router binding (sapproxy.Router.Unbind) so a later project.select
		// -- possibly to a different project -- is no longer rejected by
		// Bind's already-bound guard. This matches sap-rust's own
		// project.exit being harmless/idempotent when called while unbound.
		d.SAP.Unbind(sessionID)
		_ = d.Sessions.BindProject(sessionID, "")
		logMethod(nil)
		return json.RawMessage(`{}`), nil
	}

	result, err := d.SAP.Call(ctx, sessionID, method, params)
	logMethod(err)
	return result, err
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

	case "daemon.list":
		return d.List(ctx)

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
