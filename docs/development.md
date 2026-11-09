# Development

Local setup, checks, and project layout. For user guides and command references, see [usage.md](usage.md).

## Prerequisites

- macOS 11 (Big Sur) or newer.
- Rust stable 1.88 or newer. CI always uses the newest stable, which can report lints an older Rust does not, so run `rustup update stable` before the checks:

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

- macOS Command Line Tools:

  ```bash
  xcode-select --install
  ```

taert builds only on macOS, and the crate stops with a compile error elsewhere. `Cargo.lock` lists `windows-*` crates because the `trash` crate depends on them for Windows, Cargo keeps one lock file for all platforms, and they are not built on macOS.

## Build

Compile debug build:

```bash
cargo build
./target/debug/taert
```

Compile optimized release binary:

```bash
cargo build --release
./target/release/taert
```

## Universal Binary Build

To build a universal fat binary supporting both Apple Silicon (`arm64`) and Intel (`x86_64`):

```bash
rustup target add aarch64-apple-darwin
rustup target add x86_64-apple-darwin
./scripts/build-universal.sh
./dist/taert
```

The script runs `cargo build --release` for each target, combines them via `lipo -create`, and verifies architecture slicing with `file`.

## Checks

Run these before submitting changes. All checks must pass with zero warnings and zero failures:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

CI runs these checks and a release snapshot on every push to `main` and all pull requests (`.github/workflows/ci.yml`). A change that touches only Markdown files, `docs/`, or `LICENSE` skips CI. Releases are in [releasing.md](releasing.md).

`cargo test --test docker_live -- --ignored` runs against the real Docker daemon. These tests remove only the objects they create, and they leave a few small entries in the build cache.

`cargo test --test uninstall_live -- --ignored` runs real uninstalls of a throwaway app in a temporary home folder. They read `/Applications`, and macOS may ask to let your terminal control System Events.

`cargo test --test admin_helper_live -- --ignored --nocapture` runs the helper through sudo on throwaway root-owned bundles in `/Applications`. The test that slims an app also writes in `/Library/Application Support/taert`, and needs `TAERT_SLIM_FIXTURE` set to the path of a universal app signed with a Developer ID; without it, that test fails.

Unit tests and TUI tests use a temporary `Layout` and the fake `Host`, so they do not touch your taert files, launchd, Trash, or `/Applications`. Code that saves config, history, the ignore list, or project folders takes its path from the `Layout` it gets (`config_path()`, `history_path()`, `ignore_path()`, `projects_path()`). Scans read the Trash of each volume under the `Layout`'s `Volumes` folder.

The command handlers in `src/main.rs` take the `Host` and `Layout` they need and a `Console` that holds the input, output, and error streams. Their tests run on the fake `Host` and a temporary `Layout`, type the answers to prompts, and check what the handler writes to each stream. A test that removes files first checks that every row of the scan lies in the temporary folder.

A test of the binary sets `HOME` to a temporary folder. The binary has no fake `Host` and reads the real system folders, so these tests run only commands that read inside `HOME` or the repository, or that stop with an error before they reach the system. The ignored live tests use the real Mac on purpose and remove files for good, so nothing reaches the real Trash.

After the tests, CI fails if `~/.config/taert`, `~/Library/Application Support/taert`, the file `~/Library/LaunchAgents/io.github.ingvarch.taert.schedule.plist`, or a loaded `io.github.ingvarch.taert.schedule` agent exists.

Tests never start a program from a fake app bundle. macOS checks the bundle's signature and shows a "damaged" alert for each run. To test a running app, give the fake `Host` a process table.

## Project structure

```
taert/
├── src/
│   ├── main.rs                 # CLI entry point, argument parsing, dispatch
│   ├── lib.rs                  # Library entry point: cli, core, plain, ui modules
│   ├── cli/
│   │   └── args.rs             # Clap CLI flags, commands, and options
│   ├── core/
│   │   ├── admin.rs            # One sudo session per run: one password prompt, keep-alive, end on exit
│   │   ├── admin_helper.rs     # The root side of `taert admin-helper`: checks, compresses, or slims one root-owned app
│   │   ├── app_caches.rs       # App cache and browser cache rows, and which start selected
│   │   ├── apps.rs             # Installed apps and what their bundles say
│   │   ├── bytes.rs            # Binary size formatting (KB, MB, GB)
│   │   ├── cache.rs            # StorageCache: directory size aggregation & snapshot cache
│   │   ├── catalog.rs          # Known cache definitions and path catalogs
│   │   ├── cleaner.rs          # Safe removal engine with progress events
│   │   ├── docker.rs           # Unused Docker objects and their removal through the daemon API
│   │   ├── homebrew.rs         # Installed Homebrew formulae and casks, sizes, what uses them, and cleanup through brew
│   │   ├── host.rs             # System commands and root scripts, behind a trait for tests
│   │   ├── icloud.rs           # Local copies of iCloud Drive files: rows, 30-day selection, and removal through iCloud
│   │   ├── iokit.rs            # Read-only IORegistry access for battery, GPU, and disk counters
│   │   ├── layout.rs           # Where a Mac keeps apps and their files
│   │   ├── leftovers.rs        # What an app leaves on disk, and how sure that is
│   │   ├── path_utils.rs       # Path prettifying, tilde expansion, path completion, truncation, 30-day change check
│   │   ├── permissions.rs      # Full Disk Access check and the System Settings page that grants it
│   │   ├── processes.rs        # Process table from ps and the running-app check
│   │   ├── profiler.rs         # Disk space and system profile analyzers
│   │   ├── projects_store.rs   # Persistent store for scanned project folders
│   │   ├── purger.rs           # Project build artifact scanner (node_modules, target, etc.)
│   │   ├── safety.rs           # Safety engine: root protection, credential isolation, cloud skip
│   │   ├── scanner.rs          # Category scanner for dev, junk, ai, and trash caches
│   │   ├── sizer.rs            # APFS allocated block sizer (st_blocks * 512) and deduplication
│   │   ├── stores.rs           # Config and IgnoreStore persistence
│   │   ├── sysinfo.rs          # RAM pressure and APFS volume metrics via sysctl/statvfs
│   │   └── telemetry.rs        # Live CPU, GPU, disk, network, battery, and process sampling
│   ├── plain/
│   │   └── runner.rs           # Non-interactive plain text output mode
│   └── ui/
│       ├── analyze.rs          # Explore storage screen, breadcrumb stack, measuring thread
│       ├── app.rs              # Ratatui application state machine, event loop, key router
│       ├── env.rs              # Terminal capability, appearance, and accessibility detection
│       ├── folder_preview.rs   # Contents of the folder highlighted in Explore, measured in the background
│       ├── history.rs          # History screen: runs newest first, and the items of one run
│       ├── palette.rs          # Semantic color palette resolver for dark, light, and no-color
│       ├── path_list.rs        # Lists behind Project folders, Protected paths, and custom folder names: items, adding, removing, dialogs
│       ├── progress.rs         # Scanning progress screen with live progress bars
│       ├── select.rs           # Item selection screen with search filtering and modals
│       ├── settings.rs         # Settings screen: sections, rows, and the pickers that change them
│       ├── system_status.rs    # Live system overview with panels and history graphs
│       ├── theme.rs            # Design tokens, typography styles, and symbols
│       ├── workspace.rs        # Workspace home screen with clean submenus and actions
│       └── components/         # Reusable HIG-inspired UI components
│           ├── button.rs       # Keyboard button with primary and active states
│           ├── chrome.rs       # App header bar, breadcrumbs, and status
│           ├── columns.rs      # Places two stacks of lines side by side
│           ├── empty.rs        # Empty state with icons and helpful callouts
│           ├── frame.rs        # Rounded frames drawn line by line
│           ├── gauge.rs        # Proportional bar gauges, charts, sparklines, and legends
│           ├── help.rs         # Modal keyboard shortcuts help sheet
│           ├── hints.rs        # Bottom keybinding hints and primary action badge
│           ├── list.rs         # List row with selection indicator and size badge
│           ├── modal.rs        # Centered dialog with backdrop dimming and drop shadows
│           ├── progress.rs     # Progress view with spinner, counter, and path display
│           ├── text.rs         # Text pluralization and formatting helpers
│           └── toast.rs        # Transient success, error, and notice badges
├── tests/
│   ├── cli_tests.rs            # Integration tests for CLI flags and JSON output
│   ├── components_snapshots.rs # Snapshot tests for individual UI components
│   ├── docker_live.rs          # Ignored tests against a real Docker daemon
│   ├── ui_snapshots.rs         # Snapshot tests for full-screen UI views at multiple sizes
│   ├── ui_tests.rs             # Interactive state and navigation tests
│   └── snapshots/              # Insta snapshot baselines
├── scripts/
│   └── build-universal.sh      # Universal binary build script
├── docs/                       # Guides and technical documentation
│   ├── architecture.md         # Safety engine, APFS accounting, and smart cache
│   ├── configuration.md        # Settings, environment flags, and ignore list
│   ├── development.md          # This guide
│   ├── releasing.md            # Release workflow and its secrets
│   └── usage.md                # Interactive workspace, CLI commands, keybindings
├── .github/workflows/
│   ├── ci.yml                  # Continuous integration workflow
│   └── release.yml             # Release workflow, starts on a version tag
├── .goreleaser.yaml            # Build, sign, archive and Homebrew cask settings
├── about.toml                  # Licenses taert may link
├── about.hbs                   # Template of THIRD_PARTY_NOTICES
├── Cargo.toml                  # Dependencies and build settings
├── LICENSE                     # MIT license
└── README.md                   # Repository entry page
```

For the system architecture, see [architecture.md](architecture.md).
