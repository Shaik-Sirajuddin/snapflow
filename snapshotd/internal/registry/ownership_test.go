package registry

import (
	"path/filepath"
	"sync"
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

func TestReleaseProjectOwnershipOnSwitch(t *testing.T) {
	r, err := Open(filepath.Join(t.TempDir(), "registry.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	now := time.Now().UTC()
	for _, project := range []string{"old", "new"} {
		if err := r.SaveProjectActiveOwner(&ProjectActiveOwner{ProjectID: project, Owner: OwnershipExternal, InstanceID: "gui", InstanceNonce: "nonce", PID: 7, ProcessStart: "start", LastSeenAt: now, LeaseExpiresAt: now.Add(time.Minute)}); err != nil {
			t.Fatal(err)
		}
	}
	if err := r.ReleaseProjectOwnership(OwnershipExternal, "gui", "nonce", "start", "new"); err != nil {
		t.Fatal(err)
	}
	if _, err := r.GetProjectActiveOwner("old"); err != ErrNotFound {
		t.Fatalf("old marker should be released: %v", err)
	}
	if _, err := r.GetProjectActiveOwner("new"); err != nil {
		t.Fatalf("new marker should remain: %v", err)
	}
	candidate := &PendingProjectCandidate{ProjectID: "new", Owner: OwnershipExternal, InstanceID: "gui", InstanceNonce: "nonce", PID: 7, ProcessStart: "start", Generation: 2, Status: PendingStatus, LastSeenAt: now, LeaseExpiresAt: now.Add(time.Minute), CreatedAt: now, UpdatedAt: now}
	if err := r.UpsertPendingProjectCandidate(candidate); err != nil {
		t.Fatal(err)
	}
	if err := r.ReleaseProjectOwnership(OwnershipExternal, "gui", "nonce", "start", ""); err != nil {
		t.Fatal(err)
	}
	var got PendingProjectCandidate
	if err := r.DB().First(&got, "id = ?", candidate.ID).Error; err != nil {
		t.Fatal(err)
	}
	if got.Status != StaleStatus {
		t.Fatalf("pending identity should be stale after release: %s", got.Status)
	}
}

func TestClaimProjectOwnerConcurrentFirstCallbacksKeepOneActive(t *testing.T) {
	r, err := Open(filepath.Join(t.TempDir(), "registry.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	now := time.Now().UTC()
	candidates := []PendingProjectCandidate{
		{ProjectID: "project", Owner: OwnershipExternal, InstanceID: "a", InstanceNonce: "na", PID: 1, ProcessStart: "sa", Generation: 1, Status: PendingStatus, LeaseExpiresAt: now.Add(time.Minute), CreatedAt: now, UpdatedAt: now},
		{ProjectID: "project", Owner: OwnershipExternal, InstanceID: "b", InstanceNonce: "nb", PID: 2, ProcessStart: "sb", Generation: 1, Status: PendingStatus, LeaseExpiresAt: now.Add(time.Minute), CreatedAt: now, UpdatedAt: now},
	}
	var wg sync.WaitGroup
	errs := make(chan error, len(candidates))
	for i := range candidates {
		wg.Add(1)
		go func(candidate PendingProjectCandidate) {
			defer wg.Done()
			_, _, callErr := r.ClaimProjectOwner(candidate, now)
			errs <- callErr
		}(candidates[i])
	}
	wg.Wait()
	close(errs)
	for callErr := range errs {
		if callErr != nil {
			t.Fatal(callErr)
		}
	}
	active, err := r.GetProjectActiveOwner("project")
	if err != nil {
		t.Fatal(err)
	}
	if active.InstanceID != "a" && active.InstanceID != "b" {
		t.Fatalf("unexpected active owner: %+v", active)
	}
	pending, err := r.ListPendingProjectCandidates("project")
	if err != nil {
		t.Fatal(err)
	}
	if len(pending) != 1 || pending[0].InstanceID == active.InstanceID {
		t.Fatalf("expected losing callback to remain pending: active=%+v pending=%+v", active, pending)
	}
}

func TestClaimProjectOwnerConflictReleasesPriorProjectMarker(t *testing.T) {
	r, err := Open(filepath.Join(t.TempDir(), "registry.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	now := time.Now().UTC()
	if err := r.SaveProjectActiveOwner(&ProjectActiveOwner{ProjectID: "old", Owner: OwnershipExternal, InstanceID: "b", InstanceNonce: "nb", PID: 2, ProcessStart: "sb", LastSeenAt: now, LeaseExpiresAt: now.Add(time.Minute)}); err != nil {
		t.Fatal(err)
	}
	if err := r.SaveProjectActiveOwner(&ProjectActiveOwner{ProjectID: "new", Owner: OwnershipExternal, InstanceID: "a", InstanceNonce: "na", PID: 1, ProcessStart: "sa", LastSeenAt: now, LeaseExpiresAt: now.Add(time.Minute)}); err != nil {
		t.Fatal(err)
	}
	candidate := PendingProjectCandidate{ProjectID: "new", Owner: OwnershipExternal, InstanceID: "b", InstanceNonce: "nb", PID: 2, ProcessStart: "sb", Generation: 2, Status: PendingStatus, LastSeenAt: now, LeaseExpiresAt: now.Add(time.Minute), CreatedAt: now, UpdatedAt: now}
	pending, active, err := r.ClaimProjectOwner(candidate, now.Add(time.Second))
	if err != nil || !pending || active.InstanceID != "a" {
		t.Fatalf("expected conflict pending under active owner a: pending=%v active=%+v err=%v", pending, active, err)
	}
	if _, err := r.GetProjectActiveOwner("old"); err != ErrNotFound {
		t.Fatalf("prior project marker should be released: %v", err)
	}
	current, err := r.GetProjectActiveOwner("new")
	if err != nil || current.InstanceID != "a" {
		t.Fatalf("conflicting active owner changed unexpectedly: %+v err=%v", current, err)
	}
	queued, err := r.ListPendingProjectCandidates("new")
	if err != nil || len(queued) != 1 || queued[0].InstanceID != "b" {
		t.Fatalf("expected one pending candidate for new project: %+v err=%v", queued, err)
	}
}

func TestPromoteProjectCandidateAtomicallyUpdatesMarkerAndStatus(t *testing.T) {
	r, err := Open(filepath.Join(t.TempDir(), "registry.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	now := time.Now().UTC()
	candidate := &PendingProjectCandidate{ProjectID: "project", Owner: OwnershipExternal, InstanceID: "next", InstanceNonce: "nonce", PID: 9, ProcessStart: "start", Generation: 3, Status: PendingStatus, LastSeenAt: now, LeaseExpiresAt: now.Add(time.Minute), CreatedAt: now, UpdatedAt: now}
	if err := r.UpsertPendingProjectCandidate(candidate); err != nil {
		t.Fatal(err)
	}
	if err := r.PromoteProjectCandidate("project", candidate, now.Add(time.Second)); err != nil {
		t.Fatal(err)
	}
	active, err := r.GetProjectActiveOwner("project")
	if err != nil || active.InstanceID != "next" || active.Generation != 3 {
		t.Fatalf("promotion did not install active marker: %+v, err=%v", active, err)
	}
	var stored PendingProjectCandidate
	if err := r.DB().First(&stored, "id = ?", candidate.ID).Error; err != nil {
		t.Fatal(err)
	}
	if stored.Status != PromotedStatus {
		t.Fatalf("candidate status = %q, want promoted", stored.Status)
	}
}
