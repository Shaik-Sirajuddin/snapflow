package sapproxy

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"net"
	"sync"
	"sync/atomic"
)

// Sink receives notifications fanned out from a project's SAP connection --
// one implementation per transport (internal/sdp's raw newline-delimited
// clients, internal/mcpadapter's SSE clients), per 06-daemon-mcp-proxy.md's
// "comprehensive fan-out" requirement (doc 05/06/11 Phase B).
type Sink interface {
	// Notify delivers one opaque SAP notification (method + raw params,
	// e.g. "edit.changed", "project.dirty") to whatever transport this
	// session is using. Implementations must not block indefinitely --
	// Notify is called synchronously from the connection's single read
	// loop, so a slow/blocked sink would stall delivery to every other
	// session sharing the same pooled connection.
	Notify(method string, params json.RawMessage)
}

// Conn is a single Content-Length-framed JSON-RPC 2.0 connection to a
// running sap-rust instance. Safe for concurrent Call use; exactly one
// background goroutine (started by dial) owns reading frames off the wire.
type Conn struct {
	nc net.Conn

	writeMu sync.Mutex

	nextID  int64
	pending sync.Map // string(id JSON) -> chan inboundFrame

	// onNotification is set by Router right after dialing, before the
	// connection is published to any caller -- see router.go.
	onNotification func(method string, params json.RawMessage)

	closeOnce sync.Once
	closed    chan struct{}
	closeErr  error
}

func dial(ctx context.Context, socketPath string) (*Conn, error) {
	d := net.Dialer{}
	nc, err := d.DialContext(ctx, "unix", socketPath)
	if err != nil {
		return nil, fmt.Errorf("sapproxy: dial %s: %w", socketPath, err)
	}
	c := &Conn{nc: nc, closed: make(chan struct{})}
	go c.readLoop()
	return c, nil
}

// Dial opens a connection to socketPath and performs the sap.hello
// handshake with token, mirroring exactly what a direct SAP client would do
// per 01-jsonrpc-spec.md's session-binding model (sap.hello must be the
// first thing accepted on a new connection).
func Dial(ctx context.Context, socketPath, token string) (*Conn, error) {
	c, err := dial(ctx, socketPath)
	if err != nil {
		return nil, err
	}
	helloParams, _ := json.Marshal(map[string]string{"token": token})
	if _, err := c.Call(ctx, "sap.hello", helloParams); err != nil {
		c.Close()
		return nil, fmt.Errorf("sapproxy: sap.hello: %w", err)
	}
	return c, nil
}

func (c *Conn) readLoop() {
	r := bufio.NewReader(c.nc)
	for {
		raw, err := readFramed(r)
		if err != nil {
			c.fail(err)
			return
		}
		var f inboundFrame
		if err := json.Unmarshal(raw, &f); err != nil {
			continue // malformed frame from a misbehaving peer; drop it
		}
		if len(f.ID) > 0 && string(f.ID) != "null" {
			if chVal, ok := c.pending.LoadAndDelete(string(f.ID)); ok {
				chVal.(chan inboundFrame) <- f
			}
			continue
		}
		if f.Method != "" {
			if onNotif := c.onNotification; onNotif != nil {
				onNotif(f.Method, f.Params)
			}
		}
	}
}

func (c *Conn) fail(err error) {
	c.closeOnce.Do(func() {
		c.closeErr = err
		close(c.closed)
		_ = c.nc.Close()
	})
}

// Call sends one JSON-RPC 2.0 request and blocks for its matching response.
func (c *Conn) Call(ctx context.Context, method string, params json.RawMessage) (json.RawMessage, error) {
	id := atomic.AddInt64(&c.nextID, 1)
	idJSON, _ := json.Marshal(id)
	req := outboundRequest{JSONRPC: "2.0", ID: id, Method: method, Params: params}
	body, err := json.Marshal(req)
	if err != nil {
		return nil, err
	}

	respCh := make(chan inboundFrame, 1)
	c.pending.Store(string(idJSON), respCh)
	defer c.pending.Delete(string(idJSON))

	c.writeMu.Lock()
	err = writeFramed(c.nc, body)
	c.writeMu.Unlock()
	if err != nil {
		return nil, fmt.Errorf("sapproxy: write %s: %w", method, err)
	}

	select {
	case f := <-respCh:
		if f.Error != nil {
			return nil, f.Error
		}
		return f.Result, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-c.closed:
		return nil, fmt.Errorf("sapproxy: connection closed: %w", c.closeErr)
	}
}

// IsClosed reports whether the connection's reader loop has already
// terminated (peer closed / write error) -- used by Router to decide
// whether a pooled connection needs to be redialed.
func (c *Conn) IsClosed() bool {
	select {
	case <-c.closed:
		return true
	default:
		return false
	}
}

// Close terminates the connection.
func (c *Conn) Close() error {
	c.fail(fmt.Errorf("sapproxy: connection closed by caller"))
	return nil
}
