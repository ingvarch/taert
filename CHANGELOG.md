# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
