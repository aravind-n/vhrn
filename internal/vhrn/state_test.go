package vhrn

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestBootstrapCredentialsIsSeedOnly(t *testing.T) {
	home := t.TempDir()
	state := t.TempDir()
	h := Harness{Name: "claude", HostConfig: ".claude", Credentials: []string{".credentials.json"}}

	// No host creds: nothing seeded.
	bootstrapCredentials(home, state, h)
	if fileExists(filepath.Join(state, ".credentials.json")) {
		t.Fatal("seeded creds without a host source")
	}

	// Host login present + empty store: inherited.
	os.MkdirAll(filepath.Join(home, ".claude"), 0o700)
	os.WriteFile(filepath.Join(home, ".claude", ".credentials.json"), []byte("HOST"), 0o600)
	bootstrapCredentials(home, state, h)
	if got, _ := os.ReadFile(filepath.Join(state, ".credentials.json")); string(got) != "HOST" {
		t.Fatalf("bootstrap creds = %q, want HOST", got)
	}

	// Box has since logged in (refreshed creds): the host seed must not clobber it.
	os.WriteFile(filepath.Join(state, ".credentials.json"), []byte("BOX"), 0o600)
	bootstrapCredentials(home, state, h)
	if got, _ := os.ReadFile(filepath.Join(state, ".credentials.json")); string(got) != "BOX" {
		t.Errorf("box creds overwritten by host seed: got %q, want BOX", got)
	}
}

func TestSeedClaudeConfigJSONPreservesLogin(t *testing.T) {
	path := filepath.Join(t.TempDir(), ".claude.json")
	// A box-owned file: logged in, another project trusted, a large integer key.
	os.WriteFile(path, []byte(`{"hasCompletedOnboarding":false,"oauthAccount":{"emailAddress":"a@b.c"},"numberOfStartups":1784592922215,"projects":{"/other":{"hasTrustDialogAccepted":true}}}`), 0o600)

	if err := seedClaudeConfigJSON(path, "/proj"); err != nil {
		t.Fatal(err)
	}
	raw, _ := os.ReadFile(path)
	// Big integers survive without float mangling.
	if !strings.Contains(string(raw), "1784592922215") {
		t.Errorf("large number not preserved verbatim:\n%s", raw)
	}
	var m map[string]any
	if err := json.Unmarshal(raw, &m); err != nil {
		t.Fatal(err)
	}
	if _, ok := m["oauthAccount"]; !ok {
		t.Error("oauthAccount (login) dropped")
	}
	if m["hasCompletedOnboarding"] != false {
		t.Error("existing hasCompletedOnboarding was overwritten")
	}
	projects := m["projects"].(map[string]any)
	if _, ok := projects["/other"]; !ok {
		t.Error("existing project trust dropped")
	}
	proj := projects["/proj"].(map[string]any)
	if proj["hasTrustDialogAccepted"] != true || proj["hasCompletedProjectOnboarding"] != true {
		t.Errorf("current project not pre-trusted: %v", proj)
	}
}

func TestSeedClaudeConfigJSONFresh(t *testing.T) {
	path := filepath.Join(t.TempDir(), ".claude.json")
	if err := seedClaudeConfigJSON(path, "/proj"); err != nil {
		t.Fatal(err)
	}
	var m map[string]any
	raw, _ := os.ReadFile(path)
	json.Unmarshal(raw, &m)
	if m["hasCompletedOnboarding"] != true {
		t.Error("fresh config should complete onboarding")
	}
	proj := m["projects"].(map[string]any)["/proj"].(map[string]any)
	if proj["hasTrustDialogAccepted"] != true {
		t.Error("fresh config should pre-trust the project")
	}
}
