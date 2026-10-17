package account

import "testing"

func TestDeposit(t *testing.T) {
	a := &Account{open: true}
	t.Run("negative balance", func(t *testing.T) {
		if _, ok := a.Deposit(-20); ok {
			t.Fatal("overdrawn")
		}
	})
	_ = Open(10)
}

func BenchmarkDeposit(b *testing.B) {}

func ExampleOpen() {}

// Negative controls: not `go test` functions.
func helperNotATest(t *testing.T) {}
func Testlowercase(t *testing.T) {}
func TestWrongParam(n int) {}
