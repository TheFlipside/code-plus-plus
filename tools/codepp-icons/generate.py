#!/usr/bin/env python3
"""Generate the Code++ toolbar icon set.

All icons are 24x24 viewBox, drawn with consistent geometry and a small
shared palette so they read as a coherent set. Outputs (under
`<repo_root>/assets/icons/`):

  - <name>.svg            individual SVGs (cross-platform vector source)
  - <name>.png            24x24 raster (Win32 toolbar default)
  - <name>@2x.png         48x48 raster (HiDPI / 200% scaling)
  - sprite.svg            single sprite sheet using <symbol>

Plus `preview.html` next to this script.

The PNG rasterisation uses `svglib` + `reportlab` (with `pycairo` as the
rendering backend — pure pip-installable, no system cairo.dll required).
On a fresh icon-author machine:

    python -m pip install --user svglib reportlab pycairo

Cairocffi must NOT be installed alongside — `rlPyCairo` prefers it but
its DLL probe fails on Windows. With only `pycairo` present, the
fallback path works.
"""
from __future__ import annotations

from io import BytesIO
from pathlib import Path

# Shared palette - tweak here to retheme the whole set.
INK = "#37474F"      # outlines / dark accents
PAPER = "#FAFAFA"    # document body
BLUE = "#2196F3"     # save / disk
GREEN = "#43A047"    # play / new / run
RED = "#E53935"      # close / stop / record
ORANGE = "#FB8C00"   # paste / highlight
YELLOW = "#FBC02D"   # folder
GRAY = "#90A4AE"     # secondary lines

SVG_OPEN = (
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" '
    'width="24" height="24" fill="none" '
    'stroke-linecap="round" stroke-linejoin="round">'
)
SVG_CLOSE = "</svg>"


def page(extra: str = "", body_fill: str = PAPER) -> str:
    """A document outline shared by several icons."""
    return (
        f'<path d="M6 2 H14 L20 8 V22 H6 Z" fill="{body_fill}" '
        f'stroke="{INK}" stroke-width="1.5"/>'
        f'<path d="M14 2 V8 H20" stroke="{INK}" stroke-width="1.5"/>'
        f"{extra}"
    )


ICONS: dict[str, str] = {}


# ---------- File ops ----------------------------------------------------------

ICONS["new"] = page(
    f'<circle cx="17.5" cy="17.5" r="4.5" fill="{GREEN}"/>'
    '<path d="M17.5 15.3 V19.7 M15.3 17.5 H19.7" '
    'stroke="#FFFFFF" stroke-width="1.6"/>'
)

ICONS["open"] = (
    f'<path d="M2.5 7 H9 L11 9 H21 V19 H2.5 Z" fill="{YELLOW}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M4.5 19 L7 11 H22 L19.5 19 Z" fill="#FFE082" '
    f'stroke="{INK}" stroke-width="1.5"/>'
)

ICONS["save"] = (
    f'<path d="M4 3 H18 L21 6 V21 H4 Z" fill="{BLUE}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<rect x="7" y="3" width="9" height="6" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<rect x="13" y="4.5" width="2" height="3" fill="{INK}"/>'
    f'<rect x="7" y="13" width="11" height="8" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M9 16 H16 M9 18.5 H14" stroke="{INK}" stroke-width="1.2"/>'
)

ICONS["tab-save"] = (
    # Tab-strip floppy: same geometry family as ICONS["save"] (above)
    # but rendered at 16/32 px for the tab strip rather than 24/48 for
    # the toolbar. Drawn over a slightly tighter 24-unit viewBox so the
    # strokes don't dissolve when the rasteriser scales down to 16 px.
    f'<path d="M4 3 H18 L21 6 V21 H4 Z" fill="{BLUE}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<rect x="7" y="3" width="9" height="6" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<rect x="13" y="4.5" width="2" height="3" fill="{INK}"/>'
    f'<rect x="7" y="13" width="11" height="8" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M9 16 H16 M9 18.5 H14" stroke="{INK}" stroke-width="1.2"/>'
)

ICONS["tab-save-dirty"] = (
    # Same geometry as `tab-save` with the floppy body recoloured red
    # so a glance at the tab strip shows which buffers have unsaved
    # changes. The stroke / sticker / label / write-protect-tab all
    # stay identical to the saved variant for visual continuity.
    f'<path d="M4 3 H18 L21 6 V21 H4 Z" fill="{RED}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<rect x="7" y="3" width="9" height="6" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<rect x="13" y="4.5" width="2" height="3" fill="{INK}"/>'
    f'<rect x="7" y="13" width="11" height="8" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M9 16 H16 M9 18.5 H14" stroke="{INK}" stroke-width="1.2"/>'
)

ICONS["save-all"] = (
    # back disk
    f'<path d="M2 6 H14 L17 9 V19 H2 Z" fill="{GRAY}" '
    f'stroke="{INK}" stroke-width="1.4"/>'
    # front disk
    f'<path d="M7 9 H19 L22 12 V22 H7 Z" fill="{BLUE}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<rect x="10" y="9" width="7" height="4.5" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.4"/>'
    f'<rect x="14.5" y="10" width="1.5" height="2.5" fill="{INK}"/>'
    f'<rect x="10" y="16" width="9" height="6" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.4"/>'
)

ICONS["close"] = page(
    f'<circle cx="17.5" cy="17.5" r="4.5" fill="{RED}"/>'
    '<path d="M15.7 15.7 L19.3 19.3 M19.3 15.7 L15.7 19.3" '
    'stroke="#FFFFFF" stroke-width="1.6"/>'
)

ICONS["close-all"] = (
    f'<path d="M3 4 H10 L15 9 V18 H3 Z" fill="{GRAY}" '
    f'stroke="{INK}" stroke-width="1.4"/>'
    f'<path d="M10 4 V9 H15" stroke="{INK}" stroke-width="1.4"/>'
    f'<path d="M8 8 H15 L20 13 V22 H8 Z" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M15 8 V13 H20" stroke="{INK}" stroke-width="1.5"/>'
    f'<circle cx="17" cy="18" r="3.5" fill="{RED}"/>'
    '<path d="M15.5 16.5 L18.5 19.5 M18.5 16.5 L15.5 19.5" '
    'stroke="#FFFFFF" stroke-width="1.4"/>'
)

ICONS["print"] = (
    f'<path d="M7 3 H17 V8 H7 Z" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M3 8 H21 V17 H17 V21 H7 V17 H3 Z" fill="{GRAY}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<rect x="7" y="13" width="10" height="8" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M9 16 H15 M9 18.5 H14" stroke="{INK}" stroke-width="1.2"/>'
    f'<circle cx="18" cy="11" r="0.9" fill="{GREEN}"/>'
)

# ---------- Edit ops ----------------------------------------------------------

ICONS["cut"] = (
    # blades meeting at a pivot
    f'<path d="M14 12 L21 4 M14 12 L21 20" stroke="{INK}" '
    f'stroke-width="1.5"/>'
    f'<circle cx="7" cy="7.5" r="3.2" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<circle cx="7" cy="16.5" r="3.2" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M9.4 9.6 L21 20 M9.4 14.4 L21 4" '
    f'stroke="{INK}" stroke-width="1.5" fill="none"/>'
)

ICONS["copy"] = (
    f'<path d="M3 6 H11 L14 9 V18 H3 Z" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M11 6 V9 H14" stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M9 3 H17 L21 7 V21 H9 Z" fill="{BLUE}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M17 3 V7 H21" stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M12 12 H18 M12 15 H17 M12 18 H16" '
    f'stroke="#FFFFFF" stroke-width="1.2"/>'
)

ICONS["paste"] = (
    # clipboard
    f'<path d="M5 5 H19 V22 H5 Z" fill="{ORANGE}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M9 3 H15 V7 H9 Z" fill="{INK}"/>'
    # page on top
    f'<path d="M8 10 H14 L17 13 V20 H8 Z" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.4"/>'
    f'<path d="M14 10 V13 H17" stroke="{INK}" stroke-width="1.4"/>'
    f'<path d="M10 15 H15 M10 17.5 H14" stroke="{INK}" stroke-width="1.1"/>'
)

ICONS["undo"] = (
    # arc curving up-and-over from the arrowhead toward the upper-right
    f'<path d="M10 13 Q14 3 21 8" stroke="{BLUE}" stroke-width="2.8" '
    f'fill="none" stroke-linecap="round"/>'
    # large arrowhead (8x12 region) - stays visible at 16px
    f'<path d="M2 13 L10 7 L10 19 Z" fill="{BLUE}"/>'
)

ICONS["redo"] = (
    f'<path d="M14 13 Q10 3 3 8" stroke="{BLUE}" stroke-width="2.8" '
    f'fill="none" stroke-linecap="round"/>'
    f'<path d="M22 13 L14 7 L14 19 Z" fill="{BLUE}"/>'
)

# ---------- Search ------------------------------------------------------------

ICONS["find"] = (
    f'<circle cx="10.5" cy="10.5" r="6" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.8"/>'
    f'<path d="M15 15 L21 21" stroke="{INK}" stroke-width="2.2"/>'
    f'<path d="M7.5 9 A3.5 3.5 0 0 1 10.5 7" fill="none" '
    f'stroke="{BLUE}" stroke-width="1.4"/>'
)

ICONS["find-next"] = (
    f'<circle cx="10.5" cy="10.5" r="6" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.8"/>'
    f'<path d="M15 15 L21 21" stroke="{INK}" stroke-width="2.2"/>'
    f'<path d="M8 10.5 H13 M11 8.5 L13 10.5 L11 12.5" '
    f'stroke="{GREEN}" stroke-width="1.6" fill="none"/>'
)

ICONS["replace"] = (
    f'<circle cx="10.5" cy="10.5" r="6" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.8"/>'
    f'<path d="M15 15 L21 21" stroke="{INK}" stroke-width="2.2"/>'
    f'<path d="M8 9.2 H12.5 M11 7.8 L12.5 9.2 L11 10.6" '
    f'stroke="{BLUE}" stroke-width="1.4" fill="none"/>'
    f'<path d="M13 11.8 H8.5 M10 10.4 L8.5 11.8 L10 13.2" '
    f'stroke="{ORANGE}" stroke-width="1.4" fill="none"/>'
)

# ---------- View / zoom -------------------------------------------------------

ICONS["zoom-in"] = (
    f'<circle cx="10.5" cy="10.5" r="6" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.8"/>'
    f'<path d="M15 15 L21 21" stroke="{INK}" stroke-width="2.2"/>'
    f'<path d="M10.5 7.5 V13.5 M7.5 10.5 H13.5" '
    f'stroke="{GREEN}" stroke-width="1.8"/>'
)

ICONS["zoom-out"] = (
    f'<circle cx="10.5" cy="10.5" r="6" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.8"/>'
    f'<path d="M15 15 L21 21" stroke="{INK}" stroke-width="2.2"/>'
    f'<path d="M7.5 10.5 H13.5" stroke="{RED}" stroke-width="1.8"/>'
)

ICONS["word-wrap"] = (
    f'<path d="M3 6 H21" stroke="{INK}" stroke-width="1.6"/>'
    f'<path d="M3 12 H17 A3 3 0 0 1 17 18 H13" '
    f'stroke="{BLUE}" stroke-width="1.6" fill="none"/>'
    f'<path d="M15 16 L13 18 L15 20" stroke="{BLUE}" '
    f'stroke-width="1.6" fill="none"/>'
    f'<path d="M3 18 H10" stroke="{INK}" stroke-width="1.6"/>'
)

ICONS["show-whitespace"] = (
    # paragraph mark
    f'<path d="M16 4 H10 A4 4 0 0 0 10 12 H12 V20" '
    f'fill="none" stroke="{INK}" stroke-width="1.8"/>'
    f'<path d="M16 4 V20" stroke="{INK}" stroke-width="1.8"/>'
    f'<circle cx="6" cy="14" r="0.9" fill="{GRAY}"/>'
    f'<circle cx="6" cy="18" r="0.9" fill="{GRAY}"/>'
)

# ---------- Macro / run -------------------------------------------------------

ICONS["macro-record"] = (
    f'<circle cx="12" cy="12" r="6.5" fill="{RED}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<circle cx="12" cy="12" r="2.5" fill="#FFFFFF" opacity="0.85"/>'
)

ICONS["macro-play"] = (
    f'<path d="M7 4 L20 12 L7 20 Z" fill="{GREEN}" '
    f'stroke="{INK}" stroke-width="1.5" stroke-linejoin="round"/>'
)

ICONS["macro-stop"] = (
    f'<rect x="5" y="5" width="14" height="14" rx="1.5" '
    f'fill="{RED}" stroke="{INK}" stroke-width="1.5"/>'
)

ICONS["macro-pause"] = (
    f'<rect x="6" y="4" width="4" height="16" rx="1" '
    f'fill="{ORANGE}" stroke="{INK}" stroke-width="1.5"/>'
    f'<rect x="14" y="4" width="4" height="16" rx="1" '
    f'fill="{ORANGE}" stroke="{INK}" stroke-width="1.5"/>'
)

ICONS["run"] = (
    # lightning bolt
    f'<path d="M13 2 L4 14 H11 L9 22 L20 9 H13 Z" fill="{YELLOW}" '
    f'stroke="{INK}" stroke-width="1.5" stroke-linejoin="round"/>'
)


# ---------- View / sync scrolling --------------------------------------------

def window_bg(x: float = 2, y: float = 3, w: float = 20, h: float = 16,
              titlebar: float = 2.8, fill: str = PAPER, accent: str = GRAY,
              outline_w: float = 1.4) -> str:
    """Shared editor-window background — outlined rectangle with a
    titlebar separator. Used by icons that depict an editor view
    (`document-map`, `function-list`, `monitoring`, etc.); the
    parameters cover the size/colour variations across that family."""
    return (
        f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="1" '
        f'fill="{fill}" stroke="{INK}" stroke-width="{outline_w}"/>'
        f'<path d="M{x} {y + titlebar} H{x + w}" '
        f'stroke="{INK}" stroke-width="{outline_w}"/>'
        f'<circle cx="{x + 1.4}" cy="{y + titlebar / 2}" r="0.5" fill="{accent}"/>'
        f'<circle cx="{x + 2.8}" cy="{y + titlebar / 2}" r="0.5" fill="{accent}"/>'
        f'<circle cx="{x + 4.2}" cy="{y + titlebar / 2}" r="0.5" fill="{accent}"/>'
    )


def lock(x: float, y: float, body_color: str = YELLOW) -> str:
    """A small padlock anchored at (x, y) as the top-left of the body."""
    return (
        # shackle
        f'<path d="M{x + 1} {y} V{y - 1.5} A1.5 1.5 0 0 1 {x + 4} {y - 1.5} V{y}" '
        f'fill="none" stroke="{INK}" stroke-width="1.2"/>'
        # body
        f'<rect x="{x}" y="{y}" width="5" height="4" rx="0.5" '
        f'fill="{body_color}" stroke="{INK}" stroke-width="1.2"/>'
    )


ICONS["sync-scroll-vertical"] = (
    window_bg(x=2, y=2, w=18, h=15)
    + f'<line x1="11" y1="6" x2="11" y2="16" stroke="{BLUE}" '
      f'stroke-width="1.6" stroke-dasharray="2 1.5"/>'
    + lock(16, 17)
)

ICONS["sync-scroll-horizontal"] = (
    window_bg(x=2, y=2, w=18, h=15)
    + f'<line x1="3" y1="11" x2="19" y2="11" stroke="{BLUE}" '
      f'stroke-width="1.6" stroke-dasharray="2 1.5"/>'
    + lock(16, 17)
)

# ---------- Show all characters (pilcrow ¶) ----------------------------------

ICONS["show-all-chars"] = (
    # bowl + two stems, drawn boldly so the pilcrow reads even at 16px
    f'<path d="M17 4 H10 A4 4 0 0 0 10 12 H12 V20" '
    f'fill="none" stroke="{INK}" stroke-width="2.4" stroke-linecap="round"/>'
    f'<path d="M17 4 V20" stroke="{INK}" stroke-width="2.4" stroke-linecap="round"/>'
)

# ---------- Show indent guide ------------------------------------------------

ICONS["show-indent-guide"] = (
    # five lines of "code" at varying indent depths (top-level → indent → deep
    # → indent → top-level), drawn as short horizontal strokes
    f'<path d="M3 5 H20" stroke="{INK}" stroke-width="1.5" '
    f'stroke-linecap="round"/>'
    f'<path d="M7 9 H18" stroke="{INK}" stroke-width="1.5" '
    f'stroke-linecap="round"/>'
    f'<path d="M11 13 H19" stroke="{INK}" stroke-width="1.5" '
    f'stroke-linecap="round"/>'
    f'<path d="M7 17 H17" stroke="{INK}" stroke-width="1.5" '
    f'stroke-linecap="round"/>'
    f'<path d="M3 21 H15" stroke="{INK}" stroke-width="1.5" '
    f'stroke-linecap="round"/>'
    # red dashed vertical guides at the two indent columns
    f'<path d="M7 6.5 V19.5" stroke="{RED}" stroke-width="1.2" '
    f'stroke-dasharray="1.5 1"/>'
    f'<path d="M11 10.5 V15.5" stroke="{RED}" stroke-width="1.2" '
    f'stroke-dasharray="1.5 1"/>'
)

# ---------- Define your language ---------------------------------------------

ICONS["define-language"] = (
    window_bg(x=2, y=3, w=20, h=18, titlebar=3)
    # faint code lines under the bolt
    + f'<path d="M5 10 H13 M5 13 H16 M5 16 H10" '
      f'stroke="{GRAY}" stroke-width="1"/>'
    # lightning bolt foreground
    + f'<path d="M15 6 L8 14 H12 L10 21 L18 12 H13.5 Z" '
      f'fill="{YELLOW}" stroke="{INK}" stroke-width="1.4" '
      f'stroke-linejoin="round"/>'
)

# ---------- Document map (folded paper map) ----------------------------------

ICONS["document-map"] = (
    # tri-fold map outline with alternating vertical offsets
    f'<path d="M2 6 L9 4 L16 6 L22 4 V18 L16 20 L9 18 L2 20 Z" '
    f'fill="{PAPER}" stroke="{INK}" stroke-width="1.5" '
    f'stroke-linejoin="round"/>'
    # fold creases
    f'<path d="M9 4 V18 M16 6 V20" stroke="{INK}" '
    f'stroke-width="1.2" stroke-dasharray="1.5 1"/>'
    # tiny route + pin
    f'<path d="M5 14 Q11 9 18 12" fill="none" stroke="{RED}" '
    f'stroke-width="1.4" stroke-linecap="round"/>'
    f'<circle cx="18" cy="12" r="1.4" fill="{RED}" '
    f'stroke="{INK}" stroke-width="0.8"/>'
)

# ---------- Document list (stack over window) --------------------------------

ICONS["document-list"] = (
    # window mockup behind (faint, no fill)
    f'<rect x="1.5" y="2.5" width="21" height="19" rx="1.2" '
    f'fill="none" stroke="{GRAY}" stroke-width="1.2"/>'
    f'<line x1="1.5" y1="5.5" x2="22.5" y2="5.5" '
    f'stroke="{GRAY}" stroke-width="1.2"/>'
    # back sheet
    f'<path d="M5 8 H10 L13 11 V19 H5 Z" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.3"/>'
    # middle sheet (offset)
    f'<path d="M8 10 H13 L16 13 V21 H8 Z" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.3"/>'
    # front sheet (highlighted)
    f'<path d="M11 7 H17 L20 10 V18 H11 Z" fill="{BLUE}" '
    f'stroke="{INK}" stroke-width="1.3"/>'
    f'<path d="M17 7 V10 H20" stroke="{INK}" stroke-width="1.3"/>'
    f'<path d="M13 12.5 H18 M13 14.5 H17 M13 16.5 H16" '
    f'stroke="#FFFFFF" stroke-width="1"/>'
)

# ---------- Function list (stylized ƒ over sheet) ----------------------------

ICONS["function-list"] = (
    # paper sheet
    f'<path d="M5 3 H14 L19 8 V21 H5 Z" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.5"/>'
    f'<path d="M14 3 V8 H19" stroke="{INK}" stroke-width="1.5"/>'
    # bold italic-ish ƒ: hook at top, slanted stem, descender, crossbar
    f'<path d="M15 9 C13.5 9 12.5 9.5 12 11.5 L9 20 '
    f'C8.5 21.5 7.5 22 6 21" '
    f'fill="none" stroke="{ORANGE}" stroke-width="2.6" '
    f'stroke-linecap="round" stroke-linejoin="round"/>'
    f'<path d="M9 13.5 H13.5" stroke="{ORANGE}" '
    f'stroke-width="2.4" stroke-linecap="round"/>'
)

# ---------- Folder as workspace (red folder, no paper) -----------------------

ICONS["folder-workspace"] = (
    # closed folder body, red instead of yellow
    f'<path d="M2.5 7 H9 L11 9 H21 V19 H2.5 Z" fill="#EF5350" '
    f'stroke="{INK}" stroke-width="1.5" stroke-linejoin="round"/>'
    # tab seam to give the folder some structure
    f'<path d="M2.5 9.8 H21" stroke="{INK}" stroke-width="1.1"/>'
    # subtle highlight band
    f'<path d="M3.5 11.2 H20" stroke="#FFFFFF" '
    f'stroke-width="0.8" opacity="0.5"/>'
)

# ---------- Monitoring tail -f (open eye) ------------------------------------

ICONS["monitoring"] = (
    # almond-shaped eye outline
    f'<path d="M2 12 Q12 4 22 12 Q12 20 2 12 Z" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.6" stroke-linejoin="round"/>'
    # iris
    f'<circle cx="12" cy="12" r="4" fill="{BLUE}" '
    f'stroke="{INK}" stroke-width="1.4"/>'
    # pupil
    f'<circle cx="12" cy="12" r="1.8" fill="{INK}"/>'
    # catchlight
    f'<circle cx="13.3" cy="10.7" r="0.8" fill="#FFFFFF"/>'
)

# ---------- Save current recorded macro --------------------------------------

ICONS["save-macro"] = (
    # editor window background, shifted toward top-left so the floppy
    # can "hover" over the bottom-right corner
    f'<rect x="1" y="2" width="17" height="16" rx="1" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.3"/>'
    f'<path d="M1 4.5 H18" stroke="{INK}" stroke-width="1.3"/>'
    f'<circle cx="2.6" cy="3.25" r="0.4" fill="{GRAY}"/>'
    f'<circle cx="4" cy="3.25" r="0.4" fill="{GRAY}"/>'
    f'<circle cx="5.4" cy="3.25" r="0.4" fill="{GRAY}"/>'
    # text lines
    f'<path d="M3 7 H11 M3 9 H14 M3 11 H9 M3 13 H12 M3 15 H10" '
    f'stroke="{GRAY}" stroke-width="0.9"/>'
    # floppy hovering on bottom right
    f'<path d="M9 10 H19 L22 13 V22 H9 Z" fill="{BLUE}" '
    f'stroke="{INK}" stroke-width="1.4"/>'
    f'<rect x="11" y="10" width="7" height="3.5" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.1"/>'
    f'<rect x="15" y="10.7" width="1.5" height="2.3" fill="{INK}"/>'
    f'<rect x="11" y="16" width="9" height="6" fill="{PAPER}" '
    f'stroke="{INK}" stroke-width="1.1"/>'
    f'<path d="M12.5 18 H18.5 M12.5 20 H17" '
    f'stroke="{INK}" stroke-width="0.9"/>'
)


def wrap(body: str) -> str:
    """Wrap an icon body (just the inner shapes) in the shared
    `<svg viewBox="0 0 24 24">` envelope. Returns a complete SVG
    document as a string, ready to write to disk or hand to the
    PNG rasteriser."""
    return f"{SVG_OPEN}{body}{SVG_CLOSE}\n"


# Raster sizes baked into `assets/icons/` next to the SVGs. 24px is the
# Win32 toolbar default; 48px is the HiDPI / 200%-scaling sibling.
# `Win32Ui` picks one at runtime based on the system DPI.
PNG_SIZES = (
    ("", 24),  # foo.png
    ("@2x", 48),  # foo@2x.png
)

# Tab-strip icons live next to the toolbar set but at half the pixel
# count — the strip is too short for a 24-px icon. Names prefixed
# `tab-` get this size pair instead of `PNG_SIZES`. `Win32Ui` picks
# 16 vs 32 at runtime via `pick_tab_bitmap_size()` (mirrors the
# toolbar's HiDPI threshold).
TAB_PNG_SIZES = (
    ("", 16),  # foo.png
    ("@2x", 32),  # foo@2x.png
)


def render_png(svg_text: str, size: int) -> bytes:
    """Rasterise an SVG string to a PNG byte-string at `size`x`size`,
    with a transparent background.

    `rlPyCairo` (reportlab's PNG backend) only emits 3-channel RGB
    even when the SVG declares transparent regions — opaque white
    fills the gaps. To recover transparency for toolbar use the RGB
    output is post-processed: every pixel whose colour is exactly
    `#FFFFFF` is treated as background and gets alpha = 0; every
    other pixel keeps alpha = 255. The icon set deliberately uses
    `PAPER = #FAFAFA` (off-white) for document bodies so the
    threshold doesn't punch holes through icon interiors.

    Imports happen here (lazy) so a partial install — say `svglib`
    present but `pycairo` missing — only blocks the PNG step, not
    the SVG step that doesn't need either. PIL is a transitive dep
    of reportlab so it's always available wherever the PNG path is.
    """
    from PIL import Image, ImageChops
    from reportlab.graphics import renderPM
    from svglib.svglib import svg2rlg

    drawing = svg2rlg(BytesIO(svg_text.encode("utf-8")))
    if drawing is None or drawing.width <= 0 or drawing.height <= 0:
        # `svg2rlg` returns None when the SVG fails to parse; checking
        # `.width` first would surface as a misleading AttributeError.
        # The current icon set is all-square 24x24 by construction, so
        # this branch only fires after a future malformed icon edit.
        raise RuntimeError(
            "svg2rlg failed to produce a non-empty drawing — "
            "SVG body malformed or header missing?"
        )
    assert drawing.width == drawing.height, (
        "render_png expects a square SVG (every icon uses viewBox 0 0 24 24); "
        f"got {drawing.width} x {drawing.height}"
    )
    scale = size / drawing.width
    drawing.width *= scale
    drawing.height *= scale
    drawing.scale(scale, scale)

    rgb_png = renderPM.drawToString(drawing, fmt="PNG")
    img = Image.open(BytesIO(rgb_png)).convert("RGB")
    # Build a single-channel alpha mask: 0 where the pixel exactly
    # equals (255,255,255), 255 elsewhere. ImageChops.difference is
    # the C-fast path for this — list-comprehending pixels in pure
    # Python is ~50x slower at icon-set scale.
    white = Image.new("RGB", img.size, (255, 255, 255))
    diff = ImageChops.difference(img, white).convert("L")
    alpha = diff.point(lambda v: 255 if v > 0 else 0)
    img.putalpha(alpha)

    out = BytesIO()
    img.save(out, format="PNG", optimize=True)
    return out.getvalue()


def main() -> int:
    """Generate every SVG and PNG into `<repo_root>/assets/icons/`,
    plus the sprite sheet, then print a one-line summary. Returns
    the exit code (always `0` — failures raise instead)."""
    here = Path(__file__).parent
    # Output goes to <repo_root>/assets/icons/. The script lives at
    # <repo_root>/tools/codepp-icons/, so two parents up is the root.
    repo_root = here.parent.parent
    icons_dir = repo_root / "assets" / "icons"
    icons_dir.mkdir(parents=True, exist_ok=True)

    # individual SVGs
    for name, body in ICONS.items():
        (icons_dir / f"{name}.svg").write_text(wrap(body), encoding="utf-8")

    # PNGs at every size in PNG_SIZES (toolbar) or TAB_PNG_SIZES (tab
    # strip — names prefixed `tab-`), named <name>.png and <name>@2x.png.
    for name, body in ICONS.items():
        svg_text = wrap(body)
        sizes = TAB_PNG_SIZES if name.startswith("tab-") else PNG_SIZES
        for suffix, px in sizes:
            (icons_dir / f"{name}{suffix}.png").write_bytes(render_png(svg_text, px))

    # sprite sheet using <symbol id="icon-name">
    symbols = []
    for name, body in ICONS.items():
        symbols.append(
            f'<symbol id="icon-{name}" viewBox="0 0 24 24">{body}</symbol>'
        )
    sprite = (
        '<svg xmlns="http://www.w3.org/2000/svg" '
        'style="display:none" aria-hidden="true">'
        + "".join(symbols)
        + "</svg>\n"
    )
    (icons_dir / "sprite.svg").write_text(sprite, encoding="utf-8")

    n = len(ICONS)
    n_tab = sum(1 for name in ICONS if name.startswith("tab-"))
    n_toolbar = n - n_tab
    n_pngs = n_toolbar * len(PNG_SIZES) + n_tab * len(TAB_PNG_SIZES)
    sizes = " + ".join(f"{px}px" for _, px in PNG_SIZES)
    tab_sizes = " + ".join(f"{px}px" for _, px in TAB_PNG_SIZES)
    print(
        f"wrote {n} SVGs and {n_pngs} PNGs "
        f"({n_toolbar} toolbar @ {sizes}, {n_tab} tab @ {tab_sizes}) to {icons_dir}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
