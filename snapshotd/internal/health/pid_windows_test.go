//go:build windows

package health

import (
	"os"
	"testing"
)

func TestProcessStartIdentityUsesCreationTime(t *testing.T) {
	identity, err := ProcessStartIdentity(os.Getpid())
	if err != nil || identity == "" {
		t.Fatalf("current process identity unavailable: identity=%q err=%v", identity, err)
	}
	if !ProcessIdentityMatches(os.Getpid(), identity) {
		t.Fatalf("current process identity did not round-trip: %q", identity)
	}
	if ProcessIdentityMatches(os.Getpid(), "0") {
		t.Fatal("wrong creation identity must not match")
	}
}
