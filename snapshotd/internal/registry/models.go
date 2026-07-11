// Package registry implements snapshotd's durable, authoritative state per
// 07-daemon-persistence.md: the GORM/SQL half of the Redis+SQL split (Redis
// itself is intentionally not implemented here, see internal/session).
//
// Driver choice: github.com/glebarez/sqlite, a pure-Go (modernc.org/sqlite
// backed) GORM driver. It was picked specifically because it needs no CGO and
// therefore no host C compiler (mattn/go-sqlite3, the more common choice,
// requires CGO+gcc) -- this keeps `go build`/`go test` working in any sandbox
// that only has the Go toolchain. See snapshotd/README.md for the tradeoffs.
package registry

import "time"

// Project is the authoritative record of a project folder, corrected per
// 09-project-folder-layout.md: a project is a *folder* (the os.*/file.*
// sandbox root), not a bare .mlt path. RootDir is that folder; MltFileName is
// usually "project.mlt" but honors an existing filename for project.open on
// legacy projects that don't follow the folder convention.
type Project struct {
	ID           string `gorm:"primaryKey"`
	RootDir      string `gorm:"uniqueIndex"`
	MltFileName  string
	CreatedAt    time.Time
	LastOpenedAt time.Time
	Status       string // "active" | "archived"
}

// TableName pins the table name explicitly so it doesn't depend on GORM's
// pluralization guesses changing across versions.
func (Project) TableName() string { return "projects" }

// DefaultMltFileName is used whenever a caller doesn't supply one (e.g.
// daemon.createProject / project.new), per 09's "project.mlt -- canonical
// filename" convention.
const DefaultMltFileName = "project.mlt"

// ProcessInstance is a running (or previously running) child process for a
// Project, per 07-daemon-persistence.md's schema.
type ProcessInstance struct {
	ID         string `gorm:"primaryKey"`
	ProjectID  string `gorm:"index"`
	PID        int
	SocketPath string
	// Token is the per-launch SNAPSHOT_SAP_TOKEN generated for this
	// instance's sap.hello handshake. Persisted (not just held in-process)
	// so the daemon's own generic SAP proxy (internal/sapproxy) can
	// reconnect and re-authenticate to an already-running instance across a
	// daemon restart, per the reconciliation design in
	// 07-daemon-persistence.md.
	Token             string
	DaemonInstanceID  string // which snapshotd instance owns this (multi-instance note, 07); unused for v1 single-instance but kept for schema stability
	StartedAt         time.Time
	LastHealthCheckAt time.Time
	Status            string // "starting" | "ready" | "crashed" | "closed"
}

func (ProcessInstance) TableName() string { return "process_instances" }

// Process instance statuses, per 07's schema comment and 08's
// two-liveness-signal discussion.
const (
	StatusStarting = "starting"
	StatusReady    = "ready"
	StatusCrashed  = "crashed"
	StatusClosed   = "closed"
)

// AuditEvent is an append-only audit trail entry, per 07's schema.
type AuditEvent struct {
	ID        uint   `gorm:"primaryKey;autoIncrement"`
	ProjectID string `gorm:"index"`
	Kind      string // "launch" | "crash" | "restart" | "close"
	Detail    string
	Timestamp time.Time
}

func (AuditEvent) TableName() string { return "audit_events" }

// Audit event kinds.
const (
	AuditLaunch  = "launch"
	AuditCrash   = "crash"
	AuditRestart = "restart"
	AuditClose   = "close"
	AuditCreate  = "create"
	AuditDelete  = "delete"
)
