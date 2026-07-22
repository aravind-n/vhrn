//! Subcommand dispatch, run-flag parsing, and the usage text — the CLI's front door.

use anyhow::{Result, bail};
use tracing::{error, info, warn};

// Subcommand-first help. Bare `vhrn` prints it; a harness runs as `vhrn <harness> …`.
const USAGE: &str = r"vhrn runs coding agents in a container jailed to the current project, with
default-deny network egress.

Usage:
  vhrn install <harness>                  build images, seed egress, add a shell alias
  vhrn uninstall <harness>                remove the alias/registry entry (--image drops the image)
  vhrn <harness> [flags] [-- ] [args...]  run a harness in the container
  vhrn list                               show known and installed harnesses
  vhrn net <subcommand>                   manage the egress policy
  vhrn help                               show this help
  vhrn --version                          print the version

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
  VHRN_IMAGE         container image name (default: per-harness, e.g. vhrn-claude)
  VHRN_PROXY_IMAGE   proxy image name (default: vhrn-proxy)
  VHRN_PROXY_PORT    proxy port (default: 8080)
";

/// The reported version: an override baked in by release/nightly CI, else the crate version.
pub(crate) fn version() -> &'static str {
    option_env!("VHRN_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Dispatch argv (already stripped of the program name) and return a process exit
/// code. Bare `vhrn` and `help` print usage; an unknown command is an error (exit 2).
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None | Some("help" | "-h" | "--help") => {
            print!("{USAGE}");
            0
        }
        Some("version" | "-V" | "--version") => {
            println!("vhrn {}", version());
            0
        }
        Some("net") => crate::net::run_net(&args[1..]),
        Some("install") => run_install(&args[1..]),
        Some("uninstall") => run_uninstall(&args[1..]),
        Some("list") => run_list(&args[1..]),
        // A known harness runs that agent; the wrapper's own flags come right after
        // it, then everything else forwards to the agent verbatim.
        Some(cmd) => {
            if let Some(h) = crate::harness::lookup_harness(cmd) {
                match parse_run_flags(&args[1..]) {
                    Ok(flags) => match crate::run::run_harness(&h, &flags) {
                        Ok(code) => code,
                        // Wrapper-level failures get a message; the agent's own non-zero
                        // exit is returned verbatim above.
                        Err(e) => {
                            error!("{e}");
                            1
                        }
                    },
                    Err(e) => {
                        error!("{e}");
                        2
                    }
                }
            } else {
                eprintln!("vhrn: unknown command {cmd:?} — run 'vhrn help'");
                2
            }
        }
    }
}

/// Show every known harness and whether `vhrn install` has set it up.
fn run_list(_args: &[String]) -> i32 {
    let home = match crate::run::home_dir() {
        Ok(h) => h,
        Err(e) => {
            error!("{e}");
            return 1;
        }
    };
    let config_dir = crate::shell::vhrn_config_dir(&home);
    let installed: std::collections::HashMap<String, String> =
        crate::shell::read_installed(&config_dir)
            .into_iter()
            .map(|ih| (ih.name, ih.version))
            .collect();
    for name in crate::harness::harness_names() {
        match installed.get(&name) {
            Some(v) => println!("  {name:<12} installed ({v})"),
            None => println!("  {name:<12} available"),
        }
    }
    0
}

/// Pull a harness's image and the matching-version proxy from the registry, union its
/// egress domains into the allowlist, record the harness+version in the installed
/// registry, and write shell aliases. `--local` uses images already built by `make`
/// instead of pulling (for development/offline).
fn run_install(args: &[String]) -> i32 {
    let mut arg = String::new();
    let mut local = false;
    for a in args {
        if a == "--local" {
            local = true;
        } else if arg.is_empty() {
            arg.clone_from(a);
        }
    }
    if arg.is_empty() {
        eprintln!("usage: vhrn install <harness>[@version] [--local]");
        return 2;
    }
    let (name, mut version) = crate::image::parse_harness_arg(&arg);
    if local {
        version = crate::image::LOCAL_VERSION.to_string();
    }
    let Some(h) = crate::harness::lookup_harness(&name) else {
        error!(
            "unknown harness {name:?} (known: {})",
            crate::harness::harness_names().join(", ")
        );
        return 2;
    };
    let home = match crate::run::home_dir() {
        Ok(h) => h,
        Err(e) => {
            error!("{e}");
            return 1;
        }
    };
    let engine = match crate::run::detect_engine() {
        Ok(e) => e,
        Err(e) => {
            error!("{e}");
            return 1;
        }
    };

    if let Err(e) =
        crate::image::provision_images(&engine, &crate::image::registry_base(), &h, &version)
    {
        error!("{e}");
        return 1;
    }

    // Union base defaults + this harness's domains into the host allowlist,
    // append-if-missing so later user edits are respected.
    if let Err(e) = crate::net::seed_allowlist(&crate::run::vhrn_cache(&home), &h.allow_domains) {
        error!("{e}");
        return 1;
    }

    let config_dir = crate::shell::vhrn_config_dir(&home);
    if let Err(e) = crate::shell::add_installed(&config_dir, &name, &version) {
        error!("{e}");
        return 1;
    }
    if let Err(e) =
        crate::shell::sync_aliases(&config_dir, &home, crate::shell::current_shell().as_deref())
    {
        warn!("could not update shell aliases: {e}");
    }

    println!(
        "Installed {name} ({version}). Restart your shell (or source your rc file) to use `{}`.",
        h.alias
    );
    0
}

/// Drop a harness from the installed registry and regenerate the shell aliases so its
/// alias disappears. With `--image` it also deletes the harness image (the shared base
/// and proxy are left in place for other harnesses).
fn run_uninstall(args: &[String]) -> i32 {
    let mut name = String::new();
    let mut rm_image = false;
    for a in args {
        if a == "--image" {
            rm_image = true;
        } else if name.is_empty() {
            name.clone_from(a);
        }
    }
    if name.is_empty() {
        eprintln!("usage: vhrn uninstall <harness> [--image]");
        return 2;
    }
    let home = match crate::run::home_dir() {
        Ok(h) => h,
        Err(e) => {
            error!("{e}");
            return 1;
        }
    };
    let config_dir = crate::shell::vhrn_config_dir(&home);

    // Capture the version before dropping the entry, so --image deletes the exact ref
    // that was installed (a versioned registry ref, or the bare local name).
    let version = crate::shell::installed_version(&config_dir, &name);

    if let Err(e) = crate::shell::remove_installed(&config_dir, &name) {
        error!("{e}");
        return 1;
    }
    if let Err(e) =
        crate::shell::sync_aliases(&config_dir, &home, crate::shell::current_shell().as_deref())
    {
        warn!("could not update shell aliases: {e}");
    }

    let mut alias = name.clone();
    match crate::harness::lookup_harness(&name) {
        Some(h) => {
            alias.clone_from(&h.alias);
            if rm_image && version.is_none() {
                warn!("{name:?} was not installed; no image to remove");
            } else if rm_image && let Ok(engine) = crate::run::detect_engine() {
                let img = crate::image::harness_image_ref(
                    &crate::image::registry_base(),
                    &h,
                    version.as_deref().unwrap_or(""),
                );
                info!("removing image {img}...");
                if let Err(e) = crate::image::remove_image(&engine, &img) {
                    warn!("could not remove image {img}: {e}");
                }
            }
        }
        // Unknown harness: nothing to alias, and no image ref we can form to remove.
        None => {
            if rm_image {
                warn!("unknown harness {name:?}; cannot remove its image");
            }
        }
    }

    println!("Uninstalled {name}. Restart your shell to drop the `{alias}` alias.");
    0
}

/// The wrapper-owned flags consumed before the agent's own args.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct RunFlags {
    pub open_net: bool,           // --open-net: drop the egress guard this run
    pub extra_allow: Vec<String>, // --allow: session additions to the allowlist
    pub rest: Vec<String>,        // everything forwarded to the agent verbatim
}

/// Consume wrapper flags up front then forward the rest verbatim: `--open-net` /
/// `--allow[=]<d,d>` are read, `--` stops flag reading, and the first unrecognized
/// token ends parsing (so agent flags pass through untouched).
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
    s.split(',')
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    }

    // Smoke test: the entry point is callable and returns success.
    #[test]
    fn run_prints_usage_and_succeeds() {
        assert_eq!(run(&[]), 0);
        assert_eq!(run(&["help".to_string()]), 0);
    }

    #[test]
    fn run_unknown_command_exits_2() {
        assert_eq!(run(&["definitely-not-a-command".to_string()]), 2);
    }

    #[test]
    fn run_version_succeeds() {
        for a in ["version", "-V", "--version"] {
            assert_eq!(run(&[a.to_string()]), 0, "{a}");
        }
    }

    // With no CI override baked in, the version falls back to the crate version.
    #[test]
    fn version_falls_back_to_crate_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
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
        // Keep the case table aligned one-per-row; rustfmt would explode each into 8 lines.
        #[rustfmt::skip]
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
