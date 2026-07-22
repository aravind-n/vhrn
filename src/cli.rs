//! Subcommand dispatch, run-flag parsing, and the usage text — the CLI's front
//! door (mirrors the Go cli.go + flags.go + usage.go). Full dispatch lands in a
//! later port phase; for now `run` prints the usage text.

use anyhow::{Result, bail};

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
/// Port in progress — `help`/`net`/`list` are wired now; `install`/`uninstall` and
/// running a harness land at the cutover phase and until then fall through to usage.
pub fn run(args: Vec<String>) -> i32 {
    match args.first().map(String::as_str) {
        None | Some("help" | "-h" | "--help") => {
            print!("{USAGE}");
            0
        }
        Some("net") => crate::net::run_net(&args[1..]),
        Some("list") => run_list(&args[1..]),
        _ => {
            // install/uninstall/harness-run dispatch arrives at the cutover phase.
            print!("{USAGE}");
            0
        }
    }
}

/// Show every known harness and whether `vhrn install` has set it up (list.go).
fn run_list(_args: &[String]) -> i32 {
    let home = match crate::run::home_dir() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("vhrn: {e}");
            return 1;
        }
    };
    let config_dir = crate::shell::vhrn_config_dir(&home);
    let installed: std::collections::HashMap<String, String> =
        crate::shell::read_installed(&config_dir).into_iter().map(|ih| (ih.name, ih.version)).collect();
    for name in crate::harness::harness_names() {
        match installed.get(&name) {
            Some(v) => println!("  {name:<12} installed ({v})"),
            None => println!("  {name:<12} available"),
        }
    }
    0
}

/// The wrapper-owned flags consumed before the agent's own args.
#[derive(Debug, Default, PartialEq)]
struct RunFlags {
    open_net: bool,           // --open-net: drop the egress guard this run
    extra_allow: Vec<String>, // --allow: session additions to the allowlist
    rest: Vec<String>,        // everything forwarded to the agent verbatim
}

/// Consume wrapper flags up front then forward the rest verbatim, mirroring
/// vhrn.sh's loop: `--open-net` / `--allow[=]<d,d>` are read, `--` stops flag
/// reading, and the first unrecognized token ends parsing (so agent flags pass
/// through untouched).
fn parse_run_flags(args: &[String]) -> Result<RunFlags> {
    let mut f = RunFlags::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--open-net" {
            f.open_net = true;
            i += 1;
        } else if a == "--allow" {
            i += 1;
            let Some(v) = args.get(i) else {
                bail!("--allow needs a domain");
            };
            f.extra_allow.extend(split_domains(v));
            i += 1;
        } else if let Some(v) = a.strip_prefix("--allow=") {
            f.extra_allow.extend(split_domains(v));
            i += 1;
        } else if a == "--" {
            f.rest.extend_from_slice(&args[i + 1..]);
            return Ok(f);
        } else {
            f.rest.extend_from_slice(&args[i..]);
            return Ok(f);
        }
    }
    Ok(f)
}

/// Split a comma-separated `--allow` value, dropping empty fields.
fn split_domains(s: &str) -> Vec<String> {
    s.split(',').filter(|p| !p.is_empty()).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // Smoke test: the entry point is callable and returns success.
    #[test]
    fn run_prints_usage_and_succeeds() {
        assert_eq!(run(Vec::new()), 0);
        assert_eq!(run(vec!["help".to_string()]), 0);
    }

    #[test]
    fn parse_run_flags_table() {
        struct Case<'a> {
            name: &'a str,
            args: &'a [&'a str],
            open_net: bool,
            allow: &'a [&'a str],
            rest: &'a [&'a str],
            want_err: bool,
        }
        let cases = [
            Case { name: "empty", args: &[], open_net: false, allow: &[], rest: &[], want_err: false },
            Case { name: "agent flags pass through", args: &["--model", "opus"], open_net: false, allow: &[], rest: &["--model", "opus"], want_err: false },
            Case { name: "open-net then dashdash", args: &["--open-net", "--", "--help"], open_net: true, allow: &[], rest: &["--help"], want_err: false },
            Case { name: "allow comma list", args: &["--allow", "a.com,b.com", "arg"], open_net: false, allow: &["a.com", "b.com"], rest: &["arg"], want_err: false },
            Case { name: "allow equals form", args: &["--allow=x.com"], open_net: false, allow: &["x.com"], rest: &[], want_err: false },
            Case { name: "repeated allow", args: &["--allow", "a.com", "--allow", "b.com"], open_net: false, allow: &["a.com", "b.com"], rest: &[], want_err: false },
            Case { name: "allow missing value", args: &["--allow"], open_net: false, allow: &[], rest: &[], want_err: true },
            Case { name: "bare dashdash", args: &["--"], open_net: false, allow: &[], rest: &[], want_err: false },
            Case { name: "first unknown stops parsing", args: &["positional", "--open-net"], open_net: false, allow: &[], rest: &["positional", "--open-net"], want_err: false },
        ];
        for c in cases {
            let args = v(c.args);
            match parse_run_flags(&args) {
                Err(_) => assert!(c.want_err, "{}: unexpected error", c.name),
                Ok(f) => {
                    assert!(!c.want_err, "{}: expected error", c.name);
                    assert_eq!(f.open_net, c.open_net, "{}: open_net", c.name);
                    assert_eq!(f.extra_allow, v(c.allow), "{}: extra_allow", c.name);
                    assert_eq!(f.rest, v(c.rest), "{}: rest", c.name);
                }
            }
        }
    }
}
