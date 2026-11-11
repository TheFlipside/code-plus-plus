# Code++ — Releasing

A release is made by pushing a version tag. `.forgejo/workflows/release.yml` builds Code++ on the three runners, packages it, and publishes the result on Forgejo and on the GitHub mirror. Nothing is built or uploaded by hand.

---

## 1. What a release contains

| File | Platform | Contents |
| --- | --- | --- |
| `codepp-windows-x64-<version>.zip` | Windows 10 and 11, 64-bit | `codepp.exe`, the three plugin DLLs, the licence files |
| `codepp-linux-amd64-<version>.tar.gz` | Linux, x86-64, GTK 3 | one folder with `codepp`, the three plugin libraries, the licence files |
| `codepp-macos-arm64-<version>.zip` | macOS, Apple Silicon | `Code++.app` |
| `codepp-macos-x64-<version>.zip` | macOS, Intel | `Code++.app` |
| `SHA256SUMS` | | the checksum of each archive |

The plugins are the three preinstalled ones (DESIGN.md §6.6): `cppmimetools`, `cppconverter` and `cppexport`. They sit beside the executable, and Code++ copies them into the user's plugins folder on first start (`codepp_platform::stage_bundled_plugins`). `example-hello`, the SDK sample, is not shipped.

Every archive carries `LICENSE` (`LICENSE.txt` in the Windows zip), `THIRD_PARTY_LICENSES.md`, and `THIRD_PARTY_CRATES.md`, the licence notices of the Rust crates in the binary. The last is generated during the release by `cargo about` from `about.toml` and `about.hbs`, for all four targets at once, so the four archives carry the same file.

The same four archives and `SHA256SUMS` are attached to both releases:

- Forgejo: <https://git.fiedler.live/tux/code-plus-plus/releases>
- GitHub: <https://github.com/TheFlipside/code-plus-plus/releases>

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

Actions → Release → Run workflow starts the same jobs for a branch. It builds and packages all four archives and skips `publish`, so no tag, release or upload is made. The archives are on the run's page as artifacts, named for a version such as `0.1.0-dev.2f20913`.

A manual run started on a `v` tag is a rehearsal too: it checks the tag and names the archives for it, and still publishes nothing. Only the push of a tag publishes.

Do this once before the first real tag, and after any change to the workflow or the scripts under `.forgejo/scripts/`.

---

## 4. When a run fails

- **`prepare` refuses the tag.** The log says why: the tag does not match `Cargo.toml`, its commit is not on `main`, or a release token is missing. Delete the tag (`git push origin :refs/tags/<tag>`, `git tag -d <tag>`), fix the cause, and tag again.
- **A build fails.** Nothing has been published. Fix it on `main` and tag the fixed commit with the next pre-release number. A tag that was pushed is better left alone than moved.
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
| `linux` | `python3` (3.8 or newer), `gh`, and network access to crates.io for the first `cargo install cargo-about` |
| `windows` | Windows PowerShell 5.1, which Windows ships |
| `macos` | the Xcode Command Line Tools: `codesign`, `lipo`, `plutil`, `ditto` |

The workflow adds the Rust targets itself (`rustup target add`), the second macOS architecture included.

### 5.4 Who can release, and what runs beside it

Two settings on the Forgejo side, which the workflow cannot make for itself:

- **Protect the `v*` tags** (repository Settings → Tags). Anyone who can push a tag can start a release, and the workflow that runs is the one in the tagged commit. A protected tag pattern limits that to the people named, and stops a published tag from being moved: a moved tag and a second run would replace a published release's files.
- **Do not let pull requests from outside contributors run CI unapproved.** CI builds a pull request on the same persistent `linux` runner that runs `publish`, and a build runs the pull request's build scripts. Code running there while a release is being published could read the release tokens out of the publishing process, or leave a changed `gh`, `python3` or `cargo-about` behind for the next release. If outside pull requests are to build without approval, give `publish` a runner of its own first.

---

## 6. What the workflow checks

- **The tag.** `.forgejo/scripts/release-version.sh` holds it to `v<x.y.z>[-<pre-release>]` and to `Cargo.toml`'s version, and `prepare` requires its commit to be on `main`.
- **The lockfile.** Every build is `--locked`.
- **The licences.** `cargo about generate --fail` stops on a crate whose licence is not in `about.toml`'s `accepted` list, which is `deny.toml`'s allowlist, or whose licence it cannot read.
- **Static linking on Windows.** `.cargo/config.toml` links the C and C++ runtimes statically, so that the app starts on a machine without the Visual C++ Redistributable. `.forgejo/scripts/package-windows.ps1` reads the imports of `codepp.exe` and of each plugin DLL and refuses to package one that names a runtime DLL.
- **The macOS architecture.** `.forgejo/scripts/package-unix.sh` checks each binary with `lipo` before it is named for an architecture.
- **The set of files.** `publish` requires exactly the four archives, by name, each a regular file.
- **The mirror's tag**, as §5.2 describes.

---

## 7. Not done yet

- **Code signing.** No build is signed with a certificate. Windows shows SmartScreen's prompt and macOS refuses the first start until the user allows it; the release description tells testers how. The macOS bundle has an ad-hoc signature only, which Apple Silicon requires of every binary.
- **Installers and packages.** No MSI, `.deb`, `.rpm`, AppImage or `.dmg`, and no update check in the app.
- **A glibc baseline.** The Linux build runs on systems whose glibc and GTK 3 are at least as new as the `linux` runner's.
- **Tests in the release.** The workflow trusts the CI run of the tagged commit.
