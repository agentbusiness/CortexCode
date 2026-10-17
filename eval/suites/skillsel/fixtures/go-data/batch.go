package ingest

// Split cuts items into batches of at most n items, in order, reusing one buffer.
func Split(items []int, n int) [][]int {
	var out [][]int
	batch := make([]int, 0, n)
	for _, it := range items {
		batch = append(batch, it)
		if len(batch) == n {
			out = append(out, batch)
			batch = batch[:0]
		}
	}
	if len(batch) > 0 {
		out = append(out, batch)
	}
	return out
}
