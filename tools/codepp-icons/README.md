# Code++ icon set

Original toolbar icons drawn from scratch for the Code++ editor. The visual
vocabulary mirrors what users expect from Notepad++ and similar editors
(scissors = cut, floppy = save, magnifier = find, etc.) — these conventions
are not protected — but every icon here is original artwork.

## Contents

The generator writes into `<repo_root>/assets/icons/` (alongside the rest
of the cross-platform asset tree). Each icon has three artefacts:

```
<repo_root>/assets/icons/
  <name>.svg                 vector source (cross-platform reference)
  <name>.png                 24x24 raster (Win32 toolbar default size)
  <name>@2x.png              48x48 raster (HiDPI / 200%-scaling sibling)
  sprite.svg                 every icon as <symbol id="icon-NAME">
```

Icon names (35):

```
new                  open                 folder-workspace
save                 save-all             save-macro
close                close-all            print
cut                  copy                 paste
undo                 redo
find                 find-next            replace
zoom-in              zoom-out
word-wrap            show-whitespace      show-all-chars
show-indent-guide
sync-scroll-vertical                      sync-scroll-horizontal
document-map         document-list        function-list
define-language                           monitoring
macro-record         macro-play           macro-pause
macro-stop           run
```

24x24 viewBox, hand-tuned at small sizes but scale cleanly to any size
since they are vector.

## Palette

Defined at the top of `generate.py` — change six constants and the whole
set retheme together:

| Token   | Hex       | Used for                             |
|---------|-----------|--------------------------------------|
| INK     | `#37474F` | outlines, dark accents               |
| PAPER   | `#FAFAFA` | document body fill                   |
| BLUE    | `#2196F3` | save / disk / undo-redo / find arrow |
| GREEN   | `#43A047` | new / play / find-next / zoom-in     |
| RED     | `#E53935` | close / stop / record / zoom-out     |
| ORANGE  | `#FB8C00` | paste / replace-with / pause         |
| YELLOW  | `#FBC02D` | folder / run                         |
| GRAY    | `#90A4AE` | secondary accents                    |

## Integration

### Sprite sheet (recommended for the editor toolbar)

Inline `sprite.svg` once on the page (or `<object data="sprite.svg">` for
external load), then reference each icon by id:

```html
<svg width="24" height="24"><use href="#icon-save"/></svg>
<svg width="24" height="24"><use href="#icon-find"/></svg>
```

One HTTP request, all 24 icons available, browser-cached.

### Individual files

Drop them into your asset bundle and load per-button:

```html
<button title="Save">
  <img src="assets/icons/save.svg" width="24" height="24" alt="">
</button>
```

### Native toolkits

Most toolkits (Qt, GTK, .NET MAUI, etc.) load SVG via their image loaders
directly. The Win32 toolbar wants bitmaps via `HIMAGELIST`, so the
generator already writes both 24px and 48px PNGs alongside each SVG.
`crates/ui_win32` embeds them via `include_bytes!` at compile time and
picks 24px or 48px at runtime based on system DPI.

## Browser preview

`preview.html` next to this README renders a mocked-up toolbar with
every icon in the set, useful for eyeballing changes after a tweak to
the palette or a new icon body. It loads each SVG at runtime via
`fetch()`, which browsers refuse from a `file://` URL — opening the
HTML by double-click produces a blank page and CORS errors in the
console. Serve over HTTP instead, from the repository root:

```bash
python -m http.server 8000
# then open http://localhost:8000/tools/codepp-icons/preview.html
```

Any port works — `8000` is just the `http.server` default. Stop the
server with Ctrl+C when done.

## Regenerating / customizing

One-time setup (icon-author machine only — the build itself never runs
this script):

```bash
python -m pip install --user svglib reportlab pycairo
```

`pycairo` ships with the cairo C library statically linked into its
`.pyd`, so no system `libcairo-2.dll` is required. Cairocffi must NOT be
installed alongside; `rlPyCairo` prefers cairocffi but its DLL probe
fails on Windows, masking the working pycairo fallback. If a previous
install left cairocffi behind: `python -m pip uninstall cairocffi`.

To regenerate every SVG and PNG from `generate.py`:

```bash
python tools/codepp-icons/generate.py
```

To add an icon: add an entry to the `ICONS` dict in `generate.py` with
its SVG body (just the inner shapes — the `<svg>` wrapper is added for
you). Use the existing palette constants so it stays on-theme.

## Licensing note

This artwork is original. You can ship it under whatever license Code++
uses — MIT, GPL, Apache-2.0, etc. The Notepad++ project's own icons are
not included or referenced here; only the universal visual conventions
(floppy=save, scissors=cut, …) are shared, and those conventions are not
copyrightable.
