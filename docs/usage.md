# Usage

Commands, keybindings, and automation options. For developer setup and checks, see [development.md](development.md).

## Interactive TUI Mode

Launch the interactive workspace by running:

```bash
taert
```

`--dry-run` does not open the interactive screen. Add `--plain` to see the preview.

### Workspace Navigation

- **Main Menu**: Fast access to Clean Caches, Project Cleanup, Docker Cleanup, Uninstall Apps, Homebrew, Doctor, Startup Items, Slim Apps, Compress, Explore Storage, Large Files, System Status, Preferences, and Protected Paths. Keys `1` to `9` and `0` open a row directly: `5` is Homebrew, `6` is Doctor, and `0` is Explore Storage. Large Files and the rows after it have no digit key. In a short window the menu scrolls with the highlighted row, and its first or last line says how many rows are out of view, like `3 more below`.
- **Adaptive Layout**: On wide terminals (≥100 columns), the workspace splits into an action list on the left and details of the highlighted item on the right: disk usage for the cleanup, compress, and large files items, disk usage and the largest items of your home folder for Explore Storage, live CPU and memory for System Status, and service details for Startup Items. The main menu, System Status, Explore Storage, Startup Items, and item selection fill the window. Preferences, Review, and Scanning keep a readable width at the left edge. From 100 columns, lists in Explore Storage, Large Files, and item selection show a size bar that grows with the window. From 155 columns, Explore Storage shows what the highlighted folder holds in a panel beside the list.
- **Category Cleanup**: Inspect and clean Developer Caches, System Junk, AI Tool Caches (Ollama, LM Studio, Cursor), Orphaned App Files, or Empty Trash. System Junk also lists every other folder in `~/Library/Caches` as App caches, and the caches of Safari, Chrome, Edge, Brave, Firefox, and Arc as Browser caches. Profiles stay untouched, so cookies, history, passwords, and extensions are safe. Folders that other rows already cover, `com.apple.*`, `CloudKit`, and `FamilyCircle` are left out. The Clean Caches screen starts with nothing selected, so you choose the rows. Each browser is one row. Safari and Arc keep two folders, so their row shows one path and `+1 folder`, and the confirmation names the second folder. See [Clean Caches](#clean-caches-taert-clean) for what starts selected on the command line.
- **Project Cleanup (Purge)**: Discovers disposable build directories (`target/`, `node_modules/`, `.venv/`, `.build/`, `dist/`) across configured project roots with configurable age filtering.
- **Docker Cleanup**: Lists images, volumes, and networks that no container refers to, stopped containers included, and the build cache. Each kind sits in its own framed, colored table with name, age, share, and size, and the age reads like `docker images`. The age column appears from 85 columns. `⇧S`, `⇧N`, and `⇧T` sort each table by size, name, or date. Only untagged images and unused networks start selected. Tagged images may be the only copy, the build cache takes time to rebuild, and volumes can hold real data, so you choose those yourself. The confirmation names any selected volumes, and a volume's data can't be recovered. taert talks to the same daemon as the `docker` command, and Docker refuses to remove anything still in use.
- **Uninstall Apps**: Lists the apps in `/Applications`, `~/Applications`, the input method folders, and `/Volumes/*/Applications` with their size and last use. `⇧S`, `⇧N`, and `⇧T` sort the list. Pick apps with `Space`, or focus one app. `d`, `D`, `Backspace`, or `Delete` opens the review, which lists the bundle and everything taert found for that app, so you can change the selection before anything is removed. In the review the same keys ask for confirmation. `Enter` does nothing in either list. Apps that are open are quit first. Personal files and apps move to the Trash by default (recoverable), while root files are permanently deleted. Use `--permanent` to skip Trash. See [Uninstall Apps](#uninstall-apps-taert-uninstall) for what is found and what starts selected.
- **Homebrew**: Lists the installed formulae and casks with their size, an `ORIGIN` column that says whether you installed a formula (`on request`) or it came as a dependency (`dependency`), and an `UPDATE` column with the new version of an outdated package after an up arrow (like `↑ 1.4.4_1`, or the arrow alone when Homebrew gives no version). The name shows the version, `used by N`, `pinned`, and the apps of a cask. `ORIGIN` shows from 100 columns, like `SHARE`, and the `SHARE` bar is never wider than 12 cells. The status line adds `N updates` when Homebrew offers a newer version for some package. The first time the list opens in a run, from the menu or with `taert brew`, taert runs `brew update` in the background while the status line reads `Checking for updates…`, then reads `brew outdated` again and shows the new versions in place, with the rows, sort, search, and focus kept. Every key works meanwhile, except that `u` and `⇧U` wait for the check to end. If `brew update` fails, a toast says `Could not check for updates: ` and the last line Homebrew printed, and the list shows what `brew outdated` reports. The check goes on when you leave the list. A list you open again before it ends shows `Checking for updates…` and gets its result. If the check ends while the list is closed, its result is dropped and the next opening shows what its own read finds. `taert brew --plain` and `taert brew --json` do not run `brew update`. The formulae you installed come first, with their dependencies inside as a tree, then the formulae nothing you installed needs, then the casks, largest first by default. `⇧S`, `⇧N`, and `⇧T` sort the top level, the open rows, and the search results by size, name, or install date, and keep the open rows and the focus. An `INSTALLED` column shows how long ago each package was installed. You can search and scroll, but you cannot select several rows, so `Space`, `A`, and `n` do nothing. `d`, `D`, `Backspace`, or `Delete` removes the focused formula (`⇧T` sorts by date), as described under [Remove a formula](#remove-a-formula-taert-brew-uninstall). `u` upgrades the focused package and `⇧U` upgrades every package that has a new version and is not refused, as described under [Upgrade from the list](#upgrade-from-the-list). Casks are removed with Uninstall Apps. While taert reads Homebrew, the loading card names the step: `Reading Homebrew packages`, `Checking package versions`, then `Measuring packages, 12 of 180` with a progress bar and the count and size of the packages measured so far. See [Homebrew Packages](#homebrew-packages-taert-brew).
- **Startup Items**: Review launch agents, launch daemons, and classic login items. The list has a titles row and the columns `NAME` (with the owning app when its name differs), `TYPE` (the service kind), `STATE`, `CPU`, and `MEMORY` (live numbers of a running item, empty when taert has none). The state is `running` in green, `enabled` in the accent color, `disabled` in dim gray, `disabled · still running` in yellow, or `broken` in red, when the executable is missing on disk. The symbol at the start of a row has the color of its state. In a narrow window `TYPE` hides first, then `CPU` and `MEMORY`; `NAME` and `STATE` always show. On wide terminals (≥100 columns), details appear in a side card. `Space` or `Enter` toggles an item (opens a confirmation modal naming the owning app before anything is switched off). On a broken user agent, `d`, `D`, `Backspace`, or `Delete` asks to remove its plist, and `Space` and `Enter` do nothing. Apple system services are hidden by default and read-only. See [Startup Items](#startup-items-taert-startup) for what counts as Apple's.
- **Explore Storage**: Multi-level interactive storage browser. Features breadcrumb navigation, live streaming progress during background measuring, item selection via `Space`, and deletion via `d`, `D`, `Backspace`, or `Delete` (offering "Move to Trash" and "Delete Permanently"; with `--permanent`, "Delete Permanently" is focused).
- **Large Files**: Flat list of the largest and oldest files and packages across your home folder (or any given path) to answer "what big things did I forget about?". Shows item name, enclosing top-level folder, size, and last used date (queried via Spotlight, fallback to modified date). `⇧S`, `⇧N`, and `⇧T` sort by size, name, or last used date. Filter interactively with `/`, reveal in Finder with `O`, and select with `Space`. Selected items move to the Trash upon confirmation.
- **System Status**: A live overview refreshed every second: CPU and GPU load, per-core activity, temperatures, memory breakdown and pressure, storage volumes and disk speed, battery charge and health, top processes, and network speed. Each panel sits in its own frame. On wide terminals (≥100 columns) the panels sit in two columns, and from 170 columns in three; on narrower ones they stack and scroll. CPU and memory face each other, as do network and storage, so their graphs line up, and taller windows give each graph more rows. The process lists keep a fixed size, and tall windows add a list of the top processes by memory. Reading all of it needs no administrator rights. `taert status` prints the same data once.

Every screen shows its sections in colored frames. While taert scans or measures, a framed card in the middle of the window shows the progress. In Explore Storage the card appears only when measuring takes longer than 150 ms. A folder you saw before opens at once with `Updating…` on the list frame when it changed since then or when you saw it more than 15 minutes ago. Its rows change when the new sizes are in. All keys work meanwhile.

### Keyboard Shortcuts

| Key | Context | Action |
|---|---|---|
| `↑` / `↓` or `k` / `j` | All screens | Move selection cursor |
| `Space` | Select / Explore / Startup | Toggle focused item selection / toggle startup service |
| `A` / `N` | Select / Explore | Select all visible / clear selection |
| `d` / `D` / `Backspace` / `Delete` | All screens that remove | Start the removal: the cleanup lists, Purge, Docker, Large Files, and Homebrew cleanup ask to remove the selection; Uninstall Apps opens the review; the review asks to uninstall; Homebrew asks to uninstall the focused formula; Explore Storage asks to delete; Startup Items asks to remove a broken item's plist; the path lists ask to remove the focused path |
| `u` / `⇧U` | Homebrew | Upgrade the focused package / every package with a new version |
| `⇧S` / `⇧N` / `⇧T` | Explore Storage / Large Files / Docker / Uninstall Apps / Homebrew | Sort by size / name / date |
| `↵` (Enter) | Slim / Compress | Open the question for the selection |
| `↵` (Enter) | Question dialogs | Press the highlighted button |
| `↵` / `→` | Explore Storage | Enter selected folder |
| `↵` / `→` / `←` | Homebrew | Open a row with dependencies / close it; `←` on a closed row moves to its parent |
| `⎋` (Esc) / `←` | Explore Storage | Go up one directory level; from the top folder, leave Explore Storage |
| `q` | Explore Storage | Leave Explore Storage from any folder |
| `R` | Explore Storage | Refresh and re-scan current folder |
| `O` | Explore Storage / Select | Reveal selected item in Finder (`open -R`) |
| `/` | Select / Search | Live search filter |
| `?` | All screens | Toggle keyboard shortcuts help sheet |
| `⎋` (Esc) / `q` | All screens | Cancel dialog / Go back to the previous screen, on the row you left / Quit from the main menu |

The bottom row lists the keys that work right now. In a narrow window the less important ones drop off, and `?` still lists every key of the screen. A letter marked `⇧` needs Shift, and a letter without it doesn't.

## Non-Interactive CLI Commands

Every action can run directly from the command line without launching the full TUI.

`taert schedule`, `taert ignore` and `taert config` have no preview and no JSON output. They stop with an error when given `--dry-run` or `--json`, and change nothing.

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

# Scan orphaned files left behind by apps that are gone
taert clean orphans

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

#### Orphaned App Files (`taert clean orphans`)

`taert clean orphans` inspects the top level (one level deep) of 11 user Library folders: `~/Library/Application Scripts`, `Application Support`, `Caches`, `Containers`, `Cookies`, `Group Containers`, `HTTPStorages`, `Logs`, `Preferences`, `Saved Application State`, and `WebKit`. Symlinks are skipped, and `~/Library/LaunchAgents` is left to Startup Items.

Only names that carry a reverse-DNS bundle id are found (free-form names such as `Foo Editor` are never listed). In `Containers`, the id is read from the container metadata plist (`MCMMetadataIdentifier`), in either of its two file names. In `Group Containers`, a leading `group.` or 10-character team id prefix is removed. An id is dropped if it starts with `com.apple.`, matches an installed app bundle id or embedded helper/extension id, or for shared group containers shares a vendor token with an installed app.

Before listing an orphan candidate, taert checks Spotlight with `mdfind` for the bundle id. If Spotlight finds the app on any connected drive (including the Trash), the candidate is dropped. If the Spotlight check fails, the candidate stays listed but starts unselected.

Apps on a drive that is not connected look removed. To protect them, rows with a file changed in the last 30 days anywhere inside start unselected. So does a row with a part taert cannot read.

| Condition | Starts Selected (`taert clean orphans`) |
|---|---|
| Exact id, nothing inside changed for 30 days, Spotlight confirmed missing | Yes |
| Exact id, a file changed < 30 days ago anywhere inside (or a part that cannot be read) | No |
| Exact id, Spotlight check failed | No |
| Shared id (`Group Containers`) | No |
| Any orphan row in `taert clean` (no category) or TUI All row | No |

In `taert clean` with no category and in the TUI's All row, every orphan row starts unselected, so `taert clean --force` with no category never removes one. Orphaned files are personal data (Application Support, Containers), so rows go to the Trash by default. Passing `--permanent` overrides this to permanent removal.

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

Removal runs in this order. Open apps get a quit request and up to 40 seconds to answer it, so they can ask you to save. A Homebrew cask goes next, through `brew uninstall --cask` of the Homebrew install that holds it, without `--zap` and with `HOMEBREW_NO_AUTOREMOVE=1`, so Homebrew removes no other formula. Then taert stops the app: launch agents are unloaded, the app leaves Launch Services, the login item that opens this copy leaves Login Items, its login helpers stop, and remaining processes get `SIGTERM`, then `SIGKILL`. If its login item cannot be removed (for example, macOS refuses Automation for System Events), the report says so and names where to remove it by hand. The app bundle goes to the Trash as you, even when the app has package receipts or root files. A bundle you cannot remove goes, with its root files, in a password step before taert stops the app, so cancelling keeps that app installed and running. Receipts and files only root can remove follow once the bundle is gone, and macOS asks for your password for them. If you refuse the prompt, the app is already gone (in the Trash, or removed by Homebrew or `--permanent`) and only those files stay, and the report says so. If the Trash call fails, the app and all its files stay, and the report tells you to allow your terminal in System Settings > Privacy & Security > App Management. An app's other files go only after the app itself, so a failed removal never leaves an app without its data. Last, its Dock tile goes.

The password is asked once per run, and a refused prompt is not repeated. A second prompt comes only when one app's bundle must go as root, which is asked before taert stops it, and another app's root files wait for its bundle to go to the Trash, which is asked after.

If taert runs in a terminal of the app being removed, the app stays open until you quit it, and the bundle goes anyway.

Another copy of the app, another channel of it such as Foo Beta, or an app whose id continues this one keeps everything shared: only the selected bundle goes. taert looks for those in the app folders, Setapp, Homebrew's Caskroom, and on mounted volumes, and again right before it removes anything.

An app in `/Applications` that is a link goes alone: the link is removed, and the app it points to stays, with its files. A Homebrew cask that holds several apps stays with Homebrew: only the selected app goes, and `brew list` keeps showing the cask. Security agents such as CrowdStrike, Jamf, and SentinelOne are left alone, because they need their vendor's uninstaller. Paths on the ignore list are left out too.

taert never edits your configuration. If `~/.zshrc`, `~/.config/fish/config.fish`, or a similar file still mentions a removed path, taert names the file and line.

If macOS refuses a removal, give your terminal **Full Disk Access** for files in `~/Library`, and **App Management** for app bundles, in System Settings > Privacy & Security.

### Homebrew Packages (`taert brew`)

```bash
# Interactive list in a terminal
taert brew

# Plain text, one row per package
taert brew --plain

# Structured JSON for automation
taert brew --json
```

taert lists every installed formula and cask with the space it takes: the allocated size of `<prefix>/Cellar/<name>` for a formula and `<prefix>/Caskroom/<token>` for a cask. A cask's app in `/Applications` is not counted. Formulae come first, then casks, each largest first.

The plain output has one row per package in two sections. The columns are the name, the version, the size, `on request` or `dependency`, the new version (`outdated` when Homebrew offers no version, `-` when the package is up to date), `pinned` or `-` (a cask counts as pinned when `brew outdated` says so), and the number of installed formulae that use the package:

```
Formulae
  go       1.27.3   250.1 MB  on request  -         -       2
  bun      1.4.2     60.0 MB  on request  1.4.4_1   -       0
  ada-url  4.0.0_1    3.1 MB  dependency  -         -       0

Casks (remove with taert uninstall)
  ghostty  1.3.1     50.0 MB  on request  -         -       0
```

`--json` prints an object with `packages` and `notes`. Each package has `kind` (`formula` or `cask`), `name`, `version`, `size_bytes`, `on_request`, `outdated`, `latest_version` (the new version, or `null` when the package is up to date), `pinned`, `used_by` (the names of the formulae that list it as a dependency), and `apps` (the apps of a cask).

If Homebrew is not installed, the list is empty and the note `Homebrew is not installed.` explains why. If Homebrew cannot list its packages, taert prints `Error: ...` and exits with a failure, never an empty list.

Listing makes one `brew info --json=v2 --installed` call and one `brew outdated --json=v2` call, and removes nothing. Casks that update themselves are not listed as outdated, because `brew outdated` leaves them out. If `brew outdated` fails, the list still shows, without new versions, and a note says `Homebrew did not list the outdated packages: ...`. `--plain` and `--json` only read. In the terminal list, `d` can remove a formula (described below). Casks are removed with `taert uninstall`.

The terminal list has three groups. `Formulae` holds the formulae you installed with `brew install`. `Not needed by anything you installed` holds the formulae that you did not install and that none of yours needs, directly or through other dependencies. The casks come last. The other formulae, the ones your formulae need, show only inside the tree, and a search finds them too. Within each group, rows start largest first.

The list is a table with the columns `NAME`, `ORIGIN` from 100 columns, `UPDATE`, `INSTALLED`, `SIZE`, and `SHARE` from 100 columns. `ORIGIN` reads `on request` or `dependency` for a formula and is empty for a cask. `UPDATE` always shows: `↑` and the new version, `↑` alone when Homebrew gives no version, or nothing when the package is up to date. It is at most 14 cells wide, and a longer version is cut in the middle with `…`. The `SHARE` bar is at most 12 cells wide, plus the percent. `INSTALLED` shows from 85 columns and reads like `3 days ago`, or `—` when Homebrew has no date for the package. The date is not in `--json`. `⇧S` sorts by size, largest first, `⇧N` by name, A to Z, and `⇧T` by install date, newest first, with undated packages last. The key sorts the top level of each group, the children of every open row, and the search results. The groups keep their order, the open rows stay open, and the focus stays on the same row. The order is not saved: the list starts by size each time. A removal keeps the order you chose.

A formula with installed dependencies has `▸` when closed and `▾` when open. Its children are its installed dependencies, in the order of the list (largest first at the start), one level deeper, with the same columns. A dependency that several formulae use shows under each of them, and a formula you installed that is also a dependency shows at the top level and under every formula that uses it. A formula never opens inside its own branch, so a dependency cycle ends there. Everything starts closed, and the tree is built from the same `brew info` reading, with no new call.

- `Enter` or `→` opens a closed row that has dependencies. `Enter` closes an open row. `Enter` on a row without dependencies does nothing.
- `←` closes an open row. On a closed row or one without dependencies, it moves the focus to the parent row. At the top level it does nothing.
- Opening or closing a row never moves the focus away from it.
- A search (`/`) lists every formula and cask that matches, flat, without the tree. The formulae that otherwise show only inside the tree, the ones your formulae need, sit under their own heading, `Needed by what you installed`. When you clear the search, the tree comes back with the rows open as before.

In a terminal, the list has no check marks, and `Space`, `A`, and `n` do nothing. `d`, `D`, `Backspace`, and `Delete` act on the focused row, also inside the tree:

- On a formula that nothing uses, it asks to remove that formula. The question says that Homebrew removes it. After you confirm, taert removes it as `taert brew uninstall` does, shows the result, and takes the formula off the list without reading Homebrew again, with the open rows kept open. A formula that only it used then shows without `used by` and can be removed, unless it is pinned. A search that found only the removed formula is cleared, so the whole list shows again. Opening the list from the menu, or running `taert brew`, reads Homebrew again.
- On a cask, a pinned formula, or a formula that other formulae use, it shows the reason at the bottom of the screen and changes nothing. Inside the tree, a dependency that another formula uses is refused the same way.

#### Remove a formula (`taert brew uninstall`)

```bash
# Ask first
taert brew uninstall fd

# Only show what would be removed
taert brew uninstall fd --dry-run

# Remove without asking
taert brew uninstall fd --force

# The plan as JSON, nothing is removed
taert brew uninstall fd --json
```

taert refuses, prints `Error: ...`, exits with a failure, and runs no removing call when:

- the name is a cask: `<name> is a cask. Remove it with taert uninstall.` Casks go through `taert uninstall`, which also quits the app and removes its files.
- the formula is pinned: `<name> is pinned. Run brew unpin <name> first.` A pin means you asked Homebrew to keep it as it is.
- other installed formulae use it: `<name> stays: <a>, <b> use it.` Removing it would break them.
- the name is not installed, or Homebrew is not installed.

`--force` does not skip a refusal. It only skips the question.

Without `--force`, taert prints `Remove <name> <version> (<size>) with brew uninstall? [y/N] ` and reads one line. `y` or `yes`, in any case, goes on. Anything else prints `Operation cancelled.` and removes nothing. The question is a plain prompt, also in a terminal; this command opens no screen. `--dry-run` prints `Would remove <name> <version> (<size>) with brew uninstall.` and removes nothing. `--json` prints the plan, with the `brew` task on the row, and removes nothing.

Removal goes through the same path as `taert brew cleanup`. Before it removes anything, taert reads the installed packages again. If another formula uses the formula by then, taert reports it and the command fails. `brew uninstall <name>` removes one version at a time without `--force`, so taert repeats it until no version folder is left, with `HOMEBREW_NO_AUTOREMOVE=1`, so Homebrew does not also remove other unused formulae. taert never passes `--force`, `--zap`, or `--ignore-dependencies`.

The history gets one `removed` item under the command `brew uninstall`, with the bytes freed, measured on the Cellar folder before and after.

#### Upgrade packages (`taert brew upgrade`)

```bash
# Upgrade every outdated package, after asking
taert brew upgrade

# Upgrade one formula or cask
taert brew upgrade bun

# Only show what would be upgraded
taert brew upgrade --dry-run

# Upgrade without asking
taert brew upgrade --force

# The plan as JSON, nothing is upgraded
taert brew upgrade --json
```

Before it reads anything, taert runs `brew update`, so the new versions are current. `--dry-run` and `--json` skip it. If `brew update` fails, taert prints `Could not check for updates: ` and the last line Homebrew printed on stderr, and goes on with what Homebrew knows. Then it makes one `brew info --json=v2 --installed` call and, when something is installed, one `brew outdated --json=v2` call.

With a name, taert upgrades that package. A formula wins when a formula and a cask have the same name. Without a name, it upgrades every package that has a new version, except the ones it refuses. taert refuses, prints `Error: ...`, exits with a failure, and runs no upgrade when you name a package and:

- Homebrew is not installed: `Homebrew is not installed.`
- it is not installed: `<name> is not installed.`
- it is up to date: `<name> is up to date.`
- it is pinned: `<name> is pinned. Run brew unpin <name> first.` A cask counts as pinned when `brew outdated` says so.

`--force` does not skip a refusal. It only skips the question. Without a name, taert leaves the refused packages out. If nothing is left, it prints `Everything is up to date.` and succeeds.

Without `--force`, taert lists each package as `bun 1.4.2 → 1.4.4_1`, then prints `Upgrade 1 package? [y/N] ` and reads one line. `y` or `yes`, in any case, goes on. Anything else prints `Operation cancelled.` and upgrades nothing. `--dry-run` prints `Would upgrade bun 1.4.2 → 1.4.4_1.` for each package. `--json` prints a list of the packages with `name`, `kind`, `version`, and `latest_version`.

Each package gets its own call: `brew upgrade --formula <name>` or `brew upgrade --cask <name>`, with `HOMEBREW_NO_AUTOREMOVE=1`, so Homebrew does not also remove unused formulae. taert prints `Upgraded bun to 1.4.4_1.` for each package that worked, and `Could not upgrade bun: ` and the last line Homebrew printed on stderr for each that failed. A failed cask adds `If it needs your password, run brew upgrade --cask <name> in a terminal.` The exit code is a failure when any upgrade failed. The other packages are still upgraded. Like `brew upgrade`, Homebrew removes the old version of what it upgraded; taert turns off only autoremove. taert does not write upgrades to the history.

#### Upgrade from the list

In the terminal list, `u` upgrades the focused package and `⇧U` upgrades every package that has a new version and is not refused. Both work in the tree and in search results, and `⇧U` ignores the search. While you type a search, `u` and `⇧U` are text. The footer shows `u  Upgrade` when the focused row has a new version and the window has room, and the help screen (`?`) lists `u` and `⇧U`.

taert refuses the same packages as `taert brew upgrade`. On a refused row, `u` shows the reason as a toast and runs nothing: `<name> is up to date.` or `<name> is pinned. Run brew unpin <name> first.` If `⇧U` finds nothing to upgrade, the toast reads `Everything is up to date.` While the list checks for updates, `u` and `⇧U` run nothing and the toast reads `Checking for updates. Try again when it ends.`, so `brew upgrade` never runs while `brew update` does. While the list is read again after an upgrade, they run nothing and the toast reads `Updating the list. Try again when it ends.`

The question is `Upgrade bun?` for one package and `Upgrade N packages?` for more. It lists `bun 1.4.2 → 1.4.4_1` for each package, as many as the dialog holds and then `and N more`, and says `Homebrew may also upgrade what these need or what needs them.` The button is `Upgrade`. Each package gets its own `brew upgrade --formula <name>` or `brew upgrade --cask <name>` call with `HOMEBREW_NO_AUTOREMOVE=1`, as in `taert brew upgrade`. The progress reads `Upgrading N packages…`. The result is `Upgrade Complete` with `Upgraded N packages.`, or `Nothing was upgraded` when no package worked. Failed packages are listed under `N items failed to upgrade:` with the last line Homebrew printed on stderr, and a failed cask adds the hint to run `brew upgrade --cask <name>` in a terminal. Upgrades are not written to the history.

When you close the result, `Updating the list…` shows in the status line while taert makes one `brew info --json=v2 --installed` call and one `brew outdated --json=v2` call. It measures again only the packages that are new or whose version changed, because Homebrew may have upgraded or added dependencies. Every other package keeps its size, and packages that are gone leave the list. The sort, the search, the open rows, and the focus stay, and a search you are typing goes on. taert does not run `brew update` again. If the read fails, a toast says `Could not update the list: ` and the reason, and the list keeps the rows it had. `u` and `⇧U` work again then, though the rows may be out of date. A read that ends while a question is open waits until the question closes. If you uninstall a formula before the read ends, taert reads again so the formula does not come back.

#### Cleanup (`taert brew cleanup`)

```bash
# Preview what Homebrew no longer needs
taert brew cleanup --dry-run

# Structured JSON for automation
taert brew cleanup --json

# Remove the rows that start selected, without asking
taert brew cleanup --force
```

The list has two groups, with one row per formula and the allocated size of its Cellar folders:

- **Unused dependencies**: the formulae `brew autoremove --dry-run` lists, which nothing installed needs any more. The row shows the name and the installed version.
- **Old versions**: the old versions in `<prefix>/Cellar` that `brew cleanup --dry-run` lists. The row shows the formula and its old versions, and the size is the sum of those folders as taert measures them, not the size brew prints. A formula that is also an unused dependency is listed only in the first group, because removing it takes every version with it. The list leaves out the download cache, which `taert clean dev` covers. `brew cleanup <name>` may still remove that formula's old downloads, and the freed bytes do not count them.

Both groups start selected, except pinned formulae. A pinned formula stays out of every default. On the command line, `--force` removes the selected rows. `--plain`, or a run outside a terminal, asks first, as `taert clean` does, and you can change the selection before you confirm. In a terminal, the list opens in the select screen. It starts with nothing selected, so you choose the rows. The rows stay in an order that is safe to remove: a formula is listed before the formulae it needs, so when taert reaches a dependency, the formula that used it is already gone.

Removal goes through Homebrew and never deletes a file in the Cellar itself, so Homebrew's records stay right. An unused dependency is removed with `brew uninstall <name>` and an old version with `brew cleanup <name>`. Both run with `HOMEBREW_NO_AUTOREMOVE=1`, because Homebrew would otherwise also remove every other unused formula after the one you picked. Before it uninstalls each dependency, taert reads the installed packages again. If another formula uses the dependency by then, it is pinned, or it is gone, taert skips it and reports why, and the rest goes on. This also skips a pinned formula you select by hand. If a version is still there when brew stops removing, taert reports which versions are still installed. taert never passes `--force`, `--zap`, or `--ignore-dependencies`.

Each package goes into the removal history under the command `brew cleanup`, with the bytes it freed, measured on its Cellar folder before and after. If Homebrew is not installed, the list is empty with the note `Homebrew is not installed.` If Homebrew fails to answer one of the three read commands, taert prints `Error: ...` and exits with a failure.

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
- **Skip Rules**: Automatically skips `~/Library`, `.Trash`, `.git`, `node_modules`, standard build artifacts, cloud dataless files, symlinks, and credential stores (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.gpg`, `~/.kube`, `~/.docker`, `~/.claude`, `~/.config`, `~/.cargo`, `~/.rustup`, `~/.gem`, `~/.nvm`, `~/.pyenv`, `~/.rbenv`, `~/.ollama`, `~/.lmstudio`, `~/Library/Keychains`), except cache folders that stay scannable (`~/.docker/cli-plugins/.cache`, `~/.docker/buildx/cache`, `~/.cargo/registry/cache`, `~/.cargo/registry/index`, `~/.cargo/git/db`, `~/.cargo/git/checkouts`, `~/.ollama/logs`, `~/.lmstudio/server-logs`).
- **Packages as Single Items**: macOS bundles and packages (`.app`, `.photoslibrary`, `.fcpbundle`, `.logicx`, `.imovielibrary`, `.band`, `.pvm`, `.vmwarevm`, `.utm`) are counted as single units with their total allocated size and are never walked into.

### Storage Directory Analysis (`taert analyze`)

```bash
# Analyze disk usage of a path
taert analyze ~/Downloads

# Plain text output for scripts or logs
taert analyze ~/Downloads --plain
```

### Diagnose Slowdowns & Battery Drain (`taert doctor`)

Answers "why is my Mac slow, hot, or draining its battery?" with named causes and evidence. `taert doctor` watches the Mac for a few seconds, lists what stands out, and suggests one action for each finding.

The tone stays factual. There is no health score, no red counters, and no claim to speed anything up. If nothing stands out, it says so and lists the checks that ran. A check that could not run shows as skipped with its reason in plain text, in JSON, and in the terminal screen.

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
- **Background energy & wakeups**: Catches background processes with more than 500 wakeups per second while CPU usage stays low. The rate comes from the change in the `IDLEW` counter of `top` between samples, so the check needs at least 4 seconds. A shorter check skips it.
- **Memory & Swap**: Checks memory pressure and active swap growth, listing the largest memory-consuming processes.
- **Low disk space**: Warns when APFS available space drops below 10% (critical below 5%) and suggests Explore Storage (`taert analyze`).
- **Sleep blockers**: Detects power assertions (`PreventUserIdleSystemSleep`, `PreventUserIdleDisplaySleep`, `PreventSystemSleep`) held for over 10 minutes. Ignores display sleep assertions from `powerd` and `UserIsActive`.
- **Wake reasons**: Gathers wake events from the last 24 hours, grouping by reason (e.g. `wifibt`) and reporting frequent wakeups or wake requests.
- **Thermal throttling**: Detects thermal or performance warnings recorded by the system.
- **Network traffic**: Highlights background processes transferring data above 1 MB/s over the observation window.
- **Broken startup items**: Flags launch agents or daemons whose target executable files no longer exist. It suggests `taert startup disable` only for an item taert can switch. For any other item it says why taert leaves it alone.

Evidence and the suggested action show at every terminal width. In plain output, a finding that stops a process or frees disk space also prints a command: `kill <pid>` to stop a process, or `taert analyze` for disk space. In the terminal screen, stopping a process sends `SIGTERM` after you confirm. The dialog starts on Cancel, and a failed signal shows its error and keeps the finding.

### System Status (`taert status`)

```bash
# Interactive TUI screen
taert status

# Non-interactive plain text output
taert status --plain

# Machine-readable JSON output
taert status --json
```

Plain text output:

```text
Health:   All clear
CPU:      12.4%
Memory:   18.2 GB of 32.0 GB used, pressure Normal
Disk:     118.6 GB of 994.7 GB available
Battery:  87%, charging
Top processes:
  Safari  12.4%  1.2 GB
  node  8.0%  600 MB
  Finder  1.1%  120 MB
```

Shortened `--json` output:

```json
{
  "schema_version": 1,
  "collected_at": "2026-11-01T13:40:00Z",
  "health": {
    "severity": "good",
    "message": "All clear"
  },
  "machine": {
    "chip": "Apple M4 Pro",
    "macos_version": "27.0.1"
  },
  "memory": {
    "total_bytes": 32000000000,
    "app_bytes": 18200000000,
    "pressure": "Normal"
  },
  "storage": {
    "mount_point": "/",
    "total_bytes": 994700000000,
    "available_bytes": 118600000000
  },
  "live": {
    "cpu": {
      "usage_percent": 12.4
    },
    "power": {
      "charge_percent": 87,
      "source": "Charging"
    },
    "processes": []
  }
}
```

A value is `null` when it could not be read, or when the Mac has no such part, such as a battery on desktop Macs.

When a reading fails, notes are printed to stderr, never to stdout.

Exit code is 0 on success. The command exits with code 1 if writing fails or if the report is unusable (when memory, storage, and CPU load are all missing).

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

`disable` and `enable` take the label of the item. A login item can also be named by its name, or by its path, such as `taert startup disable /Applications/Example.app`. A name that fits several items is refused, and the error lists them, with the path of each login item. When disabling a launchd service, `taert` disables it before running `bootout` so launchd will not automatically restart it. Afterwards `taert` reads the state again and reports it, and a step that did not take effect is an error. In the interactive list, an item that is switched off but still running offers "stop". With `--all`, entries from the Background Task Management records that taert cannot switch show as read-only `background item` rows. When disabling a classic login item, `taert` removes it from System Events and saves its restore information in `~/.config/taert/disabled_login_items.json`. If taert cannot read that file, it does not write over it, and switching a login item off fails with an error that names the file. Apple system services and security agents (CrowdStrike, SentinelOne, Jamf) are strictly read-only and refused. A service counts as Apple's when its label starts with `com.apple.`. A service with no plist in `~/Library/LaunchAgents`, `/Library/LaunchAgents`, or `/Library/LaunchDaemons` is also Apple's when launchd shows its plist or program in a system folder such as `/System` or `/usr/libexec`, as for `com.openssh.ssh-agent`, or when an Apple bundle registered it. So is such a daemon when launchd shows none of its files, unless an app outside Apple registered it. Any other service with no plist in those folders is read-only too: an app registered it, or its plist lives elsewhere, such as in `~/Library/Application Support`. launchd forgets such a service once it stops, so taert could not switch it back on. Switch it in System Settings > General > Login Items & Extensions, or in the app. Switching a daemon with a plist in `/Library/LaunchDaemons` asks for the administrator password. A daemon whose plist sits in a `LaunchAgents` folder is read-only, so root never loads a file from the home folder. A plist in those folders counts only by its label, so a user agent that runs `/usr/bin/osascript` stays switchable.

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

# Slim an app and print the result as JSON (--json needs --force)
taert slim Slack --json --force

# Slim without interactive confirmation prompt
taert slim Slack --force
```

#### How Slimming Works

`taert slim` asks `Slim X? [y/N]: ` before it rewrites an app, in a terminal and in plain output. Anything but `y` cancels. `--force` skips the question. `--json` without `--force` does not ask: it prints `{"error": "confirmation needed, pass --force", "app": "X"}` and fails.

1. **Safety Check**: Checks that your Mac runs Apple Silicon (`sysctl hw.optional.arm64`). If run on an Intel Mac, `taert slim` stops immediately with a clear message: `"Slimming universal apps requires an Apple Silicon Mac."` Slim apps in the interface shows the same message on its own screen.
2. **APFS Clone**: Makes a holding folder `<name>.app.taert-slim-tmp` next to the app and clones the app into it, as `<holding>/<name>.app`, using `ditto --clone` on APFS without duplicating disk blocks. If the holding folder already exists, taert stops and names it: it is left from an earlier run and can hold the only copy of an app. taert never removes or moves it. Check it, move it to the Trash yourself, then run again. The slim list, `--json`, and `taert slim <name>` leave out an app inside a holding folder.
3. **Binary Thinning**: Scans the Mach-O fat headers inside the cloned bundle and strips x86_64 slices, rewriting the fat headers to retain only ARM slices with standard power-of-two alignment. Files sealed by whole-file hashes in `_CodeSignature/CodeResources` are left alone to ensure code signing integrity. A file inside a nested bundle (a framework, a helper app, a plug-in) is checked against the seal of the nearest nested bundle that has one. If that seal cannot be read, the file is left alone.
4. **ARM Verification**: Inspects Mach-O architecture headers to verify that a valid ARM slice (`CPU_TYPE_ARM64` / `CPU_TYPE_ARM64_32`) is present and preserved.
5. **Signature & Gatekeeper Verification**: Validates the slimmed bundle using `codesign --verify --deep --strict` and `spctl -a -t exec`. Only a failed `codesign` check makes taert remove the extended attributes (`com.apple.FinderInfo` or `com.apple.ResourceFork`) and check both again once. A failed `spctl` check after a passing `codesign` check stops the run.
6. **Original Bundle Verification**: Compares a fingerprint of the whole original bundle (every path, type, size, modification time, inode, and device) with the one taken before step 2, and lists running programs with `ps`. A change anywhere inside the bundle, a program started from it, or a failed `ps` stops the run.
7. **Atomic Swap & Trash**: Atomically exchanges the slimmed copy and the original bundle using APFS `renamex_np(RENAME_SWAP)`, then moves the old app, now at `<holding>/<name>.app`, to the Trash (`Host::trash`), so the Trash item is called `<name>.app`, and removes the empty holding folder. If any step fails before the swap, the holding folder is removed and the original application is left completely untouched. After the swap taert removes nothing.

For each slimmed app taert prints one line: `Slimmed X, saved N. The old app was moved to the Trash.` If the Trash refuses the old app, taert still reports the app as slimmed and says where the old app is: `Slimmed X, saved N. The old app is still at <path> (could not move it to the Trash: <error>)`. `--json` adds `"old_app_path"` in this case only. If taert cannot remove the empty holding folder after the Trash step, the line names it, because the next run stops at that folder: `Slimmed X, saved N. The old app was moved to the Trash, but the folder <holding> is left (could not remove it: <error>). Move it to the Trash yourself before you slim X again.` The interface shows this line only when the old app or the holding folder stays. The note that the space comes back when the Trash is emptied shows only when at least one old app went to the Trash. The result of the terminal list shows `Saved N of Intel code.`, the total for the slimmed apps, under the title `Slim Complete`, or the title `Nothing was slimmed` when no app was slimmed. Failed apps are listed under `N items failed to slim:`, each with the reason under its path. It does not count the old apps as moved to the Trash or removed.

The saved size is the allocated size of each rewritten binary before, minus the allocated size after. The estimate in the list and in `--dry-run` is the sum of the Intel slice sizes.

#### Eligibility & Exclusion Rules

`taert slim` verifies that each application is safe to slim, in the list, in `--json`, and for a named app:
- **System Applications**: Apple bundles (`com.apple.*`) and system roots are strictly refused.
- **Mac App Store Apps**: Apps with an App Store receipt (`_MASReceipt`) are skipped because macOS App Store verification requires the original unmodified bundle.
- **Running Applications**: Running applications are detected via `/bin/ps` and skipped to prevent file locks and memory corruptions. Letter case does not matter, and a program under the app's real path counts when the app's path goes through a link. If `ps` fails, every app is refused with "Could not list running programs".
- **Rosetta Preference**: Apps configured to run under Rosetta (`LSArchitecturesForX86_64` in `com.apple.LaunchServices.plist`) are excluded so Rosetta compatibility is not broken.
- **Links**: A bundle path that is a symbolic link is refused ("The app is a link. Slim the app it points to"), because the clone would copy the app it points to. Slim that app by its own path.
- **Ownership**: The bundle and every file and folder in it must belong to you. A bundle owned by root is refused ("Belongs to another user"), because the copy would make a root-owned helper yours. A bundle with a setuid or setgid file is refused ("Contains a setuid or setgid file"). If permissions fail, `taert` reports a hint to grant App Management in System Settings.
- **Developer ID Signing**: Only apps signed with a valid Developer ID certification authority are eligible.
- **Signature and Gatekeeper**: An app that already fails `codesign --verify --deep --strict` ("Already fails the signature check") or `spctl` ("Already fails the Gatekeeper check") stays as it is. Apps without Intel code are not checked.

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
- **Result in the TUI**: After a run the result reads `Compress Complete` and `Saved X by compressing N files.`, or `Nothing was compressed` when no file was compressed. It shows no Trash or removed totals, because a compress deletes no file. Failed items are listed under `N items failed to compress:`, each with the reason under its path.
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

- **Format & Location**: Stored as line-delimited JSON at `~/.config/taert/history.jsonl` with permissions `0600`. Because `~/.config` is protected by the safety engine, no cleanup can ever touch or delete this file. The history file's folder is locked with an exclusive `flock` while a line is written or while the file is pruned, so concurrent runs keep their lines.
- **Compress and slim**: Successful compress and slim operations appear in the log with their saved bytes and outcome `changed`.
- **Interrupted runs**: Events are logged as they happen. An interrupted cleanup or process kill leaves a readable trail showing what completed before the interruption.
- **Bounded size**: Keeps the last 1,000 runs automatically.
- **Overrides**: `--dry-run` does not open the interactive screen; add `--plain` to see the preview. `--dry-run` writes nothing to the log. Set `TAERT_NO_HISTORY=1` to disable recording completely.

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
- **`--categories`**: Comma-separated list of any of `dev`, `junk`, `ai`, and `trash` (default `dev,junk`). Names are stored in lower case and each is kept once, so `DEV,dev` becomes `dev`.
  - `purge`, `docker`, `uninstall`, and `clean orphans` cannot be scheduled; their results require interactive review.
- **`--min-reclaim`**: Minimum reclaimable space threshold (default `1G`). If scanning finds less than this amount, the run is skipped and the skip is recorded.

If `launchctl` cannot load the agent, `taert schedule set` prints the error, exits with failure, and saves nothing. The old plist and settings stay, but the old agent stays unloaded until `taert schedule set` succeeds.

#### Launch Agent & Full Disk Access

Scheduled cleanup installs a user launch agent into `~/Library/LaunchAgents/io.github.ingvarch.taert.schedule.plist`:
- Missed calendar intervals run automatically when your Mac wakes.
- `taert schedule run` executes silently in the background, acquiring a lock file at `~/.config/taert/schedule.lock` to prevent overlapping runs.
- Because `launchd` launches `taert` directly in the background, permissions granted to your terminal app do not apply. Any rows that fail with `EPERM` are logged as failures, and `taert schedule show` will warn and provide instructions on how to grant Full Disk Access to the stable binary (e.g. `/opt/homebrew/bin/taert`).

#### Homebrew

A `brew upgrade` does not touch the launch agent. Run `taert schedule off` before `brew uninstall taert`, or use `brew uninstall --zap taert`, which also removes the agent. If the agent file is missing, `taert schedule show` prints the `taert schedule set` command that loads it again.

#### Interactive Preferences

You can also switch on and adjust scheduled cleanup in the TUI workspace under **Preferences** → **Scheduled cleanup**. Use `←` and `→` to cycle between `Off`, `Every day`, `Every week`, and `Every month`, then press `Enter` to apply the choice. If the agent cannot be loaded, the error shows and the row goes back to its old value.

### Shell Completion (`taert completion`)

Print completion scripts for your shell. `brew install` sets completions up by itself. Casks up to 0.7.0 installed none. With those, use the commands below. In fish, commands and flags complete, but the category names of `taert clean` do not.

fish:

```fish
mkdir -p ~/.config/fish/completions
taert completion fish > ~/.config/fish/completions/taert.fish
```

zsh:

```bash
taert completion zsh > "${fpath[1]}/_taert"
```

bash:

```bash
taert completion bash > "$(brew --prefix)/etc/bash_completion.d/taert"
```

### Trash & Permanent Deletion

By default, taert moves personal files and user apps to macOS Trash instead of deleting them immediately:
- **Explore Storage (`taert analyze`)**: Dialog offers **Move to Trash** (default) and **Delete Permanently**.
- **Large Files (`taert large`)**: Selected files and packages move to Trash (default).
- **Uninstall Apps (`taert uninstall`)**: User-level app bundles, including those with package receipts, and leftovers in home folders move to Trash. Files requiring root (`/Library/...`) and package receipts are deleted permanently because root has no Trash. A bundle you cannot remove, or that has a locked or unwritable entry inside, is also deleted as root, its row says "(Permanent)", and the confirm dialog says macOS will ask for your password. A cask bundle is removed by Homebrew, and its row says "(Permanent)". With `--permanent`, nothing goes to the Trash.
- **Orphaned App Files (`taert clean orphans`)**: Orphan rows move to the Trash because they hold personal data in Application Support and Containers.
- **Caches, Build Artifacts & Docker**: Caches (`taert clean`), project artifacts (`taert purge`), Docker items (`taert docker`), Homebrew cleanup and uninstall (`taert brew cleanup`, `taert brew uninstall`, and `d` on the Homebrew list, removed by Homebrew), and emptying the Trash are always permanently removed.
- **`--permanent` flag**: Global CLI flag (`taert --permanent ...`) that skips Trash and deletes files and apps permanently.

For configuration and preferences, see [configuration.md](configuration.md).
