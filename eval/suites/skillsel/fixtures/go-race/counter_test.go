package hits

import (
	"sync"
	"testing"
)

func TestCounter(t *testing.T) {
	c := NewCounter()
	var wg sync.WaitGroup
	for i := 0; i < 50; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for j := 0; j < 200; j++ {
				c.Inc("/")
			}
		}()
	}
	wg.Wait()
	if got := c.Total(); got != 10000 {
		t.Fatalf("Total() = %d, want 10000", got)
	}
}
