// Package sdp implements the Snapshot Daemon Protocol control surface: a
// hand-rolled JSON-RPC 2.0 request/response layer over the daemon's Unix
// control socket, per 06-daemon-mcp-proxy.md's "SDP server" (JSON-RPC 2.0,
// transport-agnostic) and 08-lifecycle-and-cli.md's Docker-daemon-socket
// analogy (Unix-only by default, no remote/TLS support -- deliberately not
// built here, matching the docs' emphasis that TCP+TLS should be an explicit
// opt-in, not default).
//
// Wire framing: newline-delimited JSON. Each request and response is exactly
// one JSON value terminated by "\n"; no Content-Length headers. This is the
// simplest framing that is trivially debuggable with `nc`/`socat` by hand,
// which is exactly the property 06's "human-debuggable, tool-agnostic wire
// format" reasoning calls for. This is a deliberate, minimal implementation
// of JSON-RPC 2.0, not a full spec-complete library: batch requests are not
// supported (not needed by any of the daemon.* methods below).
package sdp

import "encoding/json"

// Request is a single JSON-RPC 2.0 request object.
type Request struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id,omitempty"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

// Response is a single JSON-RPC 2.0 response object. Exactly one of Result /
// Error is set, per spec.
type Response struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id,omitempty"`
	Result  any             `json:"result,omitempty"`
	Error   *Error          `json:"error,omitempty"`
}

// Error mirrors the JSON-RPC 2.0 error object.
type Error struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
	Data    any    `json:"data,omitempty"`
}

// Notification is a fire-and-forget JSON-RPC 2.0 notification frame (no
// id), used to relay sap-rust's fanned-out notifications (edit.changed,
// project.dirty, etc -- opaque to this package, see internal/sapproxy) back
// out over the same newline-delimited wire this connection already uses for
// ordinary request/response traffic, interleaved as they arrive.
type Notification struct {
	JSONRPC string          `json:"jsonrpc"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

// Standard JSON-RPC 2.0 error codes used by this package.
const (
	CodeParseError     = -32700
	CodeInvalidRequest = -32600
	CodeMethodNotFound = -32601
	CodeInvalidParams  = -32602
	CodeInternalError  = -32603
)

func errorResponse(id json.RawMessage, code int, msg string) Response {
	return Response{
		JSONRPC: "2.0",
		ID:      id,
		Error:   &Error{Code: code, Message: msg},
	}
}

func resultResponse(id json.RawMessage, result any) Response {
	return Response{
		JSONRPC: "2.0",
		ID:      id,
		Result:  result,
	}
}
