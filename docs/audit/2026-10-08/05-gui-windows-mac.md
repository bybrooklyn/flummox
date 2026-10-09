
# Audit of src/gui/native.rs (Windows and macOS front end)

Scope: read all of `src/gui/native.rs`, `view.rs`, `theme.rs`, `surface.rs`, `native_preview.rs`, `unsupported.rs`, `src/native.rs`, `docs/gui-architecture.md`, and the relevant parts of `app.rs`, `desktop.rs`, `windows/coordinator.rs`, `storage.rs`, `qualification.rs` and `artwork.rs`. Nothing was edited or run. The native module is cfg-gated off Linux, so no finding was rendered; layout claims rest on the code and the iced 0.14 sources in `~/.cargo/registry`.

Paths are relative to `the repository root`. "native.rs" means `src/gui/native.rs`.

## Findings, most severe first

1. **native.rs:1198-1205, 1365. The wide Games layout gives the controls column one sixth of the width.** CONFIRMED (code and iced flex source, not rendered)
   - `library` is `FillPortion(5)`. `action` is a bare `column!` whose `text_input(...).width(Length::Fill)` child makes it `Fill`, which is factor 1 (`Column::push` calls `enclose`, and `Length::fill_factor` returns 1 for `Fill`).
   - Scenario: at the default 900 px window, the content row is about 646 px, so the controls get about 108 px. The Compress, Decompress and Stop row needs about 330 px.
   - Cause: commit 596b21e removed `container(scrollable(action)).padding(18).width(FillPortion(6)).style(theme::panel)`. The column also lost its panel background, though the comment at 1207 still calls it a panel.
   - Fix: wrap `action` in `container(action).padding(16).width(FillPortion(6)).style(theme::panel)` in the non-compact branch, and `Fill` in the compact one.

2. **native.rs:1337-1344 (and 1634-1640, macOS only). The status banner is drawn only on the Games page, so Settings errors are invisible on Windows.** CONFIRMED
   - Every Settings action reports through `state.status`: `LocationResolved` Err (599), `PreferencesSaved` Err (552), `OpenChangelog` (766), `RecoveryScanned` Err (966), `FolderPicked` Err (635).
   - Scenario: on Windows, type a bad path under Locations and click "Add location". Nothing changes on screen. The error is waiting on the Games page.
   - On macOS the banner is also drawn in the Jobs section at the top of Settings, which is usually scrolled out of view from Locations.
   - Fix: draw the status once in the page shell, above the scrollable or as the Linux toast, and remove both per-page copies.

3. **native.rs:540, 1780-1799, 1830-1834. A command the Windows worker refuses is labelled a lost connection, shown on another page, and erased within a second.** CONFIRMED
   - `start` sends `Enqueue` through `worker_send`. A refusal (`anyhow::bail!(error)` at coordinator.rs:97-98) arrives as `Message::Worker(Err)` and is stored in `worker_error`.
   - It is rendered only in Settings > Jobs as `"Worker connection interrupted. Showing the previous jobs. {error}"`, and the next 1 s poll sets `worker_error = None` (521).
   - Scenario: click Compress, then "Start job" on a game the worker refuses ("This game is excluded from jobs", "Review the retained job history before adding more jobs"). The plan panel closes and nothing else happens.
   - This contradicts the doc rule "A command the worker refuses is shown for a few seconds and is not treated as a lost connection". Linux has a separate `Commanded` arm with an 8 s toast (app.rs:1565-1570).
   - Fix: give user-initiated commands their own result message that sets `state.status`, and keep `worker_error` for poll failures.

4. **native.rs:1780-1799. A successful "Start job" on Windows gives no feedback.** CONFIRMED
   - `start` sets no status and does not navigate. If the job waits behind another, or maintenance is paused, the Games page shows nothing at all.
   - Fix: set a status such as "Queued <title>", or go to the Jobs section as Linux does (`GoTo(Page::Queue)`, app.rs:1118).

5. **native.rs:1209-1212, 1254-1258; coordinator.rs:516-522. Compress stays enabled for an excluded game, and the exclusion label understates what it does.** CONFIRMED
   - The button reads "Exclude from background work", but the worker refuses manual compression too: `ensure!(restore || !...excluded..., "This game is excluded from jobs")`.
   - Scenario: exclude a game, then click Compress and "Start job". It fails as in finding 3.
   - Fix: disable Compress with a muted reason when the selected game is excluded, and rename the button "Exclude this game" (the Linux label).

6. **native.rs:915-918, 1283-1310. Every job requires a confirmation, and the plan panel does not say what it will start.** CONFIRMED
   - `Planned` always stores the plan and waits for "Start job". The doc says "A job starts without a confirmation when its space plan passes. The plan is shown only when it fails". Linux does that at app.rs:1373.
   - The panel destructures `Some((_, _, plan))`, so it names neither the folder nor whether this is Compress or Decompress.
   - It also omits `requirement.reasons` and the `plan.retained_original` line that Linux shows (view.rs:235-241).
   - Fix: start at once when `plan.check().is_ok()`, and show the panel, with operation and folder, only on failure.

7. **native.rs:507-518, 976-981, 1217-1220, 1312-1336. On Windows the Stop button and progress box in the "Selected folder" column act on whichever job the worker is running.** CONFIRMED
   - `Message::Stop` cancels the first running or paused job in the snapshot. `state.progress` mirrors that same job, with no title.
   - Scenario: an automatic job is compressing game A. Select game B and press Stop. Game A is cancelled, with no confirmation and no message.
   - `state.progress` is never reset to `None` on Windows, so "N files processed · … freed so far" stays after the job ends and after another game is selected. On macOS it is cleared only when the next job starts (410).
   - Fix: bind Stop and the progress box to the job whose `game.install_dir` matches the selected folder, title the box, and clear it when no job matches.

8. **native.rs:942-946 with 582-588. "Remember this folder" turns an existing Games library location into a single game.** CONFIRMED
   - `Remember` resolves with `LocationKind::Game`, and `LocationResolved` executes `old.kind = kind` for a path already listed.
   - Scenario: `D:\Games` is saved as "Games library". Type `D:\Games` as the selected folder and click "Remember this folder". After the rescan every game under it is gone from the list.
   - Fix: make `Remember` a no-op (with a status) when the path is already a location, or only add, never retype.

9. **native.rs:905-910, 534-536, 543-547. The banner never clears and is overwritten by "Found N remembered games."** CONFIRMED (the boot race is PLAUSIBLE)
   - There is no dismiss control and no deadline. Linux clears info after 4 s and keeps errors until dismissed.
   - `refreshing` is set only by the macOS timer, so on Windows every snapshot-driven rescan (534-536) replaces the banner, including an error.
   - At boot, `PreferencesLoaded(Err)` can be overwritten by a later `Scanned(Ok)`. `preferences_loaded` then stays false, so Add location, Remember, Exclude and Start at login stay disabled with no visible reason.
   - The error path at 897-903 returns without `state.refreshing = false`, so after a failed timer scan the next manual Refresh posts no result.
   - "Remembered" is also the wrong word: the count includes launcher-discovered games.
   - Fix: share Linux's `show_status` and deadline logic, set `refreshing` for worker-driven scans, and show a persistent inline message while preferences are unloaded.

10. **native.rs:1134, 1176, 1229, 1234, 1236, 1304-1306, 1352-1353, 1657-1659, 1688, 1703, 1714, 1747, 1866-1877. Most buttons use iced's default style, which is brighter than the primary action.** CONFIRMED
    - A button with no `.style` gets `button::primary` (iced_widget button.rs:588-590), a solid `accent` fill. `theme::action_button` fills with `accent_dim`.
    - So Compress is dimmer than Refresh, Browse…, Remove location, Cancel, and every row of the game list, which are all solid accent blocks.
    - The game rows (1176-1196) have no selected state. Nothing shows which game the right-hand column refers to.
    - Fix: move view.rs's `action`/`secondary` helpers into theme.rs and use them everywhere. Give the row whose `install_dir` matches `state.folder` a distinct style.

11. **native.rs:1167-1172, 1119. A false empty state shows during the first scan.** CONFIRMED
    - `scanning` starts true, but the branch tests only `state.games.is_empty()`.
    - Scenario: every launch shows "No games found. Add a game or games library in Settings" until the scan returns. Overview shows "0 Games".
    - Fix: show "Finding your games…" while `state.scanning && state.games.is_empty()`, as view.rs:685 does.

12. **native.rs:1209-1238, 1659-1660, 1498. Controls are enabled when their prerequisite is missing.** CONFIRMED
    - Compress, Decompress, "Qualify compatibility" and "Remember this folder" are gated only on `!state.working`.
    - With an empty folder field, Compress yields "Storage location is unavailable: " and Remember yields a raw OS error from `canonicalize`.
    - Qualify on a folder that is not a known game yields "Remember or select the game folder first." (734).
    - "Add location" is enabled with an empty input.
    - This contradicts "Controls are disabled before dispatch when a capability or prerequisite is unavailable".
    - Fix: gate on a non-empty folder, gate Qualify on the folder resolving to a known game (the lookup at 1243 already exists), and gate Add location on non-empty input.

13. **native.rs:1714; coordinator.rs:45. "Stop background worker" has no confirmation.** CONFIRMED
    - `Shutdown` is documented as "Cancel waiting jobs, stop the running one after its current file, then exit".
    - Scenario: one click discards the whole queue.
    - Fix: a two-step confirm when any job is active, or a muted line stating how many jobs will be cancelled.

14. **native.rs:1522-1529, 476-481, 1917. A stopped Windows worker is restarted by any preference change, and the window does not notice.** CONFIRMED
    - `save_preferences` calls `coordinator::request(Command::Settings)`, which spawns a worker when none answers (coordinator.rs:111-131). The reply is mapped to `()`, so `worker_enabled` stays false.
    - Scenario: stop the worker, then change Theme. A worker is running again, the button still says "Start background worker", polling stays off and the job list is frozen.
    - "Pause background work" also starts a stopped worker, because any `WorkerCommand` but Shutdown sets `worker_enabled = true`.
    - Fix: route the Settings reply through `Message::Worker`, and disable Pause/Resume while the worker is stopped.

15. **native.rs:1441-1444, 950, 1701-1705. Windows Recovery can list a job that is still running, and its restore skips the space plan.** PLAUSIBLE
    - The journal exists for the whole pass ("written before the first file is touched and removed after the last", windows/mod.rs:238). `boot` reads it unconditionally and the view does not cross-check active jobs.
    - Scenario: open the window during a background job. Its folder appears under Recovery with "Restore ordinary storage" enabled.
    - `Recover` calls `start(state, folder, false)` directly, without the plan Decompress goes through.
    - On macOS, `Recover` (952-961) sets `working` with no status line and no Stop.
    - Fix: hide records whose root has an active job, send Recover through `plan`, and set a status on start.

16. **native.rs:717-733, 841-844. Qualify has no busy state, and Escape discards the wizard.** CONFIRMED
    - `Wizard::start` "Hashes the game and measures its folder" (qualification.rs:88-91). During it nothing changes on screen, the button stays enabled, and a second click starts a second hash.
    - Escape sets `state.qualification = None`, discarding every typed field. It fires only when no widget captured the key, since `keyboard::listen` delivers ignored events only.
    - Fix: add a `qualifying` flag that disables the button and shows "Measuring…". Have Escape close only the plan.

17. **native.rs:1667-1695. The Windows maintenance checkbox sits outside the card it belongs to.** CONFIRMED
    - The checkbox is pushed before the location's container, so it reads as the footer of the previous card. That is why it has to repeat the path in its label.
    - Fix: put it inside the card's column, as view.rs:1722 does, with the label "Maintain new installs and updates".

18. **native.rs:1771-1773. Discovery warnings are unlabelled muted lines below About.** CONFIRMED
    - Scenario: a library on a disconnected drive produces a warning the user sees only by scrolling to the end of Settings.
    - Linux counts them in an Overview "N items need attention" card (view.rs:484).
    - Fix: show a count on Overview and a titled section near the top of Settings.

19. **native.rs:1182, 1192-1195. Game rows show raw model state and are disabled too often.** CONFIRMED
    - `text(game.state.to_string())` prints "idle" under every normal game, and "busy (…)" or "broken (…)" otherwise.
    - Rows are disabled while `state.scanning`, so on macOS the whole list flips to disabled every 30 s during the timer rescan. On Windows the same happens on each worker-driven rescan.
    - Non-idle games cannot be selected, so the "Needs attention" filter lists only dead rows. The same path can still be typed into the folder field and compressed.
    - Fix: show the launcher label or a worded state, do not gate selection on `scanning`, and allow selecting non-idle games with Compress disabled and a reason.

20. **native.rs:1618-1622, 1955-1958, 372-386. On macOS `working` covers both planning and running.** CONFIRMED
    - Overview and Jobs say "A storage operation is running" while only the plan is being computed.
    - The Games page shows nothing during planning except disabled buttons. `native_plan` walks the whole folder.
    - Fix: a separate `planning` flag and a "Checking free space…" line.

21. **native.rs:1352. "View jobs and recovery" restores the last Settings scroll offset.** CONFIRMED
    - It sends `GoTo(Page::Settings)`. If the user last left Settings at About, that is where they land.
    - Fix: `Message::Jump("settings-jobs")`.

22. **native.rs:833-846, 1147, 1225, 1653. Keyboard gaps compared with Linux.** CONFIRMED
    - No `on_submit` on any text input. Linux's location field submits on Enter (view.rs:1651).
    - No Ctrl/Cmd+F or Ctrl/Cmd+R (app.rs:2036-2046), and the search field has no id to focus.

23. **native.rs:549-560. A failed preference save is not reconciled.** CONFIRMED
    - On `Err` the in-memory preferences keep the unsaved change (a removed location stays removed on screen). A queued `preferences_dirty` save is dropped, and `refresh_after_save` stays set.
    - Fix: reload from disk on failure, or keep a last-saved copy to restore.

24. **native.rs:1250-1262, 452-455. The Exclude toggle can get stuck.** PLAUSIBLE
    - `excluded` is computed over `game.ids()`, but the toggle removes and adds only `game.id`.
    - If the list holds an alias id, "Include in background work" does nothing.

25. **native.rs:673. `Jump` drops the task returned by `GoTo`.** PLAUSIBLE, latent
    - Linux fixed this pattern at app.rs:1435-1440 because dropped artwork decodes stay pending and can stall the two-slot cache.
    - On native the queue is normally empty at that point, so this is drift from a fix, not an observed stall. Chain the task as Linux does.

## Duplicated code that should be shared

- **`background`**: native.rs:358-368 and app.rs:1021-1032. Same body, different signature (native flattens a `Result`). Use one in `gui/mod.rs`.
- **`artwork_tasks`**: native.rs:1537-1547 and app.rs:2055-2066. Identical apart from the `Ok(...)` wrap.
- **Artwork tile** (sensor, letter fallback, 128x192 cover): native.rs:1551-1588 and view.rs:780-803, 947-967.
- **Page transition and scroll restore** (`GoTo`), plus `Jump`/`JumpOffset`: native.rs:670-712 and app.rs:1435-1492. `JumpOffset` is line-for-line identical.
- **Tab and Shift+Tab handling**: native.rs:833-840 and app.rs:2019-2030.
- **Storage plan panel**: native.rs:1283-1310 and view.rs:223-256. The copies have already diverged (finding 6).
- **Job grouping** (Running, Waiting, Needs attention, History, latest 20) and Pause/Cancel/Retry rows: native.rs:1827-1912 and view.rs:1500-1633.
- **Settings jump-link row**: native.rs:1593-1612 and view.rs:1754-1776. **Appearance** and **About** sections: native.rs:1721-1769 and view.rs:1823-1887.
- **Page shell** (brand, nav column, `tracked_surface(scrollable(container(page)))`, `app_background`): native.rs:1380-1416 and view.rs:91-187, 283-296, 319-323. `theme::sidebar`, `nav_button`, `nav_icon` and `scrollable` are gated to Linux in theme.rs and could be ungated.
- **Button and card builders**: `action`, `secondary`, `panel`, `hero` in view.rs:27-66 belong in theme.rs. Native re-inlines padding and style at each call site.
- **`Status`**: native.rs:47-50 and app.rs:183-200. Toast (Linux-only `theme::toast`) versus banner (native-only `theme::banner`).
- **Theme resolution**: three copies, native.rs:1419-1425, mod.rs `theme_of`, and unsupported.rs:51-53.
- **Animate wrapper and frame subscription condition**: native.rs:1100-1109, 1472-1481 and view.rs:73-82, mod.rs `animation_frames`.
- **`polls` stream shape**: native.rs:1814-1823 and app.rs:2070-2080.
- **Parallel enums**: `GameFilter`/`GameSort` versus `Filter`/`Sort`; `ThemeChoice`/`MotionChoice`/`LocationKind` (desktop.rs) versus `ThemePreference`/`MotionPreference`/`FolderKind` (jobs/mod.rs), with differing labels.
- **Motion distance**: 12/6 hard-coded in view.rs:277-281, `MotionChoice::distance` on native.

## Parity table

| Area | Linux | Native | Verdict |
|---|---|---|---|
| Top-level pages | Overview, Games, Jobs, Settings | Overview, Games, Settings | Drift. Doc says "Jobs have their own page" |
| Jobs location | Own page, plus bottom bar "See jobs" on other pages | Section inside Settings, no bar | Drift |
| Sidebar width | 208 / 72 | 190 / 130 | Drift |
| Sidebar compact form | Icons with tooltips | Text labels | Drift |
| Sidebar style | `theme::sidebar` background, `nav_button`, animated highlight, icons | No background, `action_button`/`secondary_button`, no icons | Drift |
| Settings nav position | Pinned to the bottom | Third in the list | Drift |
| Nav spacing | 6 | 10 | Drift |
| Compact threshold | 880 px | 760 px | Drift |
| Page padding | 24, 16 compact | 24 always | Drift |
| Window size | 1100 x 720 | 900 x 620 | Drift |
| Scrollbar style | `theme::scrollable` | iced default (style gated to Linux) | Drift |
| Status | Floating toast, dismiss button, 4 s info, sticky errors, 8 s refusals | Inline banner on Games only (also Jobs on macOS), never clears | Drift |
| Button styles | `action`/`secondary`/`button::text` everywhere | Mostly iced default primary; Compress, Decompress, Stop, nav, jump links and What changed styled | Drift |
| Button padding | [10,14] action, [9,12] secondary | [11,18] controls, [6,10] Refresh, 5 default elsewhere | Drift |
| Game model | One card per game, Compress beside estimate or result, expandable details | List on the left, free-text "Selected folder" column on the right | Drift against the doc; partly from native having no estimates |
| Verbs | Compress, Decompress, Analyze | Compress, Decompress, Stop; no Analyze; macOS status says "Restoring ordinary storage…" | Analyze is probably a backend gap; the wording is drift |
| Decompress and Qualify placement | Details / Advanced | Top level beside Compress | Drift against the doc |
| Mode choice | Standard or Maximum per game | None (LZX or APFS only) | Justified by platform |
| Start confirmation | Only when the plan fails | Always | Drift |
| Plan panel text | "needed including headroom · available", reasons, retained-original note | "needed including headroom; available", neither extra | Drift |
| Plan panel buttons | Start job is action, Cancel is secondary | Both iced default | Drift |
| Filters | All games, Ready, Compressed, Needs attention, Updated games, plus drive and launcher filters | All games, Updated, Needs attention | Ready and Compressed need data native lacks; label "Updated" vs "Updated games" is drift; launcher filter is missing though native sorts by launcher |
| "Needs attention" meaning | Unsupported, or latest job failed, partial or interrupted | Install state not idle | Drift. Windows has Failed and Interrupted job phases and ignores them |
| Sort | Most space to save, Name, Size | Title, Launcher | "Name" vs "Title" is drift; the rest is a data gap |
| Search and filter layout | One row when wide, stacked when compact | Always stacked | Drift |
| Bulk selection | Checkboxes and "Compress selected" | None | Drift. The Windows queue could support it |
| Games empty state | Panel: "Finding your games…" or "No games match", "Add a folder" button | Muted line, wrong during scan, no button | Drift |
| Game row text | Title 16, muted status (launcher, phase, Compressed), size and saving | Title 14, path 11, raw state 11 ("idle") | Drift |
| Artwork tile | 52 / 44, `Cover` fit, click to choose artwork | 44, `Contain` fit, separate "Choose local artwork…" button | Drift |
| Cover image | Hidden when compact | Always shown | Drift |
| Refresh button | Secondary, on Overview and Games title rows | Default style, inside the library header only | Drift |
| Overview | Saving hero with primary action, stats panel (Installed, Games, Compressed), attention card, Drives, Recent work | "Save space with {MODE}" hero, stats (Games, Mode), one-line "Running now", two default buttons | Mostly drift; the saving figures are a data gap |
| Hero text size and padding | 27 or 38, padding 20 | 28, padding 22 | Drift |
| Job rows | Title 16, progress bar, elapsed, errors, drive-change note | Default-size title, counts only, `job.message` | The bar needs totals native lacks (the doc allows text); the rest is drift |
| Job empty states | Specific line per group | "No jobs in this section" for all | Drift |
| History rows | Compact row with ✓ or ○ | Same card as active jobs | Drift |
| Worker pause notice | Panel "Paused while you play {game}" | Muted "Paused: {busy}" | Drift |
| Connection error | Panel with three lines | One muted line | Drift |
| macOS jobs | n/a | One in-process job, "Cancel job" here and "Stop" on Games for the same message | Justified (no coordinator); the two labels are drift |
| Settings jump links | Locations, Recovery, Maintenance, Appearance, Reports, About | Jobs first, then the same | Follows from Jobs living in Settings |
| Settings sections | Each in a `panel` card, spacing 16 | Bare columns, spacing 28 | Drift |
| Locations title and help | "Drives & libraries", "Add a location" | "Locations", different help text | Drift |
| Location kinds | Collection first and default; "Single game" / "Games library" | Game first and default; "One game" / "Games library" | Drift |
| Add control | "Add folder" (action), Enter submits, inline `danger_text` error | "Add location" (default), no submit, error to off-page banner | Drift |
| Path hint | "~/My Games or /mnt/games" | `FOLDER_HINT`, a single-game example | Platform path is justified; the single-game example is drift |
| Detected libraries | Listed, with a maintenance toggle | Only user-added locations listed | PLAUSIBLE backend limit (`automatic` lives on `Location`) |
| Maintenance toggle | Inside the card, "Maintain new installs and updates" | Outside the card, label repeats the path | Drift |
| Excluded games | Listed in Settings with "Restore" | Toggle only on the selected game; "Exclude from background work" vs "Exclude this game" | Drift |
| Recovery | Title, help line, "Export local diagnostics", job cards, empty panel "No interrupted jobs or retained storage need review." | Path plus one default button, "No interrupted jobs need recovery" | Mostly justified (no packs); diagnostics export and wording are drift |
| Maintenance section | Summary card, "Libraries" link, "Restart worker" | Windows: pause, stop/start, start at login. macOS: one line | Justified by platform |
| Appearance | Label plus muted description per row, spacing 18, centred | Label only, spacing 12 | Drift |
| Motion labels | Expressive, Subtle, Reduced | Smooth, Subtle, Reduced | Drift |
| Reports section | "Maximum Space compatibility", report count, "Import report…" | "Compatibility reports", "Choose a game", no import or count | Drift |
| About | "About Flummox", "Version X" | "About", "{PLATFORM} · X" | Drift (the platform string is a reasonable addition) |
| Qualification wizard | Panel above the page, fixed 420 px scroll | Inline in the controls column, grows | Native matches the doc's no-fixed-height rule |
| Discovery warnings | Counted on Overview | Muted lines at the bottom of Settings | Drift |
| Error colouring | Red toast edge and mark, `danger_text` inline | Red banner edge only; worker errors and failed-job messages muted | Drift |
| Keyboard | Tab, Shift+Tab, Esc (closes details, clears selection), Ctrl+F, Ctrl+R | Tab, Shift+Tab, Esc (closes plan and wizard) | Drift |
| Unsupported targets (unsupported.rs) | n/a | Bare text, no sidebar | Contradicts the doc's "show the shared shell with compression disabled" |

