package ingest

import (
	"reflect"
	"testing"
)

func TestSplit(t *testing.T) {
	got := Split([]int{1, 2, 3, 4, 5}, 2)
	want := [][]int{{1, 2}, {3, 4}, {5}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("Split = %v, want %v", got, want)
	}
}
