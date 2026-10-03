# Release automation

Push a SemVer tag such as `v0.0.1` after committing the intended changes. The
Release workflow builds that exact commit, applies the tag version to its
temporary Cargo.toml and Cargo.lock, generates release notes from git history,
and publishes the release after every required check passes. Versions appear in
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
updating AUR or Homebrew. A manual run on a branch builds and tests artifacts
without publishing. The tag name, rather than the development version recorded
on that branch, is the version of every tagged build. Never move a published tag.

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
