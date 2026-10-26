//! Where the editor view stands in a document: the selection and the
//! scroll.
//!
//! One Scintilla view serves every tab, and `SCI_SETDOCPOINTER` clears the
//! selection on every swap (`Editor::SetDocPointer`). So the shell keeps a
//! [`ViewPosition`] for each tab while the view shows another document, and
//! puts it back when the view returns; `session.xml` records its caret. A
//! value type only: reading and applying one is `codepp_editor`'s.

/// Where the view stands in a document: the selection and the scroll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewPosition {
    /// The anchor end of the selection (`SCI_GETANCHOR`), equal to
    /// [`Self::caret`] when nothing is selected.
    pub anchor: u64,
    /// The caret (`SCI_GETCURRENTPOS`).
    pub caret: u64,
    /// The first line on screen (`SCI_GETFIRSTVISIBLELINE`, a display
    /// line), or `None` when it is not known: a caret read from
    /// `session.xml`, or one read from a view too small to show a whole
    /// line, whose scroll says nothing about where the user was. Without
    /// one, the caret is scrolled into view when the position is applied.
    pub first_visible_line: Option<u64>,
    /// The horizontal scroll in pixels (`SCI_GETXOFFSET`). Applied only
    /// with a first visible line.
    pub x_offset: u64,
}

impl ViewPosition {
    /// The caret at `caret` with nothing selected, scrolled into view when
    /// it is applied: what a caret read from `session.xml` gives.
    #[must_use]
    pub const fn at(caret: u64) -> Self {
        Self {
            anchor: caret,
            caret,
            first_visible_line: None,
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

#[cfg(test)]
mod tests {
    use super::ViewPosition;

    #[test]
    fn a_caret_from_the_session_selects_nothing_and_scrolls_into_view() {
        let pos = ViewPosition::at(42);
        assert_eq!(pos.anchor, 42);
        assert_eq!(pos.caret, 42);
        assert_eq!(pos.first_visible_line, None);
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
}
