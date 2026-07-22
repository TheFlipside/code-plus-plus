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
    /// Timestamps of every key press not yet closed off by a paint.
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

    /// Record that a character was typed.
    ///
    /// Every press is retained until a paint closes it off, and each
    /// becomes its own sample measured from its own press time. An
    /// earlier design kept only the most recent press, on the argument
    /// that the queueing delay of a fast typist is not redraw cost —
    /// but §8 defines the budget as "typed char → redraw", and from
    /// the user's side a character that waited through two frames
    /// waited through two frames. Worse, keeping only the newest press
    /// discards the *longest* latency in any burst, which biases the
    /// tail downward exactly when keystrokes are backing up — the one
    /// situation a p99 budget exists to catch. Reporting per press
    /// cannot make that mistake.
    pub fn key_pressed(&self) {
        if !self.enabled {
            return;
        }
        let mut pending = self.pending.borrow_mut();
        if pending.len() >= MAX_PENDING {
            self.dropped.set(self.dropped.get() + 1);
            return;
        }
        pending.push(Instant::now());
    }

    /// Record that a paint completed, closing off every keystroke
    /// waiting on it — one sample each, measured from that key's own
    /// press. A paint with nothing pending — a resize, an exposure, a
    /// scroll — is ignored rather than recorded as a zero-latency
    /// sample, which would deflate every percentile.
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

    /// Log the keystroke-latency distribution. Call at shutdown.
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
                "keystroke latency: no samples (nothing was typed)"
            );
            return;
        }
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
        perf.painted();
        assert!(
            perf.samples.borrow().is_empty(),
            "a disabled Perf must not accumulate; it is on the keystroke path"
        );
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
        perf.key_pressed();
        perf.key_pressed();
        perf.key_pressed();
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
        std::thread::sleep(std::time::Duration::from_millis(8));
        perf.key_pressed();
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
