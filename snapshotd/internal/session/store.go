// Package session implements the daemon's session-tracking layer, per
// 07-daemon-persistence.md's Redis/GORM split: sessions are ephemeral,
// TTL'd state (which client is connected, which project it's bound to for
// 01-jsonrpc-spec.md's "session binds to at most one project" model) that
// does NOT need to survive a daemon restart, unlike the GORM registry.
//
// v1 default is an in-memory implementation (Memory, see memory.go). Doc 07
// caveat 1 explicitly recommends defining this as an interface with the
// Redis-backed implementation as an *additive* swap for later horizontal
// scaling -- that Redis implementation is intentionally NOT built here. Any
// future RedisStore only needs to implement this same interface; nothing
// else in the daemon needs to change to plug it in.
package session

import (
	"errors"
	"time"
)

// ErrNotFound is returned when a session id has no live entry (never
// existed, or expired).
var ErrNotFound = errors.New("session: not found")

// Session is the ephemeral state tracked per connected client, per 07's
// SessionMgr sequence diagram (SET session:{id} {clientKind, projectId,
// role} EX 60) and 01-jsonrpc-spec.md's session->project binding model.
type Session struct {
	ID         string
	ClientKind string // e.g. "mcp", "cli", "raw-jsonrpc"
	ProjectID  string // bound project, if any ("" means unbound)
	Role       string // reserved per 05-multi-client-concurrency.md's role field
	CreatedAt  time.Time
	ExpiresAt  time.Time
}

// Store is the interface the daemon core depends on. Implementations must be
// safe for concurrent use.
type Store interface {
	// Create starts a new session with the given TTL, returning the stored
	// Session (with CreatedAt/ExpiresAt populated).
	Create(id, clientKind string, ttl time.Duration) (Session, error)

	// Touch renews a session's TTL (07's "EXPIRE session:{id} 60" on every
	// heartbeat/RPC call). Returns ErrNotFound if the session doesn't exist
	// or already expired.
	Touch(id string, ttl time.Duration) error

	// Lookup returns the current Session state, or ErrNotFound if it doesn't
	// exist or has expired.
	Lookup(id string) (Session, error)

	// BindProject records which project a session is routed to, per the
	// "no project_id param on every call" session-binding model.
	BindProject(id, projectID string) error

	// Expire removes a session immediately (explicit disconnect/logout),
	// rather than waiting for its TTL to lapse.
	Expire(id string) error

	// List returns all currently-live (non-expired) sessions, mainly for
	// `daemon.status`/introspection.
	List() []Session

	// Close releases any background resources (e.g. a sweep goroutine).
	Close() error
}
