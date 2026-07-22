//! Egress guard mode. The mode string is a byte-level contract — it is written to
//! the mode file and passed as VHRN_NET — so it round-trips through as_str/from_str
//! unchanged. Ports net.go's mode logic; the policy file ops land in a later phase.

/// The egress guard mode for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Enforce,
    Report,
    Open,
}

impl Mode {
    /// The wire string written to the mode file and VHRN_NET.
    fn as_str(self) -> &'static str {
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
fn resolve_mode(config_mode: &str, open_net: bool) -> Mode {
    if open_net {
        return Mode::Open;
    }
    Mode::from_str(config_mode).unwrap_or(Mode::Enforce)
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
}
