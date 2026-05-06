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
//!   Higher fan-out than 16 is counter-productive on typical NVMe
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

/// Identifier for a single FIF run. Monotonic per-shell so the UI
/// can discard events from a job preempted by a newer
/// [`FifOrchestrator::start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FifJobId(u64);

impl FifJobId {
    /// Raw counter value, useful for `tracing` spans and diagnostics.
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
    /// Job ran to completion (walker exhausted, all workers idle).
    /// Stats include the elapsed wall time the walker took.
    Done { job: FifJobId, stats: FifStats },
    /// Job was preempted by a new `start` call (or the shell shut
    /// down). No further events for this `job` will be emitted after
    /// this one.
    Cancelled { job: FifJobId },
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
            .map(|n| n.get())
            .unwrap_or(4)
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
                    )
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
    let mut stack: Vec<PathBuf> = vec![root];
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(dir = %dir.display(), error = %e, "fif walker: read_dir failed");
                continue;
            }
        };
        for entry in entries {
            // A per-entry error means the OS gave us a partial
            // result — most commonly permission-denied on one file
            // mid-enumeration. Skip the entry but log so the cause
            // is recoverable from a verbose trace.
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::debug!(
                        dir = %dir.display(),
                        error = %e,
                        "fif walker: dir entry error",
                    );
                    continue;
                }
            };
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let path = entry.path();
            let ftype = match entry.file_type() {
                Ok(t) => t,
                Err(e) => {
                    // Same treatment as the dir/entry errors above:
                    // a `file_type()` failure (broken symlink, racy
                    // permission change) is meaningful but never
                    // worth aborting the whole walk for.
                    tracing::debug!(
                        path = %path.display(),
                        error = %e,
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
                stack.push(path);
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
        let bytes = match read_capped(&path, walk.max_file_bytes) {
            Ok(b) => b,
            Err(ReadCappedError::Oversize) => {
                stats.files_skipped_size.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Err(ReadCappedError::Io(e)) => {
                tracing::debug!(path = %path.display(), error = %e, "fif worker: read failed");
                continue;
            }
        };
        let probe = &bytes[..bytes.len().min(BINARY_PROBE_BYTES)];
        if is_binary(probe) {
            stats.files_skipped_binary.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let (enc, body) = encoding::detect(&bytes);
        let text = match encoding::decode(body, &enc) {
            Ok(t) => t,
            Err(_) => {
                // Decode failure (e.g. corrupted UTF-8 body without a
                // BOM). Skip silently — N++'s FIF behaves the same.
                continue;
            }
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
                // TODO(m4 polish, DESIGN.md §7.4): apply the
                // replacement to the open tab's Scintilla buffer
                // instead of skipping. N++ does this so the user
                // gets undo + no watcher event, and the on-disk
                // file isn't out of sync with the editor.
                stats.files_skipped_open.fetch_add(1, Ordering::Relaxed);
                let _ = event_tx.send(FifEvent::FileMatches {
                    job: id,
                    path,
                    outcome,
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
                    if let Err(e) = atomic_write(&path, &new_bytes) {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
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
                        path = %path.display(),
                        error = %e,
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
/// crash mid-write. The temp file uses a `.codepp-fif.tmp` suffix
/// so a leftover from a killed process is recognizable. Same
/// cross-platform-rename caveat as everywhere else: on Windows,
/// `fs::rename` succeeds atomically when both paths are on the
/// same volume, which is guaranteed here since we generate the
/// temp path from the target's parent.
fn atomic_write(target: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target has no parent directory",
        )
    })?;
    let tmp = {
        let mut name = target
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(".codepp-fif.tmp");
        parent.join(name)
    };

    // Drop guard: removes the temp file unless `commit()` clears
    // the flag. Covers both the `?` early-returns (Err propagation)
    // and panic paths (e.g. `write_all` panic on a custom
    // allocator) so a partially-written temp doesn't survive on
    // disk to confuse a future re-run or accumulate as garbage.
    struct TempGuard<'a> {
        path: &'a std::path::Path,
        committed: bool,
    }
    impl Drop for TempGuard<'_> {
        fn drop(&mut self) {
            if !self.committed {
                let _ = std::fs::remove_file(self.path);
            }
        }
    }
    let mut guard = TempGuard {
        path: &tmp,
        committed: false,
    };

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_data()?;
    }
    std::fs::rename(&tmp, target)?;
    guard.committed = true;
    Ok(())
}

#[derive(Debug)]
enum ReadCappedError {
    Oversize,
    Io(std::io::Error),
}

/// Read the file at `path`, refusing if it would exceed `cap` bytes.
/// Bounds memory use against a runaway file even if the metadata
/// length read by the walker disagreed with the post-open size.
fn read_capped(path: &std::path::Path, cap: u64) -> Result<Vec<u8>, ReadCappedError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(ReadCappedError::Io)?;
    if let Ok(meta) = file.metadata() {
        if meta.len() > cap {
            return Err(ReadCappedError::Oversize);
        }
    }
    // Read at most cap+1 bytes so we can tell oversize vs. exact-cap.
    let mut buf = Vec::new();
    let read = file
        .by_ref()
        .take(cap.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(ReadCappedError::Io)?;
    // `usize → u64` widening is lossless on the project's 64-bit
    // targets, but `try_from` keeps the comparison correct on a
    // hypothetical 32-bit port where `usize` cannot represent
    // values past `u32::MAX`.
    if u64::try_from(read).map_or(true, |r| r > cap) {
        return Err(ReadCappedError::Oversize);
    }
    Ok(buf)
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
            if start.elapsed() > deadline {
                panic!(
                    "timed out waiting for FIF terminal event after {:?}",
                    start.elapsed()
                );
            }
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
    fn second_start_cancels_first() {
        // Build a tree large enough that a job can't complete
        // between back-to-back `start` calls. 200 small files is
        // enough on debug builds.
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

        let req1 = FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        };
        let id1 = orch.start(req1).unwrap();
        let req2 = FifRequest {
            query: "needle".into(),
            opts: FifQueryOpts::default(),
            root: dir.path().to_path_buf(),
            walk: FifWalkOpts::default(),
            replacement: None,
            open_tab_paths: Vec::new(),
        };
        let id2 = orch.start(req2).unwrap();
        assert_ne!(id1, id2);

        // Drain everything until both jobs have terminated.
        let mut id1_terminal = None;
        let mut id2_terminal = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while (id1_terminal.is_none() || id2_terminal.is_none()) && Instant::now() < deadline {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                match &ev {
                    FifEvent::Done { job, .. } | FifEvent::Cancelled { job, .. } => {
                        if *job == id1 {
                            id1_terminal = Some(matches!(ev, FifEvent::Cancelled { .. }));
                        } else if *job == id2 {
                            id2_terminal = Some(matches!(ev, FifEvent::Cancelled { .. }));
                        }
                    }
                    _ => {}
                }
            }
        }
        let id1_cancelled = id1_terminal.expect("job 1 never terminated");
        let _ = id2_terminal.expect("job 2 never terminated");
        assert!(
            id1_cancelled,
            "job 1 should have ended in Cancelled (preempted by job 2)"
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
        let buf = read_capped(&p, 4096).unwrap();
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

        // After a successful replace, no `.codepp-fif.tmp` lingers.
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        for name in &entries {
            let s = name.to_string_lossy();
            assert!(!s.contains(".codepp-fif.tmp"), "leftover temp file: {s}");
        }
    }
}
