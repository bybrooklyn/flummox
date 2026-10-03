# Next steps

## Release acceptance

These checks remain necessary before calling the release fully validated:

- Qualify real native Linux and Proton games for Maximum Space. Check launch,
  gameplay, patching, launcher verification, login remount, and restoration.
  Record load times and gameplay results against an ordinary-directory baseline.
- Exercise folder and report dialogs in KDE and another desktop session,
  including selection, cancellation, spaces in paths, and disconnected drives.
- Test the Windows WOF backend on real NTFS game installs, including launch,
  gameplay, update, restore, and interrupted operations. Native CI and installer
  tests pass, but they do not establish real-game compression compatibility.

## Completed distribution work

Version 0.0.1 is published. Tag-triggered native builds, embedded versions,
release notes, installation tests, and automatic Homebrew/AUR updates passed in
[the release workflow](https://github.com/bybrooklyn/flummox/actions/runs/37156256792).
Linux x86_64/ARM64, Apple Silicon macOS, and Windows x64 downloads are available.
macOS is currently a shell without a compression backend. This replaces the
previous draft-release and cross-build-only acceptance items.

## Recommended next release

Prioritize temporary-space planning and recovery alongside real-game acceptance.
Creating a store while retaining the original requires additional space, and
users need to understand where it is needed before a job starts. Validate launch,
patching, interruption, and restoration against ordinary installs before making
broader compatibility or performance claims.

After those core workflows, make the macOS download useful with a storage backend,
and configure Windows publisher signing and Apple Developer ID/notarization.
Choose the macOS backend through experiments with temporary game fixtures and
verified restoration before exposing it as a supported storage mode. Credentials
for trusted signing are separate from the currently configured release keys.

## Product improvements

1. **Plan temporary space before storage jobs.** Show the source, store, retained
   original, and restoration requirements on the relevant drives. Check available
   space before starting and explain what users need to free. Distinguish sample
   predictions from verified store sizes.
2. **Guide Maximum Space qualification in the app.** Start from a selected game,
   capture its build and corpus, lead the user through launch/update/restore
   checks, record baseline and compressed performance, and save the local report.
   Reports currently have to be produced separately and imported.
3. **Put recovery actions in one place.** Present interrupted jobs and affected
   installs together, explain which original/store/update layer is retained, and
   provide the appropriate retry or restore action. Add a user-controlled export
   of diagnostics for troubleshooting.
4. **Remember disconnected custom libraries.** Keep the last discovered games
   visible with an unavailable-drive state, explain why their jobs are paused,
   and rediscover them when the drive returns. Collection discovery currently
   reports a warning and cannot list children of an unavailable location.
5. **Finish desktop accessibility and navigation.** Exercise focus order, keyboard
   folder entry, button labels, contrast, and reduced motion in real sessions.
   Preserve the current sidebar and theme while improving these interactions.

Items above are proposed improvements. Windows launcher and maintenance parity,
scheduling, Bottles integration, and community report sharing can follow the core
storage workflows.

## Dependency update (2026-10-03)

The manifest now requires current stable direct releases and the lockfile updates
compatible transitive dependencies. FUSE moves to 0.18, SHA-256 to sha2 0.11,
and signal handling to signal-hook 0.4. The mount call and SHA-256 formatting
were adapted without changing stored corpus hashes. CI actions also move to their
current releases.

The preview renderer keeps tiny-skia 0.11.4 because Iced 0.14's renderer exposes
0.11 types. Its latest renderer adapter, iced_tiny_skia 0.14.1, still requires
that line; adopting standalone tiny-skia 0.12 would make the preview types
incompatible. This dependency must move with Iced's renderer.

Validation passes: `just ci`, `just build`, CLI-only compilation, required FUSE
mount/coordinator fixtures, Windows CLI/GUI cross-build (`just win`), and workflow
linting (`actionlint`). A final Cargo update dry run finds no remaining compatible
lockfile updates. Native builds, portable-module tests, installation checks, and GitHub release
execution now pass. Real-game storage acceptance remains outstanding.
