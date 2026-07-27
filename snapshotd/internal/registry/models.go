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
// Project type values (kSnapflowProjectFolder / MLT.projectFolder()).
const (
	ProjectTypeFolder = "folder"
	ProjectTypeFile   = "file"
)

type Project struct {
	ID          string `gorm:"primaryKey" json:"id"`
	RootDir     string `gorm:"uniqueIndex" json:"rootDir"`
	MltFileName string `json:"mltFileName"`
	// ProjectType is "folder" | "file". Folder-type projects have a sibling
	// asset directory (clips/audio/photos); file-type is a standalone .mlt.
	// Authoritative value is the in-.mlt kSnapflowProjectFolder flag after
	// open; creation-time value is the caller's request (default folder).
	ProjectType    string    `json:"projectType"`
	CreatedAt      time.Time `json:"createdAt"`
	LastOpenedAt   time.Time `json:"lastOpenedAt"`
	Status         string    `json:"status"` // "active" | "archived"
	// Path is RootDir for folder-type list rows, or RootDir/MltFileName for
	// file-type; filled by list helpers (not persisted).
	Path string `gorm:"-" json:"path"`
	// Active / IsOpen: true iff the project's most recent ProcessInstance
	// has Status == "ready" (DB-only by default; refresh:true re-probes).
	// Same bit under two names for plan + client convenience.
	Active         bool      `gorm:"-" json:"active"`
	IsOpen         bool      `gorm:"-" json:"isOpen"`
	Open           bool      `gorm:"-" json:"open"` // external GUI lease still open
	InstanceCount  int       `gorm:"-" json:"instanceCount"`
	LastSeenAt     time.Time `gorm:"-" json:"lastSeenAt,omitempty"`
	DiscoveryState string    `gorm:"-" json:"discoveryState"`
	// ProjectID is an alias of ID for path-first API responses.
	ProjectID string `gorm:"-" json:"projectId"`
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
	// Headless records the mode this instance was actually launched in
	// (PISO-9: per-project launch dedup). A project is live in exactly one
	// mode at a time -- once an instance exists, later daemon.launch calls
	// for the same project reuse it regardless of what mode they asked for,
	// so a caller needs this (together with LaunchResult.Reused) to know
	// whether it actually got the visible/headful instance it may have
	// wanted or is attaching to an existing headless one instead.
	Headless bool
}

func (ProcessInstance) TableName() string { return "process_instances" }

// ExternalInstance is the authoritative registration for a separately
// launched Snapflow GUI. It is deliberately separate from ProcessInstance:
// the latter is daemon-owned child-process state, while this record is a
// lease held by an external process that the daemon must never launch or
// kill implicitly.
type ExternalInstance struct {
	ID               string    `gorm:"primaryKey" json:"instanceId"`
	InstanceNonce    string    `gorm:"uniqueIndex" json:"instanceNonce"`
	PID              int       `json:"pid"`
	ProcessStart     string    `json:"processStart"`
	ProjectPath      string    `json:"projectPath,omitempty"`
	SAPSocketPath    string    `json:"sapSocketPath,omitempty"`
	CapabilitiesJSON string    `json:"capabilities,omitempty"`
	Status           string    `json:"status"`
	Source           string    `json:"source"`
	Generation       uint64    `json:"generation"`
	LifecycleReason  string    `json:"lifecycleReason,omitempty"`
	LastSeenAt       time.Time `json:"lastSeenAt"`
	LeaseExpiresAt   time.Time `json:"leaseExpiresAt"`
	CreatedAt        time.Time `json:"createdAt"`
	UpdatedAt        time.Time `json:"updatedAt"`
}

func (ExternalInstance) TableName() string { return "external_instances" }

const (
	ExternalStatusOpen   = "open"
	ExternalStatusStale  = "stale"
	ExternalStatusClosed = "closed"
)

// McpContext binds one opaque agent-session token to one chat owner and its
// current target. The target is mutable, but the chat owner is not.
type McpContext struct {
	ContextToken           string    `gorm:"primaryKey" json:"contextToken"`
	ACPSessionID           string    `gorm:"index" json:"acpSessionId"`
	ChatProjectID          string    `json:"chatProjectId"`
	DefaultTargetProjectID string    `json:"defaultTargetProjectId"`
	TargetProjectID        string    `json:"targetProjectId"`
	LastSeenAt             time.Time `json:"lastSeenAt"`
	LeaseExpiresAt         time.Time `json:"leaseExpiresAt"`
}

func (McpContext) TableName() string { return "mcp_contexts" }

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
	Kind      string // "launch" | "crash" | "restart" | "close" | "init" | "create" | "delete"
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
	// AuditInit is recorded once when a project's first real open succeeds
	// (project.select response with opened=true). Distinct from AuditLaunch
	// (process spawn) so empty-project investigations can query init rows.
	AuditInit = "init"
)
