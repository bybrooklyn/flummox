# GUI wording audit: Flummox (read-only, no files changed)

All paths are under `/home/brook/data/gamecompressor/`. Every string below was located and is quoted exactly. `docs/usage.md` line numbers are the file's own.

Scope notes:
- I extracted strings by grepping quoted lines and reading the surrounding code; I did not render the window.
- Worker-produced text that the window prints verbatim (`job.message`, `install.message`, refusals, storage-plan reasons) is included, because it reaches the user through `view.rs:1079, 1363, 1524, 1630` and `native.rs:1895, 1950`.
- `PackTask::label` (`src/jobs/mod.rs:225`) has no call site in `src/gui/view.rs`, `src/jobs`, `src/cli` or `src/pack` that I could find. The window uses its own table at `view.rs:1443-1450`. The label strings still disagree with that table, so they are listed.

---

## 1. Terminology drift

### 1.1 "Maximum" / "Maximum Space" / "store" / "pack": CONFIRMED, plus a collision

| Term | Occurrences |
|---|---|
| "Maximum" (mode) | `view.rs:983, 1010, 1074, 1451`; `recommendation.rs:25`; `app.rs:726, 743, 1111`; `view.rs:1258` ("Maximum sample about {}") |
| "Maximum Space" | `view.rs:1180` ("Maximum Space storage"), `1799`, `1874` ("Maximum Space compatibility"); `app.rs:1314, 1901`; `dialog.rs:57`; `compatibility.rs:60`; `jobs/mod.rs:228-229`; `jobs/service.rs:606, 705, 1689` |
| "Maximum store" | `view.rs:1443` ("Build Maximum store"), `1445` ("Switch to Maximum store") |
| "store" / "compressed store" | `view.rs:1010, 1113, 1121, 1146, 1183` ("Store path"), `1188, 1189, 1197` ("Create store only"), `1365, 1379`; `jobs/mod.rs:227` ("Create verified store") |
| "storage" for the same thing | `view.rs:1187` ("Choose storage folder…"), `1338` ("retained storage"), `1420` ("Review game and storage"), `1428`; `jobs/service.rs:1226` ("before activating storage"), `1734` |
| "pack" | `jobs/service.rs:394` ("Pack jobs require a storage task"), `606` ("requires a build with pack mounting"), `1320` ("Build Flummox with pack mounting to run storage jobs"); `jobs/packs.rs:505` ("This build cannot mount pack stores") |
| "Writable compressed install" | `pack/install.rs:291, 397` |

Collision: `Preset::Max` displays as "Maximum" (`backend/mod.rs:671`), and the "Standard strength" pick list at `view.rs:1042` uses that Display. A user in Standard mode can pick strength "Maximum". The helper line directly under it says "Max" (`view.rs:1051`), and `docs/usage.md:33` says "Fast, Balanced, Max".

A second mode name exists: `compatibility.rs:59` prints "Native compression" for what `recommendation.rs:24` calls "Standard". The qualification form's pick list (`qualification.rs:254`) therefore offers "Native compression" / "Maximum Space" while the game card offers "Standard" / "Maximum". `app.rs:726` also says "has no native compression". On Windows and Mac, "Mode" means the algorithm: `native.rs:1120` shows stat "Mode" = "LZX" or "APFS".

Recommendation: mode names "Standard" and "Maximum" everywhere (matches `docs/gui-architecture.md:20`). Use "store" for the on-disk object and never "pack", "storage" or "Maximum store". Change the preset Display to "Max" so "Maximum" has one meaning.

### 1.2 "Decompress" / "Restore": CONFIRMED

| Term | Occurrences |
|---|---|
| "Decompress" | `view.rs:1017`; `native.rs:1213` |
| "Decompress to ordinary files" | `view.rs:1106, 1129` |
| "Decompression" | `view.rs:1441`; `native.rs:1888` |
| "Decompression to ordinary files" | `view.rs:1447` |
| "Check decompressed files" | `view.rs:1448` |
| "Decompressing needs free space for the whole game." | `view.rs:1133` |
| "Restore ordinary files" | `view.rs:1400`; `jobs/mod.rs:231` |
| "Verify restored files" | `view.rs:1412`; `jobs/mod.rs:232` |
| "Restore ordinary storage" | `native.rs:42` (Windows) |
| "Restore retained original" | `native.rs:44` (Mac) |
| "Restoring ordinary storage…" | `native.rs:418` |
| "Restoring files" | `pack/install.rs:68` |
| "Restoring ordinary files at the launcher path" | `jobs/packs.rs:163` |
| "Restoration was interrupted. …" | `jobs/packs.rs:471` |
| "…have been restored." / "Restore Maximum Space games…" | `view.rs:1799`; `jobs/service.rs:705, 1689` |
| "before retrying or restoring." | `view.rs:1338` |
| "Restoration and restart succeed" | `qualification.rs:303`; also `qualification.rs:254`, `native.rs:1747` |

The same job has two names inside one window: the Recovery button reads "Verify restored files" (`view.rs:1412`) and the Jobs page then lists it as "Check decompressed files" (`view.rs:1448`). Likewise "Restore ordinary files" (`view.rs:1400`) becomes "Decompression to ordinary files" (`view.rs:1447`).

Recommendation: "Decompress" for the user action on every platform (it is one of the three verbs in `gui-architecture.md:19`). Keep "Recovery" only as the name of the section for interrupted work.

### 1.3 "Restore" used for un-excluding: CONFIRMED

- `view.rs:1738` button "Restore" next to "Excluded: {id}" (`view.rs:1736`).
- `jobs/service.rs:426` "This game is excluded. Restore it in Drives first."
- Opposite action: `view.rs:1240` "Exclude this game".
- Windows uses a different pair: `native.rs:1255` "Include in background work" / `native.rs:1257` "Exclude from background work". That is also a narrower meaning (background work only) than Linux.
- Worker messages: `jobs/service.rs:741` "Excluded from future work"; `windows/coordinator.rs:614` "Excluded from background work".
- `docs/usage.md:123` calls it "A hidden entry".

Recommendation: "Exclude" / "Include". "Include" is the plain opposite and frees "Restore" from a second meaning.

### 1.4 Delete vs reclaim, fold in vs compact, create vs build (not on your list)

| Concept | Window words | Other words |
|---|---|---|
| Delete the kept original | "Yes, delete the original" `view.rs:1096`; "It works, delete the original" `view.rs:1101`; "Delete the original" `view.rs:1449` | "until you explicitly reclaim it." `view.rs:239`; "Reclaim original" `jobs/mod.rs:233`; "Reclaiming space" `pack/install.rs:65`; "before reclaiming space" `jobs/packs.rs:211`; `README.md:58` "Reclaim original" |
| Delete the previous version | "Delete the previous version now" `view.rs:1167`; `view.rs:1450` | "Reclaim previous version" `jobs/mod.rs:234`; "Reclaiming previous version" `pack/install.rs:67` |
| Merge updates into the store | "Fold in updates now" `view.rs:1162`; "Fold in updates" `view.rs:1446`; `view.rs:1124, 1126` | "Compact updates" `jobs/mod.rs:230`; "Compacting updates" `pack/install.rs:66`; "Switching to the compacted store" `jobs/packs.rs:378`; "Updates compacted; previous version retained for recovery" `jobs/packs.rs:393`; "Compaction was interrupted; using the previous store" `pack/install.rs:311` |
| Make the store | "Create & activate" `view.rs:1193`; "Create store only" `view.rs:1197`; "Activate existing" `view.rs:1199` | "Build Maximum store" `view.rs:1443`; "Maximum compression" `view.rs:1444`; "Switch to Maximum store" `view.rs:1445`; "Activating" `pack/install.rs:63` |
| The copy kept for rollback | "the original is kept" `view.rs:1010` | "The original is retained" `view.rs:239`; "Retained original: {}" `view.rs:1373`; "Previous store retained: {}" `view.rs:1379`; "rollback copy retained" `pack/install.rs:291` |

The game card says "Fold in" and "Delete". The status text printed on the same card from `install.phase.label()` and `install.message` (`view.rs:1078-1079`) says "Compacting" and "Reclaiming".

Recommendation: "delete the original", "fold in updates", "kept". These are the plain words the card already uses.

### 1.5 "Analyze" / "Scan again" / "Refresh" / "Recheck": CONFIRMED, "Recheck" REFUTED as a UI string

| Term | Occurrences |
|---|---|
| "Analyze" | `view.rs:1015` |
| "Analyze again" | `view.rs:922` |
| "Analyze your games" / "Analyzing your games…" / "Analyzing…" | `view.rs:438, 436, 455` |
| "Analysis" | `view.rs:1439`; `app.rs:1776, 1901`; `view.rs:1875` |
| "Not analyzed yet" | `view.rs:858`; `app.rs:256` |
| "Scan again" | `view.rs:457` |
| "Scanning…" / "Scanning {source} · {} games found" / "Cancel scan" | `view.rs:442, 195, 199` |
| "Refresh" / "Refreshing…" | `view.rs:418, 416, 628, 626`; `native.rs:1137, 1135` |
| "Finding your games…" | `view.rs:434, 686` |
| "Starting discovery" | `jobs/service.rs:1509` (printed inside "Scanning {source}") |

- "Recheck" appears only in a stale code comment, `view.rs:862` ("The button reads "Recheck""). The button reads "Analyze again" (`view.rs:922`).
- "Scan again" (`view.rs:457`) and "Refresh" (`view.rs:418`) both send `Message::Rescan`, so one action has two labels on one page.
- While scanning, the Overview hero shows three words for one state: title "Finding your games…" (`434`), helper "Scanning…" (`442`), button "Analyzing…" (`455`).

Recommendation: "Refresh" for finding games (already the header button on both platforms). "Analyze" for estimating savings. Drop "Scan".

### 1.6 "Jobs" / "Queue" / "work": CONFIRMED

| Term | Occurrences |
|---|---|
| "Jobs" | `app.rs:65`; `view.rs:1564`; `native.rs:1595, 1617, 1829`; "See jobs" `view.rs:311`; "View jobs and recovery" `native.rs:1352`; "Start job" `view.rs:248`, `native.rs:1304`; "Cancel job" `native.rs:1632` |
| "queue" / "Queued" | `view.rs:1799` ("when the queue is empty"); `jobs/service.rs:437` ("The queue is full."), `671` ("This job is already queued"); `jobs/mod.rs:135` and `jobs/service.rs:453` ("Queued") |
| "Waiting" | `view.rs:1586`, "No games waiting" `view.rs:1611`; `native.rs:1840`; `desktop_jobs.rs:34` ("Waiting"), `200` ("Waiting to start") |
| "work" | "Recent work" `view.rs:541`; "Track running work, waiting games, and recent results" `view.rs:1565`; "background work" `native.rs:1255, 1257, 1714`, `windows/tray.rs:155, 157` |
| "storage operation" | "A storage operation is running" `native.rs:1619, 1956` |
| "Running now" | `native.rs:1350` |

- A Linux job sitting under the "Waiting" heading carries the phase label "Queued".
- A Linux job under the "Running" heading (`view.rs:1585`) carries the phase label "Working" (`jobs/mod.rs:137`). Windows prints "Running" (`desktop_jobs.rs:35`).
- The button "Cancel" (`view.rs:1513`, `native.rs:1873`) produces a phase shown as "Stopped" (`jobs/mod.rs:141`, `desktop_jobs.rs:38`). Windows also has a "Stop" button (`native.rs:1217`) and "Cancel job" (`native.rs:1632`).
- Both `Phase::Partial` and `Phase::Failed` print "Needs attention" (`jobs/mod.rs:143, 145`), so the row cannot say which happened.

Recommendation: "Jobs" for the page and the noun. "Waiting" and "Running" for phases, to match the headings. "Stop" / "Stopped" as the verb and state.

### 1.7 "Locations" / "Drives & libraries" / "Libraries" / "folder": CONFIRMED

| Term | Occurrences |
|---|---|
| "Locations" | `view.rs:1756`; `native.rs:1596, 1646`; "Add a location" `view.rs:1642`; "Add location" `native.rs:1659`; "Remove location" `view.rs:1714`, `native.rs:1688`; "Choose a games location" `dialog.rs:56` |
| "Drives & libraries" | `view.rs:1638` (the title of the section the nav calls "Locations") |
| "Drives" | `app.rs:66`; `view.rs:507` (Overview heading, real drives); "All drives" `view.rs:639`; `jobs/service.rs:426` ("Restore it in Drives first.") |
| "Libraries" / "library" | button "Libraries" `view.rs:1818`; "{} librar{y is} maintained" `view.rs:1810`; "Detected library" `view.rs:1707`; "Library settings saved." `app.rs:1651`; "Games library" `jobs/mod.rs:253`, `desktop.rs:46`; "Library · {} games" `native.rs:1133` (here it means the game list) |
| "folder" | "Add folder" `view.rs:1659`; "Add a folder" `view.rs:692`; "Selected folder" `native.rs:1224`; "Remember this folder" `native.rs:1236`; "Choose storage folder…" `view.rs:1187` |

- `jobs/service.rs:426` sends the user to "Drives". `Page::Drives` is not in the sidebar (`app.rs:32`); it resolves to the Settings section the nav labels "Locations" and the heading labels "Drives & libraries".
- Inside one panel the heading is "Add a location" (`view.rs:1642`) and the button is "Add folder" (`view.rs:1659`).
- Kind labels differ by platform: Linux "Single game" (`jobs/mod.rs:252`), Windows/Mac "One game" (`desktop.rs:46`).

Recommendation: "Locations" for the section, "location" for an entry, "library" only for a location whose subfolders are games, "drive" only for a physical drive.

### 1.8 "worker" / "background worker" / "coordinator": PARTLY CONFIRMED

- "Background worker" `view.rs:1798`; "Restart worker" `view.rs:1800`; "Worker connection interrupted" `view.rs:1571`, `native.rs:1832`; "Connecting to the background worker…" `view.rs:1579`; "Start background worker" / "Stop background worker" `native.rs:1714`; "Flummox background worker" `windows/tray.rs:280`.
- Other names for it: "The background task stopped unexpectedly." `app.rs:1030`; "The background operation stopped unexpectedly." `native.rs:367`; "Storage worker stopped; review recovery before retrying" `jobs/service.rs:1388`; "Worker stopped. Review recovery before retrying." `desktop_jobs.rs:119`; "Worker control disconnected: {error}" `jobs/service.rs:1814`.
- "worker" with a different meaning: "Choose between 1 and 32 worker threads." `jobs/service.rs:413`.
- "coordinator" is REFUTED for UI strings. It appears only in docs (`docs/usage.md:59, 68, 90`; `docs/gui-architecture.md:8-10`) and process arguments.

Recommendation: "background worker" in the window and in user docs. Keep "coordinator" for design docs.

### 1.9 "items" vs "files": CONFIRMED

`job.files_done` is printed as "items" at `view.rs:1528` ("{} / {} items · {} processed · {}s") and as "files" at `view.rs:841` ("{} of {} files") and `view.rs:1913, 1920`. "item" at `view.rs:489` counts something else (games needing attention plus warnings).

Recommendation: "files" for the counter. `docs/usage.md:57` says "verification reports checked items", so if verification counts chunks the string needs a real unit, not "items".

### 1.10 "games" vs "remembered games": CONFIRMED, one occurrence

`native.rs:908` "Found {} remembered games." Related: "Remember this folder" `native.rs:1236`; "Remember or select the game folder first." `native.rs:736`. Linux has no "remember" wording.

Recommendation: "games". "Add location" already covers remembering.

### 1.11 Smaller drifts found on the way

| Concept | Variants | Canonical |
|---|---|---|
| Nothing to gain | "Little to gain" `app.rs:258`, `docs/usage.md:10`; "Little to save" `view.rs:856`; "little to save" `view.rs:989, 1460` | "Little to save", since every other figure says "to save" |
| Motion options | Linux "Expressive" / "Subtle" / "Reduced" `jobs/mod.rs:66-68` with helper "Smooth transitions" `view.rs:1847`; Windows/Mac "Smooth" / "Subtle" / "Reduced" `desktop.rs:45` | "Smooth", since the Linux helper already says it |
| Sort by name | Linux "Name" `app.rs:213`; Windows/Mac "Title" `native.rs:155` | "Name" |
| Updated filter | Linux "Updated games" `app.rs:280`; Windows/Mac "Updated" `native.rs:141` | "Updated" (the sibling filters are "Ready", "Compressed") |
| Reports | "Import compatibility report…" `view.rs:1235`; "Import report…" `view.rs:1876`; "Save local report" `qualification.rs:336`; nav "Reports" `view.rs:1760`; heading "Maximum Space compatibility" `view.rs:1874`; heading "Compatibility reports" `native.rs:1747`; "Compatibility report saved to {}" `app.rs:1362`; "Report saved to {}" `native.rs:799`; "Qualify compatibility" `view.rs:1228`, `native.rs:1234` | "compatibility report"; the verb "Qualify" needs a plainer word such as "Test compatibility" |
| Storage plan vs space plan | "Storage plan" `view.rs:224`, `native.rs:1284`; "review the space plan again" `storage.rs:337` | "storage plan" |

### 1.12 American vs British spelling: REFUTED for UI strings

Every UI string uses American forms ("Analyze", "Analyzing", "analyzed"). No "analyse", "colour" or similar exists in a string in the GUI files or `qualification.rs`. "Cancelled" exists only as an enum identifier and displays as "Stopped". The one British form in user-facing text is `README.md:77` "## Licence".

---

## 2. Formatting inconsistency

### 2.1 Ellipsis

All ellipses are the single character "…". No "..." exists in any UI string.

Labels that open a picker and carry "…": `view.rs:1187, 1235, 1655, 1876`; `native.rs:1229, 1267, 1657`.

Inconsistencies:
- The same control is "Choose local artwork" on Linux (tooltip, `view.rs:807`) and "Choose local artwork…" on Windows/Mac (`native.rs:1267`).
- In-progress text has "…" in the window (`view.rs:416, 434, 436, 442, 455, 1573, 1579`; `native.rs:416, 418, 987, 1135`) and none in worker messages shown beside it: "Preparing files" `jobs/service.rs:1912`, `windows/coordinator.rs:784`; "Restoring ordinary files at the launcher path" `jobs/packs.rs:163`; "Checking running games" `windows/coordinator.rs:460`; "Removing the retained original files" `pack/install.rs:369`.
- The same sentence exists in both forms: "Stopping after the current file…" `native.rs:987` and "Stopping after the current file" `windows/coordinator.rs:545`.

### 2.2 Case

Sentence case is the norm. Exceptions:
- ALL CAPS: "SPACE SAVED (ESTIMATE)" `view.rs:427`; "COMPRESS YOUR GAMES" `view.rs:429`. No other label is capitalised this way.
- Mid-line capital differs between two copies of one line: "Store: {} · updates: {}" `view.rs:1146` and "Store: {} · Updates: {}" `view.rs:1365`.
- "Maximum Space" is Title Case as a product name while "Standard strength" is not. This resolves itself if the mode is always "Maximum".

### 2.3 Trailing full stops on helper lines

With a full stop: `view.rs:239, 1007, 1010, 1051, 1113, 1124, 1126, 1133, 1188, 1189, 1338, 1428, 1549, 1645, 1646, 1799, 1875`; `native.rs:1710, 1714`.

Without: `view.rs:448, 498, 691, 701, 1181, 1565, 1639`; `native.rs:1117, 1169, 1171, 1232, 1647, 1699, 1747`; `unsupported.rs:39, 40`.

Empty-state lines disagree with each other: "No interrupted jobs or retained storage need review." `view.rs:1428` against "No jobs running" `view.rs:1610` and "No interrupted jobs need recovery" `native.rs:1699`.

Toasts end with a full stop except "Game no longer exists" `app.rs:721` and "Choose an existing game or games library" `native.rs:1500`.

Suggested rule: a full sentence ends with a full stop; a fragment (label, count, status) does not.

### 2.4 Separators

- "·" is the standard.
- The same Storage plan line uses "·" on Linux (`view.rs:228` "{}: {} needed including headroom · {} available") and ";" on Windows/Mac (`native.rs:1288` "{}: {} needed including headroom; {} available").
- Progress uses "/" at `view.rs:1528` and "of" at `view.rs:841`.
- Storage-plan reasons contain ";" (`jobs/mod.rs:585, 591, 606, 636`; `storage.rs:359`) and are then joined with " · " (`view.rs:235`), giving mixed separators on one line.
- "&" appears in "Drives & libraries" `view.rs:1638` and "Create & activate" `view.rs:1193`; elsewhere "and".
- No em dash exists in any UI string.

### 2.5 Units and numbers

- Raw seconds: "{}s" at `view.rs:1528, 1535, 1909, 1917` and "{}s elapsed" at `view.rs:1537`. A long job prints e.g. "3725s".
- Raw bytes in the storage plan error: `storage.rs:320` "Not enough space on {}: need {} additional bytes including safety headroom; {} available". It is printed at `view.rs:243` and `native.rs:1298`, directly below a line that humanises the same numbers.
- Raw bytes in the qualification form: "Baseline: {} files · {} logical bytes" `qualification.rs:254`; "{} allocated bytes" `qualification.rs:97, 113`; the inputs "Original allocated bytes" `:269` and "Compressed allocated bytes" `:274` expect bare integers.
- Milliseconds as typed integers: "Baseline load time (ms)" `qualification.rs:259`, "Compressed load time (ms)" `:264`.
- Everything else uses `humansize` DECIMAL consistently (`view.rs:23`; `native.rs:1291-1326`).
- Limits use binary units: "Compatibility report exceeds 1 MiB." `app.rs:1876`; "Artwork exceeds 32 MiB" `gui/artwork.rs:41`.

### 2.6 Hand pluralisation

Wrong or able to print a wrong form:

| Location | String | Bad output |
|---|---|---|
| `view.rs:489` | "{} item{} need attention" | "1 item need attention" (verb not conjugated) |
| `view.rs:195` | "Scanning {source} · {} games found" | "1 games found" |
| `view.rs:1875` | "{} local reports. Analysis checks…" | "1 local reports." |
| `view.rs:841` | "{} · {} of {} files" | "1 of 1 files" |
| `view.rs:1528` | "{} / {} items · …" | "1 / 1 items" |
| `view.rs:1263` | "Sampled {} from {} files · {} skipped · …" | "from 1 files" |
| `view.rs:1290` | "Small files: {} in {} files · …" | "in 1 files" |
| `view.rs:764` | "Show more · {} games" | "1 games" only if the page size is below 1; effectively safe |
| `native.rs:908` | "Found {} remembered games." | "Found 1 remembered games." |
| `native.rs:1133` | "Library · {} games" | "Library · 1 games" |
| `native.rs:1320, 1627` | "{} files processed · {} changed · {} skipped" | "1 files processed" |
| `native.rs:1897` | "{} files · {} changed · {} skipped" | "1 files" |
| `qualification.rs:254, 97, 113` | "{} files" | "1 files" |

Handled correctly: `view.rs:514, 729, 1810, 1913, 1920`; `app.rs:1111`.

---

## 3. Tone and clarity

### 3.1 Jargon shown without explanation

| Term | Where |
|---|---|
| "WOF/LZX", "LZX" | `native.rs:30` ("Windows · WOF/LZX", shown in About at `:1760`); `native.rs:34` feeding "Save space with LZX" `:1116`, "Compressing worthwhile files with LZX…" `:416`, stat "Mode" `:1120` |
| "btrfs" | "btrfs levels range from -15 to 15." `jobs/service.rs:419`; the raw filesystem name is also printed beside the install path at `view.rs:940-942` |
| "FUSE" | "A disconnected FUSE mount needs fusermount3 to recover" `pack/install.rs:190`; "The disconnected FUSE mount could not be cleared" `:198`. These are error contexts and reach the window only through raw error text (3.3) |
| "store", "chunks" | "Verified chunks can be shared across games" `view.rs:1181`; "Store: {} · updates: {}" `view.rs:1146`. The only explanation of "store" is `view.rs:1010` |
| "pack", "mount" | `jobs/service.rs:394, 606, 1320`; `jobs/packs.rs:487` ("Could not mount automatically: {error}"), `505`; `jobs/service.rs:1962` ("The mount stopped; Flummox will retry it"); `pack/install.rs:291` ("Writable compressed install is mounted; rollback copy retained") |
| "launcher path", "activation metadata" | `pack/install.rs:257` ("Activation recorded; preparing the launcher path"); `jobs/mod.rs:591, 606` ("Original retained; activation metadata") |
| "headroom" | `view.rs:228`; `native.rs:1288`; `storage.rs:320` |
| "extents", "blocks", "pinned" | `storage.rs:361` ("Worst-case native rewrite with snapshots or shared extents retaining old blocks"); `storage.rs:359` ("…old blocks may remain pinned"). Both print at `view.rs:235` |
| "allocated bytes", "logical bytes", "Baseline" | `qualification.rs:254, 269, 274, 330` |
| "corpus" | Not in any label. In error text only: "Corpus size overflow" `qualification.rs:226`; "Compatibility report corpus hash is invalid" `compatibility.rs:177`, `:181, :185` |
| Format evidence line | "Formats: {} known · {} unknown · {} encoded · {} containers · {} raw{}" `view.rs:1276` |
| "Qualify" | `view.rs:1228`; `native.rs:1234`; "Qualify {}" `qualification.rs:254` |
| "worker threads" | `jobs/service.rs:413` |
| "epoch" | REFUTED. No UI string contains it |

### 3.2 Raw identifiers shown to the user

- `view.rs:1736` "Excluded: {id}" prints the `GameId` Display (`model.rs:99`), for example "steam:105600", with no title.
- `view.rs:1361` uses an install path as a card title (`install.game_path.display()`) where every other card uses the game's title.

### 3.3 Raw error text passed straight to the user

- `app.rs:1288, 1345, 1367, 1380, 1393, 1425, 1432, 1553, 1568, 1662, 1740, 1890, 1910`: `Status::error(error)` with the underlying error string and no lead-in.
- `app.rs:1161` "Artwork preferences unavailable: {error}"; `app.rs:1195` "History is unavailable: {e}"; `app.rs:1202` "Compatibility reports could not be loaded: {error}".
- `app.rs:1321`, `native.rs:780` and `qualification.rs:102` use `{error:#}`, which prints the whole anyhow chain.
- `app.rs:1353` and `native.rs:769` "Could not open a browser ({error}). The changelog is at {}".
- `view.rs:1572` prints the connection error verbatim. `native.rs:1832` appends it: "Worker connection interrupted. Showing the previous jobs. {error}".
- `view.rs:1307, 1555` print `job.errors.join("\n")`. Entries are built as "{path}: {e}" at `jobs/worker.rs:383`. `jobs/worker.rs:503` sends `{error:#}`.
- `qualification.rs:333` prints the validation error. It reads acceptably ("Enter baseline load time in milliseconds as whole-number measurements", `:138`) but "Allocated-byte measurements must be positive" (`:166`) does not.
- `native.rs:1836` "Paused: {busy}", where busy can be `error.to_string()` (`windows/coordinator.rs:461`) or "Preferences unavailable: {error}" (`:386`).
- `dialog.rs:96` "Could not open the folder picker: {error}"; `jobs/packs.rs:115` "Activation needs recovery: {error}"; `jobs/packs.rs:487`; `jobs/service.rs:1814`.

### 3.4 Messages that say what failed and not what to do

| Location | String |
|---|---|
| `app.rs:721` | "Game no longer exists" |
| `app.rs:743` | "Maximum is not available for {} on this drive." |
| `app.rs:1030` | "The background task stopped unexpectedly." |
| `native.rs:367` | "The background operation stopped unexpectedly." |
| `app.rs:1803` | "The selected game is no longer installed." |
| `app.rs:1920` | "Cannot locate your home folder." |
| `app.rs:1876` | "Compatibility report exceeds 1 MiB." |
| `view.rs:526` | "Drive unavailable" |
| `view.rs:1299` | "No compatibility report matches this build" |
| `unsupported.rs:39` | "Compression is not available on this platform yet" |
| `windows/coordinator.rs:463` | "Desktop preferences need attention" |
| `jobs/service.rs:394` | "Pack jobs require a storage task" (internal protocol error) |
| `jobs/service.rs:535` | "Storage command is missing" (internal) |
| `jobs/service.rs:1320` | "Build Flummox with pack mounting to run storage jobs" (an instruction for someone compiling the program) |
| `jobs/packs.rs:505` | "This build cannot mount pack stores" |
| `jobs/service.rs:1042` | "Storage job stopped" |

Good models already in the code: `app.rs:112` "Reconnect the original drive and refresh."; `app.rs:726` "{}'s drive has no native compression. Open the game and choose Maximum."; `dialog.rs:99` "Install Zenity or KDialog to browse folders. You can still paste a path."; `jobs/service.rs:437` "The queue is full. Let some jobs finish first."; `jobs/service.rs:697` "Finish or cancel jobs in this location before removing it."

### 3.5 Contradicting or unclear lines

- `view.rs:239` "The original is retained until you explicitly reclaim it." says the same thing as `view.rs:1010` "the original is kept until you confirm it works" in different vocabulary.
- `view.rs:1188` describes steps as "1. Create and verify a store. 2. Play the game to test it. 3. Delete the original to save the space." while the buttons beside it read "Create & activate", "Create store only", "Activate existing" (`view.rs:1193-1199`). Step 1 does not name the button that performs it.
- `view.rs:1549` "Drive free-space change: {}{}. Includes other applications' disk activity." complies with the CLAUDE.md rule about free-space deltas.
- Savings are labelled as estimates at `view.rs:427, 1462` and by "About"/"about" elsewhere. `view.rs:735` "{name} · {count} · about {} saved" has "about" but not "(estimate)".

### 3.6 CLAUDE.md prose rules

- Rule 1 (em dashes): no violation in UI strings or in `docs/gui-architecture.md`, `docs/usage.md`. `README.md:50` has "you won't believe it-- the GUI!", a double hyphen doing the same job.
- Rule 2 (reversal): none found.
- Rule 3 ("rather than"): none in UI strings.
- Rule 4: none of the banned phrases. `view.rs:239` "explicitly" is adjacent in spirit and is removed by the rewording in 1.4.
- Rule 5: the banned phrases do not occur. Borderline: "the game's files stay exactly where they are." `view.rs:1007`, repeated at `docs/usage.md:17`. "stay where they are" loses nothing.
- Rule 8 ("silently", "quietly", "lies", "nobody"): none found.
- Stale comment: `view.rs:862` names a "Recheck" button that does not exist.

---

## 4. Docs drift

| Doc location | Says | Code has |
|---|---|---|
| `docs/gui-architecture.md:26` | "Jobs have their own page." | True on Linux only (`app.rs:32`, `view.rs:1564`). On Windows and Mac, Jobs is a Settings section (`native.rs:1595, 1617, 1751, 1829`), pages are Overview, Games, Settings (`native.rs:61-63`), and Overview offers "View jobs and recovery" leading to Settings (`native.rs:1352`). This is the reverse of the example in your brief: the doc and Linux agree, native does not |
| `docs/gui-architecture.md:19` | "The window uses three verbs: Compress, Decompress and Analyze." | The window also uses Restore, Verify, Reclaim, Fold in, Compact, Create, Activate, Qualify, Exclude, Remember, Scan, Refresh (sections 1.2 to 1.5). Windows recovery is "Restore ordinary storage" (`native.rs:42`) |
| `docs/gui-architecture.md:20` | "one mode choice, Standard or Maximum" | Holds on the game card (`view.rs:982-983`). The qualification form offers "Native compression" / "Maximum Space" (`compatibility.rs:59-60`); Windows/Mac "Mode" is "LZX"/"APFS" (`native.rs:1120`) |
| `docs/gui-architecture.md:22-23` | "The native preset, store paths and the compatibility form live under Advanced." | Matches Linux (`view.rs:1036-1051, 1146, 1228`). On Windows/Mac "Qualify compatibility" is a top-level button on the Games page (`native.rs:1234`) with no Advanced |
| `docs/gui-architecture.md:24-25` | "The plan is shown only when it fails." | Consistent with the comment at `view.rs:222`. Not checked on native, where "Storage plan" with "Start job" is at `native.rs:1284-1304` |
| `docs/gui-architecture.md:39` | "Automatic Maximum Space" | Uses the long mode name the same doc retires at line 20 |
| `docs/usage.md:33` | "Standard strength (Fast, Balanced, Max)" | Pick list prints "Fast", "Balanced", "Maximum" (`backend/mod.rs:668-671`, `view.rs:1042`) |
| `docs/usage.md:52` | "The Jobs page groups running, waiting, attention and completed work." | Headings are "Running", "Waiting", "Needs attention", "History" (`view.rs:1585-1588`). No Jobs page on Windows/Mac |
| `docs/usage.md:64-65` | "Settings > Maintenance opts libraries into background work." | On Linux the opt-in checkbox "Maintain new installs and updates" is in Locations (`view.rs:1723`). The Maintenance section shows only a count and a "Libraries" button (`view.rs:1805-1818`). On Windows the checkbox is also under Locations (`native.rs:1673`) |
| `docs/usage.md:73` | "Reduce motion is saved in Settings > Appearance." | The control is "Motion" with options "Expressive"/"Subtle"/"Reduced" on Linux (`view.rs:1844`, `jobs/mod.rs:66-68`) and "Smooth"/"Subtle"/"Reduced" elsewhere (`desktop.rs:45`). Nothing is labelled "Reduce motion" |
| `docs/usage.md:10` | "little to gain" | Group header matches (`app.rs:258`); the row under it says "Little to save" (`view.rs:856`) |
| `docs/usage.md:57` | "restoration and reclaim finish without interruption" | Window words are "Decompress" and "delete the original" |
| `docs/usage.md:123` | "A hidden entry" | Window says "Exclude this game" / "Excluded:" (`view.rs:1240, 1736`) |
| `docs/usage.md:106` | "Import a locally produced compatibility qualification from Settings or an expanded game" | Buttons are "Import report…" (`view.rs:1876`) and "Import compatibility report…" (`view.rs:1235`). The object is called a "report" in the window and a "qualification" here |
| `README.md:56-58` | "Maximum Space moves the original…", "before choosing Reclaim original" | No "Reclaim original" control exists in the window. The buttons are "It works, delete the original" and "Yes, delete the original" (`view.rs:1101, 1096`). "Reclaim original" survives only in `jobs/mod.rs:233` |
| `docs/status.md:18` | "Overview, Games and Settings navigation" | Linux sidebar is Overview, Games, Jobs, plus Settings (`app.rs:32`, `view.rs:158`) |
| `docs/release-readiness.md:16` | "Games, Drives, Settings, and Queue widgets" | "Drives" is a Settings section and "Queue" is displayed as "Jobs" (`app.rs:65`) |
| `docs/release-readiness.md:77` | "Drives & libraries supports…" | Matches the heading at `view.rs:1638`; conflicts with "Settings > Locations" in `docs/install.md:140` and `docs/usage.md:64` |
| `docs/install.md:140-142` | "choose **Games library**" / "Choose **Single game**" | Matches Linux (`jobs/mod.rs:252-253`). Windows/Mac print "One game" (`desktop.rs:46`) |

Checked and still accurate: `docs/usage.md:8-12` (Games, **Compress**, **Compress selected**: `view.rs:927, 676`); `docs/usage.md:26` ("It works, delete the original"); `docs/usage.md:29` ("Decompress to ordinary files"); `docs/usage.md:41-44` (Windows **Compress**, **Stop**, **Decompress**: `native.rs:1209-1217`); `docs/usage.md:72` (Ctrl+F, Ctrl+R, Escape: `app.rs:2031-2042`); `docs/install.md:45` and `docs/jobs-and-compression.md:142` ("Restart worker": `view.rs:1800`); `docs/install.md:149` ("Remove location").

---

## Proposed glossary

| Concept | Use everywhere | Retire |
|---|---|---|
| The three user actions | Compress, Decompress, Analyze | Restore (as an action), Scan, Recheck, Optimize |
| In-place mode | Standard | Native compression, native |
| Store-backed mode | Maximum | Maximum Space, pack, Maximum compression |
| The object Maximum creates | store | pack store, storage, Maximum store, compressed install |
| Strength inside Standard | Fast, Balanced, Max | "Maximum" as a strength |
| Undo compression | Decompress / Decompression | Restore ordinary files, Restore ordinary storage, Restoration |
| Check after an interrupted decompress | Check decompressed files | Verify restored files |
| Remove the kept original | Delete the original | Reclaim, Reclaim original, Reclaiming space |
| Remove the old store after an update | Delete the previous version | Reclaim previous version, Prune |
| Merge updates into the store | Fold in updates | Compact, Compaction |
| The original while it still exists | kept | retained, rollback copy |
| Find installed games | Refresh | Scan again, Scanning, discovery |
| Estimate savings | Analyze / Analysis / "Not analyzed yet" | Recheck |
| Nothing to gain | Little to save | Little to gain |
| The page and the noun | Jobs / job | Queue, work, storage operation |
| Job waiting its turn | Waiting | Queued |
| Job in progress | Running | Working |
| End a job | Stop / Stopped | Cancel, Cancel job |
| Settings section for folders | Locations | Drives & libraries, Drives |
| One added entry | location | folder (as a noun for the entry) |
| A location whose subfolders are games | Games library | collection |
| A location that is one game | Single game | One game |
| Physical disk | drive | volume, storage volume |
| Leave a game out | Exclude / Include | Restore (for un-exclude), hidden entry |
| The background process | background worker | coordinator (in user text), background task, background operation, storage worker |
| Count of files a job touched | files | items |
| A saved compatibility result | compatibility report | qualification, local report |
| Create a compatibility report | Test compatibility | Qualify |
| Motion setting, full | Smooth | Expressive |
| Sort by title | Name | Title |
| Sizes | humanised decimal (as now) | raw byte counts |
| Durations | humanised (for example "1 h 2 min") | raw "{}s" |
| Progress separator | "{} of {} files" | "{} / {} items" |
| List separator | "·" | ";" inside a "·" line |
| Dialog-opening label | ends in "…" | same control without it |
| In-progress status | ends in "…" in window and worker text alike | mixed |
| Spelling | American in UI strings (already consistent) | none found to retire |
