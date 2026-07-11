package sdp_test

import (
	"context"
	"encoding/json"
	"errors"
	"net"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"snapshotd/internal/sapproxy"
	"snapshotd/internal/sdp"
)

// fakeHandler is a minimal Handler used to test the wire protocol/server in
// isolation from the real daemon core.
type fakeHandler struct {
	mu           sync.Mutex
	boundProject map[string]string // sessionID -> projectID, mirroring internal/daemon.ForwardSAP's project.select bookkeeping
	unbound      []string
}

func (f *fakeHandler) Dispatch(ctx context.Context, method string, params json.RawMessage) (any, error) {
	switch method {
	case "daemon.echo":
		var p map[string]any
		_ = json.Unmarshal(params, &p)
		return p, nil
	case "daemon.boom":
		return nil, errors.New("boom")
	default:
		return nil, errors.New("unknown method")
	}
}

// ForwardSAP is a minimal stand-in for internal/daemon.Daemon.ForwardSAP:
// enough to prove internal/sdp.Server routes non-"daemon."-prefixed methods
// here (rather than to Dispatch), passes the caller-supplied sink through,
// and surfaces a *sapproxy.RPCError with its code preserved.
func (f *fakeHandler) ForwardSAP(ctx context.Context, sessionID string, sink sapproxy.Sink, method string, params json.RawMessage) (json.RawMessage, error) {
	f.mu.Lock()
	if f.boundProject == nil {
		f.boundProject = make(map[string]string)
	}
	f.mu.Unlock()

	switch method {
	case "project.select":
		var p struct {
			ProjectID string `json:"projectId"`
		}
		_ = json.Unmarshal(params, &p)
		f.mu.Lock()
		f.boundProject[sessionID] = p.ProjectID
		f.mu.Unlock()
		sink.Notify("project.dirty", json.RawMessage(`{"reason":"select"}`))
		return json.Marshal(map[string]any{"projectId": p.ProjectID})
	case "sap.boom":
		return nil, &sapproxy.RPCError{Code: -32004, Message: "not found"}
	case "sap.echo":
		return params, nil
	default:
		return nil, errors.New("sap: unknown method " + method)
	}
}

func (f *fakeHandler) UnbindSession(sessionID string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.unbound = append(f.unbound, sessionID)
}

func TestServer_RoundTrip(t *testing.T) {
	sockPath := filepath.Join(t.TempDir(), "control.sock")
	srv := &sdp.Server{SocketPath: sockPath, Handler: &fakeHandler{}}

	go func() {
		_ = srv.ListenAndServe()
	}()
	defer srv.Shutdown()

	// Wait for the socket to be accepting connections.
	deadline := time.Now().Add(2 * time.Second)
	var client *sdp.Client
	var err error
	for time.Now().Before(deadline) {
		client, err = sdp.Dial(sockPath, 100*time.Millisecond)
		if err == nil {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer client.Close()

	var out map[string]any
	if err := client.Call("daemon.echo", map[string]any{"hello": "world"}, &out); err != nil {
		t.Fatalf("call: %v", err)
	}
	if out["hello"] != "world" {
		t.Fatalf("unexpected echo result: %+v", out)
	}

	err = client.Call("daemon.boom", map[string]any{}, nil)
	if err == nil {
		t.Fatalf("expected error from daemon.boom")
	}

	err = client.Call("daemon.nope", map[string]any{}, nil)
	if err == nil {
		t.Fatalf("expected error from unknown method")
	}
}

// rawSDPConn is a tiny newline-delimited JSON client that, unlike
// sdp.Client, does not assume the next line off the wire is always the
// response to the request it just sent -- it can also be an async
// Notification frame (no "id" key). That's needed here specifically because
// this test's fakeHandler.ForwardSAP fires a notification synchronously,
// before its own response is written, exercising exactly the interleaving
// internal/sdp.connSink exists to handle.
type rawSDPConn struct {
	nc  net.Conn
	dec *json.Decoder
}

func dialRaw(t *testing.T, sockPath string) *rawSDPConn {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	var nc net.Conn
	var err error
	for time.Now().Before(deadline) {
		nc, err = net.Dial("unix", sockPath)
		if err == nil {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	return &rawSDPConn{nc: nc, dec: json.NewDecoder(nc)}
}

func (c *rawSDPConn) send(method string, params any, id int) {
	paramsRaw, _ := json.Marshal(params)
	req := sdp.Request{JSONRPC: "2.0", ID: json.RawMessage(idJSON(id)), Method: method, Params: paramsRaw}
	line, _ := json.Marshal(req)
	line = append(line, '\n')
	if _, err := c.nc.Write(line); err != nil {
		panic(err)
	}
}

// nextFrame reads and classifies the next line: a Notification has a
// "method" key and no "id" key; a Response has an "id" key.
func (c *rawSDPConn) nextFrame(t *testing.T) (isNotification bool, notif sdp.Notification, resp sdp.Response) {
	t.Helper()
	var raw json.RawMessage
	if err := c.dec.Decode(&raw); err != nil {
		t.Fatalf("decode frame: %v", err)
	}
	var probe struct {
		ID     json.RawMessage `json:"id"`
		Method string          `json:"method"`
	}
	_ = json.Unmarshal(raw, &probe)
	if len(probe.ID) == 0 && probe.Method != "" {
		_ = json.Unmarshal(raw, &notif)
		return true, notif, sdp.Response{}
	}
	_ = json.Unmarshal(raw, &resp)
	return false, sdp.Notification{}, resp
}

func idJSON(id int) []byte {
	b, _ := json.Marshal(id)
	return b
}

// TestServer_ForwardsNonDaemonMethods proves internal/sdp.Server routes any
// non-"daemon."-prefixed method (e.g. sap.echo, standing in for
// project.*/edit.*/... in this isolated test) to Handler.ForwardSAP instead
// of Dispatch, that a *sapproxy.RPCError's code survives the round trip, and
// that sink.Notify calls land as async Notification frames on the same
// connection, interleaved with ordinary responses.
func TestServer_ForwardsNonDaemonMethods(t *testing.T) {
	sockPath := filepath.Join(t.TempDir(), "control.sock")
	h := &fakeHandler{}
	srv := &sdp.Server{SocketPath: sockPath, Handler: h}

	go func() { _ = srv.ListenAndServe() }()
	defer srv.Shutdown()

	c := dialRaw(t, sockPath)
	defer c.nc.Close()

	// project.select triggers the fakeHandler's sink.Notify call
	// synchronously before it returns its own response, so the very next
	// frame on the wire is the notification, followed by the response.
	c.send("project.select", map[string]any{"projectId": "proj-1"}, 1)

	sawNotification := false
	var selectResp sdp.Response
	for i := 0; i < 2; i++ {
		isNotif, notif, resp := c.nextFrame(t)
		if isNotif {
			sawNotification = true
			if notif.Method != "project.dirty" {
				t.Fatalf("unexpected notification method: %s", notif.Method)
			}
			continue
		}
		selectResp = resp
	}
	if !sawNotification {
		t.Fatalf("expected a fanned-out notification frame on the same connection")
	}
	selectOut, _ := json.Marshal(selectResp.Result)
	var selectMap map[string]any
	_ = json.Unmarshal(selectOut, &selectMap)
	if selectMap["projectId"] != "proj-1" {
		t.Fatalf("unexpected project.select result: %+v", selectResp)
	}

	c.send("sap.echo", map[string]any{"hello": "world"}, 2)
	_, _, echoResp := c.nextFrame(t)
	echoOutRaw, _ := json.Marshal(echoResp.Result)
	var echoOut map[string]any
	_ = json.Unmarshal(echoOutRaw, &echoOut)
	if echoOut["hello"] != "world" {
		t.Fatalf("expected opaque params echoed back verbatim, got %+v", echoResp)
	}

	c.send("sap.boom", map[string]any{}, 3)
	_, _, boomResp := c.nextFrame(t)
	if boomResp.Error == nil {
		t.Fatalf("expected an error from sap.boom")
	}
	if boomResp.Error.Code != -32004 || !strings.Contains(boomResp.Error.Message, "not found") {
		t.Fatalf("expected the RPCError's code/message to survive the round trip, got: %+v", boomResp.Error)
	}
}
