#!/usr/bin/env python3
"""Check Code++'s NPPM_*/NPPN_* message numbers against Notepad++'s.

Code++ declares the Notepad++ plugin ABI in two places — the Rust
dispatcher and the C header plugin authors include — and both were
written by hand. Nothing connected either to its source, so six
numbers had drifted: three sat on *other* real upstream messages, two
were invented offsets in the RUNCOMMAND family, and one was a Code++
message with no upstream counterpart squatting on a real number. A
plugin hitting any of them gets a different message than it sent.

This fetches upstream's published header and diffs the numbers. It
deliberately does NOT vendor that header into the repo: CLAUDE.md's
"No code from Notepad++" rule means the tree carries no upstream
source, and the ABI facts this prints (a name and an integer) are not
copyrightable, which is the same basis the clean-room headers rest on.

    python tools/npp-abi-check/check.py           # fetch and compare
    python tools/npp-abi-check/check.py --header <path>

Exit status is 1 when a name Code++ declares has a different number
upstream — the case that breaks binary compatibility. Names upstream
does not define at all are reported but do not fail, because some are
messages Notepad++ has since renamed while keeping the number (our
older spelling is still correct) and one is a deliberate Code++
extension; read the output rather than trusting the exit code alone
for those.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
import urllib.request

UPSTREAM_URL = (
    "https://raw.githubusercontent.com/notepad-plus-plus/notepad-plus-plus/"
    "master/PowerEditor/src/MISC/PluginsManager/Notepad_plus_msgs.h"
)

REPO = pathlib.Path(__file__).resolve().parents[2]
RUST = REPO / "crates" / "plugin-host" / "src" / "dispatch.rs"
HEADER = REPO / "plugins" / "nppcompat-headers" / "Notepad_plus_msgs.h"

# Upstream renamed these and kept the number; Code++ still spells them
# the old way, which is correct for a plugin compiled against either
# header. Listed so the report separates "renamed" from "unknown".
KNOWN_RENAMES = {
    "NPPM_ADDTOOLBARICON": "NPPM_ADDTOOLBARICON_DEPRECATED",
    "NPPM_ALLOCATESUPPORTED": "NPPM_ALLOCATESUPPORTED_DEPRECATED",
    "NPPM_DESTROYSCINTILLAHANDLE": "NPPM_DESTROYSCINTILLAHANDLE_DEPRECATED",
    "NPPM_DOCSWITCHERDISABLECOLUMN": "NPPM_DOCLISTDISABLEEXTCOLUMN",
    "NPPM_GETENABLETHEMETEXTUREFUNC": "NPPM_GETENABLETHEMETEXTUREFUNC_DEPRECATED",
    "NPPM_GETOPENFILENAMES": "NPPM_GETOPENFILENAMES_DEPRECATED",
    "NPPM_GETOPENFILENAMESPRIMARY": "NPPM_GETOPENFILENAMESPRIMARY_DEPRECATED",
    "NPPM_GETOPENFILENAMESSECOND": "NPPM_GETOPENFILENAMESSECOND_DEPRECATED",
    "NPPM_GETSETTINGSCLOUDPATH": "NPPM_GETSETTINGSONCLOUDPATH",
    "NPPM_ISDOCSWITCHERSHOWN": "NPPM_ISDOCLISTSHOWN",
    "NPPM_SHOWDOCSWITCHER": "NPPM_SHOWDOCLIST",
}


def integer_defines(text: str) -> dict[str, int]:
    """`#define NAME 7` — needed to resolve symbolic offsets such as
    `RUNCOMMAND_USER + NPP_DIRECTORY`."""
    return {
        m.group(1): int(m.group(2))
        for m in re.finditer(r"#define\s+([A-Z_][A-Z0-9_]*)\s+(\d+)\s*$", text, re.M)
    }


def parse(text: str, pattern: str, ints: dict[str, int]) -> dict[str, tuple[str, int]]:
    out: dict[str, tuple[str, int]] = {}
    for m in re.finditer(pattern, text, re.M):
        name, base, off = m.group(1), m.group(2), m.group(3)
        value = int(off) if off.isdigit() else ints.get(off)
        if value is not None:
            out[name] = (base, value)
    return out


def load_upstream(path: str | None) -> str:
    """Read the upstream header, or exit with a readable message.

    A maintenance tool that answers a network hiccup with a traceback
    reads as "the check is broken" rather than "run it again".
    """
    if path:
        try:
            return pathlib.Path(path).read_text(encoding="utf-8", errors="replace")
        except OSError as err:
            sys.exit(f"cannot read {path}: {err}")
    try:
        with urllib.request.urlopen(UPSTREAM_URL, timeout=60) as resp:  # noqa: S310
            return resp.read().decode("utf-8", errors="replace")
    except OSError as err:
        sys.exit(
            f"cannot fetch {UPSTREAM_URL}: {err}\n"
            "Offline? Download it once and pass --header <path>."
        )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--header",
        help="compare against a local copy instead of fetching (offline runs)",
    )
    args = ap.parse_args()

    up_text = load_upstream(args.header)
    ints = integer_defines(up_text)
    define_re = r"#define\s+(NPPM_\w+|NPPN_\w+)\s*\(\s*(\w+)\s*\+\s*(\w+)\s*\)"
    upstream = parse(up_text, define_re, ints)
    if not upstream:
        print("could not parse any message from the upstream header", file=sys.stderr)
        return 2

    sources = {
        "crates/plugin-host/src/dispatch.rs": parse(
            RUST.read_text(encoding="utf-8", errors="replace"),
            r"pub const (NPPM_\w+|NPPN_\w+): u32 = (\w+) \+ (\w+);",
            ints,
        ),
        "plugins/nppcompat-headers/Notepad_plus_msgs.h": parse(
            HEADER.read_text(encoding="utf-8", errors="replace"), define_re, ints
        ),
    }

    by_number: dict[tuple[str, int], list[str]] = {}
    for name, value in upstream.items():
        by_number.setdefault(value, []).append(name)

    failed = False
    for label, ours in sources.items():
        wrong, unknown = [], []
        for name, value in sorted(ours.items()):
            if name in upstream:
                if upstream[name] != value:
                    wrong.append((name, value, upstream[name]))
            else:
                unknown.append((name, value))

        print(f"\n=== {label} ({len(ours)} messages) ===")
        if wrong:
            failed = True
            print(f"  MISMATCHED ({len(wrong)}) — these break binary compatibility:")
            for name, ourv, upv in wrong:
                squat = [n for n in by_number.get(ourv, []) if n != name]
                extra = f"  [our number is upstream's {', '.join(squat)}]" if squat else ""
                print(f"    {name}: ours {ourv[0]}+{ourv[1]}, upstream {upv[0]}+{upv[1]}{extra}")
        else:
            print("  MISMATCHED: none")

        renamed = [(n, v) for n, v in unknown if n in KNOWN_RENAMES]
        other = [(n, v) for n, v in unknown if n not in KNOWN_RENAMES]
        if renamed:
            print(f"  renamed upstream, same number ({len(renamed)}) — fine:")
            for name, value in renamed:
                print(f"    {name} ({value[0]}+{value[1]}) is now {KNOWN_RENAMES[name]}")
        if other:
            print(f"  NOT DEFINED UPSTREAM ({len(other)}) — check each:")
            for name, value in other:
                squat = by_number.get(value, [])
                where = (
                    f"that number is upstream's {', '.join(squat)}"
                    if squat
                    else "number unused upstream"
                )
                print(f"    {name}: {value[0]}+{value[1]} — {where}")

    print()
    if failed:
        print("FAIL: at least one message number disagrees with upstream.")
    else:
        print("OK: every message Code++ and Notepad++ both name has the same number.")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
