package sdp

import (
	"bufio"
	"encoding/json"
	"fmt"
	"net"
	"time"
)

// Client is a minimal synchronous SDP client used by the CLI subcommands
// (status/stop/launch) -- deliberately not exported as a general-purpose
// library beyond what those subcommands need.
type Client struct {
	conn   net.Conn
	reader *bufio.Scanner
	nextID int
}

// Dial connects to a running daemon's control socket. Matches
// 09-project-folder-layout.md's CLI design note: "if no daemon is running,
// these commands simply fail to connect, by design."
func Dial(socketPath string, timeout time.Duration) (*Client, error) {
	conn, err := net.DialTimeout("unix", socketPath, timeout)
	if err != nil {
		return nil, fmt.Errorf("sdp: could not connect to daemon control socket %s: %w", socketPath, err)
	}
	scanner := bufio.NewScanner(conn)
	scanner.Buffer(make([]byte, 0, 64*1024), 8*1024*1024)
	return &Client{conn: conn, reader: scanner}, nil
}

func (c *Client) Close() error { return c.conn.Close() }

// Call sends a JSON-RPC 2.0 request and blocks for the matching response.
func (c *Client) Call(method string, params any, out any) error {
	c.nextID++
	id, _ := json.Marshal(c.nextID)
	paramsRaw, err := json.Marshal(params)
	if err != nil {
		return err
	}
	req := Request{JSONRPC: "2.0", ID: id, Method: method, Params: paramsRaw}
	line, err := json.Marshal(req)
	if err != nil {
		return err
	}
	if _, err := c.conn.Write(append(line, '\n')); err != nil {
		return err
	}
	if !c.reader.Scan() {
		if err := c.reader.Err(); err != nil {
			return err
		}
		return fmt.Errorf("sdp: connection closed without a response")
	}
	var resp Response
	if err := json.Unmarshal(c.reader.Bytes(), &resp); err != nil {
		return fmt.Errorf("sdp: decoding response: %w", err)
	}
	if resp.Error != nil {
		return fmt.Errorf("sdp: %s (code %d)", resp.Error.Message, resp.Error.Code)
	}
	if out == nil {
		return nil
	}
	raw, err := json.Marshal(resp.Result)
	if err != nil {
		return err
	}
	return json.Unmarshal(raw, out)
}
