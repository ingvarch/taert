# Development

Local setup, checks, and project layout. For user guides and command references, see [usage.md](usage.md).

## Prerequisites

- macOS 11 (Big Sur) or newer.
- Rust stable 1.88 or newer:

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

- macOS Command Line Tools:

  ```bash
  xcode-select --install
  ```

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
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

CI runs these checks and a release snapshot on every push to `main` and all pull requests (`.github/workflows/ci.yml`). Releases are in [releasing.md](releasing.md).

`cargo test --test docker_live -- --ignored` runs against the real Docker daemon. These tests remove only the objects they create, and they leave a few small entries in the build cache.

`cargo test --test uninstall_live -- --ignored` runs real uninstalls of a throwaway app in a temporary home folder. They read `/Applications`, and macOS may ask to let your terminal control System Events.

## Project structure

```
taert/
├── src/
│   ├── main.rs                 # CLI entry point, argument parsing, dispatch
│   ├── lib.rs                  # Library entry point: cli, core, plain, ui modules
│   ├── cli/
│   │   └── args.rs             # Clap CLI flags, commands, and options
│   ├── core/
│   │   ├── app_caches.rs       # App cache and browser cache rows, and which start selected
│   │   ├── apps.rs             # Installed apps and what their bundles say
│   │   ├── bytes.rs            # Binary size formatting (KB, MB, GB)
│   │   ├── cache.rs            # StorageCache: directory size aggregation & snapshot cache
│   │   ├── catalog.rs          # Known cache definitions and path catalogs
│   │   ├── cleaner.rs          # Safe removal engine with progress events
│   │   ├── docker.rs           # Unused Docker objects and their removal through the daemon API
│   │   ├── host.rs             # System commands and root scripts, behind a trait for tests
│   │   ├── iokit.rs            # Read-only IORegistry access for battery, GPU, and disk counters
│   │   ├── layout.rs           # Where a Mac keeps apps and their files
│   │   ├── leftovers.rs        # What an app leaves on disk, and how sure that is
│   │   ├── path_utils.rs       # Path prettifying, tilde expansion, truncation
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
│       ├── palette.rs          # Semantic color palette resolver for dark, light, and no-color
│       ├── path_list_editor.rs # Path list manager for protected paths and project folders
│       ├── preferences.rs      # Settings and age threshold configuration screen
│       ├── progress.rs         # Scanning progress screen with live progress bars
│       ├── review.rs           # Pre-cleanup review screen with group statistics
│       ├── select.rs           # Item selection screen with search filtering and modals
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
│   ├── configuration.md        # Preferences, environment flags, and ignore list
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
