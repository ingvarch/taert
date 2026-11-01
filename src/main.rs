pub mod cli;
pub mod core;
pub mod plain;
pub mod ui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::{
    CleanArgs, CleanCategory, Cli, Command, ConfigArgs, DoctorArgs, HistoryAction, HistoryArgs,
    IgnoreAction, PurgeArgs, StartupArgs, UninstallArgs,
};
use crate::core::apps::{self, InstalledApp, Matching};
use crate::core::bytes::format_bytes;
use crate::core::cleaner::RemovalMode;
use crate::core::date_utils::{format_system_time, parse_rfc3339};
use crate::core::docker::DockerClient;
use crate::core::doctor;
use crate::core::history::{HistoryLog, ItemOutcome};
use crate::core::host::MacHost;
use crate::core::layout::Layout;
use crate::core::path_utils::prettify_path;
use crate::core::purger::Purger;
use crate::core::safety::Safety;
use crate::core::scanner::{CategoryScan, CategoryScanner};
use crate::core::sizer::DirSizer;
use crate::core::startup;
use crate::core::stores::{Config, IgnoreStore};
use crate::core::uninstaller::{self, PlanRequest, UninstallPlan, Uninstaller};
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
                let mut app = ui::app::App::new(ignore, config).with_permanent(cli.permanent);
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

        Some(Command::Docker) => handle_docker(&cli, use_plain, &ignore),

        Some(Command::Uninstall(ref args)) => handle_uninstall(args, &cli, use_plain, &ignore),

        Some(Command::Analyze(ref args)) => {
            handle_analyze(args.path.clone(), args.depth, &cli, use_plain)
        }

        Some(Command::Doctor(ref args)) => handle_doctor(args, &cli, use_plain, &ignore, &config),

        Some(Command::Startup(ref args)) => handle_startup(args, &cli, use_plain, &ignore, &config),

        Some(Command::Ignore(args)) => handle_ignore(args.action, &ignore),

        Some(Command::Config(ref args)) => handle_config(args, &config),

        Some(Command::History(ref args)) => handle_history(args, &cli),
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

    let command = match args.category {
        Some(CleanCategory::Dev) => "clean dev",
        Some(CleanCategory::Junk) => "clean junk",
        Some(CleanCategory::Ai) => "clean ai",
        Some(CleanCategory::Trash) => "clean trash",
        None => "clean",
    };

    let scans = categories
        .into_iter()
        .map(|(id, title)| CategoryScanner::scan(id, title, ignore, None, None))
        .collect();
    let mut scan = CategoryScan::combined("Clean Review", scans);

    process_scan_result(&mut scan, cli, use_plain, ignore, command)
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

    let command = format!("purge {}", prettify_path(&scan_root));
    let older_than = args.older_than.unwrap_or(config.older_than_days);
    let mut scan = Purger::scan_projects(&scan_root, older_than, &config.extra_artifacts, ignore);

    process_scan_result(&mut scan, cli, use_plain, ignore, &command)
}

fn handle_docker(cli: &Cli, use_plain: bool, ignore: &IgnoreStore) -> ExitCode {
    match DockerClient::connect().and_then(|client| client.scan()) {
        Ok(mut scan) => process_scan_result(&mut scan, cli, use_plain, ignore, "docker"),
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn process_scan_result(
    scan: &mut CategoryScan,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    command: &str,
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
        PlainRunner::print_scan_with_options(scan, cli.permanent);
        println!("\nDry run completed. No files were deleted.");
        return ExitCode::SUCCESS;
    }

    // 3. Force mode: unattended cleanup of default-selected items
    if cli.force {
        let total = scan.selected_items().len();
        let session = HistoryLog::default_log().start_run(command, "force", Some(total));
        let report = PlainRunner::execute_clean_with_callbacks(
            scan,
            ignore,
            Some(|outcome: &ItemOutcome| session.record_item(outcome)),
        );
        session.finish();
        PlainRunner::print_notes(&scan.notes);
        return if report.failures.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    // 4. Plain interactive mode or fallback
    if use_plain {
        PlainRunner::print_scan_with_options(scan, cli.permanent);
        if let Err(e) = PlainRunner::prompt_selection(scan) {
            eprintln!("Selection error: {e}");
            return ExitCode::FAILURE;
        }

        match PlainRunner::prompt_confirm(scan) {
            Ok(true) => {
                let total = scan.selected_items().len();
                let session =
                    HistoryLog::default_log().start_run(command, "interactive", Some(total));
                let report = PlainRunner::execute_clean_with_callbacks(
                    scan,
                    ignore,
                    Some(|outcome: &ItemOutcome| session.record_item(outcome)),
                );
                session.finish();
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
            ui::app::App::with_select(scan.clone(), ignore.clone(), Config::load_or_default())
                .with_permanent(cli.permanent);
        match app.run() {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("TUI Error: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

fn handle_uninstall(
    args: &UninstallArgs,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
) -> ExitCode {
    let layout = Layout::current();
    let mut installed = apps::find_apps(&layout);

    if args.names.is_empty() {
        if cli.force {
            eprintln!("Error: Name the apps to remove: taert uninstall <name>");
            return ExitCode::FAILURE;
        }
        return pick_apps_to_uninstall(&mut installed, cli, use_plain, ignore, &layout);
    }

    let matching = if cli.force {
        Matching::Exact
    } else {
        Matching::Partial
    };
    let mut selected = match apps::find_by_names(&installed, &args.names, matching) {
        Ok(selected) => selected,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::FAILURE;
        }
    };
    apps::measure_apps(&mut selected, &MacHost, None, None);
    let plan = plan_uninstall(&selected, &installed, ignore, &layout);
    finish_uninstall(plan, cli, use_plain, ignore, &layout)
}

fn plan_uninstall(
    selected: &[InstalledApp],
    installed: &[InstalledApp],
    ignore: &IgnoreStore,
    layout: &Layout,
) -> UninstallPlan {
    let request = PlanRequest {
        selected,
        installed,
        ignore,
        host: &MacHost,
        layout,
    };
    uninstaller::build_plan(&request, None, None)
}

fn pick_apps_to_uninstall(
    installed: &mut [InstalledApp],
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    layout: &Layout,
) -> ExitCode {
    if !use_plain && !cli.json && !cli.dry_run {
        let mut app = ui::app::App::new(ignore.clone(), Config::load_or_default())
            .with_permanent(cli.permanent);
        app.launch_app_scan();
        return run_tui(&mut app);
    }

    apps::measure_apps(installed, &MacHost, None, None);
    let mut list = apps::app_scan(installed, layout);

    if cli.json {
        return match serde_json::to_string_pretty(installed) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Failed to serialize JSON: {error}");
                ExitCode::FAILURE
            }
        };
    }

    PlainRunner::print_scan_with_options(&list, cli.permanent);
    if cli.dry_run {
        println!("\nDry run completed. No files were deleted.");
        return ExitCode::SUCCESS;
    }
    if let Err(error) = PlainRunner::prompt_selection(&mut list) {
        eprintln!("Selection error: {error}");
        return ExitCode::FAILURE;
    }

    let paths: Vec<&PathBuf> = list
        .selected_items()
        .iter()
        .map(|item| &item.path)
        .collect();
    let selected: Vec<InstalledApp> = installed
        .iter()
        .filter(|app| paths.contains(&&app.path))
        .cloned()
        .collect();
    if selected.is_empty() {
        println!("\nNothing selected for removal.");
        return ExitCode::SUCCESS;
    }
    let plan = plan_uninstall(&selected, installed, ignore, layout);
    finish_uninstall(plan, cli, use_plain, ignore, layout)
}

fn run_tui(app: &mut ui::app::App) -> ExitCode {
    match app.run() {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("TUI Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_uninstall(
    plan: &UninstallPlan,
    cli: &Cli,
    ignore: &IgnoreStore,
    layout: &Layout,
) -> ExitCode {
    let mode = if cli.permanent {
        RemovalMode::Permanent
    } else {
        RemovalMode::Trash
    };
    let run_mode = if cli.force { "force" } else { "interactive" };
    let app_names: Vec<_> = plan.apps.iter().map(|a| a.app.name.as_str()).collect();
    let command = format!("uninstall {}", app_names.join(", "));
    let total = plan.scan.selected_items().len();
    let session = HistoryLog::default_log().start_run(&command, run_mode, Some(total));
    let report = Uninstaller {
        host: &MacHost,
        layout,
        ignore,
        mode,
    }
    .run_with_outcome(plan, |_| {}, |outcome| session.record_item(outcome));
    session.finish();
    PlainRunner::print_report("Uninstall", &report);
    if report.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn finish_uninstall(
    mut plan: UninstallPlan,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    layout: &Layout,
) -> ExitCode {
    if plan.apps.is_empty() {
        for note in &plan.notes {
            eprintln!("Error: {note}");
        }
        return ExitCode::FAILURE;
    }

    if cli.json {
        return match serde_json::to_string_pretty(&plan) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Failed to serialize JSON: {error}");
                ExitCode::FAILURE
            }
        };
    }

    if cli.dry_run {
        PlainRunner::print_scan_with_options(&plan.scan, cli.permanent);
        PlainRunner::print_notes(&plan.notes);
        println!("\nDry run completed. No files were deleted.");
        return ExitCode::SUCCESS;
    }

    if cli.force {
        return run_uninstall(&plan, cli, ignore, layout);
    }

    if !use_plain {
        let mut app = ui::app::App::with_plan(plan, ignore.clone(), Config::load_or_default())
            .with_permanent(cli.permanent);
        return run_tui(&mut app);
    }

    PlainRunner::print_scan_with_options(&plan.scan, cli.permanent);
    PlainRunner::print_notes(&plan.notes);
    if let Err(error) = PlainRunner::prompt_selection(&mut plan.scan) {
        eprintln!("Selection error: {error}");
        return ExitCode::FAILURE;
    }
    match PlainRunner::prompt_confirm(&plan.scan) {
        Ok(true) => run_uninstall(&plan, cli, ignore, layout),
        Ok(false) => {
            println!("Operation cancelled.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("Confirmation error: {error}");
            ExitCode::FAILURE
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
        )
        .with_permanent(cli.permanent);
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

fn handle_doctor(
    args: &DoctorArgs,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    config: &Config,
) -> ExitCode {
    let host = MacHost;
    let layout = Layout::current();

    if cli.json {
        let snapshot = doctor::collect(&host, args.seconds, &layout, ignore);
        let report = doctor::diagnose(&snapshot);
        match serde_json::to_string_pretty(&report) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("JSON serialization error: {e}");
                ExitCode::FAILURE
            }
        }
    } else if use_plain {
        let snapshot = doctor::collect(&host, args.seconds, &layout, ignore);
        let report = doctor::diagnose(&snapshot);
        doctor::print_plain_report(&report);
        ExitCode::SUCCESS
    } else {
        let mut app = ui::app::App::new_doctor(ignore.clone(), config.clone(), args.seconds);
        match app.run() {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("TUI Error: {e}");
                ExitCode::FAILURE
            }
        }
    }
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

fn handle_startup(
    args: &StartupArgs,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    config: &Config,
) -> ExitCode {
    let host = MacHost;
    let layout = Layout::current();

    if let Some(ref action) = args.action {
        let (label, is_enable) = match action {
            cli::StartupAction::Disable { label } => (label, false),
            cli::StartupAction::Enable { label } => (label, true),
        };

        let scan = startup::scan_startup(&host, &layout, ignore, args.all);
        let Some(item) = scan
            .items
            .into_iter()
            .find(|it| it.label == *label || it.name == *label)
        else {
            eprintln!("Error: Startup item '{label}' not found");
            return ExitCode::FAILURE;
        };

        let result = if is_enable {
            startup::enable_item(&host, &layout, ignore, &item, cli.dry_run)
        } else {
            startup::disable_item(&host, &layout, ignore, &item, cli.dry_run)
        };

        match result {
            Ok(msg) => {
                println!("{msg}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("Error: {err}");
                ExitCode::FAILURE
            }
        }
    } else if cli.json {
        let scan = startup::scan_startup(&host, &layout, ignore, args.all);
        match serde_json::to_string_pretty(&scan.items) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("JSON serialization error: {e}");
                ExitCode::FAILURE
            }
        }
    } else if use_plain {
        let scan = startup::scan_startup(&host, &layout, ignore, args.all);
        println!("=== macOS Startup Items ===");
        if scan.items.is_empty() {
            println!("No startup items found.");
        } else {
            for item in &scan.items {
                let app_info = item
                    .app_name
                    .as_deref()
                    .map(|a| format!(" ({a})"))
                    .unwrap_or_default();
                let state_str = match item.state {
                    startup::StartupState::Running => {
                        if let (Some(cpu), Some(mem)) = (item.cpu_percent, item.memory_bytes) {
                            format!("running (CPU: {cpu:.1}%, Memory: {})", format_bytes(mem))
                        } else {
                            "running".to_string()
                        }
                    }
                    startup::StartupState::Enabled => "enabled".to_string(),
                    startup::StartupState::Disabled => "disabled".to_string(),
                    startup::StartupState::Broken => "broken (missing executable)".to_string(),
                };
                let prog_str = item
                    .program
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .or_else(|| {
                        item.plist_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().to_string())
                    })
                    .unwrap_or_else(|| "-".to_string());

                println!(
                    "- {}{app_info} [{}] -> {} ({prog_str})",
                    item.name,
                    item.kind.as_str(),
                    state_str
                );
            }
        }
        if !scan.notes.is_empty() {
            println!();
            for note in &scan.notes {
                println!("Note: {note}");
            }
        }
        ExitCode::SUCCESS
    } else {
        let mut app = ui::app::App::new_startup(ignore.clone(), config.clone(), args.all);
        match app.run() {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("TUI Error: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

fn handle_history(args: &HistoryArgs, cli: &Cli) -> ExitCode {
    let log = HistoryLog::default_log();
    let mut runs = match log.load_runs() {
        Ok(runs) => runs,
        Err(err) => {
            eprintln!("Error reading history: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(HistoryAction::Show { ref run }) = args.action {
        // Find matching run: exact match first, then unique prefix match
        let matched = if let Some(exact) = runs.iter().find(|r| r.run == *run) {
            Some(exact.clone())
        } else {
            let prefixes: Vec<_> = runs.iter().filter(|r| r.run.starts_with(run)).collect();
            if prefixes.len() == 1 {
                Some(prefixes[0].clone())
            } else if prefixes.len() > 1 {
                eprintln!("Error: Ambiguous run ID '{run}'. Matching runs:");
                for r in prefixes {
                    eprintln!("  {}", r.run);
                }
                return ExitCode::FAILURE;
            } else {
                None
            }
        };

        let Some(run_record) = matched else {
            eprintln!("Error: Run '{run}' not found");
            return ExitCode::FAILURE;
        };

        if cli.json {
            match serde_json::to_string_pretty(&run_record) {
                Ok(json) => {
                    println!("{json}");
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("Failed to serialize JSON: {err}");
                    ExitCode::FAILURE
                }
            }
        } else {
            println!("Run:     {}", run_record.run);
            let date_str = if let Some(time) = parse_rfc3339(&run_record.at) {
                format_system_time(time)
            } else {
                run_record.at.clone()
            };
            println!("Date:    {date_str}");
            println!("Command: {}", run_record.command);
            println!("Mode:    {}", run_record.mode);
            let status = if run_record.is_interrupted() {
                "interrupted"
            } else {
                "completed"
            };
            println!("Status:  {status}");
            println!();
            if run_record.items.is_empty() {
                println!("No items recorded.");
            } else {
                for item in &run_record.items {
                    let err_suffix = if let Some(ref err) = item.error {
                        format!(": {err}")
                    } else {
                        String::new()
                    };
                    println!(
                        "  {:<9} {:>9}  {}{}",
                        item.outcome.to_string(),
                        format_bytes(item.bytes),
                        item.path,
                        err_suffix
                    );
                }
            }
            ExitCode::SUCCESS
        }
    } else {
        // Newest run first
        runs.reverse();
        if let Some(limit) = args.limit {
            runs.truncate(limit);
        }

        if cli.json {
            match serde_json::to_string_pretty(&runs) {
                Ok(json) => {
                    println!("{json}");
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("Failed to serialize JSON: {err}");
                    ExitCode::FAILURE
                }
            }
        } else if runs.is_empty() {
            println!("No history found.");
            ExitCode::SUCCESS
        } else {
            for run in &runs {
                println!("{}", run.format_line());
            }
            ExitCode::SUCCESS
        }
    }
}
