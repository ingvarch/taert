# Architecture

System design, safety guarantees, and storage algorithms. For usage instructions, see [usage.md](usage.md).

## Components

| Component | Module | Responsibility |
|---|---|---|
| Safety Engine | `crate::core::safety` | Protects critical paths, credentials, symlinks, and cloud files |
| Sizer | `crate::core::sizer` | Measures actual allocated APFS storage blocks with hardlink and clone deduplication |
| Storage Cache | `crate::core::cache` | In-memory tree aggregation and folder snapshot cache |
| Catalog | `crate::core::catalog` | Defines known cache paths, risk tiers, and auto-select defaults |
| Cleaner | `crate::core::cleaner` | Executes safe deletions with live progress and safety checks |
| Startup Manager | `crate::core::startup` | Scans, associates, inspects live metrics, and switches launchd/login items |
| Scanner & Purger | `crate::core::scanner`, `crate::core::purger` | Discovers cache categories and project build artifacts |
| App Controller | `crate::ui::app` | Main Ratatui event loop, key router, and modal overlays |
| TUI Components | `crate::ui::components` | Apple HIG-inspired widgets (AppChrome, Modal, Button, Gauge, ProgressView) |

## Safety Engine

Disk cleanup tools must never compromise system integrity, user credentials, or remote cloud storage:

1. **System Root Protection**:
   `Safety::is_critical_root` strictly prevents operations on `/`, `/System`, `/Library`, `/Applications`, `/usr`, `/bin`, `/sbin`, `/var`, `/etc`, and user home root `~`. Attempts to delete these paths fail immediately before any filesystem calls.
2. **Credential Isolation**:
   `Safety::is_credential_path` protects identity stores including `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.docker`, and keychains.
3. **Symlink Rejection**:
   `Safety::has_symlink_component` verifies that no path or parent component is a symbolic link. Deletion operations refuse to follow symlinks to prevent accidental directory traversal outside the intended target.
4. **Cloud Storage & APFS Dataless Protection**:
   `Safety::is_cloud_or_dataless` inspects filesystem metadata for `SF_DATALESS` and checks known sync provider paths (iCloud Drive, OneDrive, Google Drive, Dropbox, Box). Placeholder files with offline data are never evicted or deleted.
5. **Path Validation on Deletion**:
   Every file removal re-validates safety rules immediately before `fs::remove_dir_all` or `fs::remove_file` is invoked.

## Docker Cleanup

Docker objects are not files, so they skip the path checks above and go to the daemon instead. `ScanItem::docker` carries a `DockerObject`, and `Cleaner` hands those to `DockerRemover` before any file logic runs. A label that matches a real path can't touch that path.

Rules that keep this safe:

- An object counts as unused only when no container refers to it, stopped containers included. An unknown use count means in use.
- Nothing is removed with `force`, so Docker refuses whatever a container still needs.
- A tag is checked again before removal, because it can move to another image after the scan.
- Volumes are never selected by default, so `--force` can't remove them.
- A daemon that reports no disk usage is an error, never an empty list.
- `DOCKER_HOST` and the Docker context pick the daemon. A remote one stops with an error and never falls back to a local daemon.

The freed size comes from the daemon's totals before and after, because images share layers.

`bollard` stays pinned to 0.19. Versions 0.20 and newer read `/system/df` only in the API 1.52 shape, and a daemon on an older API then comes back empty with no error. Daemons answer an API 1.49 client in the older shape, including Docker 29 and newer.

## App and Browser Caches

[`app_caches.rs`](../src/core/app_caches.rs) adds two groups to the `junk` scan. App caches are the entries of `~/Library/Caches` one level deep. Browser caches are fixed paths per browser, one row each. A browser with two folders keeps the second in `ScanItem::also`, and `Cleaner` checks both before either goes. If one fails during the removal itself, the report counts the row with the bytes of the folder that went, and lists the one that failed. Neither group walks `Application Support`, so profiles are never rows.

- **No double counting**: an entry that a named target covers, or that is the parent of one, is left out. So are `com.apple.*`, `CloudKit`, and `FamilyCircle`.
- **Selection**: `starts_selected(stale, running)` decides. Stale means nothing in the tree changed for 30 days, and anything unreadable counts as changed. Running comes from one process table per scan. An app cache matches its app by bundle id from `find_apps`, and so does a browser. Any program inside a bundle matched this way counts. `find_apps` leaves out Safari and copies outside the app folders, so a bundle with the browser's name counts too. For those bundles only a program in `Contents/MacOS` counts, because `SafariLinkExtension` runs from inside Safari's bundle while Safari is closed.
- **Refused reads**: when macOS refuses a browser folder, the scan carries one note per browser that names Full Disk Access.
- **Safety**: rows go through the same checks as every other row, and `Cleaner` verifies identity before removal.

## Uninstalling Apps

An uninstall has two steps, so nothing is removed before you have seen it:

1. **Plan** ([`uninstaller.rs`](../src/core/uninstaller.rs)): `find_leftovers` in [`leftovers.rs`](../src/core/leftovers.rs) checks exact places and reads single folder levels. It never walks a tree, so a wrong name cannot reach far. Each find carries its `Evidence`, which decides whether it starts selected. The plan is a normal `CategoryScan`, one group per app, so the select screen, `--dry-run`, and `--json` all show the same list.
2. **Run**: `Uninstaller::run` works through the selected rows of the plan.

These rules keep it from removing the wrong thing:

- **Allowed paths**: `Safety::can_remove` refuses `/Applications` and `/Library`, so uninstalling has its own gate, `Safety::can_uninstall`. Outside the home folder it accepts only app bundles and entries right below the folders apps use, such as `/Library/LaunchDaemons`, and never `com.apple.` names. Credentials, cloud files, and the ignore list stay protected.
- **Bundle ids**: an id must look like reverse DNS before it builds a path. A name that continues an id of 3 or more parts counts as proof. For a shorter id it is only a hint that starts unselected, because `com.vendor` could continue into every app of that vendor. Apple ids count as part of macOS unless they are on a short list of apps people install themselves, and a plug-in can claim one only if it continues the id of its app. Data of a helper inside the app counts as proof only if the helper is named after the app, or comes from the same vendor and no other app of that vendor stays.
- **Other apps**: a file that carries the id or name of an app that stays is left alone. A copy of the app, another channel of it, or an app whose id continues this one narrows the plan to the selected bundle. That check runs at planning and again right before the run, and also covers copies the list leaves out.
- **Links**: a bundle that is a link goes alone, with no teardown, because a link can point anywhere. Only a Homebrew cask's link counts as the app itself. A cask counts only if its Caskroom entry resolves to the selected bundle.
- **Order**: before the password, an app is only asked to quit, and Homebrew removes casks. Whatever hurts an app that stays comes after it: unloading, removing login items, signals. An app with root files is removed in the root script, so a refused password keeps all of it. If root must stay away from the bundle, as in `~/Applications`, its root files wait until it is gone. An app's files go only after the app is gone. Before removing a bundle as the user, taert walks it for anything locked, so a removal does not stop halfway. A selected copy that fails counts as an app that stays from then on, and root files shared by two selected copies wait for both.
- **Identity**: every path is checked again right before removal, by device and inode, and the bundle by its id.
- **Processes**: taert finds them by where their program lives, so a name cannot be spoofed. If taert runs from the app, in one of its terminals, the app is neither asked to quit nor signalled.
- **Root**: everything that needs root runs as one script, so macOS asks for the password once. taert uses `sudo` if it has a cached session, and the macOS password dialog otherwise. Paths are quoted. `Safety::is_safe_as_root` refuses a target below a folder the user could replace, because `rm` run as root follows folder links, and the script compares device and inode again right before each `rm`, since the password prompt can stay open for minutes.

Everything that touches the running system goes through the `Host` trait in [`host.rs`](../src/core/host.rs), and `Layout` in [`layout.rs`](../src/core/layout.rs) holds the system's folders. Tests use a fake host and a temporary folder, so they never quit an app or ask for a password.

## Trash vs Permanent Removal

Items slated for removal are classified via `cleaner::item_removal_mode`:
- **Personal files and user apps**: user app bundles and leftovers in home folders (`~/Library`, `~/.config`, etc.) move to the Trash by default via `NSFileManager` (`Host::trash`). In Explore Storage (`taert analyze`), users can choose between "Move to Trash" (default) and "Delete Permanently".
- **Permanent removal**: caches and logs (`taert clean`), build artifacts (`taert purge`), Docker objects (`taert docker`), emptying the Trash, root files (`/Library/...`), and items with `contents_only = true` are permanently deleted.
- **`--permanent` override**: passing `--permanent` forces permanent deletion across all subcommands, bypassing the Trash.
- **Reporting**: outcomes and progress report both `trashed_bytes` ("Moved X to the Trash. Empty it to get the space back.") and `freed_bytes` ("Removed Y permanently.").

## Startup and Background Items

[`startup.rs`](../src/core/startup.rs) manages launchd services (user agents, system agents, daemons) and classic login items:

- **Launchd Management**: Inspects disabled states (`launchctl print-disabled`) and running services (`launchctl print gui/<uid>` and `launchctl print system`). Services are associated with installed applications via program path or bundle IDs.
- **Ordered Teardown**: When disabling a launchd service, `taert` executes `launchctl disable` *before* `launchctl bootout`. This prevents `launchd` from immediately respawning the job when stopped.
- **Domain Separation**: User launch agents operate in the `gui/<uid>/` domain without elevation (`Host::run`). System daemons operate in `system/` and require root elevation via `Host::run_admin`.
- **System Events & Login Items**: Discovered via AppleScript (`osascript`) through System Events. When disabled, items are recorded in `~/.config/taert/disabled_login_items.json` and deleted from Login Items; enabling re-creates them via AppleScript. If Automation permissions are refused by macOS, `taert` logs a clean diagnostic note without crashing or corrupting other items.
- **Safety Gate**:
  - Apple system items (`com.apple.*`) are hidden by default, excluded from user scans unless `--all` is passed, and strictly refused for modification or deletion.
  - Security agents (CrowdStrike, SentinelOne, Jamf, Carbon Black, etc.) are protected and read-only.
  - Broken items: user launch agents whose target binary is missing from disk can be permanently deleted after explicit confirmation, verified by `Safety::can_remove`.

## APFS Space Accounting

Nominal file lengths (`metadata.len()`) do not reflect actual disk occupancy on APFS due to sparse files, compression, and block clustering:

- **Block Allocation**: `DirSizer` queries macOS `st_blocks` via `std::os::unix::fs::MetadataExt`. Actual allocated storage is computed as `st_blocks * 512`.
- **Hardlink Deduplication**: Files with multiple links (`st_nlink > 1`) are tracked by device and inode number (`FileIdentity { dev, ino }`). Storage blocks are counted once per inode to prevent inflated space estimates.
- **Clone Deduplication**: APFS clones (created by Finder duplicate, `cp -c`, or package managers like pnpm and Bun) share disk blocks despite having distinct inodes. `DirSizer` queries clone identity and extended flags (`EF_SHARES_ALL_BLOCKS`) via `getattrlist` ([`apfs.rs`](../src/core/apfs.rs)). Files sharing all blocks with another file in the same measurement are counted once per clone ID and device. Files that share only part of their blocks count in full, because clone IDs do not specify partial sharing amounts.

Limits:

- A clone whose twin sits outside the measured folder still counts in full, though removing it frees nothing.
- The freed total of a cleanup adds up row sizes, so two rows that share blocks count twice.

## Smart Storage Cache

Explore Storage uses a two-tier in-memory cache ([`StorageCache`](file:///Users/igor/Repos/ingvarch/taert/src/core/cache.rs)):

1. **Subtree Directory Aggregation**:
   When a directory is scanned, `DirSizer` collects all visited subdirectories and direct file sizes. Upon walk completion, directories are sorted from deepest to shallowest and aggregated bottom-up:
   $$\text{size}(D) = \text{direct\_bytes}(D) + \sum_{C \in \text{children}(D)} \text{size}(C)$$
   This pre-computes exact recursive sizes for every subdirectory in the tree in a single pass ($O(D \log D)$ in $< 2\text{ ms}$).
2. **Instant Drill-Down**:
   When navigating into a subfolder, its children's sizes are already present in the cache. Directory listings load in $< 1\text{ ms}$ without additional disk traversal.
3. **Snapshot Caching**:
   Constructed `FolderSnapshot` structures are cached in memory. Re-visiting a previously viewed folder restores the view instantly in the same frame without spawning a background thread.
4. **Cache Invalidation**:
   - Deleting items in Explore Storage invokes `invalidate_prefix` on affected paths and updates parent snapshots.
   - Performing cleanup operations from the main menu clears the cache to prevent stale data.
   - Directory modified times (`mtime`) and a 15-minute TTL ensure stale entries on disk are automatically refreshed.
   - Pressing `R` in Explore Storage forces an immediate cache bypass and fresh rescan.

For developer instructions and checks, see [development.md](development.md).
