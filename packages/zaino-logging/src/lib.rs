#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]
#![deny(clippy::wildcard_enum_match_arm)]
// production code never panics: no indexing, unchecked arithmetic or panicking calls
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::string_slice,
        clippy::get_unwrap,
        clippy::let_underscore_must_use,
        let_underscore_drop
    )
)]

//! Entry points: [`init`] for binaries, [`try_init`] for tests.

use std::env;
use std::io::IsTerminal;
use std::sync::Once;

use time::macros::format_description;
use tracing::Level;
use tracing_subscriber::{
    EnvFilter,
    fmt::time::UtcTime,
    layer::SubscriberExt,
    util::{SubscriberInitExt, TryInitError},
};
use tracing_tree::HierarchicalLayer;

/// Time format for logs: HH:MM:SS.subsec (compact, no date)
const TIME_FORMAT: &[time::format_description::FormatItem<'static>] =
    format_description!("[hour]:[minute]:[second].[subsecond digits:3]");

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum LogFormat {
    /// Hierarchical tree view showing span nesting.
    Tree,
    /// Flat chronological stream (default).
    #[default]
    Stream,
    /// Machine-parseable JSON.
    Json,
}

impl LogFormat {
    /// Parse from string (case-insensitive).
    fn parse_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "tree" => Some(LogFormat::Tree),
            "stream" => Some(LogFormat::Stream),
            "json" => Some(LogFormat::Json),
            _ => None,
        }
    }
}

/// Why logging was not initialised.
#[derive(Debug)]
pub enum LogInitError {
    /// An environment setting holds a value it does not take.
    Setting {
        /// The variable.
        variable: &'static str,
        /// What it holds.
        value: String,
        /// What it takes.
        expected: &'static str,
    },
    /// `RUST_LOG` does not parse as a filter.
    Filter(String),
    /// A global subscriber is already set.
    Install(TryInitError),
}

impl std::fmt::Display for LogInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Setting {
                variable,
                value,
                expected,
            } => write!(f, "{variable}={value:?} is not one of: {expected}"),
            Self::Filter(error) => write!(f, "RUST_LOG is not a log filter: {error}"),
            Self::Install(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for LogInitError {}

/// `variable`'s value through `parse`; `default` where it is not set. A
/// value `parse` refuses is an error, never the default.
fn setting<T>(
    variable: &'static str,
    held: Result<String, env::VarError>,
    expected: &'static str,
    default: T,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<T, LogInitError> {
    let refused = |value: String| LogInitError::Setting {
        variable,
        value,
        expected,
    };
    match held {
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(value)) => Err(refused(value.to_string_lossy().into_owned())),
        Ok(value) => parse(&value).ok_or_else(|| refused(value)),
    }
}

/// `1`/`true`/`yes`/`on` or `0`/`false`/`no`/`off`, in any case.
fn switch(value: &str) -> Option<bool> {
    match value.to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Logging configuration, read from the environment.
#[derive(Debug, Clone)]
struct LogConfig {
    /// Output format (tree, stream, or json).
    format: LogFormat,
    /// Enable ANSI colors.
    color: bool,
    /// Show each event's source `file:line`. Off by default — noise for general
    /// log watching; opt in with `ZAINOLOG_LOCATION` when debugging.
    location: bool,
    /// Default log level.
    level: Level,
}

impl LogConfig {
    /// The configuration the environment asks for; a setting holding a value
    /// it does not take is an error.
    fn from_env() -> Result<Self, LogInitError> {
        let format = setting(
            "ZAINOLOG_FORMAT",
            env::var("ZAINOLOG_FORMAT"),
            "tree, stream, json",
            LogFormat::default(),
            LogFormat::parse_str,
        )?;
        // on unless set otherwise; `auto` follows the terminal
        let color = setting(
            "ZAINOLOG_COLOR",
            env::var("ZAINOLOG_COLOR"),
            "1, true, yes, on, 0, false, no, off, auto",
            true,
            |value| match value.to_lowercase().as_str() {
                "auto" => Some(std::io::stderr().is_terminal()),
                _ => switch(value),
            },
        )?;
        // Source `file:line` on every line is debugging detail: off unless
        // `ZAINOLOG_LOCATION` opts in.
        let location = setting(
            "ZAINOLOG_LOCATION",
            env::var("ZAINOLOG_LOCATION"),
            "1, true, yes, on, 0, false, no, off",
            false,
            switch,
        )?;
        Ok(Self {
            format,
            color,
            location,
            level: Level::INFO,
        })
    }
}

/// Initialise logging, configured from the environment (see the [crate
/// docs](crate)). Installs the global subscriber and the panic hook; fails
/// if a setting or `RUST_LOG` holds a value it does not take, or a global
/// tracing subscriber has already been set.
pub fn init() -> Result<(), LogInitError> {
    try_install(LogConfig::from_env()?)?;
    install_panic_logger();
    Ok(())
}

/// Idempotent variant of [`init`] for tests, where several test functions may
/// each try to initialise: a subscriber already set is kept. A setting that
/// does not parse is reported on stderr, and nothing is installed.
pub fn try_init() {
    match LogConfig::from_env().and_then(try_install) {
        Ok(()) => {}
        // already initialised: the first subscriber stays
        Err(LogInitError::Install(_)) => {}
        Err(error @ (LogInitError::Setting { .. } | LogInitError::Filter(_))) => {
            eprintln!("logging not initialised: {error}");
        }
    }
    install_panic_logger();
}

/// Installed at most once, regardless of how many times logging is initialized.
static PANIC_LOGGER: Once = Once::new();

/// Install the panic hook that routes panics through `tracing`. Idempotent —
/// installed at most once. See the [crate docs](crate) for the panic-at-origin
/// rationale.
///
/// The hook logs each panic as a structured `error` event under the `panic`
/// target (thread, location, message), then chains to the hook already
/// installed, so the default stderr backtrace is unchanged.
fn install_panic_logger() {
    PANIC_LOGGER.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_owned());
            let location = info
                .location()
                .map(std::string::ToString::to_string)
                .unwrap_or_else(|| "<unknown>".to_owned());
            let thread = std::thread::current()
                .name()
                .unwrap_or("<unnamed>")
                .to_owned();
            tracing::error!(target: "panic", %thread, %location, %message, "panic");
            previous(info);
        }));
    });
}

/// Build the subscriber described by `config` and install it as the global
/// default, erroring if one is already set.
fn try_install(config: LogConfig) -> Result<(), LogInitError> {
    // If RUST_LOG is set, use it directly. Otherwise, default to zaino crates only.
    // Users can set RUST_LOG=info to see all crates including zebra.
    let env_filter = match env::var("RUST_LOG") {
        Ok(filter) => {
            EnvFilter::try_new(filter).map_err(|error| LogInitError::Filter(error.to_string()))?
        }
        Err(env::VarError::NotPresent) => EnvFilter::new(format!(
            "zaino={level},zainod={level},zainodlib={level}",
            level = config.level.as_str()
        )),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(LogInitError::Filter("not Unicode".to_owned()));
        }
    };
    // Panics are routed through `tracing` under the `panic` target by
    // `install_panic_logger`. That target is outside the `zaino*` namespace, so
    // neither the default filter nor a typical `RUST_LOG=zaino=…` enables it — the
    // structured panic-at-origin event would be silently dropped (visible only as
    // the default hook's raw stderr backtrace, and absent from the JSON sink).
    // Ensure the `panic` target is enabled at ERROR so panic visibility is
    // structural, not contingent on the operator's filter.
    let env_filter = match "panic=error".parse() {
        Ok(directive) => env_filter.add_directive(directive),
        // a static directive: never refused
        Err(_) => env_filter,
    };
    let registry = tracing_subscriber::registry().with(env_filter);

    let installed = match config.format {
        LogFormat::Tree => registry
            .with(
                HierarchicalLayer::new(2)
                    .with_ansi(config.color)
                    .with_targets(true)
                    .with_bracketed_fields(true)
                    .with_indent_lines(true)
                    .with_thread_ids(false)
                    .with_thread_names(false)
                    .with_deferred_spans(true) // Only show spans when they have events
                    .with_verbose_entry(false) // Don't repeat span info on entry
                    .with_verbose_exit(false), // Don't repeat span info on exit
            )
            .try_init(),
        LogFormat::Stream => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_timer(UtcTime::new(TIME_FORMAT))
                    .with_target(true)
                    .with_ansi(config.color)
                    .pretty()
                    // Source location is opt-in (`ZAINOLOG_LOCATION`) — off for
                    // general watching, on for debugging.
                    .with_file(config.location)
                    .with_line_number(config.location),
            )
            .try_init(),
        LogFormat::Json => registry
            .with(
                // JSON format keeps full RFC3339 timestamps for machine parsing
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_timer(UtcTime::rfc_3339())
                    .with_target(true)
                    .with_file(config.location)
                    .with_line_number(config.location),
            )
            .try_init(),
    };
    installed.map_err(LogInitError::Install)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A setting that is not set takes its default; one holding a value it
    /// does not take is an error naming it, never the default.
    #[test]
    fn a_setting_is_its_value_its_default_or_an_error() {
        let read = |held| {
            setting(
                "ZAINOLOG_FORMAT",
                held,
                "tree, stream, json",
                LogFormat::Stream,
                LogFormat::parse_str,
            )
        };
        assert_eq!(
            read(Err(env::VarError::NotPresent)).expect("default"),
            LogFormat::Stream
        );
        assert_eq!(
            read(Ok("JSON".to_owned())).expect("parses"),
            LogFormat::Json
        );
        let refused = read(Ok("jsno".to_owned())).expect_err("refused");
        assert_eq!(
            refused.to_string(),
            "ZAINOLOG_FORMAT=\"jsno\" is not one of: tree, stream, json"
        );
        assert_eq!(switch("ON"), Some(true));
        assert_eq!(switch("no"), Some(false));
        assert_eq!(switch("maybe"), None);
    }

    #[test]
    fn panic_logger_installs_idempotently_and_still_unwinds() {
        // Idempotent (Once), and it only *adds* structured logging — a panic
        // still unwinds and is catchable, the hook does not swallow it.
        install_panic_logger();
        install_panic_logger();
        let caught = std::panic::catch_unwind(|| panic!("boom"));
        assert!(caught.is_err(), "panic still propagates through the hook");
    }

    #[test]
    fn test_log_format_from_str() {
        assert_eq!(LogFormat::parse_str("tree"), Some(LogFormat::Tree));
        assert_eq!(LogFormat::parse_str("TREE"), Some(LogFormat::Tree));
        assert_eq!(LogFormat::parse_str("stream"), Some(LogFormat::Stream));
        assert_eq!(LogFormat::parse_str("json"), Some(LogFormat::Json));
        assert_eq!(LogFormat::parse_str("unknown"), None);
    }
}
