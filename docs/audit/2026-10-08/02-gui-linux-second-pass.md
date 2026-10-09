Second-pass bug hunt, Linux front end. Read-only: no cargo and no edits, so nothing below was executed. CONFIRMED means I traced the full path in source, including iced 0.14 and the worker where relevant. All paths are under the repository root.

## Findings, most severe first

**1. Page scroll, focus and open menus reset whenever a toast, the scan banner, the wizard or the plan panel appears or disappears. High. CONFIRMED.**
- Where: src/gui/view.rs:190, 207, 223 (conditional pushes ahead of the page surface at 283) and 326-365 (`return base` versus `stack([base, overlay])`).
- Defect: the widget tree changes shape, and iced diffs children by position.
  - `Container` delegates `tag`, `children` and `diff` to its content (iced_widget container.rs:236-250). Row, Column and Stack all have the stateless tag.
  - Toast path: `Stack.diff` reuses the Row's tree, lines `base` up against the old sidebar subtree, then `body` against a `Space` tree with no children. Every body child is rebuilt, including the page `Surface` and its scrollable.
  - Banner, wizard and plan path: a child inserted before the surface shifts it one slot. Old slot 0 has the `Motion` tag and the new one is stateless, so it is rebuilt.
- Scenario: scroll down Settings and click "Export local diagnostics" or a "Maintain new installs" checkbox. The toast appears and the page jumps to the top. Four seconds later the toast leaves and it jumps again. A search box loses focus mid-typing the same way.
- Banner frequency: the worker scans every 30 s idle and every 3 s while jobs run (src/jobs/service.rs:1497-1510). Any poll that lands mid-scan shows the banner, which gives two resets per occurrence.
- Fix: keep the tree shape constant. Always return `stack([base, overlay_or_empty])`, and give banner, wizard and plan fixed slots (a zero-height `Space` when absent) or place them after the surface.

**2. A game running from a mounted Maximum Space store is scanned as unsupported. High. CONFIRMED.**
- Where: src/gui/app.rs:117-140 (`GameRow::probe`), against src/fsprobe.rs:180 and src/storage.rs:93.
- Defect: the store is FUSE-mounted at the game's own path (src/pack/mount.rs:1150, FSName `flummox-pack`). `fsprobe::probe(install_dir)` returns that mount, and `tier_for` maps `"fuse"` to `Unsupported`. `storage::volume_existing` special-cases this mount; the GUI probe does not.
- What the user sees for every activated game:
  - the status line reads "Compression isn't supported on this drive yet." in place of "Play it, then confirm" or "Compressed";
  - no Details button and a disabled checkbox (view.rs:907);
  - `capture_order` files it under the collapsed "Little to gain" group (app.rs:622);
  - it counts in "N items need attention" and in the Attention filter;
  - its folder appears as its own entry under Drives and in the drive filter.
- The preview fixtures hard-code `supported: true` for the stored games, so they cannot catch this.
- Fix: in `probe`, when magic is FUSE and source is `flummox-pack`, probe the mountpoint's parent as `storage.rs` does. Alternatively pass `snapshot.packs` into `scan`.

**3. "Compress to save about X" includes excluded games, and one refused game aborts the rest of the batch. High/medium. CONFIRMED.**
- Where: src/gui/app.rs:777-784 (`potential_saving`), 1760-1773 (`OptimizeLibrary`), 908-919 (`actionable_selection`), 1072-1083 (`send_many`); src/jobs/service.rs:420-427.
- Defect: only `filtered()` honours `snapshot.excluded`. Excluding cancels active jobs but keeps the finished analysis, so `estimate()` still returns a saving. The worker refuses the enqueue, and `send_many` stops at the first error.
- Scenario: analyze the library, exclude one worthwhile game, press the Overview button. The window jumps to Jobs, games ahead of the excluded one in `state.games` are queued, and the rest never are. The toast says "This game is excluded. Restore it in Drives first." Every later press stops at the same game.
- The same hole exists for a ticked game that is later excluded or filtered out. Selection is only pruned on scan, and only by existence and `supported`.
- Fix: one `eligible(row)` predicate that checks exclusion, used by `potential_saving`, the Overview counts, `OptimizeLibrary` and `actionable_selection`. Make `send_many` collect per-command errors and continue.

**4. A refused automatic analysis is reported as a lost worker connection, retried every second, and blocks every game after it. Medium. CONFIRMED.**
- Where: src/gui/app.rs:1249-1255, 1557-1560, 1659-1663.
- Defect: the batch aborts on the first `?`. `AnalysisQueued(Err)` is forwarded as `Message::Snapshot(Err)`, which sets `connection_error` and shows a sticky error toast. The refused game still has no job, so the next poll picks it first again.
- Reachable refusals in `enqueue_job`: "The queue is full" at 200 active jobs (service.rs:~436), and `validate_folder` failing (folder gone since the scan, or under `~/.config`, `~/.cache`, `/usr`).
- Scenario: "Compress library" on 200+ games, or "Show more" to 240 rows on a fresh library. From then on every second shows "The queue is full…", the Jobs page flashes "Worker connection interrupted", and no later game is analyzed.
- Fix: handle `AnalysisQueued(Err)` separately as a quiet notice. Remember refused ids so they are skipped, and continue past a refused game inside the batch.

**5. Automatic analysis stalls once 40 "Worth compressing" games fill the first page. Medium. CONFIRMED.**
- Where: src/gui/app.rs:1228-1241.
- Defect: candidates come from `filtered().take(state.shown)`, and the Worth order puts group 0 first. Each recapture leaves analyzed worthwhile games on top, so fewer unanalyzed rows fit in the first 40.
- Rough numbers at a 60% hit rate: analysis stops after about 67 games. That leaves about 8 of 75 unanalyzed, or about 183 of 250.
- Effect: Overview's "Compress to save about X" and `OptimizeLibrary` cover only the analyzed part, with nothing saying so. A user who never opens Games gets the same result, because the Games filter and search drive analysis from every page.
- Related: an open pane or a ticked row sets `order_stale` and skips the recapture, which also stops progress.
- Fix: choose candidates from all eligible games (largest first, capped per batch), independent of the visible page.

**6. After a cancelled or failed compression, or any decompression, the game is stuck at "Not analyzed yet". Medium. CONFIRMED.**
- Where: src/gui/app.rs:657-671 (`estimate`), 1235-1237.
- Defect: `take_while(Analyze || Queued)` ends at any started non-analysis job, so the earlier estimate stops counting. `analyze_visible` skips any game that has a job for its build, so nothing re-analyzes it.
- Scenario: press Compress, then cancel. The row drops from "About 3 GB to save" to "Not analyzed yet", falls out of the Overview total and out of `OptimizeLibrary`, and stays there until the user finds Details → Analyze.
- Fix: stop the search only at a completed Compress or Decompress. Alternatively let `analyze_visible` requeue when `estimate()` is `None` and the game is not compressed.

**7. `result()` reports the last pass's estimate, not the game's recorded saving. Medium. GUI side CONFIRMED, worker behaviour taken from its own comment.**
- Where: src/gui/app.rs:552-566.
- Defect: `job_outcome` on the newest completed Compress job is preferred over the record. The worker comment at src/jobs/worker.rs:~458-463 says a pass after an update analyses only changed files, and it carries the earlier share into the record via `carried_saving`. The GUI reads the job first.
- Scenario: a 5 GB saving becomes "About 200 MB saved (estimate)" after a small update and a maintenance pass, or "Compressed · little to save" after a same-build recompress or Retry. "SPACE SAVED" drops with it. When that job is pruned from the 300 kept, the figure flips back up.
- Fix: prefer the record for a matching build, and use the job estimate only when no record exists.

**8. The worker's 300-finished-job cap makes job-derived state flip and can keep analysis running indefinitely. Medium. PLAUSIBLE (logic traced; needs more than 300 finished jobs).**
- Where: src/jobs/service.rs:~466-478, against src/gui/app.rs:1235, 657, 822-832.
- Defect: a game whose only job is an old analysis loses its estimate when that job is pruned. It reverts to "Not analyzed yet" and leaves the potential total. If visible it is re-analyzed, and that new job prunes the next-oldest.
- Scenario: about 250 games plus about 50 other finished jobs, with "Show more" open. Analyses cycle for as long as the window is open.
- Related: when a cancelled Compress or failed Decompress job is pruned, `compressed()` falls back to `records`. The record still exists (decompress only sets `level=0, est_saving=0` unless it finishes cleanly, worker.rs:144 and 485), so the game flips to "Compressed".
- Fix: persist the latest estimate per game and build outside the job list. Ignore records with `level == 0`.

**9. Each view rebuild rescans the job list several times per game, and rebuilds run once per frame during any animation. Medium. CONFIRMED structure, cost estimated.**
- Where: src/gui/view.rs:386-407 and 773-900; src/gui/app.rs:837-895.
- Per row, `game_row` calls `compressed`, `latest` (three times), `result` (up to three), `prospect` and `choice_for` (each also scans `games`) and `estimate`. That is about 9 linear scans of `snapshot.jobs` comparing `PathBuf`s.
- The job list holds at most 500 (300 finished plus 200 active).
- Estimated comparisons per rebuild:

| Page | Path comparisons |
|---|---|
| Games, 40 rows | about 180k |
| Games, 250 rows | about 1.1M |
| Overview, 250 games | about 700k, plus 62k id comparisons |

- At tens of nanoseconds each, the two large cases come to roughly 15 to 50 ms, which is over a 16 ms frame.
- `filtered()` also allocates a `String` per `worth()` call inside the sort comparator (about 4000 per sort at n = 250) and lowercases every title. It runs again each second in `analyze_visible`.
- Bulk queueing: `send_many` and the analysis batch make one request per game, each returning the full snapshot. The worker accepts one client per 50 ms pass, so 250 games take at least 12 s and parse 250 full snapshots.
- Fix: build a per-snapshot index (`HashMap<install_dir, per-game summary>`) once in `update` and read it from the view. Add a batch enqueue command.

**10. Every applied scan re-sorts the list even with a pane open or rows ticked. Medium/low. CONFIRMED.**
- Where: src/gui/app.rs:1525.
- Defect: `Scanned` calls `capture_order()` unconditionally, which also clears `order_stale`. The `expanded`/`selected` guard at 1640-1645 is bypassed. A scan follows every finished non-analysis job and every discovery change.
- Scenario: during a bulk compress with a detail pane open, the list reorders after each job, and the "Sort again" banner disappears without the user pressing it.
- Fix: apply the same guard in `Scanned`.

**11. In-progress analysis estimates are treated as final. Low/medium. CONFIRMED.**
- Where: src/gui/app.rs:664-670; src/jobs/worker.rs:387.
- Defect: the worker sends a partial `Estimate` after every file, and `estimate()` does not exclude `Analyzing`, `Paused` or `Cancelling`.
- Scenario: a game being analyzed for the first time shows "Little to save", then climbing figures. Overview's total and `OptimizeLibrary` use partial sums. Any `capture_order` in that window (typing in search, changing a filter) can drop the game into the collapsed "Little to gain" group until the next capture.
- Fix: use estimates only from analyses in `Completed` or `Partial`.

**12. "Library settings saved." appears on every launch for anyone with a custom location or a maintained library. Low/medium. CONFIRMED.**
- Where: src/gui/app.rs:1587, 1650-1652.
- Defect: `libraries_changed` compares against the default empty snapshot. The `first` flag exists but is not consulted.
- Fix: require `!first`, or show the toast only from the `Commanded` reply to a `Library`/`RemoveLibrary` command.

**13. "Review game and storage" closes the pane if that game was already the open one, and never brings the row into view. Low/medium. CONFIRMED.**
- Where: src/gui/app.rs:1684-1698, sent from view.rs:1421.
- Defect: `ReviewGame` reuses `Expand`, which toggles when `expanded == id`. The target row can also be hidden by the search, a filter, the 40-row page or the collapsed group.
- Fix: set `expanded` and force `detail` to open. Clear the filters or raise `shown` when the row is not in `filtered().take(shown)`.

**14. `Refresh` is dropped while a scan is running, so a just-saved artwork override or compatibility report may not appear. Low. CONFIRMED.**
- Where: src/gui/app.rs:1497-1499, reached from 1431 and 1365.
- Defect: the in-flight scan built its artwork index and report list before the save. Nothing schedules another scan, and the `Scanned` staleness check only looks at epoch, generation and game list.
- Fix: a `rescan_wanted` flag that `Scanned` honours.

**15. GUI scans are discarded on `scan_generation` alone. Low. CONFIRMED.**
- Where: src/gui/app.rs:1513-1520.
- Defect: the generation increments on every worker scan, every 3 s while jobs run, even when `discovered` is identical. Roughly half the GUI scans in that state are thrown away and rerun at once.
- Cost of each: a full `/proc/self/mountinfo` parse per game, a stat of every file in Steam's `librarycache`, and a DB open.
- When the scan's snapshot is newer than the last poll, the retry repeats until the next poll arrives (up to 1 s).
- Fix: compare `discovered` and epoch only.

**16. An evicted artwork tile stays blank, and decodes are strictly first-in first-out. Low. CONFIRMED.**
- Where: src/gui/artwork.rs:294-301; view.rs:797-799.
- Defect: the sensor has no `on_hide`, so `has_popped_in` never resets (iced sensor.rs:224-236) and `on_show` fires once per key. With 250 row icons plus covers the 256-entry cap evicts the oldest icons, and they are not requested again until the tree is rebuilt.
- There is no decode loop. `waiting` is never pruned, so a cover opened after a fast scroll waits behind up to 250 icon decodes at two at a time.
- Fix: touch entries on `get`, or request from `update` for visible rows. Put the newest request at the front.

**17. The store path field cannot be emptied. Low. CONFIRMED.**
- Where: view.rs:1178-1185; app.rs:690-696.
- Defect: the field displays `store_path(game)`, which falls back to the default whenever the typed value is blank. Deleting the last character brings the whole default path back.
- Fix: show the raw `pack_paths` entry and use the default as placeholder text.

**18. "Show more" can add nothing visible. Low. CONFIRMED.**
- Where: view.rs:715, 753-755, 762.
- Defect: collapsed "Little to gain" rows count against `shown`. When the remaining rows are all in that group, the button stays and each press reveals nothing.
- Fix: page over visible rows only.

**19. The preview test's only assertion cannot fail on page content. Low. CONFIRMED.**
- Where: src/gui/preview_renderer.rs:50-58.
- Defect: it passes if any pixel has blue above 40. The light background is 0xF7, and in dark the sidebar title text satisfies it, so all ~27 renders pass with an empty page. There is no control that must fail.
- The fixture also sets `state.page` to `Page::Drives` and `Page::Recovery`, which `update` never does (`GoTo` stores `page.main()`). `drives.png` and `recovery.png` show screens the app cannot reach, and those two arms at view.rs:261-262 are dead in production.
- No fixture renders a toast or the scan banner, so the `stack` path in finding 1 is unexercised.
- Fix: assert on a region inside the page area per fixture, and add a blank-page control that must fail.

**20. dialog.rs: an orphaned picker and an ambiguous exit code. Low.**
- Where: src/gui/dialog.rs:93, 26.
- CONFIRMED: `Command::output()` runs on a detached thread with no kill on exit, so closing the window leaves the zenity or kdialog window open.
- PLAUSIBLE: exit code 1 is treated as cancel. Zenity also exits 1 when GTK cannot open the display, so Browse then does nothing, with no message and no kdialog fallback.
- Fix: keep the `Child` and kill it on shutdown. Treat exit 1 with non-empty stderr as an error.

## Checked and found sound
- **Flags:** `scanning`, `analysis_queuing` and `picker_busy` are reset on every path; `background` delivers an `Err` if its thread dies. `polling` is never set false, so it is dead state.
- **Dropped tasks:** no non-test `let _ = update(..)`. `Jump` chains the navigation task.
- **Snapshot ordering:** `revision` increments on every answered request (service.rs:1784), so stale replies are ignored and a healthy poll always clears `connection_error`.
- **Symlinked install paths:** discovery canonicalizes `install_dir` and merges games that share one, so job matching by path holds.
- **Jump and scroll restore:** operations run after the rebuilt UI has been laid out. `Anchor` measures in content coordinates. An offset beyond the new content is clamped by the scrollable.
- **surface.rs:** movements end at 100 ms. A shrinking or zero `maximum` is clamped. The wizard's scrollable sits outside the page surface, so there is no nested capture. Overlays and the toast's close button make the base cursor unavailable or levitating, so `is_over` is false. Home/End are skipped when a focused text input captured the key. The mutex cannot deadlock.
- **Redraw loop:** the `animate` surface can keep requesting frames after the frames subscription stops only until the next message, which the 1 s poll bounds.
- **Keyboard:** `keyboard::listen` delivers only ignored events, so Ctrl+F and Ctrl+R do nothing while a text field has focus and never fire by accident.
- **Numeric:** the `as f32` progress fractions are clamped and safe.
