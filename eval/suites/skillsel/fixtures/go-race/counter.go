package hits

// Counter counts page hits per path.
type Counter struct {
	total int
	byKey map[string]int
}

func NewCounter() *Counter {
	return &Counter{byKey: map[string]int{}}
}

// Inc records one hit for key.
func (c *Counter) Inc(key string) {
	c.total++
	c.byKey[key]++
}

// Total is the number of hits recorded.
func (c *Counter) Total() int {
	return c.total
}

// Of is the number of hits recorded for key.
func (c *Counter) Of(key string) int {
	return c.byKey[key]
}
