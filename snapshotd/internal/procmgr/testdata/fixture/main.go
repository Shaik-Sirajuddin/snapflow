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
	"fmt"
	"net"
	"os"
)

func main() {
	sock := os.Getenv("SNAPSHOT_SAP_SOCKET")
	token := os.Getenv("SNAPSHOT_SAP_TOKEN")
	headless := os.Getenv("SNAPSHOT_HEADLESS")
	outPath := os.Getenv("SNAPSHOT_FIXTURE_OUT")

	if outPath != "" {
		f, err := os.Create(outPath)
		if err == nil {
			fmt.Fprintf(f, "socket=%s\ntoken=%s\nheadless=%s\n", sock, token, headless)
			f.Close()
		}
	}

	if sock == "" {
		os.Exit(1)
	}

	ln, err := net.Listen("unix", sock)
	if err != nil {
		os.Exit(2)
	}
	defer ln.Close()

	for {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		conn.Close()
	}
}
