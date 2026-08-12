package daemon

import (
	"context"
	"encoding/json"
	"fmt"
	"sync"
	"time"

	"snapshotd/internal/procmgr"
	"snapshotd/internal/registry"
)

// ClientRegisterParams identifies a long-lived panel or MCP client.
type ClientRegisterParams struct {
	ClientID string `json:"clientId"`
}

type ClientHeartbeatParams struct {
	ClientID string `json:"clientId"`
}

// ProjectOpenParams is the lifecycle entry point used by GUI and MCP clients.
// ProjectPath may be used before a project has a registry id.
type ProjectOpenParams struct {
	ClientID    string `json:"clientId"`
	ProjectID   string `json:"projectId,omitempty"`
	ProjectPath string `json:"projectPath,omitempty"`
	Mode        string `json:"mode,omitempty"` // gui or mcp
	Headless    *bool  `json:"headless,omitempty"`
}

type ProjectOpenResult struct {
	ProjectID   string `json:"projectId"`
	ProjectPath string `json:"projectPath"`
	InstanceID  string `json:"instanceId"`
	Generation  int64  `json:"generation"`
	Status      string `json:"status"`
	Reused      bool   `json:"reused"`
	Headless    bool   `json:"headless"`
}

type ProjectCloseParams struct {
	ClientID  string `json:"clientId"`
	ProjectID string `json:"projectId"`
	Save      *bool  `json:"save,omitempty"`
}

type ProjectCloseResult struct {
	Released        bool  `json:"released"`
	ProcessClosed   bool  `json:"processClosed"`
	RemainingLeases int64 `json:"remainingLeases"`
}

type lifecycleSink struct{}

func (lifecycleSink) Notify(string, json.RawMessage) {}

func (d *Daemon) projectLock(key string) *sync.Mutex {
	v, _ := d.projectLocks.LoadOrStore(key, &sync.Mutex{})
	return v.(*sync.Mutex)
}

func (d *Daemon) readyInstance(projectID string) (*registry.ProcessInstance, error) {
	instances, err := d.Reg.ListProcessInstancesByProject(projectID)
	if err != nil {
		return nil, err
	}
	for i := range instances {
		if instances[i].Status != registry.StatusReady {
			continue
		}
		if _, ok, err := d.Proc.Health(instances[i].ID); err == nil && ok {
			return &instances[i], nil
		}
		_ = d.Reg.UpdateProcessInstanceStatus(instances[i].ID, registry.StatusCrashed)
	}
	return nil, nil
}

func (d *Daemon) saveAndClose(ctx context.Context, pi *registry.ProcessInstance) error {
	if pi == nil {
		return nil
	}
	sessionID := "lifecycle-save-" + pi.ID
	if _, err := d.SAP.Bind(ctx, sessionID, pi.ProjectID, lifecycleSink{}); err != nil {
		return fmt.Errorf("daemon: bind for save: %w", err)
	}
	defer d.SAP.Unbind(sessionID)
	if _, err := d.SAP.Call(ctx, sessionID, "project.save", json.RawMessage(`{}`)); err != nil {
		return fmt.Errorf("daemon: save before close: %w", err)
	}
	return d.Proc.Close(pi.ID)
}

func (d *Daemon) ProjectOpen(ctx context.Context, p ProjectOpenParams) (ProjectOpenResult, error) {
	if p.ClientID == "" {
		return ProjectOpenResult{}, fmt.Errorf("daemon: project.open: clientId is required")
	}
	if p.ProjectID == "" && p.ProjectPath == "" {
		return ProjectOpenResult{}, fmt.Errorf("daemon: project.open: projectId or projectPath is required")
	}
	var proj *registry.Project
	var err error
	if p.ProjectID != "" {
		proj, err = d.Reg.GetProject(p.ProjectID)
	} else {
		var resolved registry.Project
		resolved, err = d.resolveOrRegisterProjectByPath(p.ProjectPath)
		proj = &resolved
	}
	if err != nil {
		return ProjectOpenResult{}, err
	}
	lock := d.projectLock(proj.RootDir)
	lock.Lock()
	defer lock.Unlock()

	headless := true
	if p.Headless != nil {
		headless = *p.Headless
	}
	if p.Mode == "gui" {
		headless = false
	}
	pi, err := d.readyInstance(proj.ID)
	if err != nil {
		return ProjectOpenResult{}, err
	}
	reused := pi != nil
	if pi != nil && pi.Headless && !headless {
		if err := d.saveAndClose(ctx, pi); err != nil {
			return ProjectOpenResult{}, err
		}
		pi, reused = nil, false
	}
	if pi == nil {
		launched, _, launchErr := d.Proc.Launch(ctx, proj.ID, procmgr.LaunchOptions{
			Headless: headless, ProjectRoot: proj.RootDir, MltFileName: proj.MltFileName,
			AudioEnabled: d.Cfg.AudioEnabled,
		})
		if launchErr != nil {
			return ProjectOpenResult{}, launchErr
		}
		pi = &launched
	}
	// registry.ProcessInstance (daemon-launched child processes) has no
	// Generation field -- that concept only exists for
	// registry.ExternalInstance (separately launched GUIs, tracked via
	// daemon.go's own RegisterExternalInstanceParams/UpdateExternalInstance
	// flow). There is currently no generation counter for Proc-launched
	// instances, so ClientLease/ProjectOpenResult's Generation is left at
	// its zero value here.
	if err := d.Reg.UpsertClientLease(&registry.ClientLease{
		ClientID: p.ClientID, ProjectID: proj.ID, InstanceID: pi.ID,
		Mode: p.Mode,
	}); err != nil {
		return ProjectOpenResult{}, err
	}
	_ = d.Reg.TouchProjectOpened(proj.ID)
	return ProjectOpenResult{ProjectID: proj.ID, ProjectPath: proj.RootDir, InstanceID: pi.ID,
		Status: pi.Status, Reused: reused, Headless: pi.Headless}, nil
}

func (d *Daemon) ProjectClose(ctx context.Context, p ProjectCloseParams) (ProjectCloseResult, error) {
	if p.ClientID == "" || p.ProjectID == "" {
		return ProjectCloseResult{}, fmt.Errorf("daemon: project.close: clientId and projectId are required")
	}
	lease, err := d.Reg.GetClientLease(p.ClientID, p.ProjectID)
	if err != nil {
		return ProjectCloseResult{}, err
	}
	lock := d.projectLock(p.ProjectID)
	lock.Lock()
	defer lock.Unlock()
	if err := d.Reg.DeleteClientLease(p.ClientID, p.ProjectID); err != nil {
		return ProjectCloseResult{}, err
	}
	remaining, err := d.Reg.CountInstanceLeases(lease.InstanceID)
	if err != nil {
		return ProjectCloseResult{}, err
	}
	result := ProjectCloseResult{Released: true, RemainingLeases: remaining}
	if remaining == 0 {
		pi, getErr := d.Reg.GetProcessInstance(lease.InstanceID)
		if getErr == nil {
			save := true
			if p.Save != nil {
				save = *p.Save
			}
			if save {
				err = d.saveAndClose(ctx, pi)
			} else {
				err = d.Proc.Close(pi.ID)
			}
			if err != nil {
				return ProjectCloseResult{}, err
			}
			result.ProcessClosed = true
		}
	}
	return result, nil
}

func (d *Daemon) ClientHeartbeat(ctx context.Context, p ClientHeartbeatParams) error {
	if p.ClientID == "" {
		return fmt.Errorf("daemon: client.heartbeat: clientId is required")
	}
	return d.Reg.TouchClientLeases(p.ClientID)
}

func (d *Daemon) ClientUnregister(ctx context.Context, p ClientRegisterParams) error {
	if p.ClientID == "" {
		return fmt.Errorf("daemon: client.unregister: clientId is required")
	}
	return d.Reg.DeleteClientLeases(p.ClientID)
}

func (d *Daemon) ProjectSwitch(ctx context.Context, p struct {
	ClientID      string            `json:"clientId"`
	FromProjectID string            `json:"fromProjectId"`
	To            ProjectOpenParams `json:"to"`
}) (ProjectOpenResult, error) {
	if p.FromProjectID != "" {
		closeCtx, cancel := context.WithTimeout(ctx, 30*time.Second)
		defer cancel()
		if _, err := d.ProjectClose(closeCtx, ProjectCloseParams{ClientID: p.ClientID, ProjectID: p.FromProjectID}); err != nil && !isNotFound(err) {
			return ProjectOpenResult{}, err
		}
	}
	p.To.ClientID = p.ClientID
	return d.ProjectOpen(ctx, p.To)
}

func isNotFound(err error) bool { return err != nil && (err == registry.ErrNotFound) }
