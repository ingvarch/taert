# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Homebrew packages**: `taert brew` lists installed formulae and casks with their size, whether you installed them, what is outdated or pinned, and what uses them, in the terminal list, plain text, or JSON (`--json`). The list itself only reads.
- **Homebrew cleanup**: `taert brew cleanup` lists the unused dependencies and old versions Homebrew would remove, and removes the ones you select through `brew uninstall` and `brew cleanup`, never by deleting files itself. It skips a dependency that something uses again, never starts a pinned formula selected, and writes each package to the history.
- **Homebrew uninstall**: `taert brew uninstall <name>` removes one formula through `brew uninstall` after a question (`--force` skips it, `--dry-run` only shows). It refuses a cask, a pinned formula, and a formula that other formulae use. In the terminal list, `Enter` does the same for the focused formula. Each removal goes into the history.
- **System status**: `taert status` prints a live snapshot of CPU, memory, storage, battery, processes, and network in plain text or JSON (`--json`).
- **Orphaned app files**: `taert clean orphans` finds files and containers in `~/Library` left behind by apps that are gone and moves them to the Trash.
- **Shell completions**: `taert completion` prints completion scripts for bash, fish, and zsh.

### Changed
- **Startup items**: `taert startup --json` prints an object with `items` and `notes` instead of a list of items.
- **Platforms**: removed code and gates for platforms taert does not ship on, and the crate fails with a compile error elsewhere.

### Fixed
- **Scheduled cleanup**: a Homebrew upgrade no longer removes the launch agent, because the cask has no `uninstall` stanza; `taert schedule show` says when the agent file is missing and how to load it again. Category names are stored in lower case, each name once. A failed `launchctl bootstrap` fails `taert schedule set` and saves nothing, and the Preferences row applies with `Enter` and shows the error. `show` reports the newer of the last run and the last skip. An unreadable lock file fails the run instead of ending it silently. The unreleased `config.json` fallback is gone: settings are TOML only.
- **Interactive dry-run and history locking**: `--dry-run` refuses to open interactive screens with an error directing to `--plain`; history writes take an exclusive lock on the history directory to keep concurrent lines and prunes safe; compress and slim log saved bytes under outcome `changed`; history help text matches its behavior.
- **Large files and clone counting**: relative roots canonicalize properly and apply exclusions to credential stores and Library folders; scanning a folder inside Library no longer drops all rows; age filtering runs before the limit heap using effective Spotlight last-used dates; values above 100 years for `--older-than` are rejected by clap, and overflow during age cutoff calculation returns an empty scan instead of panicking; progress reports only final kept rows; virtual machine bundles (`.pvm`, `.vmwarevm`, `.utm`) are treated as single items; reveal in Finder hint shows across all select screens; subfolders holding duplicate hardlinks or APFS clones display their own size when measured individually.
- **Startup items**: `taert startup --all` no longer lets root write to a path in the home folder, shows the Background Task Management entries, and times out on its own. `disable` and `enable` read the state again and report it. Daemons come from `launchctl print system` without `--all`. A name that fits several items is refused. A login item's path is saved before the item is removed, and a `disabled_login_items.json` that taert cannot read is never written over. Security login items are read-only. Notes show in the TUI and in `--json`. A disabled item that still runs offers "stop". Login item paths reach AppleScript as arguments. A login item can be named by its path. Services that macOS ships under a label outside `com.apple.`, such as `com.openssh.ssh-agent`, are hidden and read-only, and so are daemons whose files launchd does not show, unless an app outside Apple registered them. A service with no plist in `~/Library/LaunchAgents`, `/Library/LaunchAgents`, or `/Library/LaunchDaemons`, such as one an app registered, is read-only and says where to switch it, because launchd forgets it once it stops. A daemon keeps its plist only from `/Library/LaunchDaemons`, and an agent only from a `LaunchAgents` folder, so root never loads a plist from the home folder. `taert doctor` offers to switch off a broken item only when taert can switch it, and otherwise says why. NetworkExtension services are named after their extension, not the last number of their version.
- **Uninstall**: `taert uninstall` removes only the login item that opens the copy it removes, and keeps other login items with the same name. It removes a Homebrew cask with `HOMEBREW_NO_AUTOREMOVE=1`, so Homebrew no longer removes unused formulae along with the app.

## [0.1.0] - 2026-10-30

### Added
- **Core Safety Engine**: Strict path protection, verification against root/system paths, iCloud/CloudStorage/APFS dataless files, symlink rejection, credential root isolation, and ignore-list inheritance.
- **Accurate Sizer**: APFS allocated block measurement (`st_blocks * 512`) with hardlink deduplication (`dev+ino`).
- **Comprehensive Catalog**:
  - `dev`: Homebrew, npm, Yarn, pnpm, pip, Cargo, Go, CocoaPods, VS Code, JetBrains, Gradle, Poetry, uv, Bun, Deno, mise, Flutter, SwiftPM, Docker Desktop, OrbStack, etc.
  - `junk`: User logs, Xcode DerivedData, Archives, iOS/watchOS/tvOS DeviceSupport, CoreSimulator caches, SwiftUI previews.
  - `ai`: Ollama, LM Studio, Cursor, Hugging Face caches.
  - `trash`: User `~/.Trash` and mounted volume trashes `/Volumes/*/.Trashes/<uid>`.
- **Project Purge**: Automatic discovery and cleanup of `node_modules`, `target`, `.build`, `.venv`, `.next`, `DerivedData`, etc., with customizable age threshold.
- **Interactive TUI (Ratatui)**:
  - Main Workspace menu.
  - Interactive selection view with live `/` search, `Space` toggle, `A`/`N` mass select, and path scroll.
  - Confirmation review view showing exact deletion candidates.
  - Storage Analyzer TUI: interactive, read-only directory browser with visual proportion bars.
- **CLI & Automation**: `--plain`, `--dry-run`, `--json`, and unattended `--force` flags.
- **Configuration & Ignore Management**: `taert config` and `taert ignore add|remove|list`.
