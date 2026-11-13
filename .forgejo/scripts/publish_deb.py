#!/usr/bin/env python3
"""Publish a release's Debian package to Forgejo's package registry.

    publish_deb.py <package.deb>

Forgejo keeps an apt repository for each owner (docs/DESIGN.md, section
11.9). A user who has added it once gets every later version of Code++
from `apt upgrade`; this script is what puts a version there. The
package goes to the distribution `stable`, component `main`.

A version is published once: the registry refuses a second upload of
the same package, version and architecture. Running this again for the
same release therefore finds the package there and changes nothing,
which it reports and counts as done.

Either way it then reads the package back from the registry, as apt
would, without a token, and requires the bytes to be the release's.
A registry that holds another file under this version, uploaded before
or built again since, ends the step as a failure. If the package cannot
be read back at all, the step says so as a warning and passes: where
the registry serves a package's file is read from Forgejo's source, which
its documentation does not state, and has not been tried against the
registry yet (docs/DESIGN.md, section 11.10).

The rest comes from the environment, where the workflow puts it, so that
the token never appears on a command line:

    FORGEJO_URL     the instance, e.g. https://git.fiedler.live
    REPO            owner/name; the registry is the owner's
    VERSION         the release's version, x.y.z
    PACKAGES_TOKEN  a token that may write the owner's packages
    PACKAGES_USER   the account the token belongs to, if it is not the
                    owner; the upload logs in as that account with the
                    token for a password, the way Forgejo's
                    documentation shows it

Only the standard library is used, and nothing newer than Python 3.8.

Kept clean under black, isort, flake8, pylint and `mypy --strict`, all at
their default settings, as publish_forgejo.py is.
"""

from __future__ import annotations

import base64
import http.client
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import List, Tuple

import publish_forgejo as shared
from publish_forgejo import PublishError

DISTRIBUTION = "stable"
COMPONENT = "main"
# What the package says of itself (package-unix.sh), which is how the
# registry files it.
PACKAGE = "codepp"
ARCHITECTURE = "amd64"
VERSION_PATTERN = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")


def registry_url(base_url: str, repo: str) -> str:
    """Return the address a package is uploaded to."""
    owner = urllib.parse.quote(shared.repo_parts(base_url, repo)[0], safe="")
    where = f"api/packages/{owner}/debian/pool/{DISTRIBUTION}/{COMPONENT}"
    return f"{base_url.rstrip('/')}/{where}/upload"


def pool_url(upload_url: str, version: str) -> str:
    """Return the address the registry serves the uploaded package at."""
    folder = upload_url[: -len("upload")]
    return f"{folder}{PACKAGE}_{version}_{ARCHITECTURE}.deb"


def _package(path: Path) -> bytes:
    """Read the package, which must be the release's, and return it."""
    version = shared.env("VERSION")
    if not VERSION_PATTERN.fullmatch(version):
        raise PublishError(f"VERSION is not x.y.z (got {version!r})")
    return shared.release_file(path, f"codepp-linux-amd64-{version}.deb")


def _put(url: str, body: bytes, user: str, token: str) -> Tuple[int, bytes]:
    """Upload `body`; return the server's status and answer."""
    request = urllib.request.Request(url, data=body, method="PUT")
    login = base64.b64encode(f"{user}:{token}".encode("utf-8"))
    request.add_header("Authorization", "Basic " + login.decode("ascii"))
    request.add_header("Content-Type", "application/octet-stream")
    opener = urllib.request.build_opener(shared.NoRedirect)
    try:
        with opener.open(request, timeout=shared.TIMEOUT_SECONDS) as answer:
            status: int = answer.status
            return status, answer.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()
    except (urllib.error.URLError, OSError) as error:
        raise PublishError(f"PUT {url}: {error}") from error
    except http.client.HTTPException as error:
        what = type(error).__name__
        raise PublishError(f"PUT {url}: no usable answer ({what})") from error


def _read_back(url: str, body: bytes) -> str:
    """Compare the registry's copy with `body`; return what was found.

    Raises if the registry serves other bytes. A copy that cannot be
    fetched is reported, as a warning to the runner, and not held
    against the release.
    """
    opener = urllib.request.build_opener(shared.NoRedirect)
    try:
        with opener.open(url, timeout=shared.TIMEOUT_SECONDS) as answer:
            served: bytes = answer.read(len(body) + 1)
    except urllib.error.HTTPError as error:
        error.close()
        return f"::warning::not compared: {url} answers HTTP {error.code}"
    except (OSError, http.client.HTTPException) as error:
        # URLError is an OSError.
        what = type(error).__name__
        return f"::warning::not compared: {url} could not be read ({what})"
    if served != body:
        raise PublishError(
            f"{url} is not the release's package: the registry holds another "
            "file under this version (docs/RELEASING.md)"
        )
    return "the registry serves the release's package"


def publish(path: Path) -> str:
    """Upload the package; return what was done."""
    body = _package(path)
    url = registry_url(shared.env("FORGEJO_URL"), shared.env("REPO"))
    token = shared.env("PACKAGES_TOKEN")
    user = os.environ.get("PACKAGES_USER") or shared.env("REPO").split("/")[0]
    if ":" in user:
        raise PublishError("PACKAGES_USER is not an account's name")
    status, answer = _put(url, body, user, token)
    if status == 201:
        done = f"published {path.name} to {DISTRIBUTION}/{COMPONENT}"
    elif status == 409:
        done = f"the registry already has this version of {path.name}"
    else:
        what = f"uploading {path.name} failed: HTTP {status}"
        raise PublishError(f"{what}: {shared.shown(answer)}")
    found = _read_back(pool_url(url, shared.env("VERSION")), body)
    # On a line of its own on purpose: it may be a warning to the runner,
    # and every word of it is this script's.
    return f"{done}\n{found}"


def _run(argv: List[str]) -> int:
    """Do what the command line asks; return the exit status."""
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    print(publish(Path(argv[1])))
    return 0


def main(argv: List[str]) -> int:
    """Run from the command line; return the exit status."""
    return shared.guarded("publish_deb.py", lambda: _run(argv))


if __name__ == "__main__":
    sys.exit(main(sys.argv))
