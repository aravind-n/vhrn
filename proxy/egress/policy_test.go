package egress

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestLoadAllow(t *testing.T) {
	p := filepath.Join(t.TempDir(), "allowlist")
	body := "# a comment\ngithub.com\n\n  api.anthropic.com  # trailing comment\n*.example.com\n"
	if err := os.WriteFile(p, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
	got := loadAllow(p)
	want := []string{"github.com", "api.anthropic.com", "example.com"}
	if len(got) != len(want) {
		t.Fatalf("loadAllow = %v, want %v", got, want)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Errorf("loadAllow[%d] = %q, want %q", i, got[i], want[i])
		}
	}
}

func TestLoadAllowMissingFile(t *testing.T) {
	if got := loadAllow("/nonexistent/vhrn/allowlist"); got != nil {
		t.Errorf("loadAllow(missing) = %v, want nil (fail closed)", got)
	}
}

// TestPolicyCheck exercises the full mode x match matrix through Check.
func TestPolicyCheck(t *testing.T) {
	dir := t.TempDir()
	ap := filepath.Join(dir, "allow")
	if err := os.WriteFile(ap, []byte("github.com\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	cases := []struct {
		mode   Mode
		host   string
		allow  bool
		logged bool
	}{
		{ModeEnforce, "github.com", true, false},
		{ModeEnforce, "api.github.com", true, false},
		{ModeEnforce, "evil.com", false, true},
		{ModeOpen, "evil.com", true, false},
		{ModeReport, "github.com", true, false},
		{ModeReport, "evil.com", true, true}, // allowed but flagged for logging
	}
	for _, c := range cases {
		mp := filepath.Join(dir, "mode")
		if err := os.WriteFile(mp, []byte(string(c.mode)), 0o644); err != nil {
			t.Fatal(err)
		}
		p := NewPolicy(ap, mp) // fresh policy: first Check reloads both files
		v := p.Check(c.host)
		if v.Allow != c.allow || v.Logged != c.logged {
			t.Errorf("mode=%s host=%s: {Allow:%v Logged:%v}, want {Allow:%v Logged:%v}",
				c.mode, c.host, v.Allow, v.Logged, c.allow, c.logged)
		}
	}
}

// TestPolicyHotReload confirms an allowlist edit is picked up without a restart
// once its mtime advances, the property `vhrn net allow` relies on.
func TestPolicyHotReload(t *testing.T) {
	dir := t.TempDir()
	ap := filepath.Join(dir, "allow")
	mp := filepath.Join(dir, "mode")
	if err := os.WriteFile(mp, []byte("enforce"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(ap, []byte("github.com\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	p := NewPolicy(ap, mp)
	if p.Check("evil.com").Allow {
		t.Fatal("evil.com allowed before it was added")
	}
	if err := os.WriteFile(ap, []byte("github.com\nevil.com\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	future := time.Now().Add(time.Second)
	if err := os.Chtimes(ap, future, future); err != nil {
		t.Fatal(err)
	}
	if !p.Check("evil.com").Allow {
		t.Error("evil.com not allowed after reload")
	}
}

func TestPolicyMissingFilesFailClosed(t *testing.T) {
	p := NewPolicy("/nonexistent/allow", "/nonexistent/mode")
	if m := p.Mode(); m != ModeEnforce {
		t.Errorf("mode with no files = %s, want enforce", m)
	}
	if p.Check("github.com").Allow {
		t.Error("host allowed with no allowlist; want deny (fail closed)")
	}
}
