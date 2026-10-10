package config

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
)

// ErrNotFound is returned by Load when no config file exists (first run).
var ErrNotFound = errors.New("config not found")

// maxRecentISOs caps the image paths the config remembers; the oldest go first.
const maxRecentISOs = 20

// AppConfig holds application-level settings persisted to disk.
type AppConfig struct {
	VMStoragePath string `json:"vm_storage_path"`
	// RecentISOs lists the image paths used before (boot ISOs and USB images),
	// newest first. The ISO picker offers them.
	RecentISOs []string `json:"recent_isos,omitempty"`
}

func configPath() string {
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".config", "ostrich", "config.json")
}

// Load reads and parses the app config. Returns ErrNotFound on first run.
func Load() (*AppConfig, error) {
	data, err := os.ReadFile(configPath())
	if err != nil {
		if os.IsNotExist(err) {
			return nil, ErrNotFound
		}
		return nil, err
	}
	var cfg AppConfig
	if err := json.Unmarshal(data, &cfg); err != nil {
		return nil, err
	}
	return &cfg, nil
}

// Save writes the config to disk, creating parent directories as needed.
func Save(cfg *AppConfig) error {
	path := configPath()
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(cfg, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0644)
}

// RecentISOs returns the image paths used before, newest first — none when
// there is no config to read.
func RecentISOs() []string {
	cfg, err := Load()
	if err != nil {
		return nil
	}
	return cfg.RecentISOs
}

// RememberISO puts path at the top of the remembered image paths and saves
// the config. A path already there moves up; the oldest beyond maxRecentISOs
// are dropped.
func RememberISO(path string) error {
	cfg, err := Load()
	if err != nil {
		return err
	}
	cfg.RecentISOs = append([]string{path}, without(cfg.RecentISOs, path)...)
	if len(cfg.RecentISOs) > maxRecentISOs {
		cfg.RecentISOs = cfg.RecentISOs[:maxRecentISOs]
	}
	return Save(cfg)
}

// ForgetISO drops path from the remembered image paths and saves the config.
func ForgetISO(path string) error {
	cfg, err := Load()
	if err != nil {
		return err
	}
	cfg.RecentISOs = without(cfg.RecentISOs, path)
	return Save(cfg)
}

// without returns paths with every occurrence of path left out.
func without(paths []string, path string) []string {
	var out []string
	for _, p := range paths {
		if p != path {
			out = append(out, p)
		}
	}
	return out
}
