#!/usr/bin/env bash
# Decide what the release workflow builds, from the ref that started it.
#
# Prints three `key=value` lines, which the workflow appends to
# `$GITHUB_OUTPUT`:
#
#   version     the version the build shows and the archives are named
#               for, e.g. `0.1.0-alpha.2`
#   prerelease  `true` when the version has a pre-release part
#   publish     `true` for a pushed tag, `false` for a manual run (which
#               builds and packages everything and publishes nothing,
#               also when it is started on a tag)
#
# A tag must be `v<version>` or `v<version>-<pre-release>`, where
# <version> is `[workspace.package].version` in Cargo.toml. So with
# Cargo.toml at 0.1.0, `v0.1.0`, `v0.1.0-alpha.1` and `v0.1.0-rc.2` are
# releases, and `v0.2.0` is refused until Cargo.toml says 0.2.0.
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
    # The version ends up in file names, a URL query and an Info.plist,
    # so the pre-release part is held to dot-separated runs of letters,
    # digits and hyphens: semver's own alphabet, without `+build`.
    [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$ ]] ||
      fail "tag 'v$version' is not v<x.y.z> or v<x.y.z>-<pre-release>"
    if [[ "$version" != "$cargo_version" && "$version" != "$cargo_version"-* ]]; then
      fail "tag 'v$version' does not match Cargo.toml's version $cargo_version. Tag v$cargo_version or v$cargo_version-<pre-release>, or bump [workspace.package].version first (docs/RELEASING.md)."
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

if [[ "$version" == *-* ]]; then
  prerelease=true
else
  prerelease=false
fi

echo "version=$version"
echo "prerelease=$prerelease"
echo "publish=$publish"
