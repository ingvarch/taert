# Usage

Commands, keybindings, and automation options. For developer setup and checks, see [development.md](development.md).

## Interactive TUI Mode

Launch the interactive workspace by running:

```bash
taert
```

### Workspace Navigation

- **Main Menu**: Fast access to Clean Caches, Project Cleanup, Docker Cleanup, Explore Storage, System Status, and Preferences. Keys `1` to `7` open a row directly.
- **Adaptive Layout**: On wide terminals (≥100 columns), the workspace splits into an action list on the left and details of the highlighted item on the right: disk usage for the cleanup items, disk usage and the largest items of your home folder for Explore Storage, and live CPU and memory for System Status. The main menu, System Status, Explore Storage, and item selection fill the window. Preferences, Review, and Scanning keep a readable width at the left edge. From 100 columns, lists in Explore Storage and item selection show a size bar that grows with the window. From 155 columns, Explore Storage shows what the highlighted folder holds in a panel beside the list.
- **Category Cleanup**: Inspect and clean Developer Caches, System Junk, AI Tool Caches (Ollama, LM Studio, Cursor), or Empty Trash.
- **Project Cleanup (Purge)**: Discovers disposable build directories (`target/`, `node_modules/`, `.venv/`, `.build/`, `dist/`) across configured project roots with configurable age filtering.
- **Explore Storage**: Multi-level interactive storage browser. Features breadcrumb navigation, live streaming progress during background measuring, item selection via `Space`, and safe deletion via `D` or `Backspace`.
- **System Status**: A live overview refreshed every second: CPU and GPU load, per-core activity, temperatures, memory breakdown and pressure, storage volumes and disk speed, battery charge and health, top processes, and network speed. Each panel sits in its own frame. On wide terminals (≥100 columns) the panels sit in two columns, and from 170 columns in three; on narrower ones they stack and scroll. CPU and memory face each other, as do network and storage, so their graphs line up, and taller windows give each graph more rows. The process lists keep a fixed size, and tall windows add a list of the top processes by memory. Reading all of it needs no administrator rights.

- **Docker Cleanup**: Lists images, volumes, and networks that no container refers to, stopped containers included, and the build cache. Each kind sits in its own framed, colored table with name, age, share, and size, and the age reads like `docker images`. The age column appears from 85 columns. `⇧S`, `⇧N`, and `⇧D` sort each table by size, name, or date. Only untagged images and unused networks start selected. Tagged images may be the only copy, the build cache takes time to rebuild, and volumes can hold real data, so you choose those yourself. The confirmation names any selected volumes, and a volume's data can't be recovered. taert talks to the same daemon as the `docker` command, and Docker refuses to remove anything still in use.

Every screen shows its sections in colored frames. While taert scans or measures, a framed card in the middle of the window shows the progress.

### Keyboard Shortcuts

| Key | Context | Action |
|---|---|---|
| `↑` / `↓` or `k` / `j` | All screens | Move selection cursor |
| `Space` | Select / Explore | Toggle focused item selection |
| `A` / `N` | Select / Explore | Select all visible / clear selection |
| `D` / `Backspace` / `Delete` | Explore Storage | Delete selected item(s) with confirmation dialog |
| `⇧S` / `⇧N` / `⇧D` | Explore Storage | Sort by size / name / date created |
| `↵` (Enter) | Select / Review | Review selection / Confirm cleanup |
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

# Scan system junk, crash reports, and user logs
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
