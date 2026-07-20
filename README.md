# claude-box

Run Claude Code inside a container jailed to the current project directory, with
**default-deny network egress**. The host filesystem is protected by absence
(only the project is mounted; `~/.ssh`, other projects, etc. never enter the
box), and outbound network is restricted to an allowlist — so you can run
`--dangerously-skip-permissions` without a prompt injection being able to reach
the rest of your machine or exfiltrate the project to an arbitrary host.

The current project is bind-mounted at its real path; a sandbox copy of
`~/.claude` is synced in on every run; session history is written straight back
to your host's `~/.claude/projects/<key>`, so in-box and native history stay
unified.

## Requirements

- [Apple Container](https://github.com/apple/container) or Docker (auto-detected, `container` first)
- Claude Code, plus `gh` on the host if you want GitHub auth forwarded

## Install

```sh
make            # build the box image and the egress proxy image
make install    # install the wrapper to /usr/local/bin (needs sudo)
```

## Usage

Run in any project directory. Arguments after the wrapper's own flags are passed
straight through to `claude`:

```sh
claude-box                      # guarded: egress limited to the allowlist
claude-box --model opus         # forwards --model opus to claude
claude-box --allow docs.rs      # add domains to the allowlist for this session
claude-box --open-net           # drop the guard for this session (all egress)
```

## Network egress guard

Every run starts a small proxy sidecar. The box's firewall pins **all** egress to
that proxy, which permits only allowlisted domains; everything else — including
direct DNS — is refused. Blocked requests fail with the domain named, e.g.
`blocked by claude-box egress policy: example.com`.

The policy lives on the host (`~/.cache/claude-box/net/`) and is mounted only
into the proxy, never the box — so an in-box process, even under
skip-permissions, cannot widen its own egress. Change it from the host while a
box is running; the proxy picks up edits on the next request, no restart:

```sh
claude-box net status                 # current mode + allowlist size
claude-box net allow docs.rs api.x.io # add domains (takes effect immediately)
claude-box net denied                 # domains blocked this session
claude-box net open                   # drop the guard (allow all)
claude-box net guard                  # re-enable enforcement
```

The default allowlist covers the Anthropic API, GitHub, and the common package
registries; edit `~/.cache/claude-box/net/allowlist` to change the defaults.

### Statusline indicator (optional)

To surface the guard state in your statusline, add to `~/.claude/statusline.sh`:

```sh
if [ -n "${CLAUDE_SANDBOX:-}" ]; then          # only inside a box
  mode="${CLAUDE_BOX_NET:-enforce}"            # launch state, cheap fallback
  if [ -n "${CLAUDE_BOX_PROXY_IP:-}" ]; then   # live state from the proxy
    live=$(curl -s --max-time 1 --noproxy '*' \
      "http://$CLAUDE_BOX_PROXY_IP:${CLAUDE_BOX_PROXY_PORT:-8080}/__status" \
      | sed -n 's/.*"mode":"\([a-z]*\)".*/\1/p')
    [ -n "$live" ] && mode="$live"
  fi
  [ "$mode" = enforce ] && printf '🔒 net:guard' || printf '⚠ net:%s' "$mode"
fi
```

## Make targets

| Target | Description |
| --- | --- |
| `make` / `make build` | Build both images (box + proxy) |
| `make build-box` / `make build-proxy` | Build one image |
| `make rebuild` | Rebuild both with no cache |
| `make clean` | Remove both images |
| `make install` | Install the wrapper to `/usr/local/bin` (needs sudo) |
| `make uninstall` | Remove the installed wrapper |
| `make ENGINE=docker …` | Force Docker instead of `container` |

## Threat model

What claude-box protects:

- **Your host.** Secrets and other projects are never mounted, so the box cannot
  read or damage them regardless of what runs inside.
- **Against casual exfiltration.** Default-deny egress means a prompt injection
  cannot POST your source to an arbitrary server; it can only reach allowlisted
  domains.

What it does **not** protect against:

- **Exfiltration to an allowlisted host.** The proxy filters by hostname without
  terminating TLS, so it cannot stop data being pushed to a domain you allow
  (e.g. a repo on `github.com`) or domain-fronted behind an allowed CDN.
- **`--open-net` sessions**, which disable the guard entirely.
- Under **Docker**, the box shares the host kernel (a container escape reaches
  the host); Apple `container` runs each box in a lightweight VM, a stronger
  boundary.

## Notes

- **No sudo inside the box.** Removing it is what makes the egress firewall
  tamper-proof. Install tools in user space instead: `mise use -g <tool>` for
  runtimes, `uv tool install <pkg>` for Python CLIs.
- `gh` auth is forwarded as an env token (`$GH_TOKEN`/`$GITHUB_TOKEN`, else
  `gh auth token`), so git-over-HTTPS works inside the box. SSH remotes stay
  unauthenticated. With `--open-net`, the wrapper warns that a token is aboard.
- Edits to the sandbox copy under `~/.cache/claude-box/` are wiped each run. To
  change your Claude settings (skills, `settings.json`, …), edit your real
  `~/.claude` on the host; if you need Claude to edit them, use native Claude
  Code rather than the box.
- Your host `~/.gitconfig` is copied in, so in-box commits use your name/email
  (change the host file to persist).
