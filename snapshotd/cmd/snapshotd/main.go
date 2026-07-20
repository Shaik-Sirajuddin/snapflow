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
	"errors"
	"flag"
	"fmt"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"syscall"
	"time"

	"snapshotd/internal/acpxmgr"
	"snapshotd/internal/config"
	"snapshotd/internal/daemon"
	"snapshotd/internal/daemonlock"
	"snapshotd/internal/mcpadapter"
	"snapshotd/internal/sdp"
)

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
	case "install":
		err = cmdInstall(cfg, os.Args[2:])
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
  snapshotd install                      print what installing a system service would do (not implemented for real)`)
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

	pidPath := cfg.ControlSocketPath + ".pid"
	if err := os.WriteFile(pidPath, []byte(fmt.Sprintf("%d\n", os.Getpid())), 0o644); err != nil {
		logger.Warn("could not write pidfile (snapshotd stop will not be able to find this process)", "path", pidPath, "err", err)
	}
	defer os.Remove(pidPath)

	// Startup reconciliation, per 07-daemon-persistence.md: reconnect to
	// already-"ready" process instances rather than assuming a fresh daemon
	// process means every child needs relaunching.
	if _, err := d.Reconcile(context.Background()); err != nil {
		logger.Warn("startup reconciliation failed", "err", err)
	}

	sdpServer := &sdp.Server{SocketPath: cfg.ControlSocketPath, Handler: d, Log: logger}
	sdpErrCh := make(chan error, 1)
	go func() {
		sdpErrCh <- sdpServer.ListenAndServe()
	}()
	logger.Info("SDP control socket listening", "path", cfg.ControlSocketPath)

	var mcpServer *mcpadapter.SSEServer
	mcpErrCh := make(chan error, 1)
	if !*noMCP {
		mcpServer = mcpadapter.NewSSEServer(d, cfg.MCPSSEAddr)
		go func() {
			if err := mcpServer.Start(); err != nil && !errors.Is(err, http.ErrServerClosed) {
				mcpErrCh <- err
			}
		}()
		logger.Info("MCP SSE endpoint listening", "addr", cfg.MCPSSEAddr)
	}

	// Optional bundled acpx-server: single gateway owner under snapshotd serve.
	var acpxMgr *acpxmgr.Manager
	if cfg.AcpxEnabled && !*noMCP {
		if cfg.AcpxBinPath == "" {
			logger.Warn("SNAPSHOTD_ACPX_ENABLED but no acpx-server binary found; skip spawn")
		} else {
			startCtx, startCancel := context.WithTimeout(context.Background(), 10*time.Second)
			mgr, err := acpxmgr.Start(startCtx, acpxmgr.Config{
				BinPath:        cfg.AcpxBinPath,
				HttpBind:       cfg.AcpxHttpBind,
				ConfigPath:     cfg.AcpxConfigPath,
				DbPath:         filepath.Join(cfg.HomeDir, "acpx.sqlite3"),
				McpURL:         acpxmgr.McpHTTPURL(cfg.MCPSSEAddr),
				DefaultAgentID: "default",
				BackendCmd:     cfg.AcpxBackendCmd,
				Log:            logger,
			})
			startCancel()
			if err != nil {
				logger.Error("failed to start bundled acpx-server", "err", err)
			} else {
				acpxMgr = mgr
				logger.Info("bundled acpx-server started",
					"bin", cfg.AcpxBinPath,
					"bind", cfg.AcpxHttpBind,
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
	case err := <-sdpErrCh:
		if err != nil {
			logger.Error("SDP server exited", "err", err)
		}
	case err := <-mcpErrCh:
		logger.Error("MCP server exited", "err", err)
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
	if mcpServer != nil {
		_ = mcpServer.Shutdown(shutdownCtx)
	}
	_ = sdpServer.Shutdown()
	return nil
}

func cmdStatus(cfg config.Config, args []string) error {
	c, err := sdp.Dial(cfg.ControlSocketPath, 2*time.Second)
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
	// v1 simplification: there is no dedicated daemon.stop SDP method (not
	// in 06's primitives table) -- the CLI sends the process a graceful
	// termination signal directly instead of round-tripping through SDP.
	// This still matches the "thin client against an already-running
	// daemon" model: if no daemon is running, the status check below fails
	// to connect and we report that cleanly rather than silently no-op'ing.
	c, err := sdp.Dial(cfg.ControlSocketPath, 2*time.Second)
	if err != nil {
		return fmt.Errorf("no running daemon found at %s: %w", cfg.ControlSocketPath, err)
	}
	defer c.Close()

	pid, err := readServePID(cfg)
	if err != nil {
		return fmt.Errorf("daemon is reachable but its PID file is missing/unreadable (%w); send SIGTERM to the `snapshotd serve` process manually", err)
	}
	proc, err := os.FindProcess(pid)
	if err != nil {
		return err
	}
	if err := proc.Signal(syscall.SIGTERM); err != nil {
		return fmt.Errorf("signaling daemon pid %d: %w", pid, err)
	}
	fmt.Printf("sent SIGTERM to snapshotd (pid %d)\n", pid)
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

	c, err := sdp.Dial(cfg.ControlSocketPath, 2*time.Second)
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

// readServePID reads a pidfile written by `snapshotd serve` next to the
// control socket. Kept intentionally simple (no locking beyond what os.Create
// gives us) -- this is a v1 convenience for `snapshotd stop`, not a general
// process-supervision primitive.
func readServePID(cfg config.Config) (int, error) {
	data, err := os.ReadFile(cfg.ControlSocketPath + ".pid")
	if err != nil {
		return 0, err
	}
	var pid int
	if _, err := fmt.Sscanf(string(data), "%d", &pid); err != nil {
		return 0, err
	}
	return pid, nil
}
