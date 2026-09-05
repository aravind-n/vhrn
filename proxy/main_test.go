package main

import (
	"os"
	"path/filepath"
	"reflect"
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
