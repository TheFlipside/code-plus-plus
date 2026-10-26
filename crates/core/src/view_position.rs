//! Where the editor view stands in a document: the selection, the scroll
//! and the folds.
//!
//! One Scintilla view serves every tab, and `SCI_SETDOCPOINTER` clears the
//! selection and expands every fold on every swap (`Editor::SetDocPointer`).
//! So the shell keeps a [`ViewPosition`] and the contracted folds for each
//! tab while the view shows another document, puts them back when the view
//! returns, and `session.xml` records both. Value types and pure rules
//! only: reading and applying them is `codepp_editor`'s.

/// The most contracted folds kept for one tab. A crafted `session.xml` can
/// list as many as it likes, and every fold kept is a Scintilla message at
/// each switch onto the tab; a file with more folds than this keeps the
/// first ones.
pub const MAX_KEPT_FOLDS: usize = 10_000;

/// The most contracted folds one `session.xml` records, across every tab
/// ([`crate::Session::limit_folds`]). Each tab may keep
/// [`MAX_KEPT_FOLDS`], and 1024 tabs of them would make a file far over
/// [`crate::session::MAX_SESSION_FILE_BYTES`], which the next start would
/// refuse and which Code++ therefore declines to write: the session, its
/// tabs and backups included, would stop being saved at all. This many
/// `<fold line="N"/>` children, each line at most [`MAX_FOLD_LINE`] and so
/// ten digits, come to under 3 MB, which leaves room for the largest tab
/// records Code++ writes.
pub const MAX_SESSION_FOLDS: usize = 100_000;

/// The highest line a kept fold may be on: a line past it is dropped
/// ([`kept_folds`]). A document of more lines than this is far past the
/// 512 MiB Code++ opens a file of ([`crate::file::MAX_FILE_BYTES`]), so only
/// a hand-edited `session.xml` lists one, and dropping it keeps every
/// `<fold>` the session writes to ten digits, which [`MAX_SESSION_FOLDS`]
/// counts on.
pub const MAX_FOLD_LINE: u64 = 0x7FFF_FFFF;

/// Where the view stands in a document: the selection and the scroll.
///
/// The scroll is kept as a document line and the display lines into it,
/// not as a display line, because display lines move: a contracted fold
/// hides lines above, and a wrapped line takes as many display lines as the
/// window is narrow. A document line stays put across both, so a position
/// read before the folds are put back, or in a window of another width,
/// still names the same text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewPosition {
    /// The anchor end of the selection (`SCI_GETANCHOR`), equal to
    /// [`Self::caret`] when nothing is selected.
    pub anchor: u64,
    /// The caret (`SCI_GETCURRENTPOS`).
    pub caret: u64,
    /// The document line at the top of the view, or `None` when it is not
    /// known: a caret from a `session.xml` written before the scroll was
    /// recorded, or one read from a view too small to show a whole line,
    /// whose scroll says nothing about where the user was. Without one, the
    /// caret is scrolled into view when the position is applied.
    pub top_line: Option<u64>,
    /// How many display lines into [`Self::top_line`] the view starts, when
    /// that line wraps.
    pub top_subline: u64,
    /// The horizontal scroll in pixels (`SCI_GETXOFFSET`). Applied only
    /// with a top line, and only while lines do not wrap.
    pub x_offset: u64,
}

impl ViewPosition {
    /// The caret at `caret` with nothing selected, scrolled into view when
    /// it is applied: what a caret read from `session.xml` alone gives.
    #[must_use]
    pub const fn at(caret: u64) -> Self {
        Self {
            anchor: caret,
            caret,
            top_line: None,
            top_subline: 0,
            x_offset: 0,
        }
    }

    /// The selection's start and end, lower first: the `startPos` and
    /// `endPos` a Notepad++ session file records.
    #[must_use]
    pub const fn selection(&self) -> (u64, u64) {
        if self.anchor <= self.caret {
            (self.anchor, self.caret)
        } else {
            (self.caret, self.anchor)
        }
    }
}

/// `lines` as a set of contracted fold headers to keep: in order, each
/// line once, none past [`MAX_FOLD_LINE`], at most [`MAX_KEPT_FOLDS`] of
/// them, the first ones kept.
#[must_use]
pub fn kept_folds(mut lines: Vec<u64>) -> Vec<u64> {
    lines.retain(|&line| line <= MAX_FOLD_LINE);
    lines.sort_unstable();
    lines.dedup();
    lines.truncate(MAX_KEPT_FOLDS);
    lines
}

#[cfg(test)]
mod tests {
    use super::{kept_folds, ViewPosition, MAX_FOLD_LINE, MAX_KEPT_FOLDS};

    #[test]
    fn a_caret_from_the_session_selects_nothing_and_scrolls_into_view() {
        let pos = ViewPosition::at(42);
        assert_eq!(pos.anchor, 42);
        assert_eq!(pos.caret, 42);
        assert_eq!(pos.top_line, None);
        assert_eq!(pos.top_subline, 0);
        assert_eq!(pos.x_offset, 0);
        assert_eq!(ViewPosition::default(), ViewPosition::at(0));
    }

    #[test]
    fn the_selection_runs_lower_first_whichever_end_the_caret_is() {
        let forward = ViewPosition {
            anchor: 3,
            caret: 9,
            ..ViewPosition::default()
        };
        let backward = ViewPosition {
            anchor: 9,
            caret: 3,
            ..ViewPosition::default()
        };
        assert_eq!(forward.selection(), (3, 9));
        assert_eq!(backward.selection(), (3, 9));
    }

    #[test]
    fn kept_folds_are_ordered_unique_and_bounded() {
        assert_eq!(kept_folds(vec![40, 3, 40, 12]), [3, 12, 40]);
        let many: Vec<u64> = (0..=MAX_KEPT_FOLDS as u64).rev().collect();
        let kept = kept_folds(many);
        assert_eq!(kept.len(), MAX_KEPT_FOLDS);
        assert_eq!(kept.first(), Some(&0));
        assert_eq!(kept.last(), Some(&(MAX_KEPT_FOLDS as u64 - 1)));
    }

    /// A line past the last one a document Code++ holds can have is not a
    /// fold to keep.
    #[test]
    fn a_fold_past_the_last_line_is_dropped() {
        assert_eq!(
            kept_folds(vec![u64::MAX, MAX_FOLD_LINE + 1, MAX_FOLD_LINE, 7]),
            [7, MAX_FOLD_LINE]
        );
    }
}
