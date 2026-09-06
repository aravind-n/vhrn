package egress

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func TestLoopbackHTTPReusesConnectionAndRevokesLive(t *testing.T) {
	var requests, connections int
	var mu sync.Mutex
	origin := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		mu.Lock()
		requests++
		mu.Unlock()
		fmt.Fprint(w, "ok")
	}))
	origin.Config.ConnState = func(_ net.Conn, state http.ConnState) {
		if state == http.StateNew {
			mu.Lock()
			connections++
			mu.Unlock()
		}
	}
	origin.Start()
	defer origin.Close()
	authority := strings.TrimPrefix(origin.URL, "http://")
	paths := loopbackPolicyFiles(t, authority+"\n")
	broker, dials := testBroker(t)
	recorder := &fakeRecorder{}
	px := NewProxyWithLoopback(fakeChecker{verdict: Verdict{Allow: true}, mode: ModeOpen}, fakeDialer{}, recorder, LoopbackPolicy{Paths: paths}, broker)
	proxy := httptest.NewServer(px)
	defer proxy.Close()
	client := proxyClient(t, proxy.URL)
	for i := 0; i < 2; i++ {
		response, err := client.Get(origin.URL + "/request")
		if err != nil {
			t.Fatal(err)
		}
		body, err := io.ReadAll(response.Body)
		response.Body.Close()
		if err != nil || response.StatusCode != http.StatusOK || string(body) != "ok" {
			t.Fatalf("response = %d %q, %v", response.StatusCode, body, err)
		}
	}
	mu.Lock()
	gotRequests, gotConnections := requests, connections
	mu.Unlock()
	if gotRequests != 2 || gotConnections != 1 || dials.Load() != 1 {
		t.Fatalf("requests=%d connections=%d broker dials=%d; want 2, 1, 1", gotRequests, gotConnections, dials.Load())
	}
	if err := os.WriteFile(paths[1], []byte("\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	response, err := client.Get(origin.URL + "/request")
	if err != nil {
		t.Fatal(err)
	}
	response.Body.Close()
	if response.StatusCode != http.StatusForbidden || dials.Load() != 1 {
		t.Fatalf("blank policy response=%d broker dials=%d", response.StatusCode, dials.Load())
	}
	mu.Lock()
	if requests != 2 {
		t.Fatalf("origin received %d requests after blank policy", requests)
	}
	mu.Unlock()
	if err := os.WriteFile(paths[1], []byte(authority+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	writeLoopbackFiles(t, paths, "")
	response, err = client.Get(origin.URL + "/request")
	if err != nil {
		t.Fatal(err)
	}
	response.Body.Close()
	if response.StatusCode != http.StatusForbidden || dials.Load() != 1 {
		t.Fatalf("revoked response=%d broker dials=%d", response.StatusCode, dials.Load())
	}
	mu.Lock()
	if requests != 2 {
		t.Fatalf("origin received %d requests after revoke", requests)
	}
	mu.Unlock()
	if recorder.count() != 2 {
		t.Fatalf("local denial was not recorded: %d", recorder.count())
	}
	recorder.mu.Lock()
	denied := append([]string(nil), recorder.hosts...)
	recorder.mu.Unlock()
	if !strings.Contains(denied[len(denied)-1], authority) {
		t.Fatalf("local denial recorded %q, want canonical authority %q", denied[len(denied)-1], authority)
	}

	for _, mode := range []Mode{ModeOpen, ModeReport} {
		writeLoopbackFiles(t, paths, "")
		px := NewProxyWithLoopback(fakeChecker{verdict: Verdict{Allow: true}, mode: mode}, fakeDialer{}, &fakeRecorder{}, LoopbackPolicy{Paths: paths}, broker)
		srv := httptest.NewServer(px)
		response, err := proxyClient(t, srv.URL).Get(origin.URL)
		if err != nil {
			t.Fatal(err)
		}
		response.Body.Close()
		srv.Close()
		if response.StatusCode != http.StatusForbidden {
			t.Errorf("mode %s local response = %d, want 403", mode, response.StatusCode)
		}
	}
}

func TestLoopbackCONNECTSurvivesRevocation(t *testing.T) {
	echo := echoServer(t)
	paths := loopbackPolicyFiles(t, echo+"\n")
	broker, _ := testBroker(t)
	proxy := httptest.NewServer(NewProxyWithLoopback(fakeChecker{verdict: Verdict{Allow: true}}, fakeDialer{}, &fakeRecorder{}, LoopbackPolicy{Paths: paths}, broker))
	defer proxy.Close()
	conn := dialProxy(t, proxy.URL)
	defer conn.Close()
	request := "CONNECT " + echo + " HTTP/1.1\r\nHost: " + echo + "\r\n\r\nfirst"
	if _, err := conn.Write([]byte(request)); err != nil {
		t.Fatal(err)
	}
	reader := bufio.NewReader(conn)
	status, err := reader.ReadString('\n')
	if err != nil || !strings.Contains(status, "200") {
		t.Fatalf("CONNECT = %q, %v", status, err)
	}
	skipHeaders(reader)
	first := make([]byte, len("first"))
	if _, err := io.ReadFull(reader, first); err != nil || string(first) != "first" {
		t.Fatalf("first echo = %q, %v", first, err)
	}
	writeLoopbackFiles(t, paths, "")
	if _, err := conn.Write([]byte("second")); err != nil {
		t.Fatal(err)
	}
	second := make([]byte, len("second"))
	if _, err := io.ReadFull(reader, second); err != nil || string(second) != "second" {
		t.Fatalf("existing tunnel stopped after revoke: %q, %v", second, err)
	}
	newConn := dialProxy(t, proxy.URL)
	defer newConn.Close()
	fmt.Fprintf(newConn, "CONNECT %s HTTP/1.1\r\nHost: %s\r\n\r\n", echo, echo)
	denied, err := bufio.NewReader(newConn).ReadString('\n')
	if err != nil || !strings.Contains(denied, "403") {
		t.Fatalf("new CONNECT = %q, %v", denied, err)
	}
}

func TestLoopbackSSEFlushesAndCancelsOrigin(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	cancelled := make(chan struct{})
	origin := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, "data: first\n\n")
		w.(http.Flusher).Flush()
		close(started)
		select {
		case <-release:
		case <-r.Context().Done():
			close(cancelled)
		}
	}))
	defer origin.Close()
	authority := strings.TrimPrefix(origin.URL, "http://")
	paths := loopbackPolicyFiles(t, authority+"\n")
	broker, _ := testBroker(t)
	proxy := httptest.NewServer(NewProxyWithLoopback(fakeChecker{verdict: Verdict{Allow: true}}, fakeDialer{}, &fakeRecorder{}, LoopbackPolicy{Paths: paths}, broker))
	defer proxy.Close()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, origin.URL, nil)
	if err != nil {
		t.Fatal(err)
	}
	response, err := proxyClient(t, proxy.URL).Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("origin did not flush first SSE event")
	}
	line := make(chan string, 1)
	go func() {
		text, _ := bufio.NewReader(response.Body).ReadString('\n')
		line <- text
	}()
	select {
	case got := <-line:
		if got != "data: first\n" {
			t.Fatalf("first SSE line = %q", got)
		}
	case <-time.After(time.Second):
		t.Fatal("first SSE event was buffered until release")
	}
	cancel()
	select {
	case <-cancelled:
	case <-time.After(time.Second):
		t.Fatal("origin request context was not cancelled")
	}
	close(release)
}

func TestLoopbackMissingLayersAndNoBrokerDeny(t *testing.T) {
	var requests atomic.Int64
	origin := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) { requests.Add(1) }))
	defer origin.Close()
	authority := strings.TrimPrefix(origin.URL, "http://")
	paths := loopbackPolicyFiles(t, authority+"\n")
	broker, dials := testBroker(t)
	proxy := httptest.NewServer(NewProxyWithLoopback(fakeChecker{verdict: Verdict{Allow: true}, mode: ModeOpen}, fakeDialer{}, &fakeRecorder{}, LoopbackPolicy{Paths: paths}, broker))
	defer proxy.Close()
	if err := os.Remove(paths[1]); err != nil {
		t.Fatal(err)
	}
	response, err := proxyClient(t, proxy.URL).Get(origin.URL)
	if err != nil {
		t.Fatal(err)
	}
	response.Body.Close()
	if response.StatusCode != http.StatusForbidden || dials.Load() != 0 || requests.Load() != 0 {
		t.Fatalf("missing project = %d, dials=%d requests=%d", response.StatusCode, dials.Load(), requests.Load())
	}
	if err := os.WriteFile(paths[1], []byte(authority+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(paths[2]); err != nil {
		t.Fatal(err)
	}
	response, err = proxyClient(t, proxy.URL).Get(origin.URL)
	if err != nil {
		t.Fatal(err)
	}
	response.Body.Close()
	if response.StatusCode != http.StatusForbidden || dials.Load() != 0 || requests.Load() != 0 {
		t.Fatalf("missing run = %d, dials=%d requests=%d", response.StatusCode, dials.Load(), requests.Load())
	}
	if err := os.WriteFile(paths[2], []byte(authority+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	response, err = proxyClient(t, proxy.URL).Get(origin.URL)
	if err != nil {
		t.Fatal(err)
	}
	response.Body.Close()
	if response.StatusCode != http.StatusOK || dials.Load() != 1 || requests.Load() != 1 {
		t.Fatalf("restored layers = %d, dials=%d requests=%d", response.StatusCode, dials.Load(), requests.Load())
	}

	noBroker := httptest.NewServer(NewProxy(fakeChecker{verdict: Verdict{Allow: true}, mode: ModeOpen}, fakeDialer{}, &fakeRecorder{}))
	defer noBroker.Close()
	response, err = proxyClient(t, noBroker.URL).Get(origin.URL)
	if err != nil {
		t.Fatal(err)
	}
	response.Body.Close()
	if response.StatusCode != http.StatusForbidden {
		t.Fatalf("no broker local response = %d", response.StatusCode)
	}
}

func loopbackPolicyFiles(t *testing.T, contents string) []string {
	t.Helper()
	dir := t.TempDir()
	paths := []string{filepath.Join(dir, "global"), filepath.Join(dir, "project"), filepath.Join(dir, "run")}
	writeLoopbackFiles(t, paths, contents)
	return paths
}

func writeLoopbackFiles(t *testing.T, paths []string, contents string) {
	t.Helper()
	for _, path := range paths {
		if err := os.WriteFile(path, []byte(contents), 0o600); err != nil {
			t.Fatal(err)
		}
	}
}

func testBroker(t *testing.T) (BrokerClient, *atomic.Int64) {
	t.Helper()
	token := strings.Repeat("b", 64)
	var dials atomic.Int64
	ln := brokerListener(t, func(client net.Conn) {
		line, err := bufio.NewReader(client).ReadString('\n')
		if err != nil {
			t.Error(err)
			return
		}
		parts := strings.Fields(line)
		if len(parts) != 4 || parts[0] != "VHRN-BROKER/1" || parts[1] != "CONNECT" || parts[2] != token {
			t.Errorf("broker request = %q", line)
			return
		}
		upstream, err := net.Dial("tcp", parts[3])
		if err != nil {
			t.Error(err)
			return
		}
		defer upstream.Close()
		dials.Add(1)
		if _, err := client.Write([]byte("OK\n")); err != nil {
			t.Error(err)
			return
		}
		pipe(client, upstream)
	})
	return BrokerClient{Addr: ln.Addr().String(), Token: token, Timeout: time.Second}, &dials
}

func proxyClient(t *testing.T, proxyURL string) *http.Client {
	t.Helper()
	u, err := url.Parse(proxyURL)
	if err != nil {
		t.Fatal(err)
	}
	return &http.Client{Transport: &http.Transport{Proxy: http.ProxyURL(u)}}
}
