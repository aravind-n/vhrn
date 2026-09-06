package egress

import (
	"bufio"
	"context"
	"errors"
	"io"
	"net"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestNormalizeLoopbackAuthoritySharedCases(t *testing.T) {
	f, err := os.Open(filepath.Join("..", "..", "testdata", "loopback-authorities.tsv"))
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = f.Close() }()
	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		fields := strings.Split(scanner.Text(), "\t")
		if len(fields) == 0 || strings.HasPrefix(fields[0], "#") {
			continue
		}
		if len(fields) < 2 {
			t.Fatalf("bad test row %q", scanner.Text())
		}
		got, err := NormalizeLoopbackAuthority(fields[1])
		if fields[0] == "valid" {
			if err != nil || len(fields) != 3 || got != fields[2] {
				t.Errorf("NormalizeLoopbackAuthority(%q) = %q, %v; want %q, nil", fields[1], got, err, fields[2])
			}
		} else if fields[0] == "invalid" && err == nil {
			t.Errorf("NormalizeLoopbackAuthority(%q) = %q, nil; want error", fields[1], got)
		}
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
}

func TestLoopbackPolicyRequiresEveryLayer(t *testing.T) {
	dir := t.TempDir()
	paths := []string{filepath.Join(dir, "global"), filepath.Join(dir, "project"), filepath.Join(dir, "run")}
	for _, path := range paths {
		if err := os.WriteFile(path, []byte("localhost:1234\n"), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	p := LoopbackPolicy{Paths: paths}
	if !p.Allows("localhost:1234") {
		t.Fatal("grant was not allowed")
	}
	if err := os.Remove(paths[2]); err != nil {
		t.Fatal(err)
	}
	if p.Allows("localhost:1234") {
		t.Fatal("missing final layer allowed grant")
	}
	if err := os.WriteFile(paths[2], []byte("bad\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if p.Allows("localhost:1234") {
		t.Fatal("malformed final layer allowed grant")
	}
	if err := os.WriteFile(paths[2], []byte("localhost:1234\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if !p.Allows("localhost:1234") {
		t.Fatal("restored layers did not recover grant")
	}
	if err := os.WriteFile(paths[0], nil, 0o600); err != nil {
		t.Fatal(err)
	}
	for _, path := range paths[1:] {
		if err := os.WriteFile(path, nil, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	if p.Allows("localhost:1234") {
		t.Fatal("empty revoked layer allowed grant")
	}
	for _, path := range paths {
		if err := os.WriteFile(path, []byte("127.0.0.1:1234\n"), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	if p.Allows("localhost:1234") || !p.Allows("127.0.0.1:1234") {
		t.Fatal("distinct authorities were conflated")
	}
	if (LoopbackPolicy{Paths: paths[:2]}).Allows("127.0.0.1:1234") {
		t.Fatal("wrong number of layers allowed grant")
	}
	for _, contents := range []string{"\n", "127.0.0.1:1234\n\n127.0.0.1:1234\n"} {
		if err := os.WriteFile(paths[2], []byte(contents), 0o600); err != nil {
			t.Fatal(err)
		}
		if p.Allows("127.0.0.1:1234") {
			t.Errorf("blank policy record in %q allowed grant", contents)
		}
	}
}

func TestHTTPAuthorityLoopbackDefaultsAndExplicitPorts(t *testing.T) {
	cases := []struct {
		target string
		want   string
		local  bool
	}{
		{"http://localhost/x", "localhost:80", true},
		{"https://localhost/x", "localhost:443", true},
		{"http://127.0.0.1/x", "127.0.0.1:80", true},
		{"https://[::1]/x", "[::1]:443", true},
		{"http://[::1]:8080/x", "[::1]:8080", true},
		{"http://localhost:/x", "", false},
		{"http://[::1]:/x", "", false},
	}
	for _, tc := range cases {
		u, err := url.Parse(tc.target)
		if err != nil {
			t.Fatal(err)
		}
		if got, local := httpAuthority(u); got != tc.want || local != tc.local {
			t.Errorf("httpAuthority(%q) = %q, %v; want %q, %v", tc.target, got, local, tc.want, tc.local)
		}
	}
	if got, local := httpAuthority(&url.URL{Scheme: "http", Host: "[localhost]"}); got != "" || local {
		t.Errorf("bracketed localhost = %q, %v; want non-local", got, local)
	}
	if _, local := connectAuthority("localhost"); local {
		t.Error("CONNECT without a port was local")
	}
}

func TestBrokerClientProtocolAndCancellation(t *testing.T) {
	token := strings.Repeat("a", 64)
	ln := brokerListener(t, func(c net.Conn) {
		line, err := bufio.NewReader(c).ReadString('\n')
		if err != nil {
			t.Error(err)
			return
		}
		if line == "VHRN-BROKER/1 READY "+token+"\n" {
			_, _ = c.Write([]byte("OK\n"))
			return
		}
		if line != "VHRN-BROKER/1 CONNECT "+token+" localhost:80\n" {
			t.Errorf("broker line = %q", line)
			return
		}
		_, _ = c.Write([]byte("OK\npayload"))
	})
	b := BrokerClient{Addr: ln.Addr().String(), Token: token, Timeout: time.Second}
	if err := b.Ready(context.Background()); err != nil {
		t.Fatal(err)
	}
	c, err := b.Dial(context.Background(), "tcp", "LOCALHOST:00080")
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	payload, err := io.ReadAll(io.LimitReader(c, int64(len("payload"))))
	if err != nil || string(payload) != "payload" {
		t.Fatalf("payload = %q, %v", payload, err)
	}

	for _, response := range []string{"NO\n", "TOOLONG"} {
		ln := brokerListener(t, func(c net.Conn) {
			_, _ = bufio.NewReader(c).ReadString('\n')
			_, _ = c.Write([]byte(response))
		})
		_, err := (BrokerClient{Addr: ln.Addr().String(), Token: token, Timeout: time.Second}).Dial(context.Background(), "tcp", "localhost:80")
		if err == nil || strings.Contains(err.Error(), token) {
			t.Errorf("response %q error = %v", response, err)
		}
	}

	blocked := brokerListener(t, func(c net.Conn) {
		_, _ = bufio.NewReader(c).ReadString('\n')
		<-time.After(time.Second)
	})
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		done <- (BrokerClient{Addr: blocked.Addr().String(), Token: token, Timeout: time.Second}).Ready(ctx)
	}()
	time.Sleep(20 * time.Millisecond)
	cancel()
	select {
	case err := <-done:
		if err == nil || errors.Is(err, context.DeadlineExceeded) {
			t.Fatalf("cancelled readiness error = %v", err)
		}
	case <-time.After(300 * time.Millisecond):
		t.Fatal("cancelled readiness did not return promptly")
	}
}

func brokerListener(t *testing.T, serve func(net.Conn)) net.Listener {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = ln.Close() })
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			go func() { defer c.Close(); serve(c) }()
		}
	}()
	return ln
}
