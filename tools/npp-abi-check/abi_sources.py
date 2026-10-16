"""Read the C headers and Rust sources the ABI check compares.

What `check.py` needs of a source is its constants and its messages, so
this reads what decides them as their compilers do: comments, line
splices, string literals and `#define` directives, then the numbers and
constant expressions a definition holds. Half the input is fetched from
the internet, so every reader here is linear in what it reads, refuses
what it cannot read the way a compiler would rather than guessing, and
bounds every value it evaluates.

`check.py`, beside this file, imports it: run that by its path, since
`python -I` and `python -P` leave this folder off the import path.
"""

from __future__ import annotations

import ast
import re
import warnings

# The most digits a number may have to be read as one. No ABI number
# comes near it, and Python refuses to convert a string of over 4300.
MAX_DIGITS = 18
# The bound on what an expression may evaluate to, at every step. Every
# ABI number fits in 64 bits, while a chain of shifts in a fetched
# header could build one too large to print: Python refuses to convert
# an int of over 4300 digits to a string.
MAX_VALUE = 1 << 64
# The expressions `evaluate` hands the parser: at most 1024 characters,
# every one of them ASCII and of a name, an integer, `+`, `<`, `|`, a
# parenthesis or whitespace, which takes in all `evaluate` reads. The
# parser costs more than the text: an f-string takes time quadratic in
# its fields, and a string literal makes it write a warning holding the
# literal's bytes to stderr.
CONSTANT_EXPR = re.compile(r"(?a)[\w\s+<|()]{1,1024}")
# A number Python reads and C does not: one with a `_` between its
# digits, or an octal one spelled `0o`. A C header holding either does
# not compile.
PYTHON_ONLY_NUMBER = re.compile(r"\b\d\w*_|\b0[oO]")

# Whitespace within a line but the carriage return: every character
# `\s` takes but the line feed and the carriage return, and U+180E,
# which clang reads as whitespace (with a warning) and Python's `\s`
# does not. Written out, so that it does not change with Python's
# Unicode tables. Wider than C's blanks on purpose: a character some
# compiler skips must not hide a directive, and one none skips only
# makes this module read more.
SPACES = (
    r"\t\x0b\x0c\x1c-\x1f \x85\xa0\u1680\u180e"
    r"\u2000-\u200a\u2028\u2029\u202f\u205f\u3000"
)
# Whitespace within a line, which is all a directive may span.
BLANK = rf"[\r{SPACES}]"
# A C directive defining a macro, `#` and `define` spaced or not.
DEFINE = rf"{BLANK}*#{BLANK}*define{BLANK}+"
# One that defines or undefines one: either ends what was in force.
DEFINE_OR_UNDEF = rf"^{BLANK}*#{BLANK}*(?:define|undef){BLANK}+(\w+)"
# What C compilers read differently from one another or from this
# module, refused in a C header rather than read one way: a NUL, which
# gcc and clang take for whitespace; a trigraph, which some modes read;
# a backslash then blanks at a line end, a splice to gcc and clang but
# not to MSVC; and a carriage return not before a line feed.
UNREADABLE = re.compile(rf"\x00|\?\?[=/'()!<>-]|\\[{SPACES}]+\r?\n|\r(?!\n)")
# A conditional directive, which this module does not evaluate.
CONDITIONAL = re.compile(
    rf"^{BLANK}*#{BLANK}*(?:if|ifdef|ifndef|elif\w*|else|endif)\b", re.M
)
# The directives an upstream header may hold, after its `#`: a macro
# defined or undefined, a null directive, `#pragma once` and an include
# of a plain header name. Any other could end or hide a definition the
# passes read as standing (`#pragma push_macro`), or is not read as C
# reads it (`#include <a/*b.h>`, whose header name holds no comment).
READ_DIRECTIVE = re.compile(
    rf"{BLANK}*(?:(?:define|undef){BLANK}.*|pragma{BLANK}+once{BLANK}*"
    rf"|include{BLANK}*<[\w./]+>{BLANK}*)?"
)
# Where a comment or a literal starts in C, and how a literal ends:
# within its line, past any escaped character.
C_MARK = re.compile(r"""/[*/]|["']""")
C_LITERAL = {
    '"': re.compile(r'"[^"\\\n]*(?:\\.[^"\\\n]*)*"'),
    "'": re.compile(r"'[^'\\\n]*(?:\\.[^'\\\n]*)*'"),
}


def number(text: str) -> int | None:
    """`text` as a number when it is ASCII digits only, no longer than
    `MAX_DIGITS` and without a leading zero, else None: C reads `010` as
    octal 8 (clippy refuses one in Rust), and a digit outside ASCII, such
    as `²`, passes `str.isdigit` and makes `int` raise."""
    if not text.isascii() or not text.isdigit() or len(text) > MAX_DIGITS:
        return None
    return int(text) if len(text) == 1 or text[0] != "0" else None


def literal(text: str) -> int | None:
    """`text` as a number when it is decimal (see `number`) or ASCII
    hex with a `0x` or `0X` prefix and at most 16 digits, else None."""
    digits = text[2:]
    hexdigits = "0123456789abcdefABCDEF"
    if text[:2] in ("0x", "0X") and 0 < len(digits) <= 16:
        if all(c in hexdigits for c in digits):
            return int(digits, 16)
        return None
    return number(text)


def integer_defines(text: str) -> dict[str, int]:
    """Every `#define NAME 7` in preprocessed `text`, a line holding
    nothing else, with its number read by `number`: needed to resolve
    symbolic offsets such as `RUNCOMMAND_USER + NPP_DIRECTORY`."""
    pattern = rf"^{DEFINE}([A-Z_][A-Z0-9_]*){BLANK}+([0-9]+){BLANK}*$"
    out: dict[str, int] = {}
    for m in re.finditer(pattern, text, re.M):
        value = number(m.group(2))
        if value is not None:
            out[m.group(1)] = value
    return out


def parse_expression(expr: str) -> ast.Expression:
    """`expr` as a syntax tree. Raises ValueError, without handing it
    to the parser, for text `CONSTANT_EXPR` does not admit, and keeps
    the parser's own warnings off the terminal."""
    if not CONSTANT_EXPR.fullmatch(expr):
        raise ValueError("not a constant expression")
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        return ast.parse(expr.strip(), mode="eval")


def evaluate(expr: str, env: dict[str, int]) -> int:
    """Evaluate a constant expression from a header or a Rust `const`,
    by AST node rather than `eval`, since half the input is fetched from
    the internet: integer literals (hex and `_` included), names already
    resolved in the same file, and `+`, `<<` and `|`, every operator the
    docking headers use. Anything else raises, and the caller leaves the
    name unresolved, as it does a value reaching `MAX_VALUE` at any step:
    a literal, a sum and a shift are checked, and neither an `|` of two
    values under it nor a name, which holds such a value, can reach it."""

    def bounded(value: int) -> int:
        if abs(value) >= MAX_VALUE:
            raise ValueError(f"out of range: {expr!r}")
        return value

    def ev(node: ast.AST) -> int:
        if isinstance(node, ast.Expression):
            return ev(node.body)
        # A `bool` is an `int` to Python, and `True` is no header's.
        value = node.value if isinstance(node, ast.Constant) else None
        if isinstance(value, int) and not isinstance(value, bool):
            return bounded(value)
        if isinstance(node, ast.Name) and node.id in env:
            return env[node.id]
        if isinstance(node, ast.BinOp):
            left, right = ev(node.left), ev(node.right)
            if isinstance(node.op, ast.Add):
                return bounded(left + right)
            if isinstance(node.op, ast.LShift) and 0 <= right < 64:
                return bounded(left << right)
            if isinstance(node.op, ast.BitOr):
                return left | right
        raise ValueError(f"unsupported expression: {expr!r}")

    return ev(parse_expression(expr))


def names_in(expr: str) -> set[str] | None:
    """The names `expr` refers to, or None when it does not parse."""
    try:
        tree = parse_expression(expr)
    except (SyntaxError, ValueError, RecursionError, MemoryError):
        # MemoryError is the parser's "too complex to parse", and memory
        # can run out besides. An expression `CONSTANT_EXPR` admits
        # should raise neither, but either leaves one name out here
        # rather than ending the run.
        return None
    return {node.id for node in ast.walk(tree) if isinstance(node, ast.Name)}


def resolve(raw: dict[str, str]) -> dict[str, int]:
    """Evaluate each definition in `raw` that can be, once, after the
    definitions it refers to, so one may refer to a name defined later
    in the file. A worklist (Kahn's algorithm) rather than passes until
    nothing changes, which take quadratic time on definitions in
    reverse order, and the fetched half of the input is not ours. A
    definition referring to itself, to a cycle, or to a name that does
    not evaluate is left out."""
    waiting: dict[str, set[str]] = {}
    users: dict[str, list[str]] = {}
    ready: list[str] = []
    for name, expr in raw.items():
        refs = names_in(expr)
        if refs is None:
            continue
        # A comprehension rather than `refs & raw.keys()`, which is linear
        # in `raw` on Pythons before 3.10, so quadratic over the loop.
        waiting[name] = {ref for ref in refs if ref in raw}
        for ref in waiting[name]:
            users.setdefault(ref, []).append(name)
        if not waiting[name]:
            ready.append(name)
    out: dict[str, int] = {}
    while ready:
        name = ready.pop()
        try:
            out[name] = evaluate(raw[name], out)
        except (ValueError, SyntaxError, RecursionError, MemoryError):
            continue
        for user in users.get(name, []):
            waiting[user].discard(name)
            if not waiting[user]:
                ready.append(user)
    return out


def strip_comments(text: str) -> str:
    r"""`text`, a Rust source, with each `/* */` and `//` comment a space:
    what `re.sub(r"/\*.*?\*/|//[^\n]*", " ", text, flags=re.S)` returns,
    in one linear scan where that takes quadratic time on many unclosed
    `/*`. A comment opened inside another is part of it, a `/*` with no
    `*/` after it opens nothing, so no later one can either, and a `//`
    comment ends before its line break. String literals are not read:
    the Rust sources this check reads hold no comment marker in one."""
    out: list[str] = []
    pos = 0
    closable = True
    while (start := text.find("/", pos)) != -1:
        end = -1
        if closable and text.startswith("*", start + 1):
            end = text.find("*/", start + 2)
            if end == -1:
                closable = False
            else:
                end += 2
        elif text.startswith("/", start + 1):
            end = text.find("\n", start)
            if end == -1:
                end = len(text)
        if end == -1:
            # Not a comment: keep the slash and look on.
            end = start + 1
            out.append(text[pos:end])
        else:
            out.append(text[pos:start] + " ")
        pos = end
    out.append(text[pos:])
    return "".join(out)


def literal_end(text: str, start: int, quote: str) -> int:
    """Where the C string or character literal `quote` opens at `start`
    in `text` ends. Raises ValueError for one never closed on its line,
    and for a quote this does not read as C reads it: one after a letter,
    a digit or `_`, which may be a digit separator (`1'000`), and a raw
    string (`R"(...)"`), whose content may hold a quote."""
    before = text[start - 1] if start else ""
    if quote == "'" and (before.isalnum() or before == "_"):
        raise ValueError(f"a quote after {before!r}, which is not read")
    if quote == '"' and before == "R":
        raise ValueError("a raw string literal, which is not read")
    lit = C_LITERAL[quote].match(text, start)
    if lit is None:
        raise ValueError("a string or character literal never closed")
    return lit.end()


def strip_c(text: str) -> str:
    """`text`, a C header with its line splices joined, as the C lexer
    leaves it for the directives: each comment a space and each string
    or character literal an empty one, so that neither can hide or forge
    a definition. Read from the left, as C reads it, so a comment marker
    inside a literal starts no comment and a quote inside a comment
    starts no literal. Raises ValueError for a comment or a literal
    never closed, which no compiler accepts either, and for a literal
    `literal_end` does not read: a quote after a letter, a digit or `_`,
    and a raw string."""
    out: list[str] = []
    pos = 0
    while found := C_MARK.search(text, pos):
        start, mark = found.start(), found.group()
        out.append(text[pos:start])
        if mark == "//":
            end = text.find("\n", start)
            pos = len(text) if end == -1 else end
            out.append(" ")
        elif mark == "/*":
            end = text.find("*/", start + 2)
            if end == -1:
                raise ValueError("a comment never closed")
            pos = end + 2
            out.append(" ")
        else:
            pos = literal_end(text, start, mark)
            out.append(mark * 2)
    out.append(text[pos:])
    return "".join(out)


def preprocessed(text: str) -> str:
    """A C header as the preprocessor reads its directives: line splices
    joined, then comments and literals read by `strip_c`. Raises
    ValueError, naming what it found, for anything `UNREADABLE` matches,
    a comment or a literal never closed, and a directive spelled with
    the `%:` digraph: each could hide a definition from every scan."""
    if found := UNREADABLE.search(text):
        # A run of blanks can be as long as the header, and `!r` writes
        # some of them in four characters: show the start of it.
        shown = found[0][:16]
        raise ValueError(f"something compilers read differently: {shown!r}")
    out = strip_c(re.sub(r"\\\r?\n", "", text))
    if re.search(rf"^{BLANK}*%:", out, re.M):
        raise ValueError("a directive spelled with the %: digraph")
    return out


def unconditional(text: str) -> str:
    """Preprocessed `text`, refused with ValueError when it holds a
    conditional directive (a definition under `#if 0` would be read as
    live, since nothing here evaluates one), a directive other than the
    few `READ_DIRECTIVE` takes, `_Pragma` or `__pragma`, which act as
    `#pragma` does: `push_macro` and `pop_macro` around a definition
    leave the macro as it was before it, where the passes read the
    definition as standing, or `##` or a `%:` digraph, which can paste
    a `_Pragma` together (`%:%:` is the digraph of `##`)."""
    if CONDITIONAL.search(text):
        raise ValueError("a conditional directive, which is not evaluated")
    for line in re.finditer(rf"^{BLANK}*#(.*)$", text, re.M):
        if not READ_DIRECTIVE.fullmatch(line[1]):
            raise ValueError(f"a directive not read: {line[1].strip()[:16]!r}")
    if re.search(r"\b_{1,2}[Pp]ragma\b", text):
        raise ValueError("_Pragma or __pragma, which is not read")
    if "##" in text or "%:" in text:
        raise ValueError("## or %:, which can paste a _Pragma together")
    return text


def defined_twice(text: str) -> set[str]:
    """The macros preprocessed `text` defines, or undefines, more than
    once, in any form: the last directive is the one in force, and one a
    pass cannot read must not leave an earlier one standing."""
    seen: set[str] = set()
    twice: set[str] = set()
    for m in re.finditer(DEFINE_OR_UNDEF, text, re.M):
        if m.group(1) in seen:
            twice.add(m.group(1))
        seen.add(m.group(1))
    return twice


def bare_operator(value: str) -> bool:
    """Whether `value` has a `+`, `<` or `|` outside every parenthesis."""
    depth = 0
    for char in value:
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
        elif depth == 0 and char in "+<|":
            return True
    return False


def bare_operator_users(raw: dict[str, str]) -> set[str]:
    """The names in `raw`, C macro bodies, that use a macro whose body
    has a bare operator (see `bare_operator`). The compiler substitutes
    a body as text, so `(A << 28)` with `A` defined as `1 | 0` is
    `(1 | 0 << 28)`, while `evaluate` would shift the value of `A`."""
    bare = {name for name, value in raw.items() if bare_operator(value)}
    users: set[str] = set()
    for name, value in raw.items():
        if (names_in(value) or set()) & bare:
            users.add(name)
    return users
