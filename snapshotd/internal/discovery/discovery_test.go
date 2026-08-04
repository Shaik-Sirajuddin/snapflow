package discovery

import (
	"bufio"
	"encoding/json"
	"net"
	"os"
	"path/filepath"
	"testing"
	"time"

	"snapshotd/internal/health"
)

func TestScanAndPingAcceptsOnlyChallengeVerifiedSameProcessDescriptor(t *testing.T) {
	temp := t.TempDir()
	socket := filepath.Join(temp, "app.sock")
	listener, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	processStart, err := health.ProcessStartIdentity(os.Getpid())
	if err != nil {
		t.Fatal(err)
	}
	nonce := "nonce-1"
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		var request pingRequest
		if json.NewDecoder(bufio.NewReader(conn)).Decode(&request) != nil {
			return
		}
		_ = json.NewEncoder(conn).Encode(map[string]any{"result": map[string]any{
			"instanceNonce": nonce,
			"pid":           os.Getpid(),
			"processStart":  processStart,
			"projectPath":   "/tmp/project.mlt",
			"challenge":     request.Params["challenge"],
		}})
	}()
	descriptor := Descriptor{Endpoint: socket, PID: os.Getpid(), ProcessStart: processStart, InstanceNonce: nonce, ProtocolVersion: protocolVersion}
	data, _ := json.Marshal(descriptor)
	if err := os.WriteFile(filepath.Join(temp, DescriptorName(os.Getpid(), nonce)), data, 0o600); err != nil {
		t.Fatal(err)
	}
	candidates, err := ScanAndPing(temp)
	if err != nil || len(candidates) != 1 || !candidates[0].Verified {
		t.Fatalf("expected one verified candidate: candidates=%+v err=%v", candidates, err)
	}
}

func TestScanAndPingRejectsWrongChallenge(t *testing.T) {
	temp := t.TempDir()
	socket := filepath.Join(temp, "wrong-challenge.sock")
	listener, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	processStart, err := health.ProcessStartIdentity(os.Getpid())
	if err != nil {
		t.Fatal(err)
	}
	nonce := "wrong-challenge"
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		var request pingRequest
		if json.NewDecoder(bufio.NewReader(conn)).Decode(&request) == nil {
			_ = json.NewEncoder(conn).Encode(map[string]any{"result": map[string]any{
				"instanceNonce": nonce, "pid": os.Getpid(), "processStart": processStart,
				"challenge": "not-the-requested-challenge",
			}})
		}
	}()
	descriptor := Descriptor{Endpoint: socket, PID: os.Getpid(), ProcessStart: processStart, InstanceNonce: nonce, ProtocolVersion: protocolVersion}
	data, _ := json.Marshal(descriptor)
	if err := os.WriteFile(filepath.Join(temp, DescriptorName(os.Getpid(), nonce)), data, 0o600); err != nil {
		t.Fatal(err)
	}
	candidates, err := ScanAndPing(temp)
	if err != nil || len(candidates) != 0 {
		t.Fatalf("wrong challenge must be rejected: candidates=%+v err=%v", candidates, err)
	}
}

func TestScanAndPingRejectsStaleAndWrongChallengeCandidates(t *testing.T) {
	temp := t.TempDir()
	stale := Descriptor{Endpoint: filepath.Join(temp, "missing.sock"), PID: 999999, ProcessStart: "old", InstanceNonce: "old", ProtocolVersion: protocolVersion}
	data, _ := json.Marshal(stale)
	if err := os.WriteFile(filepath.Join(temp, "stale.json"), data, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(temp, "malformed.json"), []byte("not-json"), 0o600); err != nil {
		t.Fatal(err)
	}
	candidates, err := ScanAndPing(temp)
	if err != nil || len(candidates) != 0 {
		t.Fatalf("expected no candidates: candidates=%+v err=%v", candidates, err)
	}
	if _, err := os.Stat(filepath.Join(temp, "stale.json")); !os.IsNotExist(err) {
		t.Fatalf("dead descriptor should be pruned, stat err=%v", err)
	}
	_ = time.Second // keep the test's deadline vocabulary explicit for future fake peers
}
