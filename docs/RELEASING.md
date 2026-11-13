# Code++ — Releasing

A release is made by pushing a version tag. `.forgejo/workflows/release.yml` builds Code++ on the three runners, packages it, publishes the result on Forgejo and on the GitHub mirror, and then offers it to the copies of Code++ that update themselves. Nothing is built or uploaded by hand.

---

## 1. What a release contains

Each platform gets an archive to unpack and a package to install, both made from the same files:

| File | Platform | Contents |
| --- | --- | --- |
| `codepp-windows-x64-<version>.zip` | Windows 10 and 11, 64-bit | `codepp.exe`, the three plugin DLLs, the licence files |
| `codepp-windows-x64-<version>.msi` | Windows 10 and 11, 64-bit | an installer for the zip's files and the updater's library, `WinSparkle.dll` (§1.1) |
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
- **Upgrades.** Installing a build replaces the one that is installed. Windows Installer versions are numbers only, so a rehearsal build `0.2.0-dev.2f20913` (§3) is version `0.2.0` to it, and the installer lets a build replace another of the same `x.y.z`: one rehearsal build replaces another, and either replaces the release `0.2.0` or is replaced by it, without a word. It refuses to replace a newer `x.y.z` with an older one.
- **The name.** A rehearsal build is listed among the installed apps as "Code++ 0.2.0-dev.2f20913", since its version column can only say 0.2.0. A release is listed as "Code++".
- **Removal.** Settings → Apps. It removes what the installer put there and leaves the user's settings in `%APPDATA%\Code++`.
- **The updater's library.** The installer puts one file beside `codepp.exe` that the zip does not have: `WinSparkle.dll`, WinSparkle 0.9.4's own x64 build, with which an installed Code++ checks for updates (DESIGN.md §11). Code++ loads it only in an installation for one account, which is the one that updates itself (§1.4, DESIGN.md §11.4); in an installation for the whole machine the file is installed and never used. `THIRD_PARTY_LICENSES.md` §1.4 has its notices and those of the libraries built into it.
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
- **The version.** A release's package has the release's version. A rehearsal build's (§3) has a hyphen in it, and dpkg orders versions by its own rules, under which `0.2.0-dev.2f20913` would be newer than `0.2.0`. Such a package's version therefore has a tilde where the build's has its first hyphen, `0.2.0~dev.2f20913`, which sorts before `0.2.0`. The file keeps the build's version in its name, like the other seven.
- **The apt repository.** `publish` also uploads the package to the apt repository Forgejo keeps for the repository's owner (`.forgejo/scripts/publish_deb.py`; DESIGN.md §11.9), distribution `stable`, component `main`. Whoever has added that repository, as the README describes, gets the version from `apt upgrade`. A package installed from the release page does not add the repository.
- **No scripts.** The package runs nothing at install or removal. The menu and the icon cache are refreshed by the triggers of the packages that own them.
- **The window icon.** Code++ asks GTK for the icon named `codepp`, so its windows show it once the package has installed it. Run from the tarball, they show the desktop's default icon.

### 1.3 The macOS disk image

Built by `.forgejo/scripts/package-unix.sh` with `hdiutil`: the same signed `Code++.app` as in the zip, beside a link to `/Applications`, so that installing is dragging one onto the other.

### 1.4 The update feed

A Code++ that the `.msi` installed for one account checks for a newer version by itself, at most once a day (DESIGN.md §11). It asks one file, `windows-x64.xml` on the repository's `appcast` branch:

<https://git.fiedler.live/tux/code-plus-plus/raw/branch/appcast/windows-x64.xml>

The file names the newest release's installer among the Forgejo release's files, its length, and its Ed25519 signature. `publish` signs the installer with the project's update key before it publishes anything (`.forgejo/scripts/update_feed.py`), and commits the feed to the branch as its last step, once both releases are out. Code++ installs nothing that signature does not verify, against the public key built into it.

That address and that public key are in `.forgejo/workflows/release.yml`, as `CODEPP_UPDATE_FEED_URL` and `CODEPP_UPDATE_PUBLIC_KEY`. Neither can change once a release with them is out: every installed copy asks that address and trusts that key for good. So the repository's owner, its name, the branch's name and the file's name stay as they are.

An installation for the whole machine does not update itself yet, and neither does a Code++ run from the zip, on Linux or on macOS (DESIGN.md §11.4, §11.1).

---

## 2. Cutting a release

1. **Pick a commit on `main` whose CI run is green.** The release workflow builds and packages; it does not run the tests again. It does check that `main` contains the tagged commit, and refuses a tag anywhere else.

2. **Set the version.** A release's version is the one in `Cargo.toml`, and every release has its own: `0.2.0`, then `0.2.1` or `0.3.0`. `Cargo.toml` says where: `[workspace.package].version` and the `version` of every internal crate under `[workspace.dependencies]`. Build once so that `Cargo.lock` follows, and commit the three together: the release builds with `--locked` and fails on a `Cargo.lock` that is behind.

3. **Tag and push.** The tag is `v` plus that version, and nothing more:

   | `Cargo.toml` | Tag | |
   | --- | --- | --- |
   | `0.2.0` | `v0.2.0` | a release |
   | `0.2.0` | `v0.2.1` | refused: bump `Cargo.toml` first |
   | `0.2.0` | `v0.2.0-rc.1`, `v0.2.0-alpha.1` | refused: a release has a plain version |

   ```sh
   git tag -a v0.2.0 -m "Code++ 0.2.0"
   git push origin v0.2.0
   ```

   A version is plain because three things compare it: an installed Code++ deciding whether an update is newer, Windows Installer, and apt (DESIGN.md §11.6). A version that was released is never built again; a fix is the next patch version. A build for testers that is not a release is a manual run's (§3).

4. **Watch the run** under Actions → Release. When `publish` is done, both release pages hold the files, and the `appcast` branch holds the feed that offers the Windows installer to installed copies (§1.4). The description lists the downloads, how to start an unsigned build on each platform, and the commits since the previous `v` tag; edit it on either page if it needs more.

---

## 3. Trying the pipeline without releasing

Actions → Release → Run workflow starts the same jobs for a branch. It builds and packages all eight files and skips `publish`, so no tag, release or upload is made. The files are on the run's page as artifacts, named for a version such as `0.1.0-dev.2f20913`.

A manual run started on a `v` tag is a rehearsal too: it checks the tag and names the files for it, and still publishes nothing. Only the push of a tag publishes.

Do this once before the first real tag, and after any change to the workflow or the scripts under `.forgejo/scripts/`.

---

## 4. When a run fails

- **The push of the tag is rejected with "Tag … is protected".** No run starts. Your account is not on the allowlist of the `v*` tag rule (§5.4). Add your account to the allowlist, under the repository's Settings → Tags, and push the tag again.
- **`prepare` refuses the tag.** The log says why: the tag does not match `Cargo.toml`, its commit is not on `main`, a release token is missing, or the `appcast` branch is (§5.5). Delete the tag (`git push origin :refs/tags/<tag>`, `git tag -d <tag>`), fix the cause, and tag again. Nothing was published under it, so deleting it is safe.
- **A build fails.** Nothing has been published. Fix it on `main`, delete the tag as above, and tag the fixed commit. Once a version has been published, its tag stays where it is: a fix is the next version.
- **A build's Package step fails for a missing tool.** `dotnet is not on PATH` on Windows, `dpkg-shlibdeps is not installed` on Linux: the runner lacks what §5.3 lists. Install it and run the job again.
- **`publish` fails at "Sign the Windows installer".** Nothing has been published. The `UPDATE_SIGNING_KEY` secret is missing, is not a key, or is not the private half of the public key in the workflow; the log says which, and never shows the key. Set the secret (§5.1) and run the job again. If the runner's `openssl` is older than version 3, the log says that instead (§5.3).
- **`publish` fails at "Publish the Windows update feed".** Both releases are out, and installed copies have not been told. The log says why: the release's installer could not be downloaded as it was signed, the branch is missing or the token may not push to it (§5.5), or the feed there already offers a newer version. Fix the cause and run the job again.
- **`publish` fails at "Publish the Debian package".** Both releases are out; the apt repository and the Windows feed have not been told. The log gives the registry's answer: 401 or 403 means `PACKAGES_TOKEN` is wrong or may not write the owner's packages (§5.1). Fix it and run the job again.
- **"… is not the release's package: the registry holds another file under this version".** The same step, after the upload: the registry already had this version, and what it serves is not the release's `.deb`. Either the builds were run again after an earlier `publish` had uploaded the first, or something else was uploaded under the version, or what answered was not the registry's file at all, a proxy's or a login page say; opening the address the log names shows which. Nothing was changed there. The clean way out is a new patch version, since a released version is never rebuilt (DESIGN.md §11.6). The other is to delete that version on the owner's Packages page and run the job again; a system that already installed the first file keeps it, since the version is the same.
- **A warning "not compared" at that step.** The registry took the package, or had it, and the script could not read it back to compare. The release stands. Look at the package on the owner's Packages page; if this happens at every release, the address the script reads back from is not where this Forgejo serves a package's file (DESIGN.md §11.11).
- **`publish` fails halfway.** Run the job again. It finds the release it already made and replaces the files of the same name; it does not touch a title or description that exists.
- **"GitHub mirror: tag … is not at …"** The job waited ten minutes for the tag to arrive on GitHub. The Forgejo release is already published. Sync the push mirror (repository Settings → Mirror settings → Synchronize now), then run the job again.

To withdraw a release, delete it on both pages and then delete the tag.

---

## 5. One-time setup

### 5.1 Secrets

Four secrets, under the repository's (or the owning account's) Settings → Actions → Secrets. `prepare` fails before anything is built if one of the three tokens is missing.

| Secret | What it is | Needs |
| --- | --- | --- |
| `RELEASE_TOKEN` | a Forgejo access token | `write:repository` |
| `GH_RELEASE_TOKEN` | a GitHub token for `TheFlipside/code-plus-plus` | a fine-grained token for that one repository with Contents, read and write |
| `PACKAGES_TOKEN` | a Forgejo access token of the repository's owner, for the apt repository | `write:package` and nothing else, if the token's scopes can be set so |
| `UPDATE_SIGNING_KEY` | the private half of the key updates are signed with | the one line of the key file `winsparkle-tool generate-key` wrote: 32 bytes in base64 |

A classic GitHub token with `public_repo` works too, but it can write to every public repository of the account, so prefer the fine-grained one.

`prepare` only checks that the three tokens are set. The `publish` job uses them, on the `linux` runner; the build jobs never see them. `RELEASE_TOKEN` also commits the update feed to the `appcast` branch (§5.5).

`PACKAGES_TOKEN` is a token of its own because of what it can do: `apt` installs what the registry holds, as root, on every system that has added the repository (DESIGN.md §11.9). Give it no scope but packages. If it belongs to another account than the repository's owner, set that account's name as `PACKAGES_USER` in the workflow step's `env`.

`UPDATE_SIGNING_KEY` is worth more than the two release tokens (DESIGN.md §11.7). A token that leaks is revoked; this key cannot be. Every installed copy accepts what it signs, and a copy learns of a new key only through an update signed with the old one.

- It reaches one step of `publish` and nothing else, `prepare` included, so a missing key stops a release only once the builds are done, though before anything is published.
- Keep a copy offline, encrypted. Without the key no further update can be offered to the copies already installed.
- Its public half is `CODEPP_UPDATE_PUBLIC_KEY` in the workflow. `publish` refuses to sign with a key whose public half is another.
- Anyone who can push a branch with a workflow of their own to this repository can have that workflow read the secret, by a push or by a manual run of it. Today only the maintainer can push. Keep it so, or find out first whether Forgejo can keep a secret to protected branches and tags (DESIGN.md §11.10).
- While `publish` signs, the key is in the memory of the signing script and of `openssl` on the `linux` runner. Anything running there under the same account at that moment could read it, which is why §5.4's third rule matters most for this secret.

### 5.2 The push mirror

GitHub receives the tag from Forgejo's push mirror, and the GitHub release is created only once the tag is there and points at the commit that was built. Turn on "Sync when commits are pushed" for the mirror, so that the tag arrives within the ten minutes `publish` waits.

### 5.3 What the runners need

Beyond what CI already needs (DEVELOPMENT.md):

| Runner | Needs |
| --- | --- |
| `linux` | `python3` (3.8 or newer), `gh`, `openssl` 3 or newer, which signs the Windows installer, and network access to crates.io for the first `cargo install cargo-about`. For the `.deb`: a Debian or Ubuntu system with `dpkg-deb` and `dpkg-shlibdeps` (the `dpkg` and `dpkg-dev` packages; `build-essential` brings both) |
| `windows` | Windows PowerShell 5.1, which Windows ships. For the `.msi`: .NET, with `dotnet` on the PATH of the account the runner's jobs run as, and network access to nuget.org and github.com at every run |
| `macos` | the Xcode Command Line Tools: `codesign`, `lipo`, `plutil`, `ditto`. For the `.dmg`: `hdiutil`, which macOS ships |

The workflow adds the Rust targets itself (`rustup target add`), the second macOS architecture included.

The Windows installer is built with WiX 5, which runs on .NET. The packaging script names its version and the SHA-256 of two package files, WiX itself and its UI extension. At every run it downloads the two from nuget.org, refuses one whose hash is not the expected one, unpacks them into its temporary folder and runs WiX from there. Nothing of WiX is installed on the runner, and no copy cached there is used, so a release's installer is built by the same bytes whatever ran on the runner before. The .NET it was tried with is the SDK, version 10 (`winget install Microsoft.DotNet.SDK.10`); the runtime alone should do, and has not been tried. To move to another WiX version, follow the comment above the hashes in `.forgejo/scripts/package-windows.ps1`, then build the installer once by hand and install it: WiX 4 and later share the `.wxs` format, but nothing here has run on any version but 5.0.2.

`WinSparkle.dll` is not built here either. The same script names WinSparkle's version, the SHA-256 of its release zip and the SHA-256 of the x64 DLL in it. At every run it downloads the zip from the project's releases on GitHub, refuses one whose hash is not the expected one, takes the one file out of it and checks that file's hash too. To move to another WinSparkle version, follow the comment above those hashes in the script: Code++ refuses a DLL of any version but the one it was built for, and `THIRD_PARTY_LICENSES.md` names what the version bundles, so all three change together.

The `.deb`'s dependencies are read from the runner's own package database (§1.2), so the distribution the `linux` runner runs decides the oldest system the package installs on.

### 5.4 Who can release, and what runs beside it

Three settings on the Forgejo side, which the workflow cannot make for itself:

- **Protect the `v*` tags** (repository Settings → Tags), and put every account that cuts releases on the rule's allowlist, your own included. Anyone who can push a tag can start a release, and the workflow that runs is the one in the tagged commit. A protected tag pattern limits that to the accounts on its allowlist. A rule with an empty allowlist lets nobody create a matching tag, the repository's owner included: the push is rejected with "Tag … is protected". The rule does not hold the accounts on the list back: one of them can still move or delete a published tag, and a moved tag and a second run would replace a published release's files. So list only the accounts that cut releases, and leave a pushed tag where it is (§4).
- **Protect `main`** (repository Settings → Branches), once anyone else has write access. A release is tagged on `main`, and the workflow that runs is the one in the tagged commit, so whoever can push to `main` decides what a release builds and what `publish` runs with the release tokens. The tag rule is only as strong as this one.
- **Do not let pull requests from outside contributors run CI unapproved.** CI builds a pull request on the same persistent `linux` runner that runs `publish`, and a build runs the pull request's build scripts. Code running there while a release is being published could read the release tokens out of the publishing process, or leave a changed `gh`, `python3` or `cargo-about` behind for the next release. If outside pull requests are to build without approval, give `publish` a runner of its own first.

### 5.5 The `appcast` branch

The update feed (§1.4) is published on a branch of its own, `appcast`, which shares no history with `main` and holds the feeds and a note saying what it is. `prepare` refuses to release until it exists. Create it once, from a clone of the repository:

```sh
git worktree add --orphan -b appcast ../code-plus-plus-appcast
cd ../code-plus-plus-appcast
echo "The update feeds of Code++, written by the release workflow. Do not edit by hand." > README.md
git add README.md
git commit -m "appcast: the branch the update feeds are published on"
git push origin appcast
cd -
git worktree remove ../code-plus-plus-appcast
```

Then protect it (repository Settings → Branches), with a rule for `appcast`:

- Only the account `RELEASE_TOKEN` belongs to may push to it. That account commits the feed, through the API.
- No force-push and no deletion, or the branch's history would no longer say what each feed offered and when.

Whoever can push to the branch decides what installed copies are offered. They cannot make a copy install something the update key did not sign, but they can withhold a release, or offer an older signed one to a copy older still (DESIGN.md §11.7).

CI does not run for the branch: it builds pushes to `main` and pull requests into it. The push mirror carries the branch to GitHub; no installed copy asks GitHub.

### 5.6 The apt repository

Nothing has to be created: Forgejo makes the owner's Debian registry, and the key it signs the index with, on first use. Two things are done once by hand:

- **After the first release, link the package to the repository**: on the owner's Packages page open `codepp`, and under its settings choose this repository. It then shows on the repository's page.
- **Check the fingerprint in the README.** It is `342F 315B C347 A13C 5333  05FA 1E15 E729 5E09 D071`, as the instance served it on 13 November 2026. `curl -fsSL https://git.fiedler.live/api/packages/tux/debian/repository.key | gpg --show-keys --with-fingerprint` shows the current one. If the instance ever makes a new key, the README has to follow.

---

## 6. What the workflow checks

- **The tag.** `.forgejo/scripts/release-version.sh` holds it to `v` and `Cargo.toml`'s version, with no pre-release part, and `prepare` requires its commit to be on `main`.
- **The lockfile.** Every build is `--locked`.
- **The licences.** `cargo about generate --fail` stops on a crate whose licence is not in `about.toml`'s `accepted` list, which is `deny.toml`'s allowlist, or whose licence it cannot read.
- **Static linking on Windows.** `.cargo/config.toml` links the C and C++ runtimes statically, so that the app starts on a machine without the Visual C++ Redistributable. `.forgejo/scripts/package-windows.ps1` reads the imports of `codepp.exe` and of each plugin DLL and refuses to package one that names a runtime DLL.
- **The Windows subsystem.** `codepp.exe` is linked for the GUI subsystem, so that starting it from Explorer opens no console window beside the editor (DESIGN.md §9.1). `.forgejo/scripts/package-windows.ps1` reads the subsystem field of the executable's header and refuses to package a console-subsystem one.
- **The macOS architecture.** `.forgejo/scripts/package-unix.sh` checks each binary with `lipo` before it is named for an architecture.
- **The tool that builds the Windows installer.** `.forgejo/scripts/package-windows.ps1` uses WiX only as the two package files whose SHA-256 it holds (§5.3), and stops before writing anything if either arrives changed.
- **The updater's library.** The same script takes `WinSparkle.dll` only out of the release zip whose SHA-256 it holds, checks the file's own SHA-256 as well (§5.3), and stops before writing anything if either differs. The read-back below holds the file inside the built installer to the same hash.
- **The Windows installer.** The same script reads the built `.msi` back through Windows Installer: that its version is the one it is named for, that it installs for the whole machine, and that it holds exactly the files the zip holds and `WinSparkle.dll`, each at the same size and, taken out of the installer again by WiX, with the same SHA-256, so a file added to the zip and forgotten in `packaging/windows/codepp.wxs` stops the release. It also requires that the installer runs no code of its own: the only custom action in it is the one behind the licence page's Print button, which WiX's UI brings, and no custom action is in any of its sequences, the elevated part of an installation included. And it requires that the installer does nothing but what §1.1 says: it may hold only the tables WiX writes for files, a shortcut, registry values, the upgrade rule and the UI, so not those that install a service, register a COM class, set an environment variable or change permissions; and it may write only the registry keys the `.wxs` names. A deliberate change to the `.wxs` that needs more is added to the lists in the script. The zip and the installer are made in a temporary folder and moved to the output folder together, so an installer that fails leaves neither.
- **The Debian package.** `.forgejo/scripts/package-unix.sh` requires `dpkg-shlibdeps` to have named the packages of glibc and GTK 3, and reads the built `.deb` back: its version, that every entry belongs to root and has one of the four modes such a package has (so no set-group-ID folder or writable file gets in from the runner's temporary folder), that its control part holds the control file and no script, so nothing of the package's runs as root at installation or removal, and that it holds the binary, the link to it, the three plugins, the menu entry, an icon and the licence. A package that fails is deleted.
- **The version, again.** Both packaging scripts check the version they are given before using it, as `release-version.sh` did before them, since they can also be run by hand.
- **The disk image.** `hdiutil verify`, after up to three attempts at creating it.
- **The set of files.** `publish` requires exactly the eight files, by name, each a regular file.
- **The Debian package's upload.** `publish_deb.py` uploads only the release's own `.deb`, by name, and takes only the registry's 201, or its 409 for a version that is already there, for done. It then reads the package back from the registry without a token and fails if the bytes are not the release's; if it cannot read it back at all, it warns and passes.
- **The update key.** `update_feed.py sign` requires the public key derived from `UPDATE_SIGNING_KEY` to be the one the builds carry, and the signature it makes to verify against that key, before anything is published.
- **The update feed.** `update_feed.py check`, in `prepare`, requires the `appcast` branch to exist and the address the builds are given to be where the feed is published. `update_feed.py publish` downloads the installer from the address the feed names and requires it to be the file that was signed, and refuses to replace a feed that offers a newer version.
- **The mirror's tag**, as §5.2 describes.

---

## 7. Not done yet

- **Code signing.** No build is signed with a certificate, and neither is an installer. A certificate is also the first thing "Edit with Code++" would take to show in Windows 11's short context menu (DESIGN.md §7.4, the entry on "Edit with Code++"); the maintainer has put that off to a later time. Windows shows SmartScreen's prompt, and its administrator prompt names the `.msi` as from an unknown publisher; macOS refuses the first start until the user allows it. The release description tells testers how. The macOS bundle has an ad-hoc signature only, which Apple Silicon requires of every binary; the disk image itself is not signed.
- **More packages.** No `.rpm`, AppImage or Flatpak.
- **The apt repository's first real run.** The upload has run against a stand-in for the registry only, and `apt` has not installed from it yet (DESIGN.md §11.11).
- **More of the updater.** Only a Code++ the `.msi` installed for one account updates itself (§1.4). An installation for the whole machine, macOS and the `.deb` do not yet (DESIGN.md §11).
- **The feed's first real run.** Signing and the commit to the `appcast` branch have run against a stand-in for the server only, and a manual run (§3) rehearses neither.
- **More of the installers.** The Windows installer has WiX's stock pictures and English text only, offers no choice of folder or of what to set up, and does not check the Windows version: on Windows 8.1 or older it would install a program that cannot start. The `.deb` has no manual page, changelog or AppStream metadata, so a software centre shows it without a description page, and `lintian` would say so; it has not been run on it. The disk image has no background picture or arranged icons.
- **The macOS disk image on a Mac.** The `.dmg` step has run only against stand-ins for Apple's tools (DESIGN.md §9.2).
- **A glibc baseline.** The Linux build runs on systems whose glibc and GTK 3 are at least as new as the `linux` runner's.
- **Tests in the release.** The workflow trusts the CI run of the tagged commit.
