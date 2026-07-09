# Preinstalled UDLs — provenance

This directory bundles third-party User Defined Language (UDL) XML files
Code++ ships with its Windows installer / release archive so a fresh install
has Markdown highlighting available without the user having to hand-install
anything. Every file here is copied verbatim from an upstream source with an
explicit MIT (or MIT-compatible) license; this file records where each one
came from, when it was copied, and what its license is. Files here are
**never** modified in-tree — bug reports and enhancement requests belong
upstream. Re-sync is a manual copy job (this directory is not a submodule;
the file count is small and changes are rare, per DESIGN.md §7.2 Phase 4.6
m1 rationale).

## markdown._preinstalled.udl.xml

- **Upstream:** <https://github.com/Edditoria/markdown-plus-plus>
- **Upstream path:** `udl/markdown._preinstalled.udl.xml`
- **License:** MIT (see [`LICENSE.markdown-plus-plus.txt`](LICENSE.markdown-plus-plus.txt))
- **Copyright:** Copyright (c) 2012 - present Edditoria
- **Copied on:** 2026-07-09
- **Purpose:** Preinstalled UDL for `.md` / `.markdown` files. Loaded at
  startup by `crates/udl` alongside any user-authored UDLs in
  `<config_dir>/userDefineLangs/`. Appears in the Language menu under
  "User-Defined language" as "Markdown (preinstalled)".

## Notes for a future contributor

- Adding another preinstalled UDL: drop its XML file next to this file,
  add a stanza above documenting the upstream + license + copy date, and
  make sure the license is on the allow-list (MIT / Apache-2.0 /
  BSD-2/3-Clause / ISC / Zlib / 0BSD / CC0-1.0 / Unicode variants — same
  set `deny.toml` uses for Cargo dependencies).
- **Copyleft licenses (GPL / LGPL / MPL / AGPL) are denied by omission.**
  A UDL shipping under GPL cannot be bundled here — a user who wants it
  can drop the XML into their own `<config_dir>/userDefineLangs/` directly;
  Code++'s runtime loads any UDL file in that directory regardless of its
  provenance.
- If upstream re-licenses, delete the file from this directory in the same
  commit that fixes any downstream code that referenced it — do NOT keep
  a copy under a license Code++'s policy denies.
