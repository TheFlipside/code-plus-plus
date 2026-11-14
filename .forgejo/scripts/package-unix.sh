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
#
# The macOS bundle updates itself (docs/DESIGN.md, section 11) when the
# environment names its feed and the key its updates are signed with,
# as the release workflow does:
#
#   CODEPP_UPDATE_FEED_URL    where the installed app asks for the newest
#                             version; it must end in /<label>.xml
#   CODEPP_UPDATE_PUBLIC_KEY  the Ed25519 public key, 32 bytes in base64
#
# The bundle then carries Sparkle.framework, downloaded here at the
# version and hash this script holds, and its Info.plist the settings
# Sparkle reads. With neither set the bundle has no updater, and
# nothing is downloaded.

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
    # A release's version is plain. A manual run's build has a part
    # after it, `0.2.0-dev.2f20913`, and dpkg orders versions by its own
    # rules, under which that is a later version than `0.2.0`: it reads
    # the hyphen as the start of a package revision. A tilde sorts
    # before everything, the end of the version included, so
    # `0.2.0~dev.2f20913` comes before `0.2.0`, as such a build must for
    # the release to upgrade it. A hyphen further on would be read as a
    # revision again, so those become dots.
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

    # The updater's library and settings, when the build has a feed.
    # Downloaded and checked before anything is put together, so that a
    # download that fails or arrives changed leaves nothing behind.
    #
    # Sparkle's version and the SHA-256 of its release archive, which is
    # the digest GitHub shows for the file. To update: change both, and
    # read docs/DESIGN.md section 11.11 for what was measured of this one.
    sparkle_version=2.10.1
    sparkle_sha256=1a856d2a9edb8e628de88121d7be04cf0e2167f8641650b78fca00b14244cf27
    feed_url="${CODEPP_UPDATE_FEED_URL:-}"
    public_key="${CODEPP_UPDATE_PUBLIC_KEY:-}"
    sparkle=""
    updater_keys=""
    if [ -n "$feed_url" ] || [ -n "$public_key" ]; then
      [ -n "$feed_url" ] && [ -n "$public_key" ] ||
        fail "CODEPP_UPDATE_FEED_URL and CODEPP_UPDATE_PUBLIC_KEY go together; only one is set"
      # Both go into the Info.plist as they are, so neither may hold a
      # character XML gives a meaning to. The address is https, or this
      # machine itself for a test of the updater, and names this
      # architecture's feed: the two macOS builds must not ask for each
      # other's.
      if ! (
        LC_ALL=C
        case "$feed_url" in *[!A-Za-z0-9:/._~%+-]*) exit 1 ;; esac
        case "$feed_url" in https://?* | http://127.0.0.1:?*) ;; *) exit 1 ;; esac
        case "$feed_url" in */"$label.xml") ;; *) exit 1 ;; esac
        case "$public_key" in *[!A-Za-z0-9+/=]*) exit 1 ;; esac
        [ "${#public_key}" -eq 44 ]
      ); then
        fail "CODEPP_UPDATE_FEED_URL must be an https address ending in /$label.xml, and CODEPP_UPDATE_PUBLIC_KEY 32 bytes in base64"
      fi
      sparkle_archive="$work/Sparkle-$sparkle_version.tar.xz"
      curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
        --max-time 300 --output "$sparkle_archive" \
        "https://github.com/sparkle-project/Sparkle/releases/download/$sparkle_version/Sparkle-$sparkle_version.tar.xz" ||
        fail "Sparkle $sparkle_version could not be downloaded"
      got=$(shasum -a 256 "$sparkle_archive" | cut -d ' ' -f 1)
      [ "$got" = "$sparkle_sha256" ] ||
        fail "Sparkle-$sparkle_version.tar.xz has SHA-256 $got, expected $sparkle_sha256; it is not unpacked"
      # Only the framework is taken out of the archive.
      mkdir "$work/sparkle"
      tar -xJf "$sparkle_archive" -C "$work/sparkle" ./Sparkle.framework ||
        fail "the Sparkle archive could not be unpacked"
      sparkle="$work/sparkle/Sparkle.framework"
      [ -f "$sparkle/Versions/B/Sparkle" ] || fail "the Sparkle archive holds no framework binary"
      # The framework comes signed ad hoc by its project and goes into
      # the bundle as it is; this says the archive unpacked whole.
      codesign --verify --strict --deep "$sparkle" ||
        fail "Sparkle.framework does not verify as it was unpacked"
      # What Sparkle reads (docs/DESIGN.md, section 11.8). Its own
      # schedule is off: Code++ decides when a check is made. An update
      # is verified before it is unpacked, the feed must be signed, and
      # a feed that fails that is refused for good, where Sparkle's
      # default starts accepting it after twenty days. Nothing is
      # installed without a click, and no feed text is shown in a web
      # view.
      updater_keys="    <key>SUFeedURL</key>
    <string>$feed_url</string>
    <key>SUPublicEDKey</key>
    <string>$public_key</string>
    <key>SUEnableAutomaticChecks</key>
    <false/>
    <key>SUAllowsAutomaticUpdates</key>
    <false/>
    <key>SURequireSignedFeed</key>
    <true/>
    <key>SUVerifyUpdateBeforeExtraction</key>
    <true/>
    <key>SUSignedFeedFailureExpirationInterval</key>
    <integer>0</integer>
    <key>SUShowReleaseNotes</key>
    <false/>
"
    fi

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
    if [ -n "$sparkle" ]; then
      # `ditto`, which keeps the framework's symlinks and modes.
      mkdir "$app/Contents/Frameworks"
      ditto "$sparkle" "$app/Contents/Frameworks/Sparkle.framework"
    fi

    # CFBundleVersion takes numbers and dots only, so it gets the
    # version without the part a manual run's build has after x.y.z.
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
$updater_keys</dict>
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
    codesign --verify --strict --deep "$app"
    if [ -n "$sparkle" ]; then
      # Read back: the bundle asks the feed it was packaged for with the
      # key it was given, and holds the framework as it was downloaded.
      [ "$(plutil -extract SUFeedURL raw "$app/Contents/Info.plist")" = "$feed_url" ] ||
        fail "the bundle's Info.plist does not name the feed"
      [ "$(plutil -extract SUPublicEDKey raw "$app/Contents/Info.plist")" = "$public_key" ] ||
        fail "the bundle's Info.plist does not hold the update key"
      # Links compared as links: followed, the framework's own
      # (`Versions/Current`) read to `diff` as a folder inside itself.
      diff -r --no-dereference "$sparkle" "$app/Contents/Frameworks/Sparkle.framework" >/dev/null ||
        fail "the bundle's Sparkle.framework is not the one that was downloaded, or this diff could not compare the two"
    fi

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
