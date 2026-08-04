package transport

import (
	"bufio"
	"fmt"
	"io"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
)

func TestDefaultControlEndpointIsPlatformNative(t *testing.T) {
	ep := DefaultControlEndpoint(`C:\Users\alice\.snapshotd`)
	if runtime.GOOS == "windows" {
		if !strings.HasPrefix(ep, `\\.\pipe\snapflow-`) {
			t.Fatalf("Windows endpoint = %q", ep)
		}
		return
	}
	if !strings.HasSuffix(ep, "control.sock") {
		t.Fatalf("Unix endpoint = %q", ep)
	}
}

func TestDefaultSAPEndpointIsPlatformNative(t *testing.T) {
	ep := DefaultSAPEndpoint(`/tmp/runtime`, "abc123")
	if runtime.GOOS == "windows" {
		if !strings.HasPrefix(ep, `\\.\pipe\snapflow-`) || !strings.HasSuffix(ep, "-sap-abc123") {
			t.Fatalf("Windows SAP endpoint = %q", ep)
		}
		return
	}
	want := filepath.Join(`/tmp/runtime`, "abc123.sock")
	if ep != want {
		t.Fatalf("Unix SAP endpoint = %q, want %q", ep, want)
	}
}

// TestNativeRoundTrip exercises the same listener/dial API used by SDP,
// discovery, SAP proxy, and health checks. On Windows this binds a real named
// pipe; on Unix it binds a real AF_UNIX socket. Keeping this test at the
// transport boundary catches accidental platform-specific branching in
// callers and proves the wire remains byte-stream oriented (not pipe message
// oriented).
func TestNativeRoundTrip(t *testing.T) {
	endpoint := fmt.Sprintf("%s/snapflow-transport-%d.sock", t.TempDir(), time.Now().UnixNano())
	if runtime.GOOS == "windows" {
		endpoint = fmt.Sprintf(`\\.\pipe\snapflow-transport-%d`, time.Now().UnixNano())
	}
	l, err := Listen(endpoint)
	if err != nil {
		t.Fatalf("Listen(%q): %v", endpoint, err)
	}
	t.Cleanup(func() { _ = l.Close(); _ = RemoveStale(endpoint) })

	accepted := make(chan error, 1)
	go func() {
		conn, err := l.Accept()
		if err != nil {
			accepted <- err
			return
		}
		defer conn.Close()
		line, err := bufio.NewReader(conn).ReadString('\n')
		if err == nil {
			_, err = io.WriteString(conn, strings.ToUpper(line))
		}
		accepted <- err
	}()

	ctxDone := time.Now().Add(5 * time.Second)
	conn, err := DialTimeout(endpoint, time.Until(ctxDone))
	if err != nil {
		t.Fatalf("DialTimeout(%q): %v", endpoint, err)
	}
	defer conn.Close()
	if _, err := io.WriteString(conn, "ping\n"); err != nil {
		t.Fatalf("write: %v", err)
	}
	conn.SetReadDeadline(ctxDone)
	got, err := bufio.NewReader(conn).ReadString('\n')
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	if got != "PING\n" {
		t.Fatalf("round-trip = %q, want %q", got, "PING\\n")
	}
	select {
	case err := <-accepted:
		if err != nil {
			t.Fatalf("server: %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("server did not complete")
	}
}
