# Third-Party Licenses

Code++ itself is distributed under the [MIT License](LICENSE). It bundles or links against third-party components that carry their own licenses. The notices required by those licenses are reproduced in full below.

This file has two parts:

1. **Manually maintained** — components that are not Cargo dependencies (vendored C/C++ source, system libraries linked dynamically, and the one prebuilt library a package ships). These notices are kept here by hand and must be updated when a vendored component is bumped or replaced.
2. **Generated at release time** — Cargo dependencies. Their notices are not kept in this file: the release workflow writes them to `THIRD_PARTY_CRATES.md` with `cargo about` (config in `about.toml`, template in `about.hbs`) and puts that file beside this one in every release archive. See §3.

---

## 1. Manually Maintained Notices

### 1.1 Scintilla

Scintilla is vendored under `crates/scintilla-sys/vendor/scintilla/` and statically linked into the Code++ binary.

Code++ ships **two modifications of Scintilla**, each kept in a file of its own, with the vendored submodule itself left unmodified:

- `crates/scintilla-sys/cxx/QuartzTextLayout.h`, a copy of the vendored `cocoa/QuartzTextLayout.h` that memoises CoreText line layout — the macOS keystroke-latency fix recorded in `docs/DESIGN.md` §8. `crates/scintilla-sys/build.rs` overlays the copy at build time.
- `crates/scintilla-sys/cxx/CopySelectionRange.cxx`, Code++'s version of `Editor::CopySelectionRange` from the vendored `src/Editor.cxx`, which copies a selection of several ranges as Notepad++ does — recorded in `docs/DESIGN.md` §7.4. `crates/scintilla-sys/build.rs` compiles a copy of `Editor.cxx` with this function in place of upstream's.

Each file retains upstream's attribution and states what was changed, which is what the licence below requires of a modified copy.

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

### 1.3 GTK 3 (Linux only, dynamically linked)

GTK 3 and its companion libraries (GLib, Pango, Cairo, GDK-Pixbuf, ATK) are linked **dynamically** at runtime against the system installation. They are not bundled with Code++.

License: LGPL-2.1-or-later (GTK and GLib); various permissive and LGPL terms for companions.
Upstream: <https://www.gtk.org/>

Dynamic linking against LGPL libraries is permitted from MIT-licensed software without imposing LGPL on the linking application. Distributors who wish to ship GTK alongside Code++ in a bundle (e.g. an AppImage) must additionally comply with the LGPL's relinking provisions.

### 1.4 WinSparkle (Windows installer only, a separate DLL)

The Windows installer puts `WinSparkle.dll` beside `codepp.exe`. It is the library an installed Code++ checks for updates and downloads them with (`docs/DESIGN.md` §11). It is WinSparkle 0.9.4's own x64 build, unmodified, as its project publishes it: the release packaging downloads the project's release zip and checks it by hash (`.forgejo/scripts/package-windows.ps1`). Code++ does not link it; it loads the file at run time. The Windows zip and the Linux and macOS packages do not carry it.

```text
Copyright (c) 2009-2026 Vaclav Slavik

Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies
of the Software, and to permit persons to whom the Software is furnished to do
so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.




This product includes software developed by the OpenSSL Project
for use in the OpenSSL Toolkit (http://www.openssl.org/).
```

License: MIT.
Upstream: <https://winsparkle.org/>, <https://github.com/vslavik/winsparkle>

WinSparkle's build links four other libraries into that DLL. Their versions below are the ones WinSparkle's source names at its `v0.9.4` tag, as the commits of its submodules; that the published DLL was built from exactly those is taken from that, not checked against the binary.

#### wxWidgets 3.2.10

License: wxWindows Library Licence, Version 3.1. That is the GNU Library General Public Licence, version 2 or later, with an exception that lets binary object code versions of works based on the library, which the DLL is, be distributed under terms of the distributor's choosing.
Upstream: <https://www.wxwidgets.org/>

```text
                wxWindows Library Licence, Version 3.1
                ======================================

  Copyright (c) 1998-2005 Julian Smart, Robert Roebling et al

  Everyone is permitted to copy and distribute verbatim copies
  of this licence document, but changing it is not allowed.

                       WXWINDOWS LIBRARY LICENCE
     TERMS AND CONDITIONS FOR COPYING, DISTRIBUTION AND MODIFICATION

  This library is free software; you can redistribute it and/or modify it
  under the terms of the GNU Library General Public Licence as published by
  the Free Software Foundation; either version 2 of the Licence, or (at
  your option) any later version.

  This library is distributed in the hope that it will be useful, but
  WITHOUT ANY WARRANTY; without even the implied warranty of
  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU Library
  General Public Licence for more details.

  You should have received a copy of the GNU Library General Public Licence
  along with this software, usually in a file named COPYING.LIB.  If not,
  write to the Free Software Foundation, Inc., 51 Franklin Street, Fifth Floor,
  Boston, MA 02110-1301 USA.

  EXCEPTION NOTICE

  1. As a special exception, the copyright holders of this library give
  permission for additional uses of the text contained in this release of
  the library as licenced under the wxWindows Library Licence, applying
  either version 3.1 of the Licence, or (at your option) any later version of
  the Licence as published by the copyright holders of version
  3.1 of the Licence document.

  2. The exception is that you may use, copy, link, modify and distribute
  under your own terms, binary object code versions of works based
  on the Library.

  3. If you copy code from files distributed under the terms of the GNU
  General Public Licence or the GNU Library General Public Licence into a
  copy of this library, as this licence permits, the exception does not
  apply to the code that you add in this way.  To avoid misleading anyone as
  to the status of such modified files, you must delete this exception
  notice from such code and/or adjust the licensing conditions notice
  accordingly.

  4. If you write modifications of your own for this library, it is your
  choice whether to permit this exception to apply to your modifications.
  If you do not wish that, you must delete the exception notice from such
  code and/or adjust the licensing conditions notice accordingly.
```

#### Expat 2.8.1

License: MIT.
Upstream: <https://libexpat.github.io/>

```text
Copyright (c) 1998-2000 Thai Open Source Software Center Ltd and Clark Cooper
Copyright (c) 2001-2025 Expat maintainers

Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
"Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be included
in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

#### OpenSSL 1.0.2u (libcrypto)

License: the OpenSSL License and the original SSLeay License, both of which apply.
Upstream: <https://www.openssl.org/>

This product includes software developed by the OpenSSL Project for use in the OpenSSL Toolkit (http://www.openssl.org/). This product includes cryptographic software written by Eric Young (eay@cryptsoft.com). This product includes software written by Tim Hudson (tjh@cryptsoft.com).

```text
  LICENSE ISSUES
  ==============

  The OpenSSL toolkit stays under a double license, i.e. both the conditions of
  the OpenSSL License and the original SSLeay license apply to the toolkit.
  See below for the actual license texts. Actually both licenses are BSD-style
  Open Source licenses. In case of any license issues related to OpenSSL
  please contact openssl-core@openssl.org.

  OpenSSL License
  ---------------

/* ====================================================================
 * Copyright (c) 1998-2018 The OpenSSL Project.  All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 *
 * 1. Redistributions of source code must retain the above copyright
 *    notice, this list of conditions and the following disclaimer.
 *
 * 2. Redistributions in binary form must reproduce the above copyright
 *    notice, this list of conditions and the following disclaimer in
 *    the documentation and/or other materials provided with the
 *    distribution.
 *
 * 3. All advertising materials mentioning features or use of this
 *    software must display the following acknowledgment:
 *    "This product includes software developed by the OpenSSL Project
 *    for use in the OpenSSL Toolkit. (http://www.openssl.org/)"
 *
 * 4. The names "OpenSSL Toolkit" and "OpenSSL Project" must not be used to
 *    endorse or promote products derived from this software without
 *    prior written permission. For written permission, please contact
 *    openssl-core@openssl.org.
 *
 * 5. Products derived from this software may not be called "OpenSSL"
 *    nor may "OpenSSL" appear in their names without prior written
 *    permission of the OpenSSL Project.
 *
 * 6. Redistributions of any form whatsoever must retain the following
 *    acknowledgment:
 *    "This product includes software developed by the OpenSSL Project
 *    for use in the OpenSSL Toolkit (http://www.openssl.org/)"
 *
 * THIS SOFTWARE IS PROVIDED BY THE OpenSSL PROJECT ``AS IS'' AND ANY
 * EXPRESSED OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
 * PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE OpenSSL PROJECT OR
 * ITS CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
 * SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
 * NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
 * LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
 * HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT,
 * STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
 * ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED
 * OF THE POSSIBILITY OF SUCH DAMAGE.
 * ====================================================================
 *
 * This product includes cryptographic software written by Eric Young
 * (eay@cryptsoft.com).  This product includes software written by Tim
 * Hudson (tjh@cryptsoft.com).
 *
 */

 Original SSLeay License
 -----------------------

/* Copyright (C) 1995-1998 Eric Young (eay@cryptsoft.com)
 * All rights reserved.
 *
 * This package is an SSL implementation written
 * by Eric Young (eay@cryptsoft.com).
 * The implementation was written so as to conform with Netscapes SSL.
 *
 * This library is free for commercial and non-commercial use as long as
 * the following conditions are aheared to.  The following conditions
 * apply to all code found in this distribution, be it the RC4, RSA,
 * lhash, DES, etc., code; not just the SSL code.  The SSL documentation
 * included with this distribution is covered by the same copyright terms
 * except that the holder is Tim Hudson (tjh@cryptsoft.com).
 *
 * Copyright remains Eric Young's, and as such any Copyright notices in
 * the code are not to be removed.
 * If this package is used in a product, Eric Young should be given attribution
 * as the author of the parts of the library used.
 * This can be in the form of a textual message at program startup or
 * in documentation (online or textual) provided with the package.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 * 1. Redistributions of source code must retain the copyright
 *    notice, this list of conditions and the following disclaimer.
 * 2. Redistributions in binary form must reproduce the above copyright
 *    notice, this list of conditions and the following disclaimer in the
 *    documentation and/or other materials provided with the distribution.
 * 3. All advertising materials mentioning features or use of this software
 *    must display the following acknowledgement:
 *    "This product includes cryptographic software written by
 *     Eric Young (eay@cryptsoft.com)"
 *    The word 'cryptographic' can be left out if the rouines from the library
 *    being used are not cryptographic related :-).
 * 4. If you include any Windows specific code (or a derivative thereof) from
 *    the apps directory (application code) you must include an acknowledgement:
 *    "This product includes software written by Tim Hudson (tjh@cryptsoft.com)"
 *
 * THIS SOFTWARE IS PROVIDED BY ERIC YOUNG ``AS IS'' AND
 * ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED.  IN NO EVENT SHALL THE AUTHOR OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
 * OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
 * HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
 * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
 * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
 * SUCH DAMAGE.
 *
 * The licence and distribution terms for any publically available version or
 * derivative of this code cannot be changed.  i.e. this code cannot simply be
 * copied and put under another distribution licence
 * [including the GNU Public Licence.]
 */
```

#### ed25519, by Orson Peters

License: zlib.
Upstream: <https://github.com/orlp/ed25519>

```text
Copyright (c) 2015 Orson Peters <orsonpeters@gmail.com>

This software is provided 'as-is', without any express or implied warranty. In no event will the
authors be held liable for any damages arising from the use of this software.

Permission is granted to anyone to use this software for any purpose, including commercial
applications, and to alter it and redistribute it freely, subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not claim that you wrote the
   original software. If you use this software in a product, an acknowledgment in the product
   documentation would be appreciated but is not required.

2. Altered source versions must be plainly marked as such, and must not be misrepresented as
   being the original software.

3. This notice may not be removed or altered from any source distribution.
```

### 1.5 System SDKs

- **Microsoft Windows SDK / Win32** — used under Microsoft's standard development terms.
- **Apple Cocoa / AppKit** — used under Apple's standard development terms.

Neither imposes obligations on derivative software beyond compliance with the respective platform vendor's terms.

---

## 2. Notepad++ Plugin ABI Compatibility

The headers under `plugins/nppcompat-headers/` are an **independent clean-room reimplementation** of the Notepad++ plugin ABI. No source has been copied from Notepad++ or its plugin SDK. The ABI surface (message numbers, struct layouts, function signatures, behavior contracts) is not copyrightable; the original header source is, and is therefore not used.

These headers are licensed under the same MIT License as the rest of Code++. See `plugins/nppcompat-headers/HEADER_TEMPLATE.txt` for the provenance notice required at the top of every file in that directory.

Notepad++ itself is licensed under the GNU General Public License v2 or later. Code++ does not include any Notepad++ source code.

---

## 3. Cargo Dependency Notices

The Rust crates compiled into Code++ and its bundled plugins carry their own licenses. Their notices are in `THIRD_PARTY_CRATES.md`, which sits beside this file in every release archive. It is not checked in: the release workflow generates it from the lockfile with `cargo about`, for every platform Code++ is released for, so it always describes the crates in the release it ships with (`docs/RELEASING.md`). To produce it from a source checkout:

```sh
cargo install --locked cargo-about --version 0.9.3 --features cli
cargo about generate --fail --locked --workspace --output-file THIRD_PARTY_CRATES.md about.hbs
```

`deny.toml` at the repo root enforces that every Cargo dependency uses a license from the allowlist (MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, 0BSD, CC0-1.0, Unicode-DFS-2016, Unicode-3.0). A dependency outside this set fails CI and must be removed or replaced before merge. `about.toml` accepts the same set, so a release cannot be generated with a license outside it either.
