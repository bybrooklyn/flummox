# Desktop architecture

`flummox-gui` has one entrypoint and one public `gui::run` function. The GUI
feature compiles the shared theme and desktop shell on every target. Linux and
Windows select their storage integration with `cfg(target_os)`, so runtime code
does not guess which operating system it is using.

Linux owns btrfs, FUSE stores, process detection and the durable coordinator.
Windows owns NTFS WOF calls and Windows launcher discovery. Targets without a
safe backend show the shared shell with compression disabled. Linux system
dependencies use `cfg(target_os = "linux")`, not the broader `cfg(unix)`, since
Landlock, `/proc` and btrfs are Linux interfaces.

The presentation follows these rules:

- A game exposes one primary action beside its estimated saving and state.
- Restore and analysis belong in expanded details.
- Pack creation, paths, compaction and reclaim live under Advanced storage.
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
