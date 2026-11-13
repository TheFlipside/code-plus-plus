#!/usr/bin/env python3
"""Sign a release's Windows installer and publish the feed that offers it.

    update_feed.py check
    update_feed.py sign <installer> <feed-file>
    update_feed.py publish <installer> <feed-file>

An installed Code++ on Windows asks one file for the newest version:
`windows-x64.xml` on the repository's `appcast` branch, which holds the
feeds and nothing else (docs/DESIGN.md, section 11.7). The feed names
the release's installer, its length and its Ed25519 signature, and
Code++ installs nothing that signature does not verify.

`check` asks the server whether the `appcast` branch exists, and
compares the address the builds are given for the feed with where this
script publishes it, so that a release which could not publish its
feed, or whose builds would ask elsewhere, stops before anything is
built.

`sign` signs <installer> with the project's update key and writes the
feed to <feed-file>. The key reaches `openssl` through a pipe and is
never written to a file. Before signing, the public key derived from it
must be the one the builds carry; afterwards the signature must verify
against that public key. A wrong secret therefore stops here, before
anything is published. What is signed is a copy of the installer's
bytes as this script read them, so the feed's length and signature are
of the same bytes whatever happens to the file meanwhile.

The key is in this process's environment when it starts and in its
memory until it ends, and in the memory of each `openssl` it is handed
to. Whatever can read those processes, as anything running under the
runner's account can, can read the key: the runner is the boundary
(docs/DESIGN.md, section 11.7), and this script only keeps the key out
of files, command lines and messages.

`publish` puts <feed-file> on the `appcast` branch, as a commit made
through the server's API. First it downloads the installer from the
address the feed names, without a token, and requires the bytes to be
<installer>'s: a feed must never name a file that is not there. It
refuses to replace a feed that offers a newer version, and does nothing
when the branch already holds exactly this feed.

The rest comes from the environment, where the workflow puts it, so that
neither the token nor the key appears on a command line:

    FORGEJO_URL         the instance, e.g. https://git.fiedler.live
    REPO                owner/name
    TAG                 the release's tag (sign, publish)
    VERSION             the release's version, x.y.z (sign, publish)
    TOKEN               a token that may write to the repository
                        (check, publish)
    UPDATE_SIGNING_KEY  the private key: 32 bytes in base64, the form
                        `winsparkle-tool generate-key` writes (sign)

and two the Windows build reads too, under the same names, so that the
workflow states each once:

    CODEPP_UPDATE_PUBLIC_KEY  the public key the builds carry: 32 bytes
                              in base64 (sign)
    CODEPP_UPDATE_FEED_URL    where the builds ask for the feed (check)

`sign` needs `openssl` 3 or later on PATH, and a system where
`/dev/stdin` names a process's standard input, which the Linux runner
is. Only the standard library is used, and nothing newer than Python
3.8.

Kept clean under black, isort, flake8, pylint and `mypy --strict`, all at
their default settings, as publish_forgejo.py is.
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import http.client
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Dict, List, Optional, Tuple
from xml.sax.saxutils import quoteattr

import publish_forgejo as shared
from publish_forgejo import Forgejo, PublishError

FEED_BRANCH = "appcast"
FEED_NAME = "windows-x64.xml"
PUBLIC_KEY = "CODEPP_UPDATE_PUBLIC_KEY"
FEED_URL = "CODEPP_UPDATE_FEED_URL"
SPARKLE_NS = "http://www.andymatuschak.org/xml-namespaces/sparkle"
OPENSSL_SECONDS = 120

# An Ed25519 key is 32 bytes and a signature 64. `openssl` reads a key
# in its standard wrapping, which for this algorithm is a fixed prefix
# in front of the key's bytes (RFC 8410): PKCS#8 for a private key,
# SubjectPublicKeyInfo for a public one.
KEY_BYTES = 32
SIGNATURE_BYTES = 64
PKCS8_PREFIX = bytes.fromhex("302e020100300506032b657004220420")
SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")

VERSION_PATTERN = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")

# A file on a branch: its blob id and its content.
Blob = Tuple[str, bytes]
FEED_VERSION = re.compile(r"<sparkle:version>([^<]*)</sparkle:version>")


def _failed(what: str, status: int, body: bytes) -> PublishError:
    """Describe a request the server answered with an unexpected status."""
    return PublishError(f"{what} failed: HTTP {status}\n{shared.shown(body)}")


class FeedRepo(Forgejo):
    """The calls of a Forgejo repository's API the feed needs."""

    def branch_exists(self, branch: str) -> bool:
        """Say whether the repository has `branch`."""
        path = "/branches/" + urllib.parse.quote(branch, safe="")
        status, body = self._send("GET", path)
        if status == 404:
            return False
        if status != 200:
            raise _failed(f"looking up the branch {branch}", status, body)
        return True

    def file_on_branch(self, name: str, branch: str) -> Optional[Blob]:
        """Return a file's blob id and content on `branch`, if it is there."""
        query = urllib.parse.urlencode({"ref": branch})
        path = f"/contents/{urllib.parse.quote(name, safe='')}?{query}"
        status, body = self._send("GET", path)
        if status == 404:
            return None
        if status != 200:
            raise _failed(f"reading {name} on {branch}", status, body)
        entry = shared.json_object(body)
        blob, content = entry.get("sha"), entry.get("content")
        if not isinstance(blob, str) or not isinstance(content, str):
            raise PublishError(f"the server's answer for {name} is no file")
        try:
            return blob, base64.b64decode(content, validate=False)
        except (binascii.Error, ValueError) as error:
            what = f"the server's content for {name} is not base64"
            raise PublishError(what) from error

    def write_file(
        self,
        name: str,
        branch: str,
        content: bytes,
        message: str,
        replaces: Optional[str],
    ) -> None:
        """Commit `content` as `name` on `branch`.

        `replaces` is the blob id of the file the commit replaces, or
        None when the branch has no such file yet. The server refuses
        the commit if the file is no longer that one.
        """
        payload: Dict[str, str] = {
            "branch": branch,
            "content": base64.b64encode(content).decode("ascii"),
            "message": message,
        }
        if replaces is not None:
            payload["sha"] = replaces
        self._expect(
            f"writing {name} on {branch}",
            201 if replaces is None else 200,
            "POST" if replaces is None else "PUT",
            "/contents/" + urllib.parse.quote(name, safe=""),
            body=json.dumps(payload).encode("utf-8"),
            content_type="application/json",
        )


def _version() -> str:
    """Return the release's version, which must be a plain x.y.z."""
    version = shared.env("VERSION")
    if not VERSION_PATTERN.fullmatch(version):
        raise PublishError(f"VERSION is not x.y.z (got {version!r})")
    return version


def _numbers(version: str) -> Optional[Tuple[int, ...]]:
    """Return a plain version's three numbers, or None for anything else."""
    if not VERSION_PATTERN.fullmatch(version):
        return None
    return tuple(int(part) for part in version.split("."))


def _installer(path: Path, version: str) -> bytes:
    """Read the installer, which must be the release's, and return it."""
    expected = f"codepp-windows-x64-{version}.msi"
    if path.name != expected:
        what = f"the installer must be {expected}"
        raise PublishError(f"{what}, not {path.name}")
    try:
        if path.is_symlink() or not path.is_file():
            raise PublishError(f"{path} is not a regular file")
        return path.read_bytes()
    except OSError as error:
        raise PublishError(str(error)) from error


def download_url(base_url: str, repo: str, tag: str, name: str) -> str:
    """Return the address a release's file is downloaded from."""
    parts = urllib.parse.urlsplit(base_url)
    local = parts.hostname in shared.LOCAL_HOSTS
    if parts.scheme != "https" and not (parts.scheme == "http" and local):
        what = "FORGEJO_URL must be an https URL"
        raise PublishError(f"{what}, not {base_url!r}")
    owner, slash, project = repo.partition("/")
    if not owner or not slash or not project or "/" in project:
        raise PublishError(f"REPO must be owner/name, not {repo!r}")
    where = (owner, project, "releases", "download", tag, name)
    return _joined(base_url, where)


def _joined(base_url: str, parts: Tuple[str, ...]) -> str:
    """Return the address below `base_url` that `parts` name."""
    path = "/".join(urllib.parse.quote(part, safe="") for part in parts)
    return f"{base_url.rstrip('/')}/{path}"


def feed_url(base_url: str, repo: str) -> str:
    """Return the address the server serves the published feed at."""
    owner, _, project = repo.partition("/")
    where = (owner, project, "raw", "branch", FEED_BRANCH, FEED_NAME)
    return _joined(base_url, where)


def _release_url(name: str) -> str:
    """Return where the release in the environment serves `name`."""
    base_url, repo, tag = (
        shared.env("FORGEJO_URL"),
        shared.env("REPO"),
        shared.env("TAG"),
    )
    return download_url(base_url, repo, tag, name)


def feed_text(version: str, url: str, signature: str, length: int) -> str:
    """Return the feed that offers one installer.

    The same inputs give the same bytes: the feed states no date, so
    that publishing a release a second time changes nothing. It has no
    release-notes link either, which WinSparkle would open in a web
    view (docs/DESIGN.md, section 11.7).
    """
    enclosure = " ".join(
        (
            f"url={quoteattr(url)}",
            f"sparkle:edSignature={quoteattr(signature)}",
            f'length="{length}"',
            'type="application/octet-stream"',
            'sparkle:os="windows"',
        )
    )
    return "\n".join(
        (
            '<?xml version="1.0" encoding="utf-8"?>',
            f'<rss version="2.0" xmlns:sparkle="{SPARKLE_NS}">',
            "  <channel>",
            "    <title>Code++ for Windows</title>",
            "    <item>",
            f"      <title>Code++ {version}</title>",
            f"      <sparkle:version>{version}</sparkle:version>",
            f"      <enclosure {enclosure}/>",
            "    </item>",
            "  </channel>",
            "</rss>",
            "",
        )
    )


def _key(name: str, text: str) -> bytes:
    """Decode a key given as base64, which must be 32 bytes."""
    try:
        key = base64.b64decode(text.strip(), validate=True)
    except (binascii.Error, ValueError):
        key = b""
    if len(key) != KEY_BYTES:
        # Without the value: one of the two is the secret.
        raise PublishError(f"{name} is not {KEY_BYTES} bytes in base64")
    return key


def _pem(label: str, der: bytes) -> bytes:
    """Wrap a key's standard encoding as the text `openssl` reads."""
    body = base64.encodebytes(der).decode("ascii")
    text = f"-----BEGIN {label}-----\n{body}-----END {label}-----\n"
    return text.encode("ascii")


def _openssl(arguments: List[str], stdin: bytes) -> Tuple[int, bytes]:
    """Run `openssl`; return its exit status and what it wrote.

    Its environment is this process's without the private key, which it
    is handed on its standard input when it needs it and never
    otherwise. What it writes to its standard error is dropped: nothing
    here shows text from a process that was handed the key.
    """
    environment = dict(os.environ)
    environment.pop("UPDATE_SIGNING_KEY", None)
    try:
        done = subprocess.run(
            ["openssl", *arguments],
            input=stdin,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=environment,
            timeout=OPENSSL_SECONDS,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as error:
        what = f"openssl {arguments[0]} could not be run"
        raise PublishError(f"{what}: {type(error).__name__}") from None
    return done.returncode, done.stdout


def _require_openssl_3() -> None:
    """Stop unless `openssl` is version 3 or later.

    Older ones cannot sign a file with Ed25519 from the command line.
    """
    status, output = _openssl(["version"], b"")
    found = re.match(rb"OpenSSL ([0-9]+)\.", output)
    if status != 0 or found is None or int(found.group(1)) < 3:
        shown = output.decode("ascii", errors="replace").strip()[:80]
        raise PublishError(f"openssl 3 or later is needed, found {shown!r}")


def _private_pem(seed: bytes, public: bytes) -> bytes:
    """Wrap the key `seed` for `openssl`, once it is known to be the one.

    `public` is the key the builds carry, and the public key derived
    from `seed` must be it.
    """
    private_pem = _pem("PRIVATE KEY", PKCS8_PREFIX + seed)
    derive = ["pkey", "-pubout", "-outform", "DER"]
    status, derived = _openssl(derive, private_pem)
    if status != 0 or len(derived) != len(SPKI_PREFIX) + KEY_BYTES:
        raise PublishError("openssl could not read UPDATE_SIGNING_KEY")
    if derived != SPKI_PREFIX + public:
        raise PublishError(
            f"UPDATE_SIGNING_KEY is not the private half of {PUBLIC_KEY}, "
            "the key the builds carry; nothing was signed"
        )
    return private_pem


def signature_of(content: bytes, seed: bytes, public: bytes) -> bytes:
    """Sign `content` with the key `seed`; return the 64 bytes.

    `public` is the key the builds carry. The key derived from `seed`
    must be it, and the signature must verify against it.
    """
    _require_openssl_3()
    private_pem = _private_pem(seed, public)
    public_pem = _pem("PUBLIC KEY", SPKI_PREFIX + public)
    sign_it = ["pkeyutl", "-sign", "-rawin", "-inkey", "/dev/stdin"]
    verify = ["pkeyutl", "-verify", "-rawin", "-pubin"]

    # `openssl` reads what it signs from a file, and the key takes its
    # standard input. So it is given a copy of the bytes, in a folder
    # only this account can open: what is signed is then what this
    # script read, not what the installer's path names by now. Only
    # public things are written there: the installer, the public key
    # and the signature.
    try:
        with tempfile.TemporaryDirectory() as scratch:
            copy = str(Path(scratch) / "installer")
            Path(copy).write_bytes(content)
            status, signature = _openssl(sign_it + ["-in", copy], private_pem)
            if status != 0 or len(signature) != SIGNATURE_BYTES:
                raise PublishError("openssl could not sign the installer")
            (Path(scratch) / "public.pem").write_bytes(public_pem)
            (Path(scratch) / "signature").write_bytes(signature)
            verify += ["-inkey", str(Path(scratch) / "public.pem")]
            verify += ["-sigfile", str(Path(scratch) / "signature")]
            status, _ = _openssl(verify + ["-in", copy], b"")
    except OSError as error:
        what = "the installer could not be copied for signing"
        raise PublishError(f"{what}: {type(error).__name__}") from None
    if status != 0:
        what = "the installer's signature does not verify"
        raise PublishError(f"{what} against {PUBLIC_KEY}")
    return signature


def sign(installer: Path, feed_file: Path) -> str:
    """Sign the installer and write its feed; return the signature."""
    version = _version()
    content = _installer(installer, version)
    url = _release_url(installer.name)
    public = _key(PUBLIC_KEY, shared.env(PUBLIC_KEY))
    seed = _key("UPDATE_SIGNING_KEY", shared.env("UPDATE_SIGNING_KEY"))
    # Out of this process's environment from here on, so that nothing it
    # starts inherits the key by oversight.
    os.environ.pop("UPDATE_SIGNING_KEY", None)
    signature = signature_of(content, seed, public)
    text = base64.b64encode(signature).decode("ascii")
    feed = feed_text(version, url, text, len(content))
    try:
        feed_file.parent.mkdir(parents=True, exist_ok=True)
        # As bytes: the text has its own line ends, which a text-mode
        # write would change on some systems.
        feed_file.write_bytes(feed.encode("utf-8"))
    except OSError as error:
        raise PublishError(str(error)) from error
    return text


def _download(url: str, most: int) -> bytes:
    """Fetch `url` without a token; return at most `most` + 1 bytes.

    A body that is not as long as the server announced is an error, as
    it is to an installed Code++ fetching its feed: the library reports
    a connection closed early as the body's end.
    """
    opener = urllib.request.build_opener(shared.NoRedirect)
    try:
        with opener.open(url, timeout=shared.TIMEOUT_SECONDS) as answer:
            announced = answer.headers.get("Content-Length")
            data: bytes = answer.read(most + 1)
        if announced is not None and announced.strip() != str(len(data)):
            what = f"announces {announced.strip()[:20]} bytes"
            raise PublishError(f"{url} {what} and sends {len(data)}")
        return data
    except urllib.error.HTTPError as error:
        raise PublishError(f"{url} answers HTTP {error.code}") from error
    except (urllib.error.URLError, OSError) as error:
        raise PublishError(f"{url}: {error}") from error
    except http.client.HTTPException as error:
        # A body shorter than the server announced, among others.
        what = type(error).__name__
        raise PublishError(f"{url}: the download failed ({what})") from error


def _require_served(url: str, content: bytes) -> None:
    """Stop unless `url` serves exactly `content`."""
    served = _download(url, len(content))
    if hashlib.sha256(served).digest() != hashlib.sha256(content).digest():
        raise PublishError(
            f"{url} does not serve the installer that was signed "
            f"({len(served)} bytes against {len(content)}); "
            "the feed is not published"
        )


def _require_not_newer(published: bytes, version: str) -> None:
    """Stop if the feed on the branch offers a newer version than ours.

    Publishing an older release a second time must not take the feed
    back to it.
    """
    offered = FEED_VERSION.findall(published.decode("utf-8", "replace"))
    known = [n for n in map(_numbers, offered) if n is not None]
    ours = _numbers(version)
    if known and ours is not None and max(known) > ours:
        raise PublishError(
            f"{FEED_NAME} on {FEED_BRANCH} offers a newer version than "
            f"{version}; it is left as it is"
        )


def publish(installer: Path, feed_file: Path) -> str:
    """Put the feed on its branch; return what was done."""
    version = _version()
    content = _installer(installer, version)
    url = _release_url(installer.name)
    try:
        feed = feed_file.read_bytes()
    except OSError as error:
        raise PublishError(str(error)) from error
    text = feed.decode("utf-8", errors="replace")
    if FEED_VERSION.findall(text) != [version] or quoteattr(url) not in text:
        raise PublishError(f"{feed_file} is not the feed for {version}")
    _require_served(url, content)

    forgejo = FeedRepo(
        shared.env("FORGEJO_URL"), shared.env("REPO"), shared.env("TOKEN")
    )
    if not forgejo.branch_exists(FEED_BRANCH):
        raise PublishError(
            f"the repository has no {FEED_BRANCH} branch (docs/RELEASING.md)"
        )
    existing = forgejo.file_on_branch(FEED_NAME, FEED_BRANCH)
    if existing is not None and existing[1] == feed:
        return f"{FEED_NAME} on {FEED_BRANCH} already offers {version}"
    if existing is not None:
        _require_not_newer(existing[1], version)
    forgejo.write_file(
        FEED_NAME,
        FEED_BRANCH,
        feed,
        f"Code++ {version} for Windows",
        None if existing is None else existing[0],
    )
    return f"{FEED_NAME} on {FEED_BRANCH} now offers {version}"


def check() -> str:
    """Stop unless the feed can be published where the builds ask for it."""
    base_url, repo = shared.env("FORGEJO_URL"), shared.env("REPO")
    forgejo = FeedRepo(base_url, repo, shared.env("TOKEN"))
    asked, served = shared.env(FEED_URL), feed_url(base_url, repo)
    if asked != served:
        raise PublishError(
            f"the builds would ask {asked} for the feed ({FEED_URL}), "
            f"but it is published at {served}"
        )
    if not forgejo.branch_exists(FEED_BRANCH):
        raise PublishError(
            f"the repository has no {FEED_BRANCH} branch, which the update "
            "feed is published on; create it first (docs/RELEASING.md)"
        )
    return f"the {FEED_BRANCH} branch exists; the feed is {served}"


def main(argv: List[str]) -> int:
    """Run from the command line; return the exit status."""
    try:
        if argv[1:] == ["check"]:
            print(check())
        elif len(argv) == 4 and argv[1] == "sign":
            signature = sign(Path(argv[2]), Path(argv[3]))
            print(f"signed {Path(argv[2]).name}: {signature}")
        elif len(argv) == 4 and argv[1] == "publish":
            print(publish(Path(argv[2]), Path(argv[3])))
        else:
            print(__doc__, file=sys.stderr)
            return 2
    except PublishError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    except Exception as error:  # pylint: disable=broad-exception-caught
        # Anything unforeseen ends the step like any other failure, and
        # says only what kind it was: a traceback would show source
        # lines of a process that may be holding the key, and an
        # exception's own text is not known to be free of it.
        what = type(error).__name__
        print(f"::error::update_feed.py failed: {what}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
