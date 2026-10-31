# Architecture

System design, safety guarantees, and storage algorithms. For usage instructions, see [usage.md](usage.md).

## Components

| Component | Module | Responsibility |
|---|---|---|
| Safety Engine | `crate::core::safety` | Protects critical paths, credentials, symlinks, and cloud files |
| Sizer | `crate::core::sizer` | Measures actual allocated APFS storage blocks with hardlink deduplication |
| Storage Cache | `crate::core::cache` | In-memory tree aggregation and folder snapshot cache |
| Catalog | `crate::core::catalog` | Defines known cache paths, risk tiers, and auto-select defaults |
| Cleaner | `crate::core::cleaner` | Executes safe deletions with live progress and safety checks |
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

## Uninstalling Apps

An uninstall has two steps, so nothing is removed before you have seen it:

1. **Plan** ([`uninstaller.rs`](../src/core/uninstaller.rs)): `find_leftovers` in [`leftovers.rs`](../src/core/leftovers.rs) checks exact places and reads single folder levels. It never walks a tree, so a wrong name cannot reach far. Each find carries its `Evidence`, which decides whether it starts selected. The plan is a normal `CategoryScan`, one group per app, so the select screen, `--dry-run`, and `--json` all show the same list.
2. **Run**: `Uninstaller::run` works through the selected rows of the plan.

These rules keep it from removing the wrong thing:

- **Allowed paths**: `Safety::can_remove` refuses `/Applications` and `/Library`, so uninstalling has its own gate, `Safety::can_uninstall`. Outside the home folder it accepts only app bundles and entries right below the folders apps use, such as `/Library/LaunchDaemons`, and never `com.apple.` names. Credentials, cloud files, and the ignore list stay protected.
- **Bundle ids**: an id must look like reverse DNS before it builds a path. A name that continues an id of 3 or more parts counts as proof. For a shorter id it is only a hint that starts unselected, because `com.vendor` could continue into every app of that vendor. Apple ids count as part of macOS unless they are on a short list of apps people install themselves, and a plug-in can claim one only if it continues the id of its app.
- **Other apps**: a file that carries the id or name of an app that stays is left alone. A copy of the app, another channel of it, or an app whose id continues this one narrows the plan to the selected bundle. That check runs at planning and again right before the run, and also covers copies the list leaves out.
- **Links**: a bundle that is a link goes alone, with no teardown, because a link can point anywhere. Only a Homebrew cask's link counts as the app itself. A cask counts only if its Caskroom entry resolves to the selected bundle.
- **Order**: before the password, an app is only asked to quit, and Homebrew removes casks. Whatever hurts an app that stays comes after it: unloading, removing login items, signals. An app with root files is removed in the root script, so a refused password keeps all of it. If root must stay away from the bundle, as in `~/Applications`, its root files wait until it is gone. An app's files go only after the app is gone. A selected copy that fails counts as an app that stays from then on.
- **Identity**: every path is checked again right before removal, by device and inode, and the bundle by its id.
- **Processes**: taert finds them by where their program lives, so a name cannot be spoofed. If taert runs from the app, in one of its terminals, the app is neither asked to quit nor signalled.
- **Root**: everything that needs root runs as one script, so macOS asks for the password once. taert uses `sudo` if it has a cached session, and the macOS password dialog otherwise. Paths are quoted. `Safety::is_safe_as_root` refuses a target below a folder the user could replace, because `rm` run as root follows folder links, and the script compares device and inode again right before each `rm`, since the password prompt can stay open for minutes.

Everything that touches the running system goes through the `Host` trait in [`host.rs`](../src/core/host.rs), and `Layout` in [`layout.rs`](../src/core/layout.rs) holds the system's folders. Tests use a fake host and a temporary folder, so they never quit an app or ask for a password.

## APFS Space Accounting

Nominal file lengths (`metadata.len()`) do not reflect actual disk occupancy on APFS due to sparse files, compression, and block clustering:

- **Block Allocation**: `DirSizer` queries macOS `st_blocks` via `std::os::unix::fs::MetadataExt`. Actual allocated storage is computed as `st_blocks * 512`.
- **Hardlink Deduplication**: Files with multiple links (`st_nlink > 1`) are tracked by device and inode number (`FileIdentity { dev, ino }`). Storage blocks are counted once per inode to prevent inflated space estimates.

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
