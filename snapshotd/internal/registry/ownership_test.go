package registry

import "testing"

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
