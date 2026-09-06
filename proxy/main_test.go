package main

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"vhrn/proxy/egress"
)

func TestAllowPaths(t *testing.T) {
	if got := allowPaths("a,,b", "old"); !reflect.DeepEqual(got, []string{"a", "", "b"}) {
		t.Fatalf("plural = %#v", got)
	}
	if got := allowPaths("", "old"); !reflect.DeepEqual(got, []string{"old"}) {
		t.Fatalf("singular = %#v", got)
	}
	if got := allowPaths("", ""); !reflect.DeepEqual(got, []string{"/etc/vhrn/allowlist"}) {
		t.Fatalf("default = %#v", got)
	}
}

func TestEmptyPluralPathFailsClosedThroughParser(t *testing.T) {
	dir := t.TempDir()
	a, b, mode := filepath.Join(dir, "a"), filepath.Join(dir, "b"), filepath.Join(dir, "mode")
	for _, path := range []string{a, b} {
		if err := os.WriteFile(path, []byte("allowed.example\n"), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(mode, []byte("open\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	p := egress.NewPolicyPaths(allowPaths(a+",,"+b, "old"), mode)
	v := p.Check("allowed.example")
	if v.Allow || !v.Logged || v.Mode != egress.ModeEnforce {
		t.Fatalf("empty plural = %#v", v)
	}
}

func TestLocalConfigFrom(t *testing.T) {
	tokenPath := filepath.Join(t.TempDir(), "token")
	validToken := "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
	if err := os.WriteFile(tokenPath, []byte(validToken), 0o600); err != nil {
		t.Fatal(err)
	}
	cases := []struct {
		name string
		vars map[string]string
		ok   bool
		nil  bool
	}{
		{"absent", nil, true, true},
		{"partial", map[string]string{"VHRN_BROKER_ADDR": "127.0.0.1:1"}, false, true},
		{"wrong path count", map[string]string{"VHRN_LOOPBACK_ALLOWLISTS": "a,b", "VHRN_BROKER_ADDR": "127.0.0.1:1", "VHRN_BROKER_TOKEN_FILE": tokenPath}, false, true},
		{"empty path", map[string]string{"VHRN_LOOPBACK_ALLOWLISTS": "a,,c", "VHRN_BROKER_ADDR": "127.0.0.1:1", "VHRN_BROKER_TOKEN_FILE": tokenPath}, false, true},
		{"valid", map[string]string{"VHRN_LOOPBACK_ALLOWLISTS": "a,b,c", "VHRN_BROKER_ADDR": "127.0.0.1:1", "VHRN_BROKER_TOKEN_FILE": tokenPath}, true, false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := localConfigFrom(func(key string) string { return tc.vars[key] })
			if (err == nil) != tc.ok || (got == nil) != tc.nil {
				t.Fatalf("localConfigFrom = %#v, %v", got, err)
			}
			if got != nil && (!reflect.DeepEqual(got.paths, []string{"a", "b", "c"}) || got.broker.Token != validToken) {
				t.Fatalf("config = %#v", got)
			}
		})
	}
	for _, token := range []string{"short", strings.Repeat("A", 64), strings.Repeat("g", 64)} {
		path := filepath.Join(t.TempDir(), "token")
		if err := os.WriteFile(path, []byte(token), 0o600); err != nil {
			t.Fatal(err)
		}
		_, err := localConfigFrom(func(key string) string {
			return map[string]string{"VHRN_LOOPBACK_ALLOWLISTS": "a,b,c", "VHRN_BROKER_ADDR": "127.0.0.1:1", "VHRN_BROKER_TOKEN_FILE": path}[key]
		})
		if err == nil {
			t.Errorf("token %q accepted", token)
		}
	}
}
