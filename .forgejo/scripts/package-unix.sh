#!/usr/bin/env bash
# Package a release build for Linux or macOS.
#
#   package-unix.sh <target-triple> <label> <crate-notices> <out-dir>
#
# e.g.  package-unix.sh x86_64-unknown-linux-gnu linux-amd64 \
#           release-notices/THIRD_PARTY_CRATES.md dist-release
#
# Reads the binaries `cargo build --release --target <target-triple>`
# left in `target/<target-triple>/release` and writes two files into
# <out-dir>, an archive to unpack and a package to install:
#
#   Linux   codepp-<label>-<version>.tar.gz, holding one folder with the
#           `codepp` binary, the bundled plugins and the licence files
#           codepp-<label>-<version>.deb, which installs the same files
#           under /usr, with an applications-menu entry and icons
#   macOS   codepp-<label>-<version>.zip, holding `Code++.app`
#           codepp-<label>-<version>.dmg, a disk image holding the same
#           `Code++.app` beside a link to /Applications
#
# The plugins sit beside the executable in all four, which is where
# `codepp_platform::stage_bundled_plugins` looks for them on first start.
#
# The version comes from CODEPP_RELEASE_VERSION, which the workflow also
# gives the build, so the files' names and the About dialog agree.
# Run from the repository root. Written for bash 3.2, which is what
# macOS ships.
#
# The .deb needs `dpkg-deb` and `dpkg-shlibdeps` (Debian's `dpkg` and
# `dpkg-dev` packages) on a Debian or Ubuntu system, whose package
# database says which packages the binaries' libraries come from. The
# .dmg needs `hdiutil`, which macOS ships.

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
# The workflow has checked the version already (release-version.sh).
# Checked again, because it goes into file names, a control file and an
# Info.plist, and this script can be run by hand: it must begin with a
# digit and hold nothing but ASCII letters, digits, dots and hyphens.
# In a subshell under the C locale, because in other locales a range
# such as `a-z` can take letters outside ASCII.
if ! (
  LC_ALL=C
  case "$version" in
    [!0-9]* | *[!0-9A-Za-z.-]* | "") exit 1 ;;
  esac
); then
  fail "CODEPP_RELEASE_VERSION is not a version (got '$version')"
fi

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
    # What the .deb needs, checked before anything is written, so that
    # a runner without it fails without leaving a tarball behind that
    # looks like a finished job.
    case "$target" in
      x86_64-*) deb_arch=amd64 ;;
      aarch64-*) deb_arch=arm64 ;;
      *) fail "no Debian architecture is known for $target" ;;
    esac
    for tool in dpkg-deb dpkg-shlibdeps; do
      command -v "$tool" >/dev/null ||
        fail "$tool is not installed; the .deb needs Debian's dpkg and dpkg-dev packages (docs/RELEASING.md)"
    done
    desktop="packaging/linux/codepp.desktop"
    [ -f "$desktop" ] || fail "no desktop entry at $desktop"
    icon_sizes="16 24 32 48 64 128 256 512"
    for size in $icon_sizes; do
      [ -f "assets/icons-app/codepp-$size.png" ] ||
        fail "no icon at assets/icons-app/codepp-$size.png (tools/codepp-app-icon/generate.py writes it)"
    done

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

    # The Debian package.
    #
    # dpkg orders versions by its own rules, under which `0.1.0-alpha.2`
    # is a later version than `0.1.0`: it reads the hyphen as the start
    # of a package revision. A tilde sorts before everything, the end of
    # the version included, so `0.1.0~alpha.2` comes before `0.1.0`, as
    # a pre-release must for the release to upgrade it. A hyphen further
    # on would be read as a revision again, so those become dots.
    upstream="${version%%-*}"
    if [ "$upstream" = "$version" ]; then
      deb_version="$version"
    else
      deb_version="$upstream~$(printf '%s' "${version#*-}" | tr - .)"
    fi

    # The modes of what dpkg-deb packs are the modes of the files here,
    # so they must not depend on the runner's umask.
    umask 022
    root="$work/deb"
    lib_dir="$root/usr/lib/codepp"
    doc_dir="$root/usr/share/doc/codepp"
    mkdir -p "$root/DEBIAN" "$root/usr/bin" "$lib_dir" "$doc_dir" \
      "$root/usr/share/applications"

    # The binary and the plugins stay together, in a folder of their
    # own, because the plugins are found beside the executable. The
    # command on PATH is a link to it: Code++ finds its folder through
    # /proc/self/exe, which names the file the link points at.
    install -m 755 "$bin_dir/codepp" "$lib_dir/codepp"
    for plugin in $plugins; do
      install -m 644 "$bin_dir/lib$plugin.so" "$lib_dir/lib$plugin.so"
    done
    ln -s ../lib/codepp/codepp "$root/usr/bin/codepp"

    # `sed` drops the comment lines, which are for whoever reads the
    # file in the repository.
    sed '/^#/d' "$desktop" > "$root/usr/share/applications/codepp.desktop"
    chmod 644 "$root/usr/share/applications/codepp.desktop"
    for size in $icon_sizes; do
      icon_dir="$root/usr/share/icons/hicolor/${size}x${size}/apps"
      mkdir -p "$icon_dir"
      install -m 644 "assets/icons-app/codepp-$size.png" "$icon_dir/codepp.png"
    done

    # /usr/share/doc/<package>/copyright is where Debian looks for a
    # package's licence.
    {
      echo "Code++ $version"
      echo "Source: https://git.fiedler.live/tux/code-plus-plus"
      echo
      echo "The licences of the components built into the binary are in"
      echo "THIRD_PARTY_LICENSES.md and THIRD_PARTY_CRATES.md, beside this file."
      echo
      cat LICENSE
    } > "$doc_dir/copyright"
    chmod 644 "$doc_dir/copyright"
    install -m 644 THIRD_PARTY_LICENSES.md "$doc_dir/THIRD_PARTY_LICENSES.md"
    install -m 644 "$notices" "$doc_dir/THIRD_PARTY_CRATES.md"

    # The packages the binaries need, with the oldest version of each
    # that has the symbols they use, as dpkg-shlibdeps works them out
    # from this system's package database. So the package refuses to
    # install where the binary could not start, on a system with an
    # older glibc or GTK than the runner's, rather than installing and
    # failing. The tool wants a source package's control file in its
    # working directory, so it gets a minimal one in a folder of its own.
    mkdir -p "$work/shlibdeps/debian"
    printf 'Source: codepp\n\nPackage: codepp\nArchitecture: %s\n' "$deb_arch" \
      > "$work/shlibdeps/debian/control"
    depends=$(cd "$work/shlibdeps" && dpkg-shlibdeps -O "$lib_dir/codepp" "$lib_dir"/lib*.so)
    depends="${depends#shlibs:Depends=}"
    for needed in libc6 libgtk-3-0; do
      case "$depends" in
        *"$needed"*) ;;
        *) fail "dpkg-shlibdeps did not name the package of $needed (got '$depends')" ;;
      esac
    done

    # A folder made below a set-group-ID folder inherits that bit,
    # whatever the umask, and the system's temporary folder may be one.
    # (`ug-s` by name: a numeric mode leaves those bits on a folder.)
    find "$root" -type d -exec chmod u=rwx,go=rx,ug-s {} +

    installed_kib=$(du -sk "$root/usr" | cut -f1)
    cat > "$root/DEBIAN/control" <<EOF
Package: codepp
Version: $deb_version
Architecture: $deb_arch
Maintainer: Max Fiedler <max.fiedler1988@pm.me>
Installed-Size: $installed_kib
Depends: $depends
Section: editors
Priority: optional
Homepage: https://git.fiedler.live/tux/code-plus-plus
Description: text and source code editor in the style of Notepad++
 Code++ is a fast, native editor for text and source code: tabs,
 sessions, syntax highlighting, find and replace in files, control over
 encodings and line endings, and plugins.
 .
 This is the Code++ $version build.
EOF

    package="$out_dir/$name.deb"
    rm -f "$package"
    # `--root-owner-group`: every file belongs to root, whoever runs
    # this. `-Zxz`: Ubuntu's dpkg-deb compresses with zstd by default,
    # which Debian's dpkg before 1.21.18 (Debian 11) cannot unpack.
    dpkg-deb --root-owner-group -Zxz --build "$root" "$package"

    # Read the package back: that it carries the version it was given,
    # that every file in it belongs to root with one of the four modes
    # a package of this kind has, and that it holds the files that make
    # it a desktop application. A listing's line is the mode, the owner,
    # the size, the date, the time and the path. A package that fails
    # this is deleted, so that it cannot be picked up from the output
    # folder as if it had passed.
    refuse() {
      rm -f "$package"
      fail "$@"
    }
    [ "$(dpkg-deb --field "$package" Version)" = "$deb_version" ] ||
      refuse "the package does not carry the version $deb_version"
    listing=$(dpkg-deb --contents "$package")
    odd=$(printf '%s\n' "$listing" | awk '$2 != "root/root" ||
      ($1 != "drwxr-xr-x" && $1 != "-rw-r--r--" && $1 != "-rwxr-xr-x" && $1 != "lrwxrwxrwx")')
    [ -z "$odd" ] || refuse "the package holds entries with an unexpected owner or mode: $odd"
    # Nothing in the package runs when it is installed or removed: its
    # control part holds the control file and no script.
    scripts=$(dpkg-deb --ctrl-tarfile "$package" | tar -t | grep -vxF -e ./ -e ./control || true)
    [ -z "$scripts" ] || refuse "the package's control part holds more than its control file: $scripts"
    paths=$(printf '%s\n' "$listing" | awk '{ print $6 }')
    wanted="./usr/bin/codepp ./usr/lib/codepp/codepp"
    for plugin in $plugins; do
      wanted="$wanted ./usr/lib/codepp/lib$plugin.so"
    done
    wanted="$wanted ./usr/share/applications/codepp.desktop"
    wanted="$wanted ./usr/share/icons/hicolor/48x48/apps/codepp.png"
    wanted="$wanted ./usr/share/doc/codepp/copyright"
    for path in $wanted; do
      # `-x`: the whole line, so that no longer path with this one in
      # it passes. Without `-q`: grep then reads its input to the end,
      # and cannot close the pipe on a `printf` that is still writing.
      printf '%s\n' "$paths" | grep -xF -- "$path" >/dev/null ||
        refuse "the package has no $path"
    done
    dpkg-deb --info "$package"
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

    # The disk image: the same signed bundle beside a link to
    # /Applications, so that installing is dragging one onto the other.
    # The bundle is moved, not copied, so that nothing about it changes
    # after it was signed.
    image_root="$work/dmg"
    mkdir "$image_root"
    mv "$app" "$image_root/Code++.app"
    ln -s /Applications "$image_root/Applications"
    package="$out_dir/$name.dmg"
    rm -f "$package"
    # HFS+, which every macOS this build runs on (11 and later) mounts.
    # Tried up to three times: hdiutil is reported to fail now and then
    # with "Resource busy" on build machines, while something else is
    # still looking at the folder. Not seen here: when this was written
    # it had not yet run on a Mac.
    attempt=1
    until hdiutil create -volname "Code++" -srcfolder "$image_root" \
      -fs HFS+ -format UDZO -ov "$package"; do
      [ "$attempt" -lt 3 ] || fail "hdiutil could not create $package"
      attempt=$((attempt + 1))
      sleep 5
    done
    hdiutil verify "$package"
    ;;

  *)
    fail "package-unix.sh does not package $target"
    ;;
esac

echo "packaged $archive and $package"
ls -l "$archive" "$package"
