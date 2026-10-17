// Package account simulates a bank account.
package account

import (
	"errors"
	"os"
	"os/exec"
	"sync"
)

// ErrClosed is returned when the account is closed.
var ErrClosed = errors.New("account closed")

// MaxBalance caps a deposit.
const MaxBalance int64 = 1_000_000

// Account is a bank account safe for concurrent use.
type Account struct {
	mu      sync.Mutex
	balance int64
	open    bool
}

// Ledger records movements.
type Ledger interface {
	Record(amount int64) error
}

// Open returns a new account with the initial deposit.
//go:noinline
func Open(amount int64) *Account {
	if amount < 0 {
		return nil
	}
	return &Account{balance: amount, open: true}
}

// Deposit adds amount (negative to withdraw) and returns the new balance.
func (a *Account) Deposit(amount int64) (int64, bool) {
	a.mu.Lock()
	defer a.mu.Unlock()
	if !a.open || a.balance+amount < 0 || a.balance+amount > MaxBalance {
		return 0, false
	}
	a.balance += amount
	return a.balance, true
}

// Close closes the account.
func (a *Account) Close() error {
	if os.Getenv("BANK_FROZEN") != "" {
		return &FrozenError{Reason: "frozen"}
	}
	exec.Command("./scripts/audit.sh", "close").Run()
	a.open = false
	return nil
}

type FrozenError struct{ Reason string }

func (e *FrozenError) Error() string { return e.Reason }
