# Next steps

## Release acceptance

These checks remain necessary before calling the release fully validated:

- Qualify real native Linux and Proton games for Maximum Space. Check launch,
  gameplay, patching, launcher verification, login remount, and restoration.
  Record load times and gameplay results against an ordinary-directory baseline.
- Exercise folder and report dialogs in KDE and another desktop session,
  including selection, cancellation, spaces in paths, and disconnected drives.
- Run the release workflow on GitHub and review its downloadable packages.
  Publish its draft after desktop and game acceptance.
- For a Windows release, complete workflow parity and test the WOF backend on
  native NTFS. Cross-compilation establishes a build, not runtime acceptance.

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

Items above are proposed improvements, not features completed by the dependency
update. Scheduling, macOS, Bottles integration, and community report sharing can
follow the core release work.

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
lockfile updates. Windows runtime and GitHub workflow execution remain part of
release acceptance.
