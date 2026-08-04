// Command snapshotd is the daemon's CLI entrypoint, per
// 08-lifecycle-and-cli.md's command table: `serve` starts the persistent
// daemon (registry + session manager + process manager + SDP control socket
// + MCP/SSE endpoint); `status`/`stop`/`launch` are thin clients that talk to
// an already-running daemon over its control socket -- they never touch
// daemon state directly and simply fail to connect if no daemon is running,
// matching `docker`'s own CLI-vs-dockerd split (per 09's summary table).
package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"log/slog"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"syscall"
	"time"

	"snapshotd/internal/acpnode"
	"snapshotd/internal/acpxmgr"
	"snapshotd/internal/config"
	"snapshotd/internal/daemon"
	"snapshotd/internal/daemonlock"
	"snapshotd/internal/health"
	"snapshotd/internal/mcpsupervisor"
	"snapshotd/internal/sdp"
)

// progName is the invoked binary name used in CLI diagnostics and usage
// strings (for example, snapflowd in packaged builds).
var progName = filepath.Base(os.Args[0])

func main() {
	if len(os.Args) < 2 {
		usage()
		os.Exit(2)
	}

	cfg := config.Default()

	var err error
	switch os.Args[1] {
	case "serve":
		err = cmdServe(cfg, os.Args[2:])
	case "status":
		err = cmdStatus(cfg, os.Args[2:])
	case "stop":
		err = cmdStop(cfg, os.Args[2:])
	case "launch":
		err = cmdLaunch(cfg, os.Args[2:])
	case "list":
		err = cmdList(cfg, os.Args[2:])
	case "listProjects":
		err = cmdListProjects(cfg, os.Args[2:])
	case "close":
		err = cmdClose(cfg, os.Args[2:])
	case "mcp":
		err = cmdMCP(cfg, os.Args[2:])
	case "install":
		err = cmdInstall(cfg, os.Args[2:])
	case "doctor":
		err = cmdDoctor(cfg, os.Args[2:])
	case "runtime":
		err = cmdRuntime(cfg, os.Args[2:])
	case "-h", "--help", "help":
		usage()
		return
	default:
		fmt.Fprintf(os.Stderr, "unknown subcommand %q\n\n", os.Args[1])
		usage()
		os.Exit(2)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}

func usage() {
	fmt.Fprintln(os.Stderr, `snapshotd - Snapshot Daemon Protocol (SDP) process manager + MCP proxy

Usage:
  snapshotd serve [--headless-default]   start the daemon (registry, session manager, process manager, SDP control socket, MCP/SSE endpoint)
  snapshotd status                       connect to a running daemon and print its state
  snapshotd stop                         ask a running daemon to shut down gracefully
  snapshotd launch <projectId>           convenience wrapper around daemon.launch
  snapshotd list                         list known process instances (bare daemon.list)
  snapshotd listProjects                 list known projects (bare daemon.listProjects)
  snapshotd close <instanceId>           stop one running process instance (bare daemon.close)
  snapshotd mcp status                   show the MCP listener's bind address and auth state
  snapshotd mcp restart [--bind ADDR]    rebind the MCP listener (default 127.0.0.1:7777; a
                                          non-loopback ADDR, e.g. 0.0.0.0:7777, is refused unless
                                          mcp auth set has already been run)
  snapshotd mcp auth set --user U --password P
                                          set/replace the MCP listener's Basic Auth credentials
  snapshotd mcp install-config get       print the MCP endpoint + credentials for a client config
  snapshotd install                      print what installing a system service would do (not implemented for real)  snapshotd doctor                       check ACP Node/npm (global-first, then product bundle)
	  snapshotd runtime install node [--force]  install official Node into product runtime/ (if global missing)`)
}

func cmdServe(cfg config.Config, args []string) error {
	fs := flag.NewFlagSet("serve", flag.ExitOnError)
	noMCP := fs.Bool("no-mcp", false, "disable the SSE MCP adapter")
	_ = fs.Parse(args)

	logLevel := slog.LevelInfo
	if v := os.Getenv("SNAPSHOTD_LOG_LEVEL"); v != "" {
		_ = logLevel.UnmarshalText([]byte(v))
	} else if debug, _ := strconv.ParseBool(os.Getenv("SNAPSHOTD_DEBUG")); debug {
		logLevel = slog.LevelDebug
	}
	logger := slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: logLevel}))

	lock, err := daemonlock.Acquire(cfg.HomeDir)
	if err != nil {
		return err
	}
	defer lock.Close()

	d, err := daemon.New(cfg, logger)
	if err != nil {
		return fmt.Errorf("initializing daemon: %w", err)
	}
	defer d.Close()

	// Written for operator/tooling visibility (e.g. a manual `kill` as a
	// last resort) -- `snapshotd stop` itself no longer reads this; it goes
	// through the SDP control socket (daemon.stop) instead, which works
	// the same on every platform.
	// The control endpoint is a filesystem path on Unix but a named-pipe
	// namespace on Windows; never derive a pidfile by appending to it.
	pidPath := filepath.Join(cfg.HomeDir, "daemon.pid")
	if err := os.WriteFile(pidPath, []byte(fmt.Sprintf("%d\n", os.Getpid())), 0o644); err != nil {
		logger.Warn("could not write pidfile", "path", pidPath, "err", err)
	}
	defer os.Remove(pidPath)

	// Startup reconciliation, per 07-daemon-persistence.md: reconnect to
	// already-"ready" process instances rather than assuming a fresh daemon
	// process means every child needs relaunching.
	if _, err := d.Reconcile(context.Background()); err != nil {
		logger.Warn("startup reconciliation failed", "err", err)
	}

	// ACP Node fallback: if global node missing and bundle missing, try
	// official local install once (same policy as install.sh ensure).
	if acpnode.Resolve().Source == acpnode.SourceMissing {
		if err := acpnode.Ensure(false); err != nil {
			logger.Warn("ACP Node ensure failed (agents needing node/npm will be limited)", "err", err)
		} else {
			logger.Info("ACP Node runtime ready", "source", string(acpnode.Resolve().Source))
		}
	}

	// Bind the control socket synchronously before announcing "listening":
	// Listen()/Serve() are split exactly so this ordering is possible. When
	// the log line was previously printed right after `go
	// sdpServer.ListenAndServe()` (which does its own Listen internally, on
	// that goroutine), there was no guarantee the bind had completed before
	// the line was logged -- a `status`/`stop` client dialing immediately
	// after seeing "listening" could race the actual net.Listen and get
	// "connection refused". Binding here, on the main goroutine, before the
	// log line and before any client could plausibly be told to dial,
	// closes that race.
	sdpServer := &sdp.Server{Endpoint: cfg.ControlSocketPath, Handler: d, Log: logger}
	if err := sdpServer.Listen(); err != nil {
		return fmt.Errorf("binding SDP control socket: %w", err)
	}
	sdpErrCh := make(chan error, 1)
	go func() {
		sdpErrCh <- sdpServer.Serve()
	}()
	logger.Info("SDP control socket listening", "path", cfg.ControlSocketPath)

	mcpStarted := false
	if !*noMCP {
		if err := d.Mcp.Start(context.Background()); err != nil {
			return fmt.Errorf("starting MCP listener: %w", err)
		}
		mcpStarted = true
		logger.Info("MCP SSE endpoint listening", "addr", d.Mcp.Status().Addr)
	}

	// Optional bundled acpx-server: single gateway owner under snapshotd serve.
	var acpxMgr *acpxmgr.Manager
	if cfg.AcpxEnabled && !*noMCP {
		if cfg.AcpxBinPath == "" {
			logger.Warn("SNAPSHOTD_ACPX_ENABLED but no acpx-server binary found; skip spawn")
		} else {
			startCtx, startCancel := context.WithTimeout(context.Background(), 10*time.Second)
			mgr, err := acpxmgr.Start(startCtx, acpxmgr.Config{
				BinPath:           cfg.AcpxBinPath,
				HttpBind:          cfg.AcpxHttpBind,
				ConfigPath:        cfg.AcpxConfigPath,
				DbPath:            filepath.Join(cfg.HomeDir, "acpx.sqlite3"),
				McpURL:            acpxmgr.McpHTTPURL(cfg.MCPSSEAddr),
				DefaultAgentID:    "default",
				AdminBind:         cfg.AcpxAdminBind,
				DefaultAcpCommand: cfg.AcpxDefaultAcpCommand,
				Log:               logger,
			})
			startCancel()
			if err != nil {
				logger.Error("failed to start bundled acpx-server", "err", err)
			} else {
				acpxMgr = mgr
				logger.Info("bundled acpx-server started",
					"bin", cfg.AcpxBinPath,
					// mgr.HTTPBind(), not cfg.AcpxHttpBind: acpxmgr.Start
					// walks to the next free port when the requested one is
					// already occupied, so the two can legitimately differ.
					"bind", mgr.HTTPBind(),
					"config", cfg.AcpxConfigPath,
				)
			}
		}
	}

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, os.Interrupt, syscall.SIGTERM)

	select {
	case sig := <-sigCh:
		logger.Info("received signal, shutting down", "signal", sig.String())
	case <-d.StopRequested():
		logger.Info("stop requested via daemon.stop (snapshotd stop)")
	case err := <-sdpErrCh:
		if err != nil {
			logger.Error("SDP server exited", "err", err)
		}
	case <-func() <-chan struct{} {
		if acpxMgr != nil {
			return acpxMgr.Done()
		}
		// Never fires when no acpx child.
		return make(chan struct{})
	}():
		logger.Warn("bundled acpx-server exited early; shutting down")
	}

	shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if acpxMgr != nil {
		if err := acpxMgr.Stop(); err != nil {
			logger.Warn("acpx-server stop", "err", err)
		}
	}
	if mcpStarted {
		_ = d.Mcp.Stop(shutdownCtx)
	}
	_ = sdpServer.Shutdown()
	return nil
}

func cmdStatus(cfg config.Config, args []string) error {
	c, err := sdp.DialConfig(cfg, 2*time.Second)
	if err != nil {
		return err
	}
	defer c.Close()

	var projects []map[string]any
	if err := c.Call("daemon.listProjects", map[string]any{}, &projects); err != nil {
		return fmt.Errorf("daemon.listProjects: %w", err)
	}
	var instances []map[string]any
	if err := c.Call("daemon.list", map[string]any{}, &instances); err != nil {
		return fmt.Errorf("daemon.list: %w", err)
	}

	fmt.Printf("snapshotd control socket: %s\n", cfg.ControlSocketPath)
	fmt.Printf("projects: %d\n", len(projects))
	for _, p := range projects {
		enc, _ := json.Marshal(p)
		fmt.Printf("  %s\n", enc)
	}
	fmt.Printf("process instances: %d\n", len(instances))
	for _, in := range instances {
		enc, _ := json.Marshal(in)
		fmt.Printf("  %s\n", enc)
	}
	return nil
}

func cmdStop(cfg config.Config, args []string) error {
	// Ask the daemon to stop itself over the SDP control socket (daemon.stop
	// -> Daemon.RequestStop, wired into cmdServe's shutdown select loop)
	// rather than signaling its PID directly. This is the only mechanism
	// that works identically on every platform: (*os.Process).Signal only
	// supports os.Kill and os.Interrupt on Windows, not SIGTERM, so a
	// PID+signal-based `stop` could never work there. Going through the
	// control socket also means "no daemon reachable" is reported the same
	// way status/launch/etc already report it, rather than as a separate
	// PID-file-missing error path.
	c, err := sdp.DialConfig(cfg, 2*time.Second)
	if err != nil {
		return fmt.Errorf("no running daemon found at %s: %w", cfg.ControlSocketPath, err)
	}
	defer c.Close()

	if err := c.Call("daemon.stop", map[string]any{}, nil); err != nil {
		return fmt.Errorf("daemon.stop: %w", err)
	}
	fmt.Println("snapshotd is shutting down")
	return nil
}

func cmdLaunch(cfg config.Config, args []string) error {
	fs := flag.NewFlagSet("launch", flag.ExitOnError)
	gui := fs.Bool("gui", false, "launch with a visible GUI instead of headless/offscreen (daemon.launch defaults to headless=1 per 08-lifecycle-and-cli.md)")
	_ = fs.Parse(args)
	if fs.NArg() < 1 {
		return fmt.Errorf("usage: snapshotd launch [--gui] <projectPath>")
	}
	projectPath := fs.Arg(0)

	c, err := sdp.DialConfig(cfg, 2*time.Second)
	if err != nil {
		return err
	}
	defer c.Close()

	params := map[string]any{"projectPath": projectPath}
	if *gui {
		// Explicit opt-out; omitting "headless" entirely lets daemon.launch
		// apply its own default (true) instead of this CLI hard-coding it.
		params["headless"] = false
	}

	var instance map[string]any
	err = c.Call("daemon.launch", params, &instance)
	if err != nil {
		return err
	}
	enc, _ := json.MarshalIndent(instance, "", "  ")
	fmt.Println(string(enc))
	return nil
}

func cmdList(cfg config.Config, args []string) error {
	c, err := sdp.DialConfig(cfg, 2*time.Second)
	if err != nil {
		return err
	}
	defer c.Close()

	var instances []map[string]any
	if err := c.Call("daemon.list", map[string]any{}, &instances); err != nil {
		return fmt.Errorf("daemon.list: %w", err)
	}
	for _, in := range instances {
		enc, _ := json.Marshal(in)
		fmt.Println(string(enc))
	}
	return nil
}

// cmdListProjects mirrors cmdList exactly, for daemon.listProjects instead
// of daemon.list -- PISO-8: the panel needs both (an instance's ProjectID
// alone has no display path/name; that lives on the Project row, per
// registry.Project's RootDir) to show which real project a live instance
// is actually for.
func cmdListProjects(cfg config.Config, args []string) error {
	c, err := sdp.DialConfig(cfg, 2*time.Second)
	if err != nil {
		return err
	}
	defer c.Close()

	var projects []map[string]any
	if err := c.Call("daemon.listProjects", map[string]any{}, &projects); err != nil {
		return fmt.Errorf("daemon.listProjects: %w", err)
	}
	for _, p := range projects {
		enc, _ := json.Marshal(p)
		fmt.Println(string(enc))
	}
	return nil
}

func cmdClose(cfg config.Config, args []string) error {
	fs := flag.NewFlagSet("close", flag.ExitOnError)
	_ = fs.Parse(args)
	if fs.NArg() < 1 {
		return fmt.Errorf("usage: snapshotd close <instanceId>")
	}
	instanceID := fs.Arg(0)

	c, err := sdp.DialConfig(cfg, 2*time.Second)
	if err != nil {
		return err
	}
	defer c.Close()

	if err := c.Call("daemon.close", map[string]any{"instanceId": instanceID}, nil); err != nil {
		return fmt.Errorf("daemon.close: %w", err)
	}
	fmt.Printf("closed instance %s\n", instanceID)
	return nil
}

// cmdMCP dispatches `snapshotd mcp <status|restart|auth|install-config>`.
func cmdMCP(cfg config.Config, args []string) error {
	if len(args) < 1 {
		return fmt.Errorf("usage: snapshotd mcp <status|restart|auth|install-config>")
	}
	switch args[0] {
	case "status":
		return cmdMCPStatus(cfg, args[1:])
	case "restart":
		return cmdMCPRestart(cfg, args[1:])
	case "auth":
		return cmdMCPAuth(cfg, args[1:])
	case "install-config":
		return cmdMCPInstallConfig(cfg, args[1:])
	default:
		return fmt.Errorf("unknown mcp subcommand %q (want status|restart|auth|install-config)", args[0])
	}
}

func cmdMCPStatus(cfg config.Config, args []string) error {
	c, err := sdp.DialConfig(cfg, 2*time.Second)
	if err != nil {
		return err
	}
	defer c.Close()

	var status map[string]any
	if err := c.Call("daemon.mcpStatus", map[string]any{}, &status); err != nil {
		return fmt.Errorf("daemon.mcpStatus: %w", err)
	}
	enc, _ := json.MarshalIndent(status, "", "  ")
	fmt.Println(string(enc))
	return nil
}

func cmdMCPRestart(cfg config.Config, args []string) error {
	fs := flag.NewFlagSet("mcp restart", flag.ExitOnError)
	bind := fs.String("bind", "", "address to rebind the MCP listener to (default: keep current address); a non-loopback address (e.g. 0.0.0.0:7777) requires `mcp auth set` to have been run first")
	_ = fs.Parse(args)

	c, err := sdp.DialConfig(cfg, 2*time.Second)
	if err != nil {
		return err
	}
	defer c.Close()

	var status map[string]any
	if err := c.Call("daemon.mcpRestart", map[string]any{"bind": *bind}, &status); err != nil {
		return fmt.Errorf("daemon.mcpRestart: %w", err)
	}
	if addr, _ := status["addr"].(string); addr != "" && !mcpsupervisor.IsLoopbackAddr(addr) {
		fmt.Fprintf(os.Stderr, "WARNING: MCP listener is now bound to %s (non-loopback). "+
			"HTTP Basic Auth credentials still travel base64-encoded, not encrypted -- "+
			"this endpoint is only as safe as the network it's reachable from. "+
			"Put it behind TLS (e.g. a reverse proxy) before exposing it beyond a trusted LAN.\n", addr)
	}
	enc, _ := json.MarshalIndent(status, "", "  ")
	fmt.Println(string(enc))
	return nil
}

func cmdMCPAuth(cfg config.Config, args []string) error {
	if len(args) < 1 || args[0] != "set" {
		return fmt.Errorf("usage: snapshotd mcp auth set --user U --password P")
	}
	fs := flag.NewFlagSet("mcp auth set", flag.ExitOnError)
	user := fs.String("user", "", "Basic Auth username")
	password := fs.String("password", "", "Basic Auth password (falls back to $SNAPSHOTD_MCP_PASSWORD if omitted -- "+
		"a shell argument is visible to other local users via `ps`/procfs, an env var generally is not)")
	_ = fs.Parse(args[1:])
	if *password == "" {
		*password = os.Getenv("SNAPSHOTD_MCP_PASSWORD")
	}
	if *user == "" || *password == "" {
		return fmt.Errorf("usage: snapshotd mcp auth set --user U [--password P | $SNAPSHOTD_MCP_PASSWORD] (both required)")
	}

	c, err := sdp.DialConfig(cfg, 2*time.Second)
	if err != nil {
		return err
	}
	defer c.Close()

	var status map[string]any
	if err := c.Call("daemon.mcpAuthSet", map[string]any{"user": *user, "password": *password}, &status); err != nil {
		return fmt.Errorf("daemon.mcpAuthSet: %w", err)
	}
	fmt.Println("MCP auth updated; run `snapshotd mcp restart` to bind beyond 127.0.0.1 if desired")
	enc, _ := json.MarshalIndent(status, "", "  ")
	fmt.Println(string(enc))
	return nil
}

func cmdMCPInstallConfig(cfg config.Config, args []string) error {
	if len(args) < 1 || args[0] != "get" {
		return fmt.Errorf("usage: snapshotd mcp install-config get")
	}

	c, err := sdp.DialConfig(cfg, 2*time.Second)
	if err != nil {
		return err
	}
	defer c.Close()

	var installConfig map[string]any
	if err := c.Call("daemon.mcpInstallConfig", map[string]any{}, &installConfig); err != nil {
		return fmt.Errorf("daemon.mcpInstallConfig: %w", err)
	}
	enc, _ := json.MarshalIndent(installConfig, "", "  ")
	fmt.Println(string(enc))
	return nil
}

func cmdInstall(cfg config.Config, args []string) error {
	// Honest stub: this sandbox/environment must not touch host system
	// services. Print exactly what a real implementation would do instead of
	// silently pretending to succeed.
	fmt.Println(`snapshotd install: NOT IMPLEMENTED for real in this build.

A real implementation would, per 08-lifecycle-and-cli.md:
  - on Linux: write a systemd unit (e.g. /etc/systemd/system/snapshotd.service)
    running "snapshotd serve" as a long-lived service, then "systemctl enable
    --now snapshotd"
  - on macOS: write a launchd plist under /Library/LaunchDaemons and load it
  - on Windows: register a Windows Service wrapping "snapshotd serve"

None of that is performed here -- this command intentionally only prints
this description and exits 0, so it is never mistaken for having actually
modified host service configuration.`)
	return nil
}

func cmdDoctor(cfg config.Config, args []string) error {
	if len(args) != 0 {
		return fmt.Errorf("usage: snapshotd doctor")
	}
	fmt.Print(acpnode.DoctorReport())
	// The Node/npm report above was the only thing `doctor` checked; it
	// never told the operator whether the daemon they'd expect this
	// command to talk to (via `status`/`stop`/`launch`, per this file's
	// own package doc comment) is actually reachable. `health.
	// SocketResponsive` already exists and is used pervasively by
	// `daemon.go`/`procmgr.go` for the exact same unix-socket dial-and-
	// close probe -- wiring it in here is a diagnostics-surface gap fix,
	// not new capability. `net.DialTimeout("unix", ...)` underneath is a
	// single cross-platform Go stdlib call (works the same on Linux/
	// macOS/modern Windows), so no OS branch is needed for this check
	// itself.
	fmt.Print(controlSocketDoctorLine(cfg.ControlSocketPath))
	return nil
}

// controlSocketDoctorLine reports whether the daemon's SDP control socket
// (the one `status`/`stop`/`launch` dial) is currently accepting
// connections, formatted to match acpnode.DoctorReport()'s plain-text
// section style.
func controlSocketDoctorLine(socketPath string) string {
	if health.SocketResponsive(socketPath, 500*time.Millisecond) {
		return fmt.Sprintf("control socket: responsive (%s)\n", socketPath)
	}
	return fmt.Sprintf(
		"control socket: not responsive (%s) -- is `%s serve` running?\n",
		socketPath, progName,
	)
}

func cmdRuntime(cfg config.Config, args []string) error {
	if len(args) < 2 || args[0] != "install" || args[1] != "node" {
		return fmt.Errorf("usage: snapshotd runtime install node [--force]")
	}
	force := false
	for _, arg := range args[2:] {
		if arg == "--force" {
			force = true
			continue
		}
		return fmt.Errorf("usage: snapshotd runtime install node [--force]")
	}
	if err := acpnode.Ensure(force); err != nil {
		return err
	}
	fmt.Print(acpnode.DoctorReport())
	return nil
}
