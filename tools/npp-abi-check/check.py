#!/usr/bin/env python3
"""Check Code++'s plugin-ABI numbers against Notepad++'s.

Two families: the NPPM_*/NPPN_* message numbers, and the docking
constants — DMN_* notification codes, DWS_* style flags and the CONT_*
container numbers.

Code++ declares the Notepad++ plugin ABI in two places — the Rust
dispatcher and the C header plugin authors include — and both were
written by hand. Nothing connected either to its source, so six
numbers had drifted: three sat on *other* real upstream messages, two
were invented offsets in the RUNCOMMAND family, and one was a Code++
message with no upstream counterpart squatting on a real number. A
plugin hitting any of them gets a different message than it sent.

The docking family drifted the same way and for the same reason:
DMN_FIRST was 0x1000 where upstream's is 1050, so every DMN_* the host
ever sent was a number no plugin recognised, and DWS_USEOWNDARKMODE was
a bit upstream does not use. Nothing compared those at all until the
second pass below, and a probe plugin loaded into both hosts is what
found them.

This fetches upstream's published headers and diffs the numbers. It
deliberately does NOT vendor those headers into the repo: CLAUDE.md's
"No code from Notepad++" rule means the tree carries no upstream
source, and the ABI facts this prints (a name and an integer) are not
copyrightable, which is the same basis the clean-room headers rest on.

    python tools/npp-abi-check/check.py           # fetch and compare
    python tools/npp-abi-check/check.py --header <path> \
        --docking-header <path> --docking-resource <path>   # offline

Exit status is 1 when a name Code++ declares has a different value
upstream, in either family — the case that breaks binary compatibility. Names upstream
does not define at all are reported but do not fail, because some are
messages Notepad++ has since renamed while keeping the number (our
older spelling is still correct) and one is a deliberate Code++
extension; read the output rather than trusting the exit code alone
for those.
"""

from __future__ import annotations

import argparse
import ast
import pathlib
import re
import sys
import urllib.request

UPSTREAM_BASE = (
    "https://raw.githubusercontent.com/notepad-plus-plus/notepad-plus-plus/master/PowerEditor/src/"
)
UPSTREAM_URL = UPSTREAM_BASE + "MISC/PluginsManager/Notepad_plus_msgs.h"
# CONT_*, DOCKCONT_MAX and DWS_* live in Docking.h; DMN_* in its
# sibling dockingResource.h, which is why a check of Docking.h alone
# would not have seen the DMN_FIRST drift.
DOCKING_URL = UPSTREAM_BASE + "WinControls/DockingWnd/Docking.h"
DOCKING_RESOURCE_URL = UPSTREAM_BASE + "WinControls/DockingWnd/dockingResource.h"

REPO = pathlib.Path(__file__).resolve().parents[2]
RUST = REPO / "crates" / "plugin-host" / "src" / "dispatch.rs"
HEADER = REPO / "plugins" / "nppcompat-headers" / "Notepad_plus_msgs.h"
DOCKING_SOURCES = {
    "plugins/nppcompat-headers/Docking.h": ("c", REPO / "plugins" / "nppcompat-headers" / "Docking.h"),
    "crates/plugin-host/src/ffi.rs": ("rust", REPO / "crates" / "plugin-host" / "src" / "ffi.rs"),
    "crates/plugin-sdk/src/lib.rs": ("rust", REPO / "crates" / "plugin-sdk" / "src" / "lib.rs"),
}
DOCKING_NAME = r"(?:DMN|DWS|CONT|DOCKCONT)_\w+"

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


def load_upstream(path: str | None, url: str = UPSTREAM_URL, flag: str = "--header") -> str:
    """Read an upstream header, or exit with a readable message.

    A maintenance tool that answers a network hiccup with a traceback
    reads as "the check is broken" rather than "run it again".
    """
    if path:
        try:
            return pathlib.Path(path).read_text(encoding="utf-8", errors="replace")
        except OSError as err:
            sys.exit(f"cannot read {path}: {err}")
    try:
        with urllib.request.urlopen(url, timeout=60) as resp:  # noqa: S310
            return resp.read().decode("utf-8", errors="replace")
    except OSError as err:
        sys.exit(f"cannot fetch {url}: {err}\nOffline? Download it once and pass {flag} <path>.")


def evaluate(expr: str, env: dict[str, int]) -> int:
    """Evaluate a constant expression from a header or a Rust `const`.

    Whitelisted by AST node rather than handed to `eval`, because the
    upstream half of the input is text fetched from the internet: only
    integer literals (Python reads hex and `_` separators natively),
    names already resolved in the same file, and `+`, `<<`, `|` —
    which is every operator the docking headers use. Anything else
    raises, and the caller leaves that name unresolved.
    """

    def ev(node: ast.AST) -> int:
        if isinstance(node, ast.Expression):
            return ev(node.body)
        if isinstance(node, ast.Constant) and type(node.value) is int:
            return node.value
        if isinstance(node, ast.Name) and node.id in env:
            return env[node.id]
        if isinstance(node, ast.BinOp):
            left, right = ev(node.left), ev(node.right)
            if isinstance(node.op, ast.Add):
                return left + right
            if isinstance(node.op, ast.LShift) and 0 <= right < 64:
                return left << right
            if isinstance(node.op, ast.BitOr):
                return left | right
        raise ValueError(f"unsupported expression: {expr!r}")

    return ev(ast.parse(expr.strip(), mode="eval"))


def docking_constants(text: str, lang: str) -> tuple[dict[str, int], list[str]]:
    """Every DMN_/DWS_/CONT_/DOCKCONT_ constant a source defines, as an
    integer, plus the names it defines but could not evaluate.

    Resolved to a fixpoint so a definition may refer to one that appears
    later in the file. The unresolved names are returned rather than
    dropped because a dropped name is a constant the check silently
    stops covering — a parsing regression would shrink the report and
    still end in "OK"."""
    if lang == "c":
        text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
        pattern = rf"^\s*#define\s+({DOCKING_NAME})\s+([^\n]+?)\s*(?://.*)?$"
    else:
        pattern = rf"^\s*pub const ({DOCKING_NAME}): u32 = ([^;]+);"
    raw = {m.group(1): m.group(2) for m in re.finditer(pattern, text, re.M)}
    out: dict[str, int] = {}
    progress = True
    while progress:
        progress = False
        for name, expr in raw.items():
            if name in out:
                continue
            try:
                out[name] = evaluate(expr, out)
                progress = True
            except (ValueError, SyntaxError, RecursionError):
                continue
    return out, sorted(set(raw) - set(out))


def check_docking(upstream_text: str) -> bool:
    """The docking family. Returns True when anything disagrees."""
    upstream, upstream_unparsed = docking_constants(upstream_text, "c")
    if "DMN_FIRST" not in upstream:
        print("could not parse the upstream docking headers", file=sys.stderr)
        return True
    failed = False
    if upstream_unparsed:
        # Upstream's own definitions this evaluator cannot read are not
        # compared; a name of ours that shares one is reported as "not
        # defined upstream" below, which is the signal to extend it.
        print(f"\nwarning: could not evaluate upstream {', '.join(upstream_unparsed)}")
    for label, (lang, path) in DOCKING_SOURCES.items():
        ours, unparsed = docking_constants(path.read_text(encoding="utf-8", errors="replace"), lang)
        wrong = [(n, v, upstream[n]) for n, v in sorted(ours.items()) if n in upstream and upstream[n] != v]
        unknown = sorted(n for n in ours if n not in upstream)
        print(f"\n=== {label} ({len(ours)} docking constants) ===")
        if wrong:
            failed = True
            print(f"  MISMATCHED ({len(wrong)}) — these break binary compatibility:")
            for name, ourv, upv in wrong:
                print(f"    {name}: ours {ourv} (0x{ourv:x}), upstream {upv} (0x{upv:x})")
        else:
            print("  MISMATCHED: none")
        if unknown:
            print(f"  NOT DEFINED UPSTREAM ({len(unknown)}) — check each: {', '.join(unknown)}")
        if unparsed:
            # One of ours the evaluator cannot read is a constant this
            # check is not covering — fail rather than report "OK" for it.
            failed = True
            print(f"  COULD NOT EVALUATE ({len(unparsed)}) — not checked: {', '.join(unparsed)}")
    return failed


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--header",
        help="compare against a local copy instead of fetching (offline runs)",
    )
    ap.add_argument("--docking-header", help="local copy of upstream's Docking.h")
    ap.add_argument("--docking-resource", help="local copy of upstream's dockingResource.h")
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

    docking_text = load_upstream(
        args.docking_header, DOCKING_URL, "--docking-header"
    ) + "\n" + load_upstream(args.docking_resource, DOCKING_RESOURCE_URL, "--docking-resource")
    docking_failed = check_docking(docking_text)

    print()
    if failed or docking_failed:
        print("FAIL: at least one number disagrees with upstream.")
    else:
        print("OK: every message and docking constant Code++ and Notepad++ both name agrees.")
    return 1 if (failed or docking_failed) else 0


if __name__ == "__main__":
    sys.exit(main())
