#!/usr/bin/env bash
# Decide what the release workflow builds, from the ref that started it.
#
# Prints two `key=value` lines, which the workflow appends to
# `$GITHUB_OUTPUT`:
#
#   version  the version the build shows and the archives are named
#            for: `0.2.0` for a tag, `0.2.0-dev.<commit>` for a manual
#            run on a branch
#   publish  `true` for a pushed tag, `false` for a manual run (which
#            builds and packages everything and publishes nothing, also
#            when it is started on a tag)
#
# A tag must be `v<version>`, where <version> is
# `[workspace.package].version` in Cargo.toml, and nothing more. So
# with Cargo.toml at 0.2.0, `v0.2.0` is a release, `v0.2.1` is refused
# until Cargo.toml says 0.2.1, and `v0.2.0-rc.1` is refused always: a
# release has a plain version, which is what an installed Code++, its
# updater and Windows Installer all compare (docs/DESIGN.md §11.6).
#
# Reads GITHUB_REF, GITHUB_EVENT_NAME and GITHUB_SHA, and Cargo.toml in
# the current directory. Exits non-zero, with the reason on stderr, for
# a tag it refuses.

set -euo pipefail

fail() {
  echo "::error::$*" >&2
  exit 1
}

cargo_version=$(
  sed -n '/^\[workspace\.package\]/,/^\[/s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
    Cargo.toml | head -n 1
)
[[ "$cargo_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
  fail "could not read a plain x.y.z [workspace.package] version from Cargo.toml (got '$cargo_version')"

ref="${GITHUB_REF:-}"
case "$ref" in
  refs/tags/v*)
    version="${ref#refs/tags/v}"
    # Compared as text with a version already held to x.y.z, so nothing
    # else of the tag can reach a file name, a URL or an Info.plist.
    if [[ "$version" != "$cargo_version" ]]; then
      fail "tag 'v$version' is not v$cargo_version, the version in Cargo.toml. A release's tag is v<x.y.z> with no pre-release part; tag v$cargo_version, or bump [workspace.package].version first (docs/RELEASING.md)."
    fi
    # Only the push of a tag publishes. A manual run can be started on a
    # tag as well as on a branch, and must stay a rehearsal either way.
    if [ "${GITHUB_EVENT_NAME:-}" = push ]; then
      publish=true
    else
      publish=false
    fi
    ;;
  *)
    sha="${GITHUB_SHA:-}"
    [[ "$sha" =~ ^[0-9a-f]{7,}$ ]] || fail "GITHUB_SHA is not a commit id (got '$sha')"
    version="$cargo_version-dev.${sha:0:7}"
    publish=false
    ;;
esac

echo "version=$version"
echo "publish=$publish"
