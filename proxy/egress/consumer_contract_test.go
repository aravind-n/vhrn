package egress

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func contractRows(t *testing.T, name string) [][]string {
	t.Helper()
	f, err := os.Open(filepath.Join("..", "..", "testdata", name))
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = f.Close() }()
	var rows [][]string
	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		line := scanner.Text()
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		rows = append(rows, strings.Split(line, "\t"))
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
	return rows
}

func TestContractDomainRows(t *testing.T) {
	for _, row := range contractRows(t, "domain-policy.tsv") {
		if len(row) != 5 {
			t.Fatalf("bad domain row %#v", row)
		}
		switch row[0] {
		case "entry":
			got, ok := normEntry(row[1])
			wantOK := row[4] == "accept"
			if ok != wantOK || (ok && got != row[2]) {
				t.Errorf("entry %q = %q, %v; want %q, %v", row[1], got, ok, row[2], wantOK)
			}
		case "host":
			got := normHost(row[1])
			if got != row[2] {
				t.Errorf("host %q = %q, want %q", row[1], got, row[2])
			}
			want := row[4] == "allow"
			if allowed := hostAllowed(row[1], []string{row[3]}); allowed != want {
				t.Errorf("host %q allowed = %v, want %v", row[1], allowed, want)
			}
		default:
			t.Fatalf("unknown domain row %#v", row)
		}
	}
}

func TestContractAddressRows(t *testing.T) {
	for _, row := range contractRows(t, "ip-addresses.tsv") {
		if len(row) != 3 {
			t.Fatalf("bad address row %#v", row)
		}
		if row[0] == "empty answer set" || strings.Contains(row[0], ",") {
			continue
		}
		ip := net.ParseIP(row[0])
		if ip == nil {
			t.Fatalf("invalid address fixture %q", row[0])
		}
		want := row[1] == "allow"
		if got := isPublicIP(ip); got != want {
			t.Errorf("address %q allowed = %v, want %v", row[0], got, want)
		}
	}
	public := net.ParseIP("8.8.8.8")
	loopback := net.ParseIP("127.0.0.1")
	if _, err := firstPublicIP(nil); err == nil {
		t.Error("empty address set was accepted")
	}
	if _, err := firstPublicIP([]net.IP{public, loopback}); err == nil {
		t.Error("mixed address set was accepted")
	}
}

func TestContractModeRows(t *testing.T) {
	for _, row := range contractRows(t, "proxy-modes.tsv") {
		if len(row) != 6 {
			t.Fatalf("bad mode row %#v", row)
		}
		dir := t.TempDir()
		paths := make([]string, 5)
		for i := range paths {
			paths[i] = filepath.Join(dir, "allow-"+string(rune('a'+i)))
			contents := "allowed.example\n"
			if row[1] == "replace" {
				contents = "blocked.example\n"
			}
			if err := os.WriteFile(paths[i], []byte(contents), 0o600); err != nil {
				t.Fatal(err)
			}
		}
		modePath := filepath.Join(dir, "mode")
		if err := os.WriteFile(modePath, []byte(row[0]), 0o600); err != nil {
			t.Fatal(err)
		}
		policy := NewPolicyPaths(paths, modePath)
		switch row[1] {
		case "missing-layer":
			if err := os.Remove(paths[2]); err != nil {
				t.Fatal(err)
			}
		case "malformed-layer":
			if err := os.WriteFile(paths[2], []byte("https://bad.example\n"), 0o600); err != nil {
				t.Fatal(err)
			}
		case "read-error":
			if err := os.Remove(paths[2]); err != nil {
				t.Fatal(err)
			}
			if err := os.Mkdir(paths[2], 0o700); err != nil {
				t.Fatal(err)
			}
		case "missing-mode":
			if err := os.Remove(modePath); err != nil {
				t.Fatal(err)
			}
		case "replace":
			if policy.Check("allowed.example").Allow {
				t.Fatal("replacement row allowed before replacement")
			}
			if err := os.WriteFile(paths[4]+".next", []byte("allowed.example\n"), 0o600); err != nil {
				t.Fatal(err)
			}
			if err := os.Rename(paths[4]+".next", paths[4]); err != nil {
				t.Fatal(err)
			}
		}
		host := "blocked.example"
		if row[2] == "yes" {
			host = "allowed.example"
		}
		verdict := policy.Check(host)
		if verdict.Allow != (row[3] == "yes") || verdict.Logged != (row[4] == "yes") || string(verdict.Mode) != row[5] {
			t.Errorf("mode row %#v produced %#v", row, verdict)
		}
	}
}

func TestContractBrokerFrameRows(t *testing.T) {
	for _, row := range contractRows(t, "broker-frames.tsv") {
		if len(row) != 5 {
			t.Fatalf("bad broker row %#v", row)
		}
		token := strings.Repeat("a", 64)
		request := make(chan string, 1)
		response := strings.ReplaceAll(row[2], `\n`, "\n")
		ln := brokerListener(t, func(c net.Conn) {
			line, _ := bufio.NewReader(c).ReadString('\n')
			request <- line
			if response != "timeout" {
				_, _ = io.WriteString(c, response+row[3])
				return
			}
			<-time.After(time.Second)
		})
		b := BrokerClient{Addr: ln.Addr().String(), Token: token, Timeout: time.Second}
		ctx := context.Background()
		var cancel context.CancelFunc
		if row[4] == "cancelled" {
			ctx, cancel = context.WithTimeout(ctx, 50*time.Millisecond)
			defer cancel()
		}
		var err error
		if row[0] == "ready" {
			err = b.Ready(ctx)
		} else {
			var c net.Conn
			c, err = b.Dial(ctx, "tcp", "localhost:80")
			if err == nil {
				defer c.Close()
				payload, readErr := io.ReadAll(io.LimitReader(c, int64(len(row[3]))))
				if readErr != nil || string(payload) != row[3] {
					t.Errorf("broker row %#v payload = %q, %v", row, payload, readErr)
				}
			}
		}
		expected := strings.ReplaceAll(strings.ReplaceAll(row[1], "<token>", token), `\n`, "\n")
		select {
		case got := <-request:
			if got != expected {
				t.Errorf("broker row %#v request = %q, want %q", row, got, expected)
			}
		case <-time.After(time.Second):
			t.Fatalf("broker row %#v did not send a request", row)
		}
		if (row[4] == "ready" || row[4] == "connected") != (err == nil) {
			t.Errorf("broker row %#v error = %v", row, err)
		}
		if err != nil && strings.Contains(err.Error(), token) {
			t.Errorf("broker row %#v exposed token", row)
		}
	}
}

func TestContractHTTPRowsExerciseHandler(t *testing.T) {
	for _, row := range contractRows(t, "proxy-http-cases.tsv") {
		if len(row) != 5 || row[0] == "" || row[1] == "" || row[3] == "" || row[4] == "" {
			t.Fatalf("bad HTTP row %#v", row)
		}
		switch row[0] {
		case "public-http":
			contractPublicHTTPRow(t, row)
		case "public-http-stream":
			contractPublicHTTPStreamRow(t, row, false)
		case "public-http-cancel":
			contractPublicHTTPStreamRow(t, row, true)
		case "public-http-pool":
			contractPublicPoolRow(t, row)
		case "public-connect":
			contractPublicConnectRow(t, row)
		case "local-http":
			contractLocalHTTPRow(t, row)
		case "local-connect":
			contractLocalConnectRow(t, row)
		case "tunnel-relay":
			contractTunnelRelayRow(t, row)
		default:
			t.Fatalf("unknown HTTP row %#v", row)
		}
	}
}

func TestContractProcessRows(t *testing.T) {
	for _, row := range contractRows(t, "proxy-process-cases.tsv") {
		if len(row) != 4 {
			t.Fatalf("bad process row %#v", row)
		}
		if row[0] != "direct" && row[0] != "diagnostic" {
			continue
		}
		path := row[1]
		logPath := ""
		if row[0] == "diagnostic" {
			path = "http://" + row[1] + "/"
			logPath = filepath.Join(t.TempDir(), "denied.log")
		}
		proxy := NewProxy(fakeChecker{verdict: Verdict{Allow: false, Logged: true, Mode: ModeEnforce}, mode: ModeEnforce}, fakeDialer{}, NewDenyLog(logPath))
		recorder := httptest.NewRecorder()
		request := httptest.NewRequest(http.MethodGet, path, nil)
		proxy.ServeHTTP(recorder, request)
		var want int
		if _, err := fmt.Sscanf(row[2], "%d", &want); err != nil {
			t.Fatal(err)
		}
		if recorder.Code != want {
			t.Errorf("process row %#v status=%d", row, recorder.Code)
		}
		if row[1] == "/healthz" && (recorder.Body.String() != "ok\n" || row[3] != "ok line-feed") {
			t.Errorf("health body=%q", recorder.Body.String())
		}
		if row[1] == "/__status" && (!strings.Contains(recorder.Body.String(), `"mode":"enforce"`) || !strings.HasSuffix(recorder.Body.String(), "\n") || row[3] != "application/json mode JSON line-feed") {
			t.Errorf("status body=%q", recorder.Body.String())
		}
		if row[1] == "/__status" && recorder.Header().Get("Content-Type") != "application/json" {
			t.Errorf("status content type=%q", recorder.Header().Get("Content-Type"))
		}
		if row[1] == "/not-found" && (recorder.Body.String() != "404 page not found\n" || row[3] != "404 page not found line-feed") {
			t.Errorf("not-found body=%q", recorder.Body.String())
		}
		if row[0] == "diagnostic" {
			data, err := os.ReadFile(logPath)
			if err != nil {
				t.Fatal(err)
			}
			parts := strings.Split(string(data), "\t")
			if len(parts) != 2 || parts[1] != row[1]+"\n" {
				t.Fatalf("diagnostic row %#v log=%q", row, data)
			}
			if _, err := time.Parse(time.RFC3339, parts[0]); err != nil || strings.Contains(string(data), strings.Repeat("a", 64)) {
				t.Fatalf("diagnostic row %#v log=%q err=%v", row, data, err)
			}
		}
	}
}

func contractPublicHTTPRow(t *testing.T, row []string) {
	t.Helper()
	if row[1] == "/origin-form" {
		dialer := &capturingDialer{}
		px := httptest.NewServer(NewProxy(fakeChecker{}, dialer, &fakeRecorder{}))
		defer px.Close()
		response, err := http.Get(px.URL + row[1])
		if err != nil || response.StatusCode != http.StatusNotFound || len(dialer.addresses()) != 0 || row[2] != "direct" || row[3] != "404" {
			t.Fatalf("row %#v response=%v err=%v", row, response, err)
		}
		response.Body.Close()
		return
	}
	if strings.HasPrefix(row[1], "https:") {
		var requests atomic.Int64
		origin := httptest.NewTLSServer(http.HandlerFunc(func(http.ResponseWriter, *http.Request) { requests.Add(1) }))
		defer origin.Close()
		px := httptest.NewServer(NewProxy(fakeChecker{verdict: Verdict{Allow: true}}, fakeDialer{target: strings.TrimPrefix(origin.URL, "https://")}, &fakeRecorder{}))
		defer px.Close()
		conn := dialProxy(t, px.URL)
		defer conn.Close()
		_, _ = fmt.Fprintf(conn, "GET %s HTTP/1.1\r\nHost: allowed.example\r\n\r\n", row[1])
		request, _ := http.NewRequest(http.MethodGet, row[1], nil)
		response, err := http.ReadResponse(bufio.NewReader(conn), request)
		if err != nil {
			t.Fatal(err)
		}
		defer response.Body.Close()
		if response.StatusCode != http.StatusBadGateway || requests.Load() != 0 || row[2] != "enforce matched" || row[3] != "502" || row[4] != "untrusted certificate fails verified TLS handshake; zero origin requests" {
			t.Fatalf("%q status = %d", row[1], response.StatusCode)
		}
		return
	}
	var method, authority, body string
	var connection, nominated string
	origin := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		method, authority = r.Method, r.Host
		payload, _ := io.ReadAll(r.Body)
		body = string(payload)
		connection, nominated = r.Header.Get("Connection"), r.Header.Get("X-Remove")
		if r.Header.Get("Proxy-Connection") != "" || r.Header.Get("Proxy-Authorization") != "" {
			t.Errorf("hop headers reached origin: %#v", r.Header)
		}
		fmt.Fprint(w, "origin")
	}))
	defer origin.Close()
	verdict := Verdict{Allow: true}
	mode := ModeEnforce
	if strings.Contains(row[2], "unmatched") {
		verdict = Verdict{Allow: false, Logged: true, Mode: ModeEnforce}
	}
	if strings.HasPrefix(row[2], "report") {
		verdict, mode = Verdict{Allow: true, Logged: true, Mode: ModeReport}, ModeReport
	}
	if strings.HasPrefix(row[2], "open") {
		verdict, mode = Verdict{Allow: true, Mode: ModeOpen}, ModeOpen
	}
	recorder := &fakeRecorder{}
	px := httptest.NewServer(NewProxy(fakeChecker{verdict: verdict, mode: mode}, fakeDialer{target: strings.TrimPrefix(origin.URL, "http://")}, recorder))
	defer px.Close()
	conn := dialProxy(t, px.URL)
	defer conn.Close()
	_, _ = fmt.Fprintf(conn, "POST %s HTTP/1.1\r\nHost: allowed.example\r\nConnection: X-Remove\r\nX-Remove: value\r\nProxy-Connection: keep-alive\r\nProxy-Authorization: secret\r\nContent-Length: 4\r\n\r\nbody", row[1])
	request, _ := http.NewRequest(http.MethodPost, row[1], nil)
	response, err := http.ReadResponse(bufio.NewReader(conn), request)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	if row[3] == "403" {
		if response.StatusCode != http.StatusForbidden || recorder.count() != 1 {
			t.Fatalf("row %#v status=%d records=%d", row, response.StatusCode, recorder.count())
		}
		return
	}
	expectedAuthority := strings.Split(strings.TrimPrefix(row[1], "http://"), "/")[0]
	if response.StatusCode != http.StatusOK || method != http.MethodPost || authority != expectedAuthority || body != "body" {
		t.Fatalf("row %#v status=%d method=%q authority=%q body=%q", row, response.StatusCode, method, authority, body)
	}
	if connection != "X-Remove" || nominated != "value" {
		t.Fatalf("row %#v retained headers Connection=%q X-Remove=%q", row, connection, nominated)
	}
	wantRecords := 0
	if strings.Contains(row[2], "report") {
		wantRecords = 1
	}
	if recorder.count() != wantRecords {
		t.Fatalf("row %#v records=%d", row, recorder.count())
	}
}

func contractPublicConnectRow(t *testing.T, row []string) {
	t.Helper()
	if row[1] == "missing authority" {
		dialer := &capturingDialer{}
		px := httptest.NewServer(NewProxy(fakeChecker{}, dialer, &fakeRecorder{}))
		defer px.Close()
		conn := dialProxy(t, px.URL)
		defer conn.Close()
		_ = conn.SetDeadline(time.Now().Add(time.Second))
		_, _ = io.WriteString(conn, "CONNECT HTTP/1.1\r\n\r\n")
		line, err := bufio.NewReader(conn).ReadString('\n')
		if err != nil || !strings.Contains(line, "400") || len(dialer.addresses()) != 0 || row[2] != "invalid" || row[3] != "400" {
			t.Fatalf("row %#v response=%q err=%v", row, line, err)
		}
		return
	}
	allow := !strings.Contains(row[2], "unmatched")
	dialer := &capturingDialer{target: echoServer(t)}
	if strings.Contains(row[2], "dial failure") {
		dialer.err = fmt.Errorf("unavailable")
	}
	px := httptest.NewServer(NewProxy(fakeChecker{verdict: Verdict{Allow: allow, Logged: !allow, Mode: ModeEnforce}}, dialer, &fakeRecorder{}))
	defer px.Close()
	conn := dialProxy(t, px.URL)
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(time.Second))
	_, _ = fmt.Fprintf(conn, "CONNECT %s HTTP/1.1\r\nHost: %s\r\n\r\npreface", row[1], row[1])
	reader := bufio.NewReader(conn)
	line, err := reader.ReadString('\n')
	if err != nil || !strings.Contains(line, row[3]) {
		t.Fatalf("row %#v response=%q err=%v", row, line, err)
	}
	if row[3] != "200" {
		if len(dialer.addresses()) != 0 && !strings.Contains(row[2], "dial failure") {
			t.Fatalf("row %#v dialed %#v", row, dialer.addresses())
		}
		return
	}
	skipHeaders(reader)
	want := row[1]
	if !strings.Contains(want, ":") {
		want += ":443"
	}
	if addresses := dialer.addresses(); len(addresses) != 1 || addresses[0] != want {
		t.Fatalf("row %#v addresses=%#v", row, addresses)
	}
	if _, err := reader.Peek(1); err == nil {
		t.Fatalf("row %#v relayed parser preface", row)
	}
	_ = conn.SetDeadline(time.Now().Add(time.Second))
	if _, err := io.WriteString(conn, "relay"); err != nil {
		t.Fatal(err)
	}
	got := make([]byte, len("relay"))
	if _, err := io.ReadFull(reader, got); err != nil || string(got) != "relay" {
		t.Fatalf("row %#v relay=%q err=%v", row, got, err)
	}
}

func contractPublicHTTPStreamRow(t *testing.T, row []string, cancelClient bool) {
	t.Helper()
	if row[2] != "enforce matched" {
		t.Fatalf("bad stream policy %#v", row)
	}
	if cancelClient && (row[3] != "origin cancelled" || row[4] != "downstream disconnect cancels origin") {
		t.Fatalf("bad cancel row %#v", row)
	}
	if !cancelClient && (row[3] != "buffered" || row[4] != "origin-flushed first chunk reaches client only after origin completion") {
		t.Fatalf("bad stream row %#v", row)
	}
	started, cancelled, release := make(chan struct{}), make(chan struct{}), make(chan struct{})
	origin := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, "data: first\n\n")
		w.(http.Flusher).Flush()
		close(started)
		select {
		case <-r.Context().Done():
			close(cancelled)
		case <-release:
		}
	}))
	defer origin.Close()
	px := httptest.NewServer(NewProxy(fakeChecker{verdict: Verdict{Allow: true}}, fakeDialer{target: strings.TrimPrefix(origin.URL, "http://")}, &fakeRecorder{}))
	defer px.Close()
	conn := dialProxy(t, px.URL)
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(time.Second))
	_, _ = fmt.Fprintf(conn, "GET %s HTTP/1.1\r\nHost: allowed.example\r\n\r\n", row[1])
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("public origin did not start")
	}
	if cancelClient {
		_ = conn.Close()
		select {
		case <-cancelled:
		case <-time.After(time.Second):
			t.Fatal("public cancellation did not reach origin")
		}
		return
	}
	reader := bufio.NewReader(conn)
	if _, err := reader.Peek(1); err == nil {
		t.Fatal("public first chunk was not buffered")
	}
	close(release)
	_ = conn.SetDeadline(time.Now().Add(time.Second))
	request, err := http.NewRequest(http.MethodGet, row[1], nil)
	if err != nil {
		t.Fatal(err)
	}
	response, err := http.ReadResponse(reader, request)
	if err != nil || response.StatusCode != http.StatusOK {
		t.Fatalf("public response=%v err=%v", response, err)
	}
	body, err := io.ReadAll(response.Body)
	response.Body.Close()
	if err != nil || string(body) != "data: first\n\n" {
		t.Fatalf("public released body=%q err=%v", body, err)
	}
}

func contractPublicPoolRow(t *testing.T, row []string) {
	t.Helper()
	if row[2] != "enforce then revoked" || row[3] != "403" || row[4] != "first two requests reuse one origin connection; revoked request makes no origin request or dial" {
		t.Fatalf("bad pool row %#v", row)
	}
	var requests, connections int
	var mu sync.Mutex
	origin := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		mu.Lock()
		requests++
		mu.Unlock()
		fmt.Fprint(w, "pool")
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
	dir := t.TempDir()
	allow, mode := filepath.Join(dir, "allow"), filepath.Join(dir, "mode")
	if err := os.WriteFile(allow, []byte("allowed.example\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(mode, []byte("enforce\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	dialer := &capturingDialer{target: strings.TrimPrefix(origin.URL, "http://")}
	px := httptest.NewServer(NewProxy(NewPolicy(allow, mode), dialer, &fakeRecorder{}))
	defer px.Close()
	client := proxyClient(t, px.URL)
	defer client.CloseIdleConnections()
	for range 2 {
		response, err := client.Get("http://allowed.example/pool")
		if err != nil {
			t.Fatal(err)
		}
		_, _ = io.Copy(io.Discard, response.Body)
		response.Body.Close()
		if response.StatusCode != http.StatusOK {
			t.Fatalf("pool allowed status=%d", response.StatusCode)
		}
	}
	mu.Lock()
	beforeRequests, beforeConnections := requests, connections
	mu.Unlock()
	beforeDials := len(dialer.addresses())
	if beforeRequests != 2 || beforeConnections != 1 || beforeDials != 1 {
		t.Fatalf("pool before requests=%d connections=%d dials=%d", beforeRequests, beforeConnections, beforeDials)
	}
	if err := os.WriteFile(allow+".next", nil, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(allow+".next", allow); err != nil {
		t.Fatal(err)
	}
	response, err := client.Get("http://allowed.example/pool")
	if err != nil {
		t.Fatal(err)
	}
	response.Body.Close()
	mu.Lock()
	afterRequests := requests
	mu.Unlock()
	if response.StatusCode != http.StatusForbidden || afterRequests != beforeRequests || len(dialer.addresses()) != beforeDials || row[3] != "403" {
		t.Fatalf("pool revoked status=%d requests=%d dials=%d", response.StatusCode, afterRequests, len(dialer.addresses()))
	}
}

func contractTunnelRelayRow(t *testing.T, row []string) {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	upstreamEOF := make(chan error, 1)
	go func() {
		connection, err := listener.Accept()
		if err != nil {
			upstreamEOF <- err
			return
		}
		defer connection.Close()
		tcp, ok := connection.(*net.TCPConn)
		if !ok {
			upstreamEOF <- fmt.Errorf("accepted connection is %T", connection)
			return
		}
		if err := tcp.SetDeadline(time.Now().Add(time.Second)); err != nil {
			upstreamEOF <- err
			return
		}
		if _, err := io.WriteString(tcp, "data"); err != nil {
			upstreamEOF <- err
			return
		}
		if err := tcp.CloseWrite(); err != nil {
			upstreamEOF <- err
			return
		}
		var byteValue [1]byte
		_, err = tcp.Read(byteValue[:])
		upstreamEOF <- err
	}()
	dialer := &capturingDialer{target: listener.Addr().String()}
	px := httptest.NewServer(NewProxy(fakeChecker{verdict: Verdict{Allow: true}}, dialer, &fakeRecorder{}))
	defer px.Close()
	client := dialProxy(t, px.URL)
	defer client.Close()
	_ = client.SetDeadline(time.Now().Add(time.Second))
	_, _ = fmt.Fprint(client, "CONNECT allowed.example:443 HTTP/1.1\r\nHost: allowed.example:443\r\n\r\n")
	reader := bufio.NewReader(client)
	line, err := reader.ReadString('\n')
	if err != nil || !strings.Contains(line, "200") {
		t.Fatalf("relay status=%q err=%v", line, err)
	}
	skipHeaders(reader)
	payload := make([]byte, 4)
	if _, err := io.ReadFull(reader, payload); err != nil || string(payload) != "data" {
		t.Fatalf("relay payload=%q err=%v", payload, err)
	}
	_, clientErr := reader.ReadByte()
	if !errors.Is(clientErr, io.EOF) {
		t.Fatalf("client EOF after upstream write-close = %v", clientErr)
	}
	select {
	case err := <-upstreamEOF:
		if err != io.EOF {
			t.Fatalf("upstream read after write-close = %v", err)
		}
	case <-time.After(time.Second):
		t.Fatal("upstream did not observe EOF")
	}
	if row[1] != "public CONNECT" || row[2] != "upstream write-close" || row[3] != "client EOF" || row[4] != "client receives data then EOF; upstream read receives EOF" {
		t.Fatalf("bad relay row %#v", row)
	}
}

type capturingDialer struct {
	target string
	err    error
	mu     sync.Mutex
	seen   []string
}

func (d *capturingDialer) Dial(ctx context.Context, network, address string) (net.Conn, error) {
	d.mu.Lock()
	d.seen = append(d.seen, address)
	d.mu.Unlock()
	if d.err != nil {
		return nil, d.err
	}
	return (&net.Dialer{}).DialContext(ctx, network, d.target)
}

func (d *capturingDialer) addresses() []string {
	d.mu.Lock()
	defer d.mu.Unlock()
	return append([]string(nil), d.seen...)
}

func contractLocalHTTPRow(t *testing.T, row []string) {
	t.Helper()
	started, cancelled := make(chan struct{}), make(chan struct{})
	release := make(chan struct{})
	origin := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, "data: first\n\n")
		w.(http.Flusher).Flush()
		close(started)
		select {
		case <-r.Context().Done():
			close(cancelled)
		case <-release:
		}
	}))
	defer origin.Close()
	_, port, err := net.SplitHostPort(strings.TrimPrefix(origin.URL, "http://"))
	if err != nil {
		t.Fatal(err)
	}
	authority := "localhost:" + port
	paths := loopbackPolicyFiles(t, authority+"\n")
	broker, dials := testBroker(t)
	public := &capturingDialer{}
	px := httptest.NewServer(NewProxyWithLoopback(fakeChecker{}, public, &fakeRecorder{}, LoopbackPolicy{Paths: paths}, broker))
	defer px.Close()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, "http://"+authority+"/path", nil)
	if err != nil {
		t.Fatal(err)
	}
	response, err := proxyClient(t, px.URL).Do(request)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("local origin did not start")
	}
	line := make(chan string, 1)
	go func() { value, _ := bufio.NewReader(response.Body).ReadString('\n'); line <- value }()
	select {
	case value := <-line:
		if value != "data: first\n" {
			t.Fatalf("row %#v first chunk=%q", row, value)
		}
	case <-time.After(time.Second):
		t.Fatal("local first chunk did not flush")
	}
	cancel()
	select {
	case <-cancelled:
	case <-time.After(time.Second):
		t.Fatal("local cancellation did not reach origin")
	}
	close(release)
	if response.StatusCode != http.StatusOK || dials.Load() != 1 || len(public.addresses()) != 0 {
		t.Fatalf("row %#v status=%d dials=%d", row, response.StatusCode, dials.Load())
	}
}

func contractLocalConnectRow(t *testing.T, row []string) {
	t.Helper()
	echo := echoServer(t)
	_, port, err := net.SplitHostPort(echo)
	if err != nil {
		t.Fatal(err)
	}
	authority := "localhost:" + port
	paths := loopbackPolicyFiles(t, authority+"\n")
	broker, dials := testBroker(t)
	public := &capturingDialer{}
	px := httptest.NewServer(NewProxyWithLoopback(fakeChecker{}, public, &fakeRecorder{}, LoopbackPolicy{Paths: paths}, broker))
	defer px.Close()
	conn := dialProxy(t, px.URL)
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(time.Second))
	_, _ = fmt.Fprintf(conn, "CONNECT %s HTTP/1.1\r\nHost: %s\r\n\r\npreface", authority, authority)
	reader := bufio.NewReader(conn)
	line, err := reader.ReadString('\n')
	if err != nil || !strings.Contains(line, row[3]) {
		t.Fatalf("row %#v response=%q err=%v", row, line, err)
	}
	skipHeaders(reader)
	preface := make([]byte, len("preface"))
	if _, err := io.ReadFull(reader, preface); err != nil || string(preface) != "preface" {
		t.Fatalf("row %#v preface=%q err=%v", row, preface, err)
	}
	writeLoopbackFiles(t, paths, "")
	if _, err := io.WriteString(conn, "live"); err != nil {
		t.Fatal(err)
	}
	live := make([]byte, len("live"))
	if _, err := io.ReadFull(reader, live); err != nil || string(live) != "live" {
		t.Fatalf("row %#v live=%q err=%v", row, live, err)
	}
	newConn := dialProxy(t, px.URL)
	defer newConn.Close()
	_ = newConn.SetDeadline(time.Now().Add(time.Second))
	_, _ = fmt.Fprintf(newConn, "CONNECT %s HTTP/1.1\r\nHost: %s\r\n\r\n", authority, authority)
	denied, err := bufio.NewReader(newConn).ReadString('\n')
	if err != nil || !strings.Contains(denied, "403") || dials.Load() != 1 || len(public.addresses()) != 0 {
		t.Fatalf("row %#v denied=%q err=%v dials=%d public=%#v", row, denied, err, dials.Load(), public.addresses())
	}
}
