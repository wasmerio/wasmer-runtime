//! Utilities to set up tracing and logging.
//!
//! By default, wasmer does not generate any tracing output. To
//! enable tracing, one must call the `wasmer_setup_tracing()`
//! function during the initialization stage of their program.
//!
//! Tracing levels can be enabled/disabled using the [RUST_LOG
//! env var](https://docs.rs/env_logger/latest/env_logger/#enabling-logging).
//!
//! A Rust crate that extends this C API can raise the verbosity of its
//! own log targets along with the engine's: with the `extensions`
//! feature, it calls `setup_tracing_with_targets()` from its own setup
//! function instead of `wasmer_setup_tracing()`.
//!
//! # Example
//!
//! ```rust
//! # use inline_c::assert_c;
//! # fn main() {
//! #    (assert_c! {
//! # #include "tests/wasmer.h"
//! #
//! int main() {
//!     // This can go up to 4, which is the most verbose
//!     int verbosity_level = 0;
//!     // Whether to use colors when logging information
//!     int use_colors = 1;
//!     wasmer_setup_tracing(verbosity_level, use_colors);
//!
//!     return 0;
//! }
//! #    })
//! #    .success();
//! # }

use std::ffi::c_int;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::{
    EnvFilter, filter::Directive, fmt, layer::SubscriberExt, util::SubscriberInitExt,
};

const WHITELISTED_LOG_TARGETS: &[&str] = &["wasmer"];

#[unsafe(no_mangle)]
pub extern "C" fn wasmer_setup_tracing(verbosity_level: c_int, use_color: c_int) {
    install_tracing(verbosity_level, use_color, &[]);
}

/// Sets up tracing like `wasmer_setup_tracing()`, and also raises the
/// verbosity of `extra_targets` to the level of the engine's own targets.
///
/// This is for a product that adds crates of its own on top of this C API,
/// such as a WASI implementation: without it, their events stay one level
/// quieter than the engine's. It installs the global subscriber, as
/// `wasmer_setup_tracing()` does, so a process can call only one of the two,
/// and only once.
///
/// Each target is the target part of a
/// [`RUST_LOG` directive](tracing_subscriber::EnvFilter#directives), usually
/// a crate name. It covers every target that starts with it, so `my_wasi`
/// also covers `my_wasi::fs`. A string that is not a valid target is reported
/// on stderr and ignored, the same way an invalid directive in `RUST_LOG` is,
/// so that a bad target cannot prevent tracing from being set up.
///
/// # Example
///
/// ```rust
/// use std::ffi::c_int;
/// use wasmer::tracing::setup_tracing_with_targets;
///
/// pub extern "C" fn my_product_setup_tracing(verbosity_level: c_int, use_color: c_int) {
///     setup_tracing_with_targets(verbosity_level, use_color, &["my_wasi", "my_fs"]);
/// }
/// # my_product_setup_tracing(2, 0);
/// ```
#[cfg(feature = "extensions")]
pub fn setup_tracing_with_targets(
    verbosity_level: c_int,
    use_color: c_int,
    extra_targets: &[&str],
) {
    install_tracing(verbosity_level, use_color, extra_targets);
}

/// Installs the global subscriber, which panics the second time around.
fn install_tracing(verbosity_level: c_int, use_color: c_int, extra_targets: &[&str]) {
    let fmt_layer = fmt::layer()
        .with_target(true)
        .with_span_events(fmt::format::FmtSpan::CLOSE)
        .with_ansi(use_color > 0)
        .with_thread_ids(true)
        .with_writer(std::io::stderr);

    tracing_subscriber::registry()
        .with(env_filter(verbosity_level, extra_targets))
        .with(fmt_layer.compact().with_target(true))
        .init();
}

fn env_filter(verbosity_level: c_int, extra_targets: &[&str]) -> EnvFilter {
    let rust_log = std::env::var(EnvFilter::DEFAULT_ENV).unwrap_or_default();
    filter_from_directives(&rust_log, verbosity_level, extra_targets)
}

/// Builds the filter from the value of `RUST_LOG`, which is passed in so that
/// the result does not depend on the environment of the process.
fn filter_from_directives(
    rust_log: &str,
    verbosity_level: c_int,
    extra_targets: &[&str],
) -> EnvFilter {
    // First, we set up the default log level.
    let filter = EnvFilter::builder()
        .with_default_directive(default_level(verbosity_level).into())
        .parse_lossy(rust_log);

    // Next we add level-specific directives, where verbosity=0 means don't
    // override anything.
    let Some(level) = raised_level(verbosity_level) else {
        return filter;
    };
    WHITELISTED_LOG_TARGETS
        .iter()
        .chain(extra_targets)
        .filter_map(|target| target_directive(target, level))
        .fold(filter, EnvFilter::add_directive)
}

fn default_level(verbosity_level: c_int) -> LevelFilter {
    let default_filters = [
        LevelFilter::OFF,
        LevelFilter::WARN,
        LevelFilter::INFO,
        LevelFilter::DEBUG,
    ];
    default_filters
        .get(verbosity_level as usize)
        .copied()
        .unwrap_or(LevelFilter::TRACE)
}

/// The level of the targets that get their own directive. Note that these are
/// shifted one level up from the default so we'll get something like
/// RUST_LOG="warn,wasmer=info"
fn raised_level(verbosity_level: c_int) -> Option<LevelFilter> {
    if verbosity_level <= 0 {
        return None;
    }
    let specific_filters = [LevelFilter::WARN, LevelFilter::INFO, LevelFilter::DEBUG];
    let level = specific_filters
        .get(verbosity_level as usize)
        .copied()
        .unwrap_or(LevelFilter::TRACE);
    Some(level)
}

fn target_directive(target: &str, level: LevelFilter) -> Option<Directive> {
    match format!("{target}={level}").parse() {
        Ok(directive) => Some(directive),
        Err(error) => {
            // The extra targets come from the caller, so unlike the constant
            // ones they may not parse. Report them the way tracing-subscriber
            // reports an invalid directive in RUST_LOG.
            eprintln!("ignoring log target `{target}`: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::filter_from_directives;
    use std::{
        ffi::c_int,
        sync::{Arc, Mutex},
    };
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::{
        Layer,
        layer::{Context, SubscriberExt},
    };

    /// The verbosity, the level it raises the engine's targets to, and the
    /// level it leaves every other target at.
    const VERBOSITY_LEVELS: [(c_int, Option<Level>, Option<Level>); 5] = [
        (0, None, None),
        (1, Some(Level::INFO), Some(Level::WARN)),
        (2, Some(Level::DEBUG), Some(Level::INFO)),
        (3, Some(Level::TRACE), Some(Level::DEBUG)),
        (4, Some(Level::TRACE), Some(Level::TRACE)),
    ];

    /// The most verbose level at which each target got an event through.
    #[derive(Debug, PartialEq)]
    struct Logged {
        engine: Option<Level>,
        embedder: Option<Level>,
        unlisted: Option<Level>,
    }

    /// Records the target and the level of every event the filter lets through.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<(String, Level)>>>);

    impl<S: Subscriber> Layer<S> for Recorder {
        fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
            let metadata = event.metadata();
            let record = (metadata.target().to_owned(), *metadata.level());
            self.0.lock().unwrap().push(record);
        }
    }

    impl Recorder {
        fn most_verbose(&self, target: &str) -> Option<Level> {
            let records = self.0.lock().unwrap();
            let levels = records.iter().filter(|(logged, _)| logged == target);
            levels.map(|(_, level)| *level).max()
        }
    }

    macro_rules! emit_at_every_level {
        ($($target:literal),*) => {$(
            tracing::error!(target: $target, "event");
            tracing::warn!(target: $target, "event");
            tracing::info!(target: $target, "event");
            tracing::debug!(target: $target, "event");
            tracing::trace!(target: $target, "event");
        )*};
    }

    /// Logs through a subscriber scoped to this call, because the global one
    /// can only be installed once per process.
    fn log(rust_log: &str, verbosity_level: c_int, extra_targets: &[&str]) -> Logged {
        let recorder = Recorder::default();
        let filter = filter_from_directives(rust_log, verbosity_level, extra_targets);
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(recorder.clone());
        tracing::subscriber::with_default(subscriber, || {
            emit_at_every_level!("wasmer::engine", "embedder_wasi::fs", "unlisted");
        });
        Logged {
            engine: recorder.most_verbose("wasmer::engine"),
            embedder: recorder.most_verbose("embedder_wasi::fs"),
            unlisted: recorder.most_verbose("unlisted"),
        }
    }

    #[test]
    fn extra_targets_are_raised_to_the_level_of_the_engine() {
        for (verbosity_level, raised, default) in VERBOSITY_LEVELS {
            let logged = log("", verbosity_level, &["embedder_wasi"]);

            let expected = Logged {
                engine: raised,
                embedder: raised,
                unlisted: default,
            };
            assert_eq!(logged, expected, "verbosity {verbosity_level}");
        }
    }

    #[test]
    fn only_the_engine_is_raised_without_extra_targets() {
        for (verbosity_level, raised, default) in VERBOSITY_LEVELS {
            let logged = log("", verbosity_level, &[]);

            let expected = Logged {
                engine: raised,
                embedder: default,
                unlisted: default,
            };
            assert_eq!(logged, expected, "verbosity {verbosity_level}");
        }
    }

    #[test]
    fn rust_log_applies_to_the_targets_that_are_not_raised() {
        let rust_log = "wasmer=error,embedder_wasi=error,unlisted=trace";

        let logged = log(rust_log, 2, &[]);
        let expected = Logged {
            engine: Some(Level::DEBUG),
            embedder: Some(Level::ERROR),
            unlisted: Some(Level::TRACE),
        };
        assert_eq!(logged, expected);

        let logged = log(rust_log, 2, &["embedder_wasi"]);
        let expected = Logged {
            engine: Some(Level::DEBUG),
            embedder: Some(Level::DEBUG),
            unlisted: Some(Level::TRACE),
        };
        assert_eq!(logged, expected);
    }

    #[test]
    fn an_invalid_extra_target_is_ignored() {
        let logged = log("", 2, &["", "embedder=wasi", "embedder_wasi"]);

        let expected = Logged {
            engine: Some(Level::DEBUG),
            embedder: Some(Level::DEBUG),
            unlisted: Some(Level::INFO),
        };
        assert_eq!(logged, expected);
    }
}
