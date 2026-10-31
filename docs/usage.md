# Usage

Commands, keybindings, and automation options. For developer setup and checks, see [development.md](development.md).

## Interactive TUI Mode

Launch the interactive workspace by running:

```bash
taert
```

### Workspace Navigation

- **Main Menu**: Fast access to Clean Caches, Project Cleanup, Explore Storage, System Status, and Preferences.
- **Adaptive Layout**: On wide terminals (≥100 columns), the workspace splits into an action list on the left and live system status and storage breakdown on the right. The main menu, System Status, Explore Storage, and item selection fill the window. Preferences, Review, and Scanning keep a readable width at the left edge. From 100 columns, lists in Explore Storage and item selection show a size bar that grows with the window.
- **Category Cleanup**: Inspect and clean Developer Caches, System Junk, AI Tool Caches (Ollama, LM Studio, Cursor), or Empty Trash.
- **Project Cleanup (Purge)**: Discovers disposable build directories (`target/`, `node_modules/`, `.venv/`, `.build/`, `dist/`) across configured project roots with configurable age filtering.
- **Explore Storage**: Multi-level interactive storage browser. Features breadcrumb navigation, live streaming progress during background measuring, item selection via `Space`, and safe deletion via `D` or `Backspace`.
- **System Status**: A live overview refreshed every second: CPU and GPU load, per-core activity, temperatures, memory breakdown and pressure, storage volumes and disk speed, battery charge and health, top processes, and network speed. On wide terminals (≥100 columns) the panels sit in two columns, and from 170 columns in three; on narrower ones they stack and scroll. Taller windows give each graph more rows, draw a bar for every core, and add a list of the top processes by memory. Reading all of it needs no administrator rights.

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
| `←` | Explore Storage | Go up one directory level |
| `R` | Explore Storage | Refresh and re-scan current folder |
| `O` | Explore Storage | Reveal selected item in Finder (`open -R`) |
| `/` | Select / Search | Live search filter |
| `?` | All screens | Toggle keyboard shortcuts help sheet |
| `⎋` (Esc) / `q` | All screens | Cancel dialog / Go back / Quit |

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
