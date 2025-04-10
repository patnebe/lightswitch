package main

import "C"
import "fmt"

//go:noinline
func top1() {
	for i := 0; i < 10000; i++ {
		fmt.Println("top1")
	}
}

//go:noinline
func c1() {
	top1()
}

//go:noinline
func b1() {
	c1()
}

//go:noinline
func a1() {
	b1()
}

//go:noinline
func top2() {
	for i := 0; i < 10000; i++ {
		fmt.Println("top2")
	}
}

//go:noinline
func c2() {
	top2()
}

//go:noinline
func b2() {
	c2()
}

//go:noinline
func a2() {
	b2()
}

func main() {
	for {
		a1()
		a2()
	}
}
