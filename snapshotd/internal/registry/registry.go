package registry

import (
	"errors"
	"fmt"
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
func Open(path string) (*Registry, error) {
	db, err := gorm.Open(sqlite.Open(path), &gorm.Config{
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
