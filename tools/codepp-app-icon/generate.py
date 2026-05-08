#!/usr/bin/env python3
"""Generate the Code++ application-icon set from `assets/code++.png`.

Outputs (next to the source PNG):

  - `assets/code++.ico`   multi-resolution Windows icon. Embedded into
                          the `code++.exe` binary by `crates/app/build.rs`
                          and used as the title-bar / Alt+Tab / taskbar /
                          Task Manager icon at runtime.

The source `code++.png` is wider than it is tall (the chameleon mascot
on a transparent background). Windows expects square icons — squishing
the chameleon to fit a square distorts the body recognisably, while
letterboxing it onto a square transparent canvas preserves the shape
at the cost of a small vertical gap above and below at every output
size. Letterboxing is the right trade.

Future Phase 5 work will add Linux PNG renditions (16/24/32/48/64/128/256
written into `assets/icons-app/`) and a macOS `.icns`. The same source
PNG drives all three; only the wrapping changes.

One-time setup (icon-author machine only — the build does not call this
script):

    python -m pip install --user pillow

(Pillow is also pulled in transitively by the toolbar-icon pipeline's
`reportlab` dep, so on a fresh icon-author setup that already runs
`tools/codepp-icons/generate.py`, no extra install is needed.)

To regenerate:

    python tools/codepp-app-icon/generate.py
"""
from __future__ import annotations

from pathlib import Path

from PIL import Image

# Sizes Windows wants in an `.ico` for full coverage:
#
#   16  - small icon (system tray, title bar at 100% DPI, Alt+Tab fallback)
#   24  - taskbar at 100% DPI
#   32  - large icon, About box, default size for `LoadIconW`
#   48  - extra-large list-view icon
#   64  - HiDPI taskbar
#   128 - thumbnail / explorer "extra large icons" view
#   256 - Vista+ shell zoom, jump-list large icon
#
# The full set is ~30 KB on disk for our chameleon source — cheap to
# embed unconditionally, since each call to `LoadIconW` only loads the
# size it actually needs.
ICO_SIZES: list[tuple[int, int]] = [
    (16, 16),
    (24, 24),
    (32, 32),
    (48, 48),
    (64, 64),
    (128, 128),
    (256, 256),
]


def to_square_canvas(img: Image.Image) -> Image.Image:
    """Pad `img` (any aspect ratio) onto a transparent square canvas
    sized to the larger edge, with the image centred. RGBA in / out;
    callers expect the result is suitable as input to ICO encoding."""
    w, h = img.size
    side = max(w, h)
    canvas = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    # Centre offset for both axes — collapses to (0, dy) for a wide
    # source, (dx, 0) for a tall one.
    canvas.paste(img, ((side - w) // 2, (side - h) // 2), img)
    return canvas


def main() -> int:
    here = Path(__file__).parent
    # Two parents up: tools/codepp-app-icon → tools → repo root.
    repo_root = here.parent.parent
    src_png = repo_root / "assets" / "code++.png"
    out_ico = repo_root / "assets" / "code++.ico"

    if not src_png.exists():
        raise SystemExit(f"missing source: {src_png}")

    src = Image.open(src_png).convert("RGBA")
    square = to_square_canvas(src)

    # Pillow's ICO encoder auto-resizes the source to each requested
    # size with bicubic resampling and writes them all into a single
    # `.ico` file. Windows picks whichever embedded size matches the
    # current DPI / context at load time.
    square.save(out_ico, format="ICO", sizes=ICO_SIZES)

    sizes_str = ", ".join(f"{w}x{h}" for (w, h) in ICO_SIZES)
    print(f"wrote {out_ico} ({sizes_str})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
