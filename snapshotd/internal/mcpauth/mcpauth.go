// Package mcpauth persists the MCP HTTP listener's bind address and
// optional Basic Auth credentials to a small JSON file under the daemon's
// home directory.
//
// The password is stored recoverable (not one-way hashed): this is a
// shared local secret used for HTTP Basic Auth, the same trust model as a
// Jupyter notebook token or a .netrc entry, not a multi-user login system.
// `snapshotd mcp install-config get` must be able to hand the real
// password back to whoever is configuring an MCP client, which a hash
// cannot support.
package mcpauth

import (
	"encoding/json"
	"os"
	"path/filepath"
)

// FileName is the config file's name under a daemon home directory.
const FileName = "mcp_config.json"

// Config is the persisted MCP listener configuration.
type Config struct {
	// BindAddr is the last address `snapshotd mcp restart` bound to (or the
	// initial default, once first saved). Empty means "no persisted
	// preference yet" -- callers should fall back to their own default.
	BindAddr string `json:"bindAddr"`

	// AuthEnabled gates both the Basic Auth middleware and the
	// non-loopback-bind refusal: a bind to a non-loopback address is only
	// permitted once this is true.
	AuthEnabled bool `json:"authEnabled"`

	AuthUser     string `json:"authUser,omitempty"`
	AuthPassword string `json:"authPassword,omitempty"`
}

func path(homeDir string) string {
	return filepath.Join(homeDir, FileName)
}

// Load reads the persisted config from homeDir. A missing file is not an
// error: it returns a zero-value Config so first-run callers can apply
// their own defaults.
func Load(homeDir string) (Config, error) {
	data, err := os.ReadFile(path(homeDir))
	if err != nil {
		if os.IsNotExist(err) {
			return Config{}, nil
		}
		return Config{}, err
	}
	var cfg Config
	if err := json.Unmarshal(data, &cfg); err != nil {
		return Config{}, err
	}
	return cfg, nil
}

// Save writes cfg to homeDir as 0600 (owner-only) JSON, since AuthPassword
// is a recoverable secret.
func Save(homeDir string, cfg Config) error {
	data, err := json.MarshalIndent(cfg, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path(homeDir), data, 0o600)
}
