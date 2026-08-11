package registry

import (
	"path/filepath"
	"testing"
	"time"
)

func TestPendingCandidateIdentityRejectsPIDOnlyReuse(t *testing.T) {
	base := PendingProjectCandidate{
		Owner: "external", InstanceID: "instance-a", InstanceNonce: "nonce-a",
		PID: 42, ProcessStart: "start-a",
	}
	if base.IdentityKey() != "external|instance-a|nonce-a|start-a" {
		t.Fatalf("unexpected identity key: %q", base.IdentityKey())
	}
	reused := base
	reused.InstanceNonce = "nonce-b"
	if reused.IdentityKey() == base.IdentityKey() {
		t.Fatal("instance nonce change must produce a distinct identity")
	}
	startReused := base
	startReused.ProcessStart = "start-b"
	if startReused.IdentityKey() == base.IdentityKey() {
		t.Fatal("process-start change must produce a distinct identity")
	}
}

func TestOwnershipModelConstantsAndQueueBound(t *testing.T) {
	if OwnershipManaged == OwnershipExternal || PendingStatus == PromotedStatus {
		t.Fatal("ownership/status values must remain distinct")
	}
	if MaxPendingCandidatesPerProject < 1 || MaxPendingCandidatesPerProject > 64 {
		t.Fatalf("queue bound is unsafe: %d", MaxPendingCandidatesPerProject)
	}
}

func TestPendingCandidateUpsertIsIdentityStable(t *testing.T) {
	r, err := Open(filepath.Join(t.TempDir(), "registry.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	now := time.Now().UTC()
	candidate := &PendingProjectCandidate{ProjectID: "project", Owner: OwnershipExternal, InstanceID: "instance", InstanceNonce: "nonce", PID: 7, ProcessStart: "start", Generation: 1, Status: PendingStatus, LastSeenAt: now, LeaseExpiresAt: now.Add(time.Minute), CreatedAt: now, UpdatedAt: now}
	if err := r.UpsertPendingProjectCandidate(candidate); err != nil {
		t.Fatal(err)
	}
	firstID := candidate.ID
	candidate.Generation = 2
	if err := r.UpsertPendingProjectCandidate(candidate); err != nil {
		t.Fatal(err)
	}
	if candidate.ID != firstID {
		t.Fatalf("same identity created a second pending row: %q != %q", candidate.ID, firstID)
	}
	var count int64
	if err := r.DB().Model(&PendingProjectCandidate{}).Where("project_id = ?", "project").Count(&count).Error; err != nil {
		t.Fatal(err)
	}
	if count != 1 {
		t.Fatalf("expected one pending row, got %d", count)
	}
}
