# taert

[![CI](https://github.com/ingvarch/taert/actions/workflows/ci.yml/badge.svg)](https://github.com/ingvarch/taert/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/ingvarch/taert)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-dea584?logo=rust&logoColor=black)](Cargo.toml)
[![macOS](https://img.shields.io/badge/macOS-11+-000000?logo=apple&logoColor=white)](https://apple.com)

A fast, keyboard-driven macOS disk cleanup and storage explorer CLI built with Rust and Ratatui. It finds caches, system junk, project build artifacts, and everything an app leaves behind, giving you transparent control over what gets cleaned. Personal files and apps move to the Trash by default, while system junk and build artifacts are permanently deleted (with `--permanent` to skip Trash entirely).

## Features

- Apple Human Interface Guidelines-inspired TUI with adaptive split-view on wide terminals (≥100 columns)
- Safety engine protecting system roots, credentials, symlinks, and iCloud dataless files
- Safe file removal: personal files and apps move to the Trash by default so they can be recovered, while caches, build artifacts, and Docker objects are deleted permanently
- Accurate APFS space accounting via allocated blocks (`st_blocks * 512`) with hardlink deduplication
- Interactive storage browser with streaming progress, selection, deletion ("Move to Trash" or "Delete Permanently"), and tree-aggregating cache
- Large file finder (`taert large`) to uncover forgotten big files and packages with size, age filtering, and Trash removal
- Live system status: CPU, GPU, memory, storage, battery, processes, and network (live in the TUI, or one snapshot with `taert status --json`)
- Project artifact purger for build targets (`target`, `node_modules`, `.venv`) with age thresholds
- App cache and browser cache cleanup that leaves profiles, cookies, and passwords alone and keeps the caches of browsers and apps it sees running
- Orphaned file detector (`taert clean orphans`) that finds files left behind in `~/Library` by apps that are gone and moves them safely to the Trash
- Docker cleanup for images, volumes, networks, and build cache that no container uses
- App uninstaller that removes an app with its settings, caches, login items, background helpers, and installer receipts
- Homebrew package list (`taert brew`) with the size of each formula and cask, which ones you installed, which are dependencies, and what uses them. `taert brew cleanup` removes unused dependencies and old versions through Homebrew, `taert brew uninstall <name>` removes one formula that nothing uses or one cask without an app, and `taert brew upgrade [name]` upgrades outdated packages.
- Startup manager to review and switch off launchd agents, daemons, and login items with live CPU/RAM metrics
- Universal app slimming (`taert slim`) to strip Intel (x86_64) code from universal apps on Apple Silicon Macs, preserving ARM slices and code signatures and moving originals to the Trash
- Doctor diagnostic command (`taert doctor`) to explain what is slowing the Mac down, heating it up, or draining battery with named causes and single actions
- Transparent APFS compression (`taert compress`) using Apple `libcompression` (LZFSE) to shrink app bundles, Homebrew packages, and toolchains without altering data or breaking codesign
- Removal of local copies of iCloud Drive files (`taert icloud`) to free space. The files stay in iCloud and download again when you open them.
- Local record of removals and changes (`taert history`) kept safely on this Mac
- Scheduled cleanup (`taert schedule`) via native macOS launchd to keep caches under control unattended, with configurable frequency, category selection, and reclaim thresholds
- Script-friendly automation with `--dry-run`, `--json`, `--force`, `--permanent`, and `--plain` flags
- Zero telemetry, zero analytics, private and open-source

## Install

```bash
brew install ingvarch/tap/taert
```

## Usage

```bash
# Launch the interactive terminal workspace
taert

# Or run direct non-interactive cleanups
taert clean dev --dry-run
taert purge ~/Projects --older-than 30 --json
taert analyze ~/Downloads
taert large --min-size 500M
taert docker --dry-run
taert uninstall Zoom --dry-run
taert slim Slack --dry-run
taert doctor
taert startup
taert compress ~/.rustup --dry-run
taert schedule show
```

For subcommands, keyboard shortcuts, and scripting options, see [docs/usage.md](docs/usage.md).

## Build from source

You need Rust and macOS 11+, see [docs/development.md](docs/development.md#prerequisites).

```bash
git clone https://github.com/ingvarch/taert.git
cd taert
cargo build --release
./target/release/taert
```

## Documentation

- [Development](docs/development.md): local setup, checks, universal binary, project structure
- [Usage](docs/usage.md): interactive workspace, CLI subcommands, keybindings
- [Architecture](docs/architecture.md): safety engine, APFS block measurement, smart storage cache
- [Configuration](docs/configuration.md): appearance, accessibility flags, ignore list
- [Releasing](docs/releasing.md): the release workflow and its secrets

## License

[MIT](LICENSE)
