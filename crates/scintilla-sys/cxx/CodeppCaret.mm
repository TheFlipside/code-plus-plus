// CodeppCaret.mm — the x Scintilla draws a caret from, for the layer caret.
//
// `ui_cocoa::caret` draws the main view's caret as a `CALayer` and has to
// put it on the device pixel Scintilla would. `EditView::DrawCarets`
// places a line caret at `round(x - 0.51)` of the float x
// `Editor::LocationFromPosition` gives, and `SCI_POINTXFROMPOSITION`
// returns that x truncated, which cannot tell a fraction under 0.01 from
// a whole number. Courier New 10, Code++'s default font, advances a hair
// over 6 pt, so its first columns are exactly that case. This returns the
// float.
//
// `LocationFromPosition` is a protected member of `Editor`. A pointer to
// it, formed in a class derived from `Editor`, may be called on any
// `Editor` (C++ [class.protected] constrains how the member is named, not
// the call through the pointer), so this reads it without patching the
// vendored tree (DESIGN.md §4.1). Main thread only, like every other call
// into the backend.

#include <cmath>
#include <optional>
#include <string_view>
#include <vector>

#import <Cocoa/Cocoa.h>

#import "ScintillaTypes.h"
#import "ScintillaMessages.h"
#import "ScintillaStructures.h"
#import "Debugging.h"
#import "Geometry.h"
#import "Platform.h"
#import "ScintillaView.h"
#import "ScintillaCocoa.h"

using namespace Scintilla;
using namespace Scintilla::Internal;

namespace {

// Never instantiated: it exists to name the protected member.
class LocationAccess : public ScintillaCocoa {
public:
	static Scintilla::Internal::Point At(Editor &editor, Sci::Position pos) {
		Scintilla::Internal::Point (Editor::*location)(Sci::Position, PointEnd) = &LocationAccess::LocationFromPosition;
		return (editor.*location)(pos, PointEnd::start);
	}
};

}

/// Write the x of `pos` in the view's client coordinates, as a float, to
/// `*x`. `direct_ptr` is the view's `SCI_GETDIRECTPOINTER`, which on Cocoa
/// is its `ScintillaCocoa` (`ScintillaCocoa::DirectFunction`). Returns 1
/// on success, and 0, writing nothing, for a null pointer, a negative
/// position, or an exception, which is not let out into Rust.
extern "C" int scintilla_cocoa_location_x(void *direct_ptr, sptr_t pos, double *x) {
	if (!direct_ptr || pos < 0 || !x) {
		return 0;
	}
	try {
		ScintillaCocoa *sci = reinterpret_cast<ScintillaCocoa *>(direct_ptr);
		*x = LocationAccess::At(*sci, pos).x;
		return 1;
	} catch (...) {
		return 0;
	}
}
