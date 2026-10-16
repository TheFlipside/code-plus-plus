#!/usr/bin/env python3
"""Check Code++'s plugin-ABI numbers against Notepad++'s.

Three families: the NPPM_*/NPPN_* message numbers; the docking
constants — DMN_* notification codes, DWS_* style flags and the CONT_*
container numbers; and the IDM_* menu command ids a plugin passes to
NPPM_MENUCOMMAND.

Code++ declares the Notepad++ plugin ABI by hand, in the Rust
dispatcher, the C header plugin authors include and the plugin SDK, and
numbers drifted in every family until this compared them: a plugin
sending a wrong one gets a different message than it sent.

This fetches upstream's published headers and diffs the numbers. It
deliberately does NOT vendor those headers into the repo: CLAUDE.md's
"No code from Notepad++" rule means the tree carries no upstream
source, and the ABI facts this prints (a name and an integer) are not
copyrightable, which is the same basis the clean-room headers rest on.

    python tools/npp-abi-check/check.py           # fetch and compare
    python tools/npp-abi-check/check.py --header <path> \\
        --docking-header <path> --docking-resource <path> \\
        --menu-header <path>                                # offline

Run it by its path, as above: it imports `abi_sources`, beside it, which
reads the sources as their compilers do, and `python -I` or `-P` leaves
that folder off the import path.

Exit status is 1 when a number or a base Code++ declares (NPPMSG,
RUNCOMMAND_USER, NPPN_FIRST, and a WM_USER of ours) differs from
upstream's or cannot be read on either side, a WM_USER being upstream's
own if its header defines one and Windows' 0x0400 if not; for a name of
ours that upstream does not define, or defines twice or in a form this
cannot evaluate, unless upstream renamed it keeping its number
(KNOWN_RENAMES, listed as renamed while the new name carries our
number); for one of ours declared in a form this cannot read, or
defined twice; and when a source of ours, or an upstream docking or
menu header, holds none of its family's names. So a header cut short
before one of our names fails. It is 1 too, with a message naming the
header, for one that cannot be read, is over 1 MiB, or holds what
`abi_sources.preprocessed` refuses (a comment or a literal never
closed, a `%:` directive, or what compilers read differently), or, for
upstream's, a conditional directive; and for a download that fails, is
cut short or runs out of time. An upstream messages header with no
message in it ends the run with status 2, as argparse does a command
line it refuses.

Both files pass black, isort, flake8, pylint and `mypy --strict` at
their default settings; keep every line within flake8's 79 columns.
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import http.client
import io
import os
import pathlib
import re
import sys
import threading
import time
import urllib.request
from typing import BinaryIO

import abi_sources

UPSTREAM_BASE = (
    "https://raw.githubusercontent.com/notepad-plus-plus/"
    "notepad-plus-plus/master/PowerEditor/src/"
)
# CONT_*, DOCKCONT_MAX and DWS_* live in Docking.h; DMN_* in its
# sibling dockingResource.h, which is why a check of Docking.h alone
# would not have seen the DMN_FIRST drift.
DOCKING_DIR = UPSTREAM_BASE + "WinControls/DockingWnd/"
# Each upstream header, by the flag that names a local copy of it for
# an offline run.
UPSTREAM_HEADERS = {
    "--header": UPSTREAM_BASE + "MISC/PluginsManager/Notepad_plus_msgs.h",
    "--docking-header": DOCKING_DIR + "Docking.h",
    "--docking-resource": DOCKING_DIR + "dockingResource.h",
    "--menu-header": UPSTREAM_BASE + "menuCmdID.h",
}

# The largest header the check reads. Upstream's are under 70 KB; the
# bound keeps a corrupt or hostile download from exhausting memory,
# since the fetched half of the input is not ours. Every scan is linear
# in the text, and only an expression `abi_sources.CONSTANT_EXPR` admits
# is parsed, so the bound also bounds the time.
MAX_HEADER_BYTES = 1 << 20
# The longest the run may take to fetch upstream's headers, counted from
# its start, so the time the passes spend between downloads counts too.
# The socket timeout bounds each wait, so a server that keeps sending,
# however slowly, never trips it, and a deadline per download would let
# four of them take four times as long.
FETCH_SECONDS = 120
# How long the run may take to end once the deadline has fired. Ending
# it means writing the report so far and a message, and a write to a
# full pipe nobody reads never returns, so the exit cannot wait on one.
GIVE_UP_SECONDS = 2.0
# How much of an error's text a failure to fetch shows. The text is
# partly the server's, and a reason phrase can run to megabytes.
MAX_REASON_CHARS = 300
# The most names read from upstream a list in the report shows, and the
# most characters of each: a fetched header may define any number of
# names of any length, and a list can be printed once per name of ours.
MAX_LISTED = 32
MAX_NAME_CHARS = 64

REPO = pathlib.Path(__file__).resolve().parents[2]
HEADERS = REPO / "plugins" / "nppcompat-headers"
PLUGIN_HOST = REPO / "crates" / "plugin-host" / "src"
RUST = PLUGIN_HOST / "dispatch.rs"
HEADER = HEADERS / "Notepad_plus_msgs.h"
SDK = REPO / "crates" / "plugin-sdk" / "src" / "lib.rs"

# Whitespace within a line, and the start of a `#define` directive.
BLANK = abi_sources.BLANK
DEFINE = abi_sources.DEFINE
# A message is a base plus an offset, either of which may be symbolic:
# `#define NPPM_X (NPPMSG + 7)` in a header, and its Rust twin
# `pub const NPPM_X: u32 = NPPMSG + 7;`. A directive is read only when
# its line holds nothing else, as one more term would change its value.
MESSAGE_NAME = r"(NPPM_\w+|NPPN_\w+)"
MESSAGE_DEFINE = (
    rf"^{DEFINE}{MESSAGE_NAME}{BLANK}+\({BLANK}*(\w+){BLANK}*\+"
    rf"{BLANK}*(\w+){BLANK}*\){BLANK}*$"
)
MESSAGE_CONST = rf"pub const {MESSAGE_NAME}: u32 = (\w+) \+ (\w+);"
# The sources of ours that declare the messages, each with the pattern
# its messages are read with and its language. The plugin SDK repeats
# a few of the dispatcher's for the plugins built on it.
MESSAGE_SOURCES = (
    (RUST, MESSAGE_CONST, "rust"),
    (HEADER, MESSAGE_DEFINE, "c"),
    (SDK, MESSAGE_CONST, "rust"),
)
# `WM_USER`, which NPPMSG and RUNCOMMAND_USER count from: it is Windows',
# from the `<windows.h>` upstream's header and ours both include. A
# source of ours that defines its own is checked against it.
WM_USER = 0x0400
# Messages by name, each as its base and its offset.
Messages = dict[str, tuple[str, int]]

# Upstream renamed these and kept the number; Code++ still spells them
# the old way, which is correct for a plugin compiled against either
# header. Listed so the report separates "renamed" from "unknown"; a
# rename passes only while upstream's new name carries our number.
# These upstream only marked deprecated, by appending `_DEPRECATED`:
DEPRECATED_UPSTREAM = (
    "NPPM_ADDTOOLBARICON",
    "NPPM_ALLOCATESUPPORTED",
    "NPPM_DESTROYSCINTILLAHANDLE",
    "NPPM_GETENABLETHEMETEXTUREFUNC",
    "NPPM_GETOPENFILENAMES",
    "NPPM_GETOPENFILENAMESPRIMARY",
    "NPPM_GETOPENFILENAMESSECOND",
)
KNOWN_RENAMES = {name: f"{name}_DEPRECATED" for name in DEPRECATED_UPSTREAM}
KNOWN_RENAMES |= {
    "NPPM_DOCSWITCHERDISABLECOLUMN": "NPPM_DOCLISTDISABLEEXTCOLUMN",
    "NPPM_GETSETTINGSCLOUDPATH": "NPPM_GETSETTINGSONCLOUDPATH",
    "NPPM_ISDOCSWITCHERSHOWN": "NPPM_ISDOCLISTSHOWN",
    "NPPM_SHOWDOCSWITCHER": "NPPM_SHOWDOCLIST",
}

# Heads the list of numbers that disagree, in every pass.
MISMATCHED = "these break binary compatibility:"
# Reported, as a failure, for a file in which a pass finds nothing: its
# pattern no longer matches the file, which would otherwise read as "no
# mismatch".
NONE_FOUND = "  NONE FOUND — the pattern no longer matches this file"


@dataclasses.dataclass(frozen=True)
class Family:
    """A family of constants that header and Rust define alike, each
    name either an integer or an expression over the family's own
    names: the docking constants and the menu ids."""

    # What the report calls the family.
    what: str
    # A name upstream must define, or its parse failed.
    anchor: str
    # A regular expression matching every name in the family.
    name_re: str
    # The type the family's Rust constants are declared with.
    rust_type: str
    # Every source of ours that declares the family, as a language
    # ("c" or "rust") and a path.
    sources: tuple[tuple[str, pathlib.Path], ...]


DOCKING = Family(
    what="docking constants",
    anchor="DMN_FIRST",
    name_re=r"(?:DMN|DWS|CONT|DOCKCONT)_\w+",
    rust_type="u32",
    sources=(
        ("c", HEADERS / "Docking.h"),
        ("rust", PLUGIN_HOST / "ffi.rs"),
        ("rust", SDK),
    ),
)
MENU = Family(
    what="menu ids",
    anchor="IDM_FILE_NEW",
    # `IDM` alone is the base most count from; a few count from one of
    # upstream's `IDR_*` menu resource ids instead.
    name_re=r"ID[MR](?:_\w+)?",
    rust_type="i32",
    sources=(
        ("c", HEADERS / "menuCmdID.h"),
        ("rust", PLUGIN_HOST / "idm.rs"),
    ),
)


def label_of(path: pathlib.Path) -> str:
    """A source's name in the report: its path within the repository."""
    return path.relative_to(REPO).as_posix()


def short(word: str) -> str:
    """`word` cut to `MAX_NAME_CHARS` for the report: upstream's words
    may be of any length, and each is printed more than once."""
    if len(word) <= MAX_NAME_CHARS:
        return word
    return f"{word[:MAX_NAME_CHARS]}..."


def listed(names: list[str]) -> str:
    """Names read from upstream, as the report lists them: each cut by
    `short`, and at most `MAX_LISTED`, then a count of the rest."""
    shown = ", ".join(short(name) for name in names[:MAX_LISTED])
    rest = len(names) - MAX_LISTED
    return f"{shown} and {rest} more" if rest > 0 else shown


def read_c(text: str, label: str, upstream: bool) -> str:
    """`text`, the C header `label` names, as `abi_sources.preprocessed`
    reads it, and refused too, for upstream's, when it holds a
    conditional directive: ours hold include guards and platform checks,
    and upstream's none. Exits naming the header for what is refused,
    since a definition it could hide would otherwise go unchecked."""
    try:
        out = abi_sources.preprocessed(text)
        return abi_sources.unconditional(out) if upstream else out
    except ValueError as err:
        sys.exit(f"{label}: {err}")


def parse(text: str, pattern: str, ints: dict[str, int]) -> Messages:
    """Every message `pattern` finds in `text`. A symbolic offset is
    looked up in `ints`, and a message whose offset is neither a number
    nor found there is left out."""
    out: Messages = {}
    for m in re.finditer(pattern, text, re.M):
        name, base, off = m.group(1), m.group(2), m.group(3)
        value = abi_sources.number(off)
        if value is None:
            value = ints.get(off)
        if value is not None:
            out[name] = (base, value)
    return out


def read_capped(stream: BinaryIO, source: str) -> str:
    """The text of `stream`, which `source` names in an error. Exits
    when it holds more than `MAX_HEADER_BYTES`, rather than reading
    on."""
    data = stream.read(MAX_HEADER_BYTES + 1)
    if len(data) > MAX_HEADER_BYTES:
        sys.exit(f"{source}: over {MAX_HEADER_BYTES} bytes, not a header")
    return data.decode("utf-8-sig", errors="replace")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Fetch the URL asked for, or fail. To follow a redirect, urllib
    reads the redirect's body however large it is, and may follow it to
    an `ftp://` or a plain `http://` address. Upstream's headers are
    served without one."""

    def redirect_request(self, *_args: object, **_kwargs: object) -> None:
        """Follow no redirect: the answer stands as an HTTP error."""
        return None


def flush_quietly() -> None:
    """Flush the report so far, swallowing a failure: on a closed or
    broken stdout there is nothing left to say it on."""
    with contextlib.suppress(OSError, ValueError, AttributeError):
        sys.stdout.flush()


def give_up(url: str, flag: str) -> None:
    """End the run when the downloads have taken `FETCH_SECONDS`, `url`
    the one under way, from the timer's thread. The report so far is
    flushed first and the message written, and the exit happens whatever
    becomes of either: a write that fails (a closed stream, a broken
    pipe, a full disk) would end this thread and leave the download
    running, and one that blocks on a full pipe would leave it running
    for good. So the flush runs on a thread of its own, waited for half
    of `GIVE_UP_SECONDS`, and a second timer ends the run regardless when
    `GIVE_UP_SECONDS` have passed since the deadline, both made inside
    the `try`: a process that cannot start them ends at the deadline
    without the report, and without the message too when it is the
    second timer that cannot start."""
    try:
        backstop = threading.Timer(GIVE_UP_SECONDS, os._exit, (1,))
        backstop.daemon = True
        backstop.start()
        try:
            flush = threading.Thread(target=flush_quietly, daemon=True)
            flush.start()
            flush.join(GIVE_UP_SECONDS / 2)
        finally:
            sys.stderr.write(
                f"cannot fetch {url}: "
                f"the downloads took more than {FETCH_SECONDS} s\n"
                f"Offline? Download it once and pass {flag} <path>.\n"
            )
            sys.stderr.flush()
    finally:
        os._exit(1)


def load_upstream(path: str | None, flag: str, deadline_at: float) -> str:
    """Read the upstream header `flag` names: the local copy at `path`
    when the command line gives one, an empty path included, otherwise
    fetched, with a readable message and an exit when it cannot, since a
    traceback for a network hiccup reads as "the check is broken". A
    fetch is one request, answered `200 OK` without a redirect and framed
    by a `Content-Length`, as upstream's are, and it must end by
    `deadline_at`, a `time.monotonic()` time the run's fetches share."""
    if path is not None:
        try:
            with open(path, "rb") as local:
                return read_capped(local, path)
        except OSError as err:
            sys.exit(f"cannot read {path!r}: {err}")
    url = UPSTREAM_HEADERS[flag]
    left = max(0.0, deadline_at - time.monotonic())
    deadline = threading.Timer(left, give_up, (url, flag))
    deadline.daemon = True
    deadline.start()
    try:
        opener = urllib.request.build_opener(NoRedirect)
        with opener.open(url, timeout=60) as resp:
            # A 206 is part of a file, and any other 2xx no file at all,
            # either of which would otherwise be read as the header.
            if resp.status != 200:
                raise http.client.HTTPException(
                    f"answered {resp.status}, not 200 with the whole file"
                )
            # Only a Content-Length bounds what http.client reads: it
            # reads a chunked body or one framed by the close without a
            # bound (a chunk size of -1 reads to the end of the stream),
            # and one cut short looks like one that is whole.
            if resp.length is None:
                raise http.client.HTTPException(
                    "no Content-Length, so a cut-off header could not be told"
                )
            text = read_capped(resp, url)
            # A bounded read returns what arrived when the connection
            # drops, where an unbounded one raises; bytes the server
            # declared and did not send mean the header was cut short.
            if resp.length:
                raise http.client.IncompleteRead(b"", resp.length)
    except (OSError, http.client.HTTPException, ValueError) as err:
        # A ValueError is a redirect's Location urllib cannot read: one
        # that is malformed, or holds what a request line cannot carry
        # (UnicodeEncodeError is a ValueError). An error's text is partly
        # the server's, that Location included, so it is escaped, since
        # a control character in it would otherwise reach the terminal,
        # and cut short.
        reason = str(err).encode("unicode_escape").decode("ascii")
        sys.exit(
            f"cannot fetch {url}: {reason[:MAX_REASON_CHARS]}\n"
            f"Offline? Download it once and pass {flag} <path>."
        )
    finally:
        deadline.cancel()
    return text


def defined_constants(
    text: str, lang: str, family: Family
) -> tuple[dict[str, int], list[str]]:
    """Every constant of `family` a source defines, as an integer, plus
    the names it defines but could not evaluate: a C header's `#define`s,
    read from its text as `read_c` returns it, or a Rust file's `pub
    const`s, read with its comments stripped. A name defined twice is
    among those, and so, in C, is one whose value holds a number Python
    reads and C does not (`PYTHON_ONLY_NUMBER`), and one that uses a
    macro whose body has an operator outside its parentheses
    (`abi_sources.bare_operator_users`). They are returned rather than
    dropped, since a dropped name is a constant the check would stop
    covering while the report says "OK"."""
    raw: dict[str, str] = {}
    refused: set[str] = set()
    if lang == "c":
        # A line at a time, so no pattern can run across lines: one
        # that could took quadratic time on a hostile download.
        refused = abi_sources.defined_twice(text)
        define = re.compile(rf"{DEFINE}({family.name_re}){BLANK}+(.*)")
        found = [m for m in map(define.match, text.split("\n")) if m]
    else:
        const = rf"^{BLANK}*pub const ({family.name_re}): {family.rust_type}"
        found = list(re.finditer(const + r" = ([^;]+);", text, re.M))
    for m in found:
        name, value = m.group(1), m.group(2).strip()
        # The last definition is the one in force, and one this cannot
        # read must not leave an earlier one standing.
        if name in raw:
            refused.add(name)
        if lang == "c" and abi_sources.PYTHON_ONLY_NUMBER.search(value):
            refused.add(name)
        raw[name] = value
    if lang == "c":
        # C substitutes a macro's body as text, which the evaluator does
        # not: a body with an operator outside its parentheses can mean
        # something else where another macro uses it.
        refused |= abi_sources.bare_operator_users(raw)
    out = abi_sources.resolve(
        {name: v for name, v in raw.items() if name not in refused}
    )
    return out, sorted(set(raw) - set(out))


def declared_names(text: str, lang: str, name_re: str) -> set[str]:
    """Every name matching `name_re` a source declares, in any form: a C
    `#define`, or a Rust `const` or `static` of any type and visibility,
    in comment-free `text` (see `defined_constants`). One its pass did
    not read is declared in a form the pass does not read, which the
    report fails on, since the constant would otherwise go unchecked
    without a word."""
    if lang == "c":
        pattern = rf"^{DEFINE}({name_re})\b"
    else:
        pattern = rf"\b(?:const|static)\s+({name_re})\s*:"
    return {m.group(1) for m in re.finditer(pattern, text, re.M)}


def report_not_read(declared: set[str], read: set[str]) -> bool:
    """Print the names a source declares that its pass did not read.
    Returns True when there is any."""
    skipped = sorted(declared - read)
    if skipped:
        names = ", ".join(skipped)
        print(f"  NOT READ ({len(skipped)}) — in a form not read: {names}")
    return bool(skipped)


def show_constant(value: int) -> str:
    """A constant in decimal and in hex: `1050 (0x41a)`."""
    return f"{value} (0x{value:x})"


def report_constants(
    ours: dict[str, int],
    unparsed: list[str],
    upstream: dict[str, int],
    upstream_unread: list[str],
) -> bool:
    """Print how one source's constants compare with upstream's, given
    the names upstream defines but the evaluator could not read. Returns
    True when anything disagrees, is not upstream's or goes unchecked,
    or when the source defines none."""
    if not ours and not unparsed:
        print(NONE_FOUND)
        return True
    wrong = [
        (name, value, upstream[name])
        for name, value in sorted(ours.items())
        if name in upstream and upstream[name] != value
    ]
    missing = sorted(name for name in ours if name not in upstream)
    # Upstream defines these, in a form the evaluator cannot read, so
    # ours were never compared with anything.
    unreadable = [name for name in missing if name in upstream_unread]
    unknown = [name for name in missing if name not in upstream_unread]
    if wrong:
        print(f"  MISMATCHED ({len(wrong)}) — {MISMATCHED}")
        for name, ourv, upv in wrong:
            ours_text, up_text = show_constant(ourv), show_constant(upv)
            print(f"    {name}: ours {ours_text}, upstream {up_text}")
    else:
        print("  MISMATCHED: none")
    if unknown:
        # Named like one of Notepad++'s constants and not one: a typo,
        # or a Code++ addition, which needs a name of its own.
        names = ", ".join(unknown)
        print(f"  NOT DEFINED UPSTREAM ({len(unknown)}) — fix each: {names}")
    if unparsed:
        # One of ours the evaluator cannot read is a constant this
        # check is not covering — fail rather than report "OK" for it.
        names = ", ".join(unparsed)
        print(f"  COULD NOT EVALUATE ({len(unparsed)}) — not checked: {names}")
    if unreadable:
        # The same hole from upstream's side: fail rather than report
        # these as merely undefined there.
        names = ", ".join(unreadable)
        count = len(unreadable)
        print(f"  UPSTREAM UNREADABLE ({count}) — not checked: {names}")
    return bool(wrong or unknown or unparsed or unreadable)


def check_family(upstream_text: str, family: Family) -> bool:
    """One family, read from upstream's headers as `read_c` returns
    them, against every source of ours that declares it. Returns True
    when anything disagrees."""
    upstream, unread = defined_constants(upstream_text, "c", family)
    if family.anchor not in upstream:
        print(f"could not parse the upstream {family.what}", file=sys.stderr)
        return True
    if unread:
        # Upstream's own definitions this evaluator cannot read are not
        # compared; a name of ours that shares one fails below as
        # unreadable upstream, which is the signal to extend it.
        print(f"\nwarning: could not evaluate upstream {listed(unread)}")
    failed = False
    for lang, path in family.sources:
        text = path.read_text(encoding="utf-8-sig", errors="replace")
        if lang == "c":
            text = read_c(text, label_of(path), upstream=False)
        else:
            text = abi_sources.strip_comments(text)
        ours, unparsed = defined_constants(text, lang, family)
        print(f"\n=== {label_of(path)} ({len(ours)} {family.what}) ===")
        failed |= report_constants(ours, unparsed, upstream, unread)
        declared = declared_names(text, lang, family.name_re)
        failed |= report_not_read(declared, ours.keys() | set(unparsed))
    return failed


def show_message(value: tuple[str, int]) -> str:
    """A message's number as its base plus its offset: `NPPMSG+7`."""
    return f"{short(value[0])}+{value[1]}"


def report_unknown_messages(
    unknown: list[tuple[str, tuple[str, int]]],
    upstream: Messages,
    by_number: dict[tuple[str, int], list[str]],
) -> bool:
    """Print the messages of ours upstream does not name, telling the
    ones upstream renamed from the rest. Returns True when any is not a
    known rename whose new name carries our number upstream: a name
    upstream has not got, a rename upstream renumbered or dropped, or a
    header cut short that names fewer than it should."""
    renamed, lost, other = [], [], []
    for name, value in unknown:
        now = KNOWN_RENAMES.get(name)
        if now is None:
            other.append((name, value))
        elif upstream.get(now) == value:
            renamed.append((name, value, now))
        else:
            lost.append((name, value, now))
    if renamed:
        print(f"  renamed upstream, same number ({len(renamed)}) — fine:")
        for name, value, now in renamed:
            print(f"    {name} ({show_message(value)}) is now {now}")
    if lost:
        print(f"  RENAME NOT UPSTREAM ({len(lost)}) — fix each:")
        for name, value, now in lost:
            up = upstream.get(now)
            where = f"upstream {show_message(up)}" if up else "not upstream's"
            print(f"    {name} ({show_message(value)}): {now} is {where}")
    if other:
        print(f"  NOT DEFINED UPSTREAM ({len(other)}) — fix each:")
        for name, value in other:
            squat = by_number.get(value, [])
            where = (
                f"that number is upstream's {listed(squat)}"
                if squat
                else "number unused upstream"
            )
            print(f"    {name}: {show_message(value)} — {where}")
    return bool(lost or other)


def report_messages(
    ours: Messages,
    upstream: Messages,
    by_number: dict[tuple[str, int], list[str]],
) -> bool:
    """Print how one source's messages compare with upstream's,
    naming the upstream message a wrong number of ours lands on.
    Returns True when a number disagrees, when a name of ours is not
    upstream's and not a known rename, or when the source declares no
    message."""
    if not ours:
        print(NONE_FOUND)
        return True
    wrong, unknown = [], []
    for name, value in sorted(ours.items()):
        if name not in upstream:
            unknown.append((name, value))
        elif upstream[name] != value:
            wrong.append((name, value, upstream[name]))
    if wrong:
        print(f"  MISMATCHED ({len(wrong)}) — {MISMATCHED}")
        for name, ourv, upv in wrong:
            squat = [n for n in by_number.get(ourv, []) if n != name]
            note = f"  [our number is upstream's {listed(squat)}]"
            extra = note if squat else ""
            print(
                f"    {name}: ours {show_message(ourv)}, "
                f"upstream {show_message(upv)}{extra}"
            )
    else:
        print("  MISMATCHED: none")
    unnamed = report_unknown_messages(unknown, upstream, by_number)
    return bool(wrong) or unnamed


def base_definitions(text: str, rust: bool, names: set[str]) -> Messages:
    """How comment-free `text` defines each of the message bases `names`,
    as a base plus an offset like a message (`NPPMSG` is `WM_USER +
    1000`), or with an empty base for a plain number (`NPPN_FIRST` is
    1000); a Rust `const`, public or private. A definition this cannot
    read, a symbolic offset among them, is left out and so fails as not
    read, and so is a base defined twice, however either reads. Read a
    line at a time, without a regular expression: the text is not ours."""
    out: Messages = {}
    seen: set[str] = set()
    for line in text.split("\n"):
        words = line.split()
        if rust:
            if words[:1] == ["pub"]:
                words = words[1:]
            if words[:1] != ["const"] or words[2:4] != ["u32", "="]:
                continue
            name = words[1].removesuffix(":")
            value = " ".join(words[4:]).removesuffix(";")
        else:
            joined = " ".join(words)
            if not joined.startswith("#"):
                continue
            words = joined[1:].split()
            if len(words) < 3 or words[0] != "define":
                continue
            name, value = words[1], " ".join(words[2:])
        if name not in names:
            continue
        if name in seen:
            out.pop(name, None)
            continue
        seen.add(name)
        value = value.strip()
        if value.startswith("(") and value.endswith(")"):
            value = value[1:-1]
        parts = [part.strip() for part in value.split("+")]
        base, offset = ("", parts[0]) if len(parts) == 1 else parts[:2]
        word = base.replace("_", "a")
        if len(parts) > 2 or base and not (word.isascii() and word.isalnum()):
            continue
        value_of = abi_sources.literal(offset)
        if value_of is not None:
            out[name] = (base, value_of)
    return out


def base_value(
    definition: tuple[str, int] | None,
    env: dict[str, int],
) -> int | None:
    """The number a base's definition stands for: its offset, plus its
    base word when it has one, a literal or a name `env` holds. None
    when the definition was not read or its word is neither."""
    if definition is None:
        return None
    word, offset = definition
    if not word:
        return offset
    start = abi_sources.literal(word)
    if start is None:
        start = env.get(word)
    return None if start is None else start + offset


def upstream_messages(up_text: str) -> Messages:
    """Upstream's messages, read from its header as `read_c` returns it,
    without the macros it defines (or undefines) twice, which a warning
    names: a message of ours by such a name, or counting from such an
    offset, is then not upstream's."""
    twice = abi_sources.defined_twice(up_text)
    if twice:
        print(f"\nwarning: defined twice upstream: {listed(sorted(twice))}")
    ints = abi_sources.integer_defines(up_text)
    ints = {k: v for k, v in ints.items() if k not in twice}
    parsed = parse(up_text, MESSAGE_DEFINE, ints)
    return {k: v for k, v in parsed.items() if k not in twice}


def show_base(definition: tuple[str, int] | None, env: dict[str, int]) -> str:
    """A base's definition as the report shows it: a plain number
    (`1000`), a base word and an offset with the number they stand for
    (`WM_USER+1000 = 2024`) or with the word that was not read
    (`WM_USER+1000, WM_USER not read`), or `not read`."""
    if definition is None:
        return "not read"
    if not definition[0]:
        return str(definition[1])
    value = base_value(definition, env)
    form = show_message(definition)
    if value is None:
        return f"{form}, {short(definition[0])} not read"
    return f"{form} = {value}"


def report_bases(
    ours: Messages,
    upstream: Messages,
    names: set[str],
    envs: tuple[dict[str, int], dict[str, int]],
) -> bool:
    """Print how a source's message bases compare with upstream's, by
    the number each stands for, `envs` resolving the base words of ours
    and of upstream's. A base that differs moves every message counted
    from it, and one either side does not define, or defines in a form
    not read, cannot be compared. Returns True for either."""
    env, up_env = envs
    wrong = []
    for name in sorted(names):
        mine = base_value(ours.get(name), env)
        if mine is None or mine != base_value(upstream.get(name), up_env):
            wrong.append(name)
    if wrong:
        print(
            f"  BASES MISMATCHED ({len(wrong)}) — or not read; each moves "
            "every message counted from it:"
        )
        for name in wrong:
            ourv = show_base(ours.get(name), env)
            upv = show_base(upstream.get(name), up_env)
            print(f"    {name}: ours {ourv}, upstream {upv}")
    else:
        print(f"  bases agree: {', '.join(sorted(names))}")
    return bool(wrong)


def check_bases(text: str, lang: str, ours: Messages, up_text: str) -> bool:
    """Compare the bases the messages `ours` count from, as `report_bases`
    prints them; True when one is wrong or not read. A `WM_USER` of ours
    is compared with upstream's own, or with Windows' when upstream's
    header defines none; a source of ours without one has Windows' in C,
    which includes `<windows.h>`, and none in Rust."""
    bases = {base for base, _ in ours.values()}
    if declared_names(text, lang, "WM_USER"):
        bases.add("WM_USER")
    mine = base_definitions(text, lang == "rust", bases)
    theirs = base_definitions(up_text, False, bases | {"WM_USER"})
    # In any form: `base_definitions` sees only the forms it reads.
    for side, source in ((mine, text), (theirs, up_text)):
        for name in abi_sources.defined_twice(source):
            side.pop(name, None)
    wm_user = base_value(mine.get("WM_USER"), {})
    if "WM_USER" not in bases:
        wm_user = None if lang == "rust" else WM_USER
    env = {} if wm_user is None else {"WM_USER": wm_user}
    if not declared_names(up_text, "c", "WM_USER"):
        theirs["WM_USER"] = ("", WM_USER)
    up_wm = base_value(theirs.get("WM_USER"), {})
    up_env = {} if up_wm is None else {"WM_USER": up_wm}
    return report_bases(mine, theirs, bases, (env, up_env))


def check_messages(up_text: str) -> bool:
    """The NPPM_*/NPPN_* messages, in the Rust dispatcher, the compat
    header and the plugin SDK, against upstream's, and the bases they
    count from. Returns True when a number or a base disagrees or cannot
    be read, when a name of ours is not upstream's or is declared in a
    form not read, and when a source declares no message; exits with
    status 2 when upstream's header, as `read_c` returns it, yields no
    message at all."""
    upstream = upstream_messages(up_text)
    if not upstream:
        print(
            "could not parse any message from the upstream header",
            file=sys.stderr,
        )
        sys.exit(2)

    by_number: dict[tuple[str, int], list[str]] = {}
    for name, value in upstream.items():
        by_number.setdefault(value, []).append(name)

    failed = False
    for path, pattern, lang in MESSAGE_SOURCES:
        text = path.read_text(encoding="utf-8-sig", errors="replace")
        if lang == "rust":
            text = abi_sources.strip_comments(text)
        else:
            text = read_c(text, label_of(path), upstream=False)
        # Our symbolic offsets are not resolved with upstream's defines,
        # which would cover for one of ours: each fails as not read.
        ours = parse(text, pattern, {})
        if lang == "c":
            # Rust refuses a name defined twice; C keeps the last, and a
            # form this does not read must not leave an earlier standing.
            for name in abi_sources.defined_twice(text):
                ours.pop(name, None)
        print(f"\n=== {label_of(path)} ({len(ours)} messages) ===")
        failed |= report_messages(ours, upstream, by_number)
        failed |= check_bases(text, lang, ours, up_text)
        declared = declared_names(text, lang, MESSAGE_NAME)
        bases = {base for base, _ in ours.values()}
        failed |= report_not_read(declared, ours.keys() | bases)
    return failed


def main() -> int:
    """Run all three passes; the exit status is 1 when any fails."""
    # A name read from upstream may hold any character a regular
    # expression's `\w` takes; one the console cannot encode is printed
    # escaped rather than ending the report in a traceback.
    for stream in (sys.stdout, sys.stderr):
        if isinstance(stream, io.TextIOWrapper):
            stream.reconfigure(errors="backslashreplace")
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    for flag, url in UPSTREAM_HEADERS.items():
        name = url.rsplit("/", 1)[1]
        ap.add_argument(
            flag,
            metavar="PATH",
            help=f"a local copy of upstream's {name}, for offline runs",
        )
    args = vars(ap.parse_args())

    deadline_at = time.monotonic() + FETCH_SECONDS

    def upstream(flag: str) -> str:
        """The upstream header `flag` names, from the command line's
        local copy or fetched, as `read_c` returns it."""
        local = args[flag[2:].replace("-", "_")]
        name = UPSTREAM_HEADERS[flag].rsplit("/", 1)[1]
        text = load_upstream(local, flag, deadline_at)
        return read_c(text, f"upstream {name}", upstream=True)

    failed = check_messages(upstream("--header"))
    docking = upstream("--docking-header")
    docking_resource = upstream("--docking-resource")
    failed |= check_family(docking + "\n" + docking_resource, DOCKING)
    failed |= check_family(upstream("--menu-header"), MENU)

    print()
    if failed:
        print(
            "FAIL: a number disagrees with upstream, or a name of ours is "
            "not upstream's or cannot be read."
        )
        return 1
    print(
        "OK: every message, docking constant and menu id Code++ and "
        "Notepad++ both name agrees."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
