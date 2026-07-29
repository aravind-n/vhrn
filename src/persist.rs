//! The container-owned state store and the disposable config sync. `state/<harness>/` is
//! the persistent store mounted as the container's config dir; host credentials seed it
//! bootstrap-only (an in-container login is never clobbered). Everything else in the store
//! belongs to the agent — onboarding and per-project trust included — so a decision made
//! in the container is the one that survives. The sandbox sync + container guide are
//! re-derived each run and layered on top as nested mounts.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use tracing::warn;

use crate::harness::Harness;
use crate::run::{look_path, set_mode};

/// The persistent, container-owned store for one harness (`<cache>/state/<harness>`),
/// physically separate from the disposable sandbox so no config sync can reach it.
fn host_state_dir(cache: &Path, harness: &str) -> PathBuf {
    cache.join("state").join(harness)
}

/// Ready the persistent store before launch and return its path: ensure the dir and
/// bootstrap credentials from the host once.
pub(crate) fn prepare_state(home: &Path, cache: &Path, h: &Harness) -> Result<PathBuf> {
    let state = host_state_dir(cache, &h.name);
    std::fs::create_dir_all(&state)?;
    set_mode(&state, 0o700)?;
    bootstrap_credentials(home, &state, h);
    Ok(state)
}

/// Ready this project's session store and return its path, or `None` for a harness that
/// does not partition sessions. A sibling of the shared state dir rather than a child: the
/// shared store holds the one login and config every project uses, while transcripts —
/// which carry source, paths, and instructions — belong only to the project that made them.
pub(crate) fn prepare_sessions(cache: &Path, h: &Harness, key: &str) -> Result<Option<PathBuf>> {
    if h.sessions_env.is_empty() {
        return Ok(None);
    }
    let store = cache
        .join("state")
        .join(format!("{}-sessions", h.name))
        .join(key);
    std::fs::create_dir_all(&store)?;
    set_mode(&store, 0o700)?;
    if !h.sessions_dir.is_empty() {
        std::fs::create_dir_all(store.join(&h.sessions_dir))?;
    }
    Ok(Some(store))
}

/// Copy each host credentials file into the store, but only when the store's copy is
/// absent. Bootstrap-only: once the container has its own (refreshed) credentials they are
/// authoritative and never clobbered, so an in-container login is never overwritten.
fn bootstrap_credentials(home: &Path, state: &Path, h: &Harness) {
    for rel in &h.credentials {
        let dst = state.join(rel);
        if dst.is_file() {
            continue; // container store already populated
        }
        let src = home.join(&h.host_config).join(rel);
        if !src.is_file() {
            continue; // nothing on the host to inherit; the container will prompt to log in
        }
        if let Err(e) = copy_file(&src, &dst) {
            warn!("could not seed {rel}: {e}");
            continue;
        }
        let _ = set_mode(&dst, 0o600); // credentials stay private
    }
}

/// Copy src to dst, following symlinks in src (like cp -L), creating parents.
fn copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

/// Mirror one host subdir into the sandbox, dereferencing symlinks (rsync -aL
/// --delete, cp -RL fallback). `real` is the host parent — the harness config dir for a
/// synced config dir, the home dir for `.agents`. --delete is confined to the
/// subdir, so top-level sandbox files are never pruned.
pub(crate) fn sync_subdir(real: &Path, sandbox: &Path, name: &str) {
    let src = real.join(name);
    let dst = sandbox.join(name);
    if !src.is_dir() {
        // The source is gone, so the mirror goes too — otherwise config the user deleted
        // keeps being mounted every run.
        let _ = std::fs::remove_dir_all(&dst);
        return;
    }
    if look_path("rsync") {
        let ok = Command::new("rsync")
            .args(["-aL", "--delete"])
            .arg(format!("{}/", src.display()))
            .arg(format!("{}/", dst.display()))
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            warn_skipped(name);
        }
        return;
    }
    let _ = std::fs::remove_dir_all(&dst);
    let ok = Command::new("cp")
        .arg("-RL")
        .arg(&src)
        .arg(&dst)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        warn_skipped(name);
    }
}

/// Copy a single ~/.claude file into the sandbox (cp -L).
pub(crate) fn copy_file_into(real: &Path, sandbox: &Path, name: &str) {
    let src = real.join(name);
    let dst = sandbox.join(name);
    if !src.is_file() {
        let _ = std::fs::remove_file(&dst); // as in sync_subdir: the mirror follows the source
        return;
    }
    if copy_file(&src, &dst).is_err() {
        warn!("could not copy '{name}'");
    }
}

fn warn_skipped(name: &str) {
    warn!("some '{name}' entries were skipped (broken symlink?)");
}

/// The system-config dir inside the sandbox, bound read-only at `/etc/<harness>`.
pub(crate) const SYSTEM_CONFIG_DIR: &str = "etc";
const CONFIG_FILE: &str = "config.toml";
/// Written by an earlier design and provably ignored by the agent; removed on sight so an
/// upgrade doesn't leave a file that looks like it enforces something.
const REQUIREMENTS_FILE: &str = "requirements.toml";
/// The table vhrn contributes as a default, when the host has not written it itself.
const ENV_POLICY_TABLE: &str = "shell_environment_policy";

/// Build the system-config layer for a harness that takes one: one file at
/// `<sandbox>/etc/config.toml`, bound read-only as the agent's admin-defaults layer.
///
/// This is the lowest-precedence layer the agent reads and never writes, which is the
/// whole point — the file the agent *does* write (its own config, carrying trust and
/// dismissed notices) stays entirely its own.
pub(crate) fn write_system_config(
    real_config: &Path,
    sandbox: &Path,
    h: &Harness,
) -> std::io::Result<()> {
    if !h.system_config {
        return Ok(());
    }
    let etc = sandbox.join(SYSTEM_CONFIG_DIR);
    std::fs::create_dir_all(&etc)?;
    let _ = std::fs::remove_file(etc.join(REQUIREMENTS_FILE));

    // TOML is UTF-8 by definition, so unreadable-as-text is unusable as config; treat it
    // as absent rather than failing the run over it.
    let src = real_config.join(CONFIG_FILE);
    let host = match std::fs::read_to_string(&src) {
        Ok(data) => data,
        Err(e) if src.is_file() => {
            warn!("could not read '{CONFIG_FILE}': {e}");
            String::new()
        }
        Err(_) => String::new(),
    };
    std::fs::write(
        etc.join(CONFIG_FILE),
        system_config_toml(&host, &h.credential_env),
    )
}

/// Compose the admin-defaults file: vhrn's own settings around the host's config.
///
/// Layout is dictated by TOML, not taste. A bare key belongs to whatever table precedes
/// it, so vhrn's top-level settings must come *before* the host's text and vhrn's table
/// must come *after* all of it — otherwise the host's own top-level keys would be swallowed
/// into vhrn's table. Duplicate keys and duplicate tables are both errors, so anything vhrn
/// sets is stripped from the host's copy first, and the table is contributed only when the
/// host has not written it.
///
/// Everything here is a *default*: this is the bottom of the precedence chain, so a
/// `--sandbox` flag or the container's own config outranks it. That is the trade for
/// putting it in the file the agent actually reads — the layer above it, which would have
/// outranked even the CLI, is ignored by the agent entirely.
fn system_config_toml(host: &str, credential_env: &[String]) -> String {
    let mut out = String::with_capacity(host.len() + 512);
    out.push_str(
        "# Generated by vhrn on every run — the host's config with vhrn's defaults around\n\
         # it. Mounted read-only; edits here are overwritten. Edit ~/.codex/config.toml.\n\n\
         # The container, its firewall, and the egress proxy are the sandbox. Nesting the\n\
         # agent's own on top buys little and costs its shell commands all network access.\n\
         sandbox_mode = \"danger-full-access\"\n",
    );
    if !host.is_empty() {
        out.push_str("\n# ---- host config.toml ----\n");
        out.push_str(&filter_host_config(host));
    }
    // Last, so it cannot capture the host's own top-level keys. Skipped when the host
    // writes this table itself: their policy is theirs, and a duplicate table is a parse
    // error that would stop the agent starting at all.
    if !credential_env.is_empty() && !defines_table(host, ENV_POLICY_TABLE) {
        let excluded = credential_env
            .iter()
            .map(|n| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(
            "\n# ---- vhrn defaults ----\n\
             # Keep forwarded credentials out of commands the agent spawns. A default, not a\n\
             # constraint: set this table in ~/.codex/config.toml and yours wins.\n[",
        );
        out.push_str(ENV_POLICY_TABLE);
        out.push_str("]\nexclude = [");
        out.push_str(&excluded);
        out.push_str("]\n");
    }
    out
}

/// Whether `doc` defines table `name` at its top level.
fn defines_table(doc: &str, name: &str) -> bool {
    doc.lines()
        .filter_map(table_header)
        .any(|t| t == name || t.starts_with(&format!("{name}.")))
}

/// The host's config, minus its project tables and minus the top-level keys vhrn sets
/// itself. A top-level `sandbox_mode` would collide with vhrn's; the same key inside a
/// table (a profile, say) belongs to that table and is left alone.
///
/// Dropping the project tables is not optional. An agent that records per-project trust in
/// its config file keys it by absolute path, and vhrn mounts the project at its real host
/// path — so copying those tables through would silently answer, inside the container, the
/// trust question the user only ever answered on the host. Trust is the agent's to ask and
/// the user's to grant, once, where the answer will actually be used.
///
/// Line-level rather than a parse, so the rest of the file crosses untouched (comments and
/// ordering included) and a malformed config stays the agent's error to report rather than
/// becoming vhrn's. The boundaries that costs: a `[projects…]`-shaped line inside a
/// multi-line string is dropped as well, and a bracketed element of a multi-line array
/// inside a project table reads as a header and ends the drop. Neither is something an
/// agent writes into this file, and both take a deliberately strange hand edit to produce.
fn filter_host_config(doc: &str) -> String {
    let mut out = String::with_capacity(doc.len());
    let mut dropping = false;
    let mut seen_table = false;
    for line in doc.lines() {
        if let Some(name) = table_header(line) {
            seen_table = true;
            dropping = name == "projects" || name.starts_with("projects.");
        } else if !seen_table && top_level_key(line) == Some("sandbox_mode") {
            continue;
        }
        if !dropping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The bare key of a top-level `key = value` line, ignoring blanks and comments.
fn top_level_key(line: &str) -> Option<&str> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    t.split_once('=').map(|(k, _)| k.trim())
}

/// The table name in a TOML header line (`[a.b]`, `[[a.b]]`), or `None` if the line is not
/// a header. Tolerant of the spellings a hand-edited file has and a generated one doesn't:
/// whitespace inside the brackets, and a trailing comment after them. Requiring the line to
/// *end* in `]` let both through, and a filter this one fails open.
fn table_header(line: &str) -> Option<&str> {
    let t = line.trim();
    if !t.starts_with('[') {
        return None;
    }
    let close = t.find(']')?;
    Some(t[..=close].trim_matches(|c| c == '[' || c == ']').trim())
}

/// Rebuild the container guide fresh each run, so it never accumulates across runs: a
/// guard-aware section that tracks the net mode, plus the host's own global instructions.
/// `real_config` is the host harness config dir; `dst` is the directory the derived file
/// lands in (the synced sandbox, or the state dir for a harness that reads it from there).
pub(crate) fn write_container_guide(
    real_config: &Path,
    dst: &Path,
    h: &Harness,
    open_net: bool,
) -> std::io::Result<()> {
    if h.guide.file.is_empty() {
        return Ok(()); // this harness takes no derived guide
    }
    let host = first_non_empty(real_config, &h.guide.sources);
    let body = compose_guide(&host, h.guide.first, open_net);
    std::fs::write(dst.join(&h.guide.file), body)
}

/// The contents of the first of `sources` that actually has any. An agent that reads only
/// the first file it finds must not be handed an empty one, so emptiness — not existence —
/// is what ends the search.
fn first_non_empty(real_config: &Path, sources: &[String]) -> Vec<u8> {
    for name in sources {
        if let Ok(data) = std::fs::read(real_config.join(name))
            && !data.is_empty()
        {
            return data;
        }
    }
    Vec::new()
}

/// Order vhrn's guide against the host's own instructions. Guide-first matters where the
/// agent caps the instruction chain by bytes and truncates the tail — the guide must not be
/// the part that gets cut.
fn compose_guide(host: &[u8], guide_first: bool, open_net: bool) -> Vec<u8> {
    let net = if open_net {
        CONTAINER_GUIDE_OPEN
    } else {
        CONTAINER_GUIDE_GUARD
    };
    let mut b: Vec<u8> = Vec::new();
    if !guide_first {
        b.extend_from_slice(host);
    }
    b.extend_from_slice(CONTAINER_GUIDE_HEADER.as_bytes());
    b.extend_from_slice(net.as_bytes());
    if guide_first {
        b.extend_from_slice(host);
    }
    b
}

const CONTAINER_GUIDE_HEADER: &str = r"
# vhrn environment

You are running inside vhrn: a container jailed to this project with a
network egress guard. Adapt as follows:

- **No sudo, no apt at runtime.** The toolchain is baked at build time. You can still
  `uv tool install <pkg>` for Python CLIs (PyPI is allowlisted). Anything else that is
  missing — a language runtime or a system package — must be added by the user to
  `[tools]` in their vhrn config (an `apt` entry or a `run` install command) and
  reinstalled; you cannot install it from inside the container.
";

const CONTAINER_GUIDE_OPEN: &str =
    "- **Network egress is unrestricted this session** (the guard is off via `--open-net`).\n";

// A denial surfaces as whatever the agent's HTTP client makes of a refused CONNECT — often
// after its own retries, and often indistinguishable from a flaky network. Say so, or the
// agent burns the session retrying instead of naming the host to the user.
const CONTAINER_GUIDE_GUARD: &str = "- **Network egress is allowlisted (default-deny).** A blocked request fails with\n  an error naming the domain, but it may arrive only after retries and may read as\n  an ordinary connection or timeout error rather than a policy denial — retrying\n  will not help. You cannot change the allowlist from inside the container; tell\n  the user the exact host(s) and ask them to run `vhrn net allow <host>` on the\n  host, then retry — no restart is needed.\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Guide;
    use crate::testutil::temp_dir;

    fn claude() -> Harness {
        Harness {
            name: "claude".into(),
            host_config: ".claude".into(),
            credentials: vec![".credentials.json".into()],
            guide: Guide {
                file: "CLAUDE.md".into(),
                sources: vec!["CLAUDE.md".into()],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    // A guide with a source fallback chain, written guide-first — the codex shape.
    fn chained() -> Harness {
        Harness {
            guide: Guide {
                file: "AGENTS.override.md".into(),
                sources: vec!["AGENTS.override.md".into(), "AGENTS.md".into()],
                first: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn sync_mirrors_a_live_source() {
        let host = temp_dir();
        let sandbox = temp_dir();
        std::fs::create_dir_all(host.path().join("skills")).unwrap();
        std::fs::write(host.path().join("skills").join("SKILL.md"), "real").unwrap();
        std::fs::write(host.path().join("settings.json"), "{}").unwrap();

        sync_subdir(host.path(), sandbox.path(), "skills");
        copy_file_into(host.path(), sandbox.path(), "settings.json");

        assert_eq!(
            std::fs::read_to_string(sandbox.path().join("skills").join("SKILL.md")).unwrap(),
            "real"
        );
        assert_eq!(
            std::fs::read_to_string(sandbox.path().join("settings.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn sync_drops_a_copy_whose_source_is_gone() {
        let host = temp_dir(); // nothing on the host behind either copy
        let sandbox = temp_dir();
        std::fs::create_dir_all(sandbox.path().join("skills")).unwrap();
        std::fs::write(sandbox.path().join("skills").join("SKILL.md"), "stale").unwrap();
        std::fs::write(sandbox.path().join("settings.json"), "stale").unwrap();

        sync_subdir(host.path(), sandbox.path(), "skills");
        copy_file_into(host.path(), sandbox.path(), "settings.json");

        // Config the user deleted must not keep being mounted.
        assert!(
            !sandbox.path().join("skills").exists(),
            "stale dir outlived its source"
        );
        assert!(
            !sandbox.path().join("settings.json").exists(),
            "stale file outlived its source"
        );
    }

    #[test]
    fn bootstrap_credentials_is_seed_only() {
        let home = temp_dir();
        let state = temp_dir();
        let h = claude();

        // No host creds: nothing seeded.
        bootstrap_credentials(home.path(), state.path(), &h);
        assert!(
            !state.path().join(".credentials.json").is_file(),
            "seeded creds without a host source"
        );

        // Host login present + empty store: inherited.
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude").join(".credentials.json"),
            "HOST",
        )
        .unwrap();
        bootstrap_credentials(home.path(), state.path(), &h);
        assert_eq!(
            std::fs::read_to_string(state.path().join(".credentials.json")).unwrap(),
            "HOST"
        );

        // Container has since logged in: the host seed must not clobber it.
        std::fs::write(state.path().join(".credentials.json"), "existing").unwrap();
        bootstrap_credentials(home.path(), state.path(), &h);
        assert_eq!(
            std::fs::read_to_string(state.path().join(".credentials.json")).unwrap(),
            "existing"
        );
    }

    // A harness that injects configuration through a read-only system layer.
    fn layered() -> Harness {
        Harness {
            system_config: true,
            credential_env: vec!["CODEX_API_KEY".into(), "OPENAI_API_KEY".into()],
            ..Default::default()
        }
    }

    fn etc_config(sandbox: &std::path::Path) -> std::io::Result<String> {
        std::fs::read_to_string(sandbox.join(SYSTEM_CONFIG_DIR).join(CONFIG_FILE))
    }

    #[test]
    fn system_config_carries_the_host_file_without_understanding_it() {
        let host = temp_dir();
        let sandbox = temp_dir();
        // Deliberately malformed: vhrn does not parse this, so the run must not fail and
        // the text must arrive intact for the agent to complain about, as it would natively.
        let doc = "model = \"gpt-5\"\nthis is not = = valid toml\n[tui]\nnotifications = true\n";
        std::fs::write(host.path().join(CONFIG_FILE), doc).unwrap();

        write_system_config(host.path(), sandbox.path(), &layered()).unwrap();
        assert!(etc_config(sandbox.path()).unwrap().contains(doc));
    }

    #[test]
    fn system_config_forces_the_sandbox_mode_the_agent_reads() {
        let host = temp_dir();
        let sandbox = temp_dir();
        // A host that asks for a nested sandbox: vhrn's setting has to win, and the two
        // cannot both appear or the file stops being valid TOML and the agent won't start.
        std::fs::write(
            host.path().join(CONFIG_FILE),
            "sandbox_mode = \"workspace-write\"\nmodel = \"gpt-5\"\n[profiles.x]\nsandbox_mode = \"read-only\"\n",
        )
        .unwrap();

        write_system_config(host.path(), sandbox.path(), &layered()).unwrap();
        let got = etc_config(sandbox.path()).unwrap();

        assert_eq!(
            got.matches("sandbox_mode = ").count(),
            2,
            "expected vhrn's key and the profile's, no host duplicate: {got:?}"
        );
        assert!(got.contains("sandbox_mode = \"danger-full-access\""));
        assert!(
            !got.contains("workspace-write"),
            "host top-level key survived"
        );
        // The same key inside a table belongs to that table and is none of vhrn's business.
        assert!(got.contains("[profiles.x]") && got.contains("read-only"));
        // vhrn's bare key must precede every table, or it lands inside one.
        assert!(
            got.find("sandbox_mode = \"danger-full-access\"").unwrap() < got.find('[').unwrap(),
            "vhrn's top-level key must come before any table: {got:?}"
        );
    }

    #[test]
    fn system_config_drops_host_trust_tables() {
        let host = temp_dir();
        let sandbox = temp_dir();
        std::fs::write(
            host.path().join(CONFIG_FILE),
            concat!(
                "model = \"gpt-5\"\n",
                "[projects.\"/Users/u/projects/vhrn\"]\n",
                "trust_level = \"trusted\"\n",
                "[projects.\"/Users/u/other\"]\n",
                "trust_level = \"trusted\"\n",
                "[tui]\n",
                "notifications = true\n",
            ),
        )
        .unwrap();

        write_system_config(host.path(), sandbox.path(), &layered()).unwrap();
        let got = etc_config(sandbox.path()).unwrap();

        // The whole reason this file is filtered: the container mounts the project at its
        // real host path, so a copied trust entry would answer the trust prompt for it.
        assert!(!got.contains("trust_level"), "host trust crossed: {got:?}");
        // Everything else survives, including the table that follows the dropped ones.
        assert!(got.contains("model = \"gpt-5\"\n[tui]\nnotifications = true\n"));
    }

    // The layout exists to satisfy TOML, so check it with a TOML parser rather than by
    // eye: a bare key after a table header, a duplicate key, or a duplicate table are all
    // parse errors, and any of them stops the agent starting at all.
    #[test]
    fn system_config_output_is_valid_toml() {
        let creds = vec!["CODEX_API_KEY".to_string(), "OPENAI_API_KEY".to_string()];
        let hosts = [
            "",
            "model = \"gpt-5\"\n",
            // Every hazard at once: a colliding top-level key, trust tables, a trailing
            // table whose keys must not absorb vhrn's, and a table vhrn also contributes.
            "sandbox_mode = \"workspace-write\"\nmodel = \"gpt-5\"\n\
             [projects.\"/p\"]\ntrust_level = \"trusted\"\n\
             [tui]\nnotifications = true\n",
            "[shell_environment_policy]\ninherit = \"all\"\n",
            "[tui]\nx = 1\n",
        ];
        for host in hosts {
            let out = system_config_toml(host, &creds);
            let parsed: toml::Value = toml::from_str(&out)
                .unwrap_or_else(|e| panic!("generated invalid TOML for {host:?}: {e}\n{out}"));
            // The setting that matters has to survive the round trip as vhrn's value.
            assert_eq!(
                parsed.get("sandbox_mode").and_then(toml::Value::as_str),
                Some("danger-full-access"),
                "sandbox_mode lost or overridden for {host:?}"
            );
            assert!(
                parsed.get("projects").is_none(),
                "host trust reached the parsed document for {host:?}"
            );
        }
    }

    #[test]
    fn filter_host_config_boundaries() {
        // A lookalike key and a lookalike table must both survive.
        let doc =
            "projects_root = \"/x\"\n[projectsettings]\na = 1\n[projects]\nb = 2\n[z]\nc = 3\n";
        assert_eq!(
            filter_host_config(doc),
            "projects_root = \"/x\"\n[projectsettings]\na = 1\n[z]\nc = 3\n"
        );
        // An array-of-tables form is dropped too, and a doc that is nothing but project
        // tables comes back empty rather than half-parsed.
        assert_eq!(filter_host_config("[[projects.a]]\nx = 1\n"), "");
        assert_eq!(filter_host_config(""), "");

        // Spellings a hand-edited file has and a generated one does not. Both used to slip
        // through header detection, taking a host trust answer into the container with them.
        for header in [
            "[projects.\"/p\"]",
            "[ projects.\"/p\" ]",
            "[projects.\"/p\"]  # work laptop",
            "\t[projects.\"/p\"]",
            "[[projects.\"/p\"]] # array-of-tables form",
        ] {
            let got = filter_host_config(&format!("{header}\ntrust_level = \"trusted\"\n"));
            assert_eq!(got, "", "trust survived {header:?}: {got:?}");
        }

        // A multi-line array must not be mistaken for a header and swallow the rest.
        let arr = "exclude = [\n  \"A\",\n]\n[tui]\nx = 1\n";
        assert_eq!(filter_host_config(arr), arr);
    }

    #[test]
    fn system_config_env_policy_yields_to_the_host() {
        let host = temp_dir();
        let sandbox = temp_dir();
        let h = layered();

        // Nothing on the host: vhrn contributes the table, rendered from credential_env so
        // the forwarded set and the excluded set cannot drift apart.
        write_system_config(host.path(), sandbox.path(), &h).unwrap();
        let got = etc_config(sandbox.path()).unwrap();
        assert!(got.contains(r#"exclude = ["CODEX_API_KEY", "OPENAI_API_KEY"]"#));

        // The host writes the table itself: theirs wins and vhrn contributes none, because
        // two definitions of one table is a parse error that stops the agent starting.
        std::fs::write(
            host.path().join(CONFIG_FILE),
            "[shell_environment_policy]\ninherit = \"all\"\n",
        )
        .unwrap();
        write_system_config(host.path(), sandbox.path(), &h).unwrap();
        let got = etc_config(sandbox.path()).unwrap();
        assert_eq!(
            got.matches("[shell_environment_policy]").count(),
            1,
            "duplicate table would fail the agent's own parse: {got:?}"
        );
        assert!(got.contains("inherit = \"all\""));
        assert!(
            !got.contains("exclude = ["),
            "vhrn overrode the host's own policy"
        );
    }

    #[test]
    fn system_config_is_one_file_and_follows_its_source() {
        let host = temp_dir();
        let sandbox = temp_dir();
        let h = layered();
        let etc = sandbox.path().join(SYSTEM_CONFIG_DIR);

        // An earlier design wrote a constraints file the agent ignores. Leaving it behind
        // would look like it still enforces something.
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join(REQUIREMENTS_FILE), "sandbox_mode = \"x\"\n").unwrap();
        write_system_config(host.path(), sandbox.path(), &h).unwrap();
        assert!(
            !etc.join(REQUIREMENTS_FILE).exists(),
            "stale constraints file kept"
        );

        // vhrn's own settings land even with no host config at all.
        assert!(
            etc_config(sandbox.path())
                .unwrap()
                .contains("danger-full-access")
        );

        // A host source that disappears leaves vhrn's settings, not the vanished text.
        std::fs::write(host.path().join(CONFIG_FILE), "model = \"gone\"\n").unwrap();
        write_system_config(host.path(), sandbox.path(), &h).unwrap();
        assert!(etc_config(sandbox.path()).unwrap().contains("gone"));
        std::fs::remove_file(host.path().join(CONFIG_FILE)).unwrap();
        write_system_config(host.path(), sandbox.path(), &h).unwrap();
        let got = etc_config(sandbox.path()).unwrap();
        assert!(
            !got.contains("gone"),
            "config the user deleted kept being mounted"
        );
        assert!(got.contains("danger-full-access"));

        // A harness that takes no system config gets no directory at all.
        let bare = temp_dir();
        write_system_config(host.path(), bare.path(), &Harness::default()).unwrap();
        assert!(!bare.path().join(SYSTEM_CONFIG_DIR).exists());
    }

    #[test]
    fn guide_order_follows_the_harness() {
        let host = temp_dir();
        let dst = temp_dir();
        std::fs::write(host.path().join("CLAUDE.md"), "HOST").unwrap();

        write_container_guide(host.path(), dst.path(), &claude(), false).unwrap();
        let got = std::fs::read_to_string(dst.path().join("CLAUDE.md")).unwrap();
        assert!(
            got.starts_with("HOST"),
            "claude puts the host text first: {got:?}"
        );
        assert!(got.contains("# vhrn environment"), "guide section missing");

        // Same body, opposite order: the guide must survive a truncated tail.
        std::fs::write(host.path().join("AGENTS.md"), "HOST").unwrap();
        write_container_guide(host.path(), dst.path(), &chained(), false).unwrap();
        let got = std::fs::read_to_string(dst.path().join("AGENTS.override.md")).unwrap();
        assert!(
            got.starts_with("\n# vhrn environment"),
            "guide should lead: {got:?}"
        );
        assert!(got.ends_with("HOST"), "host text should trail: {got:?}");
    }

    #[test]
    fn guide_source_chain_takes_the_first_with_content() {
        let host = temp_dir();
        let dst = temp_dir();
        let read = |dst: &std::path::Path| {
            std::fs::read_to_string(dst.join("AGENTS.override.md")).unwrap()
        };

        // Neither source present: the guide still lands, on its own.
        write_container_guide(host.path(), dst.path(), &chained(), false).unwrap();
        assert!(read(dst.path()).ends_with('\n'));

        // Only the fallback has content.
        std::fs::write(host.path().join("AGENTS.md"), "FALLBACK").unwrap();
        write_container_guide(host.path(), dst.path(), &chained(), false).unwrap();
        assert!(read(dst.path()).ends_with("FALLBACK"));

        // An *empty* override must not shadow a populated fallback — only the first
        // non-empty file wins, so existence alone is not enough to end the search.
        std::fs::write(host.path().join("AGENTS.override.md"), "").unwrap();
        write_container_guide(host.path(), dst.path(), &chained(), false).unwrap();
        assert!(read(dst.path()).ends_with("FALLBACK"));

        // Populated override wins.
        std::fs::write(host.path().join("AGENTS.override.md"), "OVERRIDE").unwrap();
        write_container_guide(host.path(), dst.path(), &chained(), false).unwrap();
        assert!(read(dst.path()).ends_with("OVERRIDE"));
    }

    #[test]
    fn guide_tracks_the_net_mode_and_can_be_declined() {
        let host = temp_dir();
        let dst = temp_dir();

        write_container_guide(host.path(), dst.path(), &claude(), false).unwrap();
        let guarded = std::fs::read_to_string(dst.path().join("CLAUDE.md")).unwrap();
        assert!(guarded.contains("vhrn net allow"), "guard text missing");

        write_container_guide(host.path(), dst.path(), &claude(), true).unwrap();
        let open = std::fs::read_to_string(dst.path().join("CLAUDE.md")).unwrap();
        assert!(open.contains("unrestricted"), "open-net text missing");
        assert!(
            !open.contains("vhrn net allow"),
            "stale guard text carried over"
        );

        // A harness with no guide file writes nothing at all.
        write_container_guide(host.path(), dst.path(), &Harness::default(), false).unwrap();
        assert_eq!(
            std::fs::read_dir(dst.path()).unwrap().count(),
            1,
            "a harness with no guide should leave no extra file"
        );
    }

    #[test]
    fn prepare_state_leaves_the_config_json_alone() {
        let home = temp_dir();
        let cache = temp_dir();
        let h = claude();

        // A container-owned file that has the project explicitly *untrusted*.
        let state = cache.path().join("state").join("claude");
        std::fs::create_dir_all(&state).unwrap();
        let path = state.join(".claude.json");
        let before = r#"{"projects":{"/proj":{"hasTrustDialogAccepted":false}}}"#;
        std::fs::write(&path, before).unwrap();

        prepare_state(home.path(), cache.path(), &h).unwrap();

        // The agent owns this file: an untrust made in the container must survive.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
}
