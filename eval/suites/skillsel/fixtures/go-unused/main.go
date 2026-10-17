package main

import (
	"fmt"
	"os"
	"strconv"
)

func main() {
	raw := "8080"
	if len(os.Args) > 1 {
		raw = os.Args[1]
	}
	port, err := strconv.Atoi(raw)
	fmt.Printf("checking port %d\n", port)
}
