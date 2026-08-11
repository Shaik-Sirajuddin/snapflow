package registry

import (
	"path/filepath"
	"testing"
)

func openTestRegistry(t *testing.T) *Registry {
	t.Helper()
	reg, err := Open(filepath.Join(t.TempDir(), "registry.db"))
	if err != nil {
		t.Fatalf("open registry: %v", err)
	}
	t.Cleanup(func() { _ = reg.Close() })
	return reg
}
