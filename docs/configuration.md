# Configuration

Settings, persistent ignore lists, and environment overrides. For command usage, see [usage.md](usage.md).

## Configuration Files

taert keeps its files in these places:

- **Settings**: `~/Library/Application Support/taert/config.toml`
- **Protected Paths (Ignore List)**: `~/Library/Application Support/taert/ignore.toml`
- **Project Folder Roots**: `~/Library/Application Support/taert/projects`
- **History Log**: `~/.config/taert/history.jsonl` (mode `0600`)
- **Switched-off Login Items**: `~/.config/taert/disabled_login_items.json`
- **LaunchAgent Plist**: `~/Library/LaunchAgents/io.github.ingvarch.taert.schedule.plist`
- **Schedule Lock**: `~/.config/taert/schedule.lock`

## Settings (`config.toml`)

The file is TOML. A `config.json` file is ignored.

| Setting | Type | Default | Description |
|---|---|---|---|
| `older_than_days` | integer | `7` | Project cleanup selects build artifacts at least this many days old |
| `extra_artifacts` | array | `[]` | Folder names that Project cleanup also lists. A folder matches only when its name is exactly one of them; they are not patterns |
| `appearance` | string | `"auto"` | Terminal theme mode: `"auto"`, `"dark"`, or `"light"` |
| `schedule` | object | `null` | Automatic background cleanup configuration: `every`, `categories`, and `min_reclaim` |

The folders that Project cleanup scans are not in `config.toml`. They are in the `projects` file, one folder per line. When the file is missing or empty, taert scans `~/Projects`, `~/Code`, `~/dev`, `~/GitHub`, `~/Workspace`, and `~/Repos`.

Modify settings via the CLI:

```bash
# View all settings
taert config

# Change appearance mode
taert config --appearance dark
```

`taert config` changes only the appearance. To change the age threshold for one run, pass `--older-than` to `taert purge`.

Or adjust them interactively via the **Preferences** screen in the TUI workspace.

## Protected Paths (Ignore List)

The ignore list prevents specified directories or files from ever being scanned, displayed, or deleted:

```bash
# Protect an important project directory
taert ignore add ~/Projects/mission-critical

# List all protected paths
taert ignore list

# Remove protection
taert ignore remove ~/Projects/mission-critical
```

The ignore list can also be edited interactively inside the TUI workspace under **Preferences** → **Protected Paths**.

## Scheduled Cleanup

Scheduled cleanup runs unattended in the background via a native macOS LaunchAgent (`~/Library/LaunchAgents/io.github.ingvarch.taert.schedule.plist`).

### Full Disk Access

Because `launchd` launches `taert` directly in the background, permissions granted to your terminal application (such as Terminal or iTerm2) do not apply to the launch agent. If rows that require elevated read permissions fail with `EPERM` (Operation not permitted):

1. Open **System Settings** → **Privacy & Security** → **Full Disk Access**.
2. Click `+` and add the stable `taert` binary path (typically `/opt/homebrew/bin/taert` on Apple Silicon or `/usr/local/bin/taert` on Intel).

`taert schedule show` will explain this and show the exact binary path if permissions are missing.

### Uninstalling

Before uninstalling `taert`, turn off the background agent:

```bash
taert schedule off
```

Run it before `brew uninstall taert`, or use `brew uninstall --zap taert`, which also removes the agent. A plain `brew uninstall taert` leaves the agent loaded. A `brew upgrade` leaves the agent alone.

## Environment Overrides

The following environment variables take precedence over configuration files:

| Variable | Values | Description |
|---|---|---|
| `TAERT_APPEARANCE` | `dark`, `light` | Overrides detected terminal appearance for the current session |
| `TAERT_ASCII` | `1`, `true` | Replaces Unicode glyphs with clean ASCII characters |
| `TAERT_REDUCE_MOTION` | `1`, `true` | Disables progress spinners and animations for accessibility |
| `TAERT_NO_HISTORY` | `1` | Turns off writing the local removal history log |
| `NO_COLOR` | any non-empty | Disables ANSI color escapes and uses high-contrast text styles |

For project architecture and safety guarantees, see [architecture.md](architecture.md).
