# Configuration

Settings, persistent ignore lists, and environment overrides. For command usage, see [usage.md](usage.md).

## Configuration Files

taert stores user settings in standard macOS configuration directories:

- **Settings**: `~/.config/taert/config.json`
- **Protected Paths (Ignore List)**: `~/.config/taert/ignore.json`
- **Project Folder Roots**: `~/.config/taert/project_folders.json`

## Settings (`config.json`)

| Setting | Type | Default | Description |
|---|---|---|---|
| `older_than_days` | integer | `30` | Minimum age in days for project build artifacts to be targeted by default |
| `project_folders` | array | `["~/Projects", "~/Development", "~/Repos", "~/Code"]` | Root folders scanned for project repositories |
| `extra_artifacts` | array | `[]` | Custom directory or file patterns treated as disposable build artifacts |
| `appearance` | string | `"auto"` | Terminal theme mode: `"auto"`, `"dark"`, or `"light"` |

Modify settings via the CLI:

```bash
# View all settings
taert config

# Change appearance mode
taert config --appearance dark

# Set project artifact age threshold to 14 days
taert config --older-than 14
```

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

## Environment Overrides

The following environment variables take precedence over configuration files:

| Variable | Values | Description |
|---|---|---|
| `TAERT_APPEARANCE` | `dark`, `light` | Overrides detected terminal appearance for the current session |
| `TAERT_ASCII` | `1`, `true` | Replaces Unicode glyphs with clean ASCII characters |
| `TAERT_REDUCE_MOTION` | `1`, `true` | Disables progress spinners and animations for accessibility |
| `NO_COLOR` | any non-empty | Disables ANSI color escapes and uses high-contrast text styles |

For project architecture and safety guarantees, see [architecture.md](architecture.md).
