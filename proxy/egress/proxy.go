package egress

import (
	"context"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// Checker decides whether egress to a host is permitted and reports the mode.
type Checker interface {
	Check(host string) Verdict
	Mode() Mode
}

// Dialer opens an outbound connection, enforcing its own address policy.
type Dialer interface {
	Dial(ctx context.Context, network, addr string) (net.Conn, error)
}

// DenyRecorder records a denied destination.
type DenyRecorder interface {
	Record(host string, mode Mode)
}

// Proxy is an HTTP CONNECT and plain-HTTP forward proxy gated by a Checker. It
// depends only on the three interfaces above, so its behaviour can be tested
// with fakes and its policy, dialer, and logger can each change independently.
type Proxy struct {
	policy         Checker
	dialer         Dialer
	denyLog        DenyRecorder
	transport      *http.Transport
	loopback       *LoopbackPolicy
	broker         BrokerClient
	localTransport *http.Transport
}

// NewProxyWithLoopback adds the separate, always-enforced local-endpoint path.
func NewProxyWithLoopback(policy Checker, dialer Dialer, denyLog DenyRecorder, loopback LoopbackPolicy, broker BrokerClient) *Proxy {
	px := NewProxy(policy, dialer, denyLog)
	px.loopback = &loopback
	px.broker = broker
	px.localTransport = &http.Transport{DialContext: broker.Dial, TLSHandshakeTimeout: 10 * time.Second}
	return px
}

// NewProxy wires a Proxy to its collaborators.
func NewProxy(policy Checker, dialer Dialer, denyLog DenyRecorder) *Proxy {
	return &Proxy{
		policy:  policy,
		dialer:  dialer,
		denyLog: denyLog,
		transport: &http.Transport{
			DialContext:         dialer.Dial,
			TLSHandshakeTimeout: 10 * time.Second,
		},
	}
}

func (px *Proxy) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	switch {
	case r.Method == http.MethodConnect:
		px.doConnect(w, r)
	case !r.URL.IsAbs():
		px.doDirect(w, r) // request addressed to the proxy itself (status/health)
	default:
		px.doHTTP(w, r)
	}
}

// permit checks the policy and records a denial when the verdict calls for it.
func (px *Proxy) permit(host string) bool {
	v := px.policy.Check(host)
	if v.Logged {
		px.denyLog.Record(host, v.Mode)
	}
	return v.Allow
}

// doDirect answers requests aimed at the proxy rather than through it. The
// status endpoint is what an in-box statusline polls to show live egress state.
func (px *Proxy) doDirect(w http.ResponseWriter, r *http.Request) {
	switch r.URL.Path {
	case "/__status":
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, "{\"mode\":%q}\n", px.policy.Mode())
	case "/healthz":
		fmt.Fprintln(w, "ok")
	default:
		http.NotFound(w, r)
	}
}

func (px *Proxy) doConnect(w http.ResponseWriter, r *http.Request) {
	if authority, local := connectAuthority(r.Host); local {
		if px.loopback == nil || !px.loopback.Allows(authority) {
			px.denyLocal(authority)
			http.Error(w, "blocked by vhrn local policy", http.StatusForbidden)
			return
		}
		px.doConnectLocal(w, r, authority)
		return
	}
	host := hostOnly(r.Host)
	if !px.permit(host) {
		http.Error(w, "blocked by vhrn egress policy: "+host, http.StatusForbidden)
		return
	}
	hij, ok := w.(http.Hijacker)
	if !ok {
		http.Error(w, "proxy: hijack unsupported", http.StatusInternalServerError)
		return
	}
	client, _, err := hij.Hijack()
	if err != nil {
		return
	}
	defer client.Close()

	upstream, err := px.dialer.Dial(context.Background(), "tcp", withPort(r.Host, "443"))
	if err != nil {
		_, _ = client.Write([]byte("HTTP/1.1 502 Bad Gateway\r\n\r\n"))
		return
	}
	defer upstream.Close()

	_, _ = client.Write([]byte("HTTP/1.1 200 Connection established\r\n\r\n"))
	pipe(client, upstream)
}

func (px *Proxy) doConnectLocal(w http.ResponseWriter, r *http.Request, authority string) {
	hij, ok := w.(http.Hijacker)
	if !ok {
		http.Error(w, "proxy: hijack unsupported", 500)
		return
	}
	client, rw, err := hij.Hijack()
	if err != nil {
		return
	}
	defer client.Close()
	upstream, err := px.broker.Dial(r.Context(), "tcp", authority)
	if err != nil {
		_, _ = client.Write([]byte("HTTP/1.1 502 Bad Gateway\r\n\r\n"))
		return
	}
	defer upstream.Close()
	_, _ = client.Write([]byte("HTTP/1.1 200 Connection established\r\n\r\n"))
	pipe(&bufferedConn{Conn: client, r: rw.Reader}, upstream)
}

func (px *Proxy) doHTTP(w http.ResponseWriter, r *http.Request) {
	if authority, local := httpAuthority(r.URL); local {
		if px.loopback == nil || !px.loopback.Allows(authority) {
			px.denyLocal(authority)
			http.Error(w, "blocked by vhrn local policy", http.StatusForbidden)
			return
		}
		px.doHTTPLocal(w, r)
		return
	}
	host := hostOnly(r.URL.Host)
	if !px.permit(host) {
		http.Error(w, "blocked by vhrn egress policy: "+host, http.StatusForbidden)
		return
	}
	r.RequestURI = ""
	r.Header.Del("Proxy-Connection")
	r.Header.Del("Proxy-Authorization")
	resp, err := px.transport.RoundTrip(r)
	if err != nil {
		http.Error(w, "proxy: "+err.Error(), http.StatusBadGateway)
		return
	}
	defer resp.Body.Close()
	for k, vs := range resp.Header {
		for _, v := range vs {
			w.Header().Add(k, v)
		}
	}
	w.WriteHeader(resp.StatusCode)
	_, _ = io.Copy(w, resp.Body)
}

func (px *Proxy) doHTTPLocal(w http.ResponseWriter, r *http.Request) {
	r.RequestURI = ""
	r.Header.Del("Proxy-Connection")
	r.Header.Del("Proxy-Authorization")
	resp, err := px.localTransport.RoundTrip(r)
	if err != nil {
		http.Error(w, "proxy: local origin unavailable", http.StatusBadGateway)
		return
	}
	defer resp.Body.Close()
	stop := context.AfterFunc(r.Context(), func() { _ = resp.Body.Close() })
	defer stop()
	for k, vs := range resp.Header {
		for _, v := range vs {
			w.Header().Add(k, v)
		}
	}
	w.WriteHeader(resp.StatusCode)
	if f, ok := w.(http.Flusher); ok {
		buf := make([]byte, 32*1024)
		for {
			n, e := resp.Body.Read(buf)
			if n > 0 {
				if _, err := w.Write(buf[:n]); err != nil {
					_ = resp.Body.Close()
					return
				}
				f.Flush()
			}
			if e != nil {
				return
			}
		}
	}
	_, _ = io.Copy(w, resp.Body)
}

func (px *Proxy) denyLocal(authority string) {
	px.denyLog.Record(authority, px.policy.Mode())
}

func connectAuthority(value string) (string, bool) {
	if _, _, err := net.SplitHostPort(value); err != nil {
		return "", false
	}
	a, err := NormalizeLoopbackAuthority(value)
	return a, err == nil
}
func httpAuthority(target *url.URL) (string, bool) {
	host := target.Hostname()
	if host == "" {
		return "", false
	}
	if strings.HasPrefix(target.Host, "[") && (strings.EqualFold(host, "localhost") || net.ParseIP(host).To4() != nil) {
		return "", false
	}
	port := target.Port()
	if port == "" {
		if hasEmptyExplicitPort(target.Host) {
			return "", false
		}
		port = "80"
		if target.Scheme == "https" {
			port = "443"
		}
	}
	a, err := NormalizeLoopbackAuthority(net.JoinHostPort(host, port))
	return a, err == nil
}

func hasEmptyExplicitPort(host string) bool {
	if strings.HasPrefix(host, "[") {
		end := strings.LastIndex(host, "]")
		return end >= 0 && host[end+1:] == ":"
	}
	return strings.Count(host, ":") == 1 && strings.HasSuffix(host, ":")
}

// pipe splices two connections, closing both once either direction ends so the
// opposite copy unblocks.
func pipe(a, b net.Conn) {
	done := make(chan struct{}, 2)
	go func() { _, _ = io.Copy(a, b); done <- struct{}{} }()
	go func() { _, _ = io.Copy(b, a); done <- struct{}{} }()
	<-done
	a.Close()
	b.Close()
	<-done
}

func hostOnly(hostport string) string {
	if h, _, err := net.SplitHostPort(hostport); err == nil {
		return h
	}
	return hostport
}

func withPort(hostport, defPort string) string {
	if _, _, err := net.SplitHostPort(hostport); err == nil {
		return hostport
	}
	return net.JoinHostPort(hostport, defPort)
}
