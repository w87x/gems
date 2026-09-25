//! A minimal leveled logger to stderr — hand-rolled rather than pulling in
//! `log`/`tracing` and a subscriber, consistent with this workspace's
//! "avoid third-party crates" rule. Meant for the network-facing services
//! (`gems-webui`, `gems-mcp`) where an operator needs leveled, timestamped
//! diagnostic output distinct from a CLI tool's actual stdout protocol
//! (`gems-cli`'s `println!` output *is* its API for piping/scripting, so
//! it deliberately does not go through this module).
//!
//! Level is read once from the `GEMS_LOG` environment variable (`error`,
//! `warn`, `info`, or `debug`, case-insensitive; unset or unrecognized
//! defaults to `info`) and cached — a service reads its log level once at
//! startup, not on every log call.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    fn from_env_str(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static LEVEL_INIT: OnceLock<()> = OnceLock::new();

fn init_level_from_env() {
    LEVEL_INIT.get_or_init(|| {
        if let Ok(raw) = std::env::var("GEMS_LOG") {
            if let Some(level) = Level::from_env_str(&raw) {
                LEVEL.store(level as u8, Ordering::Relaxed);
            }
        }
    });
}

fn current_level() -> Level {
    match LEVEL.load(Ordering::Relaxed) {
        0 => Level::Error,
        1 => Level::Warn,
        2 => Level::Info,
        _ => Level::Debug,
    }
}

/// Logs `message` at `level` to stderr as `<unix_epoch_secs> <LEVEL> <target> <message>`,
/// if `level` is at or below the configured `GEMS_LOG` threshold. `target`
/// is a short component name (e.g. `"gems-webui"`) so multi-component log
/// output stays attributable.
pub fn log(level: Level, target: &str, message: &str) {
    init_level_from_env();
    if level > current_level() {
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    eprintln!("{now} {:<5} {target} {message}", level.as_str());
}

#[macro_export]
macro_rules! log_error {
    ($target:expr, $($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Error, $target, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($target:expr, $($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Warn, $target, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($target:expr, $($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Info, $target, &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($target:expr, $($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Debug, $target, &format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_str_parses_known_levels_case_insensitively() {
        assert_eq!(Level::from_env_str("error"), Some(Level::Error));
        assert_eq!(Level::from_env_str("WARN"), Some(Level::Warn));
        assert_eq!(Level::from_env_str("Warning"), Some(Level::Warn));
        assert_eq!(Level::from_env_str("Info"), Some(Level::Info));
        assert_eq!(Level::from_env_str("debug"), Some(Level::Debug));
        assert_eq!(Level::from_env_str("bogus"), None);
    }

    #[test]
    fn levels_order_error_as_most_severe() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
    }

    #[test]
    fn log_does_not_panic_at_any_level() {
        // Can't easily assert on stderr content from a unit test, but this
        // at least proves the formatting path (timestamp, padding, macro
        // expansion) never panics for a representative message.
        log(Level::Error, "test", "something failed");
        log(Level::Debug, "test", "verbose detail");
        log_info!("test", "formatted {}", 42);
    }
}
