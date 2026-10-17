package main

import (
	"fmt"
	"log"
	"net/http"

	"example.com/hits"
)

func main() {
	counter := hits.NewCounter()
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		counter.Inc(r.URL.Path)
		fmt.Fprintf(w, "%d hits\n", counter.Total())
	})
	log.Println("listening on :8080")
	log.Fatal(http.ListenAndServe(":8080", nil))
}
