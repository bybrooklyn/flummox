# Flummox visual audit (read-only, no files edited)

I opened all 30 previews and 9 of the 14 live screenshots, and read `view.rs`, `theme.rs`, `preview.rs`, `preview_renderer.rs`, `qualification.rs` and both design docs.

## Read this first: the previews draw every control as disabled

`src/gui/preview_renderer.rs:23-35` calls `layout` then `draw` and never `update`. In iced 0.14.2, `button.rs:376`, `checkbox.rs:395` and `text_input.rs:471` fall back to `Status::Disabled` until a `RedrawRequested` event has been processed.

- Every button, checkbox and text input in the 30 PNGs is in its disabled style. That is why "Compress", "Add folder" and "Compress to save about 900 MB" appear as bare grey text, and game titles are grey.
- The live screenshots show the real enabled styles (filled green "Compress", bordered search box).
- The previews therefore cannot show an enabled/disabled regression.
- Fix: send one `Event::Window(window::Event::RedrawRequested(Instant::now()))` through `widget.update(...)` before `draw`. Add one fixture that leaves a button disabled, so both states appear.
- `drives.png` and `recovery.png` show a state the app cannot reach. `Message::GoTo` stores `destination.main()` (`app.rs:1454-1464`), so `state.page` is never `Drives` or `Recovery`. The missing sidebar highlight in those two images is a fixture artefact (`preview.rs:193,249`).
- The live screenshots predate the current build (Jobs still inside Settings, "View queue", "Optimized").

## Findings, most severe first

**1. Disabled primary button looks like plain text. CONFIRMED** (every preview: overview "Compress to save about 900 MB", games "Compress", drives "Add folder", space-plan "Start job")
- `theme.rs:291,298,301-305`: disabled fill is `colors.panel`, text is `muted`, border width 0. On a panel it is identical to a `theme::muted` label at 16 px.
- This is the real appearance of "Start job" on a failed plan, "Analyzing…" in the hero, and "Compress" while a job is pending.
- Fix: give disabled a 1 px `colors.border` outline and a fill of `mix(panel, accent_dim, 0.25)`.

**2. Light theme primary button is nearly invisible; dark is a strong fill. CONFIRMED** (overview-light-live "Scan again", locations-live "Add folder")
- `theme.rs:62,290`: light `accent_dim` 0xCDE8D8 on a white panel is 1.30:1. Dark is 0x2D6E49 on 0x1E2225, 2.62:1 with a clear hue.
- The selected sidebar item in light is 1.14:1 against the sidebar (`theme.rs:251-255`).
- Fix: in light, use `accent` 0x247A4B with white text for Active (5.3:1) and a darker mix for hover. Keep `accent_dim` for the nav highlight but add a 1 px `accent` border or a 3 px left bar.

**3. Failure states are not visibly failures. CONFIRMED and PLAUSIBLE**
- jobs-phases.png, "Strategy · Needs attention": same card, same green bar, muted label as a running job (`view.rs:1518-1524`).
- recovery.png: an interrupted job shows a full green bar.
- jobs-disconnected.png: "Worker connection interrupted" is an ordinary panel (`view.rs:1569-1576`).
- overview-live: "250 items need attention" is an ordinary panel (`view.rs:485-505`).
- PLAUSIBLE, no fixture has errors: job errors use `theme::muted` (`view.rs:1307`, `view.rs:1555`); the plan-check failure is plain 13 px text (`view.rs:243`); qualification errors are plain 12 px text (`qualification.rs:331-332`).
- `Colors::warning` (`theme.rs:34`) only reaches the iced palette. No widget uses it.
- Fix: add `theme::warning_text` and an `attention_panel` style (border `warning` or `danger`, fill `mix(panel, edge, 0.10)`, like `toast` at `theme.rs:200`). Use `danger_text` at 243, 1307, 1555. Colour the bar or phase label by phase.

**4. Qualification form and storage plan sit outside the page system. CONFIRMED** (qualification.png, space-plan.png)
- `view.rs:207-256` pushes both panels straight into `body` with no gutter. They touch the sidebar, the window top and the right edge, while the page below has a 24/16 gutter.
- The form is a fixed 420 px nested scrollable (`view.rs:218`). That breaks the rule in `docs/gui-architecture.md` that outer pages own scrolling. Its scrollbar overlaps the right ends of the text inputs.
- Form controls are unthemed: plain `button("…")` at `qualification.rs:326,336,337` (PLAUSIBLE, below the fold), default pick list and inputs with padding 8 against 10 elsewhere.
- Labels are placeholders only, so the filled "Game build" field shows a bare "1" (`qualification.rs:256-284`).
- "4000000000 logical bytes" is unformatted (`qualification.rs:254`).
- Title is size 20, a size used nowhere else.
- Fix: render both inside the page column, drop the fixed height, put a `theme::muted` label above each input, use `action`/`secondary`, `section_title`, and `size()` for bytes.

**5. Default iced widgets beside themed ones. CONFIRMED** (games.png, games-light.png, drives*.png, settings*.png, live light shots)
- `pick_list` at `view.rs:572,583,600,611,655,658,1041,1643,1830,1857` and `text_input` at 566, 595, 1183, 1649 have no `.style`.
- They have 2 px corners against 8 on buttons and 12 on cards. Pick lists are 31 px tall beside a 41 px search box and 38 px buttons.
- In light they sample as blue-grey 0xCCD7E1 and the scroller as 0xA4B8CB. Neither is in the green-neutral palette.
- `theme::scrollable` (`theme.rs:345-353`) restyles rails only, not the scroller.
- The filter row (`view.rs:594-618`) has no `align_y`, so pick lists hang from the top of the taller input.
- Fix: add `theme::pick_list` and `theme::text_input` (panel or background fill, 1 px `border`, radius 8, padding `[9,12]`). Set the scroller to `mix(border, muted, 0.5)`. Add `.align_y(Center)` to the row.

**6. Settings stacks three page titles; heading sizes are ad hoc. CONFIRMED** (settings.png, settings-light.png, settings-narrow.png)
- "Settings", "Drives & libraries" and "Recovery" are all `page_title` 26 on one page (`view.rs:1778,1638,1337`).
- PLAUSIBLE from code, below the fold: the remaining sections have only 17 px card headings (`view.rs:1798,1805,1825,1874,1881`), so half of Settings has section headers and half does not.
- Jump labels do not match headings: "Locations" leads to "Drives & libraries", "Reports" to "Maximum Space compatibility". "Background worker" has no link (`view.rs:1755-1762`).
- Same-level headings use 15 (`view.rs:740,749` game groups), 16 (`section_title`, 497, 1074, 1180), 17 (1607 job groups, 1642 and the preferences cards) and 18 (690 empty state).
- Fix: give `drives()` and `recovery()` a flag that renders `section_title` when embedded. Name the sections after their jump links. Use `section_title` (16) for group and card headings; add one `card_title` if 17 is wanted.

**7. Details pane is a wall of muted 13 px text with three filled buttons. CONFIRMED** (games.png, games-light.png, games-narrow.png, games-details.png)
- With Advanced open there are about 10 muted lines. The label "How to compress" (`view.rs:1003`) has the same style as the explanations.
- "Strong estimate." (`view.rs:1214`) floats between two button rows.
- The three steps are one run-on line (`view.rs:1188`).
- The analysis facts are one muted block joined with newlines (`view.rs:1301`).
- The selected mode uses the primary style (`view.rs:996-1000`). One card can then hold three primary buttons: Compress, the chosen mode, and "Create & activate" (`view.rs:1192`). That contradicts the "one primary action" rule in `docs/gui-architecture.md`.
- Fix: add a `selected_button` style (1 px `accent` border, `mix(panel, accent_dim, 0.3)` fill) for the mode toggle. Make "Create & activate" secondary. Put the confidence line beside the mode row. Render the steps as three lines in body text colour. Render the facts as label/value rows.

**8. "Advanced" is two different controls. CONFIRMED** (games-details.png against maximum-original.png and maximum-confirmed.png)
- Ordinary games get a right-aligned `button::text` (`view.rs:1021-1028`). Stored games get a left-aligned outlined `secondary` (`view.rs:1136`).
- The same split applies to "Maximum" as a raw `text(...).size(16)` (1074) against `section_title`.
- Fix: one helper, as a text-style disclosure with a chevron.

**9. Right-hand columns do not line up between rows. CONFIRMED** (games-groups.png, games-groups-all.png, games-live.png)
- The size/saving column ends at x=945 beside "Compress", 973 beside "Details", 922 beside "Analyze again", and 1060 when there is no button (`view.rs:907-931`).
- Fix: put the trailing control in a fixed-width, right-aligned container (about 132 px), present even when empty.
- Related, same cause (missing `align_y`):
  - `view.rs:545` Overview "Recent work": the muted outcome sits 3 px above the title.
  - `view.rs:1519`: job header row.
  - `view.rs:1525-1542`: the stats line is top-aligned beside 38 px buttons.
  - `view.rs:1735`: excluded row.

**10. Buttons of different heights in one row. CONFIRMED** (overview-live: "Scan again" 40 px beside "Games" 38 px; drives.png: input 41, "Browse…" 38, "Add folder" 40)
- `view.rs:33` uses padding `[10,14]`; `view.rs:48` uses `[9,12]` plus a 1 px border.
- Fix: one padding for both, with action carrying a 1 px transparent border so the boxes match.

**11. Sidebar icons. No tofu, but misaligned. CONFIRMED** (all wide and narrow previews, live)
- ⌂ ◈ ☷ ⚙ ✓ → all render in both the software renderer and the live window.
- Glyph widths differ, so labels start at x=50 (Overview), 52 (Games), 54 (Jobs, Settings) (`view.rs:119-126,150-162`).
- In compact mode the icons and the "F" are left-aligned in a 52 px button, about 7 px left of centre (`view.rs:93,114-117,141-148`).
- ▰ and ⟲ (`view.rs:374-375`) are never drawn. ● × ○ − are in no fixture, so their rendering is unverified.
- The glyphs depend on system font fallback (`theme.rs:81` is `Font::DEFAULT`).
- Fix: wrap each icon in `container(...).width(20).center_x(...)`, centre the compact label, and bundle an icon font or SVGs.

**12. Overview shows an empty "Drives" heading, and narrow drops Recent work. CONFIRMED** (overview.png, overview-light.png, overview-narrow.png, overview-paused-live.png)
- `view.rs:507` pushes the heading unconditionally. With no drives it sits directly above "Recent work".
- `view.rs:540-543` removes Recent work when compact, although overview-narrow.png is more than half empty.
- Fix: guard the heading, or show a muted "No drives yet" with an "Add a folder" button. Keep Recent work at narrow width.

**13. Card and page spacing differs page to page. CONFIRMED**
- Page column spacing: 16 (`view.rs:483`), 12 (636, 761), 14 (1341, 1567, 1671), 28 (1784), 16 (1889).
- Hero padding is 20 and panel padding 16 (`view.rs:55,62`). In overview.png the hero text starts at x=252 and the stat card below at x=248.
- Title baseline is y=43 on Overview and Games (title shares a row with a 38 px button) and y=41 on Jobs and Drives.
- Overview and Games have "Refresh" and no subtitle. Jobs, Drives and Recovery have a subtitle and no action.
- PLAUSIBLE: the active-job bar uses a fixed 24 px gutter (`view.rs:316`) while the compact page uses 16 (`view.rs:286`).
- Fix: one `page_header(title, subtitle, trailing)` helper with a fixed height, one `PAGE_GAP` constant, hero padding 16.

**14. Narrow-width squeeze on game rows. PLAUSIBLE** (no fixture renders the closed stored rows at 720; games-narrow-live.png shows the pattern, with the note wrapping to two lines)
- At 720 px a row has about 400 px for title plus right column. "Saves about 2.10 GB once you delete the original" (`view.rs:1465`) is about 295 px, leaving about 100 px for "Maximum With Original".
- The button rows at `view.rs:1093-1110`, `1160-1172` and `1191-1204` have no `.wrap()`.
- `mod.rs:155` sets no minimum window size.
- Fix: in compact, move the saving line under the title; add `.wrap()` to those rows; set a minimum size of about 640x480.

**15. Smaller items**
- CONFIRMED, jobs-phases.png: a Queued job shows an empty progress track and "0 / 140 items · 0 B processed · 12s". A Paused job is counted under "Running · 2" with a green bar. Hide the bar for `Phase::Queued` and tint paused with `warning`.
- CONFIRMED, recovery.png: the card title is a raw path (`view.rs:1361`) where the job card uses the game title. The two actions stack vertically (`view.rs:1399-1422`) where every other card uses a row. A mounted install reads "Ready" on a recovery page.
- CONFIRMED, jobs.png: the ✓ is plain text colour and "Completed" repeats it (`view.rs:1899-1904,1928`). Colour ✓ with `accent`.
- CONFIRMED: disabled and enabled `secondary` differ only in text colour (`theme.rs:325-338`). Compare "Refresh" in games.png with games-live.png. Dim the disabled border as well.
- Fixture coverage gaps: toast (`view.rs:336-351`, default `button::text` ×), scan banner (`view.rs:190-205`), selection bar (673), empty games state (681-696), stale-order bar (698), job errors, plan failure, lower half of Settings.

## Contrast (computed from `theme.rs` hex values)

| Pair | Dark | Light |
|---|---|---|
| text on panel | 13.5 | 16.6 |
| muted on panel | 6.3 | 5.2 |
| muted on background | 7.1 | 4.8 |
| muted on sidebar | 6.7 | 4.5 |
| accent stat numbers on panel | 7.6 | 5.3 |
| danger on panel | 4.9 | 5.6 |
| text on primary fill | 5.1 | 12.8 |
| border on panel | 1.31 | 1.36 |
| panel on background | 1.13 | 1.08 |

- All text passes AA 4.5:1. Light muted on the sidebar passes by 0.03.
- The failures are non-text. The 1 px border is the only boundary of `secondary` buttons and cards, and it is far under 3:1 in both themes. See finding 2 for the light primary fill.
- Suggested borders: about 0x444C53 in dark and 0xB9C4BE in light.

## Keep these

- Sidebar structure: 208 px wide, 72 px compact with tooltips, Settings pinned at the bottom, dark selected fill.
- Card system: radius 12, 1 px border, panel one step above the background; the hero's `accent_dim` border.
- The stat row (30 px accent numbers over muted labels) and the hero hierarchy (caps eyebrow, 38 px figure, muted line, actions).
- Closed game rows: checkbox, letter tile, title and status, size and saving, one action. In the enabled state (selection-return-btrfs-live.png) they read clearly.
- Group headings with count and total ("Compressed · 3 games · about 3.60 GB saved") and the collapsed "Little to gain" group.
- Jobs grouping with counts and per-section empty lines; the thin 5 px progress bar; history rows as single-line cards.
- Text passes AA everywhere; `danger_text` beside the folder input; "(estimate)" and "About" labels on unmeasured figures.
- Wrapping jump links on Settings fit at 720 px. No clipped or overlapping text in any of the 30 images apart from the qualification scrollbar.

Files: `src/gui/view.rs`, `src/gui/theme.rs`, `src/gui/preview_renderer.rs`, `src/gui/preview.rs`, `src/qualification.rs`, `src/gui/app.rs`, `src/gui/mod.rs`.
