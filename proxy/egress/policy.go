package egress

import (
	"fmt"
	"os"
	"strings"
)

type Verdict struct {
	Allow  bool
	Logged bool
	Mode   Mode
}

// Policy reopens its host-owned inputs on every decision. This makes atomic
// replacements live immediately and makes any partial read fail closed.
type Policy struct {
	allowPaths []string
	modePath   string
}

func NewPolicy(allowPath, modePath string) *Policy {
	return NewPolicyPaths([]string{allowPath}, modePath)
}
func NewPolicyPaths(allowPaths []string, modePath string) *Policy {
	return &Policy{allowPaths: append([]string(nil), allowPaths...), modePath: modePath}
}

func (p *Policy) decision() ([]string, Mode, bool) {
	allow := make([]string, 0)
	seen := make(map[string]bool)
	for _, path := range p.allowPaths {
		entries, err := loadAllow(path)
		if err != nil {
			return nil, ModeEnforce, false
		}
		for _, entry := range entries {
			if !seen[entry] {
				seen[entry] = true
				allow = append(allow, entry)
			}
		}
	}
	b, err := os.ReadFile(p.modePath)
	if err != nil {
		return nil, ModeEnforce, false
	}
	return allow, parseMode(string(b)), true
}

func (p *Policy) Mode() Mode { _, mode, _ := p.decision(); return mode }
func (p *Policy) Check(host string) Verdict {
	allow, mode, readable := p.decision()
	if !readable {
		return Verdict{Allow: false, Logged: true, Mode: ModeEnforce}
	}
	matched := hostAllowed(host, allow)
	switch mode {
	case ModeOpen:
		return Verdict{Allow: true, Mode: mode}
	case ModeReport:
		return Verdict{Allow: true, Logged: !matched, Mode: mode}
	default:
		return Verdict{Allow: matched, Logged: !matched, Mode: mode}
	}
}

func loadAllow(path string) ([]string, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var out []string
	for _, line := range strings.Split(string(b), "\n") {
		if strings.TrimSpace(line) == "" {
			continue
		}
		entry, ok := normEntry(line)
		if !ok {
			return nil, fmt.Errorf("invalid allowlist entry %q", line)
		}
		out = append(out, entry)
	}
	return out, nil
}
