// HTTP/2 interop with Go's net/http client (h2c, prior knowledge) against
// crates/ferroly/examples/h2spec_server.rs. Standard library only.
//
// usage: go-http <host:port>
//
// Each check prints PASS or FAIL; the exit status is non-zero on any
// failure. A healthy server keeps one connection for all of a check's
// requests: Go retries a GET whose connection was closed by GOAWAY without
// reporting it, so new connections are counted as well as failed requests.
package main

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"net/http"
	"net/http/httptrace"
	"os"
	"strconv"
	"sync"
	"sync/atomic"
	"time"
)

type result struct {
	ok, failed, conns atomic.Int64
	firstErr          atomic.Value
}

func (r *result) fail(err error) {
	r.failed.Add(1)
	r.firstErr.CompareAndSwap(nil, err.Error())
}

// newClient returns a client on a fresh transport (so each check starts
// without a pooled connection).
func newClient() *http.Client {
	p := new(http.Protocols)
	p.SetUnencryptedHTTP2(true)
	return &http.Client{Transport: &http.Transport{Protocols: p}, Timeout: 60 * time.Second}
}

// do sends one request and checks the answer: its length is wantLen, or
// it is exactly want when that is set.
func do(c *http.Client, r *result, method, url string, body []byte, wantLen int, want string) {
	trace := &httptrace.ClientTrace{GotConn: func(info httptrace.GotConnInfo) {
		if !info.Reused {
			r.conns.Add(1)
		}
	}}
	var rd io.Reader
	if body != nil {
		rd = bytes.NewReader(body)
	}
	req, err := http.NewRequestWithContext(httptrace.WithClientTrace(context.Background(), trace), method, url, rd)
	if err != nil {
		r.fail(err)
		return
	}
	resp, err := c.Do(req)
	if err != nil {
		r.fail(err)
		return
	}
	var got bytes.Buffer
	n, err := io.Copy(&got, resp.Body)
	resp.Body.Close()
	if err != nil {
		r.fail(err)
		return
	}
	bad := (want == "" && int(n) != wantLen) || (want != "" && got.String() != want)
	if resp.StatusCode != 200 || bad {
		r.fail(fmt.Errorf("status %d, %d body bytes", resp.StatusCode, n))
		return
	}
	r.ok.Add(1)
}

// run issues `total` requests from `workers` goroutines.
func run(total, workers int, req func(c *http.Client, r *result)) *result {
	r := &result{}
	c := newClient()
	var wg sync.WaitGroup
	per := total / workers
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := 0; i < per; i++ {
				req(c, r)
			}
		}()
	}
	wg.Wait()
	return r
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: go-http <host:port>")
		os.Exit(2)
	}
	base := "http://" + os.Args[1]
	status := 0
	check := func(name string, maxConns int64, r *result, took time.Duration) {
		verdict := "PASS"
		if r.failed.Load() > 0 || r.conns.Load() > maxConns {
			verdict = "FAIL"
			status = 1
		}
		first, _ := r.firstErr.Load().(string)
		fmt.Printf("%s %s: ok=%d failed=%d connections=%d in %v %s\n",
			verdict, name, r.ok.Load(), r.failed.Load(), r.conns.Load(), took.Round(time.Millisecond), first)
	}

	// Go returns stream window for response bytes as it reads them, after
	// the response has ended: those late WINDOW_UPDATEs must not count as
	// a flood (they used to close the connection with ENHANCE_YOUR_CALM).
	big := base + "/bytes/65536"
	start := time.Now()
	r := run(5000, 1, func(c *http.Client, r *result) { do(c, r, "GET", big, nil, 65536, "") })
	check("5000 sequential GETs of 64 KiB", 1, r, time.Since(start))

	start = time.Now()
	hello := []byte("hello")
	r = run(10000, 20, func(c *http.Client, r *result) { do(c, r, "POST", big, hello, 65536, "") })
	check("10000 POSTs of 5 bytes, 64 KiB answers, 20 at a time", 1, r, time.Since(start))

	// Concurrent buffered uploads beyond the body budget plus the window
	// (16 MiB + 4 MiB with the defaults) on one connection must all finish
	// (they used to stall until body_timeout).
	for _, up := range []struct{ n, size int }{{3, 8 << 20}, {6, 4 << 20}} {
		body := bytes.Repeat([]byte{'u'}, up.size)
		want := fmt.Sprintf("POST /upload (%d body bytes)\n", up.size)
		start = time.Now()
		r = run(up.n, up.n, func(c *http.Client, r *result) { do(c, r, "POST", base+"/upload", body, 0, want) })
		check(strconv.Itoa(up.n)+" concurrent uploads of "+strconv.Itoa(up.size>>20)+" MiB", 1, r, time.Since(start))
	}
	os.Exit(status)
}
