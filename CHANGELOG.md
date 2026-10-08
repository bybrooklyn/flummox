# Changelog

Every release, newest first. Full notes for each version are in
[docs/releases](docs/releases).

## 0.0.2 (unreleased)

Real-game acceptance is [still pending](docs/validation/README.md), so this
version has no download yet. [Full notes](docs/releases/0.0.2.md).

### Added

- Overview, Games and one Settings page, with jobs grouped as Running, Waiting,
  Needs attention and History.
- Windows discovery for Steam, Epic, GOG and Heroic, a background worker, a
  tray icon and opt-in maintenance for each location.
- Native APFS compression and restoration on Mac.
- Storage plans for each drive before a job starts, and a Recovery section for
  interrupted jobs and retained copies.
- Remembered games for drives that are disconnected.
- Compatibility reports from the app. The form measures allocated bytes, and
  `flummox compatibility measure` prints the same figure.
- Custom game folders, as a single game or a collection.
- A signed manifest covering every download.
- A changelog link in Settings > About.

### Changed

- Maximum is a guided flow. The game's details say where it stands and offer
  the one next step: play the game, then confirm to delete the original.
- After you confirm, game updates are folded into the store automatically
  while the game is closed, and the previous version is deleted after you
  next play. The buttons for doing either by hand moved under Advanced.

- Each game has one choice of how to compress it: Standard or Maximum, each
  with what it is predicted to save. The preset list and store controls moved
  under Advanced. Maximum can be chosen for a game without a compatibility
  report; the original is kept until you confirm the game runs.
- Compress starts straight away when the drive has room and leaves you on the
  page you were on. It used to open a storage plan to confirm and jump to
  Settings.
- Jobs have their own page in the sidebar.
- Queued jobs behind a job you paused say what they are waiting for.

- The Games page opens with the games worth compressing at the top, grouped
  as Worth compressing, Not analyzed yet, Compressed and Little to gain.
- A compressed game says what it gained, on its row and in History. Native
  results are estimates and say so; Maximum Space results are the store's own
  sizes.
- The window uses Compress, Decompress and Analyze throughout, on every
  platform. Optimize, Recheck, Restore and Reclaim are gone as button names.

- A newer Flummox replaces an older idle background worker by itself. Before,
  it asked you to run `flummox jobs restart` or log out.
- A compress job reports its progress once and in order. The bar used to fill
  during sampling, empty, and fill again.
- Linux downloads no longer carry debug data.
- Building a Maximum Space store uses every processor core. A 4 GB game that
  took 26 minutes takes about 4, and the store comes out byte for byte the same.

### Security

A [source audit](docs/security/2026-10-08-audit.md) led to these changes. Its
open items are listed there.

- Reclaiming the original verifies the whole store first.
- Compaction waits until the original has been reclaimed, so restoring cannot
  drop updates.
- A Steam manifest can no longer point a game at a folder outside its library.
- `flummox watch enable` quotes the program path it writes into the service.
- A slow or stuck client can no longer hold up the background worker.
- A game is chosen by id before title, and an empty name is refused.
- A job's sandbox no longer reaches the background worker's own files.
- Restoring a Maximum Space game never writes through a link inside it.
- In a Maximum Space game, a folder that an updater moves aside and recreates
  starts empty, as it would on an ordinary drive.
- Steam games with old-encoding names are listed, and manifests Flummox had
  to skip appear as scan warnings.

### Fixed

- Scrolling with the mouse wheel rebuilt the whole page on every frame, which
  made it stutter and filled the terminal with layout warnings. The window now
  keeps track of the scroll position without doing that.
- A game's details were a tall stack of single buttons and analysis text. The
  cover now sits beside the controls, the chosen mode is the filled button,
  the actions share one row, and the technical lines are under Advanced.

- Pressing Pause just as a job finished showed a lost-connection error that
  stayed on screen. It is now ignored.
- Compressing several games no longer fails entirely when one of them cannot
  be compressed that way; that game is left out and counted.

- The saved-space total no longer shrinks after a game update. A second pass
  analyses only the changed files, and its figure used to replace the first.
- The saved-space total no longer counts a Maximum Space game whose original
  has not been deleted yet.

- Windows: the background worker no longer crashes when it starts within a
  minute of boot, and Restore no longer stops at a file that was never
  compressed. Neither fix has been run on Windows yet.
- A job you paused before it started is still paused after the background
  worker restarts. It used to come back as interrupted.
- Excluding a game during a storage step that must finish waits for that step.

- Estimates cover the whole game. They used to total only the files that were
  sampled, which on a game with thousands of files showed a fraction of what a
  pass frees.

- A job that finished as a pause arrived no longer keeps the pause message.
- A History row for one file reads "1 file".

## 0.0.1

The first public release. [Full notes](docs/releases/0.0.1.md).

- Linux: Steam, Heroic and Lutris discovery, native btrfs compression, and
  Maximum Space stores mounted at the launcher's path.
- Windows: Steam discovery, WOF/LZX compression, a per-user installer and a
  portable ZIP.
- Mac: an informational app only.
- Background jobs that can be paused, resumed, cancelled and retried.
