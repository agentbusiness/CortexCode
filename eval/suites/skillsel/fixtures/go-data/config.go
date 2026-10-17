package ingest

import (
	"errors"
	"fmt"
	"os"
	"strings"
)

// Config is the ingest job's settings.
type Config struct {
	Source string
	Batch  int
}

// Load reads `key=value` lines from path.
func Load(path string) (Config, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return Config{}, fmt.Errorf("config not found: %v", err)
	}
	var cfg Config
	for _, line := range strings.Split(string(data), "\n") {
		key, value, ok := strings.Cut(strings.TrimSpace(line), "=")
		if !ok {
			continue
		}
		switch key {
		case "source":
			cfg.Source = value
		case "batch":
			if _, err := fmt.Sscan(value, &cfg.Batch); err != nil {
				return Config{}, errors.New("bad config: batch is not a number")
			}
		}
	}
	if cfg.Source == "" {
		return Config{}, errors.New("bad config: missing source")
	}
	return cfg, nil
}
