# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Homebrew packages**: `taert brew` lists installed formulae and casks with their size, whether you installed them, what is outdated or pinned, and what uses them, in the terminal list, plain text, or JSON (`--json`). The list itself only reads.
- **Homebrew cleanup**: `taert brew cleanup` lists the unused dependencies and old versions Homebrew would remove, and removes the ones you select through `brew uninstall` and `brew cleanup`, never by deleting files itself. It skips a dependency that something uses again, never starts a pinned formula selected, and writes each package to the history.
- **Homebrew uninstall**: `taert brew uninstall <name>` removes one formula through `brew uninstall` after a question (`--force` skips it, `--dry-run` only shows). It refuses a cask, a pinned formula, and a formula that other formulae use. In the terminal list, `D` does the same for the focused formula. Each removal goes into the history.
- **Homebrew tree**: the terminal list shows the formulae you installed first, with their dependencies inside, then the formulae nothing you installed needs, then the casks. `Enter` or `→` opens a row, `Enter` or `←` closes it, and `←` on a closed row moves to its parent.
- **System status**: `taert status` prints a live snapshot of CPU, memory, storage, battery, processes, and network in plain text or JSON (`--json`).
- **Orphaned app files**: `taert clean orphans` finds files and containers in `~/Library` left behind by apps that are gone and moves them to the Trash.
- **Shell completions**: `taert completion` prints completion scripts for bash, fish, and zsh.

### Changed
- **Startup items**: `taert startup --json` prints an object with `items` and `notes` instead of a list of items.
- **Platforms**: removed code and gates for platforms taert does not ship on, and the crate fails with a compile error elsewhere.

### Removed
- `taert optimize` is gone; `taert doctor` replaces it (removed in 5a80563, never recorded).

### Fixed
- **Uninstall**: an app with package receipts or root files goes to the Trash as you, as the review says, and no longer gets deleted as root. Root files follow once the app is gone, and a refused password keeps only them.
- **Uninstall**: a refused Trash call never reaches root, in any language of macOS. The report keeps the app and its files and shows the App Management or Full Disk Access hint.
- **Uninstall**: the review no longer writes into the app on every redraw, and works out where each row goes once.
- **Uninstall**: the report notes a login item taert could not remove.
- **Explore Storage**: with `--permanent`, the delete dialog focuses "Delete Permanently".
- **Slim**: the old app is never deleted. `taert slim` works in a holding folder `<name>.app.taert-slim-tmp` and refuses to run if one is left from an earlier run. The old app in a leftover holding folder is not listed for slimming. A failed Trash step no longer reports an error for an app that is slimmed: it says where the old app is (`old_app_path` in `--json`). The old app goes to the Trash as `<name>.app`. Root-owned bundles, bundles with a setuid or setgid file, and a bundle path that is a link are refused. A holding folder that cannot be removed after the Trash step is named in the result. `taert slim <name>` asks `Slim X? [y/N]` in a terminal too, and `--json` needs `--force`. The list, `--json`, and the interface run the signature and Gatekeeper checks. `TAERT_SLIM_ALLOW_ADHOC` is gone. Files sealed by a nested bundle's own `CodeResources` are left alone. A change anywhere inside the app, or a failed `ps`, stops the swap. Extended attributes are removed without following links, only after a failed `codesign` check. Savings count allocated sizes.
- **Scheduled cleanup**: a Homebrew upgrade no longer removes the launch agent, because the cask has no `uninstall` stanza; `taert schedule show` says when the agent file is missing and how to load it again. Category names are stored in lower case, each name once. A failed `launchctl bootstrap` fails `taert schedule set` and saves nothing, and the Preferences row applies with `Enter` and shows the error. `show` reports the newer of the last run and the last skip. An unreadable lock file fails the run instead of ending it silently. The unreleased `config.json` fallback is gone: settings are TOML only.
- **Doctor**: a finding never offers to stop pid 0 or pid 1 and says the process is part of macOS, `nettop` lines without a readable pid are left out, and the code that sends a signal refuses pid 0, pid 1, and a pid too large to be one process.
- **Doctor**: wake times use the zone in `pmset -g log`.
- **Doctor**: the largest apps are sorted by memory.
- **Doctor**: heat counts only `CPU_Speed_Limit` below 100 and the two warning levels.
- **Doctor**: the network rate covers the whole check.
- **Doctor**: energy findings use the change in `IDLEW`, not the `POWER` column. A check under 4 seconds skips them.
- **Doctor**: a check that could not run is skipped with its reason, in plain, JSON, and the terminal screen.
- **Doctor**: wake and memory findings name the process that asked and the swap used.
- **Doctor**: plain output prints `kill <pid>` or `taert analyze` for actions.
- **Doctor**: the narrow layout shows evidence and action, wrapped by words, and warnings are yellow.
- **Doctor**: the status line shows errors in red and successes in green.
- **Doctor**: stopping a process sends a confirmed `SIGTERM`, starts on Cancel, and shows a failed signal. The dialog is titled Stop Process.
- **Doctor**: the unused ask-an-app-to-quit action is removed.
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
