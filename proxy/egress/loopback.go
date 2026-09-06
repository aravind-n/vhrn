package egress

import (
	"bufio"
	"context"
	"fmt"
	"net"
	"os"
	"strconv"
	"strings"
	"time"
)

// LoopbackPolicy rereads every required layer for every decision.
type LoopbackPolicy struct{ Paths []string }

func NormalizeLoopbackAuthority(value string) (string, error) {
	if value == "" || strings.TrimSpace(value) != value || !isASCII(value) {
		return "", fmt.Errorf("invalid loopback authority")
	}
	host, port, err := net.SplitHostPort(value)
	if err != nil {
		return "", fmt.Errorf("invalid loopback authority")
	}
	if port == "" || strings.TrimLeft(port, "0123456789") != "" {
		return "", fmt.Errorf("invalid loopback authority")
	}
	p, err := strconv.ParseUint(port, 10, 16)
	if err != nil || p == 0 {
		return "", fmt.Errorf("invalid loopback authority")
	}
	port = strconv.Itoa(int(p))
	if strings.EqualFold(host, "localhost") {
		if strings.HasPrefix(value, "[") {
			return "", fmt.Errorf("invalid loopback authority")
		}
		return "localhost:" + port, nil
	}
	if ip := net.ParseIP(host); ip != nil && ip.To4() == nil && ip.IsLoopback() {
		return "[::1]:" + port, nil
	}
	parts := strings.Split(host, ".")
	if len(parts) != 4 {
		return "", fmt.Errorf("invalid loopback authority")
	}
	for _, part := range parts {
		if part == "" || (len(part) > 1 && part[0] == '0') || strings.Trim(part, "0123456789") != "" || strings.HasPrefix(value, "[") {
			return "", fmt.Errorf("invalid loopback authority")
		}
		n, e := strconv.Atoi(part)
		if e != nil || n < 0 || n > 255 {
			return "", fmt.Errorf("invalid loopback authority")
		}
	}
	if parts[0] != "127" {
		return "", fmt.Errorf("invalid loopback authority")
	}
	return strings.Join(parts, ".") + ":" + port, nil
}
func isASCII(s string) bool {
	for _, b := range []byte(s) {
		if b > 127 {
			return false
		}
	}
	return true
}
func (p LoopbackPolicy) Allows(authority string) bool {
	if len(p.Paths) != 3 {
		return false
	}
	foundAny := false
	for _, path := range p.Paths {
		if path == "" {
			return false
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return false
		}
		found := false
		scanner := bufio.NewScanner(strings.NewReader(string(data)))
		for scanner.Scan() {
			line := scanner.Text()
			if line == "" {
				return false
			}
			normalized, err := NormalizeLoopbackAuthority(line)
			if err != nil {
				return false
			}
			if normalized == authority {
				found = true
			}
		}
		if scanner.Err() != nil {
			return false
		}
		foundAny = foundAny || found
	}
	return foundAny
}

// BrokerClient can dial only the host-provided broker address.
type BrokerClient struct {
	Addr, Token string
	Timeout     time.Duration
}

func (b BrokerClient) Dial(ctx context.Context, network, authority string) (net.Conn, error) {
	if network != "tcp" {
		return nil, fmt.Errorf("unsupported network")
	}
	canonical, err := NormalizeLoopbackAuthority(authority)
	if err != nil {
		return nil, err
	}
	d := net.Dialer{Timeout: b.Timeout}
	conn, err := d.DialContext(ctx, "tcp", b.Addr)
	if err != nil {
		return nil, err
	}
	stop := context.AfterFunc(ctx, func() { _ = conn.Close() })
	defer func() {
		if !stop() {
			return
		}
		_ = conn.Close()
	}()
	deadline := time.Now().Add(13 * time.Second)
	if err = conn.SetDeadline(deadline); err != nil {
		conn.Close()
		return nil, err
	}
	if _, err = fmt.Fprintf(conn, "VHRN-BROKER/1 CONNECT %s %s\n", b.Token, canonical); err != nil {
		conn.Close()
		return nil, err
	}
	r := bufio.NewReader(conn)
	line, err := brokerLine(r)
	if err != nil || line != "OK\n" {
		conn.Close()
		return nil, fmt.Errorf("broker denied local request")
	}
	if err = conn.SetDeadline(time.Time{}); err != nil {
		conn.Close()
		return nil, err
	}
	if !stop() {
		conn.Close()
		return nil, ctx.Err()
	}
	// The deferred cleanup is disabled by stopping the callback above. The
	// connection now belongs to the caller.
	return &bufferedConn{Conn: conn, r: r}, nil
}
func (b BrokerClient) Ready(ctx context.Context) error {
	c, err := (&net.Dialer{Timeout: b.Timeout}).DialContext(ctx, "tcp", b.Addr)
	if err != nil {
		return err
	}
	defer c.Close()
	stop := context.AfterFunc(ctx, func() { _ = c.Close() })
	defer stop()
	if err = c.SetDeadline(time.Now().Add(3 * time.Second)); err != nil {
		return err
	}
	if _, err = fmt.Fprintf(c, "VHRN-BROKER/1 READY %s\n", b.Token); err != nil {
		return err
	}
	line, err := brokerLine(bufio.NewReader(c))
	if err != nil || line != "OK\n" {
		return fmt.Errorf("broker readiness failed")
	}
	if !stop() {
		return ctx.Err()
	}
	return nil
}
func brokerLine(r *bufio.Reader) (string, error) {
	var out [4]byte
	for i := 0; i < len(out); i++ {
		b, err := r.ReadByte()
		if err != nil {
			return "", err
		}
		out[i] = b
		if b == '\n' {
			return string(out[:i+1]), nil
		}
	}
	return "", fmt.Errorf("invalid broker response")
}

type bufferedConn struct {
	net.Conn
	r *bufio.Reader
}

func (c *bufferedConn) Read(p []byte) (int, error) { return c.r.Read(p) }
