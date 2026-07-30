// Package discovery implements the bounded, same-user fallback for GUI
// instances that have not yet registered with snapshotd. It never scans the
// filesystem for .mlt files and never treats a command line as proof that a
// project is open.
package discovery

import (
	"bufio"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"snapshotd/internal/health"
)

const protocolVersion = 1

type Descriptor struct {
	Endpoint        string `json:"endpoint"`
	PID             int    `json:"pid"`
	ProcessStart    string `json:"processStart"`
	InstanceNonce   string `json:"instanceNonce"`
	ProtocolVersion int    `json:"protocolVersion"`
}

type Candidate struct {
	Descriptor
	ProjectPath   string `json:"projectPath,omitempty"`
	SAPSocketPath string `json:"sapSocketPath,omitempty"`
	SAPToken      string `json:"sapToken,omitempty"`
	Verified      bool   `json:"verified"`
}

type pingRequest struct {
	JSONRPC string            `json:"jsonrpc"`
	ID      int               `json:"id"`
	Method  string            `json:"method"`
	Params  map[string]string `json:"params"`
}

type pingResponse struct {
	Result *struct {
		InstanceNonce string `json:"instanceNonce"`
		PID           int    `json:"pid"`
		ProcessStart  string `json:"processStart"`
		ProjectPath   string `json:"projectPath"`
		SAPSocketPath string `json:"sapSocketPath"`
		SAPToken      string `json:"sapToken"`
		Challenge     string `json:"challenge"`
	} `json:"result"`
}

func ScanAndPing(runtimeDir string) ([]Candidate, error) {
	entries, err := os.ReadDir(runtimeDir)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var candidates []Candidate
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".json") {
			continue
		}
		data, err := os.ReadFile(filepath.Join(runtimeDir, entry.Name()))
		if err != nil {
			continue
		}
		var descriptor Descriptor
		if json.Unmarshal(data, &descriptor) != nil || !validDescriptor(descriptor) {
			continue
		}
		if !health.ProcessIdentityMatches(descriptor.PID, descriptor.ProcessStart) {
			continue
		}
		challenge, err := randomChallenge()
		if err != nil {
			return nil, err
		}
		response, err := ping(descriptor, challenge)
		if err != nil || response.InstanceNonce != descriptor.InstanceNonce || response.PID != descriptor.PID || response.ProcessStart != descriptor.ProcessStart || response.Challenge != challenge {
			continue
		}
		candidates = append(candidates, Candidate{Descriptor: descriptor, ProjectPath: response.ProjectPath, SAPSocketPath: response.SAPSocketPath, SAPToken: response.SAPToken, Verified: true})
	}
	return candidates, nil
}

func validDescriptor(descriptor Descriptor) bool {
	return descriptor.Endpoint != "" && descriptor.PID > 0 && descriptor.ProcessStart != "" && descriptor.InstanceNonce != "" && descriptor.ProtocolVersion == protocolVersion
}

func randomChallenge() (string, error) {
	var bytes [16]byte
	if _, err := rand.Read(bytes[:]); err != nil {
		return "", err
	}
	return hex.EncodeToString(bytes[:]), nil
}

func ping(descriptor Descriptor, challenge string) (pingResponseResult, error) {
	conn, err := net.DialTimeout("unix", descriptor.Endpoint, time.Second)
	if err != nil {
		return pingResponseResult{}, err
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(time.Second))
	request := pingRequest{JSONRPC: "2.0", ID: 1, Method: "app.discover", Params: map[string]string{"challenge": challenge}}
	if err := json.NewEncoder(conn).Encode(request); err != nil {
		return pingResponseResult{}, err
	}
	var response pingResponse
	if err := json.NewDecoder(bufio.NewReader(conn)).Decode(&response); err != nil {
		return pingResponseResult{}, err
	}
	if response.Result == nil {
		return pingResponseResult{}, fmt.Errorf("discovery response has no result")
	}
	return pingResponseResult{InstanceNonce: response.Result.InstanceNonce, PID: response.Result.PID, ProcessStart: response.Result.ProcessStart, ProjectPath: response.Result.ProjectPath, SAPSocketPath: response.Result.SAPSocketPath, SAPToken: response.Result.SAPToken, Challenge: response.Result.Challenge}, nil
}

type pingResponseResult struct {
	InstanceNonce string
	PID           int
	ProcessStart  string
	ProjectPath   string
	SAPSocketPath string
	SAPToken      string
	Challenge     string
}

// RuntimeDir returns the daemon-owned, same-user discovery directory.
func RuntimeDir(home string) string {
	return filepath.Join(home, "apps")
}

// DescriptorName is stable and collision-resistant without exposing a
// project path in the filename.
func DescriptorName(pid int, nonce string) string {
	return strconv.Itoa(pid) + "-" + nonce + ".json"
}
