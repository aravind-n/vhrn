# berm

Run Claude Code inside a container jailed to the current project directory, with
**default-deny network egress**. Only the current project is mounted into the box,
so `~/.ssh`, your other projects, and the rest of your home directory stay outside
it. Outbound traffic is limited to an allowlist. Between them, those two things let
you run `--dangerously-skip-permissions` without a prompt injection being able to
reach the rest of your machine or push your project somewhere it shouldn't go.

The project is bind-mounted at its real path. A sandbox copy of `~/.claude` is
synced in on each run, and session history is written back to
`~/.claude/projects/<key>` on the host, so in-box and native sessions share the
same history.

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
berm                      # guarded: egress limited to the allowlist
berm --model opus         # forwards --model opus to claude
berm --allow docs.rs      # add domains to the allowlist for this session
berm --open-net           # drop the guard for this session (all egress)
```

## Network egress guard

Every run starts a small proxy sidecar. The box's firewall routes every outbound
connection through that proxy, and the proxy only allows allowlisted domains.
Everything else, including direct DNS, is refused. A blocked request fails with the
domain named, like `blocked by berm egress policy: example.com`.

The policy lives on the host, under `~/.cache/berm/net/`, and is mounted into
the proxy but never into the box. That is what stops an in-box process from
widening its own egress, even under skip-permissions. Edit it from the host while a
box is running and the proxy picks up the change on its next request, no restart
needed:

```sh
berm net status                 # current mode + allowlist size
berm net allow docs.rs api.x.io # add domains (takes effect immediately)
berm net denied                 # domains blocked this session
berm net open                   # drop the guard (allow all)
berm net guard                  # re-enable enforcement
```

The default allowlist covers the Anthropic API, GitHub, and the common package
registries. Edit `~/.cache/berm/net/allowlist` to change the defaults.

### Statusline indicator (optional)

To surface the guard state in your statusline, add to `~/.claude/statusline.sh`:

```sh
if [ -n "${BERM_SANDBOX:-}" ]; then          # only inside a box
  mode="${BERM_NET:-enforce}"            # launch state, cheap fallback
  if [ -n "${BERM_PROXY_IP:-}" ]; then   # live state from the proxy
    live=$(curl -s --max-time 1 --noproxy '*' \
      "http://$BERM_PROXY_IP:${BERM_PROXY_PORT:-8080}/__status" \
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
| `make ENGINE=docker ...` | Force Docker instead of `container` |

## Threat model

What it protects:

- Your host filesystem. Secrets and your other projects are never mounted, so
  nothing inside the box can read or damage them.
- Against casual exfiltration. Default-deny egress stops a prompt injection from
  POSTing your source to an outside server; it can only reach the domains you have
  allowed.

What it doesn't:

- Exfiltration to a domain you have already allowed. The proxy matches on hostname
  and doesn't terminate TLS, so it can't stop data being pushed to an allowed
  domain (a repo on `github.com`, for instance) or domain-fronted behind an allowed
  CDN.
- Sessions run with `--open-net`, which turn the guard off entirely.
- A container escape under Docker, where the box shares the host's kernel. Apple
  `container` puts each box in its own lightweight VM, a stronger boundary.

## Notes

- There is no sudo inside the box; removing it is what keeps the egress firewall
  tamper-proof. Install tools in user space instead: `mise use -g <tool>` for
  runtimes, `uv tool install <pkg>` for Python CLIs.
- `gh` auth is forwarded as an env token (`$GH_TOKEN` or `$GITHUB_TOKEN`, else
  `gh auth token`), which covers git-over-HTTPS inside the box. SSH remotes stay
  unauthenticated. Under `--open-net`, the wrapper warns that a token is present.
- The sandbox copy under `~/.cache/berm/` is re-synced every run, so edits to
  it don't survive. Change your real `~/.claude` on the host instead (skills,
  `settings.json`, and the rest). If you need Claude itself to edit them, use native
  Claude Code rather than the box.
- Your host `~/.gitconfig` is copied in so in-box commits use your name and email.
  Change the host file if you want a change to stick.
