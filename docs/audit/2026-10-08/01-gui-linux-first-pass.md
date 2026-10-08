# Linux front end, first pass

Read of `src/gui/app.rs`, `view.rs`, `theme.rs`, `surface.rs` and `mod.rs`.
Every item was traced in the source. Line numbers are from commit `3ba01b7`.

## Fixed in this branch

- **Page scroll reset when a toast or the scan banner came or went.**
  `view.rs` pushed the banner, wizard and plan conditionally and returned a
  different root with and without a toast, so iced matched the page surface
  against the wrong widget state. Each now keeps its slot.
- **Buttons drew as disabled during an eased wheel scroll.** `surface.rs`
  sent a synthetic wheel event on each frame. The scrollable captured it, and
  an iced button given a captured redraw event returns before refreshing its
  status. The surface now sets the offset with a scroll operation.

## Open

1. **`expanded` is never cleared.** `app.rs:1688` toggles `detail` and leaves
   `expanded` set. After any game has been opened once, each finished analysis
   sets `order_stale`, the "Sort again" banner returns, and the list stops
   sorting itself. Fix: clear `expanded` when the close animation ends, or
   test `detail.value()` at `app.rs:1641` and `:1709`.
2. **The disconnect toast is shown again on every failed poll.**
   `app.rs:1659-1663` calls `show_status` once a second, which restarts the
   reveal animation and undoes a dismissal. Fix: show it on the transition
   from connected to disconnected only. The Jobs page already has a panel.
3. **"Start job" on the storage plan can never be pressed.** The panel is
   stored only when `plan.check()` fails (`app.rs:1373-1379`), `check()` reads
   the stored numbers (`storage.rs:312`), and `StartPlanned` calls `check()`
   again. Fix: offer "Check again" backed by `recheck()`. Name the game and
   the operation in the panel, and format the byte counts.
4. **The bottom job bar shows analyses that the Jobs page hides.**
   `State::active` (`app.rs:898`) takes any active job. "See jobs" then opens
   a page reading "No jobs running". Fix: exclude `Operation::Analyze`.
5. **Scan warnings are counted and never shown.** `view.rs:484-505` adds
   `state.warnings.len()` to "N items need attention" and nothing prints the
   warnings. "Review" opens Games without `Filter::Attention`. Fix: list the
   warnings in the card and set the filter.
6. **A toast drives a page rebuild on every frame.** `mod.rs:552` keeps the
   frame subscription alive while `status_deadline` is set, and each frame is
   a `Tick` message. Fix: a single delayed task for the deadline.
7. **Analyze and Decompress in the detail pane are not gated.**
   `view.rs:1015-1019` enables both while a job runs for the game and offers
   Decompress on a game that is not compressed. Fix: gate on `ready` and on
   `compressed`.
8. **The Overview headline contradicts its button.** With a predicted saving
   and nothing compressed yet, `view.rs:431-439` reads "Analyze your games"
   above "Compress to save about X". Fix: headline "About X to save".
9. **Restart worker has no guard.** `view.rs:1800` sends `Command::Restart`
   with one click though the text beside it asks for an empty queue. Fix:
   disable it while any job is active or a store is mounted, with the reason.
10. **Excluded games are listed by raw id.** `view.rs:1736` prints
    `steam:105600`. Fix: look the title up in `state.games`.
11. **Clicking a game's artwork opens a file picker.** `view.rs:804-807`.
    The rest of the row header expands the row. Fix: make the tile expand the
    row and move the override under Advanced, with a way to clear it.
12. **The bulk selection stays ticked after "Compress selected".**
    `app.rs:1756`. Fix: clear `selected` once the commands are sent.
13. **`total_bytes` and `Sort::Size` use the launcher size only.**
    `app.rs:513` and `:885`. A custom folder has none, so it counts as zero in
    "Installed" and sorts last while its row shows the analysed size. Fix:
    one `size_of(game)` used by the row, the total and the sort.
14. **"Show more · N games" prints the total.** `view.rs:764`. Fix: print
    the number still hidden.
15. **The Games empty state gives the same advice in every case.**
    `view.rs:681-696` says "Change the filters or add a folder" for an empty
    library, an active search and a scan in progress. Fix: three messages.
16. **The Settings sidebar entry does not animate.** `view.rs:140-172`
    hard-codes 0 or 1 though `state.nav` holds an animation for it. Fix: use
    the same loop as the other entries.
17. **Transition timings disagree.** `State::new` uses 240 ms and 200 ms,
    `GoTo` 180/120, `Motion` 220/140, and `motion_easing` returns the same
    easing from both arms. Fix: one table of constants.
18. **Escape leaves the plan and the wizard open.** `app.rs:2031-2034`.
    The Windows and Mac front end closes the plan. Fix: close the plan.
19. **The "Add folder" check blocks the window thread.** `app.rs:1924`
    calls `is_dir()` in `update`. A dead network mount stalls the window. Fix:
    validate in `background`.
20. **The first frame assumes a dark desktop.** `State::new` sets
    `system_theme` to Dark until the query answers. Fix: start from
    `iced::system::theme` before the window opens when it is available.
21. **The log filter prints the graphics library's probe warnings.**
    `mod.rs:124` defaults every crate to `warn`, so each launch prints
    `wgpu_hal` lines about EGL and Vulkan extensions. Fix: default to
    `warn,wgpu_hal=error,wgpu_core=error`.
