package main

// First-of-many: one consumer takes 2M events from whichever of 8
// unbuffered channels is ready (8 producers, 250k events each) with a
// select over the 8 channels; then the same events through a merge channel
// fed by 8 forwarder goroutines. Mirrors main.zl.

import (
	"fmt"
	"os"
	"sync"
	"time"
)

func producer(ch chan int, id, n int) {
	for i := 0; i < n; i++ {
		ch <- id*1000000 + i
	}
	close(ch)
}

func selectLoop(chs []chan int, total int) int {
	// A closed channel is set to nil (never ready in a select), as main.zl
	// drops it, instead of being polled again.
	c := append([]chan int(nil), chs...)
	sum, got := 0, 0
	for got < total {
		var v, i int
		var ok bool
		select {
		case v, ok = <-c[0]:
			i = 0
		case v, ok = <-c[1]:
			i = 1
		case v, ok = <-c[2]:
			i = 2
		case v, ok = <-c[3]:
			i = 3
		case v, ok = <-c[4]:
			i = 4
		case v, ok = <-c[5]:
			i = 5
		case v, ok = <-c[6]:
			i = 6
		case v, ok = <-c[7]:
			i = 7
		}
		if ok {
			sum += v
			got++
		} else {
			c[i] = nil
		}
	}
	return sum
}

func makeChans(k int) []chan int {
	chs := make([]chan int, k)
	for i := range chs {
		chs[i] = make(chan int)
	}
	return chs
}

func main() {
	small := os.Getenv("SCALE") == "small" // smoke workload
	k, each := 8, 250000
	if small {
		each = 2500
	}
	chs := makeChans(k)
	t0 := time.Now()
	for i := 0; i < k; i++ {
		go producer(chs[i], i, each)
	}
	a := selectLoop(chs, k*each)
	t1 := time.Now()
	chs2 := makeChans(k)
	merged := make(chan int)
	var wg sync.WaitGroup
	wg.Add(k)
	for i := 0; i < k; i++ {
		go producer(chs2[i], i, each)
		go func(src chan int) {
			for v := range src {
				merged <- v
			}
			wg.Done()
		}(chs2[i])
	}
	go func() { wg.Wait(); close(merged) }()
	b := 0
	for v := range merged {
		b += v
	}
	t2 := time.Now()
	fmt.Println("select_many", a, b)
	fmt.Println("  select_ms", t1.Sub(t0).Milliseconds(), "merge_ms", t2.Sub(t1).Milliseconds())
}
