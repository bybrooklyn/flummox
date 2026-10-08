# Release automation

Push a SemVer tag such as `v0.0.2` after committing the intended changes and
passing the recorded [real-game acceptance](validation/README.md). The tagged
commit must be on `main`, and the tag's base version must equal the version in
`Cargo.toml`. A stable tag also needs `docs/releases/VERSION.md` and a passed
`docs/validation/VERSION.json`. The Release workflow builds that exact commit,
applies the tag version (which adds a prerelease suffix) to its temporary
Cargo.toml and Cargo.lock, generates release notes from git history unless
curated notes exist, and publishes the release after every required check
passes, including the btrfs and FUSE tests. Versions appear in
the CLI, desktop app, macOS bundle metadata, installer, recipes, and filenames.

## Artifacts and checks

| Platform | Runner | Downloads | Required validation |
| --- | --- | --- | --- |
| Linux x86_64 | Ubuntu 24.04 | tar.xz, Arch package | Clippy, tests, dependency policy, ELF architecture, CLI version, Arch install/upgrade/GUI |
| Linux ARM64 | Ubuntu 24.04 ARM | tar.xz | Clippy, tests, ELF architecture, CLI version |
| macOS Apple Silicon | macOS 14 ARM | Flummox.app ZIP | Clippy, tests, Mach-O architecture, bundle version/signature, Homebrew install/uninstall |
| Windows x64 | Windows 2025 | setup.exe, portable ZIP | Clippy, tests, PE architecture, CLI version, installer install/upgrade/uninstall |

GitHub displays a SHA-256 digest beside each download. Checksum files remain in
local bundles and internal CI artifacts for verification but are not uploaded
as public release downloads. AUR and Homebrew retain pinned archive hashes.
The Arch package and package recipes are
generated from the exact archives tested by the release workflow. A release is
staged as a draft during upload and then published automatically; a failed build
does not produce a public release. A failed channel update leaves the already
published downloads intact. Re-run failed jobs to retry updates; uploads and
channel commits tolerate retries of the same release.

Prerelease tags, for example `v0.0.2-rc.1`, produce a GitHub prerelease without
updating AUR or Homebrew, and skip the acceptance check and the Arch and
Homebrew install checks. A manual run on a branch builds and tests artifacts
without publishing. The tag name sets the version of every tagged build.
Never move a published tag: the publish step refuses to upload into a release
that is no longer a draft.

Add `docs/releases/VERSION.md` for curated release notes. CI uses that document
in preference to a generated commit changelog. The initial release introduces
the product and its supported workflows; later versions can describe user-facing
changes in their own documents or use the automatic changelog.

## Homebrew tap

The tap is https://github.com/bybrooklyn/homebrew-flummox. Release automation
uses the `HOMEBREW_DEPLOY_KEY` secret, a write-capable SSH deploy key registered
only on that repository. The public GitHub SSH host keys are obtained through
GitHub's HTTPS metadata API before cloning. Stable releases update
`Casks/flummox.rb` with their download URL and exact SHA-256. The cask installs
the Apple Silicon app into Applications. It does not delete application data.

## AUR

The package is `flummox-bin`, owned by the `bybrooklyn` AUR account. The
`AUR_SSH_PRIVATE_KEY` secret must correspond to an SSH public key registered in
that account. The workflow pushes only PKGBUILD and .SRCINFO to
`ssh://aur@aur.archlinux.org/flummox-bin.git`. It checks the Ed25519 host key
against the fingerprint published on the AUR website. A missing credential or
unregistered key fails the channel-update job with an explicit error.

The recipe supports x86_64 and aarch64, with a separate SHA-256 for each archive.
The release workflow compares generated .SRCINFO with `makepkg --printsrcinfo`
and tests a real x86_64 package install and upgrade in an Arch container. The
existing `packaging/PKGBUILD` remains available for local source builds.

## Windows installation

Inno Setup installs per user into `%LOCALAPPDATA%\Programs\Flummox`, creates
Start menu shortcuts, optionally creates a desktop shortcut, and registers an
uninstaller in Installed apps. A fixed AppId makes later versions upgrade the
same installation. Neither setup nor uninstall modifies game folders or removes
application data. The installer asks users to close Flummox and finish jobs;
it does not force-close compression processes. The portable ZIP remains available.

## Signing

macOS bundles have an ad hoc signature for binary integrity; they have no Apple
Developer ID signature or notarization. Windows installers and binaries have
no Authenticode signature. Trusted publisher installation requires Apple
Developer credentials and a Windows signing certificate or signing service.
Those credentials are not configured by this pipeline.

## Local validation

Run `just ci` for the code, packaging tests, dependency policy, and prose checks.
`python3 packaging/test_release.py` exercises tag-to-manifest/lock consistency,
checksum generation, and mislabeled architecture rejection using temporary
fixtures. Native installer and Homebrew acceptance run on GitHub's respective
platform runners. No release check touches a real game library.

## Release signatures

`RELEASE_SIGNING_KEY` holds the stable Minisign secret key. Its public key is
committed in `packaging/minisign.pub` and appended to every release's notes.
The signing job runs after finalized archives, installers, and the Arch package
have passed their checks. It signs a versioned JSON manifest containing the tag,
commit, exact download set, lengths, and SHA-256 digests. Publication and package
channel updates verify the signature and all local downloads first.

A local private backup is kept outside the repository in the owner's application
state. Keep that key private and back it up securely. Rotation requires a new
public key and an announcement signed with the previous key; do not regenerate
keys for each tag. User verification should pin the previously trusted public
key, since replacing a release's key and signature together does not prove
continuity with earlier releases.

Linux packaging uses native objcopy to extract debug symbols, strip only debug
information from staging copies, and attach debug links. Matching symbols are
CI artifacts retained for 90 days, the longest GitHub allows. They are not
release assets, because the signed manifest names the exact download set, so a
crash report from a release older than 90 days cannot be symbolised from CI.
Archive size reports are CI artifacts too. Download packaging leaves developer
build outputs unchanged.

## Third-party licence notices

Each archive, the portable ZIP and the macOS bundle carry
`THIRD-PARTY-LICENSES.txt`, generated by `cargo about` from
`packaging/about.toml` and `packaging/about.hbs`. `package-release.py
--require-notices` fails when the file is missing, which the Release workflow
and `just release` both pass. The Windows installer does not install the file
yet, because `packaging/windows/flummox.iss` does not list it.

`docs/validation/VERSION.json` must contain passed Linux native, Proton,
Windows, and Mac runs with local compatibility reports before a stable tag can
publish, and a missing record stops the workflow. `docs/validation/0.0.2.json`
is the record 0.0.2 needs and is pending. Workflow dispatch and prerelease tags
remain available for candidate builds while those interactive checks are
pending. This gate does not replace native automated validation.

## Owner checklist

These are repository settings that no commit can change. Until the first four
are done, the signing and channel secrets are repository secrets, and anyone with
push access can read them by editing a workflow on a branch.

1. Move `RELEASE_SIGNING_KEY`, `AUR_SSH_PRIVATE_KEY` and `HOMEBREW_DEPLOY_KEY`
   into the `release` environment (Settings > Environments > release).
2. Delete the repository-level copies of the same three secrets.
3. Add a required reviewer to the `release` environment, and restrict its
   deployment branches and tags to `v*`.
4. Add a tag ruleset for `v*` that blocks deletion and non-fast-forward updates.
5. Require the `Build, test, lint and dependency policy` check and the btrfs and
   FUSE jobs of `Filesystem tests` on `main`, so a commit is checked before it
   can be tagged.
6. TODO, needs network access to verify: pin the Arch container in the `arch`
   job to an image digest. The workflow uses `archlinux:base`, which moves.
7. TODO, needs network access to verify: pin Inno Setup in the Windows
   installer steps by version and checksum. The workflow runs an unpinned
   `choco install innosetup`.
8. TODO, needs network access to verify: pin the `cargo-about` version in the
   release build job. Only `just`, `cargo-deny` and the Rust toolchain are
   pinned there today.
