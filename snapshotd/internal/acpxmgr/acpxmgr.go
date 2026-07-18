// Package acpxmgr owns an optional long-lived acpx-server child under
// snapshotd serve: write ACPX_CONFIG_FILE with the live snapshotd MCP URL,
// spawn, poll /health, and SIGTERM on stop.
//
// See memory/rui/gen/plans/chat-panel/snapshotd-bundled-acpx-gateway.md and
// snapshotd/docs/acpx-bundled-gateway.md.
package acpxmgr

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// Config drives spawn + generated provisioning file.
type Config struct {
	// BinPath is the acpx-server executable.
	BinPath string
	// HttpBind is ACPX_HTTP_BIND (e.g. 127.0.0.1:8790).
	HttpBind string
	// ConfigPath is where the generated ACPX_CONFIG_FILE is written.
	ConfigPath string
	// DbPath is ACPX_DB_PATH (session metadata).
	DbPath string
	// McpURL is the snapshotd Streamable HTTP MCP endpoint
	// (e.g. http://127.0.0.1:7777/mcp).
	McpURL string
	// BackendCmd is optional ACPX_BACKEND_CMD.
	BackendCmd string
	// DefaultAgentID is optional ACPX_DEFAULT_AGENT_ID (default "default").
	DefaultAgentID string
	// ExtraEnv is merged into the child environment.
	ExtraEnv []string
	// Log is optional; if nil, slog.Default is used.
	Log *slog.Logger
}

// Manager holds a running acpx-server child.
type Manager struct {
	cfg    Config
	log    *slog.Logger
	cmd    *exec.Cmd
	cancel context.CancelFunc
	mu     sync.Mutex
	done   chan struct{}
	err    error
}

// WriteConfig writes an ACPX provisioning JSON that registers snapshotd MCP
// and a default profile that attaches it.
func WriteConfig(path, mcpURL, agentID string) error {
	if agentID == "" {
		agentID = "default"
	}
	doc := map[string]any{
		"providers": []any{},
		"mcp_servers": []any{
			map[string]any{
				"type":    "http",
				"name":    "snapshotd",
				"url":     mcpURL,
				"headers": []any{},
			},
		},
		"profiles": []any{
			map[string]any{
				"name":        "default",
				"agent_id":    agentID,
				"mcp_servers": []string{"snapshotd"},
			},
		},
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	raw, err := json.MarshalIndent(doc, "", "  ")
	if err != nil {
		return err
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, append(raw, '\n'), 0o644); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

// McpHTTPURL builds http://host:port/mcp from SNAPSHOTD_MCP_SSE_ADDR-style bind.
func McpHTTPURL(mcpBind string) string {
	bind := strings.TrimSpace(mcpBind)
	if bind == "" {
		bind = "127.0.0.1:7777"
	}
	if strings.HasPrefix(bind, "http://") || strings.HasPrefix(bind, "https://") {
		b := strings.TrimRight(bind, "/")
		b = strings.TrimSuffix(b, "/sse")
		b = strings.TrimSuffix(b, "/mcp")
		return b + "/mcp"
	}
	if strings.HasPrefix(bind, ":") {
		bind = "127.0.0.1" + bind
	}
	return "http://" + bind + "/mcp"
}

// Start writes config, spawns acpx-server, and waits briefly for /health.
func Start(ctx context.Context, cfg Config) (*Manager, error) {
	if cfg.BinPath == "" {
		return nil, fmt.Errorf("acpxmgr: empty BinPath")
	}
	if cfg.ConfigPath == "" {
		return nil, fmt.Errorf("acpxmgr: empty ConfigPath")
	}
	if cfg.HttpBind == "" {
		cfg.HttpBind = "127.0.0.1:8790"
	}
	if cfg.McpURL == "" {
		return nil, fmt.Errorf("acpxmgr: empty McpURL")
	}
	agentID := cfg.DefaultAgentID
	if agentID == "" {
		agentID = "default"
	}
	log := cfg.Log
	if log == nil {
		log = slog.Default()
	}
	if err := WriteConfig(cfg.ConfigPath, cfg.McpURL, agentID); err != nil {
		return nil, fmt.Errorf("acpxmgr: write config: %w", err)
	}

	runCtx, cancel := context.WithCancel(context.Background())
	cmd := exec.CommandContext(runCtx, cfg.BinPath)
	cmd.Env = append(os.Environ(),
		"ACPX_CONFIG_FILE="+cfg.ConfigPath,
		"ACPX_HTTP_BIND="+cfg.HttpBind,
	)
	if cfg.DbPath != "" {
		cmd.Env = append(cmd.Env, "ACPX_DB_PATH="+cfg.DbPath)
	}
	if cfg.BackendCmd != "" {
		cmd.Env = append(cmd.Env, "ACPX_BACKEND_CMD="+cfg.BackendCmd)
	}
	if cfg.DefaultAgentID != "" {
		cmd.Env = append(cmd.Env, "ACPX_DEFAULT_AGENT_ID="+cfg.DefaultAgentID)
	} else {
		cmd.Env = append(cmd.Env, "ACPX_DEFAULT_AGENT_ID="+agentID)
	}
	cmd.Env = append(cmd.Env, cfg.ExtraEnv...)
	// Inherit stderr for operator logs; silence stdin.
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	cmd.Stdin = nil

	if err := cmd.Start(); err != nil {
		cancel()
		return nil, fmt.Errorf("acpxmgr: start %s: %w", cfg.BinPath, err)
	}
	m := &Manager{
		cfg:    cfg,
		log:    log,
		cmd:    cmd,
		cancel: cancel,
		done:   make(chan struct{}),
	}
	go func() {
		err := cmd.Wait()
		m.mu.Lock()
		m.err = err
		m.mu.Unlock()
		close(m.done)
	}()

	healthURL := healthURLForBind(cfg.HttpBind)
	deadline := time.Now().Add(8 * time.Second)
	for time.Now().Before(deadline) {
		select {
		case <-ctx.Done():
			_ = m.Stop()
			return nil, ctx.Err()
		case <-m.done:
			m.mu.Lock()
			err := m.err
			m.mu.Unlock()
			return nil, fmt.Errorf("acpxmgr: child exited before healthy: %w", err)
		default:
		}
		if err := pollHealth(healthURL); err == nil {
			log.Info("acpx-server healthy", "bind", cfg.HttpBind, "config", cfg.ConfigPath)
			return m, nil
		}
		time.Sleep(150 * time.Millisecond)
	}
	// Not healthy in time — still return manager so serve can run; log warn.
	log.Warn("acpx-server health not ready yet; continuing", "bind", cfg.HttpBind, "url", healthURL)
	return m, nil
}

// Stop SIGTERMs the child (via context cancel) and waits briefly.
func (m *Manager) Stop() error {
	if m == nil {
		return nil
	}
	m.cancel()
	select {
	case <-m.done:
	case <-time.After(5 * time.Second):
		m.mu.Lock()
		if m.cmd != nil && m.cmd.Process != nil {
			_ = m.cmd.Process.Kill()
		}
		m.mu.Unlock()
		<-m.done
	}
	m.mu.Lock()
	err := m.err
	m.mu.Unlock()
	if err != nil && !isExpectedExit(err) {
		return err
	}
	return nil
}

// Done is closed when the child process exits.
func (m *Manager) Done() <-chan struct{} {
	if m == nil {
		ch := make(chan struct{})
		close(ch)
		return ch
	}
	return m.done
}

func healthURLForBind(bind string) string {
	b := strings.TrimSpace(bind)
	if strings.HasPrefix(b, "http://") || strings.HasPrefix(b, "https://") {
		return strings.TrimRight(b, "/") + "/health"
	}
	if strings.HasPrefix(b, ":") {
		b = "127.0.0.1" + b
	}
	return "http://" + b + "/health"
}

func pollHealth(url string) error {
	client := &http.Client{Timeout: 500 * time.Millisecond}
	resp, err := client.Get(url)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	_, _ = io.Copy(io.Discard, resp.Body)
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return fmt.Errorf("health status %d", resp.StatusCode)
	}
	return nil
}

func isExpectedExit(err error) bool {
	if err == nil {
		return true
	}
	// context canceled / killed on shutdown
	s := err.Error()
	return strings.Contains(s, "signal: killed") ||
		strings.Contains(s, "signal: terminated") ||
		strings.Contains(s, "context canceled")
}
