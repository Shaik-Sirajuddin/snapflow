package sapproxy

import (
	"bufio"
	"fmt"
	"io"
	"strconv"
	"strings"
)

// writeFramed writes one Content-Length-framed message. Mirrors
// sap-rust/src/framing.rs's write_message byte-for-byte (an LSP-style
// "Content-Length: N\r\n\r\n" header followed by exactly N body bytes).
func writeFramed(w io.Writer, body []byte) error {
	header := fmt.Sprintf("Content-Length: %d\r\n\r\n", len(body))
	if _, err := io.WriteString(w, header); err != nil {
		return err
	}
	_, err := w.Write(body)
	return err
}

// readFramed reads one Content-Length-framed message. Mirrors
// sap-rust/src/framing.rs's read_message: headers terminated by a blank
// line, unknown headers tolerated/ignored, exactly Content-Length body
// bytes read afterward.
func readFramed(r *bufio.Reader) ([]byte, error) {
	contentLength := -1
	for {
		line, err := r.ReadString('\n')
		if err != nil {
			return nil, err
		}
		trimmed := strings.TrimRight(line, "\r\n")
		if trimmed == "" {
			break
		}
		if rest, ok := strings.CutPrefix(trimmed, "Content-Length:"); ok {
			n, err := strconv.Atoi(strings.TrimSpace(rest))
			if err != nil {
				return nil, fmt.Errorf("sapproxy: malformed Content-Length header %q", trimmed)
			}
			contentLength = n
		}
	}
	if contentLength < 0 {
		return nil, fmt.Errorf("sapproxy: missing Content-Length header")
	}
	buf := make([]byte, contentLength)
	if _, err := io.ReadFull(r, buf); err != nil {
		return nil, err
	}
	return buf, nil
}
