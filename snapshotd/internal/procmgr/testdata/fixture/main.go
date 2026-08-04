// Command fixture is a trivial stand-in for the real sap-rust child binary,
// used only by internal/procmgr's tests (built on the fly into a temp dir).
// It listens on the Unix socket named by SNAPSHOT_SAP_SOCKET -- exactly what
// Manager.Launch polls for -- and records the env vars it received to the
// file named by SNAPSHOT_FIXTURE_OUT so the test can assert Launch actually
// wired SNAPSHOT_SAP_SOCKET / SNAPSHOT_SAP_TOKEN / SNAPSHOT_HEADLESS through
// correctly. It then just accepts (and immediately closes) connections until
// killed, which is all Manager's health check needs.
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

	"snapshotd/internal/transport"
)

func main() {
	sock := os.Getenv("SNAPSHOT_SAP_SOCKET")
	endpoint := os.Getenv("SNAPSHOT_SAP_ENDPOINT")
	token := os.Getenv("SNAPSHOT_SAP_TOKEN")
	headless := os.Getenv("SNAPSHOT_HEADLESS")
	outPath := os.Getenv("SNAPSHOT_FIXTURE_OUT")

	if outPath != "" {
		f, err := os.Create(outPath)
		if err == nil {
			fmt.Fprintf(f, "socket=%s\nendpoint=%s\ntoken=%s\nheadless=%s\n", sock, endpoint, token, headless)
			f.Close()
		}
	}

	if sock == "" {
		os.Exit(1)
	}

	ln, err := transport.Listen(sock)
	if err != nil {
		os.Exit(2)
	}
	defer ln.Close()

	for {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		go serveFixtureConn(conn)
	}
}

// serveFixtureConn implements the smallest useful subset of SAP's
// Content-Length framed JSON-RPC protocol.  The old fixture accepted and
// immediately closed every connection, which was sufficient for the launch
// health probe but made a subsequent daemon.close fail during sap.hello.
func serveFixtureConn(conn net.Conn) {
	defer conn.Close()
	r := bufio.NewReader(conn)
	for {
		header, err := r.ReadString('\n')
		if err != nil {
			return
		}
		if !strings.HasPrefix(strings.TrimSpace(header), "Content-Length:") {
			continue
		}
		n, err := strconv.Atoi(strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(header), "Content-Length:")))
		if err != nil || n < 0 {
			return
		}
		// Consume the remaining headers and blank separator.
		for {
			line, err := r.ReadString('\n')
			if err != nil {
				return
			}
			if strings.TrimSpace(line) == "" {
				break
			}
		}
		body := make([]byte, n)
		if _, err := io.ReadFull(r, body); err != nil {
			return
		}
		var req struct {
			ID     json.RawMessage `json:"id"`
			Method string          `json:"method"`
		}
		if json.Unmarshal(body, &req) != nil || len(req.ID) == 0 {
			continue
		}
		resp := struct {
			JSONRPC string          `json:"jsonrpc"`
			ID      json.RawMessage `json:"id"`
			Result  map[string]any  `json:"result"`
		}{"2.0", req.ID, map[string]any{}}
		encoded, err := json.Marshal(resp)
		if err != nil {
			return
		}
		if _, err := fmt.Fprintf(conn, "Content-Length: %d\r\n\r\n%s", len(encoded), encoded); err != nil {
			return
		}
	}
}
