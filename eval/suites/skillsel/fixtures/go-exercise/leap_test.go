package leap

import "testing"

func TestLeapYears(t *testing.T) {
	for _, tc := range []struct {
		year int
		want bool
	}{
		{2015, false},
		{1996, true},
		{2100, false},
		{2000, true},
	} {
		if got := IsLeapYear(tc.year); got != tc.want {
			t.Errorf("IsLeapYear(%d) = %v, want %v", tc.year, got, tc.want)
		}
	}
}
