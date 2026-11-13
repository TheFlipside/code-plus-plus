#!/usr/bin/env python3
"""Publish a release on Forgejo: create it if missing, then upload its files.

    publish_forgejo.py <files-dir> <notes-file>

Every regular file in <files-dir> becomes a release asset under its own
name. <notes-file> is the release's description, in Markdown.

The rest comes from the environment, where the workflow puts it, so that
the token never appears on a command line:

    FORGEJO_URL   the instance, e.g. https://git.fiedler.live
    REPO          owner/name
    TAG           the tag the release is for; it must exist already
    TOKEN         an access token that may write to the repository
    RELEASE_NAME  the release's title

Running it again for the same tag is safe: the release is found rather
than created, its title and description are left as they are (they may
have been edited by hand since), and each file replaces the asset of the
same name.

Only the standard library is used, and nothing newer than Python 3.8, so
the runner needs no packages installed.

Kept clean under black, isort, flake8, pylint and `mypy --strict`, all at
their default settings, as the checker in tools/npp-abi-check is.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Tuple

TIMEOUT_SECONDS = 300
ERROR_BODY_CHARS = 2000
LOCAL_HOSTS = ("localhost", "127.0.0.1", "::1")


class PublishError(Exception):
    """The release could not be published; the message says why."""


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Refuse to follow a redirect.

    urllib would send the request again, token included, to wherever the
    redirect points. The API answers directly, so a redirect here means
    the URL is wrong, and the caller sees the 3xx status as a failure.
    """

    def redirect_request(  # pylint: disable=arguments-differ
        self, *_args: Any, **_kwargs: Any
    ) -> None:
        return None


class Forgejo:
    """The calls of a Forgejo repository's release API this script makes."""

    def __init__(self, base_url: str, repo: str, token: str) -> None:
        parts = urllib.parse.urlsplit(base_url)
        local_http = parts.scheme == "http" and parts.hostname in LOCAL_HOSTS
        if parts.scheme != "https" and not local_http:
            raise PublishError(
                f"FORGEJO_URL must be an https URL, not {base_url!r}: "
                "the token would cross the network in the clear"
            )
        owner, slash, name = repo.partition("/")
        if not owner or not slash or not name or "/" in name:
            raise PublishError(f"REPO must be owner/name, not {repo!r}")
        owner = urllib.parse.quote(owner, safe="")
        name = urllib.parse.quote(name, safe="")
        self._api = f"{base_url.rstrip('/')}/api/v1/repos/{owner}/{name}"
        self._token = token
        self._opener = urllib.request.build_opener(NoRedirect)

    def _send(
        self,
        method: str,
        path: str,
        body: Optional[bytes] = None,
        content_type: Optional[str] = None,
    ) -> Tuple[int, bytes]:
        """Make one request; return its status and body, whatever they are."""
        url = self._api + path
        request = urllib.request.Request(url, data=body, method=method)
        request.add_header("Authorization", f"token {self._token}")
        request.add_header("Accept", "application/json")
        if content_type is not None:
            request.add_header("Content-Type", content_type)
        try:
            with self._opener.open(request, timeout=TIMEOUT_SECONDS) as answer:
                return answer.status, answer.read()
        except urllib.error.HTTPError as error:
            return error.code, error.read()
        except (urllib.error.URLError, OSError) as error:
            raise PublishError(f"{method} {path}: {error}") from error

    def _expect(
        self, what: str, expected: int, method: str, path: str, **kwargs: Any
    ) -> bytes:
        """Make one request that must answer `expected`; return its body."""
        status, body = self._send(method, path, **kwargs)
        if status != expected:
            raise PublishError(f"{what} failed: HTTP {status}: {shown(body)}")
        return body

    def release_for_tag(self, tag: str) -> Optional[Dict[str, Any]]:
        """Return the release published for `tag`, or None without one."""
        path = "/releases/tags/" + urllib.parse.quote(tag, safe="")
        status, body = self._send("GET", path)
        if status == 404:
            return None
        if status != 200:
            what = f"looking up the release for {tag}"
            raise PublishError(f"{what} failed: HTTP {status}: {shown(body)}")
        return json_object(body)

    def create_release(
        self,
        tag: str,
        name: str,
        notes: str,
    ) -> Dict[str, Any]:
        """Create the release for `tag`, which must exist, and return it.

        Never as a pre-release: a release has a plain version, and the
        workflow publishes no other (docs/DESIGN.md, section 11.6).
        """
        payload = {
            "tag_name": tag,
            "name": name,
            "body": notes,
            "draft": False,
            "prerelease": False,
        }
        body = self._expect(
            f"creating the release for {tag}",
            201,
            "POST",
            "/releases",
            body=json.dumps(payload).encode("utf-8"),
            content_type="application/json",
        )
        return json_object(body)

    def delete_asset(self, release_id: int, asset_id: int) -> None:
        """Remove one asset from a release."""
        self._expect(
            f"deleting asset {asset_id}",
            204,
            "DELETE",
            f"/releases/{release_id}/assets/{asset_id}",
        )

    def upload_asset(self, release_id: int, path: Path) -> None:
        """Attach the file at `path` to a release, under its own name."""
        boundary = uuid.uuid4().hex
        # The name travels in the query, which Forgejo reads; the one in
        # the form part is fixed, so no file name needs escaping there.
        disposition = 'form-data; name="attachment"; filename="attachment"'
        head = (
            f"--{boundary}\r\n"
            f"Content-Disposition: {disposition}\r\n"
            "Content-Type: application/octet-stream\r\n\r\n"
        ).encode("ascii")
        tail = f"\r\n--{boundary}--\r\n".encode("ascii")
        query = urllib.parse.urlencode({"name": path.name})
        self._expect(
            f"uploading {path.name}",
            201,
            "POST",
            f"/releases/{release_id}/assets?{query}",
            body=head + path.read_bytes() + tail,
            content_type=f"multipart/form-data; boundary={boundary}",
        )


def shown(body: bytes) -> str:
    """A server's answer, cut short and on one line, for an error message.

    On one line, and never at the start of one: the runner reads a line
    of the log that begins with `::` as a command to itself, and the
    answer is the server's text, not this script's.
    """
    text = body.decode("utf-8", errors="replace")[:ERROR_BODY_CHARS]
    flat = " ".join(text.split())
    # Nor does it get to restyle the log: an escape sequence or a
    # character that reorders text is shown as a question mark.
    return "".join(ch if ch.isprintable() else "?" for ch in flat)


def json_object(body: bytes) -> Dict[str, Any]:
    """Parse a server's answer, which must be a JSON object."""
    try:
        value = json.loads(body.decode("utf-8"))
    except ValueError as error:
        what = "the server's answer is not JSON"
        raise PublishError(f"{what}: {shown(body)}") from error
    if not isinstance(value, dict):
        what = "the server's answer is not a JSON object"
        raise PublishError(f"{what}: {shown(body)}")
    return value


def _existing_assets(release: Dict[str, Any]) -> Dict[str, int]:
    """Map each asset name of `release` to its id."""
    assets: Dict[str, int] = {}
    for asset in release.get("assets") or []:
        if isinstance(asset, dict) and isinstance(asset.get("id"), int):
            assets[str(asset.get("name"))] = asset["id"]
    return assets


def env(name: str) -> str:
    """Return a variable the workflow must have set."""
    value = os.environ.get(name, "")
    if not value:
        raise PublishError(f"{name} is not set")
    return value


def repo_parts(base_url: str, repo: str) -> Tuple[str, str]:
    """Check FORGEJO_URL and REPO; return the repository's owner and name."""
    parts = urllib.parse.urlsplit(base_url)
    local = parts.hostname in LOCAL_HOSTS
    if parts.scheme != "https" and not (parts.scheme == "http" and local):
        what = "FORGEJO_URL must be an https URL"
        raise PublishError(f"{what}, not {base_url!r}")
    owner, slash, project = repo.partition("/")
    if not owner or not slash or not project or "/" in project:
        raise PublishError(f"REPO must be owner/name, not {repo!r}")
    return owner, project


def release_file(path: Path, expected: str) -> bytes:
    """Read one of the release's files, which must be named `expected`.

    It must be a regular file too: a link could name any file the
    runner's account can read.
    """
    if path.name != expected:
        raise PublishError(f"the file must be {expected}, not {path.name}")
    try:
        if path.is_symlink() or not path.is_file():
            raise PublishError(f"{path} is not a regular file")
        return path.read_bytes()
    except OSError as error:
        raise PublishError(str(error)) from error


def guarded(script: str, step: Callable[[], int]) -> int:
    """Run `step`; end any failure in one line and the exit status 1.

    Anything unforeseen ends like any other failure, and says only what
    kind it was: a traceback would show source lines of a process that
    holds a token, or the update key, and an exception's own text is not
    known to be free of either.
    """
    try:
        return step()
    except PublishError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    except Exception as error:  # pylint: disable=broad-exception-caught
        what = type(error).__name__
        print(f"::error::{script} failed: {what}", file=sys.stderr)
        return 1


def publish(files_dir: Path, notes_file: Path) -> str:
    """Publish the release the environment describes; return its page."""
    tag = env("TAG")
    forgejo = Forgejo(env("FORGEJO_URL"), env("REPO"), env("TOKEN"))

    try:
        files = sorted(p for p in files_dir.iterdir() if p.is_file())
        notes = notes_file.read_text(encoding="utf-8")
    except OSError as error:
        raise PublishError(str(error)) from error
    if not files:
        raise PublishError(f"no files to publish in {files_dir}")

    release = forgejo.release_for_tag(tag)
    if release is None:
        print(f"creating release {tag}")
        name = env("RELEASE_NAME")
        release = forgejo.create_release(tag, name, notes)
    else:
        print(f"release {tag} exists; replacing its files")
    release_id = release.get("id")
    if not isinstance(release_id, int):
        raise PublishError("the server's answer names no release id")

    existing = _existing_assets(release)
    for path in files:
        if path.name in existing:
            print(f"deleting the existing {path.name}")
            forgejo.delete_asset(release_id, existing[path.name])
        print(f"uploading {path.name} ({path.stat().st_size} bytes)")
        forgejo.upload_asset(release_id, path)
    return str(release.get("html_url", ""))


def _run(argv: List[str]) -> int:
    """Do what the command line asks; return the exit status."""
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    url = publish(Path(argv[1]), Path(argv[2]))
    print(f"published {url}")
    return 0


def main(argv: List[str]) -> int:
    """Run from the command line; return the exit status."""
    return guarded("publish_forgejo.py", lambda: _run(argv))


if __name__ == "__main__":
    sys.exit(main(sys.argv))
