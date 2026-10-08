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
