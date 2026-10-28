// Scintilla source code edit control
// Code++'s replacement for Editor::CopySelectionRange, which build.rs
// splices into a copy of src/Editor.cxx in place of upstream's.
// Copyright 1998-2011 by Neil Hodgson <neilh@scintilla.org>
// The License.txt file describes the conditions under which this software may be distributed.

/* ==========================================================================
 * MODIFIED FUNCTION — Code++ carries this in place of the vendored one.
 * ==========================================================================
 *
 * Provenance
 * ----------
 * This replaces `Editor::CopySelectionRange` in Scintilla's
 * `src/Editor.cxx`, at the pinned submodule (rel-5-5-2). Scintilla is
 * under the HPND licence, which permits modification provided the
 * notice is retained; the notice above is upstream's. The modification
 * is Code++'s own, written from Notepad++'s measured behaviour, not
 * copied from Notepad++.
 *
 * It is **not** applied by patching the submodule. `build.rs` copies the
 * vendored `src/Editor.cxx` into `OUT_DIR` with this function in place
 * of upstream's, so the submodule stays pristine and survives
 * `git submodule update`. See `stage_patched_editor` there, which also
 * fingerprints the function this replaces, so a Scintilla bump that
 * changes it fails the build rather than silently dropping the change.
 *
 * What changed, and why
 * ---------------------
 * Every copy of a selection goes through this function: the clipboard
 * copy, a drag, GTK's primary selection, and `SCI_GETSELTEXT`, which
 * plugins send directly. Notepad++ 8.9.8.1 ships a Scintilla modified
 * here, and a plugin written for it reads what that version copies.
 * Measured with a probe plugin in Notepad++ (DESIGN.md §7.4):
 *
 *   - A selection of several ranges, a multiple selection or a
 *     rectangle over several lines, is copied with each range followed
 *     by the document's line end, the last included. A multiple
 *     selection keeps the order its ranges were made in; a rectangle's
 *     ranges are sorted, top line first.
 *   - A selection of one range, a one-line rectangle included, is its
 *     text alone.
 *   - The copy is marked rectangular exactly when it has several
 *     ranges, so a multiple selection pastes as a column and a one-line
 *     rectangle as ordinary text.
 *
 * Upstream Scintilla, 5.5.2 and 5.6.6 alike, puts nothing between a
 * multiple selection's ranges (`SCI_SETCOPYSEPARATOR`'s string, empty
 * by default), ends a rectangle's every line with a line end, a one-line
 * rectangle included, and marks only a rectangle rectangular. As in
 * Notepad++, `SCI_SETCOPYSEPARATOR` therefore no longer changes what is
 * copied: a single range has nothing to separate, and several ranges
 * are separated by the line end.
 * ========================================================================== */

void Editor::CopySelectionRange(SelectionText *ss, bool allowLineCopy) {
	if (sel.Empty()) {
		if (allowLineCopy) {
			CopyLineRange(ss);
		}
		return;
	}
	std::vector<SelectionRange> ranges = sel.RangesCopy();
	if (sel.selType == Selection::SelTypes::rectangle) {
		std::sort(ranges.begin(), ranges.end());
	}
	const bool several = ranges.size() > 1;
	std::string text;
	for (const SelectionRange &range : ranges) {
		text.append(RangeText(range.Start().Position(), range.End().Position()));
		if (several) {
			text.append(pdoc->EOLString());
		}
	}
	ss->Copy(text, pdoc->dbcsCodePage, vs.styles[StyleDefault].characterSet,
		several, sel.selType == Selection::SelTypes::lines);
}
