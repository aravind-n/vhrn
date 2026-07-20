# claude-box

Run Claude Code inside a container jailed to the current project directory. This
ensures claude can't break out of the project directory allowing you to freely
run features like `--dangerously-skip-permissions`

The current project is bind-mounted at its real path; a sandbox copy of `~/.claude` is synced in on every run; session history is written straight back to your host's `~/.claude/projects/<key>`, so in-box and native history stay unified.

## Requirements

- [Apple Container](https://github.com/apple/container) or Docker (auto-detected, `container` first)
- Claude Code, plus `gh` on the host if you want GitHub auth forwarded

## Install

```sh
make            # build the claude-sandbox image
./install.sh    # install the wrapper to /usr/local/bin (needs sudo)
```

## Usage

Run in any project directory. Arguments are passed straight through to `claude`:

```sh
claude-box
claude-box --model opus
```

## Make targets

| Target | Description |
| --- | --- |
| `make` / `make build` | Build the image (default) |
| `make rebuild` | Rebuild with no cache |
| `make clean` | Remove the image |
| `make ENGINE=docker …` | Force Docker instead of `container` |

## Notes

- `gh` auth is forwarded as an env token (`$GH_TOKEN`/`$GITHUB_TOKEN`, else `gh auth token`), so git-over-HTTPS works inside the box. SSH remotes stay unauthenticated.
- Edits to the sandbox copy under `~/.cache/claude-box/` are wiped each run. If you want to edit your claude settings (add skills, change settings.json, etc), either to it manually in your `~/.claude` directory.
  - If you need claude to edit it, use the native claude-code instead of the sandbox
- Your host `~/.gitconfig` is copied in, so in-box commits use your name/email (change the host file to persist).
