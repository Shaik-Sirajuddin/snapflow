package registry

import (
	"errors"
	"fmt"
	"io"
	"os"
	"time"

	"github.com/glebarez/sqlite"
	"gorm.io/gorm"
	"gorm.io/gorm/logger"
)

// ErrNotFound is returned by lookups that find no matching row.
var ErrNotFound = errors.New("registry: not found")

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
	if err := db.AutoMigrate(&Project{}, &ProcessInstance{}, &AuditEvent{}); err != nil {
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

// --- Project operations ---

func (r *Registry) CreateProject(p *Project) error {
	if p.MltFileName == "" {
		p.MltFileName = DefaultMltFileName
	}
	if p.Status == "" {
		p.Status = "active"
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

func (r *Registry) ListProjects() ([]Project, error) {
	var out []Project
	if err := r.db.Order("created_at asc").Find(&out).Error; err != nil {
		return nil, err
	}
	return out, nil
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

func (r *Registry) UpdateProcessInstanceStatus(id, status string) error {
	return r.db.Model(&ProcessInstance{}).Where("id = ?", id).Updates(map[string]any{
		"status":               status,
		"last_health_check_at": time.Now().UTC(),
	}).Error
}

func (r *Registry) TouchHealthCheck(id string) error {
	return r.db.Model(&ProcessInstance{}).Where("id = ?", id).Update("last_health_check_at", time.Now().UTC()).Error
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
