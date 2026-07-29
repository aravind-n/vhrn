//! The harness registry — the single source of truth a subcommand, install, run,
//! and persistence all read from. Adding an agent (codex, aider, …) is a spec here
//! plus a thin `FROM vhrn-base` Dockerfile, not a fork of the CLI.

/// Describes one coding agent vhrn can run in the container.
#[derive(Clone, Debug, Default)]
pub(crate) struct Harness {
    pub name: String,    // registry key and subcommand, e.g. "claude"
    pub image: String,   // container image built for it, e.g. "vhrn-claude"
    pub command: String, // in-container argv[0], e.g. "claude"
    pub alias: String,   // shell alias installed for it

    /// Default egress domains unioned into the host allowlist at install time.
    pub allow_domains: Vec<String>,

    // Persistence — the three home-dir buckets (see persist.rs): a
    // container-owned state dir, bootstrap-only forwarded credentials, and disposable
    // synced config layered back on top each run.
    pub state_dir: String, // container-home-relative persistent dir, e.g. ".claude"
    pub config_dir_env: String, // env var pointing the agent's config dir at state_dir
    pub host_config: String, // host-home-relative dir to sync/bootstrap FROM
    pub sync_dirs: Vec<String>, // disposable synced subdirs, e.g. skills/commands/agents
    pub sync_files: Vec<String>, // disposable synced files, e.g. settings.json/statusline.sh
    pub credentials: Vec<String>, // state_dir-relative bootstrap-only files

    /// How the container guide is derived for this harness.
    pub guide: Guide,

    /// Host env vars forwarded into the container, each only when the host has it set.
    /// Nothing else from the host environment crosses.
    pub credential_env: Vec<String>,

    /// Mount the host's own config plus vhrn's constraints read-only at `/etc/<name>`.
    /// For an agent that writes its own config file, this is the only place vhrn can put
    /// configuration without fighting the agent for ownership of that file.
    pub system_config: bool,

    /// Mount this project's host-side history into the config dir, so in-container
    /// sessions unify with native ones. Only for an agent whose per-project history
    /// layout vhrn can reproduce exactly.
    pub share_history: bool,

    /// Per-project session store, for an agent that keeps one flat tree for every project.
    /// `sessions_env` points the agent's session index at the store; `sessions_dir` is the
    /// transcript subdir inside it that also layers under the config dir. Empty
    /// `sessions_env` means no partition — sessions stay in the shared state store.
    pub sessions_env: String,
    pub sessions_dir: String,
}

/// How vhrn derives one harness's container guide. Agents differ on every axis here —
/// what the file is called, which host globals fold into it, where it lands, and whether
/// it leads or trails the host's own text — so all four travel together.
#[derive(Clone, Debug, Default)]
pub(crate) struct Guide {
    pub file: String,         // the derived file's name; "" = this harness gets no guide
    pub sources: Vec<String>, // host globals to fold in; the first non-empty one wins
    pub in_state: bool,       // write into the state dir rather than the synced sandbox
    pub first: bool,          // guide before the host's text, for a byte-capped doc chain
}

/// The built-in registry. Adding an agent is an entry here plus a `FROM vhrn-base`
/// Dockerfile and a CI matrix row — never a branch in the CLI.
fn registry() -> Vec<Harness> {
    vec![
        Harness {
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
            guide: Guide {
                file: "CLAUDE.md".into(),
                sources: vec!["CLAUDE.md".into()],
                in_state: false,
                first: false,
            },
            credential_env: vec![], // claude's login lives in the state store, not the environment
            system_config: false,
            share_history: true,
            sessions_env: String::new(), // history is already per-project; nothing to partition
            sessions_dir: String::new(),
        },
        Harness {
            name: "codex".into(),
            image: "vhrn-codex".into(),
            command: "codex".into(),
            alias: "codex".into(),
            // The proxy matches label-anchored, so openai.com already covers api. and auth.
            // Deliberately the wide set for a first install: a user who cannot authenticate
            // cannot get far, and widening is a host command they would have to discover.
            // Narrow it once a live `vhrn net report` says what a login and a turn touch.
            allow_domains: vec!["chatgpt.com".into(), "openai.com".into()],
            state_dir: ".codex".into(),
            config_dir_env: "CODEX_HOME".into(),
            host_config: ".codex".into(),
            // No `skills`: the agent's own skills dir is where it installs remote skills and
            // caches its bundled set, so it is container state rather than host config. The
            // host's skill library arrives through ~/.agents, which every harness mounts.
            sync_dirs: vec!["prompts".into()],
            sync_files: vec![],
            // Nothing bootstrapped: device-auth logs in inside the container and mints its own
            // token, rather than sharing one rotating token with the host install.
            credentials: vec![],
            guide: Guide {
                // Only the first non-empty global file is read, so a user who already has an
                // override would shadow a guide written anywhere else. Take that slot and fold
                // their text in, and both survive.
                file: "AGENTS.override.md".into(),
                sources: vec!["AGENTS.override.md".into(), "AGENTS.md".into()],
                // The config dir is where this one is resolved from, and it is the state mount.
                in_state: true,
                // The instruction chain is capped by bytes and truncated from the end; the
                // guide must not be the part that gets cut.
                first: true,
            },
            credential_env: vec![
                "CODEX_API_KEY".into(),
                "CODEX_ACCESS_TOKEN".into(),
                "OPENAI_API_KEY".into(),
            ],
            system_config: true,
            share_history: false,
            sessions_env: "CODEX_SQLITE_HOME".into(),
            sessions_dir: "sessions".into(),
        },
    ]
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
    fn lookup_harness_codex() {
        let h = lookup_harness("codex").expect("codex should be a known harness");
        assert_eq!(h.image, "vhrn-codex");
        assert_eq!(h.command, "codex");
        assert_eq!(h.config_dir_env, "CODEX_HOME");
        assert_eq!(h.state_dir, ".codex");
        assert_eq!(h.guide.file, "AGENTS.override.md");
        assert!(h.guide.first && h.guide.in_state);
        assert_eq!(h.sessions_env, "CODEX_SQLITE_HOME");
        assert!(h.system_config);
        assert!(
            !h.share_history,
            "codex has no projects/<key> layout to share"
        );
        assert_eq!(
            h.credential_env,
            ["CODEX_API_KEY", "CODEX_ACCESS_TOKEN", "OPENAI_API_KEY"]
        );
        assert!(
            h.credentials.is_empty(),
            "codex logs in inside the container; nothing is copied from the host"
        );
        // The agent's own skills dir is container state, and the host library rides in on
        // ~/.agents — syncing either would clobber what the agent installed there.
        assert_eq!(h.sync_dirs, ["prompts"]);
    }

    // Every harness has to answer the persistence questions, or the run path silently
    // falls back to a default that was only ever right for one of them.
    #[test]
    fn every_harness_declares_its_persistence() {
        for h in registry() {
            let n = &h.name;
            assert!(!h.state_dir.is_empty(), "{n}: no state dir");
            assert!(!h.host_config.is_empty(), "{n}: no host config dir");
            assert!(!h.guide.file.is_empty(), "{n}: no container guide");
            assert!(!h.guide.sources.is_empty(), "{n}: guide folds in nothing");
            assert!(!h.allow_domains.is_empty(), "{n}: no egress domains");
            // A session partition needs both halves: an env var with no transcript dir
            // would index files that were never bound in.
            assert_eq!(
                h.sessions_env.is_empty(),
                h.sessions_dir.is_empty(),
                "{n}: half a session partition"
            );
        }
    }

    #[test]
    fn harness_names_sorted() {
        let names = harness_names();
        assert_eq!(names, ["claude", "codex"]);
        for w in names.windows(2) {
            assert!(w[0] <= w[1], "harness_names not sorted: {names:?}");
        }
    }
}
