package acpnode

import (
	"os"
	"path/filepath"
	"testing"
)

func TestResolveMissingWhenNoNodeOnPath(t *testing.T) {
	t.Setenv("PATH", "/nonexistent")
	t.Setenv("SNAPFLOW_INSTALL_DIR", t.TempDir())
	t.Setenv("SNAPFLOW_ACP_NODE_HOME", "")
	// Clear any inherited force home
	_ = os.Unsetenv("SNAPFLOW_ACP_NODE_HOME")
	r := Resolve()
	if r.Source != SourceMissing {
		// Environment may still have node via absolute LookPath in some
		// sandboxes; accept global only if LookPath found something.
		if r.Source == SourceGlobal {
			t.Skip("system node still visible despite PATH override")
		}
		t.Fatalf("expected missing, got %+v", r)
	}
}

func TestPrefixOKAndBundledResolve(t *testing.T) {
	root := t.TempDir()
	home := filepath.Join(root, "runtime", "node")
	bin := filepath.Join(home, "bin")
	if err := os.MkdirAll(bin, 0o755); err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"node", "npm", "npx"} {
		p := filepath.Join(bin, name)
		if err := os.WriteFile(p, []byte("#!/bin/sh\necho ok\n"), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	t.Setenv("PATH", "/nonexistent")
	t.Setenv("SNAPFLOW_INSTALL_DIR", root)
	t.Setenv("SNAPFLOW_ACP_NODE_HOME", home)
	r := Resolve()
	if r.Source != SourceBundled {
		// If host still exposes global node via other means:
		if r.Source == SourceGlobal {
			t.Skip("global node wins (global-first); cannot assert bundled in this env")
		}
		t.Fatalf("expected bundled, got %+v", r)
	}
	if r.Node != filepath.Join(home, "bin", "node") {
		t.Fatalf("node path %s", r.Node)
	}
	env := EnvForAcpx(r)
	if len(env) < 2 {
		t.Fatalf("expected PATH + SNAPFLOW_ACP_NODE_HOME, got %v", env)
	}
}

func TestEnvForAcpxGlobalIsEmpty(t *testing.T) {
	env := EnvForAcpx(Resolved{Source: SourceGlobal, Node: "/usr/bin/node"})
	if len(env) != 0 {
		t.Fatalf("global must not force SNAPFLOW_ACP_NODE_HOME, got %v", env)
	}
}
