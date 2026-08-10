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

func TestDefaultReadsUtf8BomOnFirstRuntimeConfigKey(t *testing.T) {
	tmp := t.TempDir()
	configFile := filepath.Join(tmp, "runtime.env")
	if err := os.WriteFile(configFile, []byte("\xef\xbb\xbfSNAPSHOTD_HOME=/bom/home\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	oldFile, oldHome := os.Getenv("SNAPFLOW_CONFIG_FILE"), os.Getenv("SNAPSHOTD_HOME")
	t.Cleanup(func() {
		setOrUnset("SNAPFLOW_CONFIG_FILE", oldFile)
		setOrUnset("SNAPSHOTD_HOME", oldHome)
	})
	os.Setenv("SNAPFLOW_CONFIG_FILE", configFile)
	os.Unsetenv("SNAPSHOTD_HOME")
	if got := Default().HomeDir; got != "/bom/home" {
		t.Fatalf("BOM-prefixed key was not read: %q", got)
	}
}

func TestDiscoverShotcutBinPathFindsInstalledProductionBundle(t *testing.T) {
	root := t.TempDir()
	app := filepath.Join(root, "Snapflow.app")
	if err := os.MkdirAll(app, 0o755); err != nil {
		t.Fatal(err)
	}
	wrapper := filepath.Join(app, "snapflow")
	if err := os.WriteFile(wrapper, []byte("#!/bin/sh\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	got := discoverShotcutBinPath([]string{root})
	if got != wrapper {
		t.Fatalf("installed production wrapper = %q, want %q", got, wrapper)
	}
}

func TestDisableAIModeOnlySuppressesBundledACPX(t *testing.T) {
	oldDisable, oldAcpx := os.Getenv("SNAPFLOW_DISABLE_AI_MODE"), os.Getenv("SNAPSHOTD_ACPX_ENABLED")
	t.Cleanup(func() {
		setOrUnset("SNAPFLOW_DISABLE_AI_MODE", oldDisable)
		setOrUnset("SNAPSHOTD_ACPX_ENABLED", oldAcpx)
	})

	// An explicit ACPX enable must not override the rollout kill switch.
	os.Setenv("SNAPFLOW_DISABLE_AI_MODE", "true")
	os.Setenv("SNAPSHOTD_ACPX_ENABLED", "true")
	cfg := Default()
	if !cfg.DisableAIMode {
		t.Fatal("disable AI mode flag was not read")
	}
	if cfg.AcpxEnabled {
		t.Fatal("bundled ACPX remained enabled while AI mode was disabled")
	}

	// Re-enabling AI mode restores the explicit ACPX setting. The MCP adapter
	// is independent of this flag and is started by cmd/snapshotd/main.go
	// whenever --no-mcp is not supplied.
	os.Setenv("SNAPFLOW_DISABLE_AI_MODE", "false")
	cfg = Default()
	if cfg.DisableAIMode || !cfg.AcpxEnabled {
		t.Fatalf("AI mode enable did not restore ACPX: disable=%v acpx=%v", cfg.DisableAIMode, cfg.AcpxEnabled)
	}
}

func setOrUnset(key, value string) {
	if value == "" {
		_ = os.Unsetenv(key)
		return
	}
	_ = os.Setenv(key, value)
}
