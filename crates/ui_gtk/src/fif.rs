//! Find-in-Files: the results dock, the event drain, and the jump path.
//!
//! The *input* UI is not here — it is the third tab of the Find/Replace
//! window in [`crate::search`], sharing that window's query field and
//! option checkboxes exactly as `ui_win32` shares them across its own
//! Find/Replace/Find-in-Files tabs. This module owns everything below
//! that: the bottom results dock, the drain that turns [`FifEvent`]s into
//! rows, and double-click navigation to a match.
//!
//! The search *engine* is entirely cross-platform (`codepp_core::fif` +
//! `codepp_shell::fif`). The GTK backend only has to (1) build a
//! [`FifRequest`] from the dialog and call [`Shell::start_fif`], (2) drain
//! [`FifEvent`]s on each §5.4 wake and render them, and (3) implement the
//! jump. Open-buffer Replace-in-Files needs no code here at all —
//! `Shell::drain` applies it through the shared `is_doc_dirty` /
//! `replace_doc_text` hooks the GTK backend already provides.
//!
//! # Buffer-then-flush
//!
//! Match events are buffered as they arrive and flushed into the
//! `TreeView` only on the terminal `Done`/`Cancelled` event, matching
//! `ui_win32`. Live feedback while the job runs is the header label's
//! running "Searching… N matches in M files"; one bulk `ListStore`
//! insert at the end is cheaper than per-file churn on a large result
//! set, and keeps the model from being read mid-mutation.

use std::path::PathBuf;

use gtk::glib;
use gtk::prelude::*;

use codepp_core::{FifMatch, FifQueryOpts, FifWalkOpts};
use codepp_scintilla_sys::{SCI_GOTOLINE, SCI_POSITIONFROMLINE, SCI_SETSEL};
use codepp_shell::{
    sanitize_path_for_display, sanitize_str_for_display, FifEvent, FifJobId, FifRequest, FifStats,
    OpenBufferOutcome, OpenFileOutcome,
};

use crate::state::with_state;

/// `ListStore` column indices. The first three are what the `TreeView`
/// shows; the last four are the *functional* values a double-click reads
/// to jump — the real path and byte-offset span, kept out of the visible
/// cells so navigation never parses sanitized display text (the same
/// value-vs-label split `ui_win32`'s parallel `fif_listview_index` uses).
const COL_FILE: u32 = 0;
const COL_LINE: u32 = 1;
const COL_MATCH: u32 = 2;
const COL_PATH: u32 = 3;
const COL_LINE_NO: u32 = 4;
const COL_COL_START: u32 = 5;
const COL_COL_END: u32 = 6;

/// Minimum height the dock's scroll area asks for when shown.
const DOCK_MIN_HEIGHT: i32 = 160;

/// One matched file, buffered until the job completes.
struct PendingFile {
    path: PathBuf,
    matches: Vec<FifMatch>,
    truncated: bool,
}

/// The functional coordinates of a result row: where a double-click
/// should land once the file is open.
#[derive(Clone)]
struct JumpTarget {
    path: PathBuf,
    line_no: u32,
    col_start: u32,
    col_end: u32,
}

/// How a job ended, as observed on the terminal `FifEvent`.
enum Terminal {
    Done(FifStats),
    Cancelled,
}

/// The bottom results dock: widgets plus the per-job runtime buffers.
///
/// Built once at startup and held on [`crate::state::GtkUiState`]; hidden
/// until a search produces results. It lives in the lower pane of a
/// vertical [`gtk::Paned`] so the user can drag the divider — the GTK
/// analogue of Win32's dock splitter.
pub struct FifDock {
    /// The editor/dock divider; the dock is its second child.
    paned: gtk::Paned,
    /// The dock's root, hidden when there is nothing to show.
    container: gtk::Box,
    store: gtk::ListStore,
    /// Summary / live-progress line in the dock header.
    header: gtk::Label,
    cancel_btn: gtk::Button,

    // --- per-job runtime ------------------------------------------
    active_job: Option<FifJobId>,
    pending: Vec<PendingFile>,
    /// A double-click's target, applied once the file's load lands.
    pending_jump: Option<JumpTarget>,
    hit_files: usize,
    hit_matches: usize,
    /// Occurrences replaced in *open* buffers (via `Shell::drain`), which
    /// the worker's disk stats do not count.
    replaced_in_buffers: usize,
    /// Open files skipped because they had unsaved edits (see
    /// [`OpenBufferOutcome::SkippedDirty`]).
    skipped_dirty: usize,
    is_replace: bool,
}

impl FifDock {
    /// Reset the runtime and header for a freshly-started job.
    fn begin_job(&mut self, job: FifJobId, is_replace: bool) {
        self.active_job = Some(job);
        self.pending.clear();
        self.pending_jump = None;
        self.hit_files = 0;
        self.hit_matches = 0;
        self.replaced_in_buffers = 0;
        self.skipped_dirty = 0;
        self.is_replace = is_replace;
        self.store.clear();
        self.header.set_text("Searching…");
        self.cancel_btn.set_sensitive(true);
    }

    /// Reveal the dock and give it a sensible slice of the divider.
    ///
    /// The divider is only repositioned on the hidden→visible transition,
    /// so a second search never snaps back a height the user dragged taller
    /// after the first — matching Win32, which persists its dock height
    /// across jobs. A dock already open keeps exactly the size it has.
    fn show(&self) {
        if self.container.is_visible() {
            return;
        }
        self.container.show();
        // Position the divider so the dock gets `DOCK_MIN_HEIGHT`, once the
        // paned has a real allocation. Before first layout the allocation
        // is 0; the scroll area's min-content-height then floors it, and a
        // later resize settles it.
        let total = self.paned.allocated_height();
        if total > DOCK_MIN_HEIGHT * 2 {
            self.paned.set_position(total - DOCK_MIN_HEIGHT);
        }
    }

    /// Flush buffered matches into the store and write the summary.
    fn finalize(&mut self, term: &Terminal) {
        for file in &self.pending {
            let mut file_display = sanitize_path_for_display(&file.path);
            if file.truncated {
                file_display.push_str(" (truncated)");
            }
            // Accepted tradeoff: the functional path is stored as a
            // (lossy) UTF-8 string because a `ListStore` text column holds
            // no other type. A non-UTF-8 filename (possible on Unix) would
            // round-trip with U+FFFD substitutions, so a double-click on
            // such a row opens nothing rather than the wrong file — the
            // reject is benign, and non-UTF-8 names are vanishingly rare in
            // practice. A lossless design would need a side-table keyed by
            // row, reintroducing the index-desync risk the hidden-column
            // approach exists to avoid.
            for m in &file.matches {
                let line = m.line_no.to_string();
                let text = sanitize_str_for_display(&m.line_text);
                self.store.insert_with_values(
                    None,
                    &[
                        (COL_FILE, &file_display),
                        (COL_LINE, &line),
                        (COL_MATCH, &text),
                        (COL_PATH, &file.path.to_string_lossy().into_owned()),
                        (COL_LINE_NO, &m.line_no),
                        (COL_COL_START, &m.col_start),
                        (COL_COL_END, &m.col_end),
                    ],
                );
            }
        }
        self.header.set_text(&self.summary(term));
        self.cancel_btn.set_sensitive(false);
        self.active_job = None;
    }

    /// The completion summary shown in the dock header.
    fn summary(&self, term: &Terminal) -> String {
        use std::fmt::Write as _;
        let stats = match term {
            Terminal::Cancelled => {
                let mut s = format!(
                    "Cancelled — {} matches in {} files so far",
                    self.hit_matches, self.hit_files
                );
                if self.is_replace && self.replaced_in_buffers > 0 {
                    let _ = write!(s, ", {} replaced in open buffers", self.replaced_in_buffers);
                }
                return s;
            }
            Terminal::Done(stats) => stats,
        };

        let mut s = if self.is_replace {
            let total = stats.total_replacements + self.replaced_in_buffers;
            let mut t = format!(
                "Replaced {total} occurrences in {} files on disk",
                stats.files_modified
            );
            if self.replaced_in_buffers > 0 {
                let _ = write!(t, ", {} in open buffers", self.replaced_in_buffers);
            }
            if self.skipped_dirty > 0 {
                let _ = write!(
                    t,
                    " (skipped {} open file(s) with unsaved changes)",
                    self.skipped_dirty
                );
            }
            t
        } else {
            format!(
                "{} matches in {} files ({} scanned)",
                stats.total_matches, stats.files_with_matches, stats.files_scanned
            )
        };
        if stats.global_cap_hit || stats.depth_cap_hit {
            s.push_str(" — results incomplete, some content was not searched");
        }
        s
    }
}

/// Build the dock and pack it into the lower pane of `paned`.
///
/// Returns the [`FifDock`] to store on the window state. The container is
/// realized then hidden with `no_show_all` set, so the toplevel's
/// `show_all` at startup leaves it collapsed until the first search.
pub fn build_dock(paned: &gtk::Paned) -> FifDock {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);

    // Header: summary label (springs) + Cancel + close.
    let header_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    header_box.set_margin_top(2);
    header_box.set_margin_bottom(2);
    header_box.set_margin_start(6);
    header_box.set_margin_end(6);
    let header = gtk::Label::new(Some("Find in Files"));
    header.set_xalign(0.0);
    header_box.pack_start(&header, true, true, 0);
    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.set_sensitive(false);
    header_box.pack_start(&cancel_btn, false, false, 0);
    let close_btn = gtk::Button::with_label("✕");
    close_btn.set_relief(gtk::ReliefStyle::None);
    header_box.pack_start(&close_btn, false, false, 0);
    container.pack_start(&header_box, false, false, 0);

    // Results list.
    let store = gtk::ListStore::new(&[
        glib::Type::STRING, // file display
        glib::Type::STRING, // line display
        glib::Type::STRING, // match display
        glib::Type::STRING, // real path
        glib::Type::U32,    // line_no
        glib::Type::U32,    // col_start
        glib::Type::U32,    // col_end
    ]);
    let tree = gtk::TreeView::with_model(&store);
    tree.set_headers_visible(true);
    append_text_column(&tree, "File", COL_FILE, 360);
    append_text_column(&tree, "Line", COL_LINE, 60);
    append_text_column(&tree, "Match", COL_MATCH, 480);

    let scroll = gtk::ScrolledWindow::builder()
        .min_content_height(DOCK_MIN_HEIGHT)
        .build();
    scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroll.add(&tree);
    container.pack_start(&scroll, true, true, 0);

    // The dock is the lower, non-resizing pane; the editor above absorbs
    // window resizing. `shrink = true` lets the user drag it closed.
    paned.pack2(&container, false, true);

    // Realize children, then hide the whole dock and opt out of the
    // toplevel `show_all` so it stays collapsed until results exist.
    container.show_all();
    container.hide();
    container.set_no_show_all(true);

    // Double-click a row → open the file and jump to the match.
    tree.connect_row_activated(|tree, path, _col| {
        on_row_activated(tree, path);
    });
    cancel_btn.connect_clicked(|_| {
        with_state(|st| {
            st.shell.cancel_fif();
            st.fif_dock.header.set_text("Cancelling…");
        });
    });
    close_btn.connect_clicked(|_| {
        // Hide the dock, and cancel any still-running job so no orphaned
        // worker keeps posting wakes at an invisible dock.
        with_state(|st| {
            if st.fif_dock.active_job.take().is_some() {
                st.shell.cancel_fif();
            }
            st.fif_dock.container.hide();
        });
    });

    FifDock {
        paned: paned.clone(),
        container,
        store,
        header,
        cancel_btn,
        active_job: None,
        pending: Vec::new(),
        pending_jump: None,
        hit_files: 0,
        hit_matches: 0,
        replaced_in_buffers: 0,
        skipped_dirty: 0,
        is_replace: false,
    }
}

/// Append a left-aligned, resizable text column bound to `model_col`.
fn append_text_column(tree: &gtk::TreeView, title: &str, model_col: u32, width: i32) {
    let renderer = gtk::CellRendererText::new();
    let column = gtk::TreeViewColumn::new();
    column.set_title(title);
    column.set_resizable(true);
    column.set_fixed_width(width);
    gtk::prelude::TreeViewColumnExt::pack_start(&column, &renderer, true);
    gtk::prelude::TreeViewColumnExt::add_attribute(&column, &renderer, "text", model_col as i32);
    tree.append_column(&column);
}

/// The inputs [`run_search`] needs, harvested from the Find/Replace
/// window's Find-in-Files tab before any modal confirm runs.
///
/// The five `bool`s are the dialog's five independent checkbox states
/// (three shared search options + two scan-scope toggles), each a
/// distinct user choice forwarded verbatim into the request — so a
/// flags-struct or enum would only obscure a plain 1:1 UI mirror.
#[allow(clippy::struct_excessive_bools)]
pub struct FifInputs {
    pub query: String,
    /// `Some` for Replace-in-Files, `None` for Find All.
    pub replacement: Option<String>,
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub directory: String,
    pub filters: String,
    pub recurse: bool,
    pub hidden: bool,
}

/// Start a Find-in-Files (or Replace-in-Files) job from harvested inputs.
///
/// Returns `Err(message)` for a bad input (empty query/directory, bad
/// glob, or a rejected start) so the caller can show it on the dialog's
/// status line. On success the dock is reset and shown, and results
/// arrive later through [`drain_into_dock`].
///
/// # Errors
///
/// Surfaces empty-query / empty-directory guards, `set_includes` glob
/// errors, and any [`codepp_shell::FifError`] from `start_fif`, all as a
/// display string.
pub fn run_search(inputs: FifInputs) -> Result<(), String> {
    if inputs.query.is_empty() {
        return Err("Enter a search term".to_string());
    }
    if inputs.directory.trim().is_empty() {
        return Err("Enter a directory to search".to_string());
    }
    let mut walk = FifWalkOpts::default();
    walk.recurse = inputs.recurse;
    walk.walk_hidden_dirs = inputs.hidden;
    apply_filters(&mut walk, &inputs.filters)?;

    let opts = FifQueryOpts {
        match_case: inputs.match_case,
        whole_word: inputs.whole_word,
        regex: inputs.regex,
    };
    let root = PathBuf::from(inputs.directory.trim());
    let is_replace = inputs.replacement.is_some();

    let outcome = with_state(move |st| {
        let open_tab_paths = st
            .shell
            .tabs
            .iter()
            .filter_map(|t| t.path.clone())
            .collect();
        let request = FifRequest {
            query: inputs.query,
            opts,
            root,
            walk,
            replacement: inputs.replacement,
            open_tab_paths,
        };
        match st.shell.start_fif(request) {
            Ok(job) => {
                st.fif_dock.begin_job(job, is_replace);
                st.fif_dock.show();
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    });
    outcome.unwrap_or_else(|| Err("window is gone".to_string()))
}

/// Translate the whitespace-separated Filters box into include globs.
///
/// `*` / `*.*` / empty mean "all files" (no include set). A bare `*.ext`
/// is anchored with `**/` so it matches at any depth. Mirrors
/// `ui_win32::apply_filters_to_walk_opts` so both backends read filters
/// identically.
fn apply_filters(walk: &mut FifWalkOpts, filters_text: &str) -> Result<(), String> {
    let mut patterns: Vec<String> = Vec::new();
    for tok in filters_text.split_whitespace() {
        if tok == "*" || tok == "*.*" {
            continue;
        }
        if tok.starts_with('!') {
            return Err("filters: '!' negation is not supported — remove those tokens".to_string());
        }
        patterns.push(if tok.starts_with("**/") {
            tok.to_string()
        } else {
            format!("**/{tok}")
        });
    }
    if patterns.is_empty() {
        return Ok(());
    }
    let refs: Vec<&str> = patterns.iter().map(String::as_str).collect();
    walk.set_includes(&refs).map_err(|e| e.to_string())
}

/// Drain the FIF event queue into the dock. Called from `drain_shell` on
/// every §5.4 wake, after the main `Shell::drain`.
pub fn drain_into_dock() {
    let terminal = with_state(|st| {
        let Some(active) = st.fif_dock.active_job else {
            // No job running: discard anything still queued rather than
            // letting it accumulate against the next job.
            let _ = st.shell.take_fif_events();
            return None;
        };
        let active = active.raw();
        let mut terminal: Option<Terminal> = None;
        for ev in st.shell.take_fif_events() {
            match ev {
                FifEvent::FileMatches { job, path, outcome } if job.raw() == active => {
                    st.fif_dock.hit_files += 1;
                    st.fif_dock.hit_matches += outcome.matches.len();
                    st.fif_dock.pending.push(PendingFile {
                        path,
                        matches: outcome.matches,
                        truncated: outcome.truncated,
                    });
                }
                FifEvent::ReplacedInOpenBuffer {
                    job,
                    replaced,
                    outcome,
                    ..
                } if job.raw() == active => match outcome {
                    OpenBufferOutcome::Replaced => st.fif_dock.replaced_in_buffers += replaced,
                    OpenBufferOutcome::SkippedDirty => st.fif_dock.skipped_dirty += 1,
                    OpenBufferOutcome::TabClosed => {}
                },
                FifEvent::Done { job, stats } if job.raw() == active => {
                    terminal = Some(Terminal::Done(stats));
                }
                FifEvent::Cancelled { job } if job.raw() == active => {
                    terminal = Some(Terminal::Cancelled);
                }
                // Stale-job events, and `ReplaceInOpenBuffer` (consumed by
                // `Shell::drain` before it ever reaches here) are ignored.
                _ => {}
            }
        }
        // Live progress while the job is still running.
        if terminal.is_none() {
            let (f, m) = (st.fif_dock.hit_files, st.fif_dock.hit_matches);
            st.fif_dock
                .header
                .set_text(&format!("Searching… {m} matches in {f} files"));
        }
        terminal
    })
    .flatten();

    if let Some(term) = terminal {
        with_state(|st| st.fif_dock.finalize(&term));
    }
    apply_pending_jump();
}

/// Row double-clicked: queue the jump, open the file, and apply the jump
/// now if the file was already open (no async load to wait for).
fn on_row_activated(tree: &gtk::TreeView, path: &gtk::TreePath) {
    let Some(model) = tree.model() else {
        return;
    };
    let Some(iter) = model.iter(path) else {
        return;
    };
    let real_path: String = model
        .value(&iter, COL_PATH as i32)
        .get()
        .unwrap_or_default();
    let line_no: u32 = model
        .value(&iter, COL_LINE_NO as i32)
        .get()
        .unwrap_or_default();
    let col_start: u32 = model
        .value(&iter, COL_COL_START as i32)
        .get()
        .unwrap_or_default();
    let col_end: u32 = model
        .value(&iter, COL_COL_END as i32)
        .get()
        .unwrap_or_default();
    let target = JumpTarget {
        path: PathBuf::from(&real_path),
        line_no,
        col_start,
        col_end,
    };

    with_state(|st| st.fif_dock.pending_jump = Some(target.clone()));
    let outcome = with_state(|st| st.shell.open_file(target.path.clone()));
    match outcome {
        Some(OpenFileOutcome::SwitchedToExisting(_)) => {
            crate::rebind_active_view();
            apply_pending_jump();
        }
        Some(OpenFileOutcome::AlreadyActive) => apply_pending_jump(),
        // `Loading`: the jump applies when the load lands via
        // `drain_into_dock`. `Rejected`/None: drop it.
        Some(OpenFileOutcome::Loading) => {}
        _ => {
            with_state(|st| st.fif_dock.pending_jump = None);
        }
    }
}

/// Apply the queued jump if the active tab is now the file it targets.
///
/// A no-op while the file is still loading (active tab's path won't match
/// yet) — the next `drain_into_dock` after the load binds the doc retries
/// it. Selects the match span so the hit is highlighted, not just scrolled
/// into view.
fn apply_pending_jump() {
    with_state(|st| {
        let Some(jump) = st.fif_dock.pending_jump.clone() else {
            return;
        };
        let active_path = st
            .shell
            .active_tab
            .and_then(|i| st.shell.tabs.get(i))
            .and_then(|t| t.path.clone());
        if active_path.as_deref() != Some(jump.path.as_path()) {
            return;
        }
        let line0 = jump.line_no.saturating_sub(1) as usize;
        // Brings the line into view.
        st.editor.send(SCI_GOTOLINE, line0, 0);
        let line_start = st.editor.send(SCI_POSITIONFROMLINE, line0, 0).max(0) as usize;
        // cols are UTF-8 byte offsets within the line — the same units
        // Scintilla positions use, so no re-encoding.
        let sel_start = line_start + jump.col_start as usize;
        let sel_end = line_start + jump.col_end as usize;
        st.editor.send(SCI_SETSEL, sel_start, sel_end as isize);
        st.fif_dock.pending_jump = None;
    });
}
