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

- A newer Flummox replaces an older idle background worker by itself. Before,
  it asked you to run `flummox jobs restart` or log out.
- A compress job reports its progress once and in order. The bar used to fill
  during sampling, empty, and fill again.
- Linux downloads no longer carry debug data.

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
