# Usage

Commands, keybindings, and automation options. For developer setup and checks, see [development.md](development.md).

## Interactive TUI Mode

Launch the interactive workspace by running:

```bash
taert
```

### Workspace Navigation

- **Main Menu**: Fast access to Clean Caches, Project Cleanup, Docker Cleanup, Uninstall Apps, Doctor, Startup Items, Slim Apps, Compress, Explore Storage, Large Files, System Status, and Preferences. Keys `1` to `9` and `0` open a row directly (rows past the 10th have no digit key).
- **Adaptive Layout**: On wide terminals (≥100 columns), the workspace splits into an action list on the left and details of the highlighted item on the right: disk usage for the cleanup, compress, and large files items, disk usage and the largest items of your home folder for Explore Storage, live CPU and memory for System Status, and service details for Startup Items. The main menu, System Status, Explore Storage, Startup Items, and item selection fill the window. Preferences, Review, and Scanning keep a readable width at the left edge. From 100 columns, lists in Explore Storage, Large Files, and item selection show a size bar that grows with the window. From 155 columns, Explore Storage shows what the highlighted folder holds in a panel beside the list.
- **Category Cleanup**: Inspect and clean Developer Caches, System Junk, AI Tool Caches (Ollama, LM Studio, Cursor), or Empty Trash. System Junk also lists every other folder in `~/Library/Caches` as App caches, and the caches of Safari, Chrome, Edge, Brave, Firefox, and Arc as Browser caches. Profiles stay untouched, so cookies, history, passwords, and extensions are safe. Folders that other rows already cover, `com.apple.*`, `CloudKit`, and `FamilyCircle` are left out. The Clean Caches screen starts with nothing selected, so you choose the rows. Each browser is one row. Safari and Arc keep two folders, so their row shows one path and `+1 folder`, and the confirmation names the second folder. See [Clean Caches](#clean-caches-taert-clean) for what starts selected on the command line.
- **Project Cleanup (Purge)**: Discovers disposable build directories (`target/`, `node_modules/`, `.venv/`, `.build/`, `dist/`) across configured project roots with configurable age filtering.
- **Docker Cleanup**: Lists images, volumes, and networks that no container refers to, stopped containers included, and the build cache. Each kind sits in its own framed, colored table with name, age, share, and size, and the age reads like `docker images`. The age column appears from 85 columns. `⇧S`, `⇧N`, and `⇧D` sort each table by size, name, or date. Only untagged images and unused networks start selected. Tagged images may be the only copy, the build cache takes time to rebuild, and volumes can hold real data, so you choose those yourself. The confirmation names any selected volumes, and a volume's data can't be recovered. taert talks to the same daemon as the `docker` command, and Docker refuses to remove anything still in use.
- **Uninstall Apps**: Lists the apps in `/Applications`, `~/Applications`, the input method folders, and `/Volumes/*/Applications` with their size and last use. `⇧S`, `⇧N`, and `⇧D` sort the list. Pick apps with `Space`, or press `Enter` on a single app. taert then lists the bundle and everything it found for that app, so you can change the selection before anything is removed. `Enter` asks for confirmation. Apps that are open are quit first. Personal files and apps move to the Trash by default (recoverable), while root files are permanently deleted. Use `--permanent` to skip Trash. See [Uninstall Apps](#uninstall-apps-taert-uninstall) for what is found and what starts selected.
- **Startup Items**: Review launch agents, launch daemons, and classic login items. Shows the owning app, service kind, live CPU% and memory if active, and whether the service is enabled, disabled, or broken (missing executable on disk). On wide terminals (≥100 columns), details appear in a side card. `Space` toggles an item (opens a confirmation modal naming the owning app before anything is switched off) or removes a broken user agent plist. Apple system services are hidden by default and read-only. See [Startup Items](#startup-items-taert-startup) for what counts as Apple's.
- **Explore Storage**: Multi-level interactive storage browser. Features breadcrumb navigation, live streaming progress during background measuring, item selection via `Space`, and deletion via `D` or `Backspace` (offering "Move to Trash" by default or "Delete Permanently").
- **Large Files**: Flat list of the largest and oldest files and packages across your home folder (or any given path) to answer "what big things did I forget about?". Shows item name, enclosing top-level folder, size, and last used date (queried via Spotlight, fallback to modified date). `⇧S`, `⇧N`, and `⇧D` sort by size, name, or last used date. Filter interactively with `/`, reveal in Finder with `O`, and select with `Space`. Selected items move to the Trash upon confirmation.
- **System Status**: A live overview refreshed every second: CPU and GPU load, per-core activity, temperatures, memory breakdown and pressure, storage volumes and disk speed, battery charge and health, top processes, and network speed. Each panel sits in its own frame. On wide terminals (≥100 columns) the panels sit in two columns, and from 170 columns in three; on narrower ones they stack and scroll. CPU and memory face each other, as do network and storage, so their graphs line up, and taller windows give each graph more rows. The process lists keep a fixed size, and tall windows add a list of the top processes by memory. Reading all of it needs no administrator rights.

Every screen shows its sections in colored frames. While taert scans or measures, a framed card in the middle of the window shows the progress.

### Keyboard Shortcuts

| Key | Context | Action |
|---|---|---|
| `↑` / `↓` or `k` / `j` | All screens | Move selection cursor |
| `Space` | Select / Explore / Startup | Toggle focused item selection / toggle startup service |
| `A` / `N` | Select / Explore | Select all visible / clear selection |
| `D` / `Backspace` / `Delete` | Explore Storage | Delete selected item(s) with confirmation dialog |
| `⇧S` / `⇧N` / `⇧D` | Explore Storage / Large Files / Docker / Uninstall Apps | Sort by size / name / date |
| `↵` (Enter) | Select / Review | Review selection / Confirm cleanup / Review the focused app in Uninstall Apps |
| `↵` / `→` | Explore Storage | Enter selected folder |
| `⎋` (Esc) / `←` | Explore Storage | Go up one directory level; from the top folder, leave Explore Storage |
| `q` | Explore Storage | Leave Explore Storage from any folder |
| `R` | Explore Storage | Refresh and re-scan current folder |
| `O` | Explore Storage / Select | Reveal selected item in Finder (`open -R`) |
| `/` | Select / Search | Live search filter |
| `?` | All screens | Toggle keyboard shortcuts help sheet |
| `⎋` (Esc) / `q` | All screens | Cancel dialog / Go back to the previous screen, on the row you left / Quit from the main menu |

The bottom row lists the keys that work right now. In a narrow window the less important ones drop off, and `?` still lists every key of the screen. A letter marked `⇧` needs Shift, and a letter without it doesn't.

## Non-Interactive CLI Commands

Every action can run directly from the command line without launching the full TUI:

### Clean Caches (`taert clean`)

```bash
# Scan developer caches (Xcode, Cargo, CocoaPods, Homebrew, Gradle)
taert clean dev

# Scan system junk, user logs, app caches, and browser caches
taert clean junk

# Scan AI tool model and artifact caches
taert clean ai

# Scan and empty macOS Trash
taert clean trash

# Preview without deleting anything
taert clean dev --dry-run

# Output plain text without terminal escape codes
taert clean junk --plain

# Non-interactive deletion of default-selected items
taert clean dev --force
```

`--force`, `--plain`, `--dry-run`, and `--json` use the same starting selection, and `--force` removes only what starts selected. The Clean Caches screen starts with nothing selected instead.

An app cache starts selected when nothing in it changed for 30 days and its app is not running. taert finds the app by bundle id, so it knows an app is open only when the cache folder is named after that id. A browser cache starts selected when the browser is not running. taert finds a browser by bundle id, so a renamed one counts, and any Firefox channel keeps the cache they share. An extension that keeps running after you quit Safari does not count. If taert cannot list the running programs, no app or browser cache starts selected and a note says so. Rows of named tools, such as Homebrew, keep their own default.

`--plain` and `--dry-run` print both folders of a Safari or Arc row, and `--json` puts the second under `also`.

Safari keeps its cache in `~/Library/Caches/com.apple.Safari` and in a container. macOS may refuse both reads unless your terminal has **Full Disk Access**. Without it, taert skips those folders and shows a note on the list, in the plain output, and after a `--force` run.

### Project Build Artifacts (`taert purge`)

```bash
# Scan projects in ~/Projects
taert purge ~/Projects

# Only include artifacts older than 30 days
taert purge ~/Projects --older-than 30

# Output structured JSON for automation
taert purge ~/Projects --json

# Preview dry-run
taert purge ~/Projects --dry-run
```

### Docker Cleanup (`taert docker`)

```bash
# Preview what no container uses
taert docker --dry-run

# Structured JSON for automation
taert docker --json

# Remove only what starts selected: untagged images and unused networks
taert docker --force
```

`--force` never removes tagged images, volumes, or the build cache. taert follows `DOCKER_HOST`, `DOCKER_CONTEXT`, and the current Docker context, and supports local sockets only, so a remote daemon stops with an error.

### Uninstall Apps (`taert uninstall`)

```bash
# Pick apps from the list
taert uninstall

# Review one app, by name, bundle id, or full path
taert uninstall Zoom
taert uninstall us.zoom.xos

# Preview without deleting anything
taert uninstall Zoom --dry-run

# Structured JSON: the plan for named apps, or every app without a name
taert uninstall Zoom --json
taert uninstall --json

# Remove what starts selected, without asking
taert uninstall Zoom --force
```

A name can be part of an app name as long as only one app matches. A name that fits several apps is an error. `--force` needs a whole name, so a partial match never removes anything unreviewed. `--dry-run` wins over `--force`.

taert looks for:

- preferences, caches, logs, saved state, containers, group containers, web data, cookies, and application scripts named after the bundle id;
- folders named after the app, in `~/Library`, `~/.config`, `~/.cache`, `~/.local/share`, and under the vendor's folder;
- launch agents and daemons, by name or because they start a program inside the app;
- browser native messaging hosts that start the app, data of its extensions and helpers, and crash reports;
- files outside your home folder: `/Library` folders, privileged helpers, `/Users/Shared`, and installer receipts.

What starts selected depends on how sure taert is that a file belongs to the app:

| Starts selected | You choose |
|---|---|
| The app itself | Browser profiles and game or emulator libraries |
| Files named after the bundle id, in your home folder and in `/Library` | License and plugin state of tools such as iLok, Native Instruments, and iZotope |
| Launch and native host entries that start the app | Group containers, when another app of the same vendor stays |
| Launch and helper files from the app's installer package | Folders named after the app in `~/.config`, `~/.local/share`, `/Library`, and `/Users/Shared` |
| Folders named after the app in `~/Library` and `~/.cache` | Folders and launch entries that only mention the app's name, or use a common or short one |
| Installer receipts | Folders named after the app without its channel, like `Zed` for Zed Nightly, and files that continue a two-part id such as `md.obsidian` |
| | Crash reports in `/Library/Logs/DiagnosticReports` |

Removal runs in this order. Open apps get a quit request and up to 40 seconds to answer it, so they can ask you to save. A Homebrew cask goes next, through `brew uninstall --cask`, without `--zap`. Then, if anything needs root, macOS asks for your password. An app with files only root can remove goes in that same step, so a cancelled prompt leaves it installed, with its login item and background jobs in place. After that taert stops what is left: launch agents are unloaded, the app leaves Launch Services, the login item that opens this copy leaves Login Items, its login helpers stop, and remaining processes get `SIGTERM`, then `SIGKILL`. An app's other files go only after the app itself, so a failed removal never leaves an app without its data. Last, its Dock tile goes.

The password is asked once per run, and a refused prompt is not repeated. There are two exceptions: if macOS refuses a removal only once it has started, taert asks again for that app, which is stopped by then, and two selected copies of one app that share root files need a second prompt, because those files go only after both copies. An app outside the fixed system folders, such as one in `~/Applications`, goes first, and its files in `/Library` follow after the password. If you cancel, those files stay and taert lists them.

If taert runs in a terminal of the app being removed, the app stays open until you quit it, and the bundle goes anyway.

Another copy of the app, another channel of it such as Foo Beta, or an app whose id continues this one keeps everything shared: only the selected bundle goes. taert looks for those in the app folders, Setapp, Homebrew's Caskroom, and on mounted volumes, and again right before it removes anything.

An app in `/Applications` that is a link goes alone: the link is removed, and the app it points to stays, with its files. A Homebrew cask that holds several apps stays with Homebrew: only the selected app goes, and `brew list` keeps showing the cask. Security agents such as CrowdStrike, Jamf, and SentinelOne are left alone, because they need their vendor's uninstaller. Paths on the ignore list are left out too.

taert never edits your configuration. If `~/.zshrc`, `~/.config/fish/config.fish`, or a similar file still mentions a removed path, taert names the file and line.

If macOS refuses a removal, give your terminal **Full Disk Access** for files in `~/Library`, and **App Management** for app bundles, in System Settings > Privacy & Security.

### Large Files (`taert large`)

Answers "what big things did I forget about?" with a flat list of the largest and oldest files and packages across your home directory (or any specified path).

```bash
# Scan largest files in home directory (default: >= 100 MB, up to 100 items)
taert large

# Scan a specific directory
taert large ~/Downloads

# Filter by minimum size (supports KB, MB, GB, etc.)
taert large --min-size 500M

# Filter files older than a given number of days (Spotlight last-used date, fallback to modified)
taert large --older-than 180

# Limit the number of items returned (default: 100)
taert large --limit 50

# Plain text output for scripts or terminals
taert large --plain

# Structured JSON output
taert large --json

# Dry-run preview
taert large --dry-run
```

#### Safety and Exclusions

- **Safety First**: Items start unselected. `taert large --force` is explicitly refused with an error to protect personal files from unattended deletion.
- **Trash Default**: Selected items move to Trash (recoverable), never permanently deleted, unless `--permanent` is explicitly provided.
- **Skip Rules**: Automatically skips `~/Library`, `.Trash`, `.git`, `node_modules`, standard build artifacts, cloud dataless files, symlinks, and credential stores (`~/.ssh`, `~/.aws`, `~/.gnupg`).
- **Packages as Single Items**: macOS bundles and packages (`.app`, `.photoslibrary`, `.fcpbundle`, `.logicx`, `.imovielibrary`, `.band`) are counted as single units with their total allocated size and are never walked into.

### Storage Directory Analysis (`taert analyze`)

```bash
# Analyze disk usage of a path
taert analyze ~/Downloads

# Plain text output for scripts or logs
taert analyze ~/Downloads --plain
```

### Diagnose Slowdowns & Battery Drain (`taert doctor`)

Answers "why is my Mac slow, hot, or draining its battery?" with named causes and evidence. `taert doctor` watches the Mac for a few seconds, lists what stands out, and suggests one action for each finding.

The tone stays factual. There is no health score, no red counters, and no claim to speed anything up. If nothing stands out, it says so and lists what was checked.

```bash
# 10 second check
taert doctor

# Longer check, catches intermittent background activity
taert doctor --seconds 60

# Plain text table for scripts or redirected output
taert doctor --plain

# Machine-readable JSON output
taert doctor --json
```

#### Checks and Findings

- **Sustained CPU**: Detects processes using over 80% of a core across every sample of the observation window. A process spiking in only a single sample is not flagged.
- **System work**: Explains known macOS background processes instead of blaming them:
  - `mds_stores` & `mdworker_shared`: Spotlight indexing files.
  - `backupd`: Time Machine backing up files.
  - `photoanalysisd` & `mediaanalysisd`: Photos analyzing library in the background.
  - `softwareupdated`: macOS downloading or preparing updates.
  - `kernel_task`: macOS throttling CPU cycles to help cool down.
  - `WindowServer`: Display compositor rendering graphics or external displays.
- **Background energy & wakeups**: Catches background processes high on energy or wakeups per second while CPU usage appears low.
- **Memory & Swap**: Checks memory pressure and active swap growth, listing the largest memory-consuming processes.
- **Low disk space**: Warns when APFS available space drops below 10% (critical below 5%) and suggests Explore Storage (`taert analyze`).
- **Sleep blockers**: Detects power assertions (`PreventUserIdleSystemSleep`, `PreventUserIdleDisplaySleep`, `PreventSystemSleep`) held for over 10 minutes. Ignores display sleep assertions from `powerd` and `UserIsActive`.
- **Wake reasons**: Gathers wake events from the last 24 hours, grouping by reason (e.g. `wifibt`) and reporting frequent wakeups or wake requests.
- **Thermal throttling**: Detects thermal or performance warnings recorded by the system.
- **Network traffic**: Highlights background processes transferring data above 1 MB/s over the observation window.
- **Broken startup items**: Flags launch agents or daemons whose target executable files no longer exist.

### Startup Items (`taert startup`)

```bash
# Interactive TUI list
taert startup

# Plain text table for scripts or terminals
taert startup --plain

# Machine-readable JSON output: an object with "items" and "notes"
taert startup --json

# Include Apple system items and background task management records (asks for the administrator password once)
taert startup --all

# Switch off a service or login item
taert startup disable com.example.service

# Switch a service back on
taert startup enable com.example.service

# Preview switching off without changing state
taert startup disable com.example.service --dry-run
```

`disable` and `enable` take the label of the item. A login item can also be named by its name, or by its path, such as `taert startup disable /Applications/Example.app`. A name that fits several items is refused, and the error lists them, with the path of each login item. When disabling a launchd service, `taert` disables it before running `bootout` so launchd will not automatically restart it. Afterwards `taert` reads the state again and reports it, and a step that did not take effect is an error. In the interactive list, an item that is switched off but still running offers "stop". With `--all`, entries from the Background Task Management records that taert cannot switch show as read-only `background item` rows. When disabling a classic login item, `taert` removes it from System Events and saves its restore information in `~/.config/taert/disabled_login_items.json`. If taert cannot read that file, it does not write over it, and switching a login item off fails with an error that names the file. Apple system services and security agents (CrowdStrike, SentinelOne, Jamf) are strictly read-only and refused. A service counts as Apple's when its label starts with `com.apple.`. A service with no plist in `~/Library/LaunchAgents`, `/Library/LaunchAgents`, or `/Library/LaunchDaemons` is also Apple's when launchd shows its plist or program in a system folder such as `/System` or `/usr/libexec`, as for `com.openssh.ssh-agent`, or when an Apple bundle registered it. So is such a daemon when launchd shows none of its files, unless an app outside Apple registered it. Any other service with no plist in those folders is read-only too: an app registered it, or its plist lives elsewhere, such as in `~/Library/Application Support`. launchd forgets such a service once it stops, so taert could not switch it back on. Switch it in System Settings > General > Login Items & Extensions, or in the app. Switching a daemon with a plist in `/Library/LaunchDaemons` asks for the administrator password. A plist in those folders counts only by its label, so a user agent that runs `/usr/bin/osascript` stays switchable.

### Universal App Slimming (`taert slim`)

Reclaim gigabytes of APFS storage on Apple Silicon Macs by stripping unneeded Intel (`x86_64` / `i386`) code slices from universal applications. The native ARM (`arm64` / `arm64e`) slices remain intact and untouched, preserving code signatures, while the original unmodified app bundle is moved to the Trash.

> **Note on App Updates:** App updates (installed via the web, Sparkle updater, or Homebrew) replace the entire application bundle with a new universal binary, bringing Intel code back. Simply re-run `taert slim` after major app updates to reclaim space again.

```bash
# Interactive TUI candidate selection
taert slim

# Slim a specific application by name or partial name
taert slim Slack

# Preview slimming and estimated savings without modifying anything
taert slim Slack --dry-run
taert slim --dry-run

# Structured JSON report of eligible and ineligible apps
taert slim --json
taert slim Slack --json

# Slim without interactive confirmation prompt
taert slim Slack --force
```

#### How Slimming Works

1. **Safety Check**: Checks that your Mac runs Apple Silicon (`sysctl hw.optional.arm64`). If run on an Intel Mac, `taert slim` stops immediately with a clear message: `"Slimming universal apps requires an Apple Silicon Mac."`
2. **APFS Clone**: Clones the candidate application bundle alongside itself using `ditto --clone` on APFS without duplicating disk blocks.
3. **Binary Thinning**: Scans the Mach-O fat headers inside the cloned bundle and strips x86_64 slices, rewriting the fat headers to retain only ARM slices with standard power-of-two alignment. Files sealed by whole-file hashes in `_CodeSignature/CodeResources` are left alone to ensure code signing integrity.
4. **ARM Verification**: Inspects Mach-O architecture headers to verify that a valid ARM slice (`CPU_TYPE_ARM64` / `CPU_TYPE_ARM64_32`) is present and preserved.
5. **Signature & Gatekeeper Verification**: Validates the slimmed bundle using `codesign --verify --deep --strict` and `spctl -a -t exec`. If extended attributes (`com.apple.FinderInfo` or `com.apple.ResourceFork`) interfere, taert cleans them up and retries verification.
6. **Original Bundle Verification**: Verifies that the original application bundle remained unchanged and that no process inside it was launched while slimming was in progress.
7. **Atomic Swap & Trash**: Atomically exchanges the slimmed copy and the original bundle using APFS `renamex_np(RENAME_SWAP)`, then moves the original unslimmed app to the Trash (`Host::trash`). If any verification step fails prior to the swap, the temporary clone is removed and the original application is left completely untouched.

#### Eligibility & Exclusion Rules

`taert slim` verifies that each application is safe to slim:
- **System Applications**: Apple bundles (`com.apple.*`) and system roots are strictly refused.
- **Mac App Store Apps**: Apps with an App Store receipt (`_MASReceipt`) are skipped because macOS App Store verification requires the original unmodified bundle.
- **Running Applications**: Running applications are detected via `/bin/ps` and skipped to prevent file locks and memory corruptions.
- **Rosetta Preference**: Apps configured to run under Rosetta (`LSArchitecturesForX86_64` in `com.apple.LaunchServices.plist`) are excluded so Rosetta compatibility is not broken.
- **User Ownership**: Only user-owned applications can be slimmed without root escalation. If permissions fail, `taert` reports a hint to grant App Management in System Settings.
- **Developer ID Signing**: Only apps signed with a valid Developer ID certification authority (or ad-hoc signed when `TAERT_SLIM_ALLOW_ADHOC=1` is set) are eligible.

### Transparent APFS Compression (`taert compress`)

Reclaim gigabytes of APFS storage transparently using macOS kernel-level decmpfs compression. Compressed files remain completely transparent to apps, CLI tools, and the OS:
- **Zero code or file format changes**: files decompress on-the-fly in-memory during read operations.
- **Code signatures preserved**: `codesign --verify --deep --strict` passes without any modification or re-signing.
- **Zero launch overhead**: apps launch at indistinguishable speeds.
- **Transparent unpack on write**: if any application or tool writes to a compressed file, APFS automatically and transparently unpacks the file back to standard uncompressed storage.

```bash
# Preview compression savings on a directory or file
taert compress ~/.rustup --dry-run

# Compress a directory or bundle
taert compress /opt/homebrew/Cellar

# Output structured JSON report
taert compress /opt/homebrew/Cellar --json

# Undo compression, restoring plain uncompressed files
taert compress ~/.rustup --undo
```

#### What gets compressed

Ideal candidates for transparent compression:
- Developer toolchains: `~/.rustup`, `~/.cargo/bin`, LLVM, Go toolchains.
- Homebrew packages: `/opt/homebrew/Cellar` or `/usr/local/Cellar`.
- Installed applications: `/Applications/*.app` and `~/Applications/*.app`.

#### Safety and Skip Rules

`taert compress` protects system integrity and data safety:
- **Safety Gate**: Allowed in home directories (`~`), user-owned application bundles, and Homebrew prefixes. Strictly refuses system directories (`/System`, `/usr/bin`, `/sbin`), root trees, running application bundles (also when named with a relative path, found in `~/Applications`, or when `ps` fails), credential stores (`~/.ssh`, `~/.aws`, `~/.gnupg`, etc.), and the places where apps keep live data (`~/Library/Application Support`, `~/Library/Containers`, `~/Library/Group Containers`, `/opt/homebrew/var`, `/usr/local/var`). The interactive screen uses the same gate.
- **Per-file skip rules**: Files already compressed (`UF_COMPRESSED`), files with hardlinks (`st_nlink > 1`), APFS clones sharing blocks (`EF_SHARES_ALL_BLOCKS`), cloud placeholders (`SF_DATALESS`), files not owned by the current user, files with the `uchg`, `schg`, `uappnd`, or `sappnd` flag or in a folder with one, files whose ACL (or whose folder's ACL) denies deleting, files that a program had open for writing when the run started (one `lsof` listing; a program that opens a file later is not seen), or files saving less than 4 KB are automatically skipped without modification.
- **Atomic 4-step pipeline**: Every file is compressed to a temporary sibling file, metadata (owner, flags, xattrs, then times including the birth time, ACL, and mode) is copied, byte-for-byte readback verification is executed, and files are exchanged atomically via `renamex_np(RENAME_SWAP)`. If the original changed since it was read, the file is left as it is, or swapped back after the swap. Any error, including a metadata error, leaves the original file unmodified. Files are read block by block, so large files do not fill memory. Leftover `taert-tmp` copies from earlier runs are removed. A leftover that may be the original with changes is kept and listed as a failure.
- **Interactive TUI**: The **Compress** row in the **Optimize** section of the workspace scans user-owned applications, Homebrew packages, and developer toolchains, showing savings estimated from sampled real data and letting you review candidates before compressing. Failures are listed in the result, and `Esc` stops the run after the current file.
- **After updates**: An app or package update writes plain files again. Run `taert compress` again afterwards.

### Removal History (`taert history`)

Keep a local record of what taert removed or changed, so you can always answer "what happened to that file?".

```bash
# View recent cleanup runs (newest run first)
taert history

# Limit the number of runs shown
taert history --limit 20

# Output structured JSON with items for automation
taert history --json

# Inspect every item removed or changed in a specific run
taert history show 20261101-134000-4123
```

Example history summary:

```text
2026-11-01 13:40  clean dev        force        removed 12, 8.1 GB
2026-10-28 09:12  uninstall Foo    interactive  trash 14, 2.3 GB, 1 failed
2026-10-27 22:03  purge ~/Projects interactive  interrupted after 3 of 9
```

- **Format & Location**: Stored as line-delimited JSON at `~/.config/taert/history.jsonl` with permissions `0600`. Because `~/.config` is protected by the safety engine, no cleanup can ever touch or delete this file.
- **Interrupted runs**: Events are logged as they happen. An interrupted cleanup or process kill leaves a readable trail showing what completed before the interruption.
- **Bounded size**: Keeps the last 1,000 runs automatically.
- **Overrides**: `--dry-run` writes nothing to the log. Set `TAERT_NO_HISTORY=1` to disable recording completely.

### Scheduled Cleanup (`taert schedule`)

Keep caches under control with no manual runs. You choose categories and recurrence frequency, and macOS runs the cleanup in the background via `launchd`. It removes only what `--force` would remove, and only when enough space can be freed.

```bash
# Schedule weekly cleanup for dev and junk caches when at least 1 GB can be freed
taert schedule set --every week --categories dev,junk --min-reclaim 1G

# View schedule status, last run details, and health checks
taert schedule show

# Turn off scheduled cleanup and remove launch agent
taert schedule off
```

- **`--every`**: Frequency interval: `day` (daily at 09:00), `week` (Mondays at 09:00, default), or `month` (1st of each month at 09:00).
- **`--categories`**: Comma-separated list of any of `dev`, `junk`, `ai`, and `trash` (default `dev,junk`).
  - `purge`, `docker`, `uninstall`, and `clean orphans` cannot be scheduled; their results require interactive review.
- **`--min-reclaim`**: Minimum reclaimable space threshold (default `1G`). If scanning finds less than this amount, the run is skipped and the skip is recorded.

#### Launch Agent & Full Disk Access

Scheduled cleanup installs a user launch agent into `~/Library/LaunchAgents/io.github.ingvarch.taert.schedule.plist`:
- Missed calendar intervals run automatically when your Mac wakes.
- `taert schedule run` executes silently in the background, acquiring a lock file at `~/.config/taert/schedule.lock` to prevent overlapping runs.
- Because `launchd` launches `taert` directly in the background, permissions granted to your terminal app do not apply. Any rows that fail with `EPERM` are logged as failures, and `taert schedule show` will warn and provide instructions on how to grant Full Disk Access to the stable binary (e.g. `/opt/homebrew/bin/taert`).

#### Interactive Preferences

You can also switch on and adjust scheduled cleanup in the TUI workspace under **Preferences** → **Scheduled cleanup** using `←` and `→` to cycle between `Off`, `Every day`, `Every week`, and `Every month`.

### Trash & Permanent Deletion

By default, taert moves personal files and user apps to macOS Trash instead of deleting them immediately:
- **Explore Storage (`taert analyze`)**: Dialog offers **Move to Trash** (default) and **Delete Permanently**.
- **Large Files (`taert large`)**: Selected files and packages move to Trash (default).
- **Uninstall Apps (`taert uninstall`)**: User-level app bundles and leftovers in home folders move to Trash. Files requiring root (`/Library/...`) and package receipts are deleted permanently because root has no Trash.
- **Caches, Build Artifacts & Docker**: Caches (`taert clean`), project artifacts (`taert purge`), Docker items (`taert docker`), and emptying the Trash are always permanently removed.
- **`--permanent` flag**: Global CLI flag (`taert --permanent ...`) that skips Trash and deletes files and apps permanently.

For configuration and preferences, see [configuration.md](configuration.md).
