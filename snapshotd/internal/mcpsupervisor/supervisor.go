// Package mcpsupervisor owns the MCP HTTP listener's live lifecycle:
// starting it, rebinding it to a new address, and turning Basic Auth on --
// on top of the persisted configuration in internal/mcpauth.
//
// The one safety rule this package exists to enforce, unconditionally and
// daemon-side (not just as CLI advice): binding to a non-loopback address
// without auth already enabled is refused. That refusal happens here, in
// Restart, so it can't be bypassed by calling the underlying SDP method
// directly -- the CLI is just one caller of it.
package mcpsupervisor

import (
	"context"
	"fmt"
	"log/slog"
	"net"
	"sync"
	"time"

	"github.com/google/uuid"
	"snapshotd/internal/mcpadapter"
	"snapshotd/internal/mcpauth"
)

// Status is the read-only snapshot returned by daemon.mcpStatus.
type Status struct {
	Addr        string `json:"addr"`
	Listening   bool   `json:"listening"`
	AuthEnabled bool   `json:"authEnabled"`
	AuthUser    string `json:"authUser,omitempty"`
}

// InstallConfig is what daemon.mcpInstallConfig returns: enough for a
// client to configure itself against this daemon's MCP endpoint.
type InstallConfig struct {
	SSEURL              string `json:"sseUrl"`
	StreamableURL       string `json:"streamableUrl"`
	AuthEnabled         bool   `json:"authEnabled"`
	AuthUser            string `json:"authUser,omitempty"`
	AuthPassword        string `json:"authPassword,omitempty"`
	SessionServiceToken string `json:"sessionServiceToken,omitempty"`
}

// Supervisor owns exactly one live *mcpadapter.SSEServer at a time.
type Supervisor struct {
	h           mcpadapter.Handler
	homeDir     string
	defaultAddr string
	log         *slog.Logger

	mu        sync.Mutex
	cfg       mcpauth.Config
	srv       *mcpadapter.SSEServer
	listening bool
	// boundAddr is the listener's actual address (ln.Addr().String()),
	// which differs from cfg.BindAddr whenever the configured port is 0
	// (OS-assigned, used by tests) -- Status/InstallConfig report this one
	// once a listener is up.
	boundAddr string
}

// New constructs a Supervisor. defaultAddr is used to seed the persisted
// config the first time Start runs and no mcp_config.json exists yet.
func New(h mcpadapter.Handler, homeDir, defaultAddr string, log *slog.Logger) *Supervisor {
	if log == nil {
		log = slog.Default()
	}
	return &Supervisor{h: h, homeDir: homeDir, defaultAddr: defaultAddr, log: log}
}

// Start loads persisted config (seeding it from defaultAddr if this is the
// first run) and binds the listener. Returns nil immediately if this
// Supervisor was constructed with an empty defaultAddr and no persisted
// config exists -- callers that pass "--no-mcp" never call Start at all,
// this is only for the defaultAddr == "" edge case in tests.
func (s *Supervisor) Start(ctx context.Context) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	cfg, err := mcpauth.Load(s.homeDir)
	if err != nil {
		return fmt.Errorf("mcpsupervisor: load persisted config: %w", err)
	}
	if cfg.BindAddr == "" {
		cfg.BindAddr = s.defaultAddr
		if err := mcpauth.Save(s.homeDir, cfg); err != nil {
			return fmt.Errorf("mcpsupervisor: seed persisted config: %w", err)
		}
	}
	s.cfg = cfg
	if cfg.SessionServiceToken == "" {
		cfg.SessionServiceToken = uuid.NewString()
		if err := mcpauth.Save(s.homeDir, cfg); err != nil {
			return fmt.Errorf("mcpsupervisor: persist session service token: %w", err)
		}
		s.cfg = cfg
	}
	return s.bindLocked(cfg)
}

// bindNew constructs a listener for cfg without touching any existing live
// listener, so a caller can confirm the new address actually binds before
// retiring whatever was running before -- see Restart's comment for why
// that ordering matters.
func (s *Supervisor) bindNew(cfg mcpauth.Config) (*mcpadapter.SSEServer, net.Listener, error) {
	srv := mcpadapter.NewSSEServer(s.h, cfg.BindAddr)
	srv.SetCredentials(mcpadapter.Credentials{
		Enabled:  cfg.AuthEnabled,
		User:     cfg.AuthUser,
		Password: cfg.AuthPassword,
	})
	srv.SetSessionServiceToken(cfg.SessionServiceToken)
	ln, err := srv.Listen()
	if err != nil {
		return nil, nil, fmt.Errorf("mcpsupervisor: bind %s: %w", cfg.BindAddr, err)
	}
	return srv, ln, nil
}

// commitLocked makes srv/ln the live listener and starts serving. Caller
// must hold s.mu and have already retired any prior listener.
func (s *Supervisor) commitLocked(cfg mcpauth.Config, srv *mcpadapter.SSEServer, ln net.Listener) {
	s.srv = srv
	s.listening = true
	s.boundAddr = ln.Addr().String()
	go func() {
		if err := srv.Serve(ln); err != nil {
			s.log.Error("mcp listener exited", "addr", cfg.BindAddr, "err", err)
		}
	}()
}

// bindLocked binds and starts serving cfg.BindAddr with cfg's auth. Caller
// must hold s.mu. Only used by Start, where there is no prior listener to
// preserve on failure.
func (s *Supervisor) bindLocked(cfg mcpauth.Config) error {
	srv, ln, err := s.bindNew(cfg)
	if err != nil {
		return err
	}
	s.commitLocked(cfg, srv, ln)
	return nil
}

// Restart binds a new listener for bindAddr (or the current address, if
// bindAddr is empty -- useful for picking up an auth change without moving
// the port) and only then retires whatever was running before. That
// ordering is deliberate: binding first means a bad address (typo, port
// already in use, permission denied) fails without tearing down a
// perfectly good existing listener -- callers get "the address you asked
// for didn't work" instead of "the server you had is now gone too".
//
// Refuses to bind a non-loopback address unless auth is already enabled --
// either from a prior SetAuth call, or already true in the persisted
// config.
func (s *Supervisor) Restart(ctx context.Context, bindAddr string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	newCfg := s.cfg
	if bindAddr != "" {
		newCfg.BindAddr = bindAddr
	}
	if newCfg.BindAddr == "" {
		newCfg.BindAddr = s.defaultAddr
	}
	if newCfg.SessionServiceToken == "" {
		newCfg.SessionServiceToken = uuid.NewString()
	}
	if !newCfg.AuthEnabled && !isLoopbackAddr(newCfg.BindAddr) {
		return fmt.Errorf(
			"refusing to bind %q: non-loopback address requires auth to be enabled first -- run `snapshotd mcp auth set` before binding beyond 127.0.0.1",
			newCfg.BindAddr,
		)
	}

	newSrv, ln, err := s.bindNew(newCfg)
	if err != nil {
		return err
	}

	if s.srv != nil {
		shutdownCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
		_ = s.srv.Shutdown(shutdownCtx)
		cancel()
	}

	s.commitLocked(newCfg, newSrv, ln)
	s.cfg = newCfg
	return mcpauth.Save(s.homeDir, newCfg)
}

// SetAuth persists new Basic Auth credentials and applies them to the live
// listener immediately (no restart required).
func (s *Supervisor) SetAuth(ctx context.Context, user, password string) error {
	if user == "" || password == "" {
		return fmt.Errorf("mcpsupervisor: user and password are both required")
	}
	s.mu.Lock()
	defer s.mu.Unlock()

	newCfg := s.cfg
	newCfg.AuthEnabled = true
	newCfg.AuthUser = user
	newCfg.AuthPassword = password
	if newCfg.BindAddr == "" {
		newCfg.BindAddr = s.defaultAddr
	}
	if s.srv != nil {
		s.srv.SetCredentials(mcpadapter.Credentials{Enabled: true, User: user, Password: password})
	}
	s.cfg = newCfg
	return mcpauth.Save(s.homeDir, newCfg)
}

// Status returns the current listener state.
func (s *Supervisor) Status() Status {
	s.mu.Lock()
	defer s.mu.Unlock()
	addr := s.cfg.BindAddr
	if s.listening && s.boundAddr != "" {
		addr = s.boundAddr
	}
	return Status{
		Addr:        addr,
		Listening:   s.listening,
		AuthEnabled: s.cfg.AuthEnabled,
		AuthUser:    s.cfg.AuthUser,
	}
}

// InstallConfig returns endpoint + credentials for configuring an MCP
// client against this daemon.
func (s *Supervisor) InstallConfig() InstallConfig {
	s.mu.Lock()
	defer s.mu.Unlock()
	addr := s.cfg.BindAddr
	if s.listening && s.boundAddr != "" {
		addr = s.boundAddr
	}
	host := addr
	if h, _, err := net.SplitHostPort(addr); err == nil {
		if h == "" || h == "0.0.0.0" {
			host = "127.0.0.1"
		} else {
			host = h
		}
		_, port, _ := net.SplitHostPort(addr)
		addr = net.JoinHostPort(host, port)
	}
	return InstallConfig{
		SSEURL:              fmt.Sprintf("http://%s/sse", addr),
		StreamableURL:       fmt.Sprintf("http://%s/mcp", addr),
		AuthEnabled:         s.cfg.AuthEnabled,
		AuthUser:            s.cfg.AuthUser,
		AuthPassword:        s.cfg.AuthPassword,
		SessionServiceToken: s.cfg.SessionServiceToken,
	}
}

// Stop shuts down the live listener, if any. Safe to call even if Start was
// never called.
func (s *Supervisor) Stop(ctx context.Context) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.srv == nil {
		return nil
	}
	s.listening = false
	s.boundAddr = ""
	return s.srv.Shutdown(ctx)
}

// IsLoopbackAddr reports whether addr's host portion is a loopback address
// (127.0.0.1, ::1, localhost). An empty host (e.g. ":7777", meaning "all
// interfaces") and 0.0.0.0 are both treated as non-loopback. Exported so
// cmd/snapshotd can decide whether to print the plaintext-HTTP warning
// after a successful restart, using the exact same rule Restart enforces.
func IsLoopbackAddr(addr string) bool {
	return isLoopbackAddr(addr)
}

func isLoopbackAddr(addr string) bool {
	host, _, err := net.SplitHostPort(addr)
	if err != nil {
		host = addr
	}
	if host == "" {
		return false
	}
	if ip := net.ParseIP(host); ip != nil {
		return ip.IsLoopback()
	}
	return host == "localhost"
}
