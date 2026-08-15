package mcpadapter_test

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"
	"time"

	mcpclient "github.com/mark3labs/mcp-go/client"
	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"

	"snapshotd/internal/mcpadapter"
	"snapshotd/internal/sapproxy"
)

// fakeHandler records dispatched calls and returns canned results, standing
// in for internal/daemon.Daemon so this test exercises only the MCP <-> SDP
// translation layer.
type fakeHandler struct {
	lastMethod string
	lastParams json.RawMessage
	bound      bool
}

func (f *fakeHandler) Dispatch(ctx context.Context, method string, params json.RawMessage) (any, error) {
	f.lastMethod = method
	f.lastParams = params
	switch method {
	case "daemon.listProjects":
		return []map[string]string{{"id": "proj-1"}}, nil
	case "daemon.launch":
		return map[string]any{"id": "inst-1", "status": "ready"}, nil
	case "daemon.close":
		return nil, nil
	case "daemon.health":
		return nil, errors.New("instance not found")
	default:
		return nil, errors.New("unexpected method in test: " + method)
	}
}

// ForwardSAP is a minimal stand-in for internal/daemon.Daemon.ForwardSAP,
// enough to prove the "sap.call" tool routes here (not Dispatch), forwards
// method/params opaquely, and that sink.Notify is reachable.
func (f *fakeHandler) ForwardSAP(ctx context.Context, sessionID string, sink sapproxy.Sink, method string, params json.RawMessage) (json.RawMessage, error) {
	f.lastMethod = method
	f.lastParams = params
	switch method {
	case "project.select":
		f.bound = true
		sink.Notify("project.dirty", json.RawMessage(`{"reason":"select"}`))
		return json.Marshal(map[string]any{"projectId": "proj-1"})
	case "sap.boom":
		return nil, errors.New("sap: boom")
	default:
		if !f.bound {
			return nil, errors.New("sap: no project selected; call project.select first")
		}
		return params, nil
	}
}

func (f *fakeHandler) UnbindSession(sessionID string) {}

func TestMCPAdapter_ToolsListedAndCallable(t *testing.T) {
	h := &fakeHandler{}
	mcpServer := mcpadapter.New(h)
	testServer := server.NewTestServer(mcpServer)
	defer testServer.Close()

	c, err := mcpclient.NewSSEMCPClient(testServer.URL + "/sse")
	if err != nil {
		t.Fatalf("new client: %v", err)
	}
	defer c.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	if err := c.Start(ctx); err != nil {
		t.Fatalf("start: %v", err)
	}
	if _, err := c.Initialize(ctx, mcp.InitializeRequest{}); err != nil {
		t.Fatalf("initialize: %v", err)
	}

	toolsResult, err := c.ListTools(ctx, mcp.ListToolsRequest{})
	if err != nil {
		t.Fatalf("list tools: %v", err)
	}
	want := map[string]bool{
		"daemon.createProject": false,
		"daemon.deleteProject": false,
		"daemon.listProjects":  false,
		"daemon.launch":        false,
		"daemon.list":          false,
		"daemon.health":        false,
		"daemon.close":         false,
		"project.open":         false,
		"project.close":        false,
		"project.create":       false,
		"project.list":         false,
		"project.clone":        false,
		"project.select":       false,
		"project.exit":         false,
	}
	for _, tl := range toolsResult.Tools {
		if _, ok := want[tl.Name]; ok {
			want[tl.Name] = true
		}
	}
	for name, seen := range want {
		if !seen {
			t.Fatalf("expected tool %s to be listed, got tools: %+v", name, toolsResult.Tools)
		}
	}
	// Not asserting an exact tool count here: New() now registers every
	// typed per-method tool (edit.*/playlist.*/filter.*/etc, ~74 with this
	// fakeHandler's audio namespace left disabled), not just the compact
	// daemon.*/sap.call/sap.search set this test originally checked for
	// exact equality against. want above only spot-checks the ones this
	// test actually exercises below.
	if len(toolsResult.Tools) <= len(want) {
		t.Fatalf("expected more than the %d spot-checked tools (typed per-method tools should also be registered), got %d: %+v", len(want), len(toolsResult.Tools), toolsResult.Tools)
	}

	// Call daemon.launch and confirm arguments + result flow through.
	callReq := mcp.CallToolRequest{}
	callReq.Params.Name = "daemon.launch"
	callReq.Params.Arguments = map[string]any{"projectId": "proj-1", "headless": true}

	res, err := c.CallTool(ctx, callReq)
	if err != nil {
		t.Fatalf("call tool: %v", err)
	}
	if res.IsError {
		t.Fatalf("unexpected tool error result: %+v", res)
	}
	if h.lastMethod != "daemon.launch" {
		t.Fatalf("expected dispatch to daemon.launch, got %s", h.lastMethod)
	}
	var gotParams map[string]any
	if err := json.Unmarshal(h.lastParams, &gotParams); err != nil {
		t.Fatalf("unmarshal dispatched params: %v", err)
	}
	if gotParams["projectId"] != "proj-1" || gotParams["headless"] != true {
		t.Fatalf("unexpected dispatched params: %+v", gotParams)
	}

	// Call daemon.health, whose fakeHandler returns an error -- should surface
	// as a tool-level error result, not a client-level transport error, per
	// mcp-go's own CallToolResult.IsError convention.
	healthReq := mcp.CallToolRequest{}
	healthReq.Params.Name = "daemon.health"
	healthReq.Params.Arguments = map[string]any{"instanceId": "missing"}
	healthRes, err := c.CallTool(ctx, healthReq)
	if err != nil {
		t.Fatalf("call tool (health): %v", err)
	}
	if !healthRes.IsError {
		t.Fatalf("expected an error tool result for daemon.health, got %+v", healthRes)
	}
}

// TestMCPAdapter_AudioToolsHiddenAndMissingBindingOverSSE replaces the old
// sap.search-based coverage (search("audio.setGain") returning no results
// when the namespace is disabled) now that sap.search no longer exists:
// the equivalent live-server behavior is that audio.* tools are absent
// from ListTools() entirely when AudioNamespaceEnabled is false (fakeHandler
// doesn't implement it, so New() treats audioEnabled as false). Also keeps
// the missing-project-binding coverage the old test had, now exercised
// through the typed edit.addTrack tool instead of sap.call.
func TestMCPAdapter_AudioToolsHiddenAndMissingBindingOverSSE(t *testing.T) {
	h := &fakeHandler{}
	mcpServer := mcpadapter.New(h)
	testServer := server.NewTestServer(mcpServer)
	defer testServer.Close()

	c, err := mcpclient.NewSSEMCPClient(testServer.URL + "/sse")
	if err != nil {
		t.Fatalf("new client: %v", err)
	}
	defer c.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := c.Start(ctx); err != nil {
		t.Fatalf("start: %v", err)
	}
	if _, err := c.Initialize(ctx, mcp.InitializeRequest{}); err != nil {
		t.Fatalf("initialize: %v", err)
	}

	toolsResult, err := c.ListTools(ctx, mcp.ListToolsRequest{})
	if err != nil {
		t.Fatalf("list tools: %v", err)
	}
	for _, tl := range toolsResult.Tools {
		if strings.HasPrefix(tl.Name, "audio.") {
			t.Fatalf("expected no audio.* tools when AudioNamespaceEnabled is false, found %s", tl.Name)
		}
	}

	req := mcp.CallToolRequest{}
	req.Params.Name = "edit.addTrack"
	req.Params.Arguments = map[string]any{"kind": "video"}
	res, err := c.CallTool(ctx, req)
	if err != nil {
		t.Fatalf("call edit.addTrack before project.select: %v", err)
	}
	if !res.IsError {
		t.Fatalf("expected missing project binding to be a tool error, got %s", toolResultText(res))
	}
	if got := toolResultText(res); !strings.Contains(got, "no project selected") || !strings.Contains(got, "project.select") {
		t.Fatalf("expected legible missing-binding error, got %q", got)
	}
}

// TestMCPAdapter_TypedToolForwardsToSAP proves a typed tool (edit.addTrack)
// routes to Handler.ForwardSAP (not Dispatch) with its fixed SAP method and
// forwards its arguments verbatim as params, and that a ForwardSAP error
// surfaces as a tool-level error result -- replaces
// TestMCPAdapter_SapCallTool_ForwardsOpaquely now that sap.call (arbitrary
// method name + free-form opaque forwarding) no longer exists; every
// method is now its own fixed-name typed tool instead.
func TestMCPAdapter_TypedToolForwardsToSAP(t *testing.T) {
	h := &fakeHandler{}
	mcpServer := mcpadapter.New(h)
	testServer := server.NewTestServer(mcpServer)
	defer testServer.Close()

	c, err := mcpclient.NewSSEMCPClient(testServer.URL + "/sse")
	if err != nil {
		t.Fatalf("new client: %v", err)
	}
	defer c.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := c.Start(ctx); err != nil {
		t.Fatalf("start: %v", err)
	}
	if _, err := c.Initialize(ctx, mcp.InitializeRequest{}); err != nil {
		t.Fatalf("initialize: %v", err)
	}

	selectReq := mcp.CallToolRequest{}
	selectReq.Params.Name = "project.select"
	selectReq.Params.Arguments = map[string]any{"projectId": "proj-1"}
	selectRes, err := c.CallTool(ctx, selectReq)
	if err != nil {
		t.Fatalf("call project.select: %v", err)
	}
	if selectRes.IsError {
		t.Fatalf("unexpected error result: %+v", selectRes)
	}
	if h.lastMethod != "project.select" {
		t.Fatalf("expected ForwardSAP dispatch to project.select, got %s", h.lastMethod)
	}

	echoReq := mcp.CallToolRequest{}
	echoReq.Params.Name = "edit.addTrack"
	echoReq.Params.Arguments = map[string]any{"kind": "video"}
	echoRes, err := c.CallTool(ctx, echoReq)
	if err != nil {
		t.Fatalf("call edit.addTrack: %v", err)
	}
	if echoRes.IsError {
		t.Fatalf("unexpected error result: %+v", echoRes)
	}
	if h.lastMethod != "edit.addTrack" {
		t.Fatalf("expected ForwardSAP dispatch to edit.addTrack, got %s", h.lastMethod)
	}
	var gotParams map[string]any
	if err := json.Unmarshal(h.lastParams, &gotParams); err != nil {
		t.Fatalf("unmarshal forwarded params: %v", err)
	}
	if gotParams["kind"] != "video" {
		t.Fatalf("expected arguments forwarded verbatim as SAP params, got %+v", gotParams)
	}
}

// TestMCPAdapter_AddTrackIndexForwarded pins edit.addTrack's optional "index"
// argument to the SAP wire: when supplied it must round-trip into the
// forwarded params as camelCase "index" with the same value (so sap-rust
// inserts at that slot), and when omitted the key must be absent entirely
// rather than defaulted to 0 -- absence is what preserves the append
// behavior every existing caller relies on.
func TestMCPAdapter_AddTrackIndexForwarded(t *testing.T) {
	h := &fakeHandler{}
	mcpServer := mcpadapter.New(h)
	testServer := server.NewTestServer(mcpServer)
	defer testServer.Close()

	c, err := mcpclient.NewSSEMCPClient(testServer.URL + "/sse")
	if err != nil {
		t.Fatalf("new client: %v", err)
	}
	defer c.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := c.Start(ctx); err != nil {
		t.Fatalf("start: %v", err)
	}
	if _, err := c.Initialize(ctx, mcp.InitializeRequest{}); err != nil {
		t.Fatalf("initialize: %v", err)
	}

	selectReq := mcp.CallToolRequest{}
	selectReq.Params.Name = "project.select"
	selectReq.Params.Arguments = map[string]any{"projectId": "proj-1"}
	if res, err := c.CallTool(ctx, selectReq); err != nil || res.IsError {
		t.Fatalf("project.select: err=%v result=%s", err, toolResultText(res))
	}

	callAddTrack := func(t *testing.T, args map[string]any) map[string]any {
		t.Helper()
		req := mcp.CallToolRequest{}
		req.Params.Name = "edit.addTrack"
		req.Params.Arguments = args
		res, err := c.CallTool(ctx, req)
		if err != nil {
			t.Fatalf("call edit.addTrack %+v: %v", args, err)
		}
		if res.IsError {
			t.Fatalf("edit.addTrack %+v returned tool error: %s", args, toolResultText(res))
		}
		if h.lastMethod != "edit.addTrack" {
			t.Fatalf("expected ForwardSAP dispatch to edit.addTrack, got %s", h.lastMethod)
		}
		var got map[string]any
		if err := json.Unmarshal(h.lastParams, &got); err != nil {
			t.Fatalf("unmarshal forwarded params: %v", err)
		}
		return got
	}

	t.Run("index forwarded", func(t *testing.T) {
		got := callAddTrack(t, map[string]any{"kind": "video", "index": 2})
		if got["kind"] != "video" {
			t.Fatalf("expected kind forwarded, got %+v", got)
		}
		if string(mustJSON(t, got["index"])) != string(mustJSON(t, 2)) {
			t.Fatalf("expected index=2 forwarded to SAP, got %+v", got)
		}
	})

	t.Run("index omitted", func(t *testing.T) {
		got := callAddTrack(t, map[string]any{"kind": "audio"})
		if got["kind"] != "audio" {
			t.Fatalf("expected kind forwarded, got %+v", got)
		}
		if _, ok := got["index"]; ok {
			t.Fatalf("expected no index key when omitted (append behavior), got %+v", got)
		}
	})
}

// TestMCPAdapter_NewTypedMethods verifies the newly exposed native surfaces
// are real MCP tools (not just entries in the documentation schema), forward
// their arguments to SAP, and remain callable over the same SSE transport as
// existing tools.
func TestMCPAdapter_NewTypedMethods(t *testing.T) {
	h := &fakeHandler{}
	mcpServer := mcpadapter.New(h)
	testServer := server.NewTestServer(mcpServer)
	defer testServer.Close()

	c, err := mcpclient.NewSSEMCPClient(testServer.URL + "/sse")
	if err != nil {
		t.Fatalf("new client: %v", err)
	}
	defer c.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := c.Start(ctx); err != nil {
		t.Fatalf("start: %v", err)
	}
	if _, err := c.Initialize(ctx, mcp.InitializeRequest{}); err != nil {
		t.Fatalf("initialize: %v", err)
	}

	selectReq := mcp.CallToolRequest{}
	selectReq.Params.Name = "project.select"
	selectReq.Params.Arguments = map[string]any{"projectId": "proj-1"}
	if res, err := c.CallTool(ctx, selectReq); err != nil || res.IsError {
		t.Fatalf("project.select: err=%v result=%s", err, toolResultText(res))
	}

	cases := []struct {
		name   string
		params map[string]any
	}{
		{name: "filter.describe", params: map[string]any{"mltService": "brightness"}},
		{name: "subtitles.setStyle", params: map[string]any{"style": "italic", "valign": "bottom", "halign": "center", "size": 24}},
		{name: "playback.fastForward", params: map[string]any{}},
		{name: "edit.setClipSpeed", params: map[string]any{"trackIndex": 0, "clipIndex": 1, "speed": 1.5}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			req := mcp.CallToolRequest{}
			req.Params.Name = tc.name
			req.Params.Arguments = tc.params
			res, err := c.CallTool(ctx, req)
			if err != nil {
				t.Fatalf("call %s: %v", tc.name, err)
			}
			if res.IsError {
				t.Fatalf("%s returned tool error: %s", tc.name, toolResultText(res))
			}
			if h.lastMethod != tc.name {
				t.Fatalf("expected ForwardSAP method %q, got %q", tc.name, h.lastMethod)
			}
			var got map[string]any
			if err := json.Unmarshal(h.lastParams, &got); err != nil {
				t.Fatalf("decode %s params: %v", tc.name, err)
			}
			for key, want := range tc.params {
				if string(mustJSON(t, got[key])) != string(mustJSON(t, want)) {
					t.Fatalf("%s param %s=%v, want %v (all=%+v)", tc.name, key, got[key], want, got)
				}
			}
		})
	}

	// Closed enum values are enforced at the MCP boundary before SAP sees the
	// request; this guards against silently accepting a style the C++ layer
	// cannot apply.
	invalid := mcp.CallToolRequest{}
	invalid.Params.Name = "subtitles.setStyle"
	invalid.Params.Arguments = map[string]any{"style": "oblique"}
	invalidRes, err := c.CallTool(ctx, invalid)
	if err != nil {
		t.Fatalf("call invalid subtitles.setStyle: %v", err)
	}
	if !invalidRes.IsError {
		t.Fatalf("expected invalid subtitle style enum to be rejected, got %s", toolResultText(invalidRes))
	}
}
