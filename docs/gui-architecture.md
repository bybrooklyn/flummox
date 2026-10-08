# Desktop architecture

`flummox-gui` has one entrypoint and one public `gui::run` function. The GUI
feature compiles the shared theme and desktop shell on every target. Linux,
Windows and Mac select their storage integration with `cfg(target_os)`, so
runtime code does not guess which operating system it is using.

Linux owns btrfs, FUSE stores, process detection and its durable coordinator.
Windows owns NTFS WOF calls, launcher discovery, a separate durable coordinator
and opt-in maintenance. Mac owns native APFS compression and recovery; Mac GUI
jobs currently run in the app instead of a durable coordinator. Targets without
a safe backend show the shared shell with compression disabled. Linux system
dependencies use `cfg(target_os = "linux")`, not the broader `cfg(unix)`, since
Landlock, `/proc` and btrfs are Linux interfaces.

The presentation follows these rules:

- A game exposes one primary action, Compress, beside its estimated saving
  or its result.
- The window uses three verbs: Compress, Decompress and Analyze.
- A game has one mode choice, Standard or Maximum. Several games at once
  always use Standard.
- Decompress, Analyze and the mode choice belong in expanded details. The
  native preset, store paths and the compatibility form live under Advanced.
- A job starts without a confirmation when its space plan passes. The plan is
  shown only when it fails.
- Jobs have their own page. A command the worker refuses is shown for a few
  seconds and is not treated as a lost connection.
- Controls are disabled before dispatch when a capability or prerequisite is
  unavailable.
- Progress is measured. Work without a known total uses text instead of a
  fabricated percentage.
- Outer pages own scrolling. Expanded cards grow to their content instead of
  embedding fixed-height scroll areas.
- Reduced motion resolves transitions immediately.

Compatibility qualifications use the versioned schema in `compatibility.rs`.
Its serialized fields cannot hold titles, paths, user names or host identifiers.
Automatic Maximum Space still requires a matching game build and corpus hash,
verified bytes and metadata, writable launcher updates, rollback, a successful
launch, and no more than a ten percent measured load-time increase.

Headless desktop tests render the same widgets through iced's software renderer
using fixture state. Set `FLUMMOX_PREVIEW_DIR` when running
`cargo test --all-features gui::preview` to retain PNGs for wide and narrow views.
These renders verify layout without opening a window or scanning game libraries.
Native dialog helpers still require a desktop session for interactive selection.
