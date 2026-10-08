# Live job states in the Linux window, 2026-10-07

Status: closes the running and paused GUI states left open by the
[desktop delivery record](2026-10-06-desktop-polish.md). It does not qualify
real games.

## Setup

A release build of `994ac04` with `gui,pack-mount` ran under headless Gamescope
and Xwayland with the software renderer. A sandbox mounted a disposable home
over the real one, so no installed library was reachable. The window listed no
games on a machine that has a Steam library, which confirms the isolation.

The fixture game held 20 files of 134,217,728 bytes on btrfs. Each 128 KiB
block was half random bytes and half text, which gives zstd measurable work.
A second one-file game of 8,388,608 bytes supplied a waiting job. Jobs were
queued over the coordinator socket. Pause and Resume were clicked in the window
with `xdotool`.

Every screenshot was taken between two coordinator snapshots. A capture counts
only when both snapshots report the same phase.

## Results

- **Running and Waiting.** A Max preset compression showed under Running with
  its progress bar, file name, counts, Pause and Cancel. The second game showed
  under Waiting. The coordinator reported Running at 4 of 20 files on both
  sides of the [capture](desktop-polish/live/jobs-running-waiting-live.png).
- **Paused.** Clicking Pause on the running row moved the coordinator to
  Paused with `user_paused` set, at 5 of 20 files and 671,088,640 bytes. The
  row changed to Paused with a Resume button
  ([capture](desktop-polish/live/jobs-paused-live.png)). The phase and counts
  were unchanged after 5 seconds, after switching to
  [Overview](desktop-polish/live/overview-paused-live.png), and after
  returning to Settings.
- **Resumed.** Clicking Resume continued from the paused position. The next
  snapshots reported 8 and then 9 of 20 files, and the job completed at 20.
- **Bytes.** SHA-256 of all 20 files matched before and after a decompress,
  compress, pause and resume cycle. Comparing against an altered hash list
  failed, so the comparison can fail. `filefrag` reported at least 1,023 of
  1,024 extents encoded in each sampled file afterwards.
- **History.** Completed jobs listed their operation, file count, bytes and
  duration ([capture](desktop-polish/live/jobs-history-live.png)).

## Defects found and fixed

A compression job filled its progress bar twice. The worker reported totals for
its sampling pass and the backend reported them again for the rewrite, so the
counts ran to 17 of 20, dropped to 1 of 20 and climbed again. Storage jobs now
report totals once, when rewriting starts, and show the file being analyzed
before that. `a_compress_worker_reports_its_totals_once` reads a worker's event
stream and fails against the previous worker with "progress moved backwards".
On the rebuilt window, 23 distinct coordinator samples of one job showed a
single set of totals and no decrease.

A History row for one file read "1 files". It now reads "1 file".

## Observed and left unchanged

- The window refreshes once a second. A row can offer Pause for up to a second
  after its job finishes. Clicking it then shows "This job has finished", which
  stays until dismissed.
- A paused job keeps the single storage slot, so waiting jobs do not start
  until it resumes or is cancelled.
- A job cancelled while queued appears in History as Stopped with Retry. It
  does not appear under Needs attention.
- On this 16-thread NVMe machine the 2.68 GB fixture compressed in 5 to 11
  seconds, including about a second of sampling.

## Limits

Input was synthetic X11 input, so physical wheel and trackpad behavior is still
unverified. The renderer was tiny-skia. The harness closed the window with
SIGTERM, so this run says nothing about a normal exit. KDE and second-session
dialogs, real drive removal, Windows, Mac and every real-game qualification run
remain open.
