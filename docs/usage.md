# Using Flummox

The window, the command line, and automatic compression of new downloads.
Capabilities by platform are in [status](status.md).

## Use the window

Open `flummox-gui` on Linux and choose Games. Analysis starts in the
background, and the list sorts itself into four groups: worth compressing,
not analyzed yet, compressed, and little to save. Press **Compress** on a game,
or tick several and press **Compress selected**. The job starts at once when
the drive has room, and the window stays where you were. A compressed game
shows what it saved; on btrfs that figure is an estimate and says so.

Open a game to choose how it is compressed:

- **Standard** is quick, and the game's files stay where they are.
  It is the default wherever the drive can compress in place.
- **Maximum** saves more and takes minutes. The game runs from a compressed
  store mounted at its usual path, and the original is kept until you confirm
  the game works. It is chosen one game at a time; compressing several games
  at once always uses Standard.

A game set to Maximum is walked through three steps. Press **Compress** and
Flummox builds the store and switches the game to it. Play the game once, then
press **It works, delete the original**; that is when the space is saved, and
the whole store is checked first. From then on, game updates are folded into
the store automatically while the game is closed, and the previous version is
deleted after you next play. **Decompress to ordinary files** undoes it at any
point.

**Decompress** puts a game back and **Analyze** estimates it again. Advanced
holds the Standard strength (Fast, Balanced or Max), the store location, and the
compatibility report form. Maximum compares levels 9, 15, 19 and 22 for each
unique chunk and keeps the smallest; ties use the cheaper level.

Browse for a game folder or paste its path. Native dialogs use KDialog on KDE
and Zenity elsewhere when available; a missing dialog helper leaves the path
field usable.

On Windows, choose a locally detected Steam, Epic, GOG or Heroic game, or add
another installed-game folder, then press **Compress**. Flummox reports files
processed and space freed while Windows works. **Stop** finishes the
current file and keeps completed work valid. **Decompress**, under Advanced,
removes WOF backing and leaves the same files at the same paths.

The window theme and responsive layout are shared across targets. Linux-only
storage controls appear only when their backend and FUSE support are available.
Mac uses built-in APFS compression with staged verification and durable replacement
journals. Unsupported desktop targets retain the informational shell.

On Linux the Jobs page lists Running, Waiting, Needs attention and History.
On Windows and Mac, Jobs is a section of Settings. A job you pause keeps its
place, and the jobs behind it say they are waiting for it. Linux jobs support
pause, resume, stop and retry for Standard compression and for switching a game
to Maximum. Creating a store and folding in updates report measured progress, and
checking a store reports its own progress. Switching files, decompressing and
deleting the original finish without interruption, with controls hidden during
those steps. Linux and Windows jobs
survive closing the window because the background worker keeps running. Mac jobs
currently run inside the app. Analysis and compression pause when a detected
game is running. A stop request, from the window or the command line, lets the
current Linux filesystem operation finish: workers stop between files and at
16 MiB range boundaries inside large files.

Settings > Locations lets you add a game or a games library. On Linux and
Windows, the **Maintain new installs and updates** checkbox on a location opts
it into background jobs. Opting in establishes a baseline, so existing installs
are not all compressed immediately. Later installations and completed updates
can queue jobs. Linux keeps a login entry while an opted-in location or a game
using Maximum needs the background worker. Windows login startup is a separate
opt-in.

`Ctrl+F` focuses game search, `Ctrl+R` refreshes, and `Escape` clears selection
and closes details. Tab and Shift+Tab move focus. The Motion setting (Smooth,
Subtle or Reduced) is saved in Settings > Appearance. Artwork comes from local launcher caches; no artwork or
telemetry is sent to a server.

On Linux, the command line shares ordinary compression and decompression jobs
with the window. A plain `compress` or `decompress` queues a job and follows
it. Interrupting the command with Ctrl-C disconnects the client and leaves the
job running in the background worker; `flummox jobs cancel` stops it:

```sh
flummox jobs
flummox jobs pause 12
flummox jobs resume 12
flummox jobs cancel 12
flummox jobs retry 12
```

The advanced `compress --force`, `compress --no-pause` and
`decompress --force` paths run in the command's own process instead of the
queue. They share an operation lock with the background worker and check free
space before they start, as a queued job does.

## Use the command line

```sh
flummox scan                        # every game, and what drive it is on
flummox estimate 105600             # what compressing it would save
flummox compress 105600 --preset max
flummox decompress 105600           # put it back
flummox status 105600               # how much is stored compressed
flummox log                         # what Flummox has done
flummox doctor                      # check this machine
flummox compatibility list --json  # export path-free compatibility reports
flummox compatibility measure DIR  # allocated bytes for a compatibility report
```

On Linux, import a locally produced compatibility report from Settings or an
expanded game, or with `flummox compatibility import report.json`. The Windows
and Mac window has no import control. Analysis
hashes installed files when a candidate report matches the game build. Automatic
switching checks the game files again before the store is created and before the
game switches to it. Reports identify a launcher key, build and
corpus hash (a fingerprint of the game files). They contain no game title, install path, user name or machine
identifier. Automatic Maximum accepts only a matching verified build
whose measured load-time change stays within policy.

Steam reports some folders that are not games, like shared redistributables
and runtimes. Flummox filters the obvious ones, and you can correct the rest:

```sh
flummox exclude add "Steamworks Shared"   # leave it out
flummox exclude list
flummox exclude remove 105600
```

An excluded game stays out of the games list and will not be compressed even
if you name it directly. `exclude remove` takes an ID, a whole title or part of
a title, tried in that order. It refuses a partial title that matches several
excluded games, and an empty one, and lists the matches so you can choose.

Name a game by its Steam app ID, by `steam:105600`, or by part of its title.
Presets are `fast`, `balanced` and `max`. `--level` overrides the preset with
a zstd level from -15 to 15, and 0 is refused. `--threads` takes 1 to 32.

Every read-only command takes `--json`. `doctor --json` prints one object
with a `checks` list, and each entry has a `name`, a `status` (`ok`, `warn` or
`off`) and a `detail`. `drives --json`, `watch status --json` and
`compress --dry-run --json` work too.

Flummox will not start on a game that is running, updating or being verified.
If you launch the game while a job is working, the job pauses and waits for
you, then picks up where it left off. `compress --no-pause` stops the job when
the game is launched instead. Run the same command again to finish the rest.

Ctrl-C works in two steps. On the default queued path it only disconnects: the
job continues in the background worker until you run `flummox jobs cancel`. On
the direct path (`--force`, `--no-pause`) the first Ctrl-C asks the pass to stop
at the next safe point, between files or at a 16 MiB boundary inside a large
file, and a stopped pass leaves a game that is partly compressed, still
playable, and safe to resume. A second Ctrl-C exits at once with status 130
(143 for SIGTERM).

A direct `compress`, or `decompress --force`, exits with a non-zero status when
any file failed or the pass was cancelled. A failed `decompress --force` keeps
the game's record, because the game is still partly compressed.

Relative paths given to `jobs add-folder`, `jobs remove-folder` and the
`pack` commands are resolved against the current directory before they reach
the background worker.

`flummox compatibility import` refuses a report larger than 1 MiB.

Filesystems other than btrfs have no in-place compression. bcachefs is listed
as unsupported: `scan` and `drives` report that it has no backend yet.

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
It reads Steam's own manifests and queues a game once its download has
settled, which picks up the files the filesystem skipped on the way past. It
waits behind anything else using the disk. It queues a finished download even
if the game is already running; the queued job pauses until you close the
game, so the watcher never touches a game you are playing.

`watch enable` writes the unit to `~/.config/systemd/user/flummox-watch.service`
with the preset, level, threads and dry-run setting you passed, for example
`flummox watch enable --preset max --threads 4`. The unit carries the same
hardening as the unit shipped in the packages. The user copy takes precedence
over the packaged one, so the packaged unit is never the one that runs once you
have enabled the watcher this way.

`flummox watch status` says whether it is on, and `flummox watch disable`
turns it off. Running `flummox watch` with no argument does the same work in
the foreground, if you would rather watch it happen.

`flummox hook off` stops it applying to future downloads and leaves everything
already compressed as it is.

`flummox hook on` exits with a non-zero status when no library took the
property, for example when none of your Steam libraries is on btrfs.

## On macOS

The macOS build takes a folder, not a game selector. Its commands are
`scan`, `analyze FOLDER`, `compress FOLDER`, `decompress FOLDER`, `recovery`
and `recover FOLDER`. `status`, `log`, `doctor`, the `--preset` option and
`--json` do not exist there. Progress goes to stderr, and the result is printed
to stdout.
