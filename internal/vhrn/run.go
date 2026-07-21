package vhrn

import (
	"fmt"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"regexp"
)

// boxHome is the unprivileged box user's home; all container-side paths hang off it.
const boxHome = "/home/dev"

// boxConfig is the resolved host-side state for one run: paths, engine/image, and
// the extra --volume/--env args assembled during preparation.
type boxConfig struct {
	engine  string
	harness Harness
	image   string // resolved box image ref (registry ref, or bare local name)
	version string // installed image version (a tag, or "local")
	project string // physical cwd (pwd -P)
	key     string // history key: [^A-Za-z0-9] -> '-'
	cache   string // ~/.cache/vhrn

	state      string // <cache>/state/<harness> -> the box's persistent config dir
	sandbox    string // <cache>/sandbox         -> disposable synced config
	configDir  string // box config dir, e.g. /home/dev/.claude
	hostConfig string // host config dir, e.g. ~/.claude (sync source + guide + history)
	history    string // <hostConfig>/projects/<key>

	config Config // merged defaults + global + project config

	gitMount []string
	ghEnv    []string
	termEnv  []string
}

var nonAlnum = regexp.MustCompile(`[^A-Za-z0-9]`)

// historyKey reproduces Claude's projects/<key> encoding so in-box history
// unifies with native history (sed 's/[^A-Za-z0-9]/-/g').
func historyKey(project string) string {
	return nonAlnum.ReplaceAllString(project, "-")
}

// prepareBox performs all host-side preparation: resolve paths and engine, ready
// the persistent state store, sync the disposable config copy, and assemble the
// gitconfig/gh/terminal run args.
func prepareBox(h Harness) (*boxConfig, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return nil, err
	}
	wd, err := os.Getwd()
	if err != nil {
		return nil, err
	}
	project, err := filepath.EvalSymlinks(wd) // pwd -P: physical path
	if err != nil {
		return nil, err
	}
	engine, err := detectEngine()
	if err != nil {
		return nil, err
	}

	// Config is resolved first: a blocked cwd must abort before any host-side work.
	conf, err := loadConfig(home, project)
	if err != nil {
		return nil, err
	}
	if err := checkBlockedDir(project, home, conf.Run.BlockedDirs); err != nil {
		return nil, err
	}

	// Resolve the box image from the installed registry — the harness must be
	// installed (which pulled and recorded its version). VHRN_IMAGE overrides it.
	version, installed := installedVersion(home, h.Name)
	imgOverride := os.Getenv("VHRN_IMAGE")
	if !installed && imgOverride == "" {
		return nil, fmt.Errorf("%s is not installed — run `vhrn install %s`", h.Name, h.Name)
	}
	if !installed {
		version = localVersion // VHRN_IMAGE given without a record; pin the proxy locally
	}
	image := imgOverride
	if image == "" {
		image = harnessImageRef(h, version)
	}

	cache := vhrnCache(home)
	cfg := &boxConfig{
		engine:     engine,
		harness:    h,
		image:      image,
		version:    version,
		project:    project,
		key:        historyKey(project),
		cache:      cache,
		sandbox:    filepath.Join(cache, "sandbox"),
		configDir:  path.Join(boxHome, h.StateDir),
		hostConfig: filepath.Join(home, h.HostConfig),
		config:     conf,
	}
	cfg.history = filepath.Join(cfg.hostConfig, "projects", cfg.key)

	// The persistent, box-owned store — login/credentials/onboarding live here and
	// survive across runs. It is mounted as the box's base config dir.
	state, err := prepareState(home, cache, h, project)
	if err != nil {
		return nil, err
	}
	cfg.state = state

	if err := os.MkdirAll(cfg.sandbox, 0o755); err != nil {
		return nil, err
	}
	if err := os.MkdirAll(cfg.history, 0o755); err != nil {
		return nil, err
	}

	// Disposable config synced from the host, dereferencing symlinks so symlinked
	// skills come across. This layers on top of the state mount, never into it.
	for _, d := range h.SyncDirs {
		syncClaudeSubdir(cfg.hostConfig, cfg.sandbox, d)
	}
	for _, fn := range h.SyncFiles {
		copyFileInto(cfg.hostConfig, cfg.sandbox, fn)
	}

	cfg.gitMount = gitConfigMount(home, cache)
	cfg.ghEnv = ghTokenEnv()
	cfg.termEnv = terminalEnv()
	return cfg, nil
}

func runHarness(h Harness, f runFlags) error {
	cfg, err := prepareBox(h)
	if err != nil {
		return err
	}
	return startBox(cfg, f)
}

// nestedMounts layers the disposable synced config, the box guide, and the shared
// history dir on top of the persistent state mount as nested bind mounts. Each is
// guarded on source existence so we never bind a missing path or turn a file mount
// into a stray directory.
func (cfg *boxConfig) nestedMounts() []string {
	var m []string
	add := func(src, dst string) { m = append(m, "--volume", src+":"+dst) }
	for _, d := range cfg.harness.SyncDirs {
		if src := filepath.Join(cfg.sandbox, d); dirExists(src) {
			add(src, path.Join(cfg.configDir, d))
		}
	}
	for _, fn := range cfg.harness.SyncFiles {
		if src := filepath.Join(cfg.sandbox, fn); fileExists(src) {
			add(src, path.Join(cfg.configDir, fn))
		}
	}
	if src := filepath.Join(cfg.sandbox, "CLAUDE.md"); fileExists(src) {
		add(src, path.Join(cfg.configDir, "CLAUDE.md"))
	}
	add(cfg.history, path.Join(cfg.configDir, "projects", cfg.key))
	return m
}

// startBox seeds the egress policy, starts the proxy sidecar, then runs the jailed
// box with all egress pinned to the proxy. The box run inherits the terminal so the
// agent is interactive; its exit status is returned verbatim.
func startBox(cfg *boxConfig, f runFlags) error {
	np := newNetPolicy(cfg.cache)
	port := envOr("VHRN_PROXY_PORT", "8080")
	mode := resolveMode(cfg.config.Net.Mode, f.openNet)
	if !f.openNet && cfg.config.Net.Mode != "" && cfg.config.Net.Mode != mode {
		fmt.Fprintf(os.Stderr, "vhrn: warning: invalid net mode %q; using %s\n", cfg.config.Net.Mode, mode)
	}

	if err := np.ensure(); err != nil {
		return err
	}
	np.seedAllowlistIfAbsent()
	np.appendMissing(cfg.config.Net.Allow) // config-declared allow domains
	np.appendMissing(f.extraAllow)         // session --allow additions persist, like `net allow`
	np.writeMode(mode)
	np.truncateDenyLog()

	if err := writeBoxGuide(cfg.hostConfig, cfg.sandbox, mode == "open"); err != nil {
		fmt.Fprintf(os.Stderr, "vhrn: warning: could not write box CLAUDE.md: %v\n", err)
	}

	// Apple container needs its system service up; Docker manages its own daemon.
	if cfg.engine == "container" {
		exec.Command("container", "system", "start").Run()
	}

	// A declared toolchain resolves to a derived, content-addressed image (FROM the
	// harness image + mise-provisioned tools), built once and reused thereafter.
	if tools := cfg.config.Toolchains.Tools; len(tools) > 0 {
		img, err := ensureToolchainImage(cfg.engine, cfg.image, cfg.harness.Image, tools)
		if err != nil {
			return fmt.Errorf("provisioning toolchain: %w", err)
		}
		cfg.image = img
	}

	proxyImage := envOr("VHRN_PROXY_IMAGE", proxyImageRef(cfg.version))
	p, ip, err := startProxy(cfg.engine, proxyImage, np, port)
	if err != nil {
		return err
	}
	defer p.stop()
	stopOnSignal(p)

	proxyURL := fmt.Sprintf("http://%s:%s", ip, port)
	if mode == "open" {
		fmt.Fprintln(os.Stderr, "vhrn: network guard OFF (open) — all public egress allowed this session.")
		if len(cfg.ghEnv) > 0 {
			fmt.Fprintln(os.Stderr, "vhrn: a GitHub token is present in the box with the guard off.")
		}
	}

	// NET_ADMIN lets the entrypoint install the egress firewall (dropped before dev runs).
	args := []string{
		"run", "-it", "--rm",
		"--cap-add", "CAP_NET_ADMIN",
		"--env", "VHRN_SANDBOX=1",
		"--env", "VHRN_NET=" + mode,
		"--env", "VHRN_PROXY_IP=" + ip,
		"--env", "VHRN_PROXY_PORT=" + port,
		"--env", "HTTP_PROXY=" + proxyURL,
		"--env", "HTTPS_PROXY=" + proxyURL,
		"--env", "http_proxy=" + proxyURL,
		"--env", "https_proxy=" + proxyURL,
		"--volume", cfg.project + ":" + cfg.project,
		"--workdir", cfg.project,
	}
	// Point the agent at its config dir, then mount the persistent state there so
	// login/credentials/onboarding persist and Claude's temp+rename saves land in a
	// real directory mount (rename-safe).
	if cfg.harness.ConfigDirEnv != "" {
		args = append(args, "--env", cfg.harness.ConfigDirEnv+"="+cfg.configDir)
	}
	args = append(args, "--volume", cfg.state+":"+cfg.configDir)
	// Disposable synced config + history layer on top as nested mounts, so config
	// never pollutes the state store.
	args = append(args, cfg.nestedMounts()...)

	args = append(args, cfg.gitMount...)
	args = append(args, cfg.termEnv...)
	args = append(args, cfg.ghEnv...)
	args = append(args, cfg.image, cfg.harness.Command)
	args = append(args, f.rest...)

	cmd := exec.Command(cfg.engine, args...)
	cmd.Stdin = os.Stdin
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

func envOr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

// vhrnCache is the XDG cache root for vhrn (${XDG_CACHE_HOME:-~/.cache}/vhrn).
func vhrnCache(home string) string {
	cacheHome := os.Getenv("XDG_CACHE_HOME")
	if cacheHome == "" {
		cacheHome = filepath.Join(home, ".cache")
	}
	return filepath.Join(cacheHome, "vhrn")
}
