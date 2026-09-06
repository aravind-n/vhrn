// Command vhrn-proxy is the egress guard for vhrn: an HTTP CONNECT
// (and plain-HTTP) forward proxy that permits outbound connections only to an
// allowlisted set of domains. The guard logic lives in the egress package; this
// entrypoint only reads configuration from the environment and wires it up.
package main

import (
	"context"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"strings"
	"time"

	"vhrn/proxy/egress"
)

func env(k, def string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return def
}

func allowPaths(plural, singular string) []string {
	if plural != "" {
		return strings.Split(plural, ",")
	}
	if singular != "" {
		return []string{singular}
	}
	return []string{"/etc/vhrn/allowlist"}
}

func main() {
	paths := allowPaths(os.Getenv("VHRN_ALLOWLISTS"), os.Getenv("VHRN_ALLOWLIST"))
	modePath := env("VHRN_MODE_FILE", "/etc/vhrn/mode")
	listen := env("VHRN_PROXY_LISTEN", ":8080")
	listener, err := net.Listen("tcp", listen)
	if err != nil {
		log.Fatal(err)
	}

	policy := egress.NewPolicyPaths(paths, modePath)
	dialer := egress.SafeDialer{Timeout: 10 * time.Second}
	denyLog := egress.NewDenyLog(env("VHRN_DENY_LOG", ""))
	proxy := egress.NewProxy(policy, dialer, denyLog)
	if local, err := localConfigFrom(func(key string) string { return os.Getenv(key) }); err != nil {
		listener.Close()
		log.Fatal(err)
	} else if local != nil {
		if err := local.broker.Ready(context.Background()); err != nil {
			listener.Close()
			log.Fatal("broker readiness failed")
		}
		proxy = egress.NewProxyWithLoopback(policy, dialer, denyLog, egress.LoopbackPolicy{Paths: local.paths}, local.broker)
	}

	log.Printf("vhrn egress proxy on %s (allowlists=%s mode=%s)", listen, strings.Join(paths, ","), modePath)
	srv := &http.Server{
		Addr:              listen,
		Handler:           proxy,
		ReadHeaderTimeout: 30 * time.Second,
	}
	log.Fatal(srv.Serve(listener))
}

type localConfig struct {
	paths  []string
	broker egress.BrokerClient
}

func localConfigFrom(get func(string) string) (*localConfig, error) {
	paths, addr, tokenFile := get("VHRN_LOOPBACK_ALLOWLISTS"), get("VHRN_BROKER_ADDR"), get("VHRN_BROKER_TOKEN_FILE")
	if paths == "" && addr == "" && tokenFile == "" {
		return nil, nil
	}
	if paths == "" || addr == "" || tokenFile == "" {
		return nil, fmt.Errorf("incomplete loopback broker configuration")
	}
	parts := strings.Split(paths, ",")
	if len(parts) != 3 || parts[0] == "" || parts[1] == "" || parts[2] == "" {
		return nil, fmt.Errorf("invalid loopback policy paths")
	}
	token, err := os.ReadFile(tokenFile)
	if err != nil {
		return nil, fmt.Errorf("cannot read broker token")
	}
	value := string(token)
	if len(value) != 64 {
		return nil, fmt.Errorf("invalid broker token")
	}
	for _, c := range value {
		if !strings.ContainsRune("0123456789abcdef", c) {
			return nil, fmt.Errorf("invalid broker token")
		}
	}
	return &localConfig{parts, egress.BrokerClient{Addr: addr, Token: value, Timeout: 10 * time.Second}}, nil
}
