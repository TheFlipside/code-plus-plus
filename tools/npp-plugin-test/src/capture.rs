//! Catching what the host logs while a plugin runs.
//!
//! The dispatcher answers a message it does not handle with 0 and a
//! `tracing` warning, `unhandled NPPM_*` with the number in its `msg`
//! field; that warning is the only place the fact is recorded, so the
//! harness listens for it. Every other warning or error the host logs
//! during a run — a refused dock registration, a panic caught at a
//! plugin boundary — goes into the report too.

use std::sync::{Mutex, MutexGuard, PoisonError};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::{EnvFilter, Registry};

/// The dispatcher's message for a message it does not handle.
pub const UNHANDLED_MESSAGE: &str = "unhandled NPPM_*";

/// The most events kept. A plugin that sends an unhandled message in a
/// loop must not grow the harness without bound.
pub const MAX_EVENTS: usize = 100_000;

/// The most characters kept of one message or field. A field can carry
/// a plugin's own text, and the event count alone does not bound bytes.
pub const MAX_FIELD_CHARS: usize = 1000;

/// One captured warning or error.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Captured {
    /// The event's message.
    pub message: String,
    /// Its other fields, as `name=value`.
    pub fields: Vec<(String, String)>,
}

impl Captured {
    /// The message number of an `unhandled NPPM_*` warning.
    #[must_use]
    pub fn unhandled_message(&self) -> Option<u32> {
        if self.message != UNHANDLED_MESSAGE {
            return None;
        }
        self.fields
            .iter()
            .find(|(name, _)| name == "msg")
            .and_then(|(_, value)| value.parse().ok())
    }

    /// The message with its fields, as one line.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = self.message.clone();
        for (name, value) in &self.fields {
            text.push(' ');
            text.push_str(name);
            text.push('=');
            text.push_str(value);
        }
        text
    }
}

/// What the layer caught. Process-wide, as the subscriber is: a run is
/// the whole process, so [`take`] drains everything it logged. A test
/// binary shares it between tests; the tests here that capture take a
/// lock of their own, so one never drains another's events.
static EVENTS: Mutex<Vec<Captured>> = Mutex::new(Vec::new());

fn events() -> MutexGuard<'static, Vec<Captured>> {
    // A panic while holding the lock leaves a whole `Vec`; keep it.
    EVENTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Take every event captured so far.
#[must_use]
pub fn take() -> Vec<Captured> {
    std::mem::take(&mut *events())
}

/// The layer that captures warnings and errors.
pub struct CaptureLayer;

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // `Level` orders by verbosity: WARN and ERROR are the two
        // at or below WARN.
        if *event.metadata().level() > Level::WARN {
            return;
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let mut events = events();
        if events.len() < MAX_EVENTS {
            events.push(visitor.captured);
        }
    }
}

#[derive(Default)]
struct FieldVisitor {
    captured: Captured,
}

impl FieldVisitor {
    fn keep(&mut self, field: &Field, value: String) {
        let value = match value.char_indices().nth(MAX_FIELD_CHARS) {
            Some((end, _)) => format!("{}...", &value[..end]),
            None => value,
        };
        if field.name() == "message" {
            self.captured.message = value;
        } else {
            self.captured.fields.push((field.name().to_string(), value));
        }
    }
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.keep(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        // Without this, a `&str` field would arrive through
        // `record_debug` already quoted.
        self.keep(field, value.to_string());
    }
}

/// Install the capture as the process's subscriber, with the host's log
/// on stderr as well when `verbose` (filtered by `CODEPP_LOG`, `info`
/// by default). Does nothing if a subscriber is already installed.
pub fn install(verbose: bool) {
    let log = verbose.then(|| {
        let filter =
            EnvFilter::try_from_env("CODEPP_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_filter(filter)
    });
    // Filtered here as well as in `on_event`, so the host's `debug!` and
    // `trace!` call sites stay disabled rather than built and dropped.
    let subscriber = Registry::default()
        .with(CaptureLayer.with_filter(LevelFilter::WARN))
        .with(log);
    let _ = tracing::subscriber::set_global_default(subscriber);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Held by every test that captures, since they share [`EVENTS`].
    static CAPTURING: Mutex<()> = Mutex::new(());

    #[test]
    fn the_dispatcher_still_logs_what_the_harness_listens_for() {
        // The `unhandled:` lines rest on this exact call. Were its text
        // or its field renamed, every run would report full coverage.
        let dispatcher = include_str!("../../../crates/plugin-host/src/dispatch.rs");
        let call = format!("tracing::warn!(msg = msg, \"{UNHANDLED_MESSAGE}\");");
        assert_eq!(
            dispatcher.matches(&call).count(),
            1,
            "the dispatcher's unhandled-message warning changed: {call}"
        );
    }

    #[test]
    fn long_fields_are_cut() {
        let _serial = CAPTURING.lock().unwrap_or_else(PoisonError::into_inner);
        let subscriber = Registry::default().with(CaptureLayer);
        let long = "x".repeat(MAX_FIELD_CHARS + 50);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(blob = long.as_str(), "long field test");
        });
        let ours: Vec<_> = take()
            .into_iter()
            .filter(|e| e.message == "long field test")
            .collect();
        assert_eq!(ours.len(), 1);
        assert_eq!(ours[0].fields[0].1.chars().count(), MAX_FIELD_CHARS + 3);
    }

    #[test]
    fn the_dispatchers_warning_is_recognised() {
        let event = Captured {
            message: UNHANDLED_MESSAGE.to_string(),
            fields: vec![("msg".to_string(), "4025".to_string())],
        };
        assert_eq!(event.unhandled_message(), Some(4025));
        assert_eq!(event.text(), "unhandled NPPM_* msg=4025");
        let other = Captured {
            message: "something else".to_string(),
            fields: vec![("msg".to_string(), "4025".to_string())],
        };
        assert_eq!(other.unhandled_message(), None);
    }

    #[test]
    fn events_are_captured_through_a_scoped_subscriber() {
        let _serial = CAPTURING.lock().unwrap_or_else(PoisonError::into_inner);
        let subscriber = Registry::default().with(CaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("not kept");
            tracing::warn!(msg = 4025u32, "unhandled NPPM_*");
            tracing::error!(path = "x.dll", "plugin failed");
        });
        let captured = take();
        // Other tests in this binary may log through the same global
        // store; keep only what this one emitted.
        let ours: Vec<_> = captured
            .iter()
            .filter(|e| e.message == UNHANDLED_MESSAGE || e.message == "plugin failed")
            .collect();
        assert_eq!(ours.len(), 2, "{captured:?}");
        assert_eq!(ours[0].unhandled_message(), Some(4025));
        assert_eq!(ours[1].text(), "plugin failed path=x.dll");
        assert!(!captured.iter().any(|e| e.message == "not kept"));
    }
}
