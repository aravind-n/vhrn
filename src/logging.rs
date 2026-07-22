//! Logging setup: a thin tracing-subscriber wrapper. vhrn is an interactive CLI, not
//! a daemon, so the default is a terse, timestamp-less, target-less stderr line, with
//! `RUST_LOG` tuning levels. Config is resolved from the environment at a thin edge, like
//! env.rs, so the pure part stays testable. Modelled on ~/projects/prep/src/logging.rs.

use std::io::IsTerminal;

use anyhow::Result;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::SystemTime;

/// Output format. `Auto` picks Pretty on a TTY, Compact otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogFormat {
    Auto,
    Pretty,
    Compact,
}

impl LogFormat {
    /// Parse a `VHRN_LOG_FORMAT` value; anything unrecognized is Compact.
    fn parse(s: &str) -> LogFormat {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => LogFormat::Auto,
            "pretty" => LogFormat::Pretty,
            _ => LogFormat::Compact,
        }
    }
}

/// The pure input to `init_tracing`, kept apart from env reading (convention #2).
pub(crate) struct LogConfig {
    pub format: LogFormat,
    pub with_time: bool,
    pub with_ansi: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Compact,
            with_time: false,
            with_ansi: true,
        }
    }
}

/// Resolve a config from env inputs passed in, so this stays pure and testable.
fn resolve(format: Option<&str>, no_color: bool, is_tty: bool) -> LogConfig {
    LogConfig {
        format: format.map_or(LogFormat::Compact, LogFormat::parse),
        with_time: false,
        with_ansi: !no_color && is_tty,
    }
}

/// Edge: read the real env/TTY and resolve the logging config.
pub(crate) fn log_config_from_env() -> LogConfig {
    resolve(
        std::env::var("VHRN_LOG_FORMAT").ok().as_deref(),
        std::env::var_os("NO_COLOR").is_some(),
        std::io::stderr().is_terminal(),
    )
}

/// Install the global subscriber on stderr (stdout is the CLI's output channel). With
/// `RUST_LOG` unset the level defaults to info, so the always-on progress and warnings
/// still show; `RUST_LOG` overrides it. Errors only if a subscriber is already set.
pub(crate) fn init_tracing(config: &LogConfig) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let format = match config.format {
        LogFormat::Auto if std::io::stderr().is_terminal() => LogFormat::Pretty,
        LogFormat::Auto => LogFormat::Compact,
        other => other,
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(config.with_ansi)
        .with_target(false);
    // Each arm ends in try_init so the differing builder types unify to one Result.
    let res = match (format, config.with_time) {
        (LogFormat::Pretty, true) => builder.pretty().try_init(),
        (LogFormat::Pretty, false) => builder.pretty().without_time().try_init(),
        (_, true) => builder.compact().with_timer(SystemTime).try_init(),
        (_, false) => builder.compact().without_time().try_init(),
    };
    res.map_err(|e| anyhow::anyhow!("{e}"))
}

/// Read env config and install the subscriber once, before dispatch. Best-effort: a
/// failure (subscriber already set) just leaves diagnostics as no-ops.
pub fn init_logging() {
    let _ = init_tracing(&log_config_from_env());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_formats() {
        assert_eq!(LogFormat::parse("auto"), LogFormat::Auto);
        assert_eq!(LogFormat::parse("PRETTY"), LogFormat::Pretty);
        assert_eq!(LogFormat::parse(" compact "), LogFormat::Compact);
        assert_eq!(LogFormat::parse("nonsense"), LogFormat::Compact);
    }

    #[test]
    fn resolve_ansi_needs_tty_and_no_no_color() {
        assert!(resolve(None, false, true).with_ansi);
        assert!(!resolve(None, false, false).with_ansi);
        assert!(!resolve(None, true, true).with_ansi);
    }

    #[test]
    fn resolve_reads_format_and_never_times() {
        let c = resolve(Some("pretty"), false, false);
        assert_eq!(c.format, LogFormat::Pretty);
        assert!(!c.with_time);
        assert_eq!(resolve(None, false, false).format, LogFormat::Compact);
    }

    #[test]
    fn default_is_terse() {
        let d = LogConfig::default();
        assert_eq!(d.format, LogFormat::Compact);
        assert!(!d.with_time);
        assert!(d.with_ansi);
    }
}
