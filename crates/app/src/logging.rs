//! The `tracing` sink.
//!
//! # Why this is new
//!
//! The workspace has ~230 `tracing::` call sites and, until this
//! module, no subscriber. `tracing` records nothing without one, so
//! every `warn!` and `error!` in the codebase was a no-op that went
//! nowhere — including the ones written specifically so a failure
//! would be observable rather than silent. DESIGN.md §5.5 has always
//! specified this behaviour ("on when `--verbose` or `CODEPP_LOG=info`
//! is set"); it had simply never been implemented.
//!
//! # Cost when off
//!
//! No subscriber is installed unless asked for, which is what DESIGN.md
//! §5.5 means by "zero cost" in release: with no global subscriber,
//! each call site's `enabled()` check is an atomic load of a null
//! dispatcher and returns immediately, so no formatting, allocation,
//! or I/O happens. Installing one costs the filter parse plus a writer,
//! which is why it stays behind a flag rather than being on by default
//! — DESIGN.md §8 budgets 80 ms to first frame.

use std::io::IsTerminal;

use tracing_subscriber::filter::EnvFilter;

/// Environment variable naming the log filter, per DESIGN.md §5.5.
const LOG_ENV: &str = "CODEPP_LOG";

/// Install the sink if anything asks for it.
///
/// Asked for by `--verbose`/`--perf` (which pass `verbose = true`) or
/// by `CODEPP_LOG` being set to a non-empty value. `CODEPP_LOG` wins
/// when both are present, so a developer can narrow to one crate
/// without also having to drop the flag.
///
/// Returns whether a subscriber was installed, so `main` can warn if
/// `--perf` was asked for but the sink refused — otherwise the user
/// gets silence and no explanation for it.
pub fn init(verbose: bool) -> bool {
    // Matched rather than `.ok()`d, because `VarError` has two
    // variants and collapsing them makes one of them silent. A
    // `CODEPP_LOG` that is *set* but not valid UTF-8 — trivial to
    // produce on Linux — would otherwise be indistinguishable from
    // never having been set: logging the user asked for would simply
    // not happen, with no diagnostic, which is precisely the failure
    // this function's malformed-directive path exists to avoid.
    let env = match std::env::var(LOG_ENV) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        // Set to empty or whitespace: treat as unset, no complaint.
        // Scripts commonly clear a variable that way.
        Ok(_) | Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(raw)) => {
            // `{raw:?}` for the same reason as below: this is an
            // environment value going to a terminal, and `Debug`
            // escapes control bytes where `Display` does not.
            //
            // `clippy::unnecessary_debug_formatting` asks for
            // `raw.display()` here, and following it would reintroduce
            // the exact injection this escaping exists to stop — the
            // lint's own note says "escaped characters will no longer
            // be escaped". It is a readability lint that has no way to
            // know this value is attacker-influenced and terminal-bound,
            // so it is suppressed at the narrowest possible scope.
            #[allow(clippy::unnecessary_debug_formatting)]
            {
                eprintln!("Code++: ignoring {LOG_ENV}={raw:?}: not valid UTF-8.");
            }
            None
        }
    };
    let directives = match (env, verbose) {
        (Some(env), _) => env,
        (None, true) => "info".to_string(),
        // Nothing asked for logging. Leave the global dispatcher unset
        // so every call site stays a no-op.
        (None, false) => return false,
    };

    let filter = match EnvFilter::try_new(&directives) {
        Ok(f) => f,
        Err(err) => {
            // A malformed `CODEPP_LOG` must not take the editor down,
            // and must not silently behave as though it were valid.
            //
            // Both interpolated values are attacker-influenceable and
            // terminal-bound, so both are escaped rather than passed to
            // `Display`, which lets control bytes through — a value
            // carrying an xterm OSC escape could retitle the terminal
            // or overwrite the line above via CR. `directives` uses
            // `{:?}` unconditionally. `err` comes from the pinned
            // `tracing-subscriber`, which only ever produces fixed
            // strings here (checked by feeding it directives carrying
            // an OSC sequence and confirming none reached the message),
            // so it is escaped only when it actually contains a control
            // character — that keeps an ordinary typo readable while
            // not resting the safety on a dependency's behaviour at one
            // pinned version.
            let err = err.to_string();
            let err = if err.chars().any(char::is_control) {
                err.escape_debug().to_string()
            } else {
                err
            };
            eprintln!(
                "Code++: ignoring {LOG_ENV}={directives:?}: {err}\n\
                 Code++: falling back to `info`."
            );
            EnvFilter::new("info")
        }
    };

    // Colour only when stderr is a terminal. The same output is
    // routinely piped into a file or a CI log, where escape sequences
    // are noise rather than emphasis.
    let ansi = std::io::stderr().is_terminal();
    let built = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .with_target(true)
        .try_init();

    match built {
        Ok(()) => true,
        Err(err) => {
            // Only reachable if something else already installed a
            // global subscriber. Nothing does today; report rather
            // than assume.
            eprintln!("Code++: could not install the log subscriber: {err}");
            false
        }
    }
}
