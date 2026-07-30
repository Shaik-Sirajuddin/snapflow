// Command sap_fixture is a Content-Length JSON-RPC stand-in for sap-rust,
// used by project-open-init-close daemon tests. It speaks enough of the
// wire protocol for ForwardSAP's project.select / getState / listTracks
// path: sap.hello token gate, one-time "opened" semantics, and a simple
// track list so multi-session reuse can be asserted without a Qt build.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"strconv"
	"strings"
	"sync"
)

func main() {
	sock := os.Getenv("SNAPSHOT_SAP_SOCKET")
	token := os.Getenv("SNAPSHOT_SAP_TOKEN")
	if sock == "" || token == "" {
		os.Exit(1)
	}

	// Optional: record whether an on-disk mlt existed at launch so tests can
	// distinguish "bind-only" vs "would open" without real C++ open.
	root := os.Getenv("SNAPSHOT_PROJECT_ROOT")
	mltName := os.Getenv("SNAPSHOT_PROJECT_MLT_FILENAME")
	if mltName == "" {
		mltName = "project.mlt"
	}
	mltExists := false
	if root != "" {
		if st, err := os.Stat(root + "/" + mltName); err == nil && !st.IsDir() {
			mltExists = true
		}
	}

	ln, err := net.Listen("unix", sock)
	if err != nil {
		os.Exit(2)
	}
	defer ln.Close()

	state := &sharedState{token: token, mltExists: mltExists}

	for {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		go handleConn(conn, state)
	}
}

type sharedState struct {
	token     string
	mltExists bool

	mu        sync.Mutex
	opened    bool
	selectN   int
	tracks    []string
	undoDepth int
}

func handleConn(nc net.Conn, st *sharedState) {
	defer nc.Close()
	r := bufio.NewReader(nc)
	var writeMu sync.Mutex
	write := func(v any) {
		body, _ := json.Marshal(v)
		writeMu.Lock()
		_ = writeFramed(nc, body)
		writeMu.Unlock()
	}

	authenticated := false
	for {
		raw, err := readFramed(r)
		if err != nil {
			return
		}
		var req struct {
			ID     json.RawMessage `json:"id"`
			Method string          `json:"method"`
			Params json.RawMessage `json:"params"`
		}
		if err := json.Unmarshal(raw, &req); err != nil {
			continue
		}
		respond := func(result any, errMsg string) {
			if errMsg != "" {
				write(map[string]any{
					"jsonrpc": "2.0",
					"id":      json.RawMessage(req.ID),
					"error":   map[string]any{"code": -32000, "message": errMsg},
				})
				return
			}
			write(map[string]any{
				"jsonrpc": "2.0",
				"id":      json.RawMessage(req.ID),
				"result":  result,
			})
		}

		switch req.Method {
		case "sap.hello":
			var p struct {
				Token string `json:"token"`
			}
			_ = json.Unmarshal(req.Params, &p)
			if p.Token != st.token {
				respond(nil, "bad token")
				continue
			}
			authenticated = true
			respond(map[string]any{"ok": true}, "")
		case "project.select":
			if !authenticated {
				respond(nil, "unauthenticated")
				continue
			}
			var p struct {
				ProjectID string `json:"projectId"`
			}
			_ = json.Unmarshal(req.Params, &p)
			st.mu.Lock()
			st.selectN++
			// One-time open: first select "opens"; later selects are no-ops.
			if !st.opened {
				st.opened = true
				// Simulate content load for existing mlt: seed a track.
				if st.mltExists && len(st.tracks) == 0 {
					st.tracks = append(st.tracks, "video")
					st.undoDepth = 0
				}
			}
			opened := st.opened
			undo := st.undoDepth
			selectN := st.selectN
			st.mu.Unlock()
			respond(map[string]any{
				"projectId":   p.ProjectID,
				"dirty":       undo > 0,
				"undoDepth":   undo,
				"redoDepth":   0,
				"opened":      opened,
				"selectCount": selectN, // test-only observability
				"mltExisted":  st.mltExists,
			}, "")
		case "project.getState":
			if !authenticated {
				respond(nil, "unauthenticated")
				continue
			}
			var p struct {
				ProjectID string `json:"projectId"`
			}
			_ = json.Unmarshal(req.Params, &p)
			st.mu.Lock()
			respond(map[string]any{
				"projectId": p.ProjectID,
				"dirty":     st.undoDepth > 0,
				"undoDepth": st.undoDepth,
				"redoDepth": 0,
				"opened":    st.opened,
			}, "")
			st.mu.Unlock()
		case "edit.addTrack":
			if !authenticated {
				respond(nil, "unauthenticated")
				continue
			}
			var p struct {
				Kind string `json:"kind"`
			}
			_ = json.Unmarshal(req.Params, &p)
			if p.Kind == "" {
				p.Kind = "video"
			}
			st.mu.Lock()
			st.tracks = append(st.tracks, p.Kind)
			st.undoDepth++
			idx := len(st.tracks) - 1
			st.mu.Unlock()
			respond(map[string]any{"index": idx, "kind": p.Kind}, "")
		case "edit.listTracks":
			if !authenticated {
				respond(nil, "unauthenticated")
				continue
			}
			st.mu.Lock()
			out := make([]map[string]any, 0, len(st.tracks))
			for i, k := range st.tracks {
				out = append(out, map[string]any{"index": i, "kind": k})
			}
			st.mu.Unlock()
			respond(out, "")
		case "project.save":
			if !authenticated {
				respond(nil, "unauthenticated")
				continue
			}
			respond(map[string]any{"saved": true}, "")
		case "project.exit":
			// Real sap-rust no-op; daemon never forwards this for multi-session safety.
			respond(map[string]any{}, "")
		default:
			respond(nil, "method not found: "+req.Method)
		}
	}
}

func writeFramed(w io.Writer, body []byte) error {
	header := fmt.Sprintf("Content-Length: %d\r\n\r\n", len(body))
	if _, err := io.WriteString(w, header); err != nil {
		return err
	}
	_, err := w.Write(body)
	return err
}

func readFramed(r *bufio.Reader) ([]byte, error) {
	var contentLength int
	for {
		line, err := r.ReadString('\n')
		if err != nil {
			return nil, err
		}
		line = strings.TrimRight(line, "\r\n")
		if line == "" {
			break
		}
		if strings.HasPrefix(strings.ToLower(line), "content-length:") {
			n, err := strconv.Atoi(strings.TrimSpace(line[len("Content-Length:"):]))
			if err != nil {
				return nil, err
			}
			contentLength = n
		}
	}
	if contentLength <= 0 {
		return nil, io.ErrUnexpectedEOF
	}
	buf := make([]byte, contentLength)
	if _, err := io.ReadFull(r, buf); err != nil {
		return nil, err
	}
	return buf, nil
}
