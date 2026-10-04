# Linux real-engine smoke checks

Status: partial evidence, not completed release qualification. Recorded on
2026-10-04 UTC. Machine and game paths are omitted from the checked-in results.

## Factorio

Factorio 2.0.77, engine build 84539, Steam build 23347942 was copied into a
private disposable workspace using buffered reads and writes. Save, config,
Flummox state, and update directories were isolated. The installed game and
ordinary save files were not modified. The candidate executable came from
commit `233f2973f235e20b8b0fbabb1d93ccd7b114eeff`, before the subsequent
workflow-only Windows CI change.

The native engine created a world with seed 424242 and ran three repetitions of
1,200 simulation ticks. The same world completed through a Maximum Space mount,
after an interrupted coordinator and automatic remount, and after restoration
to ordinary files. Every phase produced simulation checksum `3108547700`.

All 13,614 original tree entries matched through the mount. SHA-256 file bytes,
permission modes, modification timestamps, symlink targets, and extended
attributes were compared. A synthetic documentation edit and one added file
survived coordinator restart and restoration; all 13,615 updated entries
matched. This checks writable overlay behavior, not a real launcher patch.

Maximum Space compared zstd levels 9, 15, 19, and 22. Creation and verification
took 480.536 seconds. The 11,837-file corpus contained 2,047,685,348 logical
bytes; the verified store contained 1,464,552,386 bytes, a 28.48 percent
file-stream reduction. Physical allocated bytes were not measured: this
machine's btrfs measurements require privileges unavailable to the session.

The ordinary, mounted, remounted, and restored headless process runs took
515, 514, 464, and 464 milliseconds respectively. These are warm-cache process
durations including simulation. They do not establish cold game load times,
rendering, streaming behavior, or representative factory performance.

The store, update layer, disposable game, world, and raw logs remain in the
private local workspace. No managed mount or test coordinator remains running.
Path-free measured results are in
[the smoke record](2026-10-04-factorio-smoke.json).

## Super Meat Boy through Proton

The Windows build of Super Meat Boy, Steam build 3241924, loaded menu textures
and completed cutscenes in a private Proton 10.0 runtime. The same startup
checks passed through the mounted store, after coordinator restart and
automatic remount, and after restoration to ordinary files. Immutable game
bytes and metadata matched across every phase. Captured writable state matched
after remount and rollback.

The 46-file starting corpus contained 519,530,692 logical bytes. Maximum Space
creation and verification took 94.761 seconds and produced a 369,884,508-byte
store. These are stream lengths, not physical disk allocation measurements.

The runtime and prefix were copied, and game state/configuration were isolated.
Network, IPC, and PID namespaces prevented access to the ordinary Steam
session. The host filesystem was read-only, with the disposable workspace
writable. Activation was refused while a private display process held the
copied game folder; it succeeded after that namespace was stopped.

Each startup run was stopped by the harness after forty seconds. Fresh Wine
traces recorded actual menu asset loads and completed cutscenes. An initial
assertion used the game's buffered file log, which remained empty after forced
termination; the repeated checks used per-run Wine traces. These checks do not
establish interactive gameplay, normal exit, progress saves, or cold load times.
See [the Proton smoke record](2026-10-04-proton-smoke.json).

## Incomplete checks

A private graphical Factorio baseline requested a Steam restart, and the
headless compositor exited with status 139. A Terraria Linux-assembly attempt
did not provide a valid Windows-client baseline. Super Meat Boy was subsequently
used for the Proton startup smoke checks above.

Interactive gameplay, real launcher patching and verification, OS reboot/login
remount, allocated-byte measurements, representative loading, and Windows/Mac
real-game checks remain outstanding. `0.0.2.json` stays pending with no qualified
runs added by these smoke checks.
