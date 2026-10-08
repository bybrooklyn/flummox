# Using Flummox

The window, the command line, and automatic compression of new downloads.
Capabilities by platform are in [status](status.md).

## Use the window

Open `flummox-gui` on Linux, choose Games, select your games, and press **Free
up space**. Analysis starts in the background. Search, drive and launcher
filters, and sorting help with larger libraries. Expanding a game shows its
path, compression preset, analysis, and recovery actions. Browse for a game folder
or paste its path. Native dialogs use KDialog on KDE and Zenity elsewhere when
available; a missing dialog helper leaves the path field usable. Maximum compares
levels 9, 15, 19, and 22 per unique chunk and keeps the smallest result;
ties use the cheaper level. Balanced remains the quicker native default.

On Windows, choose a locally detected Steam, Epic, GOG or Heroic game, or add
another installed-game folder, then press **Optimize**. Flummox reports files
processed and allocated bytes freed while Windows works. **Stop** finishes the
current file and keeps completed work valid. **Restore** removes WOF backing
and leaves the same files at the same paths.

The window theme and responsive layout are shared across targets. Linux-only
storage controls appear only when their backend and FUSE support are available.
Mac uses built-in APFS compression with staged verification and durable replacement
journals. Unsupported desktop targets retain the informational shell.

Settings > Jobs groups running, waiting, attention and completed work. Linux
jobs support pause, resume, cancel and retry for native work and Maximum Space
preparation. Store creation and compaction report measured progress; verification
reports checked items. File switches, restoration and reclaim finish without
interruption, with controls hidden during those phases. Linux and Windows jobs
survive closing the window because their coordinators keep running. Mac jobs
currently run inside the app. Analysis and compression pause when a detected
game is running. Current Linux filesystem operations finish before workers stop
at 16 MiB range boundaries, including inside large files.

Settings > Locations lets you add game folders. On Linux and Windows,
Settings > Maintenance opts libraries into background work. Opting in
establishes a baseline, so existing installs are not all compressed
immediately. Subsequent installations and
completed updates can queue work. Linux maintains a login entry while an opted-in
library or mounted install needs its coordinator. Windows login startup is a
separate opt-in.

`Ctrl+F` focuses game search, `Ctrl+R` refreshes, and `Escape` clears selection
and closes details. Tab and Shift+Tab move focus. Reduce motion is saved in
Settings > Appearance. Artwork comes from local launcher caches; no artwork or
telemetry is sent to a server.

On Linux, the command line shares ordinary compression and decompression jobs
with the window. Interrupting the CLI disconnects the client and leaves its
job running:

```sh
flummox jobs
flummox jobs pause 12
flummox jobs resume 12
flummox jobs cancel 12
flummox jobs retry 12
```

The advanced `--force` and `--no-pause` paths retain their direct execution
behavior and share an operation lock with coordinator workers.

## Use the command line

```sh
flummox scan                        # every game, and what drive it is on
flummox estimate 105600             # what compressing it would save
flummox compress 105600 --preset max
flummox decompress 105600           # put it back
flummox status 105600               # how much is stored compressed
flummox log                         # what Flummox has done
flummox doctor                      # check this machine
flummox compatibility list --json  # export path-free qualification records
flummox compatibility measure DIR  # allocated bytes for a qualification
```

Import a locally produced compatibility qualification from Settings or an
expanded game, or with `flummox compatibility import report.json`. Analysis
hashes installed files when a candidate report matches the game build. Automatic
activation checks the corpus again before creation and before switching storage. Reports identify a launcher key, build and
corpus hash. They contain no game title, install path, user name or machine
identifier. Maximum Space automation accepts only a matching verified build
whose measured load-time change stays within policy.

Steam reports some folders that are not games, like shared redistributables
and runtimes. Flummox filters the obvious ones, and you can correct the rest:

```sh
flummox exclude add "Steamworks Shared"   # stop seeing it
flummox exclude list
flummox exclude remove 105600
```

A hidden entry stays out of scans and will not be compressed even if you name
it directly.

Name a game by its Steam app ID, by `steam:105600`, or by part of its title.
Presets are `fast`, `balanced` and `max`. Every read-only command takes
`--json`.

Flummox will not start on a game that is running, updating or being verified.
If you launch the game while it is working, it pauses and waits for you, then
picks up where it left off. Pass `--no-pause` if you would rather it stop.

Ctrl-C stops between files, so a cancelled job leaves a game that is partly
compressed, still playable, and safe to resume.

## Compress new downloads automatically

Turn this on once:

```sh
flummox hook on
```

Every game you install or patch after that arrives compressed, because the
filesystem compresses the bytes the first and only time they are written. It
costs nothing: there is no second pass, no rewriting, and no time added to the
download. Games already installed are untouched, so run `flummox compress` for
those.

The hook does not catch everything. The filesystem judges each file as it is
written, using whatever compression level the drive was mounted with, and it
skips files it guesses will not pay. It guesses wrong often enough to matter.
Measured on this machine: a 37 MB Half-Life texture archive arrived
uncompressed, and a later `flummox compress` brought it to 26 MB. A 27 MB
Unity asset file arrived at 20 MB, and a pass took it to 17 MB. Running
`flummox compress` after a large download is still worth doing.

So turn on both halves:

```sh
flummox hook on          # compress during the download, at no cost
flummox watch enable     # collect the rest once it finishes
```

`flummox watch enable` starts a small background service that runs at login.
It reads Steam's own manifests and compresses a game once its download has
settled, which picks up the files the filesystem skipped on the way past. It
waits behind anything else using the disk, and it will not touch a game you
are playing.

`flummox watch status` says whether it is on, and `flummox watch disable`
turns it off. Running `flummox watch` with no argument does the same work in
the foreground, if you would rather watch it happen.

`flummox hook off` stops it applying to future downloads and leaves everything
already compressed exactly as it is.
