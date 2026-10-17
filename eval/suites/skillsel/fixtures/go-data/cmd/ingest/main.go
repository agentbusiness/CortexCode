package main

import (
	"fmt"
	"os"
	"strings"

	"example.com/ingest"
)

func main() {
	cfg, err := ingest.Load("ingest.conf")
	if err != nil {
		if strings.HasPrefix(err.Error(), "config not found") {
			cfg = ingest.Config{Source: "stdin", Batch: 100}
		} else {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(2)
		}
	}
	fmt.Printf("reading %s in batches of %d\n", cfg.Source, cfg.Batch)
}
