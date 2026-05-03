# Third-Party Licenses

Code++ itself is distributed under the [MIT License](LICENSE). It bundles or links against third-party components that carry their own licenses. The notices required by those licenses are reproduced in full below.

This file has two parts:

1. **Manually maintained** — components that are not Cargo dependencies (vendored C/C++ source, system libraries linked dynamically). These notices are kept here by hand and must be updated when a vendored component is bumped or replaced.
2. **Auto-generated at release time** — Cargo dependencies. The release tooling regenerates the bundled-crates section from `cargo about` (config in `about.toml`, added when the workspace is set up). Do not hand-edit that section; edit `about.toml` and rerun the generator instead.

---

## 1. Manually Maintained Notices

### 1.1 Scintilla

Scintilla is vendored under `crates/scintilla-sys/vendor/scintilla/` and statically linked into the Code++ binary.

> Copyright 1998-2021 by Neil Hodgson <neilh@scintilla.org>
>
> All Rights Reserved
>
> Permission to use, copy, modify, and distribute this software and its
> documentation for any purpose and without fee is hereby granted,
> provided that the above copyright notice appear in all copies and that
> both that copyright notice and this permission notice appear in
> supporting documentation.
>
> NEIL HODGSON DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS
> SOFTWARE, INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
> AND FITNESS, IN NO EVENT SHALL NEIL HODGSON BE LIABLE FOR ANY
> SPECIAL, INDIRECT OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
> WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
> WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
> TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE
> OR PERFORMANCE OF THIS SOFTWARE.

License: HPND (Historical Permission Notice and Disclaimer).
Upstream: <https://www.scintilla.org/>

### 1.2 Lexilla

Lexilla is vendored under `crates/scintilla-sys/vendor/lexilla/` and statically linked into the Code++ binary. It is distributed by the same upstream under the same terms as Scintilla:

> Copyright 1998-2021 by Neil Hodgson <neilh@scintilla.org>
>
> All Rights Reserved
>
> Permission to use, copy, modify, and distribute this software and its
> documentation for any purpose and without fee is hereby granted,
> provided that the above copyright notice appear in all copies and that
> both that copyright notice and this permission notice appear in
> supporting documentation.
>
> NEIL HODGSON DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS
> SOFTWARE, INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
> AND FITNESS, IN NO EVENT SHALL NEIL HODGSON BE LIABLE FOR ANY
> SPECIAL, INDIRECT OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
> WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
> WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
> TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE
> OR PERFORMANCE OF THIS SOFTWARE.

License: HPND.
Upstream: <https://www.scintilla.org/Lexilla.html>

### 1.3 GTK 4 (Linux only, dynamically linked)

GTK 4 and its companion libraries (GLib, Pango, Cairo, GDK-Pixbuf, Graphene) are linked **dynamically** at runtime against the system installation. They are not bundled with Code++.

License: LGPL-2.1-or-later (GTK and GLib); various permissive and LGPL terms for companions.
Upstream: <https://www.gtk.org/>

Dynamic linking against LGPL libraries is permitted from MIT-licensed software without imposing LGPL on the linking application. Distributors who wish to ship GTK alongside Code++ in a bundle (e.g. an AppImage) must additionally comply with the LGPL's relinking provisions.

### 1.4 System SDKs

- **Microsoft Windows SDK / Win32** — used under Microsoft's standard development terms.
- **Apple Cocoa / AppKit** — used under Apple's standard development terms.

Neither imposes obligations on derivative software beyond compliance with the respective platform vendor's terms.

---

## 2. Notepad++ Plugin ABI Compatibility

The headers under `plugins/nppcompat-headers/` are an **independent clean-room reimplementation** of the Notepad++ plugin ABI. No source has been copied from Notepad++ or its plugin SDK. The ABI surface (message numbers, struct layouts, function signatures, behavior contracts) is not copyrightable; the original header source is, and is therefore not used.

These headers are licensed under the same MIT License as the rest of Code++. See `plugins/nppcompat-headers/HEADER_TEMPLATE.txt` for the provenance notice required at the top of every file in that directory.

Notepad++ itself is licensed under the GNU General Public License v2 or later. Code++ does not include any Notepad++ source code.

---

## 3. Auto-Generated Cargo Dependency Notices

This section is generated at release time from the lockfile by `cargo about`. It is empty until the workspace and its dependencies are populated (Phase 0 onward).

`deny.toml` at the repo root enforces that every Cargo dependency uses a license from the allowlist (MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, 0BSD, CC0-1.0, Unicode-DFS-2016, Unicode-3.0). A dependency outside this set fails CI and must be removed or replaced before merge.

<!-- BEGIN AUTO-GENERATED CRATE LICENSES -->
<!-- (populated by `cargo about generate` at release time) -->
<!-- END AUTO-GENERATED CRATE LICENSES -->
