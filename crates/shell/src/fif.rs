// Worker-thread entry functions (`walker_main`, `worker_main`,
// `coordinator_main`) take their handles by value because each
// thread *owns* its inputs for the entire scope — they're
// genuinely consumed (the `move` closure that spawns the thread
// captures by-value). clippy's `needless_pass_by_value` lint
// would prefer `&PathBuf` / `&Arc<...>`, which is wrong here
// because the closure that becomes the thread body must take
// ownership. Applied at the module level so the worker
// signatures stay readable.
//
// `items_after_statements` is also allowed here: the worker
// functions declare named helper closures and locally-scoped
// `const`s mid-body for clarity at the use site rather than
// hoisting them above every initialisation expression.
#![allow(clippy::needless_pass_by_value, clippy::items_after_statements)]

//! Find-in-Files orchestration.
//!
//! Phase 4 m4 step 2. Glues the pure search engine in
//! [`codepp_core::fif`] to the shell's existing cross-thread plumbing
//! (DESIGN.md §5.4): a directory walker thread enumerates paths, a
//! fixed-size worker pool decodes + searches each file, and a
//! coordinator thread joins them and posts a final
//! [`FifEvent::Done`] (or [`FifEvent::Cancelled`]) so the UI can
//! mark the dock as idle.
//!
//! ## Threading model
//!
//! - **Walker** (1 thread). Depth-first traversal of `request.root`,
//!   pruning excluded directories by basename and respecting
//!   [`FifWalkOpts`]. Pushes file paths onto a bounded channel — the
//!   bound (256) keeps memory flat on multi-million-file trees while
//!   workers catch up.
//! - **Workers** (`available_parallelism()`, capped at 16). Each pulls
//!   one path at a time, opens it, runs [`is_binary`] on the prefix
//!   probe, decodes via [`encoding::detect`] + [`encoding::decode`],
//!   and posts a [`FifEvent::FileMatches`] for every file that hits.
//!   Higher fan-out than 16 is counter-productive on typical `NVMe`
//!   storage; the cap is also a defence-in-depth ceiling on resource
//!   use.
//! - **Coordinator** (1 thread). Joins the walker and all workers
//!   then emits the terminal event with elapsed-time stats.
//!
//! ## Cancellation
//!
//! A new [`FifOrchestrator::start`] preempts the prior job: the
//! prior job's `Arc<AtomicBool>` is flipped, the walker exits at its
//! next entry-loop boundary, and workers exit between files.
//! In-flight events from the cancelled job may still arrive in the
//! shell's event queue — the **UI consumer** of `take_fif_events`
//! filters by [`FifJobId`] (each [`FifEvent`] carries its job id),
//! and the shell itself does not discard them. The coordinator
//! emits exactly one terminal event (`Done` or `Cancelled`) per
//! started job.
//!
//! ## Global cap
//!
//! [`MAX_MATCHES_TOTAL`] is enforced by an `Arc<AtomicUsize>` shared
//! across workers. A worker that produces matches reserves a slice
//! of the counter via `fetch_add`; if the slice would overflow the
//! cap the worker truncates its outcome to the remainder, sets the
//! cancel flag (so siblings stop), and emits its trimmed match list
//! anyway so no work is wasted.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, available_parallelism};
use std::time::Instant;

use crossbeam_channel::{bounded, Receiver, Sender};

use codepp_core::encoding;
use codepp_core::fif::{
    is_binary, search_in_text, FifQuery, FifQueryError, FifQueryOpts, FifWalkOpts,
    FileSearchOutcome, BINARY_PROBE_BYTES, MAX_MATCHES_TOTAL,
};

/// Hard ceiling on the worker pool size. Above this, FIO contention
/// dominates and total throughput regresses; the ceiling also keeps
/// per-job memory bounded (each worker holds one decoded buffer at
/// a time). Well above the core count of any laptop.
pub const MAX_FIF_WORKERS: usize = 16;

/// Bound on the walker → worker path channel. With ~256 paths
/// in-flight a worker pull always sees a fresh path and the walker
/// blocks just before exhausting memory, even on `find / -type f`.
pub const PATH_CHANNEL_DEPTH: usize = 256;

/// Ceiling on coordinator threads that haven't reported terminal
/// yet. Each `start` increments the counter; the coordinator
/// decrements on its way out. A new `start` is rejected with
/// [`FifError::TooManyJobs`] once the ceiling is reached. Bounds
/// the thread-leak window when a prior job's worker is blocked in
/// an OS syscall that the cancel flag cannot preempt — most
/// commonly an unreachable network mount, where the OS-level
/// timeout is multiple seconds. Two jobs in flight ("the one we
/// just cancelled" + "the new one") is the legitimate ceiling for
/// interactive use.
pub const MAX_ACTIVE_JOBS: usize = 2;

/// Ceiling on directory nesting the walker will descend.
///
/// This is what bounds the walker. It keeps one open
/// [`std::fs::ReadDir`] per level of the tree it is currently inside
/// and pulls entries lazily, so its cost scales with *depth*, never
/// with how many directories are pending. An earlier version pushed
/// every subdirectory of a directory onto a `Vec<PathBuf>` before
/// descending into any of them, which made a single wide directory —
/// millions of subdirectories — grow that vector without any bound.
/// Match volume was capped, file size was capped, the path channel was
/// capped; traversal was the one dimension that was not. Measured on
/// 200 000 subdirectories in one directory: 48.3 MB peak before,
/// 1.6 MB after.
///
/// **The value is chosen by file-descriptor budget, not by path
/// length.** Holding a descriptor per level is what makes the walk
/// lazy, so this constant sets a hard ceiling on open descriptors:
///
/// ```text
/// MAX_ACTIVE_JOBS × (MAX_WALK_DEPTH + MAX_FIF_WORKERS)
///        2        × (      64       +        16      ) = 160
/// ```
///
/// 160 sits well inside a 256 soft `RLIMIT_NOFILE` (macOS's
/// traditional interactive default) and leaves ample room under
/// Linux's usual 1024 for the descriptors the rest of the editor needs
/// — open buffers, the file watcher, GTK's own. An earlier draft used
/// 512, which reached ≈1056 in the two-job worst case and would have
/// traded an unbounded-memory bug for a file-descriptor-exhaustion
/// one.
///
/// 64 levels is far beyond any real source tree; deeply nested
/// `node_modules` is the usual worst case and lands well under it.
/// Exceeding it skips the subtree, which is why the walker both warns
/// *and* records [`FifStats::depth_cap_hit`] — a search that quietly
/// returns fewer results than the truth is worse than a slow one, and
/// `tracing` is off by default in release builds (DESIGN.md §5.5), so
/// the log alone would reach nobody.
pub const MAX_WALK_DEPTH: usize = 64;

/// Identifier for a single FIF run. Monotonic per-shell so the UI
/// can discard events from a job preempted by a newer
/// [`FifOrchestrator::start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FifJobId(u64);

impl FifJobId {
    /// Build an id with a chosen counter value.
    ///
    /// Test-only, and deliberately not part of the public API: ids are
    /// minted by [`FifOrchestrator::start`] so that "newer job wins"
    /// preemption is decided by a single monotonic counter. A caller
    /// that could invent one could make two live jobs share an id.
    #[cfg(test)]
    pub(crate) fn for_test(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw counter value, useful for `tracing` spans and diagnostics.
    #[must_use]
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Errors from [`FifOrchestrator::start`].
#[derive(Debug)]
pub enum FifError {
    /// User-supplied query string failed to compile (empty, invalid
    /// regex, or oversized — see [`FifQueryError`]).
    Query(FifQueryError),
    /// `request.root` does not exist or is not a directory. The
    /// walker would otherwise emit zero events and look like a
    /// successful empty search; surface the bad path explicitly.
    BadRoot(PathBuf),
    /// A previous job is still draining (workers blocked on slow
    /// I/O, e.g., an unreachable network mount whose syscall cannot
    /// be preempted by the cancel flag) and the active-job ceiling
    /// is reached. Caller should retry once the prior job's
    /// terminal event arrives. Bounds the thread-leak window a
    /// hostile in-process plugin could exploit by calling
    /// `start_fif` in a tight loop.
    TooManyJobs,
    /// `thread::Builder::spawn` returned an OS error (typically
    /// `EAGAIN`/thread-table-full). Workers spawned before the
    /// failure exit cleanly when their `path_rx` closes.
    SpawnFailed(std::io::Error),
}

impl std::fmt::Display for FifError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FifError::Query(e) => write!(f, "find-in-files: {e}"),
            FifError::BadRoot(p) => {
                write!(f, "find-in-files: root is not a directory: {}", p.display())
            }
            FifError::TooManyJobs => {
                f.write_str("find-in-files: too many concurrent jobs (prior job still draining)")
            }
            FifError::SpawnFailed(e) => write!(f, "find-in-files: thread spawn failed: {e}"),
        }
    }
}

impl std::error::Error for FifError {}

/// Streaming events produced by a FIF job. Drained alongside
/// [`codepp_core::LoadResult`] and platform `FileChange` events in
/// the shell's `WM_APP_WAKE` handler.
#[derive(Debug)]
pub enum FifEvent {
    /// One file produced one or more matches. Outcomes from a single
    /// job are delivered in the order workers finish each file —
    /// **not** lexicographic order. The UI inserts each batch into
    /// the results dock as it arrives.
    FileMatches {
        /// Tag the listview ignores when this is from a preempted job.
        job: FifJobId,
        /// Absolute or root-relative path; whatever the walker enqueued.
        path: PathBuf,
        /// Matches plus per-file truncation flag.
        outcome: FileSearchOutcome,
    },
    /// A file that matched is **open in a tab**, so the worker did not
    /// rewrite it on disk. Carries the replacement it computed, for
    /// `Shell::drain` to apply to the buffer instead.
    ///
    /// Writing to disk here would be wrong twice over: the atomic
    /// temp+rename looks like delete-then-create to the file watcher,
    /// popping an "external change" dialog for every modified open
    /// file, and it would clobber unsaved in-buffer edits.
    ///
    /// `new_text` is computed from what is **on disk**, so it is only
    /// valid for a tab whose buffer still matches disk. `drain` checks
    /// that and skips a dirty buffer rather than overwriting the
    /// user's unsaved work — see `Shell::apply_open_buffer_replacement`.
    ReplaceInOpenBuffer {
        job: FifJobId,
        path: PathBuf,
        /// The file's full contents with every match replaced.
        new_text: String,
        /// How many matches the replacement covered.
        replaced: usize,
    },
    /// What `Shell::drain` did with a [`FifEvent::ReplaceInOpenBuffer`].
    /// Replaces it in the event stream, so the UI reports what
    /// actually happened rather than what was requested.
    ReplacedInOpenBuffer {
        job: FifJobId,
        path: PathBuf,
        /// Matches replaced. Zero unless `outcome` is `Replaced`.
        replaced: usize,
        outcome: OpenBufferOutcome,
    },
    /// Job ran to completion (walker exhausted, all workers idle).
    /// Stats include the elapsed wall time the walker took.
    Done { job: FifJobId, stats: FifStats },
    /// Job was preempted by a new `start` call (or the shell shut
    /// down). No further events for this `job` will be emitted after
    /// this one.
    Cancelled { job: FifJobId },
}

/// How an open file's Replace-in-Files replacement was handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenBufferOutcome {
    /// Applied to the tab's buffer. The user can undo it, the file
    /// watcher stayed silent, and the tab is now unsaved.
    Replaced,
    /// The tab had unsaved edits, so the worker's replacement — which
    /// it computed from the on-disk bytes — did not describe what the
    /// user is looking at. Declined rather than discarding their work.
    SkippedDirty,
    /// The tab closed between the walker listing open paths and this
    /// event being handled. Nothing was written to disk either.
    TabClosed,
}

/// Per-job aggregate stats. Captured for the UI's "complete" toast
/// (e.g. "scanned 1247 files, 89 matches in 0.34 s").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FifStats {
    /// Files the workers actually opened and decoded.
    pub files_scanned: usize,
    /// Files skipped because the binary heuristic flagged them.
    pub files_skipped_binary: usize,
    /// Files skipped because `meta.len() > walk.max_file_bytes`.
    pub files_skipped_size: usize,
    /// Files that produced at least one match.
    pub files_with_matches: usize,
    /// Sum of per-file `outcome.matches.len()` across the job.
    pub total_matches: usize,
    /// `true` when the global cap was hit and the job ended early.
    pub global_cap_hit: bool,
    /// True if the walker refused to descend past [`MAX_WALK_DEPTH`]
    /// somewhere in this run, meaning at least one subtree was not
    /// searched and the results are incomplete. Distinct from
    /// [`Self::global_cap_hit`], which means "we found more matches
    /// than we will report"; this one means "there is ground we did
    /// not cover".
    pub depth_cap_hit: bool,
    /// Files rewritten in Replace-in-Files mode. Always 0 for
    /// search-only jobs.
    pub files_modified: usize,
    /// Total substitutions performed across all rewritten files.
    /// Distinct from `total_matches` because the search loop caps
    /// per-file matches at [`codepp_core::fif::MAX_MATCHES_PER_FILE`]
    /// for display, while the replace loop runs the regex
    /// uncapped. The status label uses this for the "Replaced X
    /// occurrences" wording so it doesn't undercount on dense
    /// matches. Always 0 for search-only jobs.
    pub total_replacements: usize,
    /// Files matched during Replace-in-Files but not rewritten
    /// because the user has them open in a tab. Skipping them
    /// avoids racing the file watcher (whose "external change"
    /// dialog would otherwise pop for every open file the worker
    /// touched) and keeps the user's in-buffer edits intact.
    pub files_skipped_open: usize,
    /// Wall-clock duration measured at the coordinator.
    pub elapsed_ms: u64,
}

/// Caller-supplied bundle for [`FifOrchestrator::start`].
pub struct FifRequest {
    /// Pattern as the user typed it. Compiled via
    /// [`FifQuery::compile`]; the orchestrator returns
    /// [`FifError::Query`] on a bad pattern instead of starting any
    /// threads.
    pub query: String,
    /// Pattern compilation flags (case, whole-word, regex mode).
    pub opts: FifQueryOpts,
    /// Directory to walk. Must be a directory. Symlinks are never
    /// followed — see the walker comment for the device-file /
    /// loop-DoS reasoning.
    pub root: PathBuf,
    /// Include/exclude globs and the per-file size ceiling.
    pub walk: FifWalkOpts,
    /// `Some(s)` switches the orchestrator from search-only into
    /// "Replace in Files" mode: every matched file is rewritten
    /// in place with `s` substituted for each match. The match
    /// list still flows through `FifEvent::FileMatches` so the
    /// UI can show what changed. `None` is plain Find in Files.
    pub replacement: Option<String>,
    /// Absolute paths of files the user currently has open in a
    /// tab. Workers in Replace-in-Files mode skip these to avoid
    /// (a) the file watcher's "external change" dialog firing for
    /// every modified open file, and (b) silently overwriting an
    /// in-buffer edit the user hasn't saved yet. Empty for plain
    /// Find in Files (the search loop doesn't write anything).
    pub open_tab_paths: Vec<PathBuf>,
}

/// Per-job control surface the orchestrator retains until the next
/// `start` (which preempts) or `cancel` (which preempts and forgets).
/// Only the cancel flag is needed at preemption time — the job id is
/// already baked into the events the worker pool emits.
struct JobHandle {
    cancel: Arc<AtomicBool>,
}

/// Owns the `next_id` counter and the active job's cancel handle.
/// Held inside `Shell`; the public API is exposed through
/// `Shell::start_fif` / `Shell::cancel_fif` so callers don't have to
/// thread the channel half through.
pub(crate) struct FifOrchestrator {
    next_id: u64,
    current: Option<JobHandle>,
    out_tx: Sender<FifEvent>,
    /// Live coordinator-thread counter. Incremented before the
    /// coordinator is spawned, decremented at the very end of the
    /// coordinator after its terminal event is sent.
    active_jobs: Arc<AtomicUsize>,
}

impl FifOrchestrator {
    pub(crate) fn new(out_tx: Sender<FifEvent>) -> Self {
        Self {
            next_id: 1,
            current: None,
            out_tx,
            active_jobs: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Preempt any in-flight job and start a new one. Returns the
    /// new job's id; events will arrive on the shell's `fif_rx`.
    pub(crate) fn start(&mut self, request: FifRequest) -> Result<FifJobId, FifError> {
        // Compile up-front so a malformed query never spawns threads.
        let query = FifQuery::compile(&request.query, request.opts).map_err(FifError::Query)?;
        if !request.root.is_dir() {
            return Err(FifError::BadRoot(request.root));
        }
        // Refuse to start if a previous job's coordinator hasn't
        // wound down — typically because a worker is blocked in a
        // syscall (unreachable network mount) the cancel flag
        // can't preempt. The check happens BEFORE `cancel()` so a
        // stuck-and-still-stuck retry is rejected, not the polite
        // "user re-clicked Find" pattern (in which case the prior
        // job's coordinator has already terminated and active_jobs
        // is back below the ceiling).
        if self.active_jobs.load(Ordering::Acquire) >= MAX_ACTIVE_JOBS {
            return Err(FifError::TooManyJobs);
        }
        self.cancel();

        let id = FifJobId(self.next_id);
        // `checked_add` matches the policy `allocate_buffer_id` uses
        // for plugin-protocol-facing ids: a hostile in-process plugin
        // calling `start_fif` in a tight loop must not silently wrap
        // and collide with an old job's id (which would misroute UI
        // events). u64 overflow is unreachable in practice (~10^19
        // jobs); the panic is caught by the wnd_proc's `catch_unwind`
        // wrappers and turns the DoS path into a clean abort instead
        // of an ABI break.
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("FifJobId space exhausted (u64::MAX starts in one session)");

        let cancel = Arc::new(AtomicBool::new(false));
        let total = Arc::new(AtomicUsize::new(0));
        let walk = Arc::new(request.walk);
        let query = Arc::new(query);
        let stats = Arc::new(FifStatsAtomic::default());
        // `replacement` is `None` for search-only jobs and `Some`
        // for Replace in Files. Wrapped in an `Arc` so each worker
        // can hold a cheap pointer rather than cloning the string.
        let replacement: Option<Arc<(String, bool)>> = request.replacement.map(|s| {
            // The boolean is "expand $N capture-group references";
            // mirrors the regex flag from `request.opts`.
            Arc::new((s, request.opts.regex))
        });
        // Skip-list for replace mode — paths the user has open in a
        // tab. Stored as an `Arc<HashSet>` so workers share one
        // copy. For typical session sizes (≤ ~50 tabs) hash lookup
        // dominates linear scan; HashSet is also future-proof if
        // the editor ever supports hundreds of tabs.
        let open_paths: Arc<std::collections::HashSet<PathBuf>> =
            Arc::new(request.open_tab_paths.into_iter().collect());

        let n_workers = available_parallelism()
            .map_or(4, std::num::NonZero::get)
            .clamp(1, MAX_FIF_WORKERS);

        let (path_tx, path_rx) = bounded::<PathBuf>(PATH_CHANNEL_DEPTH);

        // Spawn workers + walker. If any spawn fails (typically
        // OS thread-table-full), drop everything we created here
        // — `path_tx` going out of scope closes the channel,
        // already-spawned workers receive `Err` from `recv()` and
        // exit cleanly. No coordinator was registered yet, so
        // `active_jobs` is unchanged.
        let mut worker_handles = Vec::with_capacity(n_workers);
        for i in 0..n_workers {
            let path_rx = path_rx.clone();
            let event_tx = self.out_tx.clone();
            let query = query.clone();
            let cancel = cancel.clone();
            let total = total.clone();
            let stats = stats.clone();
            let walk = walk.clone();
            let replacement = replacement.clone();
            let open_paths = open_paths.clone();
            let h = thread::Builder::new()
                .name(format!("codepp-fif-worker-{i}"))
                .spawn(move || {
                    worker_main(
                        id,
                        path_rx,
                        event_tx,
                        query,
                        cancel,
                        total,
                        walk,
                        stats,
                        replacement,
                        open_paths,
                    );
                })
                .map_err(FifError::SpawnFailed)?;
            worker_handles.push(h);
        }
        // Drop the orchestrator's copy of path_rx so that, once the
        // walker drops `path_tx`, the workers' `recv()` calls return
        // `Err` deterministically. Without this drop the channel
        // stays open forever (this end is also a receiver) and
        // workers would block until cancellation.
        drop(path_rx);

        let walker_walk = walk.clone();
        let walker_cancel = cancel.clone();
        let walker_stats = stats.clone();
        let root = request.root.clone();
        let walker = thread::Builder::new()
            .name("codepp-fif-walker".to_string())
            .spawn(move || walker_main(root, walker_walk, path_tx, walker_cancel, walker_stats))
            .map_err(FifError::SpawnFailed)?;

        // Reserve our slot in `active_jobs` BEFORE spawning the
        // coordinator, so the coordinator's decrement always
        // matches an increment, even if the spawn itself fails.
        self.active_jobs.fetch_add(1, Ordering::AcqRel);
        let coord_tx = self.out_tx.clone();
        let coord_cancel = cancel.clone();
        let coord_active = self.active_jobs.clone();
        let coord_stats = stats.clone();
        let started = Instant::now();
        let spawn_result = thread::Builder::new()
            .name("codepp-fif-coord".to_string())
            .spawn(move || {
                // Walker first; once it exits the path channel
                // closes, which lets workers wind down naturally.
                let _ = walker.join();
                for h in worker_handles {
                    let _ = h.join();
                }
                let mut snap = coord_stats.snapshot();
                snap.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let event = if coord_cancel.load(Ordering::Acquire) {
                    FifEvent::Cancelled { job: id }
                } else {
                    FifEvent::Done {
                        job: id,
                        stats: snap,
                    }
                };
                // Send-error means the shell dropped its receiver
                // (process tearing down). Nothing to do.
                let _ = coord_tx.send(event);
                coord_active.fetch_sub(1, Ordering::AcqRel);
            });
        if let Err(e) = spawn_result {
            // Coord spawn failed — release the slot we just took
            // and let walker + workers wind down on their own
            // (their channels will close when this scope ends).
            self.active_jobs.fetch_sub(1, Ordering::AcqRel);
            return Err(FifError::SpawnFailed(e));
        }

        self.current = Some(JobHandle { cancel });
        Ok(id)
    }

    /// Preempt the current job, if any. Idempotent.
    pub(crate) fn cancel(&mut self) {
        if let Some(handle) = self.current.take() {
            handle.cancel.store(true, Ordering::Release);
        }
    }
}

impl Drop for FifOrchestrator {
    fn drop(&mut self) {
        // Shell teardown: signal the active job so workers don't
        // outlive the shell. Threads are detached, so they'll finish
        // on their own — the cancel flip just stops them allocating
        // any more work for buffers that are about to disappear.
        self.cancel();
    }
}

/// Atomic counterpart to [`FifStats`]. Workers update fields with
/// `fetch_add` / `store`; the coordinator snapshots them once at the
/// end of the job. `Relaxed` ordering everywhere except the cancel
/// flag itself (Acquire/Release) — the counters are advisory and a
/// stale read at snapshot time is never wrong by more than the
/// last-emitted batch.
#[derive(Default)]
struct FifStatsAtomic {
    files_scanned: AtomicUsize,
    files_skipped_binary: AtomicUsize,
    files_skipped_size: AtomicUsize,
    files_with_matches: AtomicUsize,
    total_matches: AtomicUsize,
    files_modified: AtomicUsize,
    total_replacements: AtomicUsize,
    files_skipped_open: AtomicUsize,
    global_cap_hit: AtomicBool,
    depth_cap_hit: AtomicBool,
}

impl FifStatsAtomic {
    fn snapshot(&self) -> FifStats {
        FifStats {
            files_scanned: self.files_scanned.load(Ordering::Relaxed),
            files_skipped_binary: self.files_skipped_binary.load(Ordering::Relaxed),
            files_skipped_size: self.files_skipped_size.load(Ordering::Relaxed),
            files_with_matches: self.files_with_matches.load(Ordering::Relaxed),
            total_matches: self.total_matches.load(Ordering::Relaxed),
            files_modified: self.files_modified.load(Ordering::Relaxed),
            total_replacements: self.total_replacements.load(Ordering::Relaxed),
            files_skipped_open: self.files_skipped_open.load(Ordering::Relaxed),
            global_cap_hit: self.global_cap_hit.load(Ordering::Relaxed),
            depth_cap_hit: self.depth_cap_hit.load(Ordering::Relaxed),
            // Filled in by the coordinator from the wall-clock timer.
            elapsed_ms: 0,
        }
    }
}

/// Directory basenames the walker prunes unconditionally — purely
/// performance-driven exclusions whose contents are almost never
/// what the user is searching for (build artefacts, dependency
/// caches, etc.). Mirrors the corresponding lists in ripgrep / ag.
/// Dot-prefixed directories (`.git`, `.idea`, etc.) are pruned
/// dynamically via the `walk_hidden_dirs` flag rather than enumerated
/// here — that way new VCS systems and IDEs don't need to be added
/// one at a time.
const ALWAYS_PRUNED_DIR_BASENAMES: &[&str] =
    &["target", "node_modules", "build", "dist", "__pycache__"];

/// Whether the walker should descend into `name`, given the user's
/// hidden-folders preference. Dot-prefixed basenames (the dotfile
/// convention used by VCS metadata directories like `.git`/`.hg`,
/// IDE state directories like `.idea`/`.vs`/`.vscode`, and tool
/// caches like `.cargo`/`.next`/`.terraform`) are pruned by default;
/// they're descended only when the user opts in via "In hidden
/// folders". Always-pruned basenames are pruned regardless.
fn dir_should_prune(name: &OsStr, walk_hidden_dirs: bool) -> bool {
    if ALWAYS_PRUNED_DIR_BASENAMES
        .iter()
        .any(|p| name == OsStr::new(p))
    {
        return true;
    }
    if !walk_hidden_dirs {
        // Cheap dotfile check. `name.to_str()` only fails on a
        // non-UTF-8 OsStr — on Windows that's a WTF-16 sequence
        // containing a lone surrogate, which by definition cannot
        // start with the ASCII byte `.`, so a `None` here is
        // conservatively-correct: the directory is descended into,
        // matching the "not a dotfile" interpretation.
        if let Some(s) = name.to_str() {
            if s.starts_with('.') {
                return true;
            }
        }
    }
    false
}

fn walker_main(
    root: PathBuf,
    walk: Arc<FifWalkOpts>,
    path_tx: Sender<PathBuf>,
    cancel: Arc<AtomicBool>,
    stats: Arc<FifStatsAtomic>,
) {
    // A stack of open directory iterators, one per level currently
    // being descended, rather than a queue of pending directory paths.
    // The difference is what bounds this walk: entries are pulled
    // lazily, so a directory with a million subdirectories costs one
    // iterator, not a million `PathBuf`s. Depth is capped by
    // `MAX_WALK_DEPTH`, so both memory and open descriptors are
    // bounded by a constant. See that constant for the history.
    let mut stack: Vec<std::fs::ReadDir> = Vec::new();
    match std::fs::read_dir(&root) {
        Ok(it) => stack.push(it),
        Err(e) => {
            tracing::debug!(dir = ?root, error = ?e, "fif walker: read_dir failed");
            return;
        }
    }

    while let Some(iter) = stack.last_mut() {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        // Exhausted this level — climb back out. Dropping the iterator
        // closes its descriptor, which is what keeps the descriptor
        // count tied to depth rather than to how much of the tree has
        // been walked.
        let Some(entry) = iter.next() else {
            stack.pop();
            continue;
        };
        // A per-entry error means the OS gave us a partial
        // result — most commonly permission-denied on one file
        // mid-enumeration. Skip the entry but log so the cause
        // is recoverable from a verbose trace.
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = ?e, "fif walker: dir entry error");
                continue;
            }
        };
        let path = entry.path();
        let ftype = match entry.file_type() {
            Ok(t) => t,
            Err(e) => {
                // Same treatment as the dir/entry errors above:
                // a `file_type()` failure (broken symlink, racy
                // permission change) is meaningful but never
                // worth aborting the whole walk for.
                tracing::debug!(
                    path = ?path,
                    error = ?e,
                    "fif walker: file_type query failed",
                );
                continue;
            }
        };
        // Symlinks fall through both `is_dir()` (false) and
        // `is_file()` (false on the lstat-style result that
        // `DirEntry::file_type()` returns), so the `!is_file()`
        // continue below skips them. We deliberately do not
        // follow symlinks: a symlink loop is a classic FIF DoS,
        // and a symlink that resolves to a Windows COM port or
        // a Linux FIFO would block `read_capped` on a
        // `read_to_end` syscall the cancel flag can't preempt.
        // N++ doesn't follow either.
        if ftype.is_dir() {
            if !walk.recurse {
                continue;
            }
            if let Some(name) = path.file_name() {
                if dir_should_prune(name, walk.walk_hidden_dirs) {
                    continue;
                }
            }
            if stack.len() >= MAX_WALK_DEPTH {
                // Warn, not debug: this is the one skip in this
                // function that hides content the user asked to
                // search, so it must be visible rather than
                // buried in a trace nobody enables.
                // Record before logging: `tracing` is off by default
                // in release builds (DESIGN.md §5.5), so the flag is
                // the only channel that actually reaches the user.
                stats.depth_cap_hit.store(true, Ordering::Relaxed);
                tracing::warn!(
                    path = ?path,
                    depth = MAX_WALK_DEPTH,
                    "fif walker: maximum directory depth reached; not descending further",
                );
                continue;
            }
            match std::fs::read_dir(&path) {
                Ok(it) => stack.push(it),
                Err(e) => {
                    tracing::debug!(
                        dir = ?path,
                        error = ?e,
                        "fif walker: read_dir failed",
                    );
                }
            }
            continue;
        }
        if !ftype.is_file() {
            continue;
        }
        if !walk.path_matches(&path) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if meta.len() > walk.max_file_bytes {
                stats.files_skipped_size.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }
        // Bounded-channel send blocks while workers catch up.
        // `Err` means the receiving end closed: shell dropped
        // the orchestrator (process exit) → nothing to do.
        if path_tx.send(path).is_err() {
            return;
        }
    }
    // path_tx drops here; workers receive `Err` and exit.
}

// Ten `Arc` clones, all freshly cloned by `start()` per worker.
// Bundling them into a struct just to satisfy the lint would add an
// indirection without removing any data — every field is still on
// the worker's stack frame.
#[allow(clippy::too_many_arguments)]
fn worker_main(
    id: FifJobId,
    path_rx: Receiver<PathBuf>,
    event_tx: Sender<FifEvent>,
    query: Arc<FifQuery>,
    cancel: Arc<AtomicBool>,
    total: Arc<AtomicUsize>,
    walk: Arc<FifWalkOpts>,
    stats: Arc<FifStatsAtomic>,
    // `Some((replacement, expand_groups))` for Replace in Files
    // mode — the worker rewrites each matched file in place after
    // emitting its match list. `None` is plain Find in Files.
    replacement: Option<Arc<(String, bool)>>,
    // Files the user has open in a tab. In replace mode the worker
    // skips writing to these — the file watcher would otherwise
    // pop "external change" prompts for every modified open file,
    // and overwriting could clobber unsaved in-buffer edits.
    open_paths: Arc<std::collections::HashSet<PathBuf>>,
) {
    while let Ok(path) = path_rx.recv() {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        // Re-check size at read time: a concurrent writer could
        // have grown the file past the cap between the walker's
        // metadata call and now. Bound the read explicitly.
        let Some((bytes, perms)) = read_for_search(&path, walk.max_file_bytes, &stats) else {
            continue;
        };
        let probe = &bytes[..bytes.len().min(BINARY_PROBE_BYTES)];
        if is_binary(probe) {
            stats.files_skipped_binary.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let (enc, body) = encoding::detect(&bytes);
        let Ok(text) = encoding::decode(body, &enc) else {
            // Decode failure (e.g. corrupted UTF-8 body without a
            // BOM). Skip silently — N++'s FIF behaves the same.
            stats.files_skipped_binary.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let outcome = search_in_text(&query, &text);
        stats.files_scanned.fetch_add(1, Ordering::Relaxed);
        if outcome.matches.is_empty() {
            continue;
        }

        // Reserve the file's matches against the global cap. Two
        // workers can race here; the loser's `prior + n` will exceed
        // the cap and trim accordingly. After trimming, refund the
        // un-emitted portion so `total` and the per-job stats counter
        // stay in lockstep — `total` is the gate the next worker
        // tests, so over-charging would make a sibling skip matches
        // it could legitimately have emitted before the first
        // cap-hit worker won the race.
        let n = outcome.matches.len();
        let prior = total.fetch_add(n, Ordering::AcqRel);
        let outcome = if prior + n > MAX_MATCHES_TOTAL {
            stats.global_cap_hit.store(true, Ordering::Relaxed);
            cancel.store(true, Ordering::Release);
            let allowed = MAX_MATCHES_TOTAL.saturating_sub(prior);
            // On the trim path `allowed < n` always (we got here
            // because `prior + n > MAX`), so `n - allowed` is the
            // un-emitted portion to refund. Hands the global cap
            // back to siblings still mid-search.
            total.fetch_sub(n - allowed, Ordering::AcqRel);
            if allowed == 0 {
                continue;
            }
            let mut trimmed = outcome;
            trimmed.matches.truncate(allowed);
            trimmed.truncated = true;
            trimmed
        } else {
            outcome
        };

        stats.files_with_matches.fetch_add(1, Ordering::Relaxed);
        stats
            .total_matches
            .fetch_add(outcome.matches.len(), Ordering::Relaxed);

        // Replace in Files: rewrite the file in place with the
        // user's replacement string substituted for each match.
        // The match list still flows through `FileMatches` so the
        // dock can show what changed. We write through a temp +
        // rename so a crash mid-write can't truncate the source.
        //
        // Skip files the user has open: rewriting them would race
        // the file watcher (Windows reports the temp+rename as a
        // delete-then-create, popping "external change" dialogs
        // for every modified open file) and could clobber unsaved
        // in-buffer edits. The match list still flows so the dock
        // shows what *would* have been replaced; the user can
        // re-run after closing those tabs.
        if let Some(repl) = &replacement {
            if open_paths.contains(&path) {
                // Compute the replacement but do not write it: hand it
                // to the UI thread, which can apply it to the tab's
                // Scintilla buffer where the user keeps undo and the
                // file watcher stays silent. Only the UI thread may
                // touch Scintilla (DESIGN.md §5.4), so this is as far
                // as a worker can take it.
                let (new_text, n_replaced) =
                    codepp_core::fif::replace_in_text(&query, &text, &repl.0, repl.1);
                let _ = event_tx.send(FifEvent::FileMatches {
                    job: id,
                    path: path.clone(),
                    outcome,
                });
                let _ = event_tx.send(FifEvent::ReplaceInOpenBuffer {
                    job: id,
                    path,
                    new_text: new_text.into_owned(),
                    replaced: n_replaced,
                });
                continue;
            }
            let (new_text, n_replaced) =
                codepp_core::fif::replace_in_text(&query, &text, &repl.0, repl.1);
            // `n_replaced` is the truth — `outcome.matches.len()`
            // caps at `MAX_MATCHES_PER_FILE` for display, while
            // `replace_in_text` runs uncapped. UI status label
            // reads `total_replacements` for the replace wording.
            // Re-encode through the file's original encoding so a
            // CP1252 file stays CP1252 after the rewrite. The
            // best-effort encode below may fail if the replacement
            // text introduces characters the legacy codepage can't
            // represent — surface that as a skipped file rather
            // than corrupting the contents.
            match codepp_core::encoding::encode(&new_text, &enc) {
                Ok(new_bytes) => {
                    if let Err(e) = atomic_write(&path, &new_bytes, &perms) {
                        tracing::warn!(
                            path = ?path,
                            error = ?e,
                            "fif worker: replace write failed"
                        );
                    } else {
                        stats.files_modified.fetch_add(1, Ordering::Relaxed);
                        stats
                            .total_replacements
                            .fetch_add(n_replaced, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        path = ?path,
                        error = ?e,
                        "fif worker: replacement encoding failed (file kept unchanged)",
                    );
                }
            }
        }

        if event_tx
            .send(FifEvent::FileMatches {
                job: id,
                path,
                outcome,
            })
            .is_err()
        {
            return;
        }
    }
}

/// Atomic file write: emit to a temp file in the same directory,
/// then rename over the target. Avoids truncating the original on a
/// crash mid-write. The temp file gets an unpredictable name from
/// `tempfile` (`.codepp-fif-<random>.tmp`), created `O_EXCL`
/// so a leftover from a killed process is recognizable. Same
/// cross-platform-rename caveat as everywhere else: on Windows,
/// `fs::rename` succeeds atomically when both paths are on the
/// same volume, which is guaranteed here since we generate the
/// temp path from the target's parent.
fn atomic_write(
    target: &std::path::Path,
    bytes: &[u8],
    perms: &std::fs::Permissions,
) -> std::io::Result<()> {
    use std::io::Write;
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target has no parent directory",
        )
    })?;

    // Randomly-named temp via `tempfile`, not a predictable
    // `<target>.codepp-fif.tmp` opened with `File::create`.
    //
    // The predictable form was a symlink attack: anyone able to write
    // to the directory being searched could pre-place that exact name
    // as a symlink, and `File::create` — which follows symlinks and
    // truncates — would send this replacement's bytes wherever it
    // pointed, outside the search root entirely. `tempfile` picks an
    // unpredictable name and creates it `O_EXCL`, which POSIX requires
    // to fail on a symlink, so neither half of that attack works.
    //
    // Same construction `Shell::save_current_to_disk` and
    // `save_buffer_as` already use; this path was the odd one out.
    //
    // `persist` renames over `target`. POSIX rename replaces a symlink
    // at the destination rather than following it, so a `target`
    // swapped for a symlink mid-run gets replaced, not written through
    // — the destructive direction was already safe here and stays that
    // way. On Windows `persist` goes through `MoveFileExW`, which
    // documents the same behaviour (destination reparse points are
    // replaced, not followed). Pinned on both platforms by
    // `atomic_write_replaces_a_symlink_at_the_destination`. The
    // Windows variant of that test is `#[ignore]`d because creating
    // the setup symlink needs `SeCreateSymbolicLinkPrivilege`, so
    // CI needs Windows developer mode and
    // `cargo test -- --include-ignored` for it to run — see
    // docs/DEVELOPMENT.md §2.6.
    //
    // `permissions` is not optional. `tempfile` creates at 0600 when
    // it is not set, and because `persist` renames the replacement
    // over the original, the replacement's mode wins — so omitting it
    // would silently strip group and world access from every file a
    // Replace-in-Files run rewrites, across a whole tree, unattended.
    // A matched shell script would quietly lose its execute bit. The
    // mode comes from the `fstat` in `read_capped` that already
    // validated this file, so it is the mode of the thing actually
    // read, not of whatever the name resolves to now.
    let mut tmp = tempfile::Builder::new()
        .prefix(".codepp-fif-")
        .suffix(".tmp")
        .permissions(perms.clone())
        .tempfile_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file_mut().sync_data()?;
    tmp.persist(target).map_err(|e| e.error)?;
    Ok(())
}

#[derive(Debug)]
enum ReadCappedError {
    Oversize,
    /// The opened descriptor is not a regular file — a FIFO, a device,
    /// or a directory. Determined by `fstat` on the handle rather than
    /// by a fresh lookup on the path, so it cannot be raced: see
    /// [`read_capped`]. Treated like any other unreadable file, which
    /// is what the walker's own symlink policy already implies.
    NotARegularFile,
    Io(std::io::Error),
}

/// Read a candidate file, classifying every refusal into the right
/// statistic or log line.
///
/// Split out of `worker_main` so the error taxonomy sits next to
/// [`read_capped`], which decides it, rather than padding the worker
/// loop. Returns `None` for every "skip this file" outcome; the caller
/// just continues.
fn read_for_search(
    path: &std::path::Path,
    cap: u64,
    stats: &FifStatsAtomic,
) -> Option<(Vec<u8>, std::fs::Permissions)> {
    match read_capped(path, cap) {
        Ok(pair) => Some(pair),
        Err(ReadCappedError::Oversize) => {
            stats.files_skipped_size.fetch_add(1, Ordering::Relaxed);
            None
        }
        Err(ReadCappedError::NotARegularFile) => {
            // The entry changed type between the walker's check and
            // this open — a FIFO or device now sits where a regular
            // file was. Warn rather than debug: on a tree nobody is
            // racing, this never fires, so if it does the user is
            // either searching something changing underneath them or
            // being attacked.
            tracing::warn!(
                path = ?path,
                "fif worker: entry is no longer a regular file; skipping",
            );
            None
        }
        // `ErrorKind::FilesystemLoop` would read better but is still
        // unstable on the pinned toolchain, so match the errno. Unix
        // only, which costs nothing: `O_NOFOLLOW` is unix only too, so
        // this is the only platform that can produce ELOOP here.
        #[cfg(unix)]
        Err(ReadCappedError::Io(e)) if e.raw_os_error() == Some(libc::ELOOP) => {
            // `O_NOFOLLOW` refused the open: the entry became a
            // symlink after the walker vetted it. Same reasoning as
            // the branch above — on a tree nobody is racing this never
            // fires, so it earns the same visibility as the other
            // swap, not the routine-IO treatment.
            tracing::warn!(
                path = ?path,
                "fif worker: entry became a symlink after enumeration; skipping",
            );
            None
        }
        Err(ReadCappedError::Io(e)) => {
            tracing::debug!(path = ?path, error = ?e, "fif worker: read failed");
            None
        }
    }
}

/// Read the file at `path`, refusing if it would exceed `cap` bytes.
/// Bounds memory use against a runaway file even if the metadata
/// length read by the walker disagreed with the post-open size.
fn read_capped(
    path: &std::path::Path,
    cap: u64,
) -> Result<(Vec<u8>, std::fs::Permissions), ReadCappedError> {
    use std::io::Read;

    // The walker already rejected symlinks, FIFOs and devices when it
    // enumerated this path — but that check was against the *path*,
    // and an unbounded amount of time passes while the path sits in
    // the bounded channel waiting for a worker. Anything able to write
    // to the directory can swap the entry in that window, so the
    // decision has to be re-made against the thing actually opened.
    //
    // Two independent guards:
    //
    // 1. The open itself refuses to follow a reparse point at the
    //    final path component, so a swapped-in symlink or Windows
    //    junction hands us a descriptor to the reparse point itself
    //    (which then fails guard 2) rather than to whatever it names.
    //    * Unix: `O_NOFOLLOW` makes the kernel refuse the open
    //      outright — no descriptor at all. `O_NONBLOCK` is paired
    //      because opening a FIFO for reading otherwise blocks
    //      *inside `open` itself* until a writer appears, and the
    //      type check below would sit after the hang it exists to
    //      prevent (exactly what happened to the first version of
    //      this fix, caught by the FIFO test).
    //    * Windows: `FILE_FLAG_OPEN_REPARSE_POINT` (`0x00200000`)
    //      tells `CreateFileW` to open the reparse point itself
    //      rather than following it. `Metadata::file_type()` reads
    //      `FILE_ATTRIBUTE_REPARSE_POINT` and, for a reparse point,
    //      makes a second handle-bound
    //      `GetFileInformationByHandleEx(FileAttributeTagInfo)` call
    //      so `is_symlink()` reflects the reparse tag's
    //      name-surrogate bit specifically (not every reparse tag —
    //      OneDrive placeholders and other non-surrogate tags stay
    //      classified as regular files, which is the correct
    //      behaviour here). The descriptor from a swapped-in
    //      symlink or junction therefore surfaces with
    //      `is_file() == false` and guard 2 refuses it as
    //      `NotARegularFile`. Named pipes and
    //      character devices reachable through the namespace do not
    //      block on open the way POSIX FIFOs do, so no `O_NONBLOCK`
    //      analogue is needed — if a future adversarial harness
    //      demonstrates otherwise, `FILE_FLAG_OVERLAPPED` +
    //      `GetFileType` screening is the shape to add.
    // 2. The file-type check below runs on `file.metadata()`, which
    //    is `fstat` on the open descriptor rather than a fresh
    //    lookup by name. It therefore describes exactly what was
    //    opened and cannot be raced. Rejecting anything that is not
    //    a regular file is what stops `read_to_end` blocking forever
    //    on a FIFO or a character device on Unix, and what catches
    //    the reparse-point-carrying descriptor on Windows.
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Bare integer rather than the `windows` crate's named
        // constant so the shell crate stays free of a Windows-only
        // dep pulled in just for one `u32`. Value is the documented
        // `FILE_FLAG_OPEN_REPARSE_POINT` from `winnt.h`, stable
        // since Windows 2000.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = opts.open(path).map_err(ReadCappedError::Io)?;

    // Strict where this used to be tolerant: the old code did
    // `if let Ok(meta)` and read the file anyway when `fstat` failed.
    // The type check is a safety guard now, not just a size
    // optimisation, so a failure to classify has to mean "skip",
    // never "proceed unchecked".
    let meta = file.metadata().map_err(ReadCappedError::Io)?;
    if !meta.file_type().is_file() {
        return Err(ReadCappedError::NotARegularFile);
    }
    if meta.len() > cap {
        return Err(ReadCappedError::Oversize);
    }
    // Carried out with the bytes so a replace can restore the mode it
    // found. Taken from the same `fstat` that validated the type, so
    // it describes the file actually opened rather than whatever the
    // name resolves to later.
    let perms = meta.permissions();

    let mut buf = Vec::new();
    let read = file
        .by_ref()
        .take(cap.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(ReadCappedError::Io)?;
    // A file that grew between the `fstat` above and the read still
    // gets caught here: `take` bounds the read at `cap + 1`, so
    // exceeding `cap` means there was more to come.
    if u64::try_from(read).map_or(true, |r| r > cap) {
        return Err(ReadCappedError::Oversize);
    }
    Ok((buf, perms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use std::fs;
    use std::time::Duration;
    use tempfile::tempdir;

    fn drain_until_done(rx: &Receiver<FifEvent>, deadline: Duration) -> Vec<FifEvent> {
        let mut out = Vec::new();
        let start = Instant::now();
        loop {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(50)) {
                let terminal = matches!(ev, FifEvent::Done { .. } | FifEvent::Cancelled { .. });
                out.push(ev);
                if terminal {
                    return out;
                }
            }
            assert!(
                start.elapsed() <= deadline,
                "timed out waiting for FIF terminal event after {:?}",
                start.elapsed()
            );
        }
    }

    #[test]
    fn dir_should_prune_recognises_known_basenames() {
        // walk_hidden_dirs = false (default): always-pruned + dot-prefixed.
        assert!(dir_should_prune(OsStr::new(".git"), false));
        assert!(dir_should_prune(OsStr::new(".idea"), false));
        assert!(dir_should_prune(OsStr::new(".future-vcs"), false));
        assert!(dir_should_prune(OsStr::new("target"), false));
        assert!(dir_should_prune(OsStr::new("node_modules"), false));
        assert!(!dir_should_prune(OsStr::new("src"), false));
        assert!(!dir_should_prune(OsStr::new("git"), false)); // no leading dot
    }

    #[test]
    fn walk_hidden_dirs_descends_into_dot_prefixed() {
        // walk_hidden_dirs = true: dot-prefixed names lose their
        // automatic prune; always-pruned still applies.
        assert!(!dir_should_prune(OsStr::new(".git"), true));
        assert!(!dir_should_prune(OsStr::new(".idea"), true));
        assert!(dir_should_prune(OsStr::new("target"), true));
        assert!(dir_should_prune(OsStr::new("node_modules"), true));
    }

    #[test]
    fn invalid_query_is_rejected_before_threads_spawn() {
        let (tx, _rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();
        let req = FifRequest {
            query: String::new(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        };
        let err = orch.start(req).unwrap_err();
        assert!(matches!(err, FifError::Query(_)));
        assert!(orch.current.is_none());
    }

    #[test]
    fn missing_root_is_rejected() {
        // Construct a path that's guaranteed not to exist on any OS
        // by joining a unique subdir under a tempdir. The tempdir
        // itself exists; the joined subdir does not — `is_dir()`
        // returns false on Windows, Linux, and macOS alike.
        let dir = tempdir().unwrap();
        let bad = dir.path().join("does-not-exist-fif-root");
        let (tx, _rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let req = FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: bad,
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        };
        let err = orch.start(req).unwrap_err();
        assert!(matches!(err, FifError::BadRoot(_)));
    }

    #[test]
    fn empty_directory_completes_with_zero_results() {
        let (tx, rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();
        let req = FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        };
        let job = orch.start(req).unwrap();
        let events = drain_until_done(&rx, Duration::from_secs(5));
        assert_eq!(events.len(), 1);
        match &events[0] {
            FifEvent::Done {
                job: j,
                stats:
                    FifStats {
                        files_scanned: 0,
                        files_with_matches: 0,
                        ..
                    },
            } => {
                assert_eq!(*j, job);
            }
            other => panic!("expected Done with zero stats, got {other:?}"),
        }
    }

    #[test]
    fn finds_matches_across_files_and_skips_binaries() {
        let (tx, rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();

        fs::write(dir.path().join("a.txt"), "alpha\nneedle here\nbeta\n").unwrap();
        fs::write(dir.path().join("b.txt"), "no match here\n").unwrap();
        fs::write(dir.path().join("c.txt"), "needle once\nneedle twice\n").unwrap();
        // Binary file: NUL byte in the first 8 KiB triggers the
        // skip path, even though the literal matches.
        fs::write(dir.path().join("d.bin"), b"needle\x00needle\n").unwrap();

        let req = FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        };
        let _ = orch.start(req).unwrap();
        let events = drain_until_done(&rx, Duration::from_secs(5));

        // Find the Done event and verify counts; collect the file
        // paths from FileMatches events along the way.
        let mut paths: Vec<String> = Vec::new();
        let mut done_stats: Option<FifStats> = None;
        for ev in events {
            match ev {
                FifEvent::FileMatches { path, .. } => {
                    paths.push(path.file_name().unwrap().to_string_lossy().into_owned());
                }
                FifEvent::Done { stats, .. } => done_stats = Some(stats),
                FifEvent::Cancelled { .. } => panic!("unexpected cancel"),
                // This job is search-only with no open files, so
                // neither can occur. Named rather than wildcarded so a
                // future variant has to be considered here.
                FifEvent::ReplaceInOpenBuffer { .. } | FifEvent::ReplacedInOpenBuffer { .. } => {
                    panic!("search-only job emitted an open-buffer replacement")
                }
            }
        }
        let stats = done_stats.expect("no Done event");
        assert_eq!(stats.files_with_matches, 2, "a.txt + c.txt");
        assert_eq!(stats.files_skipped_binary, 1, "d.bin");
        assert_eq!(stats.total_matches, 3, "1 in a + 2 in c");
        paths.sort();
        assert_eq!(paths, vec!["a.txt", "c.txt"]);
    }

    #[test]
    fn recurses_into_subdirectories() {
        let (tx, rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();
        let sub = dir.path().join("nested").join("deep");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("buried.txt"), "needle deep down\n").unwrap();

        let req = FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        };
        let _ = orch.start(req).unwrap();
        let events = drain_until_done(&rx, Duration::from_secs(5));
        let hit = events
            .iter()
            .any(|e| matches!(e, FifEvent::FileMatches { .. }));
        assert!(hit, "expected a match from a nested file");
    }

    #[test]
    fn pruned_dir_basenames_are_not_descended() {
        let (tx, rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();
        // .git contains a hit but should never be descended.
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(
            dir.path().join(".git").join("config"),
            "needle inside .git\n",
        )
        .unwrap();
        fs::write(dir.path().join("real.txt"), "needle outside\n").unwrap();

        let req = FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        };
        let _ = orch.start(req).unwrap();
        let events = drain_until_done(&rx, Duration::from_secs(5));
        let mut hits = 0usize;
        for ev in events {
            if let FifEvent::FileMatches { path, .. } = ev {
                assert!(!path.to_string_lossy().contains(".git"));
                hits += 1;
            }
        }
        assert_eq!(hits, 1);
    }

    #[test]
    fn second_start_preempts_the_prior_job() {
        // What preemption actually guarantees, per this module's
        // header: a new `start` flips the prior job's cancel flag, the
        // walker exits at its next entry-loop boundary, workers exit
        // between files, and the coordinator emits exactly one terminal
        // event per started job.
        //
        // Note what that does NOT guarantee: that the prior job's
        // terminal event is `Cancelled`. A job that has already
        // finished its walk observes the flag only after emitting
        // `Done`, and that is legal, documented behaviour — the flag is
        // a request to stop early, not a promise that the job had work
        // left. An earlier version of this test asserted `Cancelled`
        // and was flaky at roughly one run in three, because on a fast
        // machine the first job really can finish a 200-file tree
        // before the second `start` lands. Enlarging the corpus would
        // only move that threshold, not remove it.
        //
        // So assert the two things that are deterministic — the flag is
        // set, and each job terminates exactly once — and accept either
        // terminal variant for the preempted job.
        let (tx, rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();
        for i in 0..200 {
            fs::write(
                dir.path().join(format!("f{i:03}.txt")),
                format!("needle line {i}\n"),
            )
            .unwrap();
        }
        let make_req = || FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        };

        let id1 = orch.start(make_req()).unwrap();
        // Clone the first job's cancel handle before the second
        // `start` takes it out of the orchestrator — this is the flag
        // preemption is defined in terms of.
        let job1_cancel = orch
            .current
            .as_ref()
            .expect("a started job must be current")
            .cancel
            .clone();
        assert!(
            !job1_cancel.load(Ordering::Acquire),
            "a freshly started job must not already be cancelled"
        );

        let id2 = orch.start(make_req()).unwrap();
        assert_ne!(id1, id2);
        // The deterministic half of the contract, independent of how
        // far job 1 got.
        assert!(
            job1_cancel.load(Ordering::Acquire),
            "starting a second job must flip the first job's cancel flag"
        );

        // The other deterministic half: exactly one terminal event per
        // started job.
        let mut terminals: Vec<FifJobId> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while terminals.len() < 2 && Instant::now() < deadline {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                match ev {
                    FifEvent::Done { job, .. } | FifEvent::Cancelled { job, .. } => {
                        assert!(
                            !terminals.contains(&job),
                            "job {job:?} emitted a second terminal event"
                        );
                        terminals.push(job);
                    }
                    // Non-terminal. Matched explicitly rather than
                    // with a wildcard so a future event variant has
                    // to be classified here rather than silently
                    // falling through as "not a terminal".
                    FifEvent::FileMatches { .. }
                    | FifEvent::ReplaceInOpenBuffer { .. }
                    | FifEvent::ReplacedInOpenBuffer { .. } => {}
                }
            }
        }
        assert!(
            terminals.contains(&id1) && terminals.contains(&id2),
            "both jobs must terminate exactly once; saw {terminals:?}"
        );
    }

    #[test]
    fn rejects_when_active_jobs_at_ceiling() {
        // Simulates the "prior worker stuck in a blocking syscall"
        // case: the coordinator hasn't decremented yet, so the
        // counter sits at the ceiling. Hostile in-process callers
        // get a clean error instead of an unbounded thread spawn.
        let (tx, _rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        orch.active_jobs
            .store(MAX_ACTIVE_JOBS, std::sync::atomic::Ordering::Release);
        let dir = tempdir().unwrap();
        let req = FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        };
        let err = orch.start(req).unwrap_err();
        assert!(matches!(err, FifError::TooManyJobs));
        // No threads were spawned, no `current` job was registered.
        assert!(orch.current.is_none());
    }

    #[test]
    fn read_capped_refuses_oversize_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("big.txt");
        fs::write(&p, "x".repeat(2048)).unwrap();
        let err = read_capped(&p, 1024).unwrap_err();
        assert!(matches!(err, ReadCappedError::Oversize));
        // Same file under a generous cap reads cleanly.
        let (buf, _perms) = read_capped(&p, 4096).unwrap();
        assert_eq!(buf.len(), 2048);
    }

    #[test]
    fn replace_in_files_rewrites_matching_files() {
        let (tx, rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "alpha needle gamma\n").unwrap();
        fs::write(dir.path().join("b.txt"), "no match here\n").unwrap();
        fs::write(
            dir.path().join("c.txt"),
            "needle once\nneedle twice\nneedle thrice\n",
        )
        .unwrap();

        let req = FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: Some("HAYSTACK".into()),
            open_tab_paths: Vec::new(),
        };
        let _ = orch.start(req).unwrap();
        let events = drain_until_done(&rx, Duration::from_secs(5));

        // Verify the two matching files were rewritten and the
        // non-matching file is untouched.
        let a = fs::read_to_string(dir.path().join("a.txt")).unwrap();
        let b = fs::read_to_string(dir.path().join("b.txt")).unwrap();
        let c = fs::read_to_string(dir.path().join("c.txt")).unwrap();
        assert_eq!(a, "alpha HAYSTACK gamma\n");
        assert_eq!(b, "no match here\n");
        assert_eq!(c, "HAYSTACK once\nHAYSTACK twice\nHAYSTACK thrice\n");

        // Done event reports `files_modified == 2`.
        let done_stats = events
            .into_iter()
            .find_map(|e| match e {
                FifEvent::Done { stats, .. } => Some(stats),
                _ => None,
            })
            .expect("no Done event");
        assert_eq!(done_stats.files_modified, 2);
        assert_eq!(done_stats.total_matches, 4);
    }

    #[test]
    fn replace_skips_files_open_in_a_tab() {
        let (tx, rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();
        let kept = dir.path().join("kept.txt");
        let modified = dir.path().join("modified.txt");
        fs::write(&kept, "needle in user buffer\n").unwrap();
        fs::write(&modified, "needle on disk\n").unwrap();

        let req = FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: Some("HAY".into()),
            open_tab_paths: vec![kept.clone()],
        };
        let _ = orch.start(req).unwrap();
        let _events = drain_until_done(&rx, Duration::from_secs(5));

        // The open file is left untouched; the other was rewritten.
        assert_eq!(
            fs::read_to_string(&kept).unwrap(),
            "needle in user buffer\n"
        );
        assert_eq!(fs::read_to_string(&modified).unwrap(), "HAY on disk\n");
    }

    #[test]
    fn replace_atomic_write_leaves_no_temp_on_success() {
        let (tx, rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("only.txt"), "needle\n").unwrap();

        let req = FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: Some("X".into()),
            open_tab_paths: Vec::new(),
        };
        let _ = orch.start(req).unwrap();
        let _events = drain_until_done(&rx, Duration::from_secs(5));

        // After a successful replace, no temp file lingers.
        //
        // Asserted against the directory's exact expected contents
        // rather than by pattern. The previous version searched for
        // the literal `.codepp-fif.tmp`, which the randomised naming
        // (`.codepp-fif-<random>.tmp`) can never contain as a
        // contiguous substring — so it had become unconditionally
        // true and would have passed even if every temp leaked.
        let mut entries: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            vec!["only.txt".to_string()],
            "the directory must contain exactly the replaced file and nothing else",
        );
        for s in &entries {
            assert!(!s.starts_with(".codepp-fif-"), "leftover temp file: {s}");
        }
    }

    /// A directory with many subdirectories must not cost memory
    /// proportional to how many there are.
    ///
    /// This is the shape the walker used to be unbounded on: it pushed
    /// every subdirectory of a directory onto a `Vec<PathBuf>` before
    /// descending into any, so one wide directory pinned one allocation
    /// per child. The lazy `ReadDir` stack holds one iterator per level
    /// instead, so this tree costs two regardless of the fan-out.
    ///
    /// 500 subdirectories is small enough to keep the test fast and
    /// large enough that the old code would have held 500 paths at
    /// once; the property being pinned is "flat in the fan-out", which
    /// the assertions below check by content rather than by measuring
    /// allocations.
    #[test]
    fn wide_tree_is_walked_completely() {
        let (tx, rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();
        for i in 0..500 {
            let sub = dir.path().join(format!("d{i:03}"));
            fs::create_dir(&sub).unwrap();
            fs::write(sub.join("f.txt"), "needle\n").unwrap();
        }

        orch.start(FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        })
        .unwrap();

        let events = drain_until_done(&rx, Duration::from_secs(30));
        let hits = events
            .iter()
            .filter(|e| matches!(e, FifEvent::FileMatches { .. }))
            .count();
        assert_eq!(hits, 500, "every subdirectory must still be visited");
    }

    /// Depth is what bounds the walk now, so the walker must actually
    /// descend to a realistic nesting level and find the file at the
    /// bottom.
    ///
    /// Also guards the loop's climb-out: an exhausted level has to be
    /// popped so its descriptor closes and its parent resumes. Get that
    /// wrong and this either misses the deep file or spins.
    #[test]
    fn deeply_nested_tree_is_walked_to_the_bottom() {
        let (tx, rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();

        // Well inside MAX_WALK_DEPTH, and deep enough that a
        // per-level descriptor leak would be obvious.
        const DEPTH: usize = 60;
        let mut deep = dir.path().to_path_buf();
        for i in 0..DEPTH {
            deep = deep.join(format!("l{i}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("bottom.txt"), "needle\n").unwrap();
        // A sibling at the top level, so the test also proves the
        // walker climbs back out and resumes the parent rather than
        // stopping once it bottoms out.
        fs::write(dir.path().join("top.txt"), "needle\n").unwrap();

        orch.start(FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        })
        .unwrap();

        let events = drain_until_done(&rx, Duration::from_secs(30));
        let mut found: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                FifEvent::FileMatches { path, .. } => {
                    Some(path.file_name()?.to_string_lossy().into_owned())
                }
                _ => None,
            })
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec!["bottom.txt".to_string(), "top.txt".to_string()],
            "the walker must reach the deepest file and still resume the root level"
        );
    }

    /// The depth cap must stop the descent at exactly the right level,
    /// and must say so.
    ///
    /// Puts a marker file at *every* level of an over-deep tree and
    /// asserts the deepest one found is exactly the last level the cap
    /// allows. Checking the precise boundary rather than "something
    /// deep was missed" is deliberate: an earlier version of this test
    /// placed the deep file ten levels past the cap and therefore
    /// passed even when `>=` was mutated to `>`, which is precisely
    /// the off-by-one a boundary test exists to catch.
    ///
    /// Also asserts the run is flagged incomplete. That is the point of
    /// the flag — a search that quietly returns fewer results than the
    /// truth is worse than a slow one, and before `depth_cap_hit`
    /// existed nothing distinguished the two.
    ///
    /// Feasible as a real on-disk tree only because the cap is 64. At
    /// the 512 an earlier draft used, this would have needed a tree
    /// deep enough to trip Windows' legacy `MAX_PATH`.
    #[test]
    fn depth_cap_stops_at_the_exact_boundary_and_reports_incompleteness() {
        let (tx, rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();

        // A marker at every level, a few past the cap.
        let over = MAX_WALK_DEPTH + 5;
        let mut path = dir.path().to_path_buf();
        for level in 1..=over {
            path = path.join(format!("l{level:03}"));
            fs::create_dir(&path).unwrap();
            fs::write(path.join(format!("m{level:03}.txt")), "needle\n").unwrap();
        }

        orch.start(FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        })
        .unwrap();

        let events = drain_until_done(&rx, Duration::from_mins(1));
        let mut levels: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                FifEvent::FileMatches { path, .. } => {
                    let name = path.file_name()?.to_string_lossy().into_owned();
                    name.strip_prefix('m')?.strip_suffix(".txt")?.parse().ok()
                }
                _ => None,
            })
            .collect();
        levels.sort_unstable();

        // The walk starts with the root's own iterator on the stack, so
        // a directory at level N is opened only while the stack holds
        // N entries; the cap refuses the push once the stack has
        // reached MAX_WALK_DEPTH. The deepest *entered* directory is
        // therefore level MAX_WALK_DEPTH - 1, and its marker is the
        // deepest file that can be found.
        let deepest_reachable = MAX_WALK_DEPTH - 1;
        assert_eq!(
            levels,
            (1..=deepest_reachable).collect::<Vec<_>>(),
            "every level up to the cap must be searched and nothing beyond it"
        );

        let flagged = events.iter().any(|e| match e {
            FifEvent::Done { stats, .. } => stats.depth_cap_hit,
            _ => false,
        });
        assert!(
            flagged,
            "skipping a subtree must set depth_cap_hit so the UI can say results are incomplete"
        );
    }

    /// `read_capped` must refuse anything that is not a regular file,
    /// decided from the open descriptor rather than from the path.
    ///
    /// The walker screens symlinks, FIFOs and devices when it
    /// enumerates, but that check is by name and an unbounded amount
    /// of time passes while the path waits in the bounded channel.
    /// Anything able to write to the directory can swap the entry in
    /// that window. A FIFO is the dangerous swap: `read_to_end` on one
    /// blocks until a writer appears, and the cancel flag cannot
    /// interrupt a blocking syscall — the whole reason the walker
    /// refuses to follow symlinks in the first place.
    #[cfg(unix)]
    #[test]
    fn read_capped_refuses_a_fifo() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path in a directory
        // this test owns; mkfifo has no other precondition.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);

        // Would hang forever without the guard, so the assertion is
        // as much about returning at all as about the variant.
        assert!(matches!(
            read_capped(&fifo, 1024),
            Err(ReadCappedError::NotARegularFile)
        ));
    }

    /// A path swapped for a symlink after enumeration must not be
    /// followed. `O_NOFOLLOW` makes the kernel refuse the open, so the
    /// worker never sees content from outside the search root.
    #[cfg(unix)]
    #[test]
    fn read_capped_refuses_a_symlink() {
        let dir = tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "content the search never asked for\n").unwrap();
        let link = dir.path().join("innocent.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let got = read_capped(&link, 1024);
        assert!(
            matches!(got, Err(ReadCappedError::Io(_))),
            "O_NOFOLLOW must make the open fail rather than follow the link"
        );
        // And the guard is specific, not a blanket refusal: the real
        // file behind it still reads fine.
        assert!(read_capped(&outside, 1024).is_ok());
    }

    /// Replace-in-Files must not write through a pre-placed symlink.
    ///
    /// `atomic_write` used to build a predictable temp name next to
    /// the target and open it with `File::create`, which follows
    /// symlinks and truncates. Anyone able to write to the directory
    /// being searched could pre-place that exact name pointing
    /// somewhere else and redirect the replacement's bytes out of the
    /// tree. `tempfile` picks an unpredictable name and creates it
    /// `O_EXCL`, which POSIX requires to fail on a symlink.
    #[cfg(unix)]
    #[test]
    fn atomic_write_cannot_be_redirected_by_a_planted_temp_symlink() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("victim.txt");
        fs::write(&target, "original\n").unwrap();

        let elsewhere = dir.path().join("attacker_target.txt");
        fs::write(&elsewhere, "must not be overwritten\n").unwrap();

        // The name the old implementation would have used.
        let predictable = dir.path().join("victim.txt.codepp-fif.tmp");
        std::os::unix::fs::symlink(&elsewhere, &predictable).unwrap();

        let perms = fs::metadata(&target).unwrap().permissions();
        atomic_write(&target, b"replaced\n", &perms).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "replaced\n");
        assert_eq!(
            fs::read_to_string(&elsewhere).unwrap(),
            "must not be overwritten\n",
            "the replacement must not have been redirected through the planted symlink"
        );
    }

    /// Windows equivalent of the Unix `read_capped_refuses_a_symlink`
    /// (which is the file-symlink case), and the one test that
    /// actually regression-covers the new
    /// `FILE_FLAG_OPEN_REPARSE_POINT` code path: neutralising that
    /// flag would let `CreateFileW` follow the symlink and return
    /// bytes from `outside.txt`, which this test's `matches!` would
    /// catch as an `Ok(_)` where a reject was required.
    ///
    /// **`#[ignore]` because the setup needs
    /// `SeCreateSymbolicLinkPrivilege`.** Windows only grants that
    /// privilege to elevated shells or to processes running under
    /// developer mode; a runner without either sees
    /// `symlink_file` return `ERROR_PRIVILEGE_NOT_HELD` (1314). The
    /// earlier revision of this test tried to skip that quietly
    /// with an `eprintln!`, but `cargo test`'s default output
    /// capture hides the message on a pass, so a CI runner without
    /// developer mode would go on reporting fully green while
    /// silently running no assertions at all — proven empirically
    /// during the security audit by neutralising
    /// `FILE_FLAG_OPEN_REPARSE_POINT` in the source and observing
    /// the suite still pass. `#[ignore]` puts the skip in the test
    /// summary where it can't be missed. Windows runners set up
    /// per `docs/DEVELOPMENT.md` §2 with developer mode should run
    /// `cargo test -- --include-ignored` (or `--ignored`) to
    /// exercise this.
    ///
    /// The junction test below is not a substitute: it exercises a
    /// directory reparse point, which `std::fs::OpenOptions`
    /// already refuses via `ERROR_ACCESS_DENIED` (directories need
    /// `FILE_FLAG_BACKUP_SEMANTICS` to open at all) regardless of
    /// whether `FILE_FLAG_OPEN_REPARSE_POINT` is set. Still worth
    /// keeping as a belt-and-suspenders "worker never returns
    /// bytes from outside the search root," but only this test
    /// pins the fix itself.
    #[cfg(windows)]
    #[test]
    #[ignore = "needs SeCreateSymbolicLinkPrivilege — Windows developer mode; \
                run with `cargo test -- --include-ignored` on a properly \
                provisioned runner. See docs/DEVELOPMENT.md §2."]
    fn read_capped_refuses_a_symlink() {
        let dir = tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "content the search never asked for\n").unwrap();
        let link = dir.path().join("innocent.txt");
        // With `#[ignore]` gating this test, a
        // `SeCreateSymbolicLinkPrivilege` failure here means the
        // runner is misconfigured — panic loudly rather than skip.
        std::os::windows::fs::symlink_file(&outside, &link)
            .expect("symlink_file failed — is developer mode on?");

        let got = read_capped(&link, 1024);
        assert!(
            matches!(got, Err(ReadCappedError::NotARegularFile)),
            "FILE_FLAG_OPEN_REPARSE_POINT must surface the reparse point \
             itself so is_file() rejects it; got {got:?}"
        );
        // The guard is specific, not a blanket refusal: the real
        // file behind it still reads fine.
        assert!(read_capped(&outside, 1024).is_ok());
    }

    /// A directory junction sitting in the search root must be
    /// refused the same way a symlink is — junctions are reparse
    /// points too, they just point at directories instead of
    /// files. Unlike file symlinks, junctions are user-creatable
    /// on Windows without any privilege (they were originally
    /// added for the Distributed File System and predate the
    /// symlink privilege model), so this test runs on every
    /// Windows CI runner regardless of developer-mode state.
    ///
    /// Two acceptable reject paths, both valid — the security
    /// invariant is "worker returns `Err` and never surfaces bytes
    /// from outside the search root":
    ///   * `NotARegularFile` — the reparse point opened but
    ///     `is_file()` was false and guard 2 refused it.
    ///   * `Io(PermissionDenied)` — the open itself failed. This
    ///     is what actually happens for a junction: without
    ///     `FILE_FLAG_BACKUP_SEMANTICS`, `CreateFileW` on a
    ///     directory reparse point returns `ERROR_ACCESS_DENIED`
    ///     (5). Guard 1 has already done its job — the descriptor
    ///     never carried a follow-through to the target directory.
    ///
    /// Also confirms the guard fires against a reparse point that
    /// CI runners in the wild can actually plant — a symlink swap
    /// requires elevated privileges, a junction swap does not, so
    /// junctions are the higher-likelihood adversarial shape here.
    #[cfg(windows)]
    #[test]
    fn read_capped_refuses_a_junction() {
        let dir = tempdir().unwrap();
        // Real directory the junction points at.
        let target_dir = dir.path().join("outside");
        fs::create_dir(&target_dir).unwrap();
        // A file inside it — proves the target is a live tree; the
        // junction itself is what read_capped opens, not this file.
        fs::write(target_dir.join("outside.txt"), "not for the search\n").unwrap();

        // `mklink /J <link> <target>` creates a directory junction.
        // Runs under `cmd /c` since mklink is a cmd builtin, not a
        // standalone executable.
        let junction = dir.path().join("innocent");
        let status = std::process::Command::new("cmd")
            .args([
                "/c",
                "mklink",
                "/J",
                junction.to_str().expect("tempdir path is UTF-8"),
                target_dir.to_str().expect("tempdir path is UTF-8"),
            ])
            .output()
            .expect("cmd /c mklink /J must be available");
        assert!(
            status.status.success(),
            "mklink /J failed: stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        );

        let got = read_capped(&junction, 1024);
        // See the doc comment above for why both variants are
        // acceptable rejects. What is *not* acceptable is `Ok(_)` —
        // that would mean the worker had followed the junction and
        // read content from outside the search root.
        let is_access_denied = matches!(
            &got,
            Err(ReadCappedError::Io(e))
                if e.kind() == std::io::ErrorKind::PermissionDenied
        );
        let is_wrong_type = matches!(&got, Err(ReadCappedError::NotARegularFile));
        assert!(
            is_wrong_type || is_access_denied,
            "FILE_FLAG_OPEN_REPARSE_POINT must stop the worker \
             following the junction — either by refusing the open \
             (Io(PermissionDenied)) or by surfacing the reparse \
             point so is_file() rejects it (NotARegularFile); got \
             {got:?}"
        );
    }

    /// Windows equivalent of the Unix
    /// `atomic_write_cannot_be_redirected_by_a_planted_temp_symlink`
    /// — pins the same "the switch to `tempfile`'s randomly-named
    /// temp closes the predictable-name attack the old
    /// `File::create` path was vulnerable to" property, on the
    /// Windows path. Does *not* test the separate
    /// `MoveFileExW`-replaces-a-destination-reparse-point
    /// property — that's what
    /// `atomic_write_replaces_a_symlink_at_the_destination`
    /// below covers.
    ///
    /// `#[ignore]`d for the same setup reason as
    /// `read_capped_refuses_a_symlink` above — see that
    /// docstring for the full rationale.
    #[cfg(windows)]
    #[test]
    #[ignore = "needs SeCreateSymbolicLinkPrivilege — Windows developer mode; \
                run with `cargo test -- --include-ignored` on a properly \
                provisioned runner. See docs/DEVELOPMENT.md §2."]
    fn atomic_write_cannot_be_redirected_by_a_planted_temp_symlink() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("victim.txt");
        fs::write(&target, "original\n").unwrap();

        let elsewhere = dir.path().join("attacker_target.txt");
        fs::write(&elsewhere, "must not be overwritten\n").unwrap();

        // The name the old implementation would have used.
        let predictable = dir.path().join("victim.txt.codepp-fif.tmp");
        std::os::windows::fs::symlink_file(&elsewhere, &predictable)
            .expect("symlink_file failed — is developer mode on?");

        let perms = fs::metadata(&target).unwrap().permissions();
        atomic_write(&target, b"replaced\n", &perms).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "replaced\n");
        assert_eq!(
            fs::read_to_string(&elsewhere).unwrap(),
            "must not be overwritten\n",
            "the replacement must not have been redirected through the planted symlink"
        );
    }

    /// A `target` that is (or gets swapped for) a symlink at the
    /// moment `atomic_write` runs must be *replaced*, not written
    /// through — anything else would let anyone able to write into
    /// the search-root directory redirect the replacement bytes to
    /// an arbitrary victim path via a pre-placed symlink at a
    /// filename FIF is about to rewrite. The property is a
    /// documented `MoveFileExW` invariant on Windows and an equally
    /// documented POSIX `rename` invariant on Unix, but until this
    /// test landed it was asserted in the `atomic_write` docstring
    /// on both platforms without being exercised (DESIGN.md §7.4
    /// flagged the Windows half specifically).
    ///
    /// Unix version — creates `target` directly as a symlink to
    /// `elsewhere` (no privilege needed on Unix), then asserts
    /// after `atomic_write` that `target` reads as a regular file
    /// with the replacement content and `elsewhere` still carries
    /// its original content. Also asserts that `target` is *no
    /// longer* a symlink post-write, since "replaces the symlink"
    /// is the load-bearing half of the invariant — a
    /// hypothetical implementation that opened through the
    /// symlink but wrote to a same-name target would satisfy the
    /// content asserts alone.
    #[cfg(unix)]
    #[test]
    fn atomic_write_replaces_a_symlink_at_the_destination() {
        let dir = tempdir().unwrap();

        // The file the symlink points at — must survive untouched.
        let elsewhere = dir.path().join("elsewhere.txt");
        fs::write(&elsewhere, "the symlink's target must survive\n").unwrap();

        // `target` IS the symlink. `atomic_write(target, …)` must
        // replace it in-place rather than following it to
        // `elsewhere`.
        let target = dir.path().join("target.txt");
        std::os::unix::fs::symlink(&elsewhere, &target).unwrap();

        // A conservative default — `read_capped` normally supplies
        // the perms from its `fstat` on the read side, but this
        // test isn't exercising that pipeline.
        let perms = fs::metadata(&elsewhere).unwrap().permissions();
        atomic_write(&target, b"replaced\n", &perms).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "replaced\n");
        assert_eq!(
            fs::read_to_string(&elsewhere).unwrap(),
            "the symlink's target must survive\n",
            "atomic_write followed the symlink instead of replacing it"
        );
        assert!(
            !fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink(),
            "target should be a regular file after the replace, not a symlink"
        );
    }

    /// Windows twin of `atomic_write_replaces_a_symlink_at_the_destination`.
    /// The specific gap DESIGN.md §7.4 flagged: the `atomic_write`
    /// docstring asserted `NamedTempFile::persist` on Windows
    /// (which calls `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`)
    /// replaces a destination reparse point rather than following
    /// it, but nothing exercised it. This test closes that gap.
    ///
    /// `#[ignore]`d because the setup needs
    /// `SeCreateSymbolicLinkPrivilege` — see
    /// `read_capped_refuses_a_symlink`'s docstring for the full
    /// developer-mode rationale.
    #[cfg(windows)]
    #[test]
    #[ignore = "needs SeCreateSymbolicLinkPrivilege — Windows developer mode; \
                run with `cargo test -- --include-ignored` on a properly \
                provisioned runner. See docs/DEVELOPMENT.md §2."]
    fn atomic_write_replaces_a_symlink_at_the_destination() {
        let dir = tempdir().unwrap();

        let elsewhere = dir.path().join("elsewhere.txt");
        fs::write(&elsewhere, "the symlink's target must survive\n").unwrap();

        let target = dir.path().join("target.txt");
        std::os::windows::fs::symlink_file(&elsewhere, &target)
            .expect("symlink_file failed — is developer mode on?");

        let perms = fs::metadata(&elsewhere).unwrap().permissions();
        atomic_write(&target, b"replaced\n", &perms).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "replaced\n");
        assert_eq!(
            fs::read_to_string(&elsewhere).unwrap(),
            "the symlink's target must survive\n",
            "atomic_write followed the symlink instead of replacing it"
        );
        assert!(
            !fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink(),
            "target should be a regular file after the replace, not a symlink"
        );
    }

    /// Replace-in-Files must preserve each file's mode.
    ///
    /// `tempfile` creates at 0600 unless told otherwise, and because
    /// the replacement is renamed over the original, the
    /// replacement's mode is the one that survives. Left unset, a
    /// single Replace-in-Files run would silently strip group and
    /// world access from every file it rewrote across a whole tree —
    /// a matched shell script would quietly lose its execute bit.
    /// That regression rode in with the switch away from
    /// `File::create` and is exactly what this pins.
    #[cfg(unix)]
    #[test]
    fn replace_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (tx, rx) = unbounded();
        let mut orch = FifOrchestrator::new(tx);
        let dir = tempdir().unwrap();

        // An executable script and a world-readable data file: the two
        // modes a blanket 0600 would visibly damage.
        let script = dir.path().join("run.sh");
        fs::write(&script, "#!/bin/sh\nneedle\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let data = dir.path().join("data.txt");
        fs::write(&data, "needle\n").unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o644)).unwrap();

        orch.start(FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: Some("HAY".into()),
            open_tab_paths: Vec::new(),
        })
        .unwrap();
        let _ = drain_until_done(&rx, Duration::from_secs(10));

        // Sanity: the replacement actually happened, so the mode
        // assertions below are about rewritten files.
        assert!(fs::read_to_string(&script).unwrap().contains("HAY"));
        assert!(fs::read_to_string(&data).unwrap().contains("HAY"));

        let script_mode = fs::metadata(&script).unwrap().permissions().mode() & 0o777;
        let data_mode = fs::metadata(&data).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            script_mode, 0o755,
            "the executable bit must survive a replace"
        );
        assert_eq!(data_mode, 0o644, "group/world read must survive a replace");
    }
}
