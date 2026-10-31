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
