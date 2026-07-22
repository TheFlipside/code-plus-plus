//! The `--perf` instrumentation behind DESIGN.md §8's budgets.
//!
//! §8 sets two constraints that only a running binary can check —
//! **cold start under 80 ms** and **keystroke latency under 5 ms
//! p99** — and names `--perf` as how they get measured. §7.3 then
//! lists "cold-start time measured against the §8 budget" as a thing
//! re-tested at every phase boundary. The flag had never been
//! implemented, so both were unenforceable: Phase 5 m1's cold-start
//! figure came from ad-hoc instrumentation that was not kept, and
//! keystroke latency had never been measured on Linux at all.
//!
//! # Why the clock starts in `main`
//!
//! `main` takes an `Instant` as its very first statement — before
//! argument parsing, before the log sink, before any window exists —
//! and hands it to [`Perf::started_at`] once the flags are known.
//! ([`Perf::new`], which starts the clock itself, is the convenience
//! form and is used only by tests.)
//! Anything later would quietly exclude work that a user waiting for
//! the editor is nonetheless waiting through. It still cannot see
//! process spawn and dynamic linking, which happen before `main` is
//! entered — so the figure is a lower bound on what the user
//! experiences, and should not be compared against an out-of-process
//! measurement without saying so.
//!
//! # Why it is single-threaded
//!
//! Every call site is a UI-thread event handler: the first paint, a
//! key press, a redraw. `Cell`/`RefCell` rather than atomics keeps the
//! hot path — [`Perf::key_pressed`] and [`Perf::painted`], which run
//! per keystroke — free of synchronisation the budget cannot afford.
//!
//! The `Cell`s make `Perf` `!Sync` on their own, so it can never be
//! *shared* across threads. They do not make it `!Send` — every field
//! is individually `Send`, so without help `Perf` could still be
//! *moved* into a `thread::spawn` closure and quietly start reporting
//! another thread's timings. Both backends happen to wrap it in an
//! `Rc` immediately, which blocks that, but by convention rather than
//! by construction. [`Perf::_not_send`] closes it properly: a future
//! refactor that captures a bare `Perf` into a worker fails to compile
//! instead of silently producing the wrong numbers.

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::time::{Duration, Instant};

/// Percentiles reported for keystroke latency. p99 is the one §8
/// actually budgets; the others are context for reading it — a p50
/// far below a p99 says the tail is a stall rather than a uniformly
/// slow path.
const REPORTED_PERCENTILES: [f64; 4] = [50.0, 90.0, 99.0, 100.0];

/// Cap on keystrokes awaiting a paint.
///
/// Presses accumulate only while no paint arrives — an occluded or
/// minimised window, or a genuine stall. 256 unpainted keystrokes is
/// already far past any state worth measuring precisely, and the
/// samples already held describe it; past that, further presses are
/// counted as dropped rather than growing a buffer without bound.
const MAX_PENDING: usize = 256;

/// Cap on retained keystroke samples.
///
/// A long editing session is unbounded, and this is a diagnostic, not
/// a feature — it must not grow memory in a process whose §8 budget is
/// a memory floor. 100k samples is ~400 KB at 4 bytes each and far
/// more than any manual measurement run needs; past that the oldest
/// are dropped and [`Perf::report`] says so, rather than silently
/// reporting percentiles over a truncated window as though they
/// covered everything.
const MAX_SAMPLES: usize = 100_000;

/// Startup and input-latency measurements. One per process.
pub struct Perf {
    /// When `main` began. See the module docs.
    start: Instant,
    /// Whether `--perf` was passed. When false every method is an
    /// early return, so call sites need no `if` of their own.
    enabled: bool,
    /// Set once the first-paint measurement has been reported, so a
    /// backend can call [`Perf::mark_first_draw`] from every paint
    /// without special-casing the first.
    first_draw_done: Cell<bool>,
    /// Instant of the previous key press, for inter-arrival timing.
    last_press: Cell<Option<Instant>>,
    /// Gaps between consecutive key presses, in microseconds.
    ///
    /// Reported alongside latency because a latency distribution is
    /// close to meaningless without the input rate that produced it: a
    /// p99 of 100 ms at four characters per second is a stall, the
    /// same figure at forty is a queue. It also makes a synthetic
    /// input tool auditable — `xdotool` injects through XTEST and does
    /// not reliably preserve the spacing its `--delay` implies, so a
    /// measured tail may be describing the injector rather than the
    /// editor. Recording arrivals is the only way to tell.
    gaps: RefCell<Vec<u32>>,
    /// The most recent key press that has not yet been shown to have
    /// changed the document. Promoted to [`Self::pending`] by
    /// [`Perf::text_modified`], discarded if the next key press
    /// arrives first.
    candidate: Cell<Option<Instant>>,
    /// Timestamps of key presses that *did* change the document and
    /// are now waiting on the paint that shows the change.
    ///
    /// A `Vec`, not a single slot, because several keystrokes commonly
    /// arrive within one frame. See [`Perf::key_pressed`] for why each
    /// gets its own sample rather than being collapsed into one.
    pending: RefCell<Vec<Instant>>,
    /// Recorded keystroke→paint latencies, in microseconds.
    samples: RefCell<Vec<u32>>,
    /// How many samples were dropped once [`MAX_SAMPLES`] was reached.
    dropped: Cell<u64>,
    /// Makes `Perf` `!Send` as well as `!Sync`. See the module docs.
    /// A raw pointer is the standard marker for this and costs
    /// nothing at runtime — `PhantomData` is zero-sized.
    _not_send: PhantomData<*const ()>,
}

impl Perf {
    /// Start the clock now.
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self::started_at(Instant::now(), enabled)
    }

    /// Build from a clock started earlier.
    ///
    /// `main` needs this: the cold-start measurement has to begin
    /// before argument parsing, but whether it is wanted is only known
    /// *after* parsing. Taking the instant as a parameter keeps the
    /// measurement honest instead of quietly excluding the parse.
    #[must_use]
    pub fn started_at(start: Instant, enabled: bool) -> Self {
        Self {
            start,
            enabled,
            first_draw_done: Cell::new(false),
            last_press: Cell::new(None),
            gaps: RefCell::new(Vec::new()),
            candidate: Cell::new(None),
            pending: RefCell::new(Vec::new()),
            samples: RefCell::new(Vec::new()),
            dropped: Cell::new(0),
            _not_send: PhantomData,
        }
    }

    /// Whether measurements are being taken.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Report the cold-start figure. Idempotent — only the first call
    /// after construction measures anything, so backends can wire this
    /// into their ordinary paint handler rather than maintaining a
    /// "have I painted yet" flag of their own.
    pub fn mark_first_draw(&self) {
        if !self.enabled || self.first_draw_done.replace(true) {
            return;
        }
        let elapsed = self.start.elapsed();
        // `budget_ms` is emitted alongside so a reader does not have to
        // know §8 by heart to tell a pass from a fail.
        tracing::info!(
            target: "codepp::perf",
            ms = as_millis_f64(elapsed),
            budget_ms = 80.0,
            "cold start: main() to first draw"
        );
    }

    /// Record that a key was pressed. Provisional until
    /// [`Perf::text_modified`] confirms it changed something.
    ///
    /// **Nothing is measured from a press alone**, because plenty of
    /// keys change no text: Escape, arrows, Home/End, a Backspace at
    /// position 0, anything at all on a read-only buffer. Scintilla
    /// does not repaint for those, so a press committed here would
    /// wait in [`Self::pending`] until an unrelated later paint — the
    /// caret-blink timer will do — closed it with a fabricated latency
    /// running into hundreds of milliseconds, landing straight in p99.
    /// Requiring a modification first is what makes Tab, Enter and
    /// Backspace measurable at all: they edit the buffer, but only
    /// sometimes.
    ///
    /// A press that never modifies is simply overwritten by the next
    /// one and costs nothing.
    pub fn key_pressed(&self) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if let Some(prev) = self.last_press.replace(Some(now)) {
            let mut gaps = self.gaps.borrow_mut();
            if gaps.len() < MAX_SAMPLES {
                gaps.push(
                    u32::try_from(now.saturating_duration_since(prev).as_micros())
                        .unwrap_or(u32::MAX),
                );
            }
        }
        self.candidate.set(Some(now));
    }

    /// Record that the document's text changed, promoting the pending
    /// key press to a real measurement.
    ///
    /// A modification with no candidate — a plugin insert, a
    /// find-in-files replace, a file load — is ignored rather than
    /// timed from nothing.
    pub fn text_modified(&self) {
        if !self.enabled {
            return;
        }
        let Some(pressed) = self.candidate.take() else {
            return;
        };
        let mut pending = self.pending.borrow_mut();
        if pending.len() >= MAX_PENDING {
            self.dropped.set(self.dropped.get() + 1);
            return;
        }
        pending.push(pressed);
    }

    /// Record that a paint completed, closing off every keystroke
    /// waiting on it — one sample each, measured from that key's own
    /// press. A paint with nothing pending — a resize, an exposure, a
    /// scroll, or a key that changed no text — is ignored rather than
    /// recorded as a zero-latency sample, which would deflate every
    /// percentile.
    pub fn painted(&self) {
        if !self.enabled {
            return;
        }
        let mut pending = self.pending.borrow_mut();
        if pending.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut samples = self.samples.borrow_mut();
        for pressed in pending.drain(..) {
            if samples.len() >= MAX_SAMPLES {
                self.dropped.set(self.dropped.get() + 1);
                continue;
            }
            let micros = u32::try_from(now.saturating_duration_since(pressed).as_micros())
                .unwrap_or(u32::MAX);
            samples.push(micros);
        }
    }

    /// Log the keystroke-latency distribution and the input rate that
    /// produced it. Call at shutdown.
    ///
    /// Says so explicitly when there is nothing to report: a run that
    /// took no measurements and one whose measurements were all zero
    /// must not look alike.
    pub fn report(&self) {
        if !self.enabled {
            return;
        }
        let samples = self.samples.borrow();
        if samples.is_empty() {
            tracing::info!(
                target: "codepp::perf",
                "keystroke latency: no samples (nothing was typed, or nothing it typed changed the buffer)"
            );
        } else {
            let mut sorted = samples.clone();
            sorted.sort_unstable();
            for pct in REPORTED_PERCENTILES {
                let micros = percentile(&sorted, pct);
                tracing::info!(
                    target: "codepp::perf",
                    percentile = pct,
                    ms = f64::from(micros) / 1000.0,
                    samples = sorted.len(),
                    budget_ms = 5.0,
                    "keystroke latency: typed char to redraw"
                );
            }
        }

        // The input rate, so the distribution above can be read. Also
        // the audit trail on a synthetic input tool: if these gaps do
        // not match what the tool was asked for, the latency tail is
        // partly the tool's.
        let gaps = self.gaps.borrow();
        if !gaps.is_empty() {
            let mut sorted = gaps.clone();
            sorted.sort_unstable();
            for pct in REPORTED_PERCENTILES {
                let micros = percentile(&sorted, pct);
                tracing::info!(
                    target: "codepp::perf",
                    percentile = pct,
                    ms = f64::from(micros) / 1000.0,
                    samples = sorted.len(),
                    "key arrival: gap since the previous keystroke"
                );
            }
        }

        let dropped = self.dropped.get();
        if dropped > 0 {
            tracing::warn!(
                target: "codepp::perf",
                dropped,
                cap = MAX_SAMPLES,
                "keystroke samples were dropped; percentiles cover only the first {MAX_SAMPLES}"
            );
        }
    }
}

/// Milliseconds as a float, for logging. `as_secs_f64` is exact enough
/// here and avoids the integer truncation `as_millis` would apply to a
/// sub-millisecond duration.
fn as_millis_f64(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Nearest-rank percentile of an ascending slice.
///
/// Nearest-rank rather than interpolating: it always returns a value
/// that was actually observed, which is what a latency budget wants —
/// an interpolated "p99" can name a duration no keystroke ever took.
/// `p100` is the maximum. Panic-free for any `pct` in 0..=100 given a
/// non-empty slice.
fn percentile(sorted: &[u32], pct: f64) -> u32 {
    debug_assert!(!sorted.is_empty(), "caller checks for empty");
    if sorted.is_empty() {
        return 0;
    }
    // ceil(pct/100 * n), clamped to 1..=n, then to a 0-based index.
    // Both suppressions are bounded by construction: `sorted.len()` is
    // at most `MAX_SAMPLES` (100_000), far inside the range `f64`
    // represents exactly, so the widening loses nothing; and `pct` only
    // ever comes from `REPORTED_PERCENTILES`, so the product is
    // non-negative and `ceil()` of it cannot be negative or exceed `n`.
    #[allow(clippy::cast_precision_loss)]
    let n = sorted.len() as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rank = ((pct / 100.0 * n).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::{percentile, Perf};

    #[test]
    fn disabled_perf_records_nothing() {
        let perf = Perf::new(false);
        perf.key_pressed();
        perf.text_modified();
        perf.painted();
        assert!(
            perf.samples.borrow().is_empty(),
            "a disabled Perf must not accumulate; it is on the keystroke path"
        );
    }

    #[test]
    fn key_arrival_gaps_are_recorded_between_presses() {
        // The input rate is what makes a latency figure readable, and
        // it is the only way to audit a synthetic input tool: if the
        // gaps do not match what the tool was told to produce, part of
        // the measured tail belongs to the tool.
        let perf = Perf::new(true);
        perf.key_pressed();
        assert!(
            perf.gaps.borrow().is_empty(),
            "the first press has no predecessor to measure from"
        );
        std::thread::sleep(std::time::Duration::from_millis(12));
        perf.key_pressed();
        let gaps = perf.gaps.borrow().clone();
        assert_eq!(gaps.len(), 1);
        assert!(
            gaps[0] >= 12_000,
            "gap should reflect the real wait; got {}us",
            gaps[0]
        );
        // Gaps track *arrivals*, so a press that modifies nothing
        // still counts toward the rate.
        drop(gaps);
        perf.key_pressed();
        assert_eq!(perf.gaps.borrow().len(), 2);
    }

    #[test]
    fn a_key_that_changes_nothing_is_never_measured() {
        // Escape, arrows, Backspace at position 0, anything on a
        // read-only buffer. Scintilla does not repaint for these, so a
        // committed press would sit until an unrelated later paint —
        // the caret blink — closed it with a fabricated latency.
        let perf = Perf::new(true);
        perf.key_pressed();
        perf.painted();
        assert!(perf.samples.borrow().is_empty());
        assert!(
            perf.pending.borrow().is_empty(),
            "nothing may be left waiting"
        );
        // And it must not linger to be picked up by a later, unrelated
        // modification either.
        perf.key_pressed();
        perf.key_pressed();
        perf.text_modified();
        perf.painted();
        assert_eq!(
            perf.samples.borrow().len(),
            1,
            "only the press that actually modified may be measured"
        );
    }

    #[test]
    fn a_modification_with_no_key_press_is_not_measured() {
        // A plugin insert, a find-in-files replace, a file load. There
        // is no keystroke to time from.
        let perf = Perf::new(true);
        perf.text_modified();
        perf.painted();
        assert!(perf.samples.borrow().is_empty());
    }

    #[test]
    fn editing_keys_are_measured_once_they_modify() {
        // Tab, Enter and Backspace were excluded outright before this
        // gate existed, because they could not be told apart from keys
        // that repaint nothing. They are measured now, on the runs
        // where they do edit.
        let perf = Perf::new(true);
        for _ in 0..3 {
            perf.key_pressed();
            perf.text_modified();
        }
        perf.painted();
        assert_eq!(perf.samples.borrow().len(), 3);
    }

    #[test]
    fn a_paint_without_a_keypress_is_not_a_sample() {
        // Resizes, exposures and scrolls all paint. Counting them as
        // zero-latency keystrokes would drag every percentile down and
        // make the budget look satisfied when it is not.
        let perf = Perf::new(true);
        perf.painted();
        perf.painted();
        assert!(perf.samples.borrow().is_empty());
    }

    #[test]
    fn every_keypress_gets_its_own_sample_even_when_coalesced() {
        // The regression this guards: an earlier version kept only the
        // most recent unpainted press, which threw away the *longest*
        // latency in a burst — biasing p99 downward exactly when
        // keystrokes are backing up, which is what the budget exists
        // to catch.
        let perf = Perf::new(true);
        for _ in 0..3 {
            perf.key_pressed();
            perf.text_modified();
        }
        perf.painted();
        assert_eq!(
            perf.samples.borrow().len(),
            3,
            "three presses closed by one paint is three measurements"
        );
        // All pending presses are consumed, so a second paint adds
        // nothing.
        perf.painted();
        assert_eq!(perf.samples.borrow().len(), 3);
    }

    #[test]
    fn the_earliest_press_in_a_burst_reports_the_longest_latency() {
        // The ordering property that makes the fix above meaningful:
        // whichever key waited longest must be represented, because it
        // is the one nearest the budget.
        let perf = Perf::new(true);
        perf.key_pressed();
        perf.text_modified();
        std::thread::sleep(std::time::Duration::from_millis(8));
        perf.key_pressed();
        perf.text_modified();
        perf.painted();
        let samples = perf.samples.borrow().clone();
        assert_eq!(samples.len(), 2);
        let longest = samples.iter().copied().max().unwrap_or(0);
        assert!(
            longest >= 8_000,
            "the first press waited >=8ms and must be reported as such; got {longest}us"
        );
    }

    #[test]
    fn unpainted_presses_are_bounded() {
        // A window that stops painting (occluded, minimised) must not
        // grow the pending buffer without bound.
        let perf = Perf::new(true);
        for _ in 0..super::MAX_PENDING + 10 {
            perf.key_pressed();
            perf.text_modified();
        }
        assert_eq!(perf.pending.borrow().len(), super::MAX_PENDING);
        assert_eq!(perf.dropped.get(), 10);
    }

    #[test]
    fn samples_are_capped_and_the_overflow_is_counted() {
        let perf = Perf::new(true);
        // One press per paint, so sample count tracks press count.
        for _ in 0..super::MAX_SAMPLES + 5 {
            perf.key_pressed();
            perf.text_modified();
            perf.painted();
        }
        assert_eq!(perf.samples.borrow().len(), super::MAX_SAMPLES);
        assert_eq!(
            perf.dropped.get(),
            5,
            "the overflow must be counted, so the report can say the \
             percentiles are over a truncated window"
        );
    }

    #[test]
    fn percentiles_are_nearest_rank_and_never_invent_a_value() {
        let s: Vec<u32> = (1..=100).collect();
        assert_eq!(percentile(&s, 50.0), 50);
        assert_eq!(percentile(&s, 99.0), 99);
        assert_eq!(percentile(&s, 100.0), 100);
        // p0 and a single-element slice are the boundary cases where a
        // naive index computation underflows or runs off the end.
        assert_eq!(percentile(&s, 0.0), 1);
        assert_eq!(percentile(&[42], 99.0), 42);
        assert_eq!(percentile(&[42], 0.0), 42);
        // Every reported percentile must be a value that was actually
        // observed — the reason for nearest-rank over interpolation.
        let observed = [3u32, 9, 27];
        for pct in super::REPORTED_PERCENTILES {
            assert!(observed.contains(&percentile(&observed, pct)));
        }
    }

    #[test]
    fn first_draw_is_measured_once() {
        // Backends call this from their ordinary paint handler, so it
        // runs on every frame and must self-limit.
        let perf = Perf::new(true);
        assert!(!perf.first_draw_done.get());
        perf.mark_first_draw();
        assert!(perf.first_draw_done.get());
        perf.mark_first_draw();
        assert!(perf.first_draw_done.get());
    }
}
