// Command vhrn-proxy is the egress guard for vhrn: an HTTP CONNECT
// (and plain-HTTP) forward proxy that permits outbound connections only to an
// allowlisted set of domains. The guard logic lives in the egress package; this
// entrypoint only reads configuration from the environment and wires it up.
package main

import (
	"log"
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

	policy := egress.NewPolicyPaths(paths, modePath)
	dialer := egress.SafeDialer{Timeout: 10 * time.Second}
	denyLog := egress.NewDenyLog(env("VHRN_DENY_LOG", ""))
	proxy := egress.NewProxy(policy, dialer, denyLog)

	log.Printf("vhrn egress proxy on %s (allowlists=%s mode=%s)", listen, strings.Join(paths, ","), modePath)
	srv := &http.Server{
		Addr:              listen,
		Handler:           proxy,
		ReadHeaderTimeout: 30 * time.Second,
	}
	log.Fatal(srv.ListenAndServe())
}
