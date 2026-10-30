pub mod cli;
pub mod core;
pub mod plain;
pub mod ui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::{CleanArgs, CleanCategory, Cli, Command, ConfigArgs, IgnoreAction, PurgeArgs};
use crate::core::bytes::format_bytes;
use crate::core::purger::Purger;
use crate::core::safety::Safety;
use crate::core::scanner::{CategoryScan, CategoryScanner};
use crate::core::sizer::DirSizer;
use crate::core::stores::{Config, IgnoreStore};
use crate::core::sysinfo::SysInfo;
use crate::plain::PlainRunner;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let is_tty = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();

    // Plain mode is forced if explicitly specified or if not running in an interactive TTY
    let use_plain = cli.plain || !is_tty;

    let ignore = IgnoreStore::load_or_default();
    let config = Config::load_or_default();

    match cli.command {
        None => {
            // Default `taert` workspace
            if use_plain {
                println!("taert v{}", env!("CARGO_PKG_VERSION"));
                println!(
                    "Run 'taert --help' or specify a subcommand in non-interactive/plain mode."
                );
                ExitCode::SUCCESS
            } else {
                let mut app = ui::app::App::new(ignore, config);
                match app.run() {
                    Ok(_) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("TUI Error: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
        }

        Some(Command::Clean(ref args)) => handle_clean(args, &cli, use_plain, &ignore),

        Some(Command::Purge(ref args)) => handle_purge(args, &cli, use_plain, &ignore, &config),

        Some(Command::Analyze(ref args)) => {
            handle_analyze(args.path.clone(), args.depth, &cli, use_plain)
        }

        Some(Command::Optimize) => handle_optimize(&cli),

        Some(Command::Ignore(args)) => handle_ignore(args.action, &ignore),

        Some(Command::Config(ref args)) => handle_config(args, &config),
    }
}

fn handle_clean(args: &CleanArgs, cli: &Cli, use_plain: bool, ignore: &IgnoreStore) -> ExitCode {
    let categories: Vec<(&str, &str)> = match args.category {
        Some(CleanCategory::Dev) => vec![("dev", "Developer Caches")],
        Some(CleanCategory::Junk) => vec![("junk", "System Junk")],
        Some(CleanCategory::Ai) => vec![("ai", "AI Tool Caches")],
        Some(CleanCategory::Trash) => vec![("trash", "Trash")],
        None => vec![
            ("dev", "Developer Caches"),
            ("junk", "System Junk"),
            ("ai", "AI Tool Caches"),
            ("trash", "Trash"),
        ],
    };

    let mut combined_groups = Vec::new();
    for (cat_id, cat_title) in categories {
        let scan = CategoryScanner::scan(cat_id, cat_title, ignore, None, None);
        combined_groups.extend(scan.groups);
    }

    let mut scan = CategoryScan {
        id: "clean".to_string(),
        title: "Clean Review".to_string(),
        groups: combined_groups,
    };

    process_scan_result(&mut scan, cli, use_plain, ignore)
}

fn handle_purge(
    args: &PurgeArgs,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    config: &Config,
) -> ExitCode {
    let scan_root = match Safety::validate_scan_root(&args.path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: Cannot scan path '{}': {}", args.path.display(), e);
            return ExitCode::FAILURE;
        }
    };

    let older_than = args.older_than.unwrap_or(config.older_than_days);
    let mut scan = Purger::scan_projects(&scan_root, older_than, &config.extra_artifacts, ignore);

    process_scan_result(&mut scan, cli, use_plain, ignore)
}

fn process_scan_result(
    scan: &mut CategoryScan,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
) -> ExitCode {
    // 1. JSON mode: output JSON and exit immediately without modifying files
    if cli.json {
        match serde_json::to_string_pretty(scan) {
            Ok(json_str) => {
                println!("{json_str}");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("Failed to serialize JSON: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // 2. Dry-run mode: display scan and exit without deleting
    if cli.dry_run {
        PlainRunner::print_scan(scan);
        println!("\nDry run completed. No files were deleted.");
        return ExitCode::SUCCESS;
    }

    // 3. Force mode: unattended cleanup of default-selected items
    if cli.force {
        let report = PlainRunner::execute_clean(scan, ignore);
        return if report.failures.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    // 4. Plain interactive mode or fallback
    if use_plain {
        PlainRunner::print_scan(scan);
        if let Err(e) = PlainRunner::prompt_selection(scan) {
            eprintln!("Selection error: {e}");
            return ExitCode::FAILURE;
        }

        match PlainRunner::prompt_confirm(scan) {
            Ok(true) => {
                let report = PlainRunner::execute_clean(scan, ignore);
                if report.failures.is_empty() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Ok(false) => {
                println!("Operation cancelled.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("Confirmation error: {e}");
                ExitCode::FAILURE
            }
        }
    } else {
        // Launch TUI with pre-scanned items
        let mut app =
            ui::app::App::with_select(scan.clone(), ignore.clone(), Config::load_or_default());
        match app.run() {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("TUI Error: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

fn handle_analyze(path: PathBuf, depth: usize, cli: &Cli, use_plain: bool) -> ExitCode {
    let scan_root = match Safety::validate_scan_root(&path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: Cannot analyze '{}': {}", path.display(), e);
            return ExitCode::FAILURE;
        }
    };

    if !use_plain && !cli.json {
        let mut app = ui::app::App::with_analyze(
            scan_root,
            IgnoreStore::load_or_default(),
            Config::load_or_default(),
        );
        return match app.run() {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("TUI Error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    println!("Analyzing disk usage for: {}\n", scan_root.display());

    // Compute entries up to requested depth
    let mut entries = Vec::new();
    if let Ok(dir_entries) = std::fs::read_dir(&scan_root) {
        for entry in dir_entries.flatten() {
            let p = entry.path();
            if Safety::has_symlink_component(&p) || Safety::is_cloud_or_dataless(&p) {
                continue;
            }
            let size = DirSizer::size_of(&p);
            entries.push((p, size));
        }
    }

    entries.sort_by_key(|a| std::cmp::Reverse(a.1));

    if cli.json {
        let json_entries: Vec<_> = entries
            .iter()
            .map(|(p, s)| serde_json::json!({ "path": p, "bytes": s }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_entries).unwrap());
        return ExitCode::SUCCESS;
    }

    for (p, size) in entries.iter().take(depth * 25) {
        let name = p.file_name().unwrap_or_default().to_string_lossy();
        println!("{:>10}  {}", format_bytes(*size), name);
    }

    ExitCode::SUCCESS
}

fn handle_optimize(cli: &Cli) -> ExitCode {
    let status = SysInfo::current_status();

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&status).unwrap());
        return ExitCode::SUCCESS;
    }

    println!("=== System Storage & Memory ===");
    println!(
        "Memory: {} / {} used",
        crate::core::bytes::format_memory_bytes(status.memory.used_bytes),
        crate::core::bytes::format_memory_bytes(status.memory.total_bytes)
    );
    if status.memory.swap_total_bytes > 0 {
        println!(
            "Swap:   {} / {} used",
            crate::core::bytes::format_memory_bytes(status.memory.swap_used_bytes),
            crate::core::bytes::format_memory_bytes(status.memory.swap_total_bytes)
        );
    }

    if let Some(disk) = status.main_disk {
        println!(
            "Disk ({}): {} available / {} total",
            disk.mount_point,
            format_bytes(disk.available_bytes),
            format_bytes(disk.total_bytes)
        );
    }

    ExitCode::SUCCESS
}

fn handle_ignore(action: IgnoreAction, ignore: &IgnoreStore) -> ExitCode {
    let mut current = ignore.clone();
    let config_path = IgnoreStore::default_path().unwrap_or_else(|| PathBuf::from("ignore.toml"));

    match action {
        IgnoreAction::Add { path } => {
            let abs_path = path.canonicalize().unwrap_or(path);
            let path_str = abs_path.to_string_lossy().to_string();
            if current.add(path_str.clone()) {
                if let Err(e) = current.save_to(&config_path) {
                    eprintln!("Failed to save ignore list: {e}");
                    return ExitCode::FAILURE;
                }
                println!("Protected path: {path_str}");
            } else {
                println!("Path already in ignore list: {path_str}");
            }
        }
        IgnoreAction::Remove { path } => {
            let path_str = path.to_string_lossy();
            if current.remove(&path_str) {
                if let Err(e) = current.save_to(&config_path) {
                    eprintln!("Failed to save ignore list: {e}");
                    return ExitCode::FAILURE;
                }
                println!("Removed from ignore list: {path_str}");
            } else {
                println!("Path was not found in ignore list: {path_str}");
            }
        }
        IgnoreAction::List => {
            println!("Protected paths (ignore list):");
            if current.roots.is_empty() {
                println!("  (no paths protected)");
            } else {
                for r in &current.roots {
                    println!("  - {r}");
                }
            }
        }
    }

    ExitCode::SUCCESS
}

fn handle_config(args: &ConfigArgs, config: &Config) -> ExitCode {
    if let Some(ref app) = args.appearance {
        let normalized = app.to_lowercase();
        if !["auto", "dark", "light"].contains(&normalized.as_str()) {
            eprintln!("Error: Invalid appearance '{app}'. Expected 'auto', 'dark', or 'light'.");
            return ExitCode::FAILURE;
        }
        let mut updated = config.clone();
        updated.appearance = normalized;
        if let Some(config_path) = Config::default_path() {
            if let Err(e) = updated.save_to(&config_path) {
                eprintln!("Failed to save configuration: {e}");
                return ExitCode::FAILURE;
            }
            println!(
                "Updated appearance to '{}' in {}",
                updated.appearance,
                config_path.display()
            );
        }
        return ExitCode::SUCCESS;
    }

    println!("=== taert Configuration ===");
    if let Some(config_path) = Config::default_path() {
        println!("Config path: {}", config_path.display());
    }
    if let Some(ignore_path) = IgnoreStore::default_path() {
        println!("Ignore list: {}", ignore_path.display());
    }
    println!(
        "Default artifact age threshold: {} days",
        config.older_than_days
    );
    println!("Appearance: {}", config.appearance);
    if !config.extra_artifacts.is_empty() {
        println!("Custom artifacts: {:?}", config.extra_artifacts);
    }
    ExitCode::SUCCESS
}
