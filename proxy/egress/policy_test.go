package egress

import (
	"os"
	"path/filepath"
	"testing"
)

func TestLoadAllow(t *testing.T) {
	p := filepath.Join(t.TempDir(), "allowlist")
	body := "github.com\n\n  api.anthropic.com\n*.example.com\n"
	if err := os.WriteFile(p, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
	got, err := loadAllow(p)
	if err != nil {
		t.Fatal(err)
	}
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
	if _, err := loadAllow("/nonexistent/vhrn/allowlist"); err == nil {
		t.Error("loadAllow(missing) succeeded")
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
		p := NewPolicy(ap, mp) // decisions reopen both host-controlled files
		v := p.Check(c.host)
		if v.Allow != c.allow || v.Logged != c.logged {
			t.Errorf("mode=%s host=%s: {Allow:%v Logged:%v}, want {Allow:%v Logged:%v}",
				c.mode, c.host, v.Allow, v.Logged, c.allow, c.logged)
		}
	}
}

// TestPolicySameMtimeAtomicReplacement proves decisions reopen paths, not mtimes.
func TestPolicySameMtimeAtomicReplacement(t *testing.T) {
	dir := t.TempDir()
	global := filepath.Join(dir, "global")
	project := filepath.Join(dir, "project")
	mp := filepath.Join(dir, "mode")
	if err := os.WriteFile(mp, []byte("enforce"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(global, []byte("github.com\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(project, []byte("project.example\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	p := NewPolicyPaths([]string{global, project}, mp)
	if p.Check("evil.com").Allow {
		t.Fatal("evil.com allowed before it was added")
	}
	replaceSameMtime(t, global, []byte("github.com\nevil.com\n"))
	replaceSameMtime(t, project, []byte("project.example\nlast.example\n"))
	if !p.Check("evil.com").Allow {
		t.Error("evil.com not allowed after reload")
	}
	if !p.Check("last.example").Allow {
		t.Error("project atomic replacement was not reloaded")
	}
}

func replaceSameMtime(t *testing.T, path string, body []byte) {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	original := info.ModTime()
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, body, 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Chtimes(tmp, original, original); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(tmp, path); err != nil {
		t.Fatal(err)
	}
	info, err = os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if !info.ModTime().Equal(original) {
		t.Fatal("replacement mtime changed")
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

func TestPolicyLayeredUnionAndRecovery(t *testing.T) {
	dir := t.TempDir()
	paths := make([]string, 5)
	for i := range paths {
		paths[i] = filepath.Join(dir, "allow"+string(rune('a'+i)))
		if err := os.WriteFile(paths[i], []byte("dup.example\n"), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(paths[4], []byte("dup.example\nlast.example\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	mode := filepath.Join(dir, "mode")
	if err := os.WriteFile(mode, []byte("enforce\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	p := NewPolicyPaths(paths, mode)
	allow, _, ok := p.decision()
	if !ok || len(allow) != 2 || allow[0] != "dup.example" || allow[1] != "last.example" {
		t.Fatalf("union = %#v, readable=%v", allow, ok)
	}
	if !p.Check("last.example").Allow {
		t.Fatal("last layer not read")
	}
	if err := os.Remove(paths[2]); err != nil {
		t.Fatal(err)
	}
	if v := p.Check("last.example"); v.Allow || v.Mode != ModeEnforce {
		t.Fatal("missing layer did not fail closed")
	}
	if err := os.WriteFile(paths[2], []byte("dup.example\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if !p.Check("last.example").Allow {
		t.Fatal("policy did not recover")
	}
	if err := os.WriteFile(paths[1], []byte("https://bad.example\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if p.Check("last.example").Allow {
		t.Fatal("malformed layer did not fail closed")
	}
}

func TestInvalidModeRetainsReadableUnion(t *testing.T) {
	dir := t.TempDir()
	allow := filepath.Join(dir, "allow")
	mode := filepath.Join(dir, "mode")
	if err := os.WriteFile(allow, []byte("github.com\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(mode, []byte("invalid\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	p := NewPolicy(allow, mode)
	if !p.Check("github.com").Allow || p.Check("evil.com").Allow || p.Mode() != ModeEnforce {
		t.Fatal("invalid readable mode was not enforce with union")
	}
}

func TestEveryRequiredLayerFailureFailsClosedAndRecovers(t *testing.T) {
	for _, mode := range []Mode{ModeEnforce, ModeReport, ModeOpen} {
		for _, kind := range []string{"missing", "malformed", "unreadable"} {
			for layer := 0; layer < 5; layer++ {
				t.Run(string(mode)+"/"+kind+"/layer-"+string(rune('0'+layer)), func(t *testing.T) {
					dir := t.TempDir()
					paths := make([]string, 5)
					for i := range paths {
						paths[i] = filepath.Join(dir, string(rune('a'+i)))
						if err := os.WriteFile(paths[i], []byte("allowed.example\n"), 0o644); err != nil {
							t.Fatal(err)
						}
					}
					mp := filepath.Join(dir, "mode")
					if err := os.WriteFile(mp, []byte(mode), 0o644); err != nil {
						t.Fatal(err)
					}
					p := NewPolicyPaths(paths, mp)
					switch kind {
					case "missing":
						if err := os.Remove(paths[layer]); err != nil {
							t.Fatal(err)
						}
					case "malformed":
						if err := os.WriteFile(paths[layer], []byte("https://bad\n"), 0o644); err != nil {
							t.Fatal(err)
						}
					case "unreadable":
						if err := os.Remove(paths[layer]); err != nil {
							t.Fatal(err)
						}
						if err := os.Mkdir(paths[layer], 0o755); err != nil {
							t.Fatal(err)
						}
					}
					v := p.Check("allowed.example")
					if v.Allow || !v.Logged || v.Mode != ModeEnforce || p.Mode() != ModeEnforce {
						t.Fatalf("failure did not fail closed: %#v", v)
					}
					if err := os.RemoveAll(paths[layer]); err != nil {
						t.Fatal(err)
					}
					if err := os.WriteFile(paths[layer], []byte("allowed.example\n"), 0o644); err != nil {
						t.Fatal(err)
					}
					if !p.Check("allowed.example").Allow {
						t.Fatal("did not recover")
					}
				})
			}
		}
	}
}

func TestMissingModeFailsClosedAndRecovers(t *testing.T) {
	dir := t.TempDir()
	allow := filepath.Join(dir, "allow")
	mode := filepath.Join(dir, "mode")
	if err := os.WriteFile(allow, []byte("allowed.example\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	p := NewPolicy(allow, mode)
	v := p.Check("allowed.example")
	if v.Allow || !v.Logged || v.Mode != ModeEnforce {
		t.Fatalf("missing mode = %#v", v)
	}
	if err := os.WriteFile(mode, []byte("open\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if !p.Check("allowed.example").Allow {
		t.Fatal("mode restoration did not recover")
	}
}

func TestModeFileFailuresFailClosedAndRecover(t *testing.T) {
	for _, stored := range []Mode{ModeEnforce, ModeReport, ModeOpen} {
		for _, kind := range []string{"missing", "unreadable"} {
			t.Run(string(stored)+"/"+kind, func(t *testing.T) {
				dir := t.TempDir()
				allow := filepath.Join(dir, "allow")
				mode := filepath.Join(dir, "mode")
				if err := os.WriteFile(allow, []byte("allowed.example\n"), 0o644); err != nil {
					t.Fatal(err)
				}
				if kind == "unreadable" {
					if err := os.Mkdir(mode, 0o755); err != nil {
						t.Fatal(err)
					}
				}
				p := NewPolicy(allow, mode)
				v := p.Check("allowed.example")
				if v.Allow || !v.Logged || v.Mode != ModeEnforce || p.Mode() != ModeEnforce {
					t.Fatalf("failure = %#v", v)
				}
				if err := os.RemoveAll(mode); err != nil {
					t.Fatal(err)
				}
				if err := os.WriteFile(mode, []byte(stored), 0o644); err != nil {
					t.Fatal(err)
				}
				if !p.Check("allowed.example").Allow {
					t.Fatal("did not recover")
				}
				if p.Mode() != stored {
					t.Fatalf("mode = %s, want %s", p.Mode(), stored)
				}
				v = p.Check("other.example")
				wantAllow, wantLogged := stored != ModeEnforce, stored != ModeOpen
				if v.Allow != wantAllow || v.Logged != wantLogged || v.Mode != stored {
					t.Fatalf("recovered unmatched = %#v", v)
				}
			})
		}
	}
}

func TestMultiLineModeIsInvalid(t *testing.T) {
	dir := t.TempDir()
	allow := filepath.Join(dir, "allow")
	mode := filepath.Join(dir, "mode")
	if err := os.WriteFile(allow, []byte("allowed.example\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(mode, []byte("open\ngarbage\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	p := NewPolicy(allow, mode)
	if !p.Check("allowed.example").Allow || p.Check("other.example").Allow {
		t.Fatal("multi-line mode did not retain union under enforce")
	}
}

func TestEmptyPluralPathFailsClosedEndToEnd(t *testing.T) {
	dir := t.TempDir()
	a := filepath.Join(dir, "a")
	b := filepath.Join(dir, "b")
	mode := filepath.Join(dir, "mode")
	for _, path := range []string{a, b} {
		if err := os.WriteFile(path, []byte("allowed.example\n"), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(mode, []byte("open\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	p := NewPolicyPaths([]string{a, "", b}, mode)
	v := p.Check("allowed.example")
	if v.Allow || !v.Logged || v.Mode != ModeEnforce {
		t.Fatalf("empty layer = %#v", v)
	}
}
