package main

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"time"
)

type Config struct {
	Connect     string `json:"connect"`
	Device      string `json:"device"`
	Dir         string `json:"dir"`
	Port        int    `json:"port"`
	MaxBuffers  int    `json:"max_buffers"`
	Retry       int    `json:"retry"`
	RetryDelay  string `json:"retry_delay"`
	DialTimeout string `json:"dial_timeout"`
	LogFile     string `json:"log_file"`
	LogLevel    string `json:"log_level"`
	Resume      bool   `json:"resume"`
	Checksum    bool   `json:"checksum"`
}

func hasExplicitConfig(args []string) bool {
	for i := 0; i < len(args); i++ {
		if args[i] == "--config" {
			return true
		}
		if strings.HasPrefix(args[i], "--config=") {
			return true
		}
	}
	return false
}

func extractConfigPath(args []string) string {
	for i := 0; i < len(args); i++ {
		if args[i] == "--config" && i+1 < len(args) {
			return args[i+1]
		}
		if strings.HasPrefix(args[i], "--config=") {
			return strings.TrimPrefix(args[i], "--config=")
		}
	}
	return defaultConfigPath()
}

func defaultConfigPath() string {
	exe, err := os.Executable()
	if err != nil {
		return "nekotrans.json"
	}
	return filepath.Join(filepath.Dir(exe), "nekotrans.json")
}

func loadConfig(path string) (Config, error) {
	if path == "" {
		return Config{}, errors.New("empty config path")
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return Config{}, err
	}
	var cfg Config
	if err := json.Unmarshal(data, &cfg); err != nil {
		return Config{}, err
	}
	return cfg, nil
}

func parseDurationDefault(s string, def time.Duration) time.Duration {
	if strings.TrimSpace(s) == "" {
		return def
	}
	d, err := time.ParseDuration(s)
	if err != nil {
		return def
	}
	return d
}

func ensureWritableDir(dir string) error {
	if dir == "" || dir == "/" {
		return nil
	}
	if err := os.MkdirAll(dir, 0755); err != nil {
		return err
	}
	testFile := filepath.Join(dir, ".nekotrans_write_test")
	f, err := os.OpenFile(testFile, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0644)
	if err != nil {
		return err
	}
	_ = f.Close()
	return os.Remove(testFile)
}
