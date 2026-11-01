# Usage

Commands, keybindings, and automation options. For developer setup and checks, see [development.md](development.md).

## Interactive TUI Mode

Launch the interactive workspace by running:

```bash
taert
```

### Workspace Navigation

- **Main Menu**: Fast access to Clean Caches, Project Cleanup, Docker Cleanup, Uninstall Apps, Explore Storage, System Status, and Preferences. Keys `1` to `8` open a row directly.
- **Adaptive Layout**: On wide terminals (≥100 columns), the workspace splits into an action list on the left and details of the highlighted item on the right: disk usage for the cleanup items, disk usage and the largest items of your home folder for Explore Storage, and live CPU and memory for System Status. The main menu, System Status, Explore Storage, and item selection fill the window. Preferences, Review, and Scanning keep a readable width at the left edge. From 100 columns, lists in Explore Storage and item selection show a size bar that grows with the window. From 155 columns, Explore Storage shows what the highlighted folder holds in a panel beside the list.
- **Category Cleanup**: Inspect and clean Developer Caches, System Junk, AI Tool Caches (Ollama, LM Studio, Cursor), or Empty Trash. System Junk also lists every other folder in `~/Library/Caches` as App caches, and the caches of Safari, Chrome, Edge, Brave, Firefox, and Arc as Browser caches. Profiles stay untouched, so cookies, history, passwords, and extensions are safe. These rules set what starts selected for `--force`, `--plain`, `--dry-run`, and `--json`. The Clean Caches screen starts with nothing selected, as it does for every other group, so you choose the rows. An app cache starts selected when nothing in it changed for 30 days and its app is not running. taert finds the app by bundle id, so it knows an app is open only when the cache folder is named after that id. A browser cache starts selected when the browser is not running. taert finds a browser by bundle id, so a renamed one counts, and any Firefox channel keeps the cache they share. If taert cannot list the running programs, no cache starts selected and a note says so. Folders that other rows already cover, `com.apple.*`, `CloudKit`, and `FamilyCircle` are left out. Each browser is one row. Safari and Arc keep two folders, so their row shows one path and `+1 folder`. `--plain` and `--dry-run` print both folders, and `--json` puts the second under `also`. Safari keeps its cache in `~/Library/Caches/com.apple.Safari` and in a container. macOS may refuse both reads unless your terminal has Full Disk Access. Without it, taert skips those folders and shows a note on the list, in the plain output, and after a `--force` run.
- **Project Cleanup (Purge)**: Discovers disposable build directories (`target/`, `node_modules/`, `.venv/`, `.build/`, `dist/`) across configured project roots with configurable age filtering.
- **Explore Storage**: Multi-level interactive storage browser. Features breadcrumb navigation, live streaming progress during background measuring, item selection via `Space`, and safe deletion via `D` or `Backspace`.
- **System Status**: A live overview refreshed every second: CPU and GPU load, per-core activity, temperatures, memory breakdown and pressure, storage volumes and disk speed, battery charge and health, top processes, and network speed. Each panel sits in its own frame. On wide terminals (≥100 columns) the panels sit in two columns, and from 170 columns in three; on narrower ones they stack and scroll. CPU and memory face each other, as do network and storage, so their graphs line up, and taller windows give each graph more rows. The process lists keep a fixed size, and tall windows add a list of the top processes by memory. Reading all of it needs no administrator rights.

- **Docker Cleanup**: Lists images, volumes, and networks that no container refers to, stopped containers included, and the build cache. Each kind sits in its own framed, colored table with name, age, share, and size, and the age reads like `docker images`. The age column appears from 85 columns. `⇧S`, `⇧N`, and `⇧D` sort each table by size, name, or date. Only untagged images and unused networks start selected. Tagged images may be the only copy, the build cache takes time to rebuild, and volumes can hold real data, so you choose those yourself. The confirmation names any selected volumes, and a volume's data can't be recovered. taert talks to the same daemon as the `docker` command, and Docker refuses to remove anything still in use.

- **Uninstall Apps**: Lists the apps in `/Applications`, `~/Applications`, the input method folders, and `/Volumes/*/Applications` with their size and last use. `⇧S`, `⇧N`, and `⇧D` sort the list. Pick apps with `Space`, or press `Enter` on a single app. taert then lists the bundle and everything it found for that app, so you can change the selection before anything is removed. `Enter` asks for confirmation. Apps that are open are quit first. Deletion is permanent. See [Uninstall Apps](#uninstall-apps-taert-uninstall) for what is found and what starts selected.

Every screen shows its sections in colored frames. While taert scans or measures, a framed card in the middle of the window shows the progress.

### Keyboard Shortcuts

| Key | Context | Action |
|---|---|---|
| `↑` / `↓` or `k` / `j` | All screens | Move selection cursor |
| `Space` | Select / Explore | Toggle focused item selection |
| `A` / `N` | Select / Explore | Select all visible / clear selection |
| `D` / `Backspace` / `Delete` | Explore Storage | Delete selected item(s) with confirmation dialog |
| `⇧S` / `⇧N` / `⇧D` | Explore Storage / Docker / Uninstall Apps | Sort by size / name / date |
| `↵` (Enter) | Select / Review | Review selection / Confirm cleanup / Review the focused app in Uninstall Apps |
| `↵` / `→` | Explore Storage | Enter selected folder |
| `⎋` (Esc) / `←` | Explore Storage | Go up one directory level; from the top folder, leave Explore Storage |
| `q` | Explore Storage | Leave Explore Storage from any folder |
| `R` | Explore Storage | Refresh and re-scan current folder |
| `O` | Explore Storage | Reveal selected item in Finder (`open -R`) |
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

Removal runs in this order. Open apps get a quit request and up to 40 seconds to answer it, so they can ask you to save. A Homebrew cask goes next, through `brew uninstall --cask`, without `--zap`. Then, if anything needs root, macOS asks for your password. An app with files only root can remove goes in that same step, so a cancelled prompt leaves it installed, with its login item and background jobs in place. After that taert stops what is left: launch agents are unloaded, the app leaves Launch Services and Login Items, its login helpers stop, and remaining processes get `SIGTERM`, then `SIGKILL`. An app's other files go only after the app itself, so a failed removal never leaves an app without its data. Last, its Dock tile goes.

The password is asked once per run, and a refused prompt is not repeated. There are two exceptions: if macOS refuses a removal only once it has started, taert asks again for that app, which is stopped by then, and two selected copies of one app that share root files need a second prompt, because those files go only after both copies. An app outside the fixed system folders, such as one in `~/Applications`, goes first, and its files in `/Library` follow after the password. If you cancel, those files stay and taert lists them.

If taert runs in a terminal of the app being removed, the app stays open until you quit it, and the bundle goes anyway.

Another copy of the app, another channel of it such as Foo Beta, or an app whose id continues this one keeps everything shared: only the selected bundle goes. taert looks for those in the app folders, Setapp, Homebrew's Caskroom, and on mounted volumes, and again right before it removes anything.

An app in `/Applications` that is a link goes alone: the link is removed, and the app it points to stays, with its files. A Homebrew cask that holds several apps stays with Homebrew: only the selected app goes, and `brew list` keeps showing the cask. Security agents such as CrowdStrike, Jamf, and SentinelOne are left alone, because they need their vendor's uninstaller. Paths on the ignore list are left out too.

taert never edits your configuration. If `~/.zshrc`, `~/.config/fish/config.fish`, or a similar file still mentions a removed path, taert names the file and line.

If macOS refuses a removal, give your terminal **Full Disk Access** for files in `~/Library`, and **App Management** for app bundles, in System Settings > Privacy & Security.

### Storage Directory Analysis (`taert analyze`)

```bash
# Analyze disk usage of a path
taert analyze ~/Downloads

# Plain text output for scripts or logs
taert analyze ~/Downloads --plain
```

### System Health & RAM Pressure (`taert optimize`)

```bash
# View factual RAM pressure and APFS disk status
taert optimize

# Machine-readable JSON output
taert optimize --json
```

For configuration and preferences, see [configuration.md](configuration.md).
