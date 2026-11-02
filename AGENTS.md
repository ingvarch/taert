# Rules for coding agents

taert is a macOS-only Rust TUI that deletes, compresses, and rewrites files and switches launchd services. Agents run it on the owner's real Mac, so these rules come before anything else.

Read [docs/development.md](docs/development.md) and [docs/architecture.md](docs/architecture.md) before you change code.

## Never do this on the Mac you run on

- Run taert with a real removal, compress, slim, uninstall, or schedule change against the real home folder. To try the binary, set `HOME` to a temporary folder, or use `--dry-run`.
- Run `launchctl` `disable`, `enable`, `bootout`, `bootstrap`, `load`, or `unload`, or write to `~/Library/LaunchAgents`. `launchctl print` and `print-disabled` are fine.
- Run `sudo`, `sfltool`, `tmutil` changes, `kill`, `killall`, or anything that asks for a password.
- Empty the Trash, or change anything in `/Applications`, `/Library`, or `/opt/homebrew`.
- Run tests marked `#[ignore]`, unless the issue says so.
- Start a program from a fake `.app` bundle. macOS shows the owner a "damaged" alert. Give the fake `Host` a process table instead.
- Give a fake app a real product's bundle id in a test that writes the bundle to disk. LaunchServices registers it.

## Tests

- Test first: write a failing test, check that it fails for the reason you expect, then write the least code that passes.
- Tests use the fake `Host` and a temporary `Layout`. No test reads or writes the real `~/.config/taert`, `~/Library/Application Support/taert`, launchd, Trash, or `/Applications`.
- A test of the binary sets `HOME` to a temporary folder and runs only commands that read, or that write inside that folder.
- `cargo-insta` is not installed. Update snapshots with `INSTA_UPDATE=always cargo test --all-targets`, read the `.snap` changes with `git diff`, and leave no `.snap.new` files.

## Checks

Run `rustup update stable` once, so clippy matches CI. All three must pass before you commit:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

## Scope

- Do what the issue says, and only that. Its "Out of scope" list is binding.
- Do not add a new saved file, setting, command, or flag unless the issue names it. If a fix seems to need one, stop and ask the owner.
- When a choice can lose a user's data, refuse and report instead of removing, replacing, or rewriting.
- If the code does not match what the issue describes, stop and report the difference rather than guess.

## Commits and text

- Conventional commits: `type(scope): description`, short and imperative. No `Co-Authored-By`, no "Generated with", no emoji.
- Docs, comments, and messages use plain language: short sentences, ordinary verbs.
- Never name other Mac cleanup products in code, docs, or commits.
