// Package acpnode resolves and optionally ensures a Node/npm toolchain for
// ACP/acpx (global-first, then product-bundled official Node).
package acpnode

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
)

// DefaultNodeVersion is the official nodejs.org LTS pin for bundled installs.
const DefaultNodeVersion = "v22.14.0"

// Source is where the resolved toolchain came from.
type Source string

const (
	SourceGlobal  Source = "global"
	SourceBundled Source = "bundled"
	SourceMissing Source = "missing"
)

// Resolved is a same-prefix node/npm/npx choice.
type Resolved struct {
	Source Source
	Home   string // directory containing bin/ (may be empty for some globals)
	Node   string
	Npm    string
	Npx    string
}

// InstallDir is SNAPFLOW_INSTALL_DIR or ~/.local/share/snapflow.
func InstallDir() string {
	if v := os.Getenv("SNAPFLOW_INSTALL_DIR"); v != "" {
		return v
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return ".local/share/snapflow"
	}
	return filepath.Join(home, ".local", "share", "snapflow")
}

// BundleHome is the product-local Node extract root.
func BundleHome() string {
	if v := os.Getenv("SNAPFLOW_ACP_NODE_HOME"); v != "" {
		return v
	}
	return filepath.Join(InstallDir(), "runtime", "node")
}

func prefixOK(home string) bool {
	return executable(filepath.Join(home, "bin", "node")) &&
		executable(filepath.Join(home, "bin", "npm")) &&
		executable(filepath.Join(home, "bin", "npx"))
}

func executable(path string) bool {
	st, err := os.Stat(path)
	if err != nil || st.IsDir() {
		return false
	}
	return st.Mode()&0o111 != 0
}

func systemOK() bool {
	for _, name := range []string{"node", "npm", "npx"} {
		if _, err := exec.LookPath(name); err != nil {
			return false
		}
	}
	if err := exec.Command("node", "--version").Run(); err != nil {
		return false
	}
	if err := exec.Command("npm", "--version").Run(); err != nil {
		return false
	}
	return true
}

// Resolve implements global-first then bundled.
func Resolve() Resolved {
	if systemOK() {
		node, _ := exec.LookPath("node")
		npm, _ := exec.LookPath("npm")
		npx, _ := exec.LookPath("npx")
		home := filepath.Clean(filepath.Join(filepath.Dir(node), ".."))
		return Resolved{
			Source: SourceGlobal,
			Home:   home,
			Node:   node,
			Npm:    npm,
			Npx:    npx,
		}
	}
	home := BundleHome()
	if prefixOK(home) {
		return Resolved{
			Source: SourceBundled,
			Home:   home,
			Node:   filepath.Join(home, "bin", "node"),
			Npm:    filepath.Join(home, "bin", "npm"),
			Npx:    filepath.Join(home, "bin", "npx"),
		}
	}
	return Resolved{Source: SourceMissing}
}

// EnvForAcpx returns extra env vars for an acpx-server child.
// Global: no SNAPFLOW_ACP_NODE_HOME force. Bundled: set home + PATH prefix.
func EnvForAcpx(r Resolved) []string {
	if r.Source != SourceBundled || r.Home == "" {
		return nil
	}
	path := os.Getenv("PATH")
	return []string{
		"SNAPFLOW_ACP_NODE_HOME=" + r.Home,
		"PATH=" + filepath.Join(r.Home, "bin") + string(os.PathListSeparator) + path,
	}
}

// Ensure downloads official Node into BundleHome when global is missing
// (or force is true). Uses curl + tar like the bash helper.
func Ensure(force bool) error {
	if !force && systemOK() {
		return nil
	}
	home := BundleHome()
	if !force && prefixOK(home) {
		return nil
	}
	ver := os.Getenv("SNAPFLOW_NODE_VERSION")
	if ver == "" {
		ver = DefaultNodeVersion
	}
	if !strings.HasPrefix(ver, "v") {
		ver = "v" + ver
	}
	plat, err := platformArch()
	if err != nil {
		return err
	}
	base := fmt.Sprintf("node-%s-%s", ver, plat)
	url := os.Getenv("SNAPFLOW_NODE_DIST_URL")
	if url == "" {
		url = fmt.Sprintf("https://nodejs.org/dist/%s/%s.tar.xz", ver, base)
	}
	tmp, err := os.MkdirTemp("", "acp-node-*")
	if err != nil {
		return err
	}
	defer os.RemoveAll(tmp)
	archive := filepath.Join(tmp, base+".tar.xz")
	if err := run("curl", "-fsSL", url, "-o", archive); err != nil {
		return fmt.Errorf("download %s: %w", url, err)
	}
	if err := run("tar", "-xJf", archive, "-C", tmp); err != nil {
		return fmt.Errorf("extract: %w", err)
	}
	src := filepath.Join(tmp, base)
	if _, err := os.Stat(src); err != nil {
		entries, _ := os.ReadDir(tmp)
		for _, e := range entries {
			if e.IsDir() {
				src = filepath.Join(tmp, e.Name())
				break
			}
		}
	}
	_ = os.RemoveAll(home)
	if err := os.MkdirAll(filepath.Dir(home), 0o755); err != nil {
		return err
	}
	if err := os.Rename(src, home); err != nil {
		// cross-device: copy via tar pipe
		if err2 := run("bash", "-c", fmt.Sprintf("cp -a %q %q", src, home)); err2 != nil {
			return fmt.Errorf("install to %s: %v / %v", home, err, err2)
		}
	}
	_ = os.WriteFile(filepath.Join(home, ".version"), []byte(ver+"\n"), 0o644)
	_ = os.WriteFile(filepath.Join(home, ".source-url"), []byte(url+"\n"), 0o644)
	if !prefixOK(home) {
		return fmt.Errorf("bundled node incomplete at %s", home)
	}
	return nil
}

func platformArch() (string, error) {
	var osPart string
	switch runtime.GOOS {
	case "linux":
		osPart = "linux"
	case "darwin":
		osPart = "darwin"
	default:
		return "", fmt.Errorf("unsupported OS %s", runtime.GOOS)
	}
	var arch string
	switch runtime.GOARCH {
	case "amd64":
		arch = "x64"
	case "arm64":
		arch = "arm64"
	default:
		return "", fmt.Errorf("unsupported arch %s", runtime.GOARCH)
	}
	return osPart + "-" + arch, nil
}

func run(name string, args ...string) error {
	cmd := exec.Command(name, args...)
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

// DoctorReport is a human-readable multi-line status.
func DoctorReport() string {
	r := Resolve()
	var b strings.Builder
	fmt.Fprintf(&b, "ACP Node doctor (global-first)\n")
	fmt.Fprintf(&b, "  source:  %s\n", r.Source)
	fmt.Fprintf(&b, "  home:    %s\n", emptyDash(r.Home))
	fmt.Fprintf(&b, "  node:    %s\n", emptyDash(r.Node))
	fmt.Fprintf(&b, "  npm:     %s\n", emptyDash(r.Npm))
	fmt.Fprintf(&b, "  npx:     %s\n", emptyDash(r.Npx))
	bundle := BundleHome()
	if prefixOK(bundle) {
		ver, _ := os.ReadFile(filepath.Join(bundle, ".version"))
		fmt.Fprintf(&b, "  bundle:  present (%s) version=%s\n", bundle, strings.TrimSpace(string(ver)))
	} else {
		fmt.Fprintf(&b, "  bundle:  absent (%s)\n", bundle)
	}
	return b.String()
}

func emptyDash(s string) string {
	if s == "" {
		return "(none)"
	}
	return s
}
