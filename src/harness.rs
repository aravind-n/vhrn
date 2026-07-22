//! The harness registry — the single source of truth a subcommand, install, run,
//! and persistence all read from. Adding an agent (codex, aider, …) is a spec here
//! plus a thin `FROM vhrn-base` Dockerfile, not a fork of the CLI. Ports harness.go.

/// Describes one coding agent vhrn can run in the box.
#[derive(Clone, Debug, Default)]
pub(crate) struct Harness {
    pub name: String,    // registry key and subcommand, e.g. "claude"
    pub image: String,   // box image built for it, e.g. "vhrn-claude"
    pub command: String, // in-box argv[0], e.g. "claude"
    pub alias: String,   // shell alias installed for it

    /// Default egress domains unioned into the host allowlist at install time.
    pub allow_domains: Vec<String>,

    // Persistence — the three home-dir buckets (see persist.rs / state.go): a
    // box-owned state dir, bootstrap-only forwarded credentials, and disposable
    // synced config layered back on top each run.
    pub state_dir: String,        // box-home-relative persistent dir, e.g. ".claude"
    pub config_dir_env: String,   // env var pointing the agent's config dir at state_dir
    pub host_config: String,      // host-home-relative dir to sync/bootstrap FROM
    pub sync_dirs: Vec<String>,   // disposable synced subdirs, e.g. skills/commands/agents
    pub sync_files: Vec<String>,  // disposable synced files, e.g. settings.json/statusline.sh
    pub credentials: Vec<String>, // state_dir-relative bootstrap-only files
    pub config_json: String,      // state_dir-relative login/onboarding/trust file
    pub seed_trust: bool,         // pre-seed onboarding + per-project trust into config_json
}

/// The built-in registry. Only claude exists today; the struct shape is what a
/// codex/aider spec would fill in.
fn registry() -> Vec<Harness> {
    vec![Harness {
        name: "claude".into(),
        image: "vhrn-claude".into(),
        command: "claude".into(),
        alias: "claude".into(),
        allow_domains: vec![
            "api.anthropic.com".into(),
            "claude.ai".into(),
            "platform.claude.com".into(),
            "statsig.anthropic.com".into(),
            "sentry.io".into(),
        ],
        state_dir: ".claude".into(),
        config_dir_env: "CLAUDE_CONFIG_DIR".into(),
        host_config: ".claude".into(),
        sync_dirs: vec!["skills".into(), "commands".into(), "agents".into()],
        sync_files: vec!["settings.json".into(), "statusline.sh".into()],
        credentials: vec![".credentials.json".into()],
        config_json: ".claude.json".into(),
        seed_trust: true,
    }]
}

/// The spec for `name`, or `None` if it is not a known harness.
pub(crate) fn lookup_harness(name: &str) -> Option<Harness> {
    registry().into_iter().find(|h| h.name == name)
}

/// The known harness names, sorted for stable output.
pub(crate) fn harness_names() -> Vec<String> {
    let mut names: Vec<String> = registry().into_iter().map(|h| h.name).collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_harness_claude() {
        let h = lookup_harness("claude").expect("claude should be a known harness");
        assert_eq!(h.image, "vhrn-claude");
        assert_eq!(h.command, "claude");
        assert_eq!(h.alias, "claude");
        assert_eq!(h.config_dir_env, "CLAUDE_CONFIG_DIR");
        assert_eq!(h.state_dir, ".claude");
        assert!(
            !h.credentials.is_empty(),
            "claude should bootstrap at least one credentials file"
        );
        assert!(
            lookup_harness("nope").is_none(),
            "unknown harness should not resolve"
        );
    }

    #[test]
    fn harness_names_sorted() {
        let names = harness_names();
        assert!(!names.is_empty(), "expected at least one harness");
        for w in names.windows(2) {
            assert!(w[0] <= w[1], "harness_names not sorted: {names:?}");
        }
    }
}
