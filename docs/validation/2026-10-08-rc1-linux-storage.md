# 0.0.2-rc.1 on a real game: Linux storage cycle

The released `flummox-0.0.2-rc.1-linux-x86_64` binary, run on a disposable
copy of Firewatch (Steam build of 2026-10-08, 6,734 files, 4,252,003,098
bytes) on btrfs mounted `compress=zstd:1`. The copy was made with
`cp --reflink=never` and matched the original by SHA-256 before anything ran.
`HOME` and the XDG folders pointed at an empty fixture, and `flummox scan`
listed one game, so the real library and state were out of reach.

This covers storage only. The game was not launched, so it says nothing about
loading or play, and it is not one of the acceptance runs `0.0.2.json` needs.

## Native tier

| Step | Result |
|---|---|
| `compatibility measure` on the copy as written through the mount | Refused: 471 of 6,734 files held compressed extents |
| `decompress` | 11 s |
| `compatibility measure` after it | 4,265,816,064 allocated bytes |
| `compress`, balanced | 45 s |
| SHA-256 of every file afterwards | Identical to the original |

The drive's data counter fell by 1,531,133,952 bytes during that pass and by
1,815,007,232 during a second decompress and compress later in the session.
The counter covers the whole drive and includes other processes' writes, so
these falls are not the saving of the job. It moved by 0 and by 12,095,488
bytes in two idle 20-second windows beside the second pass, which shows how
quiet the drive was in those windows and does not turn the counter into a
measurement. The sizes of the copy before and after a pass need `compsize`,
which needs privileges Flummox does not ask for, so this record has no
measured saving for the native tier.

## Estimates against that pass

| Estimator | Predicted saving |
|---|---|
| 0.0.2-rc.1 | 438 MB before the first pass, 319 MB before the second |
| With unsampled files scaled in | 1,516 MB before the second |

The released estimator inspected 6,206 files and left 389 unsampled, and those
included the largest. The change samples the largest first and scales the rest.

## Maximum Space

| Step | Result |
|---|---|
| `pack create --maximum` | 26 min 13 s on one core; store verified |
| Store allocation by `compatibility measure` | 1,917,288,448 bytes, 44.9% of the uncompressed baseline |
| `pack activate` | Mounted at the game's path, original retained beside it |
| SHA-256 of every file read through the mount | Identical to the original; 8.2 s for the whole game |
| Rename a folder aside, create a new one of the same name | New folder empty, moved folder kept its 72 entries |
| `pack reclaim` | 4.4 s, including verifying every chunk |
| `pack rollback` | 30 s; ordinary files at the game's path, no install record left |
| SHA-256 afterwards | Every original file identical except the one edited through the mount; the file created through the mount present with its content |

## What this found

- The estimate was three to five times too low. Fixed after the candidate.
- Building a Maximum Space store uses one core and took 26 minutes for a
  4 GB game.

The copy, the store and the fixture home were deleted afterwards.
