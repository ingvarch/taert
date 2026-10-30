# taert

> From Icelandic *tært*: "clear, crystal clean".

**taert** is a fast, keyboard-driven macOS disk cleanup and storage explorer CLI built with **Rust** and **Ratatui**.

It finds caches, system junk, and project build artifacts, giving you full, transparent control over what gets deleted permanently.

---

## Highlights

- **Safety First**: Deep system root protection (`/System`, `/Library`, `/Applications`), credential isolation (`.ssh`, `.aws`, `.gnupg`), protection of iCloud / CloudStorage / APFS dataless files, and symlink rejection.
- **Accurate Space Accounting**: Measures actual allocated APFS storage blocks (`st_blocks * 512`), deduping hardlinks.
- **Interactive TUI**: PureMac-inspired keyboard workspace powered by Ratatui:
  - Live search filtering (`/`).
  - Toggle selections (`Space`), select all (`A`), or clear (`N`).
  - Read-only storage directory analyzer with visual proportional bars.
- **Unattended & Script-Friendly**: Complete support for `--dry-run`, `--json`, and non-interactive `--plain` modes.
- **Zero Telemetry**: Free, open-source, and private.

---

## Installation

### From Source

Requires macOS 11+ and Rust toolchain:

```bash
git clone https://github.com/ingvarch/taert.git
cd taert
cargo build --release
./target/release/taert
```

### Universal Binary Build (Apple Silicon & Intel)

```bash
./scripts/build-universal.sh
./dist/taert
```

---

## Quick Start

Launch the interactive workspace:
```bash
taert
```

Or jump straight to a specific action:

```bash
# Scan developer tool caches
taert clean dev

# Scan system junk & logs
taert clean junk

# Scan AI tool caches (Ollama, LM Studio, Cursor)
taert clean ai

# Purge project artifacts (node_modules, target, .venv) in a directory
taert purge ~/Projects

# Browse disk usage interactively (read-only)
taert analyze ~/Projects

# View factual RAM pressure & APFS disk state
taert optimize
```

---

## Keybindings (Interactive TUI)

| Key | Action |
|---|---|
| `↑` / `↓` | Move selection cursor |
| `Space` | Toggle focused item on/off |
| `A` / `N` | Select all visible / clear selection |
| `/` | Live search filter |
| `←` / `→` | Scroll long path in footer / navigate directories |
| `Enter` | Review selection & proceed to confirmation |
| `Esc` / `q` | Cancel / Go back / Quit |

---

## Scripting & Automation

- **`--dry-run`**: Preview scan results without modifying any files.
  ```bash
  taert clean dev --dry-run
  ```
- **`--json`**: Structured JSON output for scripts.
  ```bash
  taert purge ~/Projects --json
  ```
- **`--force`**: Non-interactive unattended cleanup of default-selected items (previews recommended first).
  ```bash
  taert purge ~/Projects --older-than 30 --force
  ```
- **`--plain`**: Text mode without TUI.
  ```bash
  taert clean junk --plain
  ```

---

## Protecting Paths (Ignore List)

You can prevent any directory from ever being scanned or cleaned:

```bash
taert ignore add ~/Projects/important-project
taert ignore list
taert ignore remove ~/Projects/important-project
```

---

## Acknowledgements

Path catalogs and safety rules are inspired by and ported from [PureMac CLI](https://github.com/momenbasel/PureMac) under the MIT License (see `NOTICE`).

## License

MIT © 2026 Igor (ingvarch)
