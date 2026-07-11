package sapproxy

import (
	"encoding/json"
	"fmt"
)

// outboundRequest is what Conn.Call sends: a standard JSON-RPC 2.0 request
// object, matching sap-rust's RpcRequest (sap-rust/src/protocol.rs).
type outboundRequest struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      int64           `json:"id"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

// inboundFrame parses any message sap-rust sends us generically: it is
// either a response to one of our own requests (ID set, Result or Error
// set) or a fire-and-forget notification (Method set, no ID), matching
// sap-rust's OutboundMessage enum (sap-rust/src/protocol.rs) -- both
// variants serialize as plain JSON-RPC 2.0 objects on the wire, so a single
// permissive struct is enough to tell them apart after the fact.
type inboundFrame struct {
	ID     json.RawMessage `json:"id,omitempty"`
	Method string          `json:"method,omitempty"`
	Params json.RawMessage `json:"params,omitempty"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  *RPCError       `json:"error,omitempty"`
}

// RPCError mirrors sap-rust's RpcError (sap-rust/src/protocol.rs): a
// JSON-RPC 2.0 error object. Forwarded verbatim (code + message preserved)
// so callers of Router.Call can tell a real SAP application error (e.g.
// NOT_FOUND, NO_PROJECT_BOUND) from a transport-level failure.
type RPCError struct {
	Code    int64           `json:"code"`
	Message string          `json:"message"`
	Data    json.RawMessage `json:"data,omitempty"`
}

func (e *RPCError) Error() string {
	return fmt.Sprintf("sap-rust error %d: %s", e.Code, e.Message)
}
