#!/usr/bin/env bash
# Package a release build for Linux or macOS.
#
#   package-unix.sh <target-triple> <label> <crate-notices> <out-dir>
#
# e.g.  package-unix.sh x86_64-unknown-linux-gnu linux-amd64 \
#           release-notices/THIRD_PARTY_CRATES.md dist-release
#
# Reads the binaries `cargo build --release --target <target-triple>`
# left in `target/<target-triple>/release` and writes one archive into
# <out-dir>:
#
#   Linux   codepp-<label>-<version>.tar.gz, holding one folder with the
#           `codepp` binary, the bundled plugins and the licence files
#   macOS   codepp-<label>-<version>.zip, holding `Code++.app`
#
# The plugins sit beside the executable in both, which is where
# `codepp_platform::stage_bundled_plugins` looks for them on first start.
#
# The version comes from CODEPP_RELEASE_VERSION, which the workflow also
# gives the build, so the archive's name and the About dialog agree.
# Run from the repository root. Written for bash 3.2, which is what
# macOS ships.

set -euo pipefail

fail() {
  echo "::error::$*" >&2
  exit 1
}

[ "$#" -eq 4 ] || fail "usage: package-unix.sh <target-triple> <label> <crate-notices> <out-dir>"
target="$1"
label="$2"
notices="$3"
out_dir="$4"
version="${CODEPP_RELEASE_VERSION:?CODEPP_RELEASE_VERSION must name the version being packaged}"

# The preinstalled plugins (DESIGN.md §6.6). `example_hello` is the SDK
# sample and is left out of a release; staging skips a plugin that is
# not beside the executable.
plugins="cppmimetools cppconverter cppexport"

bin_dir="target/$target/release"
name="codepp-$label-$version"

[ -f "$bin_dir/codepp" ] || fail "no release binary at $bin_dir/codepp"
[ -s "$notices" ] || fail "the crate licence notices are missing or empty: $notices"

mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# `install`, so that the modes in the archive do not depend on how the
# checkout or the downloaded notices happen to be stored.
copy_licences() {
  install -m 644 LICENSE "$1/LICENSE"
  install -m 644 THIRD_PARTY_LICENSES.md "$1/THIRD_PARTY_LICENSES.md"
  install -m 644 "$notices" "$1/THIRD_PARTY_CRATES.md"
}

case "$target" in
  *-linux-*)
    stage="$work/$name"
    mkdir -p "$stage"
    install -m 755 "$bin_dir/codepp" "$stage/codepp"
    for plugin in $plugins; do
      lib="$bin_dir/lib$plugin.so"
      [ -f "$lib" ] || fail "no plugin library at $lib"
      install -m 644 "$lib" "$stage/lib$plugin.so"
    done
    copy_licences "$stage"
    archive="$out_dir/$name.tar.gz"
    tar -C "$work" --owner=0 --group=0 --numeric-owner -czf "$archive" "$name"
    ;;

  *-apple-darwin)
    case "$target" in
      aarch64-*) arch=arm64 ;;
      x86_64-*) arch=x86_64 ;;
      *) fail "unknown macOS target: $target" ;;
    esac
    # A build that ignored `--target` would otherwise be packaged under
    # the other architecture's name.
    built=$(lipo -archs "$bin_dir/codepp")
    [ "$built" = "$arch" ] || fail "$bin_dir/codepp is '$built', expected '$arch'"

    icon="assets/code++.icns"
    [ -f "$icon" ] || fail "no application icon at $icon (tools/codepp-app-icon/generate.py writes it)"

    app="$work/Code++.app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    install -m 755 "$bin_dir/codepp" "$app/Contents/MacOS/codepp"
    for plugin in $plugins; do
      lib="$bin_dir/lib$plugin.dylib"
      [ -f "$lib" ] || fail "no plugin library at $lib"
      install -m 644 "$lib" "$app/Contents/MacOS/lib$plugin.dylib"
    done
    install -m 644 "$icon" "$app/Contents/Resources/codepp.icns"
    copy_licences "$app/Contents/Resources"

    # CFBundleVersion takes numbers and dots only, so it gets the
    # version without its pre-release part.
    cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>Code++</string>
    <key>CFBundleExecutable</key>
    <string>codepp</string>
    <key>CFBundleIconFile</key>
    <string>codepp</string>
    <key>CFBundleIdentifier</key>
    <string>org.code-plus-plus.codepp</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Code++</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>${version%%-*}</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>Copyright © Max Fiedler and Code++ contributors. MIT License.</string>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
EOF
    plutil -lint "$app/Contents/Info.plist"

    # Ad-hoc signatures, inside out: no certificate is involved, and
    # Gatekeeper still asks the tester to approve the first launch. What
    # they buy is a sealed bundle, and on Apple Silicon the signature
    # every binary needs to run at all. No hardened runtime: with it the
    # app could not load a plugin somebody else built.
    for lib in "$app"/Contents/MacOS/*.dylib; do
      codesign --force --sign - "$lib"
    done
    codesign --force --sign - "$app"
    codesign --verify --strict "$app"

    archive="$out_dir/$name.zip"
    rm -f "$archive"
    # `ditto`, not `zip`: it keeps the bundle's symlinks, modes and
    # extended attributes as Finder's own Compress does.
    (cd "$work" && ditto -c -k --sequesterRsrc --keepParent "Code++.app" "$archive")
    ;;

  *)
    fail "package-unix.sh does not package $target"
    ;;
esac

echo "packaged $archive"
ls -l "$archive"
