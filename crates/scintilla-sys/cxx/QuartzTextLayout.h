/*
 *  QuartzTextLayout.h
 *
 *  Original Code by Evan Jones on Wed Oct 02 2002.
 *  Contributors:
 *  Shane Caraveo, ActiveState
 *  Bernd Paradies, Adobe
 *
 */

/* ==========================================================================
 * MODIFIED COPY — Code++ carries this file in place of the vendored one.
 * ==========================================================================
 *
 * Provenance
 * ----------
 * This is Scintilla's `cocoa/QuartzTextLayout.h`, taken from the pinned
 * submodule (rel-5-5-2) and modified. Scintilla is under the HPND
 * licence, which permits modification provided the notice is retained;
 * the original attribution above is upstream's and is kept verbatim.
 * `THIRD_PARTY_LICENSES.md` carries the licence text.
 *
 * It is **not** applied by patching the submodule. `build.rs` stages the
 * vendored `cocoa/` sources into `OUT_DIR` and drops this file over the
 * top, so the submodule working tree stays pristine and survives
 * `git submodule update`. See `stage_cocoa_sources` there, which also
 * fingerprints the file this replaces so a Scintilla bump that changes
 * it fails the build loudly rather than silently reverting the change
 * below.
 *
 * What changed, and why
 * ---------------------
 * Upstream builds a `CTLine` — CoreText typesetting, glyph encoding, the
 * lot — in the constructor and throws it away in the destructor. The
 * object is only ever a stack temporary in `SurfaceImpl`, so *every*
 * text draw and *every* width measurement re-typesets from scratch.
 *
 * That is the whole of Code++'s macOS keystroke-latency budget miss, and
 * it was measured rather than guessed (DESIGN.md §8): on macOS every
 * keystroke repaints every visible line, because `NSWindow`'s content
 * view is unconditionally layer-backed and the layer redraw hands
 * Scintilla the full bounds rather than the changed line's rect. With
 * ~64 visible lines that is p99 21 ms against a 5 ms budget, and it
 * scales linearly with window height — 6.4 ms at 14 lines. `sample`
 * puts the time in `CTLineCreateWithAttributedString` underneath
 * `EditView::DrawLine`.
 *
 * Neither other backend has the problem, because neither re-typesets per
 * paint: Win32 goes through GDI/DirectWrite and GTK through Pango, both
 * of which cache internally. CoreText does not, so the cache has to live
 * here.
 *
 * The change is therefore: memoise the `CTLine` (and the attributed
 * string it was built from) on `(text, requested encoding, font,
 * foreground colour)`. Those four inputs are the complete set — the
 * style dictionary `QuartzTextStyle` maintains only ever holds
 * `kCTFontAttributeName` and `kCTForegroundColorAttributeName` — so two
 * lines with equal keys are interchangeable, and a repaint of an
 * unchanged line becomes a draw instead of a typeset.
 *
 * Correctness notes that are not obvious
 * --------------------------------------
 * * **The colour is part of the key because the dictionary is mutated in
 *   place.** `QuartzTextStyle::setCTStyleColour` calls
 *   `CFDictionarySetValue` on the *same* dictionary, so neither the
 *   style pointer nor the dictionary pointer identifies the attributes.
 *   `SurfaceImpl::DrawTextTransparent` sets the colour immediately
 *   before constructing a layout, so keying on the colour's value is
 *   what makes the cache sound.
 * * **The colour is keyed by value, not by pointer.** The draw paths
 *   build a fresh `CGColorRef` per call, so a pointer key would miss
 *   every time. Components are packed at 8 bits each, which is exact:
 *   they originate from a `ColourRGBA`, whose components are 8-bit.
 * * **The font is retained by the cache.** Keying on a bare `CTFontRef`
 *   address would be unsound — a font could be released and a different
 *   one allocated at the same address, and the stale entry would then
 *   match. Holding a reference makes the address unique for as long as
 *   the entry lives.
 * * **The layout takes its own reference.** The cached line outlives the
 *   layout object, so the object holds +1 rather than borrowing. That
 *   costs a retain/release pair per call — nothing against building a
 *   line — and means a cache eviction can never pull a line out from
 *   under a live layout.
 * * **Single-threaded, like the rest of Scintilla.** Every caller is on
 *   the thread that owns the view. The cache is a plain map with no
 *   locking, for the same reason `Editor` has none.
 *
 * The cache is bounded and cleared wholesale when it fills, rather than
 * evicted LRU: entries are keyed on style runs within a line, so the
 * working set is small and stable, and a rare O(n) clear is cheaper than
 * per-lookup bookkeeping on the hot path.
 *
 * Setting `CODEPP_NO_CTLINE_CACHE` in the environment disables it, so
 * the before/after can be re-measured without a rebuild.
 * ========================================================================== */

#ifndef QUARTZTEXTLAYOUT_H
#define QUARTZTEXTLAYOUT_H

#include <cstdint>
#include <cstdlib>
#include <string>
#include <string_view>
#include <unordered_map>

#include <Cocoa/Cocoa.h>

#include "QuartzTextStyle.h"

namespace CodeppTextCache {

/// The attributes that fully determine a `CTLine`, plus the text.
struct Key {
	std::string text;
	CFStringEncoding encoding;
	/// Retained by the owning entry — see the header notes.
	CTFontRef font;
	/// Foreground colour packed 8 bits per component, or `kNoColour`.
	std::uint64_t colour;

	bool operator==(const Key &other) const noexcept {
		return encoding == other.encoding && font == other.font &&
		       colour == other.colour && text == other.text;
	}
};

struct KeyHash {
	std::size_t operator()(const Key &k) const noexcept {
		std::size_t h = std::hash<std::string>()(k.text);
		auto mix = [&h](std::uint64_t v) noexcept {
			h ^= std::hash<std::uint64_t>()(v) + 0x9e3779b97f4a7c15ULL + (h << 6) + (h >> 2);
		};
		mix(static_cast<std::uint64_t>(k.encoding));
		mix(reinterpret_cast<std::uintptr_t>(k.font));
		mix(k.colour);
		return h;
	}
};

/// Everything the layout needs to answer its queries.
struct Entry {
	CTLineRef line;
	CFAttributedStringRef string;
	CFIndex stringLength;
	CFStringEncoding encodingUsed;
};

/// Sentinel for "the style carries no foreground colour", which is the
/// case on the measuring paths. Distinct from every packed colour: those
/// occupy the low 32 bits plus a small component count above them, well
/// clear of bit 63.
constexpr std::uint64_t kNoColour = 1ULL << 63;

/// Upper bound on live entries. A full screen of styled text is a few
/// hundred runs; this leaves generous room for scrolling through a file
/// before the clear below costs anything.
constexpr std::size_t kMaxEntries = 4096;

/// Longest text run that will be cached, in bytes.
///
/// Defence in depth rather than a live bound: Scintilla subdivides every
/// run it hands to `SurfaceImpl` well below this
/// (`BreakFinder::lengthStartSubdivision` is 300 bytes,
/// `lengthEachSubdivision` 100 — `src/PositionCache.h`), so nothing
/// reaches it today. That bound lives in a different file this header
/// does not reference, and a Scintilla bump or a new call site could
/// change it without anything here noticing — at which point the cap on
/// entry *count* would no longer imply a cap on cache *size*. A hostile
/// file is the case that matters: one very long unbroken line is a
/// plausible thing to be handed.
constexpr std::size_t kMaxTextLength = 1024;

inline bool Enabled() noexcept {
	static const bool enabled = std::getenv("CODEPP_NO_CTLINE_CACHE") == nullptr;
	return enabled;
}

inline std::unordered_map<Key, Entry, KeyHash> &Map() {
	static std::unordered_map<Key, Entry, KeyHash> map;
	return map;
}

/// Hit and miss counters, so the memoisation itself is testable without
/// timing anything. `scintilla_cocoa_ctline_cache_stats` in the shim
/// exposes them; `crates/scintilla-sys`'s test asserts that a repeated
/// identical layout hits. A latency assertion would be the obvious
/// alternative and is a bad one — it would be flaky, need a window
/// server, and fail for reasons unrelated to this cache.
struct Counters {
	std::uint64_t hits;
	std::uint64_t misses;
};

inline Counters &Stats() noexcept {
	static Counters counters{0, 0};
	return counters;
}

/// Pack the style's current foreground colour into the key.
///
/// Reads the *value* rather than the pointer: the draw paths create a
/// fresh `CGColorRef` per call, so a pointer would never match twice.
inline std::uint64_t PackForeground(CFDictionaryRef attributes) noexcept {
	const void *value = CFDictionaryGetValue(attributes, kCTForegroundColorAttributeName);
	if (!value) {
		return kNoColour;
	}
	CGColorRef colour = static_cast<CGColorRef>(const_cast<void *>(value));
	const CGFloat *components = CGColorGetComponents(colour);
	if (!components) {
		return kNoColour;
	}
	// Colour spaces differ in component count — generic RGB has four,
	// greyscale two — so the count goes into the key alongside the
	// components. Without it the packing is ambiguous: a two-component
	// grey [0.5, 1.0] and a four-component RGBA [0, 0, 0.5, 1.0] both
	// pack to 0x0000_80FF, and two visibly different colours would
	// share a cached line. Every call site today builds a
	// four-component `CGColorCreateGenericRGB`, so this is unreachable
	// as written — it is closed rather than asserted because the cost
	// is one shift and the failure would be a silently wrong glyph run.
	const std::size_t count = CGColorGetNumberOfComponents(colour);
	std::uint64_t packed = 0;
	for (std::size_t i = 0; i < count && i < 4; i++) {
		const CGFloat c = components[i] < 0.0 ? 0.0 : (components[i] > 1.0 ? 1.0 : components[i]);
		packed = (packed << 8) | static_cast<std::uint64_t>(c * 255.0 + 0.5);
	}
	// The tag is OR'd in **after** the loop, not before it. Shifted
	// along with the components it would be pushed out of the value
	// entirely for the four-component case — which is every call site
	// today, so the property this exists to guarantee would have held
	// only by accident of the remaining bit positions being disjoint.
	// Four components occupy the low 32 bits, so bits 32+ are free.
	return packed | (static_cast<std::uint64_t>(count) << 32);
}

inline void Clear() {
	auto &map = Map();
	for (auto &pair : map) {
		if (pair.second.line) {
			CFRelease(pair.second.line);
		}
		if (pair.second.string) {
			CFRelease(pair.second.string);
		}
		if (pair.first.font) {
			CFRelease(pair.first.font);
		}
	}
	map.clear();
}

}

class QuartzTextLayout {
public:
	/** Create a text layout for drawing. */
	QuartzTextLayout(std::string_view sv, CFStringEncoding encoding, const QuartzTextStyle *r) {
		CFMutableDictionaryRef stringAttribs = r->getCTStyle();

		const bool cacheable = CodeppTextCache::Enabled() && r->getFontRef() != NULL &&
				       sv.length() <= CodeppTextCache::kMaxTextLength;
		CodeppTextCache::Key key;
		if (cacheable) {
			key.text.assign(sv.data(), sv.length());
			key.encoding = encoding;
			key.font = r->getFontRef();
			key.colour = CodeppTextCache::PackForeground(stringAttribs);

			auto &map = CodeppTextCache::Map();
			const auto found = map.find(key);
			if (found != map.end()) {
				// Take our own reference: the entry may be
				// cleared while this layout is still alive.
				mLine = static_cast<CTLineRef>(CFRetain(found->second.line));
				mString = static_cast<CFAttributedStringRef>(CFRetain(found->second.string));
				stringLength = found->second.stringLength;
				encodingUsed = found->second.encodingUsed;
				CodeppTextCache::Stats().hits++;
				return;
			}
		}

		// --- Upstream's construction, unchanged ---------------------
		encodingUsed = encoding;
		const UInt8 *puiBuffer = reinterpret_cast<const UInt8 *>(sv.data());
		CFStringRef str = CFStringCreateWithBytes(NULL, puiBuffer, sv.length(), encodingUsed, false);
		if (!str) {
			// Failed to decode bytes into string with given encoding so try
			// MacRoman which should accept any byte.
			encodingUsed = kCFStringEncodingMacRoman;
			str = CFStringCreateWithBytes(NULL, puiBuffer, sv.length(), encodingUsed, false);
		}
		if (!str) {
			return;
		}

		stringLength = CFStringGetLength(str);

		mString = ::CFAttributedStringCreate(NULL, str, stringAttribs);

		mLine = ::CTLineCreateWithAttributedString(mString);

		CFRelease(str);
		// --- end upstream -------------------------------------------

		if (!cacheable || !mLine || !mString) {
			return;
		}
		auto &map = CodeppTextCache::Map();
		if (map.size() >= CodeppTextCache::kMaxEntries) {
			CodeppTextCache::Clear();
		}
		// The entry holds its own references to the line, the string
		// and the font; the font's keeps its address unique for as
		// long as the key uses it.
		CFRetain(key.font);
		// If the insert below throws, the three references taken here
		// leak. `std::bad_alloc` on a small node allocation is not a
		// case this codebase hardens against anywhere else, and the
		// alternative — a scope guard per insert on the paint hot
		// path — costs more than the failure is worth.
		CodeppTextCache::Entry entry;
		entry.line = static_cast<CTLineRef>(CFRetain(mLine));
		entry.string = static_cast<CFAttributedStringRef>(CFRetain(mString));
		entry.stringLength = stringLength;
		entry.encodingUsed = encodingUsed;
		const CTFontRef keyFont = key.font;
		const auto inserted = map.emplace(std::move(key), entry);
		if (!inserted.second) {
			// The lookup above missed, so reaching this means the map
			// gained the key in between — only possible if drawing
			// became re-entrant, which it is not today. Handled rather
			// than asserted because the cost is three releases on a
			// branch that never runs, while getting it wrong is a
			// silent leak of a font and two CoreText objects per call.
			CFRelease(entry.line);
			CFRelease(entry.string);
			CFRelease(keyFont);
			return;
		}
		CodeppTextCache::Stats().misses++;
	}

	~QuartzTextLayout() {
		if (mString) {
			CFRelease(mString);
			mString = NULL;
		}
		if (mLine) {
			CFRelease(mLine);
			mLine = NULL;
		}
	}

	// The object owns references; copying would double-release. Every
	// call site is a stack temporary, so this only pins what is already
	// true.
	QuartzTextLayout(const QuartzTextLayout &) = delete;
	QuartzTextLayout &operator=(const QuartzTextLayout &) = delete;

	/** Draw the text layout into a CGContext at the specified position.
	* @param gc The CGContext in which to draw the text.
	* @param x The x axis position to draw the baseline in the current CGContext.
	* @param y The y axis position to draw the baseline in the current CGContext. */
	void draw(CGContextRef gc, double x, double y) {
		if (!mLine)
			return;

		::CGContextSetTextMatrix(gc, CGAffineTransformMakeScale(1.0, -1.0));

		// Set the text drawing position.
		::CGContextSetTextPosition(gc, x, y);

		// And finally, draw!
		::CTLineDraw(mLine, gc);
	}

	float MeasureStringWidth() {
		if (mLine == NULL)
			return 0.0f;

		return static_cast<float>(::CTLineGetTypographicBounds(mLine, NULL, NULL, NULL));
	}

	CTLineRef getCTLine() {
		return mLine;
	}

	CFIndex getStringLength() {
		return stringLength;
	}

	CFStringEncoding getEncoding() {
		return encodingUsed;
	}

private:
	CFAttributedStringRef mString = NULL;
	CTLineRef mLine = NULL;
	CFIndex stringLength = 0;
	CFStringEncoding encodingUsed = kCFStringEncodingMacRoman;
};

#endif
