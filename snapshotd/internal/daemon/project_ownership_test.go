package daemon

import (
	"testing"

	"snapshotd/internal/registry"
)

func TestClaimProjectOwnerRetainsConflictingCandidate(t *testing.T) {
	d := newTestDaemon(t, "unused")
	first := InstanceProjectChangedParams{Owner: registry.OwnershipExternal, InstanceID: "gui-a", InstanceNonce: "nonce-a", PID: 11, ProcessStart: "start-a", Generation: 1}
	if pending, err := d.claimProjectOwner(first, "project-a"); err != nil || pending {
		t.Fatalf("first owner should become active: pending=%v err=%v", pending, err)
	}
	second := InstanceProjectChangedParams{Owner: registry.OwnershipExternal, InstanceID: "gui-b", InstanceNonce: "nonce-b", PID: 22, ProcessStart: "start-b", Generation: 2}
	if pending, err := d.claimProjectOwner(second, "project-a"); err != nil || !pending {
		t.Fatalf("conflicting owner should be pending: pending=%v err=%v", pending, err)
	}
	active, err := d.Reg.GetProjectActiveOwner("project-a")
	if err != nil || active.InstanceID != "gui-a" {
		t.Fatalf("active owner was replaced: active=%+v err=%v", active, err)
	}
	var pendingCount int64
	if err := d.Reg.DB().Model(&registry.PendingProjectCandidate{}).Where("project_id = ? AND status = ?", "project-a", registry.PendingStatus).Count(&pendingCount).Error; err != nil {
		t.Fatal(err)
	}
	if pendingCount != 1 {
		t.Fatalf("expected one pending candidate, got %d", pendingCount)
	}
}
