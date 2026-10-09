# Desktop architecture

`flummox-gui` has one entrypoint and one public `gui::run` function. The GUI
feature compiles the shared theme and desktop shell on every target. Linux,
Windows and Mac select their storage integration with `cfg(target_os)`, so
runtime code does not guess which operating system it is using.

Linux owns btrfs, FUSE stores, process detection and its durable coordinator.
Windows owns NTFS WOF calls, launcher discovery, a separate durable coordinator
and opt-in maintenance. Mac owns APFS compression and recovery; Mac GUI
jobs currently run in the app instead of a durable coordinator. Targets without
a safe backend show one titled page with a card that says compression is not
available there. Linux system
dependencies use `cfg(target_os = "linux")`, not the broader `cfg(unix)`, since
Landlock, `/proc` and btrfs are Linux interfaces.

The presentation follows these rules:

- A game exposes one primary action, Compress, beside its estimated saving
  or its result.
- Vocabulary. The window, the command line output and the docs use one term per
  concept. The three verbs are Compress, Decompress and Analyze. The modes are
  Standard and Maximum, and a Standard strength is Fast, Balanced or Max.
  Maximum keeps its data in a store. A job is Waiting, Running, Paused or
  Stopped, and ends Completed, Partly done or Failed. Maximum's later steps are
  named Delete the original, Delete the previous version, Fold in updates and
  Check decompressed files. A game is excluded or included. Folders are
  locations: a Single game or a Games library. The helper process is the
  background worker; "coordinator" appears only in design docs. A saved
  compatibility result is a compatibility report. Code identifiers, CLI
  subcommands and file formats keep their names (`flummox pack ...`).
  Sentences end with a full stop and fragments do not, in-progress text ends
  with an ellipsis, counts agree with their noun, and sizes and durations are
  humanised.
- On Linux a game has one mode choice, Standard or Maximum. Several games at
  once always use Standard. Windows and Mac have one compression method each
  and no mode choice.
- Decompress, Analyze and the mode choice belong in expanded details. The
  Standard strength, store paths and the compatibility report form live under
  Advanced. Windows and Mac put Decompress and Test compatibility under
  Advanced and have no strength or store form.
- A job starts without a confirmation when its storage plan passes. The plan is
  shown only when it fails.
- On Linux, Jobs have their own page. On Windows and Mac, Jobs is a section of
  Settings. A command the worker refuses is shown for a few seconds and is not
  treated as a lost connection.
- Controls are disabled before dispatch when a capability or prerequisite is
  unavailable.
- Progress is measured. Work without a known total uses text instead of a
  fabricated percentage.
- Outer pages own scrolling. Expanded cards grow to their content instead of
  embedding fixed-height scroll areas.
- Reduced motion resolves transitions immediately.

Compatibility reports use the versioned schema in `compatibility.rs`.
Its serialized fields cannot hold titles, paths, user names or host identifiers.
Automatic Maximum still requires a matching game build and corpus hash,
checked bytes and metadata, writable launcher updates, the kept original, a successful
launch, and no more than a ten percent measured load-time increase.

Headless desktop tests render the same widgets through iced's software renderer
using fixture state. Set `FLUMMOX_PREVIEW_DIR` when running
`cargo test --all-features gui::preview` to retain PNGs for wide and narrow views.
These renders verify layout without opening a window or scanning game libraries.
Native dialog helpers still require a desktop session for interactive selection.
