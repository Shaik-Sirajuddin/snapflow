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
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"time"

	"github.com/google/uuid"

	"snapshotd/internal/config"
	"snapshotd/internal/health"
	"snapshotd/internal/procmgr"
	"snapshotd/internal/registry"
	"snapshotd/internal/sapproxy"
	"snapshotd/internal/session"
)

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
	pm := procmgr.New(reg, cfg.SnapshotBinPath, cfg.RunDir)
	d := &Daemon{
		Cfg:      cfg,
		Reg:      reg,
		Sessions: session.NewMemory(30 * time.Second),
		Proc:     pm,
		Log:      logger,
	}
	d.SAP = sapproxy.NewRouter(d.resolveProjectInstance)
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

func (d *Daemon) Launch(ctx context.Context, p LaunchParams) (registry.ProcessInstance, error) {
	projectID := p.ProjectID
	if projectID == "" {
		if p.ProjectPath == "" {
			return registry.ProcessInstance{}, fmt.Errorf("daemon: launch: one of projectId or projectPath is required")
		}
		proj, err := d.resolveOrRegisterProjectByPath(p.ProjectPath)
		if err != nil {
			return registry.ProcessInstance{}, err
		}
		projectID = proj.ID
	}
	proj, err := d.Reg.GetProject(projectID)
	if err != nil {
		return registry.ProcessInstance{}, fmt.Errorf("daemon: launch: %w", err)
	}
	headless := true
	if p.Headless != nil {
		headless = *p.Headless
	}
	return d.Proc.Launch(ctx, projectID, procmgr.LaunchOptions{
		Headless:     headless,
		ProjectRoot:  proj.RootDir,
		AudioEnabled: d.Cfg.AudioEnabled,
	})
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

	if method == "project.select" {
		var p struct {
			ProjectID string `json:"projectId"`
		}
		if err := unmarshalParams(params, &p); err != nil {
			return nil, err
		}
		if p.ProjectID == "" {
			return nil, fmt.Errorf("daemon: project.select: projectId is required")
		}
		if _, err := d.Reg.GetProject(p.ProjectID); err != nil {
			return nil, fmt.Errorf("daemon: project.select: %w", err)
		}
		result, err := d.SAP.Bind(ctx, sessionID, p.ProjectID, sink)
		if err != nil {
			return nil, err
		}
		_ = d.Sessions.BindProject(sessionID, p.ProjectID)
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
		return json.RawMessage(`{}`), nil
	}

	return d.SAP.Call(ctx, sessionID, method, params)
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
