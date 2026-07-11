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
		"sap.call":             false,
		"sap.search":           false,
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
	if len(toolsResult.Tools) != len(want) {
		t.Fatalf("expected compact initial tool list of %d tools, got %d: %+v", len(want), len(toolsResult.Tools), toolsResult.Tools)
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

func TestMCPAdapter_SapSearchAndMissingBindingOverSSE(t *testing.T) {
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

	search := func(query string) []map[string]any {
		t.Helper()
		req := mcp.CallToolRequest{}
		req.Params.Name = "sap.search"
		req.Params.Arguments = map[string]any{"query": query}
		res, err := c.CallTool(ctx, req)
		if err != nil {
			t.Fatalf("call sap.search(%q): %v", query, err)
		}
		if res.IsError {
			t.Fatalf("sap.search(%q) returned an error: %s", query, toolResultText(res))
		}
		var matches []map[string]any
		if err := json.Unmarshal([]byte(toolResultText(res)), &matches); err != nil {
			t.Fatalf("decode sap.search(%q): %v; raw=%s", query, err, toolResultText(res))
		}
		return matches
	}

	crossfade := search("transitions.addCrossfade")
	if len(crossfade) != 1 || crossfade[0]["method"] != "transitions.addCrossfade" {
		t.Fatalf("expected crossfade search result, got %+v", crossfade)
	}
	title := search("generator.createTitle")
	if len(title) != 1 || title[0]["method"] != "generator.createTitle" {
		t.Fatalf("expected title search result, got %+v", title)
	}
	if audio := search("audio.setGain"); len(audio) != 0 {
		t.Fatalf("expected audio methods to be undiscoverable, got %+v", audio)
	}

	req := mcp.CallToolRequest{}
	req.Params.Name = "sap.call"
	req.Params.Arguments = map[string]any{
		"method": "edit.addTrack",
		"params": map[string]any{"kind": "video"},
	}
	res, err := c.CallTool(ctx, req)
	if err != nil {
		t.Fatalf("call sap.call before project.select: %v", err)
	}
	if !res.IsError {
		t.Fatalf("expected missing project binding to be a tool error, got %s", toolResultText(res))
	}
	if got := toolResultText(res); !strings.Contains(got, "no project selected") || !strings.Contains(got, "project.select") {
		t.Fatalf("expected legible missing-binding error, got %q", got)
	}
}

// TestMCPAdapter_SapCallTool_ForwardsOpaquely proves the "sap.call" tool
// routes to Handler.ForwardSAP (not Dispatch), forwards method+params
// verbatim, and surfaces a ForwardSAP error as a tool-level error result.
func TestMCPAdapter_SapCallTool_ForwardsOpaquely(t *testing.T) {
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
	selectReq.Params.Name = "sap.call"
	selectReq.Params.Arguments = map[string]any{
		"method": "project.select",
		"params": map[string]any{"projectId": "proj-1"},
	}
	selectRes, err := c.CallTool(ctx, selectReq)
	if err != nil {
		t.Fatalf("call sap.call(project.select): %v", err)
	}
	if selectRes.IsError {
		t.Fatalf("unexpected error result: %+v", selectRes)
	}
	if h.lastMethod != "project.select" {
		t.Fatalf("expected ForwardSAP dispatch to project.select, got %s", h.lastMethod)
	}

	echoReq := mcp.CallToolRequest{}
	echoReq.Params.Name = "sap.call"
	echoReq.Params.Arguments = map[string]any{
		"method": "edit.addTrack",
		"params": map[string]any{"kind": "video"},
	}
	echoRes, err := c.CallTool(ctx, echoReq)
	if err != nil {
		t.Fatalf("call sap.call(edit.addTrack): %v", err)
	}
	if echoRes.IsError {
		t.Fatalf("unexpected error result: %+v", echoRes)
	}
	var gotParams map[string]any
	if err := json.Unmarshal(h.lastParams, &gotParams); err != nil {
		t.Fatalf("unmarshal forwarded params: %v", err)
	}
	if gotParams["kind"] != "video" {
		t.Fatalf("expected opaque params forwarded verbatim, got %+v", gotParams)
	}

	boomReq := mcp.CallToolRequest{}
	boomReq.Params.Name = "sap.call"
	boomReq.Params.Arguments = map[string]any{"method": "sap.boom", "params": map[string]any{}}
	boomRes, err := c.CallTool(ctx, boomReq)
	if err != nil {
		t.Fatalf("call sap.call(sap.boom): %v", err)
	}
	if !boomRes.IsError {
		t.Fatalf("expected an error tool result for sap.boom, got %+v", boomRes)
	}

	missingMethodReq := mcp.CallToolRequest{}
	missingMethodReq.Params.Name = "sap.call"
	missingMethodReq.Params.Arguments = map[string]any{}
	missingRes, err := c.CallTool(ctx, missingMethodReq)
	if err != nil {
		t.Fatalf("call sap.call with no method: %v", err)
	}
	if !missingRes.IsError {
		t.Fatalf("expected an error tool result when \"method\" is omitted")
	}
}
