package mcpadapter_test

// Shared test scaffolding for 11-e2e-scenario-tests.md's Phase B (same
// project, concurrency) and Phase C (different projects, isolation)
// integration tests: a small mcpAgent wrapper around a real MCP/SSE client
// connection standing in for one independent agent process, per doc 11's
// "two independent Claude Code processes" framing. Split out from the two
// phase test files since both need the exact same real-client plumbing.

import (
	"context"
	"encoding/json"
	"os/exec"
	"sync"
	"testing"
	"time"

	mcpclient "github.com/mark3labs/mcp-go/client"
	"github.com/mark3labs/mcp-go/mcp"
)

// mcpAgent is one real, independent MCP/SSE client connection (its own
// session, its own notification stream) against the shared test daemon --
// exactly the transport shape a real agent uses, per the existing
// *_realsaprust_test.go files' pattern, just factored so two (or more) of
// them can be driven side by side in the same test.
type mcpAgent struct {
	t      *testing.T
	ctx    context.Context
	client *mcpclient.Client

	mu     sync.Mutex
	notifs []mcp.JSONRPCNotification
}

// newMCPAgent dials a fresh SSE connection to sseURL, initializes it, and
// starts recording every notification it receives -- a new "agent process"
// joining the daemon.
func newMCPAgent(t *testing.T, ctx context.Context, sseURL string) *mcpAgent {
	t.Helper()
	c, err := mcpclient.NewSSEMCPClient(sseURL)
	if err != nil {
		t.Fatalf("new MCP client: %v", err)
	}
	a := &mcpAgent{t: t, ctx: ctx, client: c}
	c.OnNotification(func(n mcp.JSONRPCNotification) {
		a.mu.Lock()
		defer a.mu.Unlock()
		a.notifs = append(a.notifs, n)
	})
	if err := c.Start(ctx); err != nil {
		t.Fatalf("start MCP client: %v", err)
	}
	if _, err := c.Initialize(ctx, mcp.InitializeRequest{}); err != nil {
		t.Fatalf("initialize MCP client: %v", err)
	}
	return a
}

// Close tears down this agent's MCP client connection. Callers must
// explicitly `defer agent.Close()` themselves (registered *after* the test
// server's own `defer testServer.Close()`) rather than relying on
// t.Cleanup, which runs strictly after the test function's own defers --
// too late here, since httptest.Server.Close blocks (and eventually just
// hangs, per net/http/httptest's own "blocked in Close" diagnostic) waiting
// for any still-open SSE connections to finish, so clients must close
// first.
func (a *mcpAgent) Close() {
	_ = a.client.Close()
}

// sapCall drives the typed tool named after the given SAP method (e.g.
// "edit.addTrack" calls the tool literally named "edit.addTrack") and
// fails the test on any transport or SAP-level error -- the happy-path
// helper. Named "sapCall" rather than renamed for the typed-tool era so
// every existing call site (method name + params map) kept working
// unchanged when the underlying generic "sap.call" passthrough tool was
// dropped from the live server in favor of one tool per method (see
// mcpadapter.go's New() doc comment).
func (a *mcpAgent) sapCall(method string, params map[string]any) map[string]any {
	a.t.Helper()
	req := mcp.CallToolRequest{}
	req.Params.Name = method
	req.Params.Arguments = params
	res, err := a.client.CallTool(a.ctx, req)
	if err != nil {
		a.t.Fatalf("%s: transport error: %v", method, err)
	}
	if res.IsError {
		a.t.Fatalf("%s returned an error result: %s", method, toolResultText(res))
	}
	return decodeToolResultJSON(a.t, res)
}

// sapCallExpectError is like sapCall but asserts the call comes back as a
// clean SAP-level error result (not a transport error, hang, or crash) --
// used by Phase C's cross-project rejection assertions. Returns the error
// text for the caller to log/inspect.
func (a *mcpAgent) sapCallExpectError(method string, params map[string]any) string {
	a.t.Helper()
	req := mcp.CallToolRequest{}
	req.Params.Name = method
	req.Params.Arguments = params
	res, err := a.client.CallTool(a.ctx, req)
	if err != nil {
		a.t.Fatalf("%s: transport error (expected a clean SAP error result instead): %v", method, err)
	}
	if !res.IsError {
		a.t.Fatalf("%s expected an error result, got success: %s", method, toolResultText(res))
	}
	return toolResultText(res)
}

// notificationCount returns how many notifications this agent has received
// so far (a snapshot, safe to call concurrently with delivery).
func (a *mcpAgent) notificationCount() int {
	a.mu.Lock()
	defer a.mu.Unlock()
	return len(a.notifs)
}

// notificationsSince returns a copy of every notification received at or
// after index start (see notificationCount, used as a "since" marker).
func (a *mcpAgent) notificationsSince(start int) []mcp.JSONRPCNotification {
	a.mu.Lock()
	defer a.mu.Unlock()
	if start >= len(a.notifs) {
		return nil
	}
	out := make([]mcp.JSONRPCNotification, len(a.notifs)-start)
	copy(out, a.notifs[start:])
	return out
}

// waitForSAPNotification blocks until this agent has received a
// "sap.notification" MCP notification wrapping a SAP method equal to
// sapMethod, or timeout elapses. Returns the wrapped SAP params
// (AdditionalFields of the MCP notification's own params) and whether one
// was found.
func (a *mcpAgent) waitForSAPNotification(sapMethod string, timeout time.Duration) (map[string]any, bool) {
	a.t.Helper()
	deadline := time.Now().Add(timeout)
	for {
		a.mu.Lock()
		for _, n := range a.notifs {
			if n.Method != "sap.notification" {
				continue
			}
			if m, _ := n.Params.AdditionalFields["method"].(string); m == sapMethod {
				a.mu.Unlock()
				return n.Params.AdditionalFields, true
			}
		}
		a.mu.Unlock()
		if time.Now().After(deadline) {
			return nil, false
		}
		time.Sleep(50 * time.Millisecond)
	}
}

// decodeArrayResult unmarshals a tool result body that server-side
// wrapArrayResult (mcpadapter.go) has wrapped as {"items": [...]}, and
// returns the underlying items. All SAP array-shaped results (edit.list*,
// playlist.list, jobs.list, sap.search, daemon.list, daemon.listProjects,
// ...) go through wrapArrayResult on the server, so every test-side decode
// of an array result must unwrap through this helper rather than
// unmarshaling directly into []map[string]any.
func decodeArrayResult(t *testing.T, raw string) []map[string]any {
	t.Helper()
	var wrapped struct {
		Items []map[string]any `json:"items"`
	}
	if err := json.Unmarshal([]byte(raw), &wrapped); err != nil {
		t.Fatalf("decode wrapped array result: %v (raw: %s)", err, raw)
	}
	return wrapped.Items
}

// sapCallList is like sapCall but for SAP methods whose result is a JSON
// array (e.g. edit.listClips, edit.listTracks, playlist.list) rather than
// an object.
func (a *mcpAgent) sapCallList(method string, params map[string]any) []map[string]any {
	a.t.Helper()
	req := mcp.CallToolRequest{}
	req.Params.Name = method
	req.Params.Arguments = params
	res, err := a.client.CallTool(a.ctx, req)
	if err != nil {
		a.t.Fatalf("%s: transport error: %v", method, err)
	}
	if res.IsError {
		a.t.Fatalf("%s returned an error result: %s", method, toolResultText(res))
	}
	return decodeArrayResult(a.t, toolResultText(res))
}

// requireFFmpegTools skips the test if ffmpeg/ffprobe aren't on PATH --
// both Phase B and Phase C need them to generate/verify real media.
func requireFFmpegTools(t *testing.T) {
	t.Helper()
	if _, err := exec.LookPath("ffmpeg"); err != nil {
		t.Skip("ffmpeg not on PATH; required to generate synthetic test sources")
	}
	if _, err := exec.LookPath("ffprobe"); err != nil {
		t.Skip("ffprobe not on PATH; required to verify exported files")
	}
}
