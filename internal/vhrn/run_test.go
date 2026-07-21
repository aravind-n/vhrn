package vhrn

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestHistoryKey(t *testing.T) {
	cases := map[string]string{
		"/Users/aravind/projects/vhrn": "-Users-aravind-projects-vhrn",
		"/a/b_c.d":                     "-a-b-c-d",
		"/x/y-z":                       "-x-y-z",
	}
	for in, want := range cases {
		if got := historyKey(in); got != want {
			t.Errorf("historyKey(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestNestedMountsGuardOnExistence(t *testing.T) {
	sandbox := t.TempDir()
	os.MkdirAll(filepath.Join(sandbox, "skills"), 0o755)                       // present dir
	os.WriteFile(filepath.Join(sandbox, "settings.json"), []byte("{}"), 0o644) // present file
	os.WriteFile(filepath.Join(sandbox, "CLAUDE.md"), []byte("guide"), 0o644)  // guide
	// commands/agents dirs and statusline.sh are intentionally absent.

	cfg := &boxConfig{
		harness:   Harness{SyncDirs: []string{"skills", "commands", "agents"}, SyncFiles: []string{"settings.json", "statusline.sh"}},
		sandbox:   sandbox,
		configDir: "/home/dev/.claude",
		history:   "/host/history",
		key:       "-proj",
	}
	got := cfg.nestedMounts()
	if len(got)%2 != 0 {
		t.Fatalf("mount args must pair --volume with a value: %v", got)
	}
	joined := strings.Join(got, " ")

	for _, want := range []string{
		filepath.Join(sandbox, "skills") + ":/home/dev/.claude/skills",
		filepath.Join(sandbox, "settings.json") + ":/home/dev/.claude/settings.json",
		filepath.Join(sandbox, "CLAUDE.md") + ":/home/dev/.claude/CLAUDE.md",
		"/host/history:/home/dev/.claude/projects/-proj",
	} {
		if !strings.Contains(joined, want) {
			t.Errorf("missing mount %q in %v", want, got)
		}
	}
	for _, absent := range []string{"commands", "agents", "statusline.sh"} {
		if strings.Contains(joined, "/home/dev/.claude/"+absent) {
			t.Errorf("mounted absent source %q: %v", absent, got)
		}
	}
}
