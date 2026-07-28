package config

import (
	"os"
	"path/filepath"
	"testing"
)

func TestDefaultReadsPersistedRuntimeConfigWithEnvPrecedence(t *testing.T) {
	tmp := t.TempDir()
	configFile := filepath.Join(tmp, "runtime.env")
	if err := os.WriteFile(configFile, []byte("SNAPSHOTD_HOME=/persisted/home\nSNAPSHOTD_MCP_SSE_ADDR=127.0.0.1:4321\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	oldFile, oldHome, oldAddr := os.Getenv("SNAPFLOW_CONFIG_FILE"), os.Getenv("SNAPSHOTD_HOME"), os.Getenv("SNAPSHOTD_MCP_SSE_ADDR")
	t.Cleanup(func() {
		setOrUnset("SNAPFLOW_CONFIG_FILE", oldFile)
		setOrUnset("SNAPSHOTD_HOME", oldHome)
		setOrUnset("SNAPSHOTD_MCP_SSE_ADDR", oldAddr)
	})
	os.Setenv("SNAPFLOW_CONFIG_FILE", configFile)
	os.Unsetenv("SNAPSHOTD_HOME")
	os.Unsetenv("SNAPSHOTD_MCP_SSE_ADDR")
	cfg := Default()
	if cfg.HomeDir != "/persisted/home" || cfg.MCPSSEAddr != "127.0.0.1:4321" {
		t.Fatalf("persisted config not applied: home=%q mcp=%q", cfg.HomeDir, cfg.MCPSSEAddr)
	}
	os.Setenv("SNAPSHOTD_HOME", "/env/home")
	os.Setenv("SNAPSHOTD_MCP_SSE_ADDR", "127.0.0.1:5432")
	cfg = Default()
	if cfg.HomeDir != "/env/home" || cfg.MCPSSEAddr != "127.0.0.1:5432" {
		t.Fatalf("environment did not override persisted config: home=%q mcp=%q", cfg.HomeDir, cfg.MCPSSEAddr)
	}
}

func setOrUnset(key, value string) {
	if value == "" {
		_ = os.Unsetenv(key)
		return
	}
	_ = os.Setenv(key, value)
}
