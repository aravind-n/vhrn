//! Egress guard mode and the host-side policy files. The policy (allowlist, mode,
//! deny-log) lives under `<cache>/net` and is mounted only into the proxy, never the
//! box, so an in-box process can never widen its own egress; `vhrn net …` is the only
//! path that mutates it. The mode string is a byte-level contract (mode file +
//! VHRN_NET), so it round-trips through as_str/from_str unchanged. Ports net.go.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::run::set_mode;

/// The egress guard mode for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Enforce,
    Report,
    Open,
}

impl Mode {
    /// The wire string written to the mode file and VHRN_NET.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Mode::Enforce => "enforce",
            Mode::Report => "report",
            Mode::Open => "open",
        }
    }

    /// Parse a mode string; unknown values yield None (callers fall back to enforce).
    fn from_str(s: &str) -> Option<Mode> {
        match s {
            "enforce" => Some(Mode::Enforce),
            "report" => Some(Mode::Report),
            "open" => Some(Mode::Open),
            _ => None,
        }
    }
}

/// Pick the egress mode for a run: `--open-net` wins, else the config's net.mode,
/// else enforce. An unrecognized config value falls back to enforce.
pub(crate) fn resolve_mode(config_mode: &str, open_net: bool) -> Mode {
    if open_net {
        return Mode::Open;
    }
    Mode::from_str(config_mode).unwrap_or(Mode::Enforce)
}

/// Prepare the egress policy for a run and return the policy dir (to mount into the
/// proxy): ensure the dir, seed the default allowlist if absent, add the config- and
/// session-declared domains, write the mode, and truncate the deny log.
pub(crate) fn prepare_policy(
    cache: &Path,
    mode: Mode,
    config_allow: &[String],
    extra_allow: &[String],
) -> std::io::Result<PathBuf> {
    let np = NetPolicy::new(cache);
    np.ensure()?;
    np.seed_allowlist_if_absent();
    np.append_missing(config_allow);
    np.append_missing(extra_allow);
    np.write_mode(mode.as_str());
    np.truncate_deny_log();
    Ok(np.dir)
}

/// Seed the egress policy for an install: ensure the policy dir, write the default
/// allowlist if absent, then union the harness's default domains in (append-if-missing,
/// so later user edits survive). Unlike prepare_policy this touches neither the mode
/// file nor the deny log — an install only ever widens the allowlist.
pub(crate) fn seed_allowlist(cache: &Path, domains: &[String]) -> std::io::Result<()> {
    let np = NetPolicy::new(cache);
    np.ensure()?;
    np.seed_allowlist_if_absent();
    np.append_missing(domains);
    Ok(())
}

// Seeded on first run; never clobbers later edits. 12 domains + 2 comment lines.
const DEFAULT_ALLOWLIST: &str = r#"# vhrn egress allowlist — one domain per line, matching the domain and its
# subdomains. Edit freely, or run `vhrn net allow <domain>` while a box runs.
api.anthropic.com
claude.ai
platform.claude.com
statsig.anthropic.com
sentry.io
github.com
githubusercontent.com
registry.npmjs.org
pypi.org
files.pythonhosted.org
astral.sh
mise.jdx.dev
"#;

/// Locates the host-side egress policy files under `<cache>/net`.
struct NetPolicy {
    dir: PathBuf,
    allowlist: PathBuf,
    mode_file: PathBuf,
    deny_log: PathBuf,
}

impl NetPolicy {
    fn new(cache: &Path) -> NetPolicy {
        let dir = cache.join("net");
        NetPolicy {
            allowlist: dir.join("allowlist"),
            mode_file: dir.join("mode"),
            deny_log: dir.join("denied.log"),
            dir,
        }
    }

    /// Create the policy dir world-writable so the proxy container (a different uid)
    /// can append to denied.log.
    fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        set_mode(&self.dir, 0o777)
    }

    /// Write the default allowlist on first run; never clobber later edits.
    fn seed_allowlist_if_absent(&self) {
        if self.allowlist.is_file() {
            return;
        }
        let _ = std::fs::write(&self.allowlist, DEFAULT_ALLOWLIST);
    }

    /// The current allowlist file contents, one entry per line.
    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.allowlist)
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default()
    }

    /// Count non-comment, non-blank allowlist entries.
    fn count_domains(&self) -> usize {
        self.lines()
            .iter()
            .filter(|line| {
                let t = line.trim();
                !t.is_empty() && !t.starts_with('#')
            })
            .count()
    }

    /// Append domains not already present (exact line match), mirroring the run path's
    /// --allow handling. Non-atomic, as in the wrapper's run path.
    fn append_missing(&self, domains: &[String]) {
        if domains.is_empty() {
            return;
        }
        let mut set: HashSet<String> = self.lines().into_iter().collect();
        let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(&self.allowlist)
        else {
            return;
        };
        use std::io::Write;
        for d in domains {
            if set.insert(d.clone()) {
                let _ = writeln!(f, "{d}");
            }
        }
    }

    /// `net allow`: write the updated allowlist to a same-dir temp file and rename it
    /// into place, so the proxy (reading concurrently) never sees a torn file.
    fn append_missing_atomic(&self, domains: &[String]) -> std::io::Result<()> {
        let mut buf = String::new();
        let mut set: HashSet<String> = HashSet::new();
        if let Ok(data) = std::fs::read_to_string(&self.allowlist) {
            buf.push_str(&data);
            for line in data.split('\n') {
                set.insert(line.to_string());
            }
            if !data.is_empty() && !data.ends_with('\n') {
                buf.push('\n');
            }
        }
        for d in domains {
            if set.insert(d.clone()) {
                buf.push_str(d);
                buf.push('\n');
            }
        }
        let tmp = self.dir.join(format!("allowlist.{}.{}", std::process::id(), next_tmp_id()));
        std::fs::write(&tmp, &buf)?;
        set_mode(&tmp, 0o666)?;
        std::fs::rename(&tmp, &self.allowlist) // atomic on the same fs; proxy re-reads
    }

    fn write_mode(&self, mode: &str) {
        let _ = std::fs::write(&self.mode_file, format!("{mode}\n"));
    }

    fn truncate_deny_log(&self) {
        let _ = std::fs::write(&self.deny_log, b"");
        let _ = set_mode(&self.deny_log, 0o666);
    }

    /// The unique, sorted set of domains from the deny log's second field.
    fn denied_domains(&self) -> Vec<String> {
        let Ok(data) = std::fs::read_to_string(&self.deny_log) else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for line in data.split('\n') {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 2 {
                continue;
            }
            let d = fields[1];
            if seen.insert(d.to_string()) {
                out.push(d.to_string());
            }
        }
        out.sort();
        out
    }
}

// Per-process unique suffix for atomic temp files (os.CreateTemp's role).
fn next_tmp_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    CTR.fetch_add(1, Ordering::Relaxed)
}

/// Handle `vhrn net <subcommand>`: mutate the host-side egress policy the running box
/// reads. This is the only path to that policy — the box has none.
pub(crate) fn run_net(args: &[String]) -> i32 {
    let home = match crate::run::home_dir() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("vhrn: {e}");
            return 1;
        }
    };
    let np = NetPolicy::new(&crate::run::vhrn_cache(&home));
    let _ = std::fs::create_dir_all(&np.dir);

    let (cmd, rest): (&str, &[String]) = match args.split_first() {
        Some((c, r)) => (c, r),
        None => ("status", &[]),
    };

    match cmd {
        "status" => {
            let mode = std::fs::read_to_string(&np.mode_file)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "enforce".to_string());
            println!("mode:    {mode}");
            println!("allowed: {} domain(s) ({})", np.count_domains(), np.allowlist.display());
        }
        "denied" => {
            let domains = np.denied_domains();
            if domains.is_empty() {
                println!("no denials recorded this session");
                return 0;
            }
            for d in domains {
                println!("{d}");
            }
        }
        "allow" => {
            if rest.is_empty() {
                eprintln!("usage: vhrn net allow <domain>...");
                return 2;
            }
            if let Err(e) = np.append_missing_atomic(rest) {
                eprintln!("vhrn: {e}");
                return 1;
            }
            println!("allowed: {}", rest.join(" "));
        }
        "open" => {
            np.write_mode("open");
            println!("egress guard OFF (open) — all public hosts allowed");
        }
        "guard" => {
            np.write_mode("enforce");
            println!("egress guard ON (enforce) — allowlist enforced");
        }
        "report" => {
            np.write_mode("report");
            println!("egress guard REPORT — all allowed, denials logged");
        }
        _ => {
            eprintln!("usage: vhrn net {{status|denied|allow <domain>...|open|guard|report}}");
            return 2;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_mode_cases() {
        let cases = [
            ("", false, "enforce"),
            ("enforce", false, "enforce"),
            ("report", false, "report"),
            ("open", false, "open"),
            ("bogus", false, "enforce"),
            ("report", true, "open"), // --open-net wins over config
            ("", true, "open"),
        ];
        for (cfg, open, want) in cases {
            assert_eq!(resolve_mode(cfg, open).as_str(), want, "resolve_mode({cfg:?}, {open})");
        }
    }

    #[test]
    fn mode_roundtrips() {
        for m in [Mode::Enforce, Mode::Report, Mode::Open] {
            assert_eq!(Mode::from_str(m.as_str()), Some(m));
        }
        assert_eq!(Mode::from_str("nope"), None);
    }

    #[test]
    fn allowlist_seed_count_and_atomic_add() {
        let np = NetPolicy::new(&crate::testutil::temp_dir());
        np.ensure().unwrap();
        np.seed_allowlist_if_absent();
        let base = np.count_domains();
        assert_eq!(base, 12, "default domain count");
        // Adds a new domain; ignores duplicates (incl. one already present).
        np.append_missing_atomic(&["docs.rs".into(), "api.anthropic.com".into(), "docs.rs".into()])
            .unwrap();
        assert_eq!(np.count_domains(), base + 1);
        // Idempotent re-add.
        np.append_missing_atomic(&["docs.rs".into()]).unwrap();
        assert_eq!(np.count_domains(), base + 1);
    }

    #[test]
    fn seed_allowlist_unions_harness_domains() {
        let cache = crate::testutil::temp_dir();
        // Install seeds the 12 defaults, then unions the harness domains not already
        // present — api.anthropic.com is a default (no-op); example.test is new.
        seed_allowlist(&cache, &["api.anthropic.com".into(), "example.test".into()]).unwrap();
        let np = NetPolicy::new(&cache);
        assert_eq!(np.count_domains(), 13, "12 defaults + 1 new harness domain");
        // Idempotent: re-seeding the same domain adds nothing.
        seed_allowlist(&cache, &["example.test".into()]).unwrap();
        assert_eq!(np.count_domains(), 13);
    }

    #[test]
    fn denied_domains_unique_sorted() {
        let np = NetPolicy::new(&crate::testutil::temp_dir());
        np.ensure().unwrap();
        std::fs::write(&np.deny_log, "t1 evil.com GET\nt2 evil.com GET\nt3 tracker.io POST\n").unwrap();
        assert_eq!(np.denied_domains(), vec!["evil.com".to_string(), "tracker.io".to_string()]);

        let empty = NetPolicy::new(&crate::testutil::temp_dir());
        empty.ensure().unwrap();
        assert!(empty.denied_domains().is_empty());
    }
}
