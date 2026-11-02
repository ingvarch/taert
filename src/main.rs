pub mod cli;
pub mod core;
pub mod plain;
pub mod ui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::{
    CleanArgs, CleanCategory, Cli, Command, CompressArgs, ConfigArgs, DoctorArgs, HistoryAction,
    HistoryArgs, IgnoreAction, LargeArgs, PurgeArgs, ScheduleAction, ScheduleArgs, SlimArgs,
    StartupArgs, UninstallArgs,
};
use crate::core::apps::{self, InstalledApp, Matching};
use crate::core::bytes::format_bytes;
use crate::core::cleaner::{RemovalMode, RemovalReport};
use crate::core::date_utils::{format_system_time, parse_rfc3339};
use crate::core::docker::DockerClient;
use crate::core::doctor;
use crate::core::history::HistoryLog;
use crate::core::host::{Host, MacHost};
use crate::core::large_files;
use crate::core::layout::Layout;
use crate::core::path_utils::{expand_tilde, prettify_path};
use crate::core::purger::Purger;
use crate::core::safety::Safety;
use crate::core::scanner::{CategoryScan, CategoryScanner, ScanContext};
use crate::core::schedule;
use crate::core::sizer::DirSizer;
use crate::core::startup;
use crate::core::stores::{Config, IgnoreStore};
use crate::core::uninstaller::{self, PlanRequest, UninstallPlan, Uninstaller};
use crate::plain::{Console, PlainRunner};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let is_tty = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();

    // Plain mode is forced if explicitly specified or if not running in an interactive TTY
    let use_plain = cli.plain || !is_tty;

    let layout = Layout::current();
    let ignore = saved_ignore_list(&layout);
    let config = saved_config(&layout);
    let context = ScanContext {
        layout: &layout,
        host: &MacHost,
    };
    let mut input = std::io::stdin().lock();
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let console = &mut Console {
        input: &mut input,
        out: &mut out,
        err: &mut err,
    };

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
                run_tui(&mut app, console)
            }
        }

        Some(Command::Clean(ref args)) => {
            handle_clean(args, &cli, use_plain, &ignore, &context, console)
        }

        Some(Command::Purge(ref args)) => {
            handle_purge(args, &cli, use_plain, &ignore, &config, &context, console)
        }

        Some(Command::Docker) => handle_docker(&cli, use_plain, &ignore, &context, console),

        Some(Command::Uninstall(ref args)) => {
            handle_uninstall(args, &cli, use_plain, &ignore, &context, console)
        }

        Some(Command::Compress(ref args)) => handle_compress(args, &cli, &ignore),

        Some(Command::Slim(ref args)) => {
            handle_slim(args, &cli, use_plain, &ignore, &context, console)
        }

        Some(Command::Analyze(ref args)) => handle_analyze(
            args.path.clone(),
            args.depth,
            &cli,
            use_plain,
            &ignore,
            &config,
        ),

        Some(Command::Large(ref args)) => {
            handle_large(args, &cli, use_plain, &ignore, &context, console)
        }

        Some(Command::Doctor(ref args)) => {
            handle_doctor(args, &cli, use_plain, &ignore, &config, &context, console)
        }

        Some(Command::Startup(ref args)) => {
            handle_startup(args, &cli, use_plain, &ignore, &config, &context, console)
        }

        Some(Command::Ignore(args)) => handle_ignore(args.action, &ignore, &layout, console),

        Some(Command::Config(ref args)) => handle_config(args, &config, &layout, console),

        Some(Command::History(ref args)) => handle_history(args, &cli, &layout, console),

        Some(Command::Schedule(ref args)) => {
            handle_schedule(args, &config, &MacHost, &layout, console)
        }
    }
}

fn saved_config(layout: &Layout) -> Config {
    Config::load_from(&layout.config_path()).unwrap_or_default()
}

fn saved_ignore_list(layout: &Layout) -> IgnoreStore {
    IgnoreStore::load_from(&layout.ignore_path()).unwrap_or_default()
}

fn handle_clean(
    args: &CleanArgs,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
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
        .map(|(id, title)| CategoryScanner::scan(context, id, title, ignore, None, None))
        .collect();
    let mut scan = CategoryScan::combined("Clean Review", scans);

    process_scan_result(&mut scan, cli, use_plain, ignore, command, context, console)
}

fn handle_purge(
    args: &PurgeArgs,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    config: &Config,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    let scan_root = match Safety::validate_scan_root(&args.path) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(
                console.err,
                "Error: Cannot scan path '{}': {}",
                args.path.display(),
                e
            );
            return ExitCode::FAILURE;
        }
    };

    let command = format!("purge {}", prettify_path(&scan_root));
    let older_than = args.older_than.unwrap_or(config.older_than_days);
    let mut scan = Purger::scan_projects(&scan_root, older_than, &config.extra_artifacts, ignore);

    process_scan_result(
        &mut scan, cli, use_plain, ignore, &command, context, console,
    )
}

fn handle_docker(
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    match DockerClient::connect().and_then(|client| client.scan()) {
        Ok(mut scan) => process_scan_result(
            &mut scan, cli, use_plain, ignore, "docker", context, console,
        ),
        Err(error) => {
            let _ = writeln!(console.err, "Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn handle_large(
    args: &LargeArgs,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    if cli.force {
        let _ = writeln!(
            console.err,
            "Error: personal files must never be removed unreviewed"
        );
        return ExitCode::FAILURE;
    }

    let root = match &args.path {
        Some(p) => expand_tilde(&p.to_string_lossy()),
        None => context.layout.home.clone(),
    };

    if !root.exists() {
        let _ = writeln!(
            console.err,
            "Error: Path '{}' does not exist",
            root.display()
        );
        return ExitCode::FAILURE;
    }

    let command = format!("large {}", prettify_path(&root));
    let options = large_files::LargeScanOptions::new(args.min_size, args.older_than, args.limit);
    let mut scan = large_files::scan(&root, &options, context.host, ignore, None, None);

    process_scan_result(
        &mut scan, cli, use_plain, ignore, &command, context, console,
    )
}

/// Removes the selected rows of `scan` and logs the run to the history of the layout.
fn clean_and_log(
    scan: &CategoryScan,
    cli: &Cli,
    ignore: &IgnoreStore,
    command: &str,
    run_mode: &str,
    context: &ScanContext,
) -> RemovalReport {
    let total = scan.selected_items().len();
    let session =
        HistoryLog::new(context.layout.history_path()).start_run(command, run_mode, Some(total));
    let report = PlainRunner::execute_clean(scan, ignore, cli.permanent, context.host, |outcome| {
        session.record_item(outcome)
    });
    session.finish();
    report
}

fn process_scan_result(
    scan: &mut CategoryScan,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    command: &str,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    // 1. JSON mode: output JSON and exit immediately without modifying files
    if cli.json {
        match serde_json::to_string_pretty(scan) {
            Ok(json_str) => {
                let _ = writeln!(console.out, "{json_str}");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                let _ = writeln!(console.err, "Failed to serialize JSON: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // 2. Dry-run mode: display scan and exit without deleting
    if cli.dry_run {
        let _ = write!(
            console.out,
            "{}",
            PlainRunner::scan_text_with_options(scan, cli.permanent)
        );
        let _ = writeln!(console.out, "\nDry run completed. No files were deleted.");
        return ExitCode::SUCCESS;
    }

    // 3. Force mode: unattended cleanup of default-selected items
    if cli.force {
        let report = clean_and_log(scan, cli, ignore, command, "force", context);
        let _ = PlainRunner::write_report("Clean", &report, console);
        let _ = write!(console.out, "{}", PlainRunner::notes_text(&scan.notes));
        return if report.failures.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    // 4. Plain interactive mode or fallback
    if use_plain {
        let _ = write!(
            console.out,
            "{}",
            PlainRunner::scan_text_with_options(scan, cli.permanent)
        );
        if let Err(e) = PlainRunner::prompt_selection(scan, console) {
            let _ = writeln!(console.err, "Selection error: {e}");
            return ExitCode::FAILURE;
        }

        match PlainRunner::prompt_confirm(scan, console) {
            Ok(true) => {
                let report = clean_and_log(scan, cli, ignore, command, "interactive", context);
                let _ = PlainRunner::write_report("Clean", &report, console);
                if report.failures.is_empty() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Ok(false) => {
                let _ = writeln!(console.out, "Operation cancelled.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                let _ = writeln!(console.err, "Confirmation error: {e}");
                ExitCode::FAILURE
            }
        }
    } else {
        // Launch TUI with pre-scanned items
        let mut app =
            ui::app::App::with_select(scan.clone(), ignore.clone(), saved_config(context.layout))
                .with_permanent(cli.permanent);
        run_tui(&mut app, console)
    }
}

fn handle_uninstall(
    args: &UninstallArgs,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    let mut installed = apps::find_apps(context.layout);

    if args.names.is_empty() {
        if cli.force {
            let _ = writeln!(
                console.err,
                "Error: Name the apps to remove: taert uninstall <name>"
            );
            return ExitCode::FAILURE;
        }
        return pick_apps_to_uninstall(&mut installed, cli, use_plain, ignore, context, console);
    }

    let matching = if cli.force {
        Matching::Exact
    } else {
        Matching::Partial
    };
    let mut selected = match apps::find_by_names(&installed, &args.names, matching) {
        Ok(selected) => selected,
        Err(error) => {
            let _ = writeln!(console.err, "Error: {error}");
            return ExitCode::FAILURE;
        }
    };
    apps::measure_apps(&mut selected, context.host, None, None);
    let plan = plan_uninstall(&selected, &installed, ignore, context);
    finish_uninstall(plan, cli, use_plain, ignore, context, console)
}

fn plan_uninstall(
    selected: &[InstalledApp],
    installed: &[InstalledApp],
    ignore: &IgnoreStore,
    context: &ScanContext,
) -> UninstallPlan {
    let request = PlanRequest {
        selected,
        installed,
        ignore,
        host: context.host,
        layout: context.layout,
    };
    uninstaller::build_plan(&request, None, None)
}

fn pick_apps_to_uninstall(
    installed: &mut [InstalledApp],
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    if !use_plain && !cli.json && !cli.dry_run {
        let mut app = ui::app::App::new(ignore.clone(), saved_config(context.layout))
            .with_permanent(cli.permanent);
        app.launch_app_scan();
        return run_tui(&mut app, console);
    }

    apps::measure_apps(installed, context.host, None, None);
    let mut list = apps::app_scan(installed, context.layout);

    if cli.json {
        return match serde_json::to_string_pretty(installed) {
            Ok(json) => {
                let _ = writeln!(console.out, "{json}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                let _ = writeln!(console.err, "Failed to serialize JSON: {error}");
                ExitCode::FAILURE
            }
        };
    }

    let _ = write!(
        console.out,
        "{}",
        PlainRunner::scan_text_with_options(&list, cli.permanent)
    );
    if cli.dry_run {
        let _ = writeln!(console.out, "\nDry run completed. No files were deleted.");
        return ExitCode::SUCCESS;
    }
    if let Err(error) = PlainRunner::prompt_selection(&mut list, console) {
        let _ = writeln!(console.err, "Selection error: {error}");
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
        let _ = writeln!(console.out, "\nNothing selected for removal.");
        return ExitCode::SUCCESS;
    }
    let plan = plan_uninstall(&selected, installed, ignore, context);
    finish_uninstall(plan, cli, use_plain, ignore, context, console)
}

fn run_tui(app: &mut ui::app::App, console: &mut Console) -> ExitCode {
    match app.run() {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(console.err, "TUI Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_uninstall(
    plan: &UninstallPlan,
    cli: &Cli,
    ignore: &IgnoreStore,
    context: &ScanContext,
    console: &mut Console,
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
    let session =
        HistoryLog::new(context.layout.history_path()).start_run(&command, run_mode, Some(total));
    let report = Uninstaller {
        host: context.host,
        layout: context.layout,
        ignore,
        mode,
    }
    .run_with_outcome(plan, |_| {}, |outcome| session.record_item(outcome));
    session.finish();
    let _ = PlainRunner::write_report("Uninstall", &report, console);
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
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    if plan.apps.is_empty() {
        for note in &plan.notes {
            let _ = writeln!(console.err, "Error: {note}");
        }
        return ExitCode::FAILURE;
    }

    if cli.json {
        return match serde_json::to_string_pretty(&plan) {
            Ok(json) => {
                let _ = writeln!(console.out, "{json}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                let _ = writeln!(console.err, "Failed to serialize JSON: {error}");
                ExitCode::FAILURE
            }
        };
    }

    if cli.dry_run {
        let _ = write!(
            console.out,
            "{}",
            PlainRunner::scan_text_with_options(&plan.scan, cli.permanent)
        );
        let _ = write!(console.out, "{}", PlainRunner::notes_text(&plan.notes));
        let _ = writeln!(console.out, "\nDry run completed. No files were deleted.");
        return ExitCode::SUCCESS;
    }

    if cli.force {
        return run_uninstall(&plan, cli, ignore, context, console);
    }

    if !use_plain {
        let mut app = ui::app::App::with_plan(plan, ignore.clone(), saved_config(context.layout))
            .with_permanent(cli.permanent);
        return run_tui(&mut app, console);
    }

    let _ = write!(
        console.out,
        "{}",
        PlainRunner::scan_text_with_options(&plan.scan, cli.permanent)
    );
    let _ = write!(console.out, "{}", PlainRunner::notes_text(&plan.notes));
    if let Err(error) = PlainRunner::prompt_selection(&mut plan.scan, console) {
        let _ = writeln!(console.err, "Selection error: {error}");
        return ExitCode::FAILURE;
    }
    match PlainRunner::prompt_confirm(&plan.scan, console) {
        Ok(true) => run_uninstall(&plan, cli, ignore, context, console),
        Ok(false) => {
            let _ = writeln!(console.out, "Operation cancelled.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(console.err, "Confirmation error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn handle_analyze(
    path: PathBuf,
    depth: usize,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    config: &Config,
) -> ExitCode {
    let scan_root = match Safety::validate_scan_root(&path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: Cannot analyze '{}': {}", path.display(), e);
            return ExitCode::FAILURE;
        }
    };

    if !use_plain && !cli.json {
        let mut app = ui::app::App::with_analyze(scan_root, ignore.clone(), config.clone())
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
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    if cli.json {
        let snapshot = doctor::collect(context.host, args.seconds, context.layout, ignore);
        let report = doctor::diagnose(&snapshot);
        match serde_json::to_string_pretty(&report) {
            Ok(json) => {
                let _ = writeln!(console.out, "{json}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                let _ = writeln!(console.err, "JSON serialization error: {e}");
                ExitCode::FAILURE
            }
        }
    } else if use_plain {
        let snapshot = doctor::collect(context.host, args.seconds, context.layout, ignore);
        let report = doctor::diagnose(&snapshot);
        let _ = write!(console.out, "{}", doctor::plain_report(&report));
        ExitCode::SUCCESS
    } else {
        let mut app = ui::app::App::new_doctor(ignore.clone(), config.clone(), args.seconds);
        run_tui(&mut app, console)
    }
}

fn handle_ignore(
    action: IgnoreAction,
    ignore: &IgnoreStore,
    layout: &Layout,
    console: &mut Console,
) -> ExitCode {
    let mut current = ignore.clone();
    let ignore_path = layout.ignore_path();
    let out = &mut *console.out;

    match action {
        IgnoreAction::Add { path } => {
            let abs_path = path.canonicalize().unwrap_or(path);
            let path_str = abs_path.to_string_lossy().to_string();
            if current.add(path_str.clone()) {
                if let Err(e) = current.save_to(&ignore_path) {
                    let _ = writeln!(console.err, "Failed to save ignore list: {e}");
                    return ExitCode::FAILURE;
                }
                let _ = writeln!(out, "Protected path: {path_str}");
            } else {
                let _ = writeln!(out, "Path already in ignore list: {path_str}");
            }
        }
        IgnoreAction::Remove { path } => {
            let path_str = path.to_string_lossy();
            if current.remove(&path_str) {
                if let Err(e) = current.save_to(&ignore_path) {
                    let _ = writeln!(console.err, "Failed to save ignore list: {e}");
                    return ExitCode::FAILURE;
                }
                let _ = writeln!(out, "Removed from ignore list: {path_str}");
            } else {
                let _ = writeln!(out, "Path was not found in ignore list: {path_str}");
            }
        }
        IgnoreAction::List => {
            let _ = writeln!(out, "Protected paths (ignore list):");
            if current.roots.is_empty() {
                let _ = writeln!(out, "  (no paths protected)");
            } else {
                for r in &current.roots {
                    let _ = writeln!(out, "  - {r}");
                }
            }
        }
    }

    ExitCode::SUCCESS
}

fn handle_config(
    args: &ConfigArgs,
    config: &Config,
    layout: &Layout,
    console: &mut Console,
) -> ExitCode {
    let config_path = layout.config_path();
    let out = &mut *console.out;
    if let Some(ref app) = args.appearance {
        let normalized = app.to_lowercase();
        if !["auto", "dark", "light"].contains(&normalized.as_str()) {
            let _ = writeln!(
                console.err,
                "Error: Invalid appearance '{app}'. Expected 'auto', 'dark', or 'light'."
            );
            return ExitCode::FAILURE;
        }
        let mut updated = config.clone();
        updated.appearance = normalized;
        if let Err(e) = updated.save_to(&config_path) {
            let _ = writeln!(console.err, "Failed to save configuration: {e}");
            return ExitCode::FAILURE;
        }
        let _ = writeln!(
            out,
            "Updated appearance to '{}' in {}",
            updated.appearance,
            config_path.display()
        );
        return ExitCode::SUCCESS;
    }

    let _ = writeln!(out, "=== taert Configuration ===");
    let _ = writeln!(out, "Config path: {}", config_path.display());
    let _ = writeln!(out, "Ignore list: {}", layout.ignore_path().display());
    let _ = writeln!(
        out,
        "Default artifact age threshold: {} days",
        config.older_than_days
    );
    let _ = writeln!(out, "Appearance: {}", config.appearance);
    if !config.extra_artifacts.is_empty() {
        let _ = writeln!(out, "Custom artifacts: {:?}", config.extra_artifacts);
    }
    ExitCode::SUCCESS
}

fn handle_startup(
    args: &StartupArgs,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    config: &Config,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    let ScanContext { layout, host } = *context;
    let out = &mut *console.out;
    if let Some(ref action) = args.action {
        let (label, is_enable) = match action {
            cli::StartupAction::Disable { label } => (label, false),
            cli::StartupAction::Enable { label } => (label, true),
        };

        let scan = startup::scan_startup(host, layout, ignore, args.all);
        let Some(item) = scan
            .items
            .into_iter()
            .find(|it| it.label == *label || it.name == *label)
        else {
            let _ = writeln!(console.err, "Error: Startup item '{label}' not found");
            return ExitCode::FAILURE;
        };

        let result = if is_enable {
            startup::enable_item(host, layout, ignore, &item, cli.dry_run)
        } else {
            startup::disable_item(host, layout, ignore, &item, cli.dry_run)
        };

        match result {
            Ok(msg) => {
                let _ = writeln!(out, "{msg}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                let _ = writeln!(console.err, "Error: {err}");
                ExitCode::FAILURE
            }
        }
    } else if cli.json {
        let scan = startup::scan_startup(host, layout, ignore, args.all);
        match serde_json::to_string_pretty(&scan.items) {
            Ok(json) => {
                let _ = writeln!(out, "{json}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                let _ = writeln!(console.err, "JSON serialization error: {e}");
                ExitCode::FAILURE
            }
        }
    } else if use_plain {
        let scan = startup::scan_startup(host, layout, ignore, args.all);
        let _ = writeln!(out, "=== macOS Startup Items ===");
        if scan.items.is_empty() {
            let _ = writeln!(out, "No startup items found.");
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

                let _ = writeln!(
                    out,
                    "- {}{app_info} [{}] -> {} ({prog_str})",
                    item.name,
                    item.kind.as_str(),
                    state_str
                );
            }
        }
        if !scan.notes.is_empty() {
            let _ = writeln!(out,);
            for note in &scan.notes {
                let _ = writeln!(out, "Note: {note}");
            }
        }
        ExitCode::SUCCESS
    } else {
        let mut app = ui::app::App::new_startup(ignore.clone(), config.clone(), args.all);
        run_tui(&mut app, console)
    }
}

fn handle_history(
    args: &HistoryArgs,
    cli: &Cli,
    layout: &Layout,
    console: &mut Console,
) -> ExitCode {
    let log = HistoryLog::new(layout.history_path());
    let (out, err) = (&mut *console.out, &mut *console.err);
    let mut runs = match log.load_runs() {
        Ok(runs) => runs,
        Err(error) => {
            let _ = writeln!(err, "Error reading history: {error}");
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
                let _ = writeln!(err, "Error: Ambiguous run ID '{run}'. Matching runs:");
                for r in prefixes {
                    let _ = writeln!(err, "  {}", r.run);
                }
                return ExitCode::FAILURE;
            } else {
                None
            }
        };

        let Some(run_record) = matched else {
            let _ = writeln!(err, "Error: Run '{run}' not found");
            return ExitCode::FAILURE;
        };

        if cli.json {
            match serde_json::to_string_pretty(&run_record) {
                Ok(json) => {
                    let _ = writeln!(out, "{json}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(err, "Failed to serialize JSON: {error}");
                    ExitCode::FAILURE
                }
            }
        } else {
            let _ = writeln!(out, "Run:     {}", run_record.run);
            let date_str = if let Some(time) = parse_rfc3339(&run_record.at) {
                format_system_time(time)
            } else {
                run_record.at.clone()
            };
            let _ = writeln!(out, "Date:    {date_str}");
            let _ = writeln!(out, "Command: {}", run_record.command);
            let _ = writeln!(out, "Mode:    {}", run_record.mode);
            let status = if run_record.is_interrupted() {
                "interrupted"
            } else {
                "completed"
            };
            let _ = writeln!(out, "Status:  {status}");
            let _ = writeln!(out);
            if run_record.items.is_empty() {
                let _ = writeln!(out, "No items recorded.");
            } else {
                for item in &run_record.items {
                    let err_suffix = if let Some(ref err) = item.error {
                        format!(": {err}")
                    } else {
                        String::new()
                    };
                    let _ = writeln!(
                        out,
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
                    let _ = writeln!(out, "{json}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(err, "Failed to serialize JSON: {error}");
                    ExitCode::FAILURE
                }
            }
        } else if runs.is_empty() {
            let _ = writeln!(out, "No history found.");
            ExitCode::SUCCESS
        } else {
            for run in &runs {
                let _ = writeln!(out, "{}", run.format_line());
            }
            ExitCode::SUCCESS
        }
    }
}

fn handle_schedule(
    args: &ScheduleArgs,
    config: &Config,
    host: &dyn Host,
    layout: &Layout,
    console: &mut Console,
) -> ExitCode {
    let mut config = config.clone();
    let out = &mut *console.out;

    match args.action {
        None | Some(ScheduleAction::Show) => {
            let history = HistoryLog::new(layout.history_path());
            let text = schedule::show_schedule(host, layout, &config, &history);
            let _ = write!(out, "{text}");
            ExitCode::SUCCESS
        }
        Some(ScheduleAction::Set(ref set_args)) => {
            let categories: Vec<String> = set_args
                .categories
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            match schedule::set_schedule(
                host,
                layout,
                &mut config,
                set_args.every,
                categories,
                set_args.min_reclaim,
                None,
            ) {
                Ok(()) => {
                    let history = HistoryLog::new(layout.history_path());
                    let text = schedule::show_schedule(host, layout, &config, &history);
                    let _ = write!(out, "{text}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    let _ = writeln!(console.err, "Error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(ScheduleAction::Off) => match schedule::remove_schedule(host, layout, &mut config) {
            Ok(()) => {
                let _ = writeln!(out, "Scheduled cleanup turned off.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                let _ = writeln!(console.err, "Error: {e}");
                ExitCode::FAILURE
            }
        },
        Some(ScheduleAction::Run) => {
            match schedule::run_scheduled(host, layout, &mut config, None) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::FAILURE,
            }
        }
    }
}

fn handle_compress(args: &CompressArgs, cli: &Cli, ignore: &IgnoreStore) -> ExitCode {
    let layout = Layout::current();
    if let Err(e) = Safety::can_compress(&args.path, ignore, &layout) {
        if cli.json {
            let error_json = serde_json::json!({
                "error": e.to_string(),
                "path": args.path,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&error_json).unwrap_or_default()
            );
        } else {
            eprintln!("Error: {e}");
        }
        return ExitCode::FAILURE;
    }

    let options = crate::core::compress::CompressOptions {
        dry_run: cli.dry_run,
        undo: args.undo,
    };
    let cancelled = std::sync::atomic::AtomicBool::new(false);

    let report =
        crate::core::compress::process_path(&args.path, &options, ignore, &cancelled, |_, _| {});

    if cli.json {
        match serde_json::to_string_pretty(&report) {
            Ok(json) => {
                println!("{json}");
                if report.failures.is_empty() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(e) => {
                eprintln!("Failed to serialize JSON: {e}");
                ExitCode::FAILURE
            }
        }
    } else {
        PlainRunner::print_compress_report(&report, &args.path);
        if report.failures.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }
}

fn handle_slim(
    args: &SlimArgs,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    let ScanContext { layout, host } = *context;
    if !crate::core::slim::is_apple_silicon(host) {
        let _ = writeln!(
            console.err,
            "Slimming universal apps requires an Apple Silicon Mac."
        );
        return ExitCode::FAILURE;
    }

    let skip_developer_id = std::env::var("TAERT_SLIM_ALLOW_ADHOC").is_ok();
    if args.names.is_empty() {
        return pick_apps_to_slim(cli, use_plain, ignore, context, skip_developer_id, console);
    }

    let installed = apps::find_apps(layout);
    let matching = if cli.force {
        Matching::Exact
    } else {
        Matching::Partial
    };
    let selected = match apps::find_by_names(&installed, &args.names, matching) {
        Ok(selected) => selected,
        Err(error) => {
            let _ = writeln!(console.err, "Error: {error}");
            return ExitCode::FAILURE;
        }
    };

    let rosetta = crate::core::slim::rosetta_bundle_ids(layout);
    let running = crate::core::slim::running_processes(host);

    for app in &selected {
        let binaries = match crate::core::slim::check_eligibility(
            app,
            host,
            layout,
            ignore,
            &rosetta,
            Some(&running),
            skip_developer_id,
            true,
        ) {
            Ok(b) => b,
            Err(reason) => {
                if cli.json {
                    let err = serde_json::json!({
                        "error": reason,
                        "app": app.name,
                    });
                    let _ = writeln!(
                        console.out,
                        "{}",
                        serde_json::to_string_pretty(&err).unwrap_or_default()
                    );
                } else {
                    let _ = writeln!(
                        console.err,
                        "Error: {} cannot be slimmed: {reason}",
                        app.name
                    );
                }
                return ExitCode::FAILURE;
            }
        };

        let intel_size: u64 = binaries
            .iter()
            .filter_map(|p| {
                crate::core::slim::parse_fat_header_from_file(p)
                    .ok()
                    .flatten()
            })
            .map(|h| {
                h.slices
                    .iter()
                    .filter(|s| {
                        s.cputype == crate::core::slim::CPU_TYPE_X86_64
                            || s.cputype == crate::core::slim::CPU_TYPE_I386
                    })
                    .map(|s| s.size)
                    .sum::<u64>()
            })
            .sum();

        if cli.dry_run {
            if cli.json {
                let info = serde_json::json!({
                    "dry_run": true,
                    "name": app.name,
                    "path": app.path,
                    "intel_bytes": intel_size,
                });
                let _ = writeln!(
                    console.out,
                    "{}",
                    serde_json::to_string_pretty(&info).unwrap_or_default()
                );
            } else {
                let _ = writeln!(
                    console.out,
                    "Would slim {}, saving {} of Intel code. Original will go to Trash.",
                    app.name,
                    format_bytes(intel_size)
                );
                let _ = writeln!(
                    console.out,
                    "Clones share blocks, so the space comes back when the Trash is emptied."
                );
            }
            continue;
        }

        if !cli.force && use_plain {
            let _ = write!(console.out, "Slim {}? [y/N]: ", app.name);
            let _ = console.out.flush();
            let mut input = String::new();
            if console.input.read_line(&mut input).is_err()
                || !input.trim().eq_ignore_ascii_case("y")
            {
                let _ = writeln!(console.out, "Cancelled.");
                return ExitCode::SUCCESS;
            }
        }

        match crate::core::slim::slim_app(app, &binaries, host, skip_developer_id) {
            Ok(saved) => {
                if cli.json {
                    let report = serde_json::json!({
                        "name": app.name,
                        "path": app.path,
                        "saved_bytes": saved,
                        "success": true,
                    });
                    let _ = writeln!(
                        console.out,
                        "{}",
                        serde_json::to_string_pretty(&report).unwrap_or_default()
                    );
                } else {
                    let _ = writeln!(
                        console.out,
                        "Slimmed {}, saved {} of Intel code. Original was moved to Trash.",
                        app.name,
                        format_bytes(saved)
                    );
                    let _ = writeln!(
                        console.out,
                        "Clones share blocks, so the space comes back when the Trash is emptied."
                    );
                }
            }
            Err((step, error)) => {
                if cli.json {
                    let err = serde_json::json!({
                        "name": app.name,
                        "path": app.path,
                        "step": step,
                        "error": error,
                        "success": false,
                    });
                    let _ = writeln!(
                        console.out,
                        "{}",
                        serde_json::to_string_pretty(&err).unwrap_or_default()
                    );
                } else {
                    let _ = writeln!(
                        console.err,
                        "Error: Slimming {} failed at step {step}: {error}",
                        app.name
                    );
                }
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}

fn pick_apps_to_slim(
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    context: &ScanContext,
    skip_developer_id: bool,
    console: &mut Console,
) -> ExitCode {
    let ScanContext { layout, host } = *context;
    if !use_plain && !cli.json && !cli.dry_run {
        let mut app =
            ui::app::App::new(ignore.clone(), saved_config(layout)).with_permanent(cli.permanent);
        app.launch_slim_scan();
        return run_tui(&mut app, console);
    }

    let mut scan = crate::core::slim::slim_scan(layout, host, ignore, None, None);

    if cli.json {
        let installed = apps::find_apps(layout);
        let rosetta = crate::core::slim::rosetta_bundle_ids(layout);
        let running = crate::core::slim::running_processes(host);

        let infos: Vec<crate::core::slim::SlimAppInfo> = installed
            .into_iter()
            .map(|app| {
                match crate::core::slim::check_eligibility(
                    &app,
                    host,
                    layout,
                    ignore,
                    &rosetta,
                    Some(&running),
                    skip_developer_id,
                    false,
                ) {
                    Ok(binaries) => {
                        let intel_bytes: u64 = binaries
                            .iter()
                            .filter_map(|p| {
                                crate::core::slim::parse_fat_header_from_file(p)
                                    .ok()
                                    .flatten()
                            })
                            .map(|h| {
                                h.slices
                                    .iter()
                                    .filter(|s| {
                                        s.cputype == crate::core::slim::CPU_TYPE_X86_64
                                            || s.cputype == crate::core::slim::CPU_TYPE_I386
                                    })
                                    .map(|s| s.size)
                                    .sum::<u64>()
                            })
                            .sum();
                        crate::core::slim::SlimAppInfo {
                            name: app.name,
                            path: app.path,
                            bundle_id: app.bundle_id,
                            intel_bytes,
                            eligible: true,
                            reason: None,
                        }
                    }
                    Err(reason) => crate::core::slim::SlimAppInfo {
                        name: app.name,
                        path: app.path,
                        bundle_id: app.bundle_id,
                        intel_bytes: 0,
                        eligible: false,
                        reason: Some(reason),
                    },
                }
            })
            .collect();

        return match serde_json::to_string_pretty(&infos) {
            Ok(json) => {
                let _ = writeln!(console.out, "{json}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                let _ = writeln!(console.err, "Failed to serialize JSON: {error}");
                ExitCode::FAILURE
            }
        };
    }

    let _ = write!(
        console.out,
        "{}",
        PlainRunner::scan_text_with_options(&scan, false)
    );
    if cli.dry_run {
        let _ = writeln!(console.out, "\nDry run completed. No apps were modified.");
        return ExitCode::SUCCESS;
    }

    if let Err(error) = PlainRunner::prompt_selection(&mut scan, console) {
        let _ = writeln!(console.err, "Selection error: {error}");
        return ExitCode::FAILURE;
    }

    let selected_paths: Vec<_> = scan
        .selected_items()
        .iter()
        .map(|item| item.path.clone())
        .collect();

    if selected_paths.is_empty() {
        let _ = writeln!(console.out, "\nNothing selected for slimming.");
        return ExitCode::SUCCESS;
    }

    let installed = apps::find_apps(layout);
    let chosen: Vec<InstalledApp> = installed
        .into_iter()
        .filter(|app| selected_paths.contains(&app.path))
        .collect();

    let mut total_saved = 0u64;
    for app in &chosen {
        let (binaries, _) = crate::core::slim::find_universal_binaries(&app.path);
        match crate::core::slim::slim_app(app, &binaries, host, skip_developer_id) {
            Ok(saved) => {
                total_saved += saved;
                let _ = writeln!(
                    console.out,
                    "Slimmed {}, saved {} of Intel code.",
                    app.name,
                    format_bytes(saved)
                );
            }
            Err((step, error)) => {
                let _ = writeln!(
                    console.err,
                    "Error: Slimming {} failed at step {step}: {error}",
                    app.name
                );
            }
        }
    }

    if total_saved > 0 {
        let _ = writeln!(
            console.out,
            "\nTotal saved: {}. Clones share blocks, so the space comes back when the Trash is emptied.",
            format_bytes(total_saved)
        );
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::{fake_app, sandbox};
    use crate::core::host::fake::FakeHost;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("taert").chain(args.iter().copied())).unwrap()
    }

    fn succeeded(code: ExitCode) -> bool {
        format!("{code:?}") == format!("{:?}", ExitCode::SUCCESS)
    }

    /// Whether a handler succeeded, and what it wrote to each stream.
    struct Ran {
        ok: bool,
        out: String,
        err: String,
    }

    /// Runs a handler that reads `typed` as the answers to its prompts.
    fn run(typed: &str, handler: impl FnOnce(&mut Console) -> ExitCode) -> Ran {
        let mut input = typed.as_bytes();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = handler(&mut Console {
            input: &mut input,
            out: &mut out,
            err: &mut err,
        });
        Ran {
            ok: succeeded(code),
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    fn schedule(args: &[&str], host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Schedule(ref schedule_args)) = cli.command else {
            panic!("not a schedule command");
        };
        let config = Config::load_from(&layout.config_path()).unwrap();
        run("", |console| {
            handle_schedule(schedule_args, &config, host, layout, console)
        })
    }

    fn startup(args: &[&str], host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Startup(ref startup_args)) = cli.command else {
            panic!("not a startup command");
        };
        let context = ScanContext { layout, host };
        run("", |console| {
            handle_startup(
                startup_args,
                &cli,
                true,
                &IgnoreStore::default(),
                &Config::default(),
                &context,
                console,
            )
        })
    }

    fn slim(args: &[&str], host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Slim(ref slim_args)) = cli.command else {
            panic!("not a slim command");
        };
        let context = ScanContext { layout, host };
        run("", |console| {
            handle_slim(
                slim_args,
                &cli,
                true,
                &IgnoreStore::default(),
                &context,
                console,
            )
        })
    }

    fn doctor_report(args: &[&str], host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Doctor(ref doctor_args)) = cli.command else {
            panic!("not a doctor command");
        };
        let context = ScanContext { layout, host };
        run("", |console| {
            handle_doctor(
                doctor_args,
                &cli,
                true,
                &IgnoreStore::default(),
                &Config::default(),
                &context,
                console,
            )
        })
    }

    fn uninstall(args: &[&str], host: &FakeHost, layout: &Layout) -> Ran {
        uninstall_typing(args, "", host, layout)
    }

    fn uninstall_typing(args: &[&str], typed: &str, host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Uninstall(ref uninstall_args)) = cli.command else {
            panic!("not an uninstall command");
        };
        let context = ScanContext { layout, host };
        run(typed, |console| {
            handle_uninstall(
                uninstall_args,
                &cli,
                true,
                &IgnoreStore::default(),
                &context,
                console,
            )
        })
    }

    fn clean(args: &[&str], typed: &str, host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Clean(ref clean_args)) = cli.command else {
            panic!("not a clean command");
        };
        let context = ScanContext { layout, host };
        run(typed, |console| {
            handle_clean(
                clean_args,
                &cli,
                true,
                &IgnoreStore::default(),
                &context,
                console,
            )
        })
    }

    /// Runs a clean that removes, once its scan shows that every row lies in the sandbox.
    fn clean_removing(args: &[&str], typed: &str, host: &FakeHost, layout: &Layout) -> Ran {
        let category = args[1];
        let listed = clean(&["clean", category, "--json"], "", host, layout);
        let scan: serde_json::Value = serde_json::from_str(&listed.out).unwrap();
        for group in scan["groups"].as_array().unwrap() {
            for item in group["items"].as_array().unwrap() {
                let also = item["also"].as_array().into_iter().flatten();
                for row in std::iter::once(item).chain(also) {
                    let path = Path::new(row["path"].as_str().unwrap());
                    assert!(path.starts_with(&layout.home), "{}", path.display());
                }
            }
        }
        clean(args, typed, host, layout)
    }

    fn config(args: &[&str], layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Config(ref config_args)) = cli.command else {
            panic!("not a config command");
        };
        let saved = Config::load_from(&layout.config_path()).unwrap();
        run("", |console| {
            handle_config(config_args, &saved, layout, console)
        })
    }

    fn ignore(args: &[&str], layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Ignore(ignore_args)) = cli.command else {
            panic!("not an ignore command");
        };
        let saved = IgnoreStore::load_from(&layout.ignore_path()).unwrap();
        run("", |console| {
            handle_ignore(ignore_args.action, &saved, layout, console)
        })
    }

    fn history(args: &[&str], layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::History(ref history_args)) = cli.command else {
            panic!("not a history command");
        };
        run("", |console| {
            handle_history(history_args, &cli, layout, console)
        })
    }

    fn runs_in(layout: &Layout) -> Vec<crate::core::history::RunRecord> {
        HistoryLog::new(layout.history_path()).load_runs().unwrap()
    }

    fn apple_silicon_host() -> FakeHost {
        let host = FakeHost::idle();
        host.reply("sysctl hw.optional.arm64", "hw.optional.arm64: 1\n");
        host
    }

    #[test]
    fn schedule_set_and_off_use_the_given_host_and_layout() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();

        let set = schedule(
            &[
                "schedule",
                "set",
                "--categories",
                "dev,junk",
                "--min-reclaim",
                "1G",
            ],
            &host,
            &layout,
        );

        assert!(set.ok);
        assert!(
            set.out
                .contains("Every week: dev, junk, when at least 1 GB can be freed.")
        );
        assert!(host.ran("/bin/launchctl bootstrap"));
        let saved = Config::load_from(&layout.config_path()).unwrap();
        assert_eq!(saved.schedule.unwrap().categories, ["dev", "junk"]);

        let off = schedule(&["schedule", "off"], &host, &layout);

        assert!(off.ok);
        assert_eq!(off.out, "Scheduled cleanup turned off.\n");
        assert!(host.ran("/bin/launchctl bootout"));
        let saved = Config::load_from(&layout.config_path()).unwrap();
        assert!(saved.schedule.is_none());
    }

    #[test]
    fn schedule_refuses_purge_and_docker_before_it_runs_anything() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();

        for category in ["purge", "docker"] {
            let ran = schedule(
                &["schedule", "set", "--categories", category],
                &host,
                &layout,
            );
            assert!(!ran.ok);
            assert!(ran.out.is_empty());
            assert!(
                ran.err
                    .contains(&format!("Category '{category}' cannot be scheduled")),
                "{}",
                ran.err
            );
        }

        assert!(host.calls.lock().unwrap().is_empty());
        assert!(!layout.config_path().exists());
    }

    #[test]
    fn schedule_show_when_off_says_so() {
        let (_temp, layout) = sandbox();

        let ran = schedule(&["schedule"], &FakeHost::default(), &layout);

        assert!(ran.ok);
        assert_eq!(ran.out, "Scheduled cleanup is off.\n");
    }

    fn layout_with_a_launch_agent() -> (tempfile::TempDir, Layout) {
        let (temp, layout) = sandbox();
        let folder = layout.user("Library/LaunchAgents");
        std::fs::create_dir_all(&folder).unwrap();
        let mut agent = plist::Dictionary::new();
        agent.insert("Label".into(), "com.example.demo".into());
        agent.insert("Program".into(), "/bin/ls".into());
        plist::Value::Dictionary(agent)
            .to_file_xml(folder.join("com.example.demo.plist"))
            .unwrap();
        (temp, layout)
    }

    #[test]
    fn startup_lists_items_in_plain_and_json() {
        let (_temp, layout) = layout_with_a_launch_agent();
        let host = FakeHost::idle();

        let plain = startup(&["startup", "--plain"], &host, &layout);

        assert!(plain.ok);
        assert!(plain.out.starts_with("=== macOS Startup Items ===\n"));
        assert!(
            plain
                .out
                .contains("- demo [user agent] -> enabled (/bin/ls)")
        );

        let json = startup(&["startup", "--json"], &host, &layout);

        assert!(json.ok);
        let items: serde_json::Value = serde_json::from_str(&json.out).unwrap();
        assert_eq!(items[0]["label"], "com.example.demo");
    }

    #[test]
    fn startup_says_when_it_finds_nothing() {
        let (_temp, layout) = sandbox();

        let ran = startup(&["startup", "--plain"], &FakeHost::idle(), &layout);

        assert!(ran.ok);
        assert!(
            ran.out
                .starts_with("=== macOS Startup Items ===\nNo startup items found.\n")
        );
    }

    #[test]
    fn startup_enable_and_disable_fail_for_an_unknown_item() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::idle();

        for action in ["enable", "disable"] {
            let ran = startup(
                &["startup", action, "com.nonexistent.service"],
                &host,
                &layout,
            );
            assert!(!ran.ok);
            assert!(ran.out.is_empty());
            assert_eq!(
                ran.err,
                "Error: Startup item 'com.nonexistent.service' not found\n"
            );
        }

        assert!(!host.ran("/bin/launchctl enable"));
        assert!(!host.ran("/bin/launchctl disable"));
    }

    #[test]
    fn slim_needs_an_apple_silicon_mac() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::idle();

        let ran = slim(&["slim", "--dry-run"], &host, &layout);

        assert!(!ran.ok);
        assert!(ran.err.contains("requires an Apple Silicon Mac"));
        assert!(!host.ran("codesign"));
    }

    #[test]
    fn slim_lists_nothing_in_an_empty_applications_folder() {
        let (_temp, layout) = sandbox();
        let host = apple_silicon_host();

        let json = slim(&["slim", "--json"], &host, &layout);

        assert!(json.ok);
        assert_eq!(json.out.trim(), "[]");

        let dry = slim(&["slim", "--dry-run"], &host, &layout);

        assert!(dry.ok);
        assert!(
            dry.out
                .contains("Dry run completed. No apps were modified.")
        );
    }

    #[test]
    fn slim_fails_for_an_unknown_app_name() {
        let (_temp, layout) = sandbox();
        let host = apple_silicon_host();

        let ran = slim(&["slim", "NonExistentApp12345"], &host, &layout);

        assert!(!ran.ok);
        assert_eq!(
            ran.err,
            "Error: No installed app matches 'NonExistentApp12345'\n"
        );
        assert!(!host.ran("codesign"));
    }

    #[test]
    fn doctor_describes_the_checks_in_json_and_plain() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::idle();

        let json = doctor_report(&["doctor", "--seconds", "2", "--json"], &host, &layout);

        assert!(json.ok);
        let report: serde_json::Value = serde_json::from_str(&json.out).unwrap();
        assert_eq!(report["seconds"], 2);
        assert!(report["findings"].is_array());
        assert!(report["checks"].is_array());

        let plain = doctor_report(&["doctor", "--seconds", "2", "--plain"], &host, &layout);

        assert!(plain.ok);
        assert!(plain.out.starts_with("Checked for 2 seconds."));
    }

    fn layout_with_the_fixture_app() -> (tempfile::TempDir, Layout) {
        let (temp, layout) = sandbox();
        fake_app(
            &layout.user("Applications/Taert Fixture.app"),
            "is.taert.fixture",
            "run",
        );
        let cache = layout.user("Library/Caches/is.taert.fixture");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("data"), vec![0_u8; 4096]).unwrap();
        (temp, layout)
    }

    fn fixture_is_untouched(layout: &Layout) -> bool {
        layout.user("Applications/Taert Fixture.app").exists()
            && layout.user("Library/Caches/is.taert.fixture").exists()
    }

    #[test]
    fn uninstall_dry_run_shows_the_plan_and_removes_nothing() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = FakeHost::idle();

        let ran = uninstall(
            &["uninstall", "Taert Fixture", "--dry-run", "--force"],
            &host,
            &layout,
        );

        assert!(ran.ok);
        assert!(ran.out.contains("Taert Fixture.app"));
        assert!(ran.out.contains("is.taert.fixture"));
        assert!(ran.out.contains("Dry run completed"));
        assert!(fixture_is_untouched(&layout));
        assert!(host.trashed().is_empty());
    }

    #[test]
    fn uninstall_json_describes_the_plan_and_removes_nothing() {
        let (_temp, layout) = layout_with_the_fixture_app();

        let ran = uninstall(
            &["uninstall", "is.taert.fixture", "--json"],
            &FakeHost::idle(),
            &layout,
        );

        assert!(ran.ok);
        let plan: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(plan["apps"][0]["app"]["bundle_id"], "is.taert.fixture");
        assert_eq!(plan["scan"]["groups"][0]["items"][0]["role"], "bundle");
        assert!(fixture_is_untouched(&layout));
    }

    #[test]
    fn uninstall_force_needs_the_whole_name() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = FakeHost::idle();

        let ran = uninstall(&["uninstall", "taert fix", "--force"], &host, &layout);

        assert!(!ran.ok);
        assert_eq!(ran.err, "Error: No installed app matches 'taert fix'\n");
        assert!(fixture_is_untouched(&layout));
        assert!(host.trashed().is_empty());
    }

    #[test]
    fn uninstall_force_without_a_name_refuses() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = FakeHost::idle();

        let ran = uninstall(&["uninstall", "--force"], &host, &layout);

        assert!(!ran.ok);
        assert_eq!(
            ran.err,
            "Error: Name the apps to remove: taert uninstall <name>\n"
        );
        assert!(fixture_is_untouched(&layout));
        assert!(host.trashed().is_empty());
    }

    #[test]
    fn uninstall_fails_for_an_unknown_name() {
        let (_temp, layout) = layout_with_the_fixture_app();

        let ran = uninstall(
            &["uninstall", "No Such App", "--dry-run"],
            &FakeHost::idle(),
            &layout,
        );

        assert!(!ran.ok);
        assert_eq!(ran.err, "Error: No installed app matches 'No Such App'\n");
    }

    #[test]
    fn a_forced_uninstall_logs_to_the_history_of_the_layout() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = FakeHost::idle();
        host.simulate_trash();

        let ran = uninstall(&["uninstall", "Taert Fixture", "--force"], &host, &layout);

        assert!(ran.ok);
        let runs = runs_in(&layout);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].command, "uninstall Taert Fixture");
        assert!(!layout.user("Applications/Taert Fixture.app").exists());
    }

    #[test]
    fn plain_uninstall_asks_before_it_removes() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = FakeHost::idle();
        host.simulate_trash();

        let ran = uninstall_typing(
            &["uninstall", "Taert Fixture", "--plain"],
            "\nno\n",
            &host,
            &layout,
        );

        assert!(ran.ok);
        assert!(ran.out.contains("Type 'yes' to confirm"));
        assert!(ran.out.ends_with("Operation cancelled.\n"));
        assert!(fixture_is_untouched(&layout));
        assert!(host.trashed().is_empty());
        assert!(!layout.history_path().exists());
    }

    #[test]
    fn plain_uninstall_removes_after_yes() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = FakeHost::idle();
        host.simulate_trash();

        let ran = uninstall_typing(
            &["uninstall", "Taert Fixture", "--plain"],
            "\nyes\n",
            &host,
            &layout,
        );

        assert!(ran.ok);
        assert!(ran.out.contains("Uninstall complete"));
        assert!(!layout.user("Applications/Taert Fixture.app").exists());
        let runs = runs_in(&layout);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].mode, "interactive");
    }

    const SAFARI: &str = "Library/Caches/com.apple.Safari";
    const SAFARI_CONTAINER: &str = "Library/Containers/com.apple.Safari/Data/Library/Caches";
    const CHROME: &str = "Library/Caches/Google/Chrome";
    const CHROME_PROGRAM: &str = "/nowhere/Google Chrome.app/Contents/MacOS/Google Chrome";
    const FULL_DISK_ACCESS: &str = "Full Disk Access";

    fn fill(layout: &Layout, relative: &str) -> PathBuf {
        let folder = layout.user(relative);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("data"), [1_u8; 8192]).unwrap();
        folder
    }

    fn age(folder: &Path, days: u32) {
        let then = SystemTime::now() - Duration::from_secs(24 * 60 * 60) * days;
        for path in [folder.join("data"), folder.to_path_buf()] {
            std::fs::File::open(path)
                .unwrap()
                .set_modified(then)
                .unwrap();
        }
    }

    /// A folder this terminal cannot read. It opens up again so the sandbox can be deleted.
    struct Refused(PathBuf);

    impl Refused {
        fn in_home(layout: &Layout, relative: &str) -> Self {
            let folder = fill(layout, relative);
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o000)).unwrap();
            Self(folder)
        }
    }

    impl Drop for Refused {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    #[test]
    fn clean_dry_run_lists_both_groups_and_removes_nothing() {
        let (_temp, layout) = sandbox();
        let app = fill(&layout, "Library/Caches/com.acme.app");
        let chrome = fill(&layout, CHROME);

        let ran = clean(
            &["clean", "junk", "--dry-run"],
            "",
            &FakeHost::idle(),
            &layout,
        );

        assert!(ran.ok);
        assert!(ran.out.contains("[App caches]"));
        assert!(ran.out.contains("[Browser caches]"));
        assert!(ran.out.contains("Dry run completed"));
        assert!(app.exists());
        assert!(chrome.exists());
    }

    #[test]
    fn clean_of_one_category_keeps_the_note_of_a_refused_read() {
        let (_temp, layout) = sandbox();
        let _refused = Refused::in_home(&layout, SAFARI_CONTAINER);

        let ran = clean(
            &["clean", "junk", "--dry-run"],
            "",
            &FakeHost::idle(),
            &layout,
        );

        assert!(ran.ok);
        assert!(ran.out.contains(FULL_DISK_ACCESS));
    }

    #[test]
    fn clean_of_every_category_keeps_the_note_of_a_refused_read() {
        let (_temp, layout) = sandbox();
        let _refused = Refused::in_home(&layout, SAFARI_CONTAINER);

        let ran = clean(&["clean", "--dry-run"], "", &FakeHost::idle(), &layout);

        assert!(ran.ok);
        assert!(ran.out.contains(FULL_DISK_ACCESS));
    }

    #[test]
    fn clean_of_every_category_lists_the_trash_of_the_layout() {
        let (_temp, layout) = sandbox();
        let trashed = fill(&layout, ".Trash/old");
        let uid = unsafe { libc::getuid() }.to_string();
        let on_volume = layout.system("Volumes/Ext/.Trashes").join(uid);
        std::fs::create_dir_all(&on_volume).unwrap();
        std::fs::write(on_volume.join("gone"), [1_u8; 8192]).unwrap();

        let ran = clean(&["clean", "--dry-run"], "", &FakeHost::idle(), &layout);

        assert!(ran.ok);
        assert!(ran.out.contains(&trashed.display().to_string()));
        assert!(
            ran.out
                .contains(&on_volume.join("gone").display().to_string())
        );
        assert!(trashed.exists());
    }

    #[test]
    fn clean_json_carries_the_notes_and_every_folder_of_a_row() {
        let (_temp, layout) = sandbox();
        let outer = fill(&layout, SAFARI);
        let container = fill(&layout, SAFARI_CONTAINER);
        let _refused = Refused::in_home(&layout, "Library/Caches/Firefox");

        let ran = clean(&["clean", "junk", "--json"], "", &FakeHost::idle(), &layout);

        let scan: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert!(
            scan["notes"][0]
                .as_str()
                .unwrap()
                .contains(FULL_DISK_ACCESS)
        );
        let browsers = scan["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|group| group["tool"] == "Browser caches")
            .unwrap();
        let row = &browsers["items"][0];
        assert_eq!(row["path"].as_str(), outer.to_str());
        assert_eq!(row["also"][0]["path"].as_str(), container.to_str());
    }

    #[test]
    fn forced_clean_leaves_a_running_browsers_cache_and_removes_a_stale_app_cache() {
        let (_temp, layout) = sandbox();
        let chrome = fill(&layout, CHROME);
        let stale = fill(&layout, "Library/Caches/com.acme.stale");
        age(&stale, 90);
        let host = FakeHost::default();
        host.reply("/bin/ps", &format!("  501 1 {CHROME_PROGRAM}\n"));

        let ran = clean_removing(&["clean", "junk", "--force"], "", &host, &layout);

        assert!(ran.ok);
        assert!(ran.out.contains("Clean complete"));
        assert!(chrome.join("data").exists());
        assert!(!stale.exists());
    }

    #[test]
    fn forced_clean_removes_a_stale_app_cache_and_leaves_a_recent_one() {
        let (_temp, layout) = sandbox();
        let stale = fill(&layout, "Library/Caches/com.acme.stale");
        age(&stale, 90);
        let recent = fill(&layout, "Library/Caches/com.acme.recent");

        let ran = clean_removing(
            &["clean", "junk", "--force"],
            "",
            &FakeHost::idle(),
            &layout,
        );

        assert!(ran.ok);
        assert!(ran.out.contains("Clean complete"));
        assert!(!stale.exists());
        assert!(recent.join("data").exists());
    }

    #[test]
    fn forced_clean_reports_the_note_of_a_refused_read() {
        let (_temp, layout) = sandbox();
        let _refused = Refused::in_home(&layout, SAFARI_CONTAINER);

        let ran = clean_removing(
            &["clean", "junk", "--force"],
            "",
            &FakeHost::idle(),
            &layout,
        );

        assert!(ran.ok);
        assert!(ran.out.contains("Clean complete"));
        assert_eq!(ran.out.matches(FULL_DISK_ACCESS).count(), 1);
    }

    #[test]
    fn plain_clean_prints_a_note_once_through_a_whole_clean() {
        let (_temp, layout) = sandbox();
        let _refused = Refused::in_home(&layout, SAFARI_CONTAINER);
        let cache = fill(&layout, "Library/Caches/com.acme.app");

        let ran = clean_removing(
            &["clean", "junk", "--plain"],
            "all\nyes\n",
            &FakeHost::idle(),
            &layout,
        );

        assert!(ran.ok);
        assert!(ran.out.contains("Clean complete"));
        assert_eq!(ran.out.matches(FULL_DISK_ACCESS).count(), 1);
        assert!(!cache.exists());
    }

    #[test]
    fn plain_clean_names_both_folders_of_a_row_before_it_asks() {
        let (_temp, layout) = sandbox();
        let outer = fill(&layout, SAFARI);
        let container = fill(&layout, SAFARI_CONTAINER);

        let ran = clean(
            &["clean", "junk", "--plain"],
            "all\nno\n",
            &FakeHost::idle(),
            &layout,
        );

        assert!(ran.ok);
        let named = ran.out.matches(&container.display().to_string()).count();
        assert_eq!(named, 2);
        assert!(ran.out.ends_with("Operation cancelled.\n"));
        assert!(outer.exists());
        assert!(container.exists());
        assert!(!layout.history_path().exists());
    }

    #[test]
    fn a_forced_clean_logs_to_the_history_of_the_layout() {
        let (_temp, layout) = sandbox();
        let stale = fill(&layout, "Library/Caches/com.acme.stale");
        age(&stale, 90);

        let ran = clean_removing(
            &["clean", "junk", "--force"],
            "",
            &FakeHost::idle(),
            &layout,
        );

        assert!(ran.ok);
        let runs = runs_in(&layout);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].command, "clean junk");
        assert_eq!(runs[0].mode, "force");
    }

    #[test]
    fn a_plain_clean_logs_to_the_history_of_the_layout() {
        let (_temp, layout) = sandbox();
        fill(&layout, "Library/Caches/com.acme.app");

        let ran = clean_removing(
            &["clean", "junk", "--plain"],
            "all\nyes\n",
            &FakeHost::idle(),
            &layout,
        );

        assert!(ran.ok);
        let runs = runs_in(&layout);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].command, "clean junk");
        assert_eq!(runs[0].mode, "interactive");
    }

    #[test]
    fn config_saves_the_appearance_to_the_config_of_the_layout() {
        let (_temp, layout) = sandbox();

        let ran = config(&["config", "--appearance", "Dark"], &layout);

        assert!(ran.ok);
        assert_eq!(
            ran.out,
            format!(
                "Updated appearance to 'dark' in {}\n",
                layout.config_path().display()
            )
        );
        let saved = Config::load_from(&layout.config_path()).unwrap();
        assert_eq!(saved.appearance, "dark");
    }

    #[test]
    fn config_shows_the_files_of_the_layout() {
        let (_temp, layout) = sandbox();

        let ran = config(&["config"], &layout);

        assert!(ran.ok);
        let config_line = format!("Config path: {}\n", layout.config_path().display());
        let ignore_line = format!("Ignore list: {}\n", layout.ignore_path().display());
        assert!(ran.out.contains(&config_line), "{}", ran.out);
        assert!(ran.out.contains(&ignore_line), "{}", ran.out);
    }

    #[test]
    fn config_refuses_an_unknown_appearance() {
        let (_temp, layout) = sandbox();

        let ran = config(&["config", "--appearance", "neon"], &layout);

        assert!(!ran.ok);
        assert!(ran.err.contains("Invalid appearance 'neon'"), "{}", ran.err);
        assert!(!layout.config_path().exists());
    }

    #[test]
    fn ignore_add_and_remove_save_to_the_ignore_list_of_the_layout() {
        let (_temp, layout) = sandbox();
        let folder = layout.user("Projects");
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.display().to_string();

        let added = ignore(&["ignore", "add", &path], &layout);

        assert!(added.ok);
        assert_eq!(added.out, format!("Protected path: {path}\n"));
        let saved = IgnoreStore::load_from(&layout.ignore_path()).unwrap();
        assert!(saved.contains(&path));

        let removed = ignore(&["ignore", "remove", &path], &layout);

        assert!(removed.ok);
        assert_eq!(removed.out, format!("Removed from ignore list: {path}\n"));
        let saved = IgnoreStore::load_from(&layout.ignore_path()).unwrap();
        assert!(!saved.contains(&path));
    }

    #[test]
    fn history_reads_the_log_of_the_layout() {
        let (_temp, layout) = sandbox();
        let session =
            HistoryLog::new(layout.history_path()).start_run("clean junk", "force", Some(0));
        let run_id = session.run_id().to_string();
        session.finish();

        let listed = history(&["history"], &layout);

        assert!(listed.ok);
        assert!(listed.out.contains("clean junk"), "{}", listed.out);

        let shown = history(&["history", "show", &run_id], &layout);

        assert!(shown.ok);
        assert!(shown.out.contains(&format!("Run:     {run_id}\n")));
        assert!(shown.out.contains("Command: clean junk\n"));
    }

    #[test]
    fn history_names_a_run_it_cannot_find() {
        let (_temp, layout) = sandbox();

        let ran = history(&["history", "show", "20260101-000000-abcd"], &layout);

        assert!(!ran.ok);
        assert_eq!(ran.err, "Error: Run '20260101-000000-abcd' not found\n");
    }
}
