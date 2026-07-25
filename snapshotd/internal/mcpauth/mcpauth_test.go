package mcpauth

import (
	"os"
	"path/filepath"
	"runtime"
	"testing"
)

func TestLoadMissingFileReturnsZeroValue(t *testing.T) {
	cfg, err := Load(t.TempDir())
	if err != nil {
		t.Fatalf("load missing file: %v", err)
	}
	if cfg != (Config{}) {
		t.Fatalf("expected zero-value Config, got %+v", cfg)
	}
}

func TestSaveLoadRoundTrip(t *testing.T) {
	home := t.TempDir()
	want := Config{
		BindAddr:     "0.0.0.0:7777",
		AuthEnabled:  true,
		AuthUser:     "alice",
		AuthPassword: "s3cret",
	}
	if err := Save(home, want); err != nil {
		t.Fatalf("save: %v", err)
	}
	got, err := Load(home)
	if err != nil {
		t.Fatalf("load: %v", err)
	}
	if got != want {
		t.Fatalf("round-trip mismatch: got %+v, want %+v", got, want)
	}
}

func TestSavePermissionsOwnerOnly(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix file permission bits don't apply on windows")
	}
	home := t.TempDir()
	if err := Save(home, Config{AuthUser: "alice", AuthPassword: "s3cret"}); err != nil {
		t.Fatalf("save: %v", err)
	}
	info, err := os.Stat(filepath.Join(home, FileName))
	if err != nil {
		t.Fatalf("stat: %v", err)
	}
	if perm := info.Mode().Perm(); perm != 0o600 {
		t.Fatalf("expected 0600 perms, got %o", perm)
	}
}
