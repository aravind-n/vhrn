package vhrn

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

// hostStateDir is the persistent, box-owned store for one harness
// (<cache>/state/<harness>). It is mounted as the box's base config dir and is
// physically separate from the disposable sandbox/, so no config sync can reach
// it — an in-box login and refreshed credentials persist across runs.
func hostStateDir(cache, harness string) string {
	return filepath.Join(cache, "state", harness)
}

// prepareState readies the persistent store before launch and returns its path:
// ensure the dir, bootstrap credentials from the host once, and seed onboarding +
// this project's trust into the config JSON.
func prepareState(home, cache string, h Harness, project string) (string, error) {
	state := hostStateDir(cache, h.Name)
	if err := os.MkdirAll(state, 0o700); err != nil {
		return "", err
	}
	bootstrapCredentials(home, state, h)
	if h.SeedTrust && h.ConfigJSON != "" {
		if err := seedClaudeConfigJSON(filepath.Join(state, h.ConfigJSON), project); err != nil {
			fmt.Fprintf(os.Stderr, "vhrn: warning: could not seed %s: %v\n", h.ConfigJSON, err)
		}
	}
	return state, nil
}

// bootstrapCredentials copies each host credentials file into the store, but only
// when the store's copy is absent. The host seed is bootstrap-only: once the box
// has its own (refreshed) credentials they are authoritative and never clobbered,
// so an in-box login is never overwritten and vhrn needs no host agent install.
func bootstrapCredentials(home, state string, h Harness) {
	for _, rel := range h.Credentials {
		dst := filepath.Join(state, rel)
		if fileExists(dst) {
			continue // box store already populated
		}
		src := filepath.Join(home, h.HostConfig, rel)
		if !fileExists(src) {
			continue // nothing on the host to inherit; the box will prompt to log in
		}
		if err := copyFile(src, dst); err != nil {
			fmt.Fprintf(os.Stderr, "vhrn: warning: could not seed %s: %v\n", rel, err)
			continue
		}
		os.Chmod(dst, 0o600) // credentials stay private
	}
}

// seedClaudeConfigJSON ensures the box-owned config JSON has onboarding completed
// and this project pre-trusted, without disturbing anything the box wrote (login /
// oauthAccount, other projects). Numbers are preserved exactly (UseNumber), and an
// unparseable box-owned file is left untouched rather than clobbered.
func seedClaudeConfigJSON(path, project string) error {
	m := map[string]any{}
	if data, err := os.ReadFile(path); err == nil && len(data) > 0 {
		dec := json.NewDecoder(bytes.NewReader(data))
		dec.UseNumber()
		if err := dec.Decode(&m); err != nil {
			return nil
		}
	}
	if _, ok := m["hasCompletedOnboarding"]; !ok {
		m["hasCompletedOnboarding"] = true
	}
	projects, _ := m["projects"].(map[string]any)
	if projects == nil {
		projects = map[string]any{}
	}
	proj, _ := projects[project].(map[string]any)
	if proj == nil {
		proj = map[string]any{}
	}
	proj["hasTrustDialogAccepted"] = true
	proj["hasCompletedProjectOnboarding"] = true
	projects[project] = proj
	m["projects"] = projects

	data, err := json.MarshalIndent(m, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, append(data, '\n'), 0o600)
}
