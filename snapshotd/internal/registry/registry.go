package registry

import (
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"time"

	"github.com/glebarez/sqlite"
	"github.com/google/uuid"
	"gorm.io/gorm"
	"gorm.io/gorm/logger"
)

// ErrNotFound is returned by lookups that find no matching row.
var ErrNotFound = errors.New("registry: not found")

// ErrProjectAlreadyExists is returned by project.create when the path is
// already registered or already exists on disk. Distinct from project.open,
// which attaches to whatever is already there.
var ErrProjectAlreadyExists = errors.New("registry: project already exists")

// Registry wraps a GORM database handle and exposes the daemon's persistence
// operations. It is safe for concurrent use (GORM/database/sql pool their own
// connections internally).
type Registry struct {
	db *gorm.DB
}

// Open opens (creating if necessary) the SQLite file at path and runs
// AutoMigrate for all registry models, per 07-daemon-persistence.md.
//
// Before touching an existing file, backs it up (bounded to one previous
// copy, same pattern as scripts/install.sh's own install-dir backup) --
// AutoMigrate's SQLite path can rebuild a table (create new, copy rows,
// drop old, rename) for column type/constraint changes, which is not
// crash-atomic; a hard kill or OOM mid-migration on an app upgrade would
// otherwise have nothing to fall back to. Also enables WAL journal mode
// (more crash-resistant than the default rollback-journal mode) and a
// real busy_timeout (the default is 0, meaning any concurrent access --
// however unlikely today -- fails immediately with "database is locked"
// instead of waiting, which could otherwise look like a dropped write).
func Open(path string) (*Registry, error) {
	if err := backupIfExists(path); err != nil {
		return nil, fmt.Errorf("registry: backup before open: %w", err)
	}

	dsn := path + "?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)"
	db, err := gorm.Open(sqlite.Open(dsn), &gorm.Config{
		Logger: logger.Default.LogMode(logger.Silent),
	})
	if err != nil {
		return nil, fmt.Errorf("registry: open %s: %w", path, err)
	}
	if err := db.AutoMigrate(&Project{}, &ProcessInstance{}, &ExternalInstance{}, &ProjectActiveOwner{}, &PendingProjectCandidate{}, &McpContext{}, &ClientLease{}, &AuditEvent{}); err != nil {
		return nil, fmt.Errorf("registry: automigrate: %w", err)
	}
	return &Registry{db: db}, nil
}

// backupIfExists copies path (and its WAL/SHM sidecar files, if present --
// a WAL-mode database's most recent writes can live in -wal until the next
// checkpoint, so a backup that skipped it could silently omit real data)
// to a single bounded ".prev" backup before any migration touches the
// original. A no-op for a fresh install (nothing to back up yet).
func backupIfExists(path string) error {
	if _, err := os.Stat(path); os.IsNotExist(err) {
		return nil
	} else if err != nil {
		return err
	}
	for _, suffix := range []string{"", "-wal", "-shm"} {
		src := path + suffix
		if _, err := os.Stat(src); os.IsNotExist(err) {
			continue
		} else if err != nil {
			return err
		}
		if err := copyFile(src, src+".prev"); err != nil {
			return fmt.Errorf("backing up %s: %w", src, err)
		}
	}
	return nil
}

func copyFile(src, dst string) error {
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	// O_TRUNC: overwrite any older .prev rather than accumulating an
	// unbounded backup history.
	out, err := os.OpenFile(dst, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o600)
	if err != nil {
		return err
	}
	defer out.Close()
	if _, err := io.Copy(out, in); err != nil {
		return err
	}
	return out.Sync()
}

// DB exposes the underlying *gorm.DB for callers (e.g. tests) that need
// lower-level access.
func (r *Registry) DB() *gorm.DB { return r.db }

// Close releases the underlying database connection.
func (r *Registry) Close() error {
	sqlDB, err := r.db.DB()
	if err != nil {
		return err
	}
	return sqlDB.Close()
}

// GetProjectActiveOwner returns the durable owner marker for a project.
func (r *Registry) GetProjectActiveOwner(projectID string) (*ProjectActiveOwner, error) {
	var owner ProjectActiveOwner
	if err := r.db.First(&owner, "project_id = ?", projectID).Error; err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, ErrNotFound
		}
		return nil, err
	}
	return &owner, nil
}

func (r *Registry) SaveProjectActiveOwner(owner *ProjectActiveOwner) error {
	return r.db.Save(owner).Error
}

// ClaimProjectOwner serializes active-marker selection with pending retention.
// The project primary key makes the transaction authoritative when multiple
// GUI callbacks race to become the first owner; only the loser is queued.
func (r *Registry) ClaimProjectOwner(candidate PendingProjectCandidate, now time.Time) (bool, *ProjectActiveOwner, error) {
	var pending bool
	var active ProjectActiveOwner
	var found bool
	err := r.db.Transaction(func(tx *gorm.DB) error {
		result := tx.First(&active, "project_id = ?", candidate.ProjectID)
		if result.Error != nil && !errors.Is(result.Error, gorm.ErrRecordNotFound) {
			return result.Error
		}
		found = result.Error == nil
		if !found {
			// A successful switch clears this identity's old marker only after
			// this transaction has established the new active owner.
			q := tx.Where("owner = ? AND instance_id = ? AND process_start = ? AND project_id <> ?", candidate.Owner, candidate.InstanceID, candidate.ProcessStart, candidate.ProjectID)
			if candidate.InstanceNonce != "" {
				q = q.Where("instance_nonce = ?", candidate.InstanceNonce)
			}
			if err := q.Delete(&ProjectActiveOwner{}).Error; err != nil {
				return err
			}
			active = ProjectActiveOwner{ProjectID: candidate.ProjectID, Owner: candidate.Owner,
				InstanceID: candidate.InstanceID, InstanceNonce: candidate.InstanceNonce,
				PID: candidate.PID, ProcessStart: candidate.ProcessStart, Generation: candidate.Generation,
				LastSeenAt: now, LeaseExpiresAt: candidate.LeaseExpiresAt, UpdatedAt: now}
			return tx.Create(&active).Error
		}
		same := active.Owner == candidate.Owner && active.InstanceID == candidate.InstanceID &&
			active.InstanceNonce == candidate.InstanceNonce && active.PID == candidate.PID && active.ProcessStart == candidate.ProcessStart
		if same {
			active.Generation = maxGeneration(active.Generation, candidate.Generation)
			active.LastSeenAt, active.LeaseExpiresAt, active.UpdatedAt = now, candidate.LeaseExpiresAt, now
			return tx.Save(&active).Error
		}
		pending = true
		// A switch away from another project relinquishes this identity's old
		// active marker even when the requested project is currently owned by a
		// different identity. Keep the new request pending, but never leave the
		// instance active for two projects at once.
		q := tx.Where("owner = ? AND instance_id = ? AND process_start = ? AND project_id <> ?", candidate.Owner, candidate.InstanceID, candidate.ProcessStart, candidate.ProjectID)
		if candidate.InstanceNonce != "" {
			q = q.Where("instance_nonce = ?", candidate.InstanceNonce)
		}
		if err := q.Delete(&ProjectActiveOwner{}).Error; err != nil {
			return err
		}
		pq := tx.Model(&PendingProjectCandidate{}).Where("owner = ? AND instance_id = ? AND process_start = ? AND project_id <> ? AND status = ?", candidate.Owner, candidate.InstanceID, candidate.ProcessStart, candidate.ProjectID, PendingStatus)
		if candidate.InstanceNonce != "" {
			pq = pq.Where("instance_nonce = ?", candidate.InstanceNonce)
		}
		if err := pq.Updates(map[string]any{"status": StaleStatus, "updated_at": now}).Error; err != nil {
			return err
		}
		var existing PendingProjectCandidate
		lookup := tx.Where("project_id = ? AND owner = ? AND instance_id = ? AND instance_nonce = ? AND process_start = ?", candidate.ProjectID, candidate.Owner, candidate.InstanceID, candidate.InstanceNonce, candidate.ProcessStart).First(&existing)
		if lookup.Error == nil {
			candidate.ID, candidate.CreatedAt = existing.ID, existing.CreatedAt
		} else if !errors.Is(lookup.Error, gorm.ErrRecordNotFound) {
			return lookup.Error
		}
		if candidate.ID == "" {
			candidate.ID = uuid.NewString()
		}
		if err := tx.Save(&candidate).Error; err != nil {
			return err
		}
		var queued []PendingProjectCandidate
		if err := tx.Where("project_id = ? AND status = ?", candidate.ProjectID, PendingStatus).Order("created_at DESC").Find(&queued).Error; err != nil {
			return err
		}
		for _, stale := range queued[minInt(len(queued), MaxPendingCandidatesPerProject):] {
			if err := tx.Model(&stale).Update("status", StaleStatus).Error; err != nil {
				return err
			}
		}
		return nil
	})
	if err != nil {
		return false, nil, err
	}
	return pending, &active, nil
}

func maxGeneration(a, b uint64) uint64 {
	if a > b {
		return a
	}
	return b
}

func minInt(a, b int) int {
	if a < b {
		return a
	}
	return b
}

// ReleaseProjectOwnership removes active markers held by an instance for
// projects other than keepProjectID. A GUI switch therefore cannot leave its
// previous project permanently owned after claiming the new one.
func (r *Registry) ReleaseProjectOwnership(owner, instanceID, nonce, processStart, keepProjectID string) error {
	q := r.db.Where("owner = ? AND instance_id = ? AND process_start = ? AND project_id <> ?", owner, instanceID, processStart, keepProjectID)
	if nonce != "" {
		q = q.Where("instance_nonce = ?", nonce)
	}
	if err := q.Delete(&ProjectActiveOwner{}).Error; err != nil {
		return err
	}
	pq := r.db.Model(&PendingProjectCandidate{}).Where("owner = ? AND instance_id = ? AND process_start = ? AND status = ?", owner, instanceID, processStart, PendingStatus)
	if nonce != "" {
		pq = pq.Where("instance_nonce = ?", nonce)
	}
	return pq.Updates(map[string]any{"status": StaleStatus, "updated_at": time.Now().UTC()}).Error
}

// UpsertPendingProjectCandidate retains a conflicting lifecycle identity.
// Repeated callbacks refresh the same candidate instead of growing rows.
func (r *Registry) UpsertPendingProjectCandidate(candidate *PendingProjectCandidate) error {
	var existing PendingProjectCandidate
	err := r.db.Where("project_id = ? AND owner = ? AND instance_id = ? AND instance_nonce = ? AND process_start = ?", candidate.ProjectID, candidate.Owner, candidate.InstanceID, candidate.InstanceNonce, candidate.ProcessStart).First(&existing).Error
	if err == nil {
		candidate.ID = existing.ID
		candidate.CreatedAt = existing.CreatedAt
	} else if !errors.Is(err, gorm.ErrRecordNotFound) {
		return err
	}
	if candidate.ID == "" {
		candidate.ID = uuid.NewString()
	}
	if err := r.db.Save(candidate).Error; err != nil {
		return err
	}
	var pending []PendingProjectCandidate
	if err := r.db.Where("project_id = ? AND status = ?", candidate.ProjectID, PendingStatus).Order("created_at DESC").Find(&pending).Error; err != nil {
		return err
	}
	if len(pending) <= MaxPendingCandidatesPerProject {
		return nil
	}
	for _, stale := range pending[MaxPendingCandidatesPerProject:] {
		if err := r.db.Model(&stale).Update("status", StaleStatus).Error; err != nil {
			return err
		}
	}
	return nil
}

// ListPendingProjectCandidates returns newest pending identities first.
func (r *Registry) ListPendingProjectCandidates(projectID string) ([]PendingProjectCandidate, error) {
	var out []PendingProjectCandidate
	err := r.db.Where("project_id = ? AND status = ?", projectID, PendingStatus).
		Order("generation DESC, created_at DESC").Find(&out).Error
	return out, err
}

// PromoteProjectCandidate atomically replaces the active marker and retires
// the candidate. Callers must validate liveness before invoking this method.
func (r *Registry) PromoteProjectCandidate(projectID string, candidate *PendingProjectCandidate, now time.Time) error {
	return r.db.Transaction(func(tx *gorm.DB) error {
		owner := ProjectActiveOwner{ProjectID: projectID, Owner: candidate.Owner,
			InstanceID: candidate.InstanceID, InstanceNonce: candidate.InstanceNonce,
			PID: candidate.PID, ProcessStart: candidate.ProcessStart,
			Generation: candidate.Generation, LastSeenAt: now,
			LeaseExpiresAt: candidate.LeaseExpiresAt, UpdatedAt: now}
		if err := tx.Save(&owner).Error; err != nil {
			return err
		}
		if err := tx.Model(&PendingProjectCandidate{}).Where("id = ?", candidate.ID).
			Updates(map[string]any{"status": PromotedStatus, "updated_at": now}).Error; err != nil {
			return err
		}
		return nil
	})
}

// --- Project operations ---

func (r *Registry) CreateProject(p *Project) error {
	if p.MltFileName == "" {
		p.MltFileName = DefaultMltFileName
	}
	if p.Status == "" {
		p.Status = "active"
	}
	if p.ProjectType == "" {
		p.ProjectType = ProjectTypeFolder
	}
	now := time.Now().UTC()
	if p.CreatedAt.IsZero() {
		p.CreatedAt = now
	}
	if p.LastOpenedAt.IsZero() {
		p.LastOpenedAt = now
	}
	return r.db.Create(p).Error
}

// UpdateProjectType persists the authoritative file|folder type after open.
func (r *Registry) UpdateProjectType(id, projectType string) error {
	if projectType != ProjectTypeFolder && projectType != ProjectTypeFile {
		return fmt.Errorf("registry: invalid projectType %q", projectType)
	}
	return r.db.Model(&Project{}).Where("id = ?", id).Update("project_type", projectType).Error
}

// FillProjectPathFields sets Path and ProjectID on a Project for path-first APIs.
func FillProjectPathFields(p *Project) {
	if p == nil {
		return
	}
	p.ProjectID = p.ID
	if p.ProjectType == ProjectTypeFile {
		p.Path = filepath.Join(p.RootDir, p.MltFileName)
	} else {
		p.Path = p.RootDir
	}
}

func (r *Registry) GetProject(id string) (*Project, error) {
	var p Project
	if err := r.db.First(&p, "id = ?", id).Error; err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, ErrNotFound
		}
		return nil, err
	}
	return &p, nil
}

// EnsureProjectForPath makes an externally opened MLT visible in the same
// project inventory used by daemon-launched instances. It never creates or
// deletes files; it only derives the registry folder/file identity.
func (r *Registry) EnsureProjectForPath(projectPath string) (*Project, error) {
	abs, err := filepath.Abs(projectPath)
	if err != nil {
		return nil, err
	}
	abs = filepath.Clean(abs)
	if resolved, err := filepath.EvalSymlinks(abs); err == nil {
		abs = filepath.Clean(resolved)
	}
	root := filepath.Dir(abs)
	fileName := filepath.Base(abs)
	if filepath.Ext(abs) == "" {
		root = abs
		fileName = DefaultMltFileName
	}
	var existing Project
	if err := r.db.Where("root_dir = ?", root).First(&existing).Error; err == nil {
		if explicitFile && fileName != existing.MltFileName {
			if err := r.db.Model(&Project{}).Where("id = ?", existing.ID).
				Update("mlt_file_name", fileName).Error; err != nil {
				return nil, err
			}
			existing.MltFileName = fileName
		}
		return &existing, nil
	} else if !errors.Is(err, gorm.ErrRecordNotFound) {
		return nil, err
	}
	project := &Project{
		ID: uuid.NewString(), RootDir: root, MltFileName: fileName, Status: "active",
	}
	if err := r.CreateProject(project); err != nil {
		return nil, err
	}
	return project, nil
}

func (r *Registry) ListProjects() ([]Project, error) {
	var out []Project
	if err := r.db.Order("created_at asc").Find(&out).Error; err != nil {
		return nil, err
	}
	instances, err := r.ListExternalInstances()
	if err != nil {
		return nil, err
	}
	// Newest process instance per project (for active/isOpen marker).
	allPI, err := r.ListProcessInstances()
	if err != nil {
		return nil, err
	}
	newestPI := map[string]ProcessInstance{}
	for i := len(allPI) - 1; i >= 0; i-- { // ListProcessInstances is started_at asc
		pi := allPI[i]
		if _, ok := newestPI[pi.ProjectID]; !ok {
			newestPI[pi.ProjectID] = pi
		}
	}
	for i := range out {
		projectRoot := filepath.Clean(out[i].RootDir)
		for _, instance := range instances {
			if instance.ProjectPath == "" {
				continue
			}
			instanceRoot := filepath.Clean(instance.ProjectPath)
			if filepath.Ext(instanceRoot) == ".mlt" {
				instanceRoot = filepath.Dir(instanceRoot)
			} else if info, statErr := os.Stat(instanceRoot); statErr == nil && !info.IsDir() {
				instanceRoot = filepath.Dir(instanceRoot)
			}
			if instanceRoot != projectRoot {
				continue
			}
			out[i].InstanceCount++
			if instance.Status == ExternalStatusOpen && instance.LeaseExpiresAt.After(time.Now().UTC()) {
				out[i].Open = true
				out[i].Active = true
				out[i].IsOpen = true
				out[i].DiscoveryState = "registered"
			}
			if instance.LastSeenAt.After(out[i].LastSeenAt) {
				out[i].LastSeenAt = instance.LastSeenAt
			}
		}
		if out[i].DiscoveryState == "" {
			out[i].DiscoveryState = "known"
		}
		// active/isOpen: most recent ProcessInstance Status == ready (DB-only).
		// Also fold daemon-launched ready instances into open/instanceCount so
		// list aggregates cover both external leases and owned children.
		if pi, ok := newestPI[out[i].ID]; ok && pi.Status == StatusReady {
			out[i].Active = true
			out[i].IsOpen = true
			out[i].Open = true
			out[i].InstanceCount++
			out[i].DiscoveryState = "registered"
			if pi.LastHealthCheckAt.After(out[i].LastSeenAt) {
				out[i].LastSeenAt = pi.LastHealthCheckAt
			}
		}
		FillProjectPathFields(&out[i])
	}
	return out, nil
}

// GetProjectByRootDir returns the project registered at rootDir, or ErrNotFound.
func (r *Registry) GetProjectByRootDir(rootDir string) (*Project, error) {
	var p Project
	if err := r.db.Where("root_dir = ?", rootDir).First(&p).Error; err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, ErrNotFound
		}
		return nil, err
	}
	FillProjectPathFields(&p)
	return &p, nil
}

func (r *Registry) DeleteProject(id string) error {
	res := r.db.Delete(&Project{}, "id = ?", id)
	if res.Error != nil {
		return res.Error
	}
	if res.RowsAffected == 0 {
		return ErrNotFound
	}
	return nil
}

func (r *Registry) TouchProjectOpened(id string) error {
	return r.db.Model(&Project{}).Where("id = ?", id).Update("last_opened_at", time.Now().UTC()).Error
}

// --- ProcessInstance operations ---

func (r *Registry) CreateProcessInstance(pi *ProcessInstance) error {
	if pi.StartedAt.IsZero() {
		pi.StartedAt = time.Now().UTC()
	}
	if pi.LastHealthCheckAt.IsZero() {
		pi.LastHealthCheckAt = pi.StartedAt
	}
	return r.db.Create(pi).Error
}

func (r *Registry) GetProcessInstance(id string) (*ProcessInstance, error) {
	var pi ProcessInstance
	if err := r.db.First(&pi, "id = ?", id).Error; err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, ErrNotFound
		}
		return nil, err
	}
	return &pi, nil
}

// ListByStatus returns all ProcessInstance rows with the given status, used
// by the startup reconciliation sweep (status = "ready").
func (r *Registry) ListByStatus(status string) ([]ProcessInstance, error) {
	var out []ProcessInstance
	if err := r.db.Where("status = ?", status).Find(&out).Error; err != nil {
		return nil, err
	}
	return out, nil
}

func (r *Registry) ListProcessInstances() ([]ProcessInstance, error) {
	var out []ProcessInstance
	if err := r.db.Order("started_at asc").Find(&out).Error; err != nil {
		return nil, err
	}
	return out, nil
}

// ListProcessInstancesByProject returns all instances for a project, newest first.
func (r *Registry) ListProcessInstancesByProject(projectID string) ([]ProcessInstance, error) {
	var out []ProcessInstance
	if err := r.db.Where("project_id = ?", projectID).Order("started_at desc").Find(&out).Error; err != nil {
		return nil, err
	}
	return out, nil
}

func (r *Registry) UpdateProcessInstanceProject(id, projectID string, generation uint64) error {
	return r.db.Model(&ProcessInstance{}).Where("id = ?", id).Updates(map[string]any{
		"project_id": projectID, "generation": generation,
		"last_health_check_at": time.Now().UTC(),
	}).Error
}

func (r *Registry) UpdateProcessInstanceStatus(id, status string) error {
	return r.db.Model(&ProcessInstance{}).Where("id = ?", id).Updates(map[string]any{
		"status":               status,
		"last_health_check_at": time.Now().UTC(),
	}).Error
}

func (r *Registry) TouchHealthCheck(id string) error {
	return r.db.Model(&ProcessInstance{}).Where("id = ?", id).Update("last_health_check_at", time.Now().UTC()).Error
}

// GetExternalInstanceByNonce returns the one registration owned by a GUI
// instance nonce, allowing reconnect/retry to be idempotent.
func (r *Registry) GetExternalInstanceByNonce(nonce string) (*ExternalInstance, error) {
	var instance ExternalInstance
	if err := r.db.First(&instance, "instance_nonce = ?", nonce).Error; err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, ErrNotFound
		}
		return nil, err
	}
	return &instance, nil
}

func (r *Registry) GetExternalInstance(id string) (*ExternalInstance, error) {
	var instance ExternalInstance
	if err := r.db.First(&instance, "id = ?", id).Error; err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, ErrNotFound
		}
		return nil, err
	}
	return &instance, nil
}

func (r *Registry) ListExternalInstances() ([]ExternalInstance, error) {
	var out []ExternalInstance
	if err := r.db.Order("updated_at desc").Find(&out).Error; err != nil {
		return nil, err
	}
	return out, nil
}

func (r *Registry) SaveExternalInstance(instance *ExternalInstance) error {
	return r.db.Save(instance).Error
}

func (r *Registry) GetMcpContext(token string) (*McpContext, error) {
	var context McpContext
	if err := r.db.First(&context, "context_token = ?", token).Error; err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, ErrNotFound
		}
		return nil, err
	}
	return &context, nil
}

func (r *Registry) SaveMcpContext(context *McpContext) error {
	return r.db.Save(context).Error
}

func (r *Registry) ListMcpContexts() ([]McpContext, error) {
	var out []McpContext
	if err := r.db.Find(&out).Error; err != nil {
		return nil, err
	}
	return out, nil
}

func (r *Registry) DeleteMcpContext(token string) error {
	result := r.db.Delete(&McpContext{}, "context_token = ?", token)
	if result.Error != nil {
		return result.Error
	}
	if result.RowsAffected == 0 {
		return ErrNotFound
	}
	return nil
}

func (r *Registry) UpsertClientLease(lease *ClientLease) error {
	var existing ClientLease
	err := r.db.Where("client_id = ? AND project_id = ?", lease.ClientID, lease.ProjectID).First(&existing).Error
	if errors.Is(err, gorm.ErrRecordNotFound) {
		if lease.ID == "" {
			lease.ID = fmt.Sprintf("%s:%s", lease.ClientID, lease.ProjectID)
		}
		if lease.LastHeartbeat.IsZero() {
			lease.LastHeartbeat = time.Now().UTC()
		}
		return r.db.Create(lease).Error
	}
	if err != nil {
		return err
	}
	return r.db.Model(&existing).Updates(map[string]any{
		"instance_id":    lease.InstanceID,
		"generation":     lease.Generation,
		"mode":           lease.Mode,
		"last_heartbeat": time.Now().UTC(),
	}).Error
}

func (r *Registry) GetClientLease(clientID, projectID string) (*ClientLease, error) {
	var lease ClientLease
	if err := r.db.Where("client_id = ? AND project_id = ?", clientID, projectID).First(&lease).Error; err != nil {
		if errors.Is(err, gorm.ErrRecordNotFound) {
			return nil, ErrNotFound
		}
		return nil, err
	}
	return &lease, nil
}

func (r *Registry) DeleteClientLease(clientID, projectID string) error {
	return r.db.Where("client_id = ? AND project_id = ?", clientID, projectID).Delete(&ClientLease{}).Error
}

func (r *Registry) DeleteClientLeases(clientID string) error {
	return r.db.Where("client_id = ?", clientID).Delete(&ClientLease{}).Error
}

func (r *Registry) TouchClientLeases(clientID string) error {
	return r.db.Model(&ClientLease{}).Where("client_id = ?", clientID).Update("last_heartbeat", time.Now().UTC()).Error
}

func (r *Registry) CountInstanceLeases(instanceID string) (int64, error) {
	var count int64
	if err := r.db.Model(&ClientLease{}).Where("instance_id = ?", instanceID).Count(&count).Error; err != nil {
		return 0, err
	}
	return count, nil
}

// --- Audit ---

func (r *Registry) Audit(projectID, kind, detail string) error {
	return r.db.Create(&AuditEvent{
		ProjectID: projectID,
		Kind:      kind,
		Detail:    detail,
		Timestamp: time.Now().UTC(),
	}).Error
}

// AuditOnce records an audit event only if no prior row exists for the
// same projectID+kind pair. Used for AuditInit so multi-client
// project.select/Bind re-attaches do not spam init rows.
func (r *Registry) AuditOnce(projectID, kind, detail string) error {
	var n int64
	if err := r.db.Model(&AuditEvent{}).
		Where("project_id = ? AND kind = ?", projectID, kind).
		Count(&n).Error; err != nil {
		return err
	}
	if n > 0 {
		return nil
	}
	return r.Audit(projectID, kind, detail)
}

func (r *Registry) ListAuditEvents(projectID string) ([]AuditEvent, error) {
	var out []AuditEvent
	q := r.db.Order("timestamp asc")
	if projectID != "" {
		q = q.Where("project_id = ?", projectID)
	}
	if err := q.Find(&out).Error; err != nil {
		return nil, err
	}
	return out, nil
}
