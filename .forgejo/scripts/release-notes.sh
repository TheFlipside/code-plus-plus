#!/usr/bin/env bash
# Write a release's description, in Markdown, to stdout.
#
#   release-notes.sh <tag> <version> <prerelease>
#
# e.g.  release-notes.sh v0.1.0-alpha.2 0.1.0-alpha.2 true
#
# The same text goes on the Forgejo release and the GitHub one. It says
# which files are for which platform, how to install or start an
# unsigned build on each, and what changed since the release before: the
# commit subjects back to the nearest earlier `v` tag. A first release
# lists no changes.
#
# Needs the repository's full history and tags (`fetch-depth: 0`).

set -euo pipefail

[ "$#" -eq 3 ] || {
  echo "usage: release-notes.sh <tag> <version> <prerelease>" >&2
  exit 2
}
tag="$1"
version="$2"
prerelease="$3"

if [ "$prerelease" = true ]; then
  cat <<EOF
This is a pre-release build of Code++ for testers. Expect rough edges, and keep copies of files you care about.

EOF
fi

cat <<EOF
## Downloads

| Platform | To install | To unpack and run |
| --- | --- | --- |
| Windows 10 and 11, 64-bit | \`codepp-windows-x64-$version.msi\` | \`codepp-windows-x64-$version.zip\` |
| Linux, x86-64, GTK 3 | \`codepp-linux-amd64-$version.deb\` (Debian, Ubuntu) | \`codepp-linux-amd64-$version.tar.gz\` |
| macOS, Apple Silicon | \`codepp-macos-arm64-$version.dmg\` | \`codepp-macos-arm64-$version.zip\` |
| macOS, Intel | \`codepp-macos-x64-$version.dmg\` | \`codepp-macos-x64-$version.zip\` |

Each pair holds the same build. \`SHA256SUMS\` holds each file's checksum.

## Installing it

None of the builds is code-signed yet, so each system asks before the first start.

- **Windows:** run the \`.msi\`. It installs Code++ into Program Files and adds it to the Start Menu and to "Open with" for text files; a later version's installer replaces it, and Settings → Apps removes it. If SmartScreen says "Windows protected your PC", choose "More info", then "Run anyway". Without administrator rights, \`msiexec /i codepp-windows-x64-$version.msi MSIINSTALLPERUSER=1 ALLUSERS=2\` installs it for your account only. Or extract the zip anywhere and run \`codepp.exe\`.
- **Linux:** on Debian and Ubuntu, \`sudo apt install ./codepp-linux-amd64-$version.deb\` installs Code++ with the libraries it needs and adds it to the applications menu; \`sudo apt remove codepp\` removes it. If apt refuses the package, the system is older than the one it was built on. Elsewhere, extract the \`.tar.gz\` and run \`./codepp\` from its folder; GTK 3 must be installed (\`gtk3\` on Fedora and Arch).
- **macOS:** open the \`.dmg\` and drag Code++ to Applications, or unzip and move \`Code++.app\` there. On the first start macOS refuses to open it: open System Settings → Privacy & Security and choose "Open Anyway", or run \`xattr -dr com.apple.quarantine /Applications/Code++.app\` once.

Your settings are kept apart from the program on every system, so installing a newer build or removing Code++ leaves them. The three bundled plugins are installed into your Code++ settings folder on first start. About shows the version, $version; please name it when you report a problem.
EOF

# The nearest other `v` tag at or before the commit being released, if
# there is one. The workflow has that commit checked out, so it is HEAD,
# whether or not the checkout made a local tag for it.
# This release's own tag is left out by name rather than by starting at
# HEAD^, so that an earlier tag on the same commit still counts: v0.1.0
# tagged on the commit that was v0.1.0-rc.2 has nothing new to list.
previous=$(git describe --tags --abbrev=0 --match 'v[0-9]*' --exclude "$tag" HEAD 2>/dev/null || true)
if [ -n "$previous" ]; then
  echo
  echo "## Changes from $previous to $tag"
  echo
  changes=$(git log --no-merges --format='- %s (%h)' "$previous..HEAD")
  if [ -n "$changes" ]; then
    printf '%s\n' "$changes"
  else
    echo "None: this is the same commit as $previous."
  fi
fi
