package config

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
)

// ErrNotFound is returned by Load when no config file exists (first run).
var ErrNotFound = errors.New("config not found")

// AppConfig holds application-level settings persisted to disk.
type AppConfig struct {
	VMStoragePath string `json:"vm_storage_path"`
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
