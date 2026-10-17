package main

import (
	"example.com/bank/account"
	"strings"
)

func main() {
	a := account.Open(1)
	_ = strings.Split("a,b", ",")
	_ = a
}
