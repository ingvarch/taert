# taert

[![CI](https://github.com/ingvarch/taert/actions/workflows/ci.yml/badge.svg)](https://github.com/ingvarch/taert/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/ingvarch/taert)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-dea584?logo=rust&logoColor=black)](Cargo.toml)
[![macOS](https://img.shields.io/badge/macOS-11+-000000?logo=apple&logoColor=white)](https://apple.com)

A fast, keyboard-driven macOS disk cleanup and storage explorer CLI built with Rust and Ratatui. It finds caches, system junk, and project build artifacts, giving you transparent control over what gets deleted permanently.

## Features

- Apple Human Interface Guidelines-inspired TUI with adaptive split-view on wide terminals (≥100 columns)
- Safety engine protecting system roots, credentials, symlinks, and iCloud dataless files
- Accurate APFS space accounting via allocated blocks (`st_blocks * 512`) with hardlink deduplication
- Interactive storage browser with streaming progress, selection, deletion, and tree-aggregating cache
- Live system status: CPU, GPU, memory, storage, battery, processes, and network
- Project artifact purger for build targets (`target`, `node_modules`, `.venv`) with age thresholds
- Docker cleanup for images, volumes, networks, and build cache that no container uses
- Script-friendly automation with `--dry-run`, `--json`, `--force`, and `--plain` flags
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
taert docker --dry-run
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
