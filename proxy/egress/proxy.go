package egress

import (
	"context"
	"fmt"
	"io"
	"net"
	"net/http"
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
	policy    Checker
	dialer    Dialer
	denyLog   DenyRecorder
	transport *http.Transport
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

func (px *Proxy) doHTTP(w http.ResponseWriter, r *http.Request) {
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
