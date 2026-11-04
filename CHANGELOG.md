# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Homebrew sorting**: `⇧S`, `⇧N`, and `⇧T` sort the Homebrew list by size, name, or install date, at the top level, inside open rows, and in search results. An `INSTALLED` column shows when each package was installed. The order is not saved.
- **Homebrew updates**: the Homebrew list shows the new version of each outdated package in an `UPDATE` column as `↑ 1.4.4_1` in the accent style, and the status line adds `N updates`. `taert brew --plain` prints the new version in the `outdated` column, and `--json` has `latest_version` (`null` when up to date). taert reads it with `brew outdated --json=v2`.
- **Homebrew update check**: the first time the Homebrew list opens in a run (from the menu or with `taert brew`), taert runs `brew update` in the background, then `brew outdated --json=v2`, and shows the new versions in place. The status line reads `Checking for updates…` meanwhile, and a failed update shows the toast `Could not check for updates: ...`. The check goes on when you leave the list. A list you open again before it ends shows `Checking for updates…` and gets its result. `taert brew --plain` and `taert brew --json` do not run `brew update`.
- **Homebrew upgrade**: `taert brew upgrade [name]` upgrades one outdated formula or cask, or every outdated package, with one `brew upgrade --formula` or `brew upgrade --cask` call each. It runs `brew update` first, asks before it upgrades (`--force` skips the question), and refuses a package that is not installed, up to date, or pinned. A cask counts as pinned when `brew outdated` says so, so the list now shows it as pinned too. `--dry-run` and `--json` upgrade nothing. Upgrades are not written to the history.
- **Homebrew list upgrade**: in the Homebrew list, `u` upgrades the focused package and `⇧U` upgrades every package with a new version that is not refused, in the tree and in search results. The question lists `bun 1.4.2 → 1.4.4_1` for each package and says Homebrew may also upgrade what they need. The result reads `Upgrade Complete` or `Nothing was upgraded`, with the reason and the terminal hint for a failed cask. Refusals are the same as in `taert brew upgrade`. While the list checks for updates, `u` and `⇧U` run nothing and say `Checking for updates. Try again when it ends.` While the list is read again after an upgrade, they run nothing and say `Updating the list. Try again when it ends.` When you close the result, taert reads Homebrew once and measures only the packages that are new or changed. Upgrades are not written to the history.

### Changed
- **Homebrew list columns**: the Homebrew list shows `ORIGIN` (`on request` or `dependency`, empty for a cask) and `UPDATE` (`↑` and the new version) as columns between the name and `INSTALLED`. The name keeps its version, `used by N`, `pinned`, and the apps of a cask. The `SHARE` bar is at most 12 cells wide, so the name and the new columns get the room. Below 100 columns `ORIGIN` hides and `UPDATE` stays. `taert brew --plain` and `--json` do not change.
- **Explore Storage**: a folder that changed since your last visit, or that was cached more than 15 minutes ago, opens at once with `Updating…` and is measured again in the background. Its rows change when the new sizes are in.
- **Removal keys**: `d`, `D`, `Backspace`, and `Delete` start a removal on every screen that removes something: the cleanup lists, Purge, Docker, Large Files, Homebrew cleanup, the uninstall review, the Homebrew list, Explore Storage, broken Startup Items, and the path lists. In Uninstall Apps they open the review. `Enter` no longer starts a removal outside a question dialog. It still goes into a folder, a Homebrew row, or a menu row, and it still starts Slim and Compress and turns a working startup item on or off. On a broken startup item, `Space` and `Enter` do nothing.
- **Date sort**: the date sort moved from `⇧D` to `⇧T` in Explore Storage, Docker, Large Files, and Uninstall Apps, because `D` now removes. The footers and the help sheets show `⇧S/N/T`, `d` for removal, and `d` as the main action of a removal list.

### Fixed
- **History read errors**: when the history file cannot be read, for example when `history.jsonl` is a folder, `taert history` shows the error instead of hanging. The cleanup of old runs stops, leaves the file as it is, and frees the history lock. A line that is not UTF-8 or not JSON is still skipped.
- **History run ids**: two runs no longer get the same id, in one process or in two processes that write the same history file. taert picks the id under the history lock and moves the last four digits on while the file already has that id. The id keeps its form, and old history files load as before.
- **Explore Storage**: when the background update of a folder stops without a result, `Updating…` goes away and the rows keep their old sizes. Open the folder again from its parent, or press `R`, to measure it again.
- **Slim apps**: on a Mac that is not Apple Silicon, the interface leaves the scanning screen and shows why under the name `Slim Universal Apps`.
- **Uninstall**: an uninstall plan with no apps shows the crumbs `taert › Uninstall apps › Review`, the `Uninstall` button, and the right help for the Uninstall key.
- **Search**: the count in the search line reads `1 result` instead of `1 results`.
- **Slim apps**: the result in the terminal list shows `Saved N of Intel code.` under `Slim Complete`, or `Nothing was slimmed` when no app was slimmed. It no longer shows the size of the app folder entry as moved to the Trash, or the saved bytes as removed for good.
- **Compress**: the result in the terminal list shows `Saved N by compressing M files.` under `Compress Complete`, or `Nothing was compressed` when no file was compressed. It no longer says `Clean Complete`, shows the saved bytes as removed for good, or adds a second line with the same bytes. A failure list reads `failed to compress:`, and for slim `failed to slim:`.
- **Slim apps and Compress**: the question before a slim says `Saves about N of Intel code. The old copy of each app goes to the Trash.`, and the question before a compress says `Compress N items?` and `The files stay where they are.`, with a `Compress` button instead of `Remove`. Neither says the files are permanently deleted. While they run, the title reads `Slimming N apps…` or `Compressing N items…` instead of `Removing N items…`.
- **Uninstall**: a Homebrew cask is removed with the brew of the Homebrew install that holds it, so a cask in `/usr/local` no longer fails on a Mac that also has `/opt/homebrew`.
- **Main menu**: in a short window the menu scrolls with the highlighted row, so every row can be reached, and the first or last line says how many rows are out of view.

## [0.6.0] - 2026-11-03

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
- **Explore Storage cache**: entering a folder no longer measures it again when its parent was already measured. Hardlinks and clones no longer drop folder sizes from the cache, apps and other packages use the cache, and the measuring dialog shows only after 150 ms.
- **Orphaned app files**: a row starts selected only when nothing inside it changed for 30 days, as for app caches. Before, only the folder and its direct children counted, so a recent file deeper inside did not protect it. A row with a part taert cannot read starts unselected.
- **Uninstall**: a container named by a UUID that has only the older `.com.apple.containermanager.metadata.plist` is now listed, and an emptied container holding only that file is removed instead of reported as a failure.
- **Flags that do nothing**: `taert schedule`, `taert ignore` and `taert config` now stop with an error when given `--dry-run` or `--json`. Before, `--dry-run` still ran the scheduled cleanup, wrote the schedule agent, and saved the ignore list and settings.
- **Safety**: `~/Library/Keychains` and everything in it is refused by Explore Storage, the clean scans, large files, and uninstall, as it already was for compress. Purge rows follow the same safety rule as the clean scans: no row is listed inside an ignored folder, or holding one, the walk does not enter an ignored folder, rows under 1 KiB no longer show, and a project below a symlinked folder gives no rows.
- **Slim**: an app counts as running when `ps` prints its program in other letter case, under the app's real path, or with two spaces in a row in the name. Slim and compress now share one running check.
- **Uninstall**: an app with package receipts or root files goes to the Trash as you, as the review says, and no longer gets deleted as root. Root files follow once the app is gone, and a refused password keeps only them.
- **Uninstall**: a refused Trash call never reaches root, in any language of macOS. The report keeps the app and its files and shows the App Management or Full Disk Access hint.
- **Uninstall**: the review no longer writes into the app on every redraw, and works out where each row goes once.
- **Uninstall**: the report notes a login item taert could not remove.
- **Uninstall**: the review, the confirmation, and the run now agree on where each row goes. The row of a cask bundle says "(Permanent)", because Homebrew removes it. A bundle you own with a locked entry inside makes the confirm dialog say that macOS will ask for your password, and `taert clean orphans` no longer shows that line for a run that never asks for one.
- **Plain confirmation**: it lists each row with "(Trash)" or "(Permanent)" and names the Trash and permanent removal in separate lines, instead of calling every removal permanent.
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
