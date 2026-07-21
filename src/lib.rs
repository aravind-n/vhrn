//! vhrn runs coding agents ("harnesses") in a container jailed to the current
//! project, with default-deny network egress. This crate is the Rust port of the
//! Go CLI (`cmd/vhrn` + `internal/vhrn`); it is built alongside the Go binary
//! until the port completes. Logic lives here (testable); `src/main.rs` is a thin
//! shim. Comments explain why, not what, and stay terse.

// Subcommand-first help. Bare `vhrn` prints it; a harness runs as `vhrn <harness>
// …`. Transcribed from the Go usageText (internal/vhrn/usage.go).
const USAGE: &str = r#"vhrn runs coding agents in a container jailed to the current project, with
default-deny network egress.

Usage:
  vhrn install <harness>                  build images, seed egress, add a shell alias
  vhrn uninstall <harness>                remove the alias/registry entry (--image drops the image)
  vhrn <harness> [flags] [-- ] [args...]  run a harness in the box
  vhrn list                               show known and installed harnesses
  vhrn net <subcommand>                   manage the egress policy
  vhrn help                               show this help

Harnesses:
  claude                   Claude Code

Run flags (after the harness name, before the agent's own flags):
  --open-net               drop the egress guard for this run (all egress)
  --allow <domain>...      add allowlist domains (comma-separated or repeated)
  --                       stop reading flags; forward the rest to the agent

After `vhrn install claude` a shell alias lets you run `claude` directly; `command claude`
or `\claude` still reaches the real binary. Examples:
  vhrn claude --model opus         # forwards --model opus to claude
  vhrn claude --open-net           # drop the guard for this session
  vhrn claude -- --help            # the agent's own help, not this one

net subcommands:
  net status               current mode and allowlist size
  net allow <domain>...    add domains to the allowlist (effective now)
  net denied               domains blocked this session
  net open                 drop the guard (allow everything)
  net guard                re-enable enforcement
  net report               allow everything, but log what would be denied

Environment:
  VHRN_ENGINE        container engine (default: container, then docker)
  VHRN_IMAGE         box image name (default: per-harness, e.g. vhrn-claude)
  VHRN_PROXY_IMAGE   proxy image name (default: vhrn-proxy)
  VHRN_PROXY_PORT    proxy port (default: 8080)
"#;

/// Entry point: dispatch argv (already stripped of the program name) and return a
/// process exit code, matching the Go `vhrn.Run`.
///
/// Phase 0 scaffold — real subcommand dispatch (help/net/install/uninstall/list/
/// harness) is ported in a later phase. For now every invocation prints the usage
/// text and exits 0; the Go binary remains the behavioural oracle meanwhile.
pub fn run(_args: Vec<String>) -> i32 {
    print!("{USAGE}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    // Smoke test: the scaffold's entry point is callable and returns success.
    #[test]
    fn run_prints_usage_and_succeeds() {
        assert_eq!(run(Vec::new()), 0);
        assert_eq!(run(vec!["help".to_string()]), 0);
    }
}
