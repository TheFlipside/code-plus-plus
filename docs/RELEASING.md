# Code++ — Releasing

A release is made by pushing a version tag. `.forgejo/workflows/release.yml` builds Code++ on the three runners, packages it, and publishes the result on Forgejo and on the GitHub mirror. Nothing is built or uploaded by hand.

---

## 1. What a release contains

Each platform gets an archive to unpack and a package to install, both made from the same files:

| File | Platform | Contents |
| --- | --- | --- |
| `codepp-windows-x64-<version>.zip` | Windows 10 and 11, 64-bit | `codepp.exe`, the three plugin DLLs, the licence files |
| `codepp-windows-x64-<version>.msi` | Windows 10 and 11, 64-bit | an installer for the zip's files (§1.1) |
| `codepp-linux-amd64-<version>.tar.gz` | Linux, x86-64, GTK 3 | one folder with `codepp`, the three plugin libraries, the licence files |
| `codepp-linux-amd64-<version>.deb` | Debian and Ubuntu, x86-64 | a package of the tarball's files (§1.2) |
| `codepp-macos-arm64-<version>.zip` | macOS, Apple Silicon | `Code++.app` |
| `codepp-macos-arm64-<version>.dmg` | macOS, Apple Silicon | a disk image with the same `Code++.app` (§1.3) |
| `codepp-macos-x64-<version>.zip` | macOS, Intel | `Code++.app` |
| `codepp-macos-x64-<version>.dmg` | macOS, Intel | a disk image with the same `Code++.app` (§1.3) |
| `SHA256SUMS` | | the checksum of each of the eight |

The plugins are the three preinstalled ones (DESIGN.md §6.6): `cppmimetools`, `cppconverter` and `cppexport`. They sit beside the executable, and Code++ copies them into the user's plugins folder on first start (`codepp_platform::stage_bundled_plugins`). `example-hello`, the SDK sample, is not shipped.

Every archive and package carries `LICENSE` (`LICENSE.txt` on Windows, `copyright` in the `.deb`), `THIRD_PARTY_LICENSES.md`, and `THIRD_PARTY_CRATES.md`, the licence notices of the Rust crates in the binary. The last is generated during the release by `cargo about` from `about.toml` and `about.hbs`, for all four targets at once, so all eight files carry the same one.

The same eight files and `SHA256SUMS` are attached to both releases:

- Forgejo: <https://git.fiedler.live/tux/code-plus-plus/releases>
- GitHub: <https://github.com/TheFlipside/code-plus-plus/releases>

### 1.1 The Windows installer

Built by `.forgejo/scripts/package-windows.ps1` from `packaging/windows/codepp.wxs` with WiX, from the folder of files the zip is made from.

- **Where.** `C:\Program Files\Code++`, for every account of the machine, so Windows asks for administrator rights once. The installer has one page, the licence, and an Install button. It does not ask for the folder, because nothing would remember the answer for the next version's installer. `INSTALLFOLDER=<folder>` on `msiexec`'s command line puts Code++ elsewhere, and must be given again at each upgrade. Whoever does that for the whole machine should choose a folder that only administrators can write to: the Start Menu entry, "Edit with Code++" and "Open with" of every account will start what is in it.
- **What else.** A Start Menu entry. `codepp` as a name Win+R and `start` find. "Edit with Code++" in Explorer's context menu for every file, of any type or none, for one file or many. Code++ in Explorer's "Open with" menu for 34 text and source file types, which the `.wxs` lists; it becomes the default for none of them.
- **Where "Edit with Code++" shows.** On Windows 10, in the context menu. On Windows 11, in the menu behind "Show more options", which Shift+right-click opens directly; the short menu Windows 11 shows first takes entries only from a signed app package, which Code++ does not have (DESIGN.md §7.4, the entry on "Edit with Code++"). The zip sets up none of this.
- **Upgrades.** Installing a build replaces the one that is installed. Windows Installer versions are numbers only, so every pre-release of `0.1.0` is version `0.1.0` to it, and the installer lets a build replace another of the same `x.y.z`: `0.1.0-alpha.2` replaces `0.1.0-alpha.1`, and the other way round. So does a pre-release over the release it led to: `0.1.0-alpha.2` installed over `0.1.0` replaces it, without a word. It refuses to replace a newer `x.y.z` with an older one.
- **The name.** A pre-release is listed among the installed apps as "Code++ 0.1.0-alpha.2", since its version column can only say 0.1.0. A release is listed as "Code++".
- **Removal.** Settings → Apps. It removes what the installer put there and leaves the user's settings in `%APPDATA%\Code++`.
- **Without administrator rights.** `msiexec /i <file> MSIINSTALLPERUSER=1 ALLUSERS=2` installs for the current account alone, into `%LOCALAPPDATA%\Programs\Code++`, with the same Start Menu entry, "Edit with Code++" and "Open with" entries under that account. An installation of one kind does not see one of the other: installing for the machine does not replace a per-account installation.

The Start Menu entry is a plain shortcut. An advertised one would have Windows Installer check the installation at every start, before the editor's first instruction runs, which the cold-start budget has no room for (DESIGN.md §8).

### 1.2 The Debian package

Built by `.forgejo/scripts/package-unix.sh` with `dpkg-deb`.

| Path | What |
| --- | --- |
| `/usr/lib/codepp/` | `codepp` and the three plugin libraries, together, because the plugins are found beside the executable |
| `/usr/bin/codepp` | a link to the binary |
| `/usr/share/applications/codepp.desktop` | the applications-menu entry, from `packaging/linux/codepp.desktop`; it offers Code++ in "Open With" for text files |
| `/usr/share/icons/hicolor/<size>/apps/codepp.png` | the icon at eight sizes, 16 to 512, from `assets/icons-app/` |
| `/usr/share/doc/codepp/` | `copyright` and the two third-party notices |

- **Dependencies.** `dpkg-shlibdeps` works them out on the runner: the package of each library the binaries load, with the oldest version that has the symbols they use. So the package names and versions are those of the `linux` runner's distribution, and apt refuses the package on an older system, where the binary could not start, rather than installing it.
- **The version.** dpkg orders versions by its own rules, under which `0.1.0-alpha.2` would be newer than `0.1.0`. The package's version therefore has a tilde where the tag has its first hyphen, `0.1.0~alpha.2`, which sorts before `0.1.0`. The file keeps the tag's version in its name, like the other seven.
- **No scripts.** The package runs nothing at install or removal. The menu and the icon cache are refreshed by the triggers of the packages that own them.
- **The window icon.** Code++ asks GTK for the icon named `codepp`, so its windows show it once the package has installed it. Run from the tarball, they show the desktop's default icon.

### 1.3 The macOS disk image

Built by `.forgejo/scripts/package-unix.sh` with `hdiutil`: the same signed `Code++.app` as in the zip, beside a link to `/Applications`, so that installing is dragging one onto the other.

---

## 2. Cutting a release

1. **Pick a commit on `main` whose CI run is green.** The release workflow builds and packages; it does not run the tests again. It does check that `main` contains the tagged commit, and refuses a tag anywhere else.

2. **Choose the tag.** It is `v` plus the version in `Cargo.toml` (`[workspace.package].version`), optionally followed by a pre-release part:

   | `Cargo.toml` | Tag | Published as |
   | --- | --- | --- |
   | `0.1.0` | `v0.1.0-alpha.1`, `v0.1.0-alpha.2`, `v0.1.0-rc.1` | pre-release |
   | `0.1.0` | `v0.1.0` | release |
   | `0.1.0` | `v0.2.0` | refused: bump `Cargo.toml` first |

   Builds for testers are the pre-release tags. They need no change to `Cargo.toml`: tag the next `-alpha.N` on whichever commit should go out. The About dialog of each build shows the tag's version, not `Cargo.toml`'s, so a tester's report says which build it is about.

   The pre-release part may hold letters, digits, hyphens and dots, as in semver. A `+build` suffix is refused, because the version goes into file names and URLs.

3. **Tag and push.**

   ```sh
   git tag -a v0.1.0-alpha.1 -m "Code++ 0.1.0-alpha.1"
   git push origin v0.1.0-alpha.1
   ```

4. **Watch the run** under Actions → Release. When `publish` is done, both release pages hold the files. The description lists the downloads, how to start an unsigned build on each platform, and the commits since the previous `v` tag; edit it on either page if it needs more.

After a release without a pre-release part, bump the version for the next one. `Cargo.toml` says where: `[workspace.package].version` and the `version` of every internal crate under `[workspace.dependencies]`. Build once so that `Cargo.lock` follows, and commit the three together: the release builds with `--locked` and fails on a `Cargo.lock` that is behind.

---

## 3. Trying the pipeline without releasing

Actions → Release → Run workflow starts the same jobs for a branch. It builds and packages all eight files and skips `publish`, so no tag, release or upload is made. The files are on the run's page as artifacts, named for a version such as `0.1.0-dev.2f20913`.

A manual run started on a `v` tag is a rehearsal too: it checks the tag and names the files for it, and still publishes nothing. Only the push of a tag publishes.

Do this once before the first real tag, and after any change to the workflow or the scripts under `.forgejo/scripts/`.

---

## 4. When a run fails

- **The push of the tag is rejected with "Tag … is protected".** No run starts. Your account is not on the allowlist of the `v*` tag rule (§5.4). Add your account to the allowlist, under the repository's Settings → Tags, and push the tag again.
- **`prepare` refuses the tag.** The log says why: the tag does not match `Cargo.toml`, its commit is not on `main`, or a release token is missing. Delete the tag (`git push origin :refs/tags/<tag>`, `git tag -d <tag>`), fix the cause, and tag again. Nothing was published under it, so deleting it is safe.
- **A build fails.** Nothing has been published. Fix it on `main` and tag the fixed commit with the next pre-release number. A tag that was pushed is better left alone than moved.
- **A build's Package step fails for a missing tool.** `dotnet is not on PATH` on Windows, `dpkg-shlibdeps is not installed` on Linux: the runner lacks what §5.3 lists. Install it and run the job again.
- **`publish` fails halfway.** Run the job again. It finds the release it already made and replaces the files of the same name; it does not touch a title or description that exists.
- **"GitHub mirror: tag … is not at …"** The job waited ten minutes for the tag to arrive on GitHub. The Forgejo release is already published. Sync the push mirror (repository Settings → Mirror settings → Synchronize now), then run the job again.

To withdraw a release, delete it on both pages and then delete the tag.

---

## 5. One-time setup

### 5.1 Secrets

Two secrets, under the repository's (or the owning account's) Settings → Actions → Secrets. `prepare` fails before anything is built if either is missing.

| Secret | What it is | Needs |
| --- | --- | --- |
| `RELEASE_TOKEN` | a Forgejo access token | `write:repository` |
| `GH_RELEASE_TOKEN` | a GitHub token for `TheFlipside/code-plus-plus` | a fine-grained token for that one repository with Contents, read and write |

A classic GitHub token with `public_repo` works too, but it can write to every public repository of the account, so prefer the fine-grained one.

`prepare` only checks that the two are set. The `publish` job uses them, on the `linux` runner; the build jobs never see them.

### 5.2 The push mirror

GitHub receives the tag from Forgejo's push mirror, and the GitHub release is created only once the tag is there and points at the commit that was built. Turn on "Sync when commits are pushed" for the mirror, so that the tag arrives within the ten minutes `publish` waits.

### 5.3 What the runners need

Beyond what CI already needs (DEVELOPMENT.md):

| Runner | Needs |
| --- | --- |
| `linux` | `python3` (3.8 or newer), `gh`, and network access to crates.io for the first `cargo install cargo-about`. For the `.deb`: a Debian or Ubuntu system with `dpkg-deb` and `dpkg-shlibdeps` (the `dpkg` and `dpkg-dev` packages; `build-essential` brings both) |
| `windows` | Windows PowerShell 5.1, which Windows ships. For the `.msi`: .NET, with `dotnet` on the PATH of the account the runner's jobs run as, and network access to nuget.org at every run |
| `macos` | the Xcode Command Line Tools: `codesign`, `lipo`, `plutil`, `ditto`. For the `.dmg`: `hdiutil`, which macOS ships |

The workflow adds the Rust targets itself (`rustup target add`), the second macOS architecture included.

The Windows installer is built with WiX 5, which runs on .NET. The packaging script names its version and the SHA-256 of two package files, WiX itself and its UI extension. At every run it downloads the two from nuget.org, refuses one whose hash is not the expected one, unpacks them into its temporary folder and runs WiX from there. Nothing of WiX is installed on the runner, and no copy cached there is used, so a release's installer is built by the same bytes whatever ran on the runner before. The .NET it was tried with is the SDK, version 10 (`winget install Microsoft.DotNet.SDK.10`); the runtime alone should do, and has not been tried. To move to another WiX version, follow the comment above the hashes in `.forgejo/scripts/package-windows.ps1`, then build the installer once by hand and install it: WiX 4 and later share the `.wxs` format, but nothing here has run on any version but 5.0.2.

The `.deb`'s dependencies are read from the runner's own package database (§1.2), so the distribution the `linux` runner runs decides the oldest system the package installs on.

### 5.4 Who can release, and what runs beside it

Three settings on the Forgejo side, which the workflow cannot make for itself:

- **Protect the `v*` tags** (repository Settings → Tags), and put every account that cuts releases on the rule's allowlist, your own included. Anyone who can push a tag can start a release, and the workflow that runs is the one in the tagged commit. A protected tag pattern limits that to the accounts on its allowlist. A rule with an empty allowlist lets nobody create a matching tag, the repository's owner included: the push is rejected with "Tag … is protected". The rule does not hold the accounts on the list back: one of them can still move or delete a published tag, and a moved tag and a second run would replace a published release's files. So list only the accounts that cut releases, and leave a pushed tag where it is (§4).
- **Protect `main`** (repository Settings → Branches), once anyone else has write access. A release is tagged on `main`, and the workflow that runs is the one in the tagged commit, so whoever can push to `main` decides what a release builds and what `publish` runs with the release tokens. The tag rule is only as strong as this one.
- **Do not let pull requests from outside contributors run CI unapproved.** CI builds a pull request on the same persistent `linux` runner that runs `publish`, and a build runs the pull request's build scripts. Code running there while a release is being published could read the release tokens out of the publishing process, or leave a changed `gh`, `python3` or `cargo-about` behind for the next release. If outside pull requests are to build without approval, give `publish` a runner of its own first.

---

## 6. What the workflow checks

- **The tag.** `.forgejo/scripts/release-version.sh` holds it to `v<x.y.z>[-<pre-release>]` and to `Cargo.toml`'s version, and `prepare` requires its commit to be on `main`.
- **The lockfile.** Every build is `--locked`.
- **The licences.** `cargo about generate --fail` stops on a crate whose licence is not in `about.toml`'s `accepted` list, which is `deny.toml`'s allowlist, or whose licence it cannot read.
- **Static linking on Windows.** `.cargo/config.toml` links the C and C++ runtimes statically, so that the app starts on a machine without the Visual C++ Redistributable. `.forgejo/scripts/package-windows.ps1` reads the imports of `codepp.exe` and of each plugin DLL and refuses to package one that names a runtime DLL.
- **The Windows subsystem.** `codepp.exe` is linked for the GUI subsystem, so that starting it from Explorer opens no console window beside the editor (DESIGN.md §9.1). `.forgejo/scripts/package-windows.ps1` reads the subsystem field of the executable's header and refuses to package a console-subsystem one.
- **The macOS architecture.** `.forgejo/scripts/package-unix.sh` checks each binary with `lipo` before it is named for an architecture.
- **The tool that builds the Windows installer.** `.forgejo/scripts/package-windows.ps1` uses WiX only as the two package files whose SHA-256 it holds (§5.3), and stops before writing anything if either arrives changed.
- **The Windows installer.** The same script reads the built `.msi` back through Windows Installer: that its version is the one it is named for, that it installs for the whole machine, and that it holds exactly the files the zip holds, each at the same size, so a file added to the zip and forgotten in `packaging/windows/codepp.wxs` stops the release. It also requires that the installer runs no code of its own: the only custom action in it is the one behind the licence page's Print button, which WiX's UI brings, and no custom action is in any of its sequences, the elevated part of an installation included. And it requires that the installer does nothing but what §1.1 says: it may hold only the tables WiX writes for files, a shortcut, registry values, the upgrade rule and the UI, so not those that install a service, register a COM class, set an environment variable or change permissions; and it may write only the registry keys the `.wxs` names. A deliberate change to the `.wxs` that needs more is added to the lists in the script. The zip and the installer are made in a temporary folder and moved to the output folder together, so an installer that fails leaves neither.
- **The Debian package.** `.forgejo/scripts/package-unix.sh` requires `dpkg-shlibdeps` to have named the packages of glibc and GTK 3, and reads the built `.deb` back: its version, that every entry belongs to root and has one of the four modes such a package has (so no set-group-ID folder or writable file gets in from the runner's temporary folder), that its control part holds the control file and no script, so nothing of the package's runs as root at installation or removal, and that it holds the binary, the link to it, the three plugins, the menu entry, an icon and the licence. A package that fails is deleted.
- **The version, again.** Both packaging scripts check the version they are given before using it, as `release-version.sh` did before them, since they can also be run by hand.
- **The disk image.** `hdiutil verify`, after up to three attempts at creating it.
- **The set of files.** `publish` requires exactly the eight files, by name, each a regular file.
- **The mirror's tag**, as §5.2 describes.

---

## 7. Not done yet

- **Code signing.** No build is signed with a certificate, and neither is an installer. A certificate is also the first thing "Edit with Code++" would take to show in Windows 11's short context menu (DESIGN.md §7.4, the entry on "Edit with Code++"); the maintainer has put that off to a later time. Windows shows SmartScreen's prompt, and its administrator prompt names the `.msi` as from an unknown publisher; macOS refuses the first start until the user allows it. The release description tells testers how. The macOS bundle has an ad-hoc signature only, which Apple Silicon requires of every binary; the disk image itself is not signed.
- **More packages.** No `.rpm`, AppImage or Flatpak, no package repository to add to apt, and no update check in the app.
- **More of the installers.** The Windows installer has WiX's stock pictures and English text only, offers no choice of folder or of what to set up, and does not check the Windows version: on Windows 8.1 or older it would install a program that cannot start. The `.deb` has no manual page, changelog or AppStream metadata, so a software centre shows it without a description page, and `lintian` would say so; it has not been run on it. The disk image has no background picture or arranged icons.
- **The macOS disk image on a Mac.** The `.dmg` step has run only against stand-ins for Apple's tools (DESIGN.md §9.2).
- **A glibc baseline.** The Linux build runs on systems whose glibc and GTK 3 are at least as new as the `linux` runner's.
- **Tests in the release.** The workflow trusts the CI run of the tagged commit.
