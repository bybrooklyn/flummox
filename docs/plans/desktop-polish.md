# Flummox desktop polish and completion

## Delivery order

1. Navigation, layouts, and dark/light previews.
2. Motion, scrolling, stable jobs, asynchronous scanning, and local artwork.
3. Graphical acceptance and repairs.
4. Windows discovery and background-maintenance parity.
5. Real-game acceptance, native CI, and release publication.

Preserve the local release-check fixes. Never test on ordinary game libraries.

## Navigation and presentation

Use exactly Overview, Games, Settings as main navigation. Settings is one
continuous scroll with jump links to Jobs, Locations, Recovery, Maintenance,
Appearance, Compatibility reports, About. Preserve scroll positions per page;
explicit shortcuts override the restored position.

Overview shows storage (estimates distinguished from measurements), current
work, and attention items, with direct links. Games preserves search, filters,
sort, selection, expanded details and scroll on refresh. Put updated games in a
filter. Settings Jobs combines Queue and Activity, grouped as Running, Waiting,
Needs attention, History, with counts and section-specific empty states. Pause,
Resume, Cancel, Retry and recovery links follow actual job state. Preserve all
retained-copy, compatibility, and recovery verification controls.

Refine charcoal surfaces and the green accent, typography, borders and spacing;
remove unnecessary nested panels. Use green for primary actions and success.
Review Overview and Games in dark and light themes before extending styling.
Keep branding and avoid unrelated content rewrites.

## Motion and content stability

Wheel steps ease toward a clamped target over 100 ms without momentum, velocity,
bounce or overscroll. Retarget from the current position and reverse immediately.
Trackpad pixel deltas, scrollbar dragging, keyboard scrolling remain direct.
Reduced motion disables interpolation; animate only while movement is pending.

Navigation uses clipped visual translation, never a layout-changing spacer.
Moving to an earlier sidebar item brings content down from above; moving later
brings it up from below. Normal: 12 px/180 ms ease-out cubic. Subtle: 6 px/120 ms.
Reduced: immediate. Repeated current-page clicks do not restart the transition.
Rapid switching cannot leave stale content or accumulated displacement.

Reproduce queue disappearance and inspect subscriptions, widget identity,
snapshots and navigation. Preserve the last valid snapshot; loading only before
the first snapshot; retain rows on polling failure and show connection status.
Use persistent job IDs and update progress in place. No artificial delays.
Keep failures beside items and routine confirmations in brief notifications.

## Discovery and artwork

Move Linux coordinator scans off its request loop. Keep native scanning in
background tasks. Add generations, progress, provider batches, completion,
warning and cancellation events. One scan at a time; coalesce extra refreshes.
Discard stale results without clearing the existing library. Separate discovery,
analysis and images. Coordinator owns Linux discovery-cache writes. Failed or
cancelled providers cannot remove remembered games. Report provider and count,
without invented percentages. Cancellation suppresses late results; an existing
OS read can finish before the worker exits.

Derive metadata only locally, without guesses, IGDB or online requests. Index
launcher art once per scan, deterministically choose images by role, use icons
for rows and covers for details. Decode/resize in two bounded background workers.
Cache by source, modification stamp, dimensions and role, capped at 256 decoded
thumbnails. Prioritize visible rows, fix image slot dimensions, retain images on
refresh, isolate corrupt sources, and allow local-image overrides. Reuse shell,
motion, scrolling and art components across front ends, exposing real backend
capabilities only.

## Windows phase

Discover Steam (registry/library manifests), Epic (local item manifests), GOG
(installed-game registry), Heroic (local installed metadata). Bound reads, isolate
provider failures, deduplicate resolved directories retaining launcher aliases.
Preserve old serialized identities; migrate remembered folders as individual
games and add collections.

Single per-user coordinator, current-user restricted versioned named pipe,
durable queue/settings, one storage job, existing WOF locks/journals and volume
space rechecks. GUI jobs run through coordinator. Maintenance is opt-in by
location, with exclusions, pause/resume and deferral for games/updaters, unknown
activity, unavailable drives or recovery. Establish baseline without compressing
existing games. Detect new/updated installations, query WOF state, skip unchanged
compressed files. Tray: Open, Pause/Resume, Exit. Closing window keeps maintenance;
Exit cancels safely preserving recovery. Login startup is separately opt-in.
Installer upgrade/uninstall shuts down owned worker safely and removes only
owned startup entries.

## Acceptance and release

Test direction, interruptions, reduced motion, scroll reversals/clamping and
restoration, stable job identities/snapshots in every phase, slow/corrupt/cancelled
providers, stale results, offline locations, art cache and malformed images.
Native Windows fixtures cover discovery, IPC security, restart, persistence,
maintenance, recovery and installer/tray/startup lifecycle.

Run just lint, just test, dependency policy, packaging and workflow validation.
Visually inspect wide/narrow, light/dark, large libraries, missing art, offline
folders, native dialogs, keyboard navigation and real wheel/trackpad behavior.
Confirm no disappearing jobs or layout jumps. Full integration requires IPC and
a graphical environment.

Complete disposable Linux native, Proton, Windows WOF and Mac APFS gameplay,
saves, normal exit, loading/allocation measurements, launcher update/verification,
restore/interruption and reboot/login checks. Commit/push, verify remote and
native CI, build final candidate. Publish v0.0.2 only after required acceptance
records pass, then verify release assets and Homebrew/AUR updates.

## Deferred adoption backlog

Reference https://store.steampowered.com/app/4339880/Game_Compressor/ . Compare
first-use discovery, savings, batch compression, updates and restoration. Equal
priority for Windows/Linux/Mac: install, compress a supported game and find
Restore entirely through GUI without documentation. Provide supported defaults,
clear prerequisites, and inspect existing Windows WOF/LZX compression. Test
migration on disposable copies before claiming no decompression is necessary.
Keep this outside the UI phase. Scheduling, Bottles, Mac FUSE, community sharing,
online metadata and publisher signing remain deferred.

## Implementation status

- Implemented Linux navigation, continuous Settings, grouped stable jobs,
  directional motion, wheel easing, ordered snapshots, background discovery and
  local artwork. The baseline UI change is committed as `596b21e`.
- Implemented the native Settings sections, persisted theme/motion, collections,
  local artwork, search, sorting and Updated/Needs attention filters. Native
  snapshot/discovery ordering retains the library through refresh failures.
- Implemented Windows Steam registry, Epic, GOG and Heroic discovery; a per-user
  restricted versioned pipe coordinator; durable jobs and drive identities;
  opt-in maintenance baselines/exclusions; tray controls; separate login startup;
  and installer shutdown handling. Jobs show current deferral reasons. Recovery
  blocks automatic maintenance, and restoration remains available for exclusions.
- Verified Linux lint, 144 library tests, remaining portable integration targets,
  packaging, workflow syntax, cached dependency policy and Windows x64
  cross-target lint. Windows runtime fixtures and native UI previews compile;
  CI runs them and exports preview images. See the
  [delivery and validation record](../validation/2026-10-06-desktop-polish.md).
- Pending graphical acceptance: actual queue disappearance, wheel/trackpad and
  anchor scrolling, keyboard navigation, native dialogs and image layouts on
  each desktop. The complete Linux `just ci` gate now passes, including all eight
  coordinator lifecycle tests and a real btrfs worker round trip. Native
  Windows/Mac runtime CI has not run for this change.
- Pending Windows manual lifecycle acceptance: tray/Explorer restart, login,
  busy/updating games, unplugged/replaced drives, recovery and upgrades while a
  worker is finishing a file. Installer smoke checks and isolated coordinator
  tests are wired into CI, not claimed as locally executed.
- Pending release acceptance, Mac compilation/runtime, real-game qualification,
  remote CI and publication. Git metadata is now writable in this session.
  No new release was published. Game Compressor adoption work remains deferred.
