pub mod cli;
pub mod core;
pub mod plain;
pub mod ui;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser};

use crate::cli::{
    AdminHelperAction, AdminHelperCli, AdminHelperCommand, BrewAction, CleanArgs, CleanCategory,
    Cli, Command, CompressArgs, ConfigArgs, DoctorArgs, HistoryAction, HistoryArgs, IgnoreAction,
    LargeArgs, PurgeArgs, ScheduleAction, ScheduleArgs, SlimArgs, StartupArgs, UninstallArgs,
};
use crate::core::apps::{self, InstalledApp, Matching};
use crate::core::bytes::format_bytes;
use crate::core::catalog::Category;
use crate::core::cleaner::{RemovalMode, RemovalReport, item_removal_mode};
use crate::core::date_utils::format_rfc3339;
use crate::core::docker::DockerClient;
use crate::core::doctor;
use crate::core::history::{HistoryLog, ItemOutcome, brew_uninstall_command};
use crate::core::homebrew;
use crate::core::host::{Host, MacHost};
use crate::core::large_files;
use crate::core::layout::Layout;
use crate::core::path_utils::{expand_tilde, prettify_path};
use crate::core::purger::Purger;
use crate::core::safety::Safety;
use crate::core::scanner::{
    CategoryScan, CategoryScanner, ScanContext, ScanItem, scan_all_categories,
};
use crate::core::schedule;
use crate::core::sizer::DirSizer;
use crate::core::startup;
use crate::core::status;
use crate::core::stores::{Config, IgnoreStore};
use crate::core::uninstaller::{self, PlanRequest, Route, UninstallPlan, Uninstaller};
use crate::core::{admin, admin_helper};
use crate::plain::{Console, PlainRunner};

/// Ends the sudo session when `main` returns or a panic unwinds.
struct EndAdminSession;

impl Drop for EndAdminSession {
    fn drop(&mut self) {
        admin::session().end(&MacHost);
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "admin-helper") {
        return handle_admin_helper(args);
    }
    let _end_admin_session = EndAdminSession;
    let cli = Cli::parse_from(args);
    if let Some(Command::Completion { shell }) = cli.command {
        clap_complete::generate(shell, &mut Cli::command(), "taert", &mut std::io::stdout());
        return ExitCode::SUCCESS;
    }
    if let Some(error) = unsupported_flag(&cli) {
        eprintln!("Error: {error}");
        return ExitCode::FAILURE;
    }
    let is_tty = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();

    // Plain mode is forced if explicitly specified or if not running in an interactive TTY
    let use_plain = cli.plain || !is_tty;

    let layout = Layout::current();
    let ignore = saved_ignore_list(&layout);
    let config = saved_config(&layout);
    let context = ScanContext {
        layout: &layout,
        host: &MacHost,
        admin: admin::session(),
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
                run_tui(&mut app, &cli, console)
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

        Some(Command::Brew(ref args)) => match args.action {
            Some(BrewAction::Cleanup) => {
                handle_brew_cleanup(&cli, use_plain, &ignore, &context, console)
            }
            Some(BrewAction::Uninstall { ref name }) => {
                handle_brew_uninstall(name, &cli, &ignore, &context, console)
            }
            Some(BrewAction::Upgrade { ref name }) => {
                handle_brew_upgrade(name.as_deref(), &cli, &context, console)
            }
            None => handle_brew(&cli, use_plain, &ignore, &config, &context, console),
        },

        Some(Command::Compress(ref args)) => handle_compress(
            args,
            &cli,
            &ignore,
            &context,
            crate::core::safety::SYSTEM_OWNER,
            console,
        ),

        Some(Command::Slim(ref args)) => handle_slim(
            args,
            &cli,
            use_plain,
            &ignore,
            &context,
            crate::core::safety::SYSTEM_OWNER,
            console,
        ),

        Some(Command::Analyze(ref args)) => handle_analyze(
            args.path.clone(),
            args.depth,
            &cli,
            use_plain,
            &ignore,
            &config,
            console,
        ),

        Some(Command::Large(ref args)) => {
            handle_large(args, &cli, use_plain, &ignore, &context, console)
        }

        Some(Command::Doctor(ref args)) => {
            handle_doctor(args, &cli, use_plain, &ignore, &config, &context, console)
        }

        Some(Command::Status) => handle_status(&cli, use_plain, &config, &ignore, console),

        Some(Command::Startup(ref args)) => {
            handle_startup(args, &cli, use_plain, &ignore, &config, &context, console)
        }

        Some(Command::Ignore(args)) => handle_ignore(args.action, &ignore, &layout, console),

        Some(Command::Config(ref args)) => handle_config(args, &config, &layout, console),

        Some(Command::History(ref args)) => handle_history(args, &cli, &layout, console),

        Some(Command::Schedule(ref args)) => {
            handle_schedule(args, &config, &MacHost, &layout, console)
        }

        Some(Command::Completion { .. }) => ExitCode::SUCCESS,
    }
}

/// Prints one JSON object and exits 0 even on an error, bad arguments and `--help` included,
/// because the parent reads stdout only when the script exits with 0.
fn handle_admin_helper(args: Vec<std::ffi::OsString>) -> ExitCode {
    let answer = match AdminHelperCli::try_parse_from(args) {
        Ok(cli) => {
            let AdminHelperCommand::AdminHelper(action) = cli.command;
            admin_helper::answer_or_error(|| match action {
                AdminHelperAction::Compress(args) => {
                    admin_helper::run_compress(&args.home, &args.bundle, args.undo)
                }
                AdminHelperAction::Slim(args) => {
                    admin_helper::run_slim(&args.home, args.owner, args.group, &args.bundle)
                }
            })
        }
        Err(error) => admin_helper::error_answer(error.to_string().trim()),
    };
    println!("{answer}");
    ExitCode::SUCCESS
}

fn saved_config(layout: &Layout) -> Config {
    Config::load_from(&layout.config_path()).unwrap_or_default()
}

fn saved_ignore_list(layout: &Layout) -> IgnoreStore {
    IgnoreStore::load_from(&layout.ignore_path()).unwrap_or_default()
}

/// The name of a command that reads neither `--dry-run` nor `--json`.
fn flagless_command(command: &Option<Command>) -> Option<&'static str> {
    match command {
        Some(Command::Schedule(_)) => Some("schedule"),
        Some(Command::Ignore(_)) => Some("ignore"),
        Some(Command::Config(_)) => Some("config"),
        _ => None,
    }
}

/// The error for a flag the command would ignore, so it is refused instead.
fn unsupported_flag(cli: &Cli) -> Option<String> {
    let name = flagless_command(&cli.command)?;
    let flag = if cli.dry_run {
        "--dry-run"
    } else if cli.json {
        "--json"
    } else {
        return None;
    };
    Some(format!("{flag} does not work with taert {name}"))
}

fn handle_clean(
    args: &CleanArgs,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    let category = args.category.map(|category| match category {
        CleanCategory::Dev => Category::Dev,
        CleanCategory::Junk => Category::Junk,
        CleanCategory::Ai => Category::Ai,
        CleanCategory::Trash => Category::Trash,
        CleanCategory::Orphans => Category::Orphans,
    });
    let command = category.map_or("clean", Category::history_command);
    let scans = match category {
        Some(category) => vec![CategoryScanner::scan(context, category, ignore, None, None)],
        None => scan_all_categories(context, ignore, None, None),
    };
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

fn handle_brew_cleanup(
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    match homebrew::cleanup_scan(context.layout, context.host) {
        Ok(mut scan) => process_scan_result(
            &mut scan,
            cli,
            use_plain,
            ignore,
            "brew cleanup",
            context,
            console,
        ),
        Err(error) => {
            let _ = writeln!(console.err, "Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn handle_brew_uninstall(
    name: &str,
    cli: &Cli,
    ignore: &IgnoreStore,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    let scan = match homebrew::uninstall_plan(context.layout, context.host, name) {
        Ok(scan) => scan,
        Err(error) => {
            let _ = writeln!(console.err, "Error: {error}");
            return ExitCode::FAILURE;
        }
    };

    if cli.json {
        return match serde_json::to_string_pretty(&scan) {
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

    let Some(row) = scan.selected_items().first().copied() else {
        let _ = writeln!(
            console.err,
            "Error: taert has nothing to remove for {name}."
        );
        return ExitCode::FAILURE;
    };
    let what = format!(
        "{} ({}) with brew uninstall",
        row.path.display(),
        format_bytes(row.size_bytes)
    );
    if cli.dry_run {
        let _ = writeln!(console.out, "Would remove {what}.");
        return ExitCode::SUCCESS;
    }

    let mode = if cli.force {
        "force"
    } else {
        if !confirmed(console, &format!("Remove {what}?")) {
            return ExitCode::SUCCESS;
        }
        "interactive"
    };

    let command = brew_uninstall_command(&[homebrew::formula_name(&row.path.to_string_lossy())]);
    let report = clean_and_log(&scan, cli, ignore, &command, mode, context);
    let _ = PlainRunner::write_report("Uninstall", &report, console);
    if report.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Asks `question [y/N]`; anything but y or yes prints `Operation cancelled.` and is false.
fn confirmed(console: &mut Console, question: &str) -> bool {
    let _ = write!(console.out, "{question} [y/N] ");
    let _ = console.out.flush();
    let mut answer = String::new();
    let _ = console.input.read_line(&mut answer);
    let yes = matches!(answer.trim().to_lowercase().as_str(), "y" | "yes");
    if !yes {
        let _ = writeln!(console.out, "Operation cancelled.");
    }
    yes
}

fn new_version(package: &homebrew::Package) -> &str {
    package.latest_version.as_deref().unwrap_or_default()
}

fn handle_brew_upgrade(
    name: Option<&str>,
    cli: &Cli,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    if !(cli.dry_run || cli.json)
        && let Some(brew) = homebrew::find(context.layout)
        && let Err(error) = homebrew::update(context.host, &brew.path)
    {
        let _ = writeln!(console.err, "Could not check for updates: {error}");
    }
    let plan = match homebrew::upgrade_plan(context.layout, context.host, name) {
        Ok(plan) => plan,
        Err(error) => {
            let _ = writeln!(console.err, "Error: {error}");
            return ExitCode::FAILURE;
        }
    };

    if cli.json {
        let rows: Vec<_> = plan
            .packages
            .iter()
            .map(|package| {
                serde_json::json!({
                    "name": package.name,
                    "kind": package.kind,
                    "version": package.version,
                    "latest_version": package.latest_version,
                })
            })
            .collect();
        return match serde_json::to_string_pretty(&rows) {
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

    if plan.packages.is_empty() {
        let _ = writeln!(console.out, "Everything is up to date.");
        return ExitCode::SUCCESS;
    }
    if cli.dry_run {
        for package in &plan.packages {
            let change = homebrew::upgrade_change(package);
            let _ = writeln!(console.out, "Would upgrade {change}.");
        }
        return ExitCode::SUCCESS;
    }

    if !cli.force {
        for package in &plan.packages {
            let _ = writeln!(console.out, "{}", homebrew::upgrade_change(package));
        }
        let count = ui::components::text::plural(plan.packages.len(), "package", "packages");
        if !confirmed(console, &format!("Upgrade {count}?")) {
            return ExitCode::SUCCESS;
        }
    }

    let mut failed = false;
    for package in &plan.packages {
        let name = &package.name;
        match homebrew::upgrade(context.host, &plan.brew, package.kind, name) {
            Ok(()) => {
                let _ = writeln!(console.out, "Upgraded {name} to {}.", new_version(package));
            }
            Err(error) => {
                failed = true;
                let _ = writeln!(console.err, "Could not upgrade {name}: {error}");
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn handle_brew(
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    config: &Config,
    context: &ScanContext,
    console: &mut Console,
) -> ExitCode {
    let inventory = match homebrew::scan(context.layout, context.host) {
        Ok(inventory) => inventory,
        Err(error) => {
            let _ = writeln!(console.err, "Error: {error}");
            return ExitCode::FAILURE;
        }
    };

    if cli.json {
        return match serde_json::to_string_pretty(&inventory) {
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

    if use_plain {
        let notes = PlainRunner::notes_text(&inventory.notes);
        let list = homebrew::plain_text(&inventory);
        let notes = if list.is_empty() {
            notes.trim_start()
        } else {
            &notes
        };
        let _ = write!(console.out, "{list}{notes}");
        return ExitCode::SUCCESS;
    }

    let mut app = ui::app::App::with_brew_list(&inventory, ignore.clone(), config.clone());
    run_tui(&mut app, cli, console)
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
        Some(p) => Safety::absolute(&expand_tilde(&p.to_string_lossy())),
        None => context.layout.home.clone(),
    };

    let root = match root.canonicalize() {
        Ok(r) => r,
        Err(err) => {
            let _ = writeln!(
                console.err,
                "Error: Cannot scan path '{}': {err}",
                root.display()
            );
            return ExitCode::FAILURE;
        }
    };

    let command = format!("large {}", prettify_path(&root));
    let options = large_files::LargeScanOptions::new(args.min_size, args.older_than, args.limit);
    let mut scan = large_files::scan(
        &root,
        &options,
        context.layout,
        context.host,
        ignore,
        None,
        None,
    );

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
            PlainRunner::scan_text_with_options(scan, mode_of(scan, cli.permanent))
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
            PlainRunner::scan_text_with_options(scan, mode_of(scan, cli.permanent))
        );
        if let Err(e) = PlainRunner::prompt_selection(scan, console) {
            let _ = writeln!(console.err, "Selection error: {e}");
            return ExitCode::FAILURE;
        }

        match PlainRunner::prompt_confirm(scan, console, mode_of(scan, cli.permanent)) {
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
        run_tui(&mut app, cli, console)
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
    let mut installed = apps::find_all_apps(context.layout, context.host);

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
        return run_tui(&mut app, cli, console);
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
        PlainRunner::scan_text_with_options(&list, mode_of(&list, cli.permanent))
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

/// Prepares the sudo session and, the first time taert itself asked, prints the Touch ID tip on stderr.
fn prepare_admin(
    context: &ScanContext,
    reason: &str,
    err: &mut dyn std::io::Write,
) -> Result<(), String> {
    context.admin.prepare(context.host, reason)?;
    if context.admin.take_hint()
        && let Some(hint) = admin::touch_id_hint(context.host, context.layout)
    {
        let _ = writeln!(err, "{hint}");
    }
    Ok(())
}

fn run_tui(app: &mut ui::app::App, cli: &Cli, console: &mut Console) -> ExitCode {
    if cli.dry_run {
        let _ = writeln!(
            console.err,
            "Error: --dry-run does not work in the interactive screen. Add --plain to see the preview."
        );
        return ExitCode::FAILURE;
    }
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
    routes: &HashMap<PathBuf, Route>,
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
    if uninstaller::needs_password(&plan.scan, routes) {
        // A refusal needs no text here: the report says what stayed.
        let _ = prepare_admin(context, uninstaller::ADMIN_REASON, &mut *console.err);
    }
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

/// Where each row of a scan goes.
fn mode_of(scan: &CategoryScan, permanent: bool) -> impl Fn(&ScanItem) -> RemovalMode + '_ {
    move |item| item_removal_mode(item, &scan.id, permanent)
}

/// Where each row of an uninstall plan goes, by the route worked out for it.
/// Where each row of a plan goes, read from routes worked out once for the whole run.
fn plan_mode(
    routes: &HashMap<PathBuf, Route>,
    permanent: bool,
) -> impl Fn(&ScanItem) -> RemovalMode + '_ {
    move |item| uninstaller::route_of(routes, item).mode(permanent)
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
        let routes = uninstaller::routes(&plan);
        let _ = write!(
            console.out,
            "{}",
            PlainRunner::scan_text_with_options(&plan.scan, plan_mode(&routes, cli.permanent))
        );
        let _ = write!(console.out, "{}", PlainRunner::notes_text(&plan.notes));
        let _ = writeln!(console.out, "\nDry run completed. No files were deleted.");
        return ExitCode::SUCCESS;
    }

    if !cli.force && !use_plain {
        let mut app = ui::app::App::with_plan(plan, ignore.clone(), saved_config(context.layout))
            .with_permanent(cli.permanent);
        return run_tui(&mut app, cli, console);
    }

    // The list, the confirmation, and the run read the same routes, so they cannot disagree.
    let routes = uninstaller::routes(&plan);
    if cli.force {
        return run_uninstall(&plan, &routes, cli, ignore, context, console);
    }

    let _ = write!(
        console.out,
        "{}",
        PlainRunner::scan_text_with_options(&plan.scan, plan_mode(&routes, cli.permanent))
    );
    let _ = write!(console.out, "{}", PlainRunner::notes_text(&plan.notes));
    if let Err(error) = PlainRunner::prompt_selection(&mut plan.scan, console) {
        let _ = writeln!(console.err, "Selection error: {error}");
        return ExitCode::FAILURE;
    }
    match PlainRunner::prompt_confirm(&plan.scan, console, plan_mode(&routes, cli.permanent)) {
        Ok(true) => run_uninstall(&plan, &routes, cli, ignore, context, console),
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
    console: &mut Console,
) -> ExitCode {
    let scan_root = match Safety::validate_scan_root(&path) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(
                console.err,
                "Error: Cannot analyze '{}': {}",
                path.display(),
                e
            );
            return ExitCode::FAILURE;
        }
    };

    if !use_plain && !cli.json {
        let mut app = ui::app::App::with_analyze(scan_root, ignore.clone(), config.clone())
            .with_permanent(cli.permanent);
        return run_tui(&mut app, cli, console);
    }

    let _ = writeln!(
        console.out,
        "Analyzing disk usage for: {}\n",
        scan_root.display()
    );

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
        let _ = writeln!(
            console.out,
            "{}",
            serde_json::to_string_pretty(&json_entries).unwrap()
        );
        return ExitCode::SUCCESS;
    }

    for (p, size) in entries.iter().take(depth * 25) {
        let name = p.file_name().unwrap_or_default().to_string_lossy();
        let _ = writeln!(console.out, "{:>10}  {}", format_bytes(*size), name);
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
        run_tui(&mut app, cli, console)
    }
}

fn handle_status(
    cli: &Cli,
    use_plain: bool,
    config: &Config,
    ignore: &IgnoreStore,
    console: &mut Console,
) -> ExitCode {
    if cli.json || use_plain {
        let raw = status::read();
        let collected_at = format_rfc3339(std::time::SystemTime::now());
        let (report, notes) = status::StatusReport::from_status(raw, collected_at);
        for note in &notes {
            let _ = writeln!(console.err, "{note}");
        }
        let write_ok = if cli.json {
            match serde_json::to_string_pretty(&report) {
                Ok(json) => writeln!(console.out, "{json}").is_ok(),
                Err(_) => false,
            }
        } else {
            write!(console.out, "{}", status::plain_text(&report)).is_ok()
        };
        if write_ok && !report.is_unusable() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    } else {
        let mut app = ui::app::App::new_status(ignore.clone(), config.clone());
        run_tui(&mut app, cli, console)
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
    let ScanContext { layout, host, .. } = *context;
    let out = &mut *console.out;
    // A refusal needs no handling here: the root step then fails with the reason.
    if args.all {
        let _ = prepare_admin(context, "read background task records", &mut *console.err);
    }
    if let Some(ref action) = args.action {
        let (label, is_enable) = match action {
            cli::StartupAction::Disable { label } => (label, false),
            cli::StartupAction::Enable { label } => (label, true),
        };

        let scan = startup::scan_startup(host, layout, ignore, args.all);
        let item = match startup::resolve_item(&scan.items, label) {
            Ok(item) => item,
            Err(err) => {
                let _ = writeln!(console.err, "Error: {err}");
                return ExitCode::FAILURE;
            }
        };
        if item.kind == startup::StartupKind::Daemon
            && !cli.dry_run
            && item.read_only_reason().is_none()
        {
            let _ = prepare_admin(context, "switch system daemons", &mut *console.err);
        }

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
        match serde_json::to_string_pretty(&scan) {
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
                    startup::StartupState::Disabled if item.is_disabled_but_running() => {
                        "disabled (still running)".to_string()
                    }
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
        run_tui(&mut app, cli, console)
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
            let _ = writeln!(out, "Date:    {}", run_record.date_label());
            let _ = writeln!(out, "Command: {}", run_record.command_label());
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

fn handle_compress(
    args: &CompressArgs,
    cli: &Cli,
    ignore: &IgnoreStore,
    context: &ScanContext,
    system_owner: u32,
    console: &mut Console,
) -> ExitCode {
    let options = crate::core::compress::CompressOptions {
        dry_run: cli.dry_run,
        undo: args.undo,
    };
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let compressor = crate::core::compress::Compressor {
        host: context.host,
        layout: context.layout,
        ignore,
        system_owner,
    };

    let path = Safety::absolute(&args.path);
    let of_the_system =
        Safety::is_app_of_the_system(&path, context.layout, system_owner, context.host.uid());
    let result = if of_the_system {
        compress_system_app(&compressor, context, &path, &options, &cancelled)
    } else {
        compressor
            .process_path(&args.path, &options, &cancelled, |_, _| {})
            .map_err(|e| e.to_string())
    };
    let report = match result {
        Ok(report) => report,
        Err(e) => {
            if cli.json {
                let error_json = serde_json::json!({
                    "error": e,
                    "path": args.path,
                });
                let _ = writeln!(
                    console.out,
                    "{}",
                    serde_json::to_string_pretty(&error_json).unwrap_or_default()
                );
            } else {
                let _ = writeln!(console.err, "Error: {e}");
            }
            return ExitCode::FAILURE;
        }
    };

    if !options.undo && !options.dry_run && report.processed_files > 0 {
        let history = HistoryLog::new(context.layout.history_path());
        let mode = if cli.force { "force" } else { "interactive" };
        let session = history.start_run("compress", mode, Some(1));
        session.record_item(&ItemOutcome::changed(args.path.clone(), report.saved_bytes));
        session.finish();
    }

    let printed = if cli.json {
        match serde_json::to_string_pretty(&report) {
            Ok(json) => writeln!(console.out, "{json}"),
            Err(e) => {
                let _ = writeln!(console.err, "Failed to serialize JSON: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        writeln!(
            console.out,
            "{}",
            PlainRunner::compress_report_text(&report, &args.path)
        )
    };
    if printed.is_ok() && report.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// An app that belongs to the system. An estimate reads its files here. A change asks for the
/// administrator password and goes to the root helper.
fn compress_system_app(
    compressor: &crate::core::compress::Compressor,
    context: &ScanContext,
    path: &std::path::Path,
    options: &crate::core::compress::CompressOptions,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<crate::core::compress::CompressReport, String> {
    if options.dry_run {
        return compressor
            .process_path_guarded(
                path,
                options,
                compressor.system_owner,
                cancelled,
                |_| Ok(()),
                |_, _| {},
            )
            .map_err(|e| e.to_string());
    }
    Safety::can_compress_with_host(
        path,
        compressor.ignore,
        context.layout,
        context.host,
        compressor.system_owner,
    )
    .map_err(|e| e.to_string())?;
    context
        .admin
        .prepare(context.host, crate::core::compress::ADMIN_REASON)?;
    compressor.compress_as_root(path, options.undo)
}

fn handle_slim(
    args: &SlimArgs,
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    context: &ScanContext,
    system_owner: u32,
    console: &mut Console,
) -> ExitCode {
    let ScanContext { layout, host, .. } = *context;
    if !crate::core::slim::is_apple_silicon(host) {
        let _ = writeln!(
            console.err,
            "Slimming universal apps requires an Apple Silicon Mac."
        );
        return ExitCode::FAILURE;
    }

    if args.names.is_empty() {
        return pick_apps_to_slim(cli, use_plain, ignore, context, system_owner, console);
    }

    let installed = crate::core::slim::candidate_apps(layout);
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
    let Some(running) = crate::core::processes::read(host) else {
        let _ = writeln!(console.err, "Error: {}", crate::core::slim::PS_FAILED);
        return ExitCode::FAILURE;
    };

    let history_session = (!cli.dry_run).then(|| {
        let history = HistoryLog::new(layout.history_path());
        let mode = if cli.force { "force" } else { "interactive" };
        history.start_run("slim", mode, Some(selected.len()))
    });

    let rules = crate::core::slim::SlimRules {
        rosetta_apps: &rosetta,
        running: Some(&running),
        skip_developer_id: false,
        system_owner,
    };
    for app in &selected {
        let eligible = match crate::core::slim::check_eligibility(app, host, layout, ignore, &rules)
        {
            Ok(eligible) => eligible,
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
                if let Some(session) = history_session {
                    session.finish();
                }
                return ExitCode::FAILURE;
            }
        };

        let intel_size: u64 = eligible
            .binaries
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
                let _ = writeln!(console.out, "{}", crate::core::slim::TRASH_NOTE);
            }
            continue;
        }

        if !cli.force {
            if cli.json {
                let err = serde_json::json!({
                    "error": "confirmation needed, pass --force",
                    "app": app.name,
                });
                let _ = writeln!(
                    console.out,
                    "{}",
                    serde_json::to_string_pretty(&err).unwrap_or_default()
                );
                if let Some(session) = history_session {
                    session.finish();
                }
                return ExitCode::FAILURE;
            }
            let _ = write!(console.out, "Slim {}? [y/N]: ", app.name);
            let _ = console.out.flush();
            let mut input = String::new();
            if console.input.read_line(&mut input).is_err()
                || !input.trim().eq_ignore_ascii_case("y")
            {
                let _ = writeln!(console.out, "Cancelled.");
                if let Some(session) = history_session {
                    session.finish();
                }
                return ExitCode::SUCCESS;
            }
        }

        let Some(session) = history_session.as_ref() else {
            unreachable!("a dry run continues before the slim step");
        };
        let slim_res = slim_one(context, session, app, eligible.needs_admin);
        match slim_res {
            Ok(done) => {
                if cli.json {
                    let mut report = serde_json::json!({
                        "name": app.name,
                        "path": app.path,
                        "saved_bytes": done.saved,
                        "success": true,
                    });
                    if let Some((old_path, _)) = &done.old_app {
                        report["old_app_path"] = serde_json::json!(old_path);
                    }
                    let _ = writeln!(
                        console.out,
                        "{}",
                        serde_json::to_string_pretty(&report).unwrap_or_default()
                    );
                } else {
                    let _ = writeln!(console.out, "{}", done.summary(&app.name));
                    if done.old_app.is_none() {
                        let _ = writeln!(console.out, "{}", crate::core::slim::TRASH_NOTE);
                    }
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
                if let Some(session) = history_session {
                    session.finish();
                }
                return ExitCode::FAILURE;
            }
        }
    }

    if let Some(session) = history_session {
        session.finish();
    }

    ExitCode::SUCCESS
}

/// Slims one app: an app that belongs to root goes to the root helper after one password prompt.
fn slim_one(
    context: &ScanContext,
    session: &crate::core::history::HistorySession,
    app: &InstalledApp,
    needs_admin: bool,
) -> Result<crate::core::slim::SlimDone, (usize, String)> {
    if needs_admin {
        context
            .admin
            .prepare(context.host, crate::core::slim::ADMIN_REASON)
            .map_err(|error| (1, error))?;
    }
    crate::core::slim::slim_logged(session, context.host, context.layout, app, needs_admin)
}

fn pick_apps_to_slim(
    cli: &Cli,
    use_plain: bool,
    ignore: &IgnoreStore,
    context: &ScanContext,
    system_owner: u32,
    console: &mut Console,
) -> ExitCode {
    let ScanContext { layout, host, .. } = *context;
    if !use_plain && !cli.json && !cli.dry_run {
        let mut app =
            ui::app::App::new(ignore.clone(), saved_config(layout)).with_permanent(cli.permanent);
        app.launch_slim_scan();
        return run_tui(&mut app, cli, console);
    }

    if cli.json {
        let installed = crate::core::slim::candidate_apps(layout);
        let rosetta = crate::core::slim::rosetta_bundle_ids(layout);
        let running = crate::core::processes::read(host);

        let infos: Vec<crate::core::slim::SlimAppInfo> = installed
            .into_iter()
            .map(|app| {
                let eligible = running
                    .as_ref()
                    .ok_or_else(|| crate::core::slim::PS_FAILED.to_string())
                    .and_then(|running| {
                        let rules = crate::core::slim::SlimRules {
                            rosetta_apps: &rosetta,
                            running: Some(running),
                            skip_developer_id: false,
                            system_owner,
                        };
                        crate::core::slim::check_eligibility(&app, host, layout, ignore, &rules)
                    });
                match eligible {
                    Ok(eligible) => {
                        let intel_bytes: u64 = eligible
                            .binaries
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
                            needs_admin: eligible.needs_admin,
                        }
                    }
                    Err(reason) => crate::core::slim::SlimAppInfo {
                        name: app.name,
                        path: app.path,
                        bundle_id: app.bundle_id,
                        intel_bytes: 0,
                        eligible: false,
                        reason: Some(reason),
                        needs_admin: false,
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

    let mut scan = crate::core::slim::slim_scan(layout, host, ignore, system_owner, None, None);

    let _ = write!(console.out, "{}", PlainRunner::scan_text(&scan));
    if cli.dry_run {
        let _ = writeln!(console.out, "\nDry run completed. No apps were modified.");
        return ExitCode::SUCCESS;
    }

    if let Err(error) = PlainRunner::prompt_selection(&mut scan, console) {
        let _ = writeln!(console.err, "Selection error: {error}");
        return ExitCode::FAILURE;
    }

    let selected = scan.selected_items();
    let selected_paths: Vec<_> = selected.iter().map(|item| item.path.clone()).collect();
    let of_root: Vec<_> = selected
        .iter()
        .filter(|item| item.needs_admin)
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

    let mode = if cli.force { "force" } else { "interactive" };
    let history = HistoryLog::new(layout.history_path());
    let session = history.start_run("slim", mode, Some(chosen.len()));

    let mut total_saved = 0u64;
    let mut any_trashed = false;
    for app in &chosen {
        match slim_one(context, &session, app, of_root.contains(&app.path)) {
            Ok(done) => {
                total_saved += done.saved;
                any_trashed |= done.old_app.is_none();
                let _ = writeln!(console.out, "{}", done.summary(&app.name));
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
    session.finish();

    if total_saved > 0 {
        let _ = write!(console.out, "\nTotal saved: {}.", format_bytes(total_saved));
        if any_trashed {
            let _ = write!(console.out, " {}", crate::core::slim::TRASH_NOTE);
        }
        let _ = writeln!(console.out);
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::apps::testing::{
        ACME_PACKAGE, ACME_PREFIX, fake_app, package_app, package_listing, sandbox,
    };
    use crate::core::homebrew::testing::{AUTOREMOVE_DRY_RUN, BREW_INFO, BREW_OUTDATED_BUN};
    use crate::core::host::fake::FakeHost;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("taert").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn flags_that_do_nothing_are_refused() {
        let commands: [(&[&str], &str); 10] = [
            (&["schedule"], "schedule"),
            (&["schedule", "show"], "schedule"),
            (&["schedule", "set"], "schedule"),
            (&["schedule", "off"], "schedule"),
            (&["schedule", "run"], "schedule"),
            (&["ignore", "list"], "ignore"),
            (&["ignore", "add", "/tmp/x"], "ignore"),
            (&["ignore", "remove", "/tmp/x"], "ignore"),
            (&["config"], "config"),
            (&["config", "--appearance", "dark"], "config"),
        ];
        for (command, name) in commands {
            let with = |flags: &[&str]| unsupported_flag(&parse(&[command, flags].concat()));
            let error = |flag: &str| Some(format!("{flag} does not work with taert {name}"));
            assert_eq!(with(&["--dry-run"]), error("--dry-run"), "{command:?}");
            assert_eq!(with(&["--json"]), error("--json"), "{command:?}");
            assert_eq!(
                with(&["--dry-run", "--json"]),
                error("--dry-run"),
                "{command:?}"
            );
        }
    }

    #[test]
    fn flags_stay_accepted_where_commands_use_them() {
        let commands: [&[&str]; 9] = [
            &["clean", "--dry-run"],
            &["clean", "junk", "--json"],
            &["startup", "disable", "com.acme.agent", "--dry-run"],
            &["brew", "uninstall", "fd", "--json"],
            &["history", "--json"],
            &["uninstall", "Foo", "--dry-run", "--force"],
            &["schedule", "set"],
            &["ignore", "list", "--plain"],
            &["config", "--force"],
        ];
        for command in commands {
            assert_eq!(unsupported_flag(&parse(command)), None, "{command:?}");
        }
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
        startup_ignoring(args, &IgnoreStore::default(), host, layout)
    }

    fn startup_ignoring(
        args: &[&str],
        ignore: &IgnoreStore,
        host: &FakeHost,
        layout: &Layout,
    ) -> Ran {
        let cli = parse(args);
        let Some(Command::Startup(ref startup_args)) = cli.command else {
            panic!("not a startup command");
        };
        let context = ScanContext {
            layout,
            host,
            admin: crate::core::admin::test_session(),
        };
        run("", |console| {
            handle_startup(
                startup_args,
                &cli,
                true,
                ignore,
                &Config::default(),
                &context,
                console,
            )
        })
    }

    fn slim(args: &[&str], host: &FakeHost, layout: &Layout) -> Ran {
        slim_typed(args, "", true, host, layout)
    }

    fn slim_typed(
        args: &[&str],
        typed: &str,
        use_plain: bool,
        host: &FakeHost,
        layout: &Layout,
    ) -> Ran {
        slim_typed_for(
            args,
            typed,
            use_plain,
            host,
            layout,
            crate::core::safety::SYSTEM_OWNER,
        )
    }

    fn slim_typed_for(
        args: &[&str],
        typed: &str,
        use_plain: bool,
        host: &FakeHost,
        layout: &Layout,
        system_owner: u32,
    ) -> Ran {
        let cli = parse(args);
        let Some(Command::Slim(ref slim_args)) = cli.command else {
            panic!("not a slim command");
        };
        let context = ScanContext {
            layout,
            host,
            admin: crate::core::admin::test_session(),
        };
        run(typed, |console| {
            handle_slim(
                slim_args,
                &cli,
                use_plain,
                &IgnoreStore::default(),
                &context,
                system_owner,
                console,
            )
        })
    }

    /// A universal app "Fat" that passes every check, and a Mac that can slim it.
    fn slim_ready(layout: &Layout) -> (FakeHost, std::path::PathBuf) {
        slim_ready_signed_by(layout, "Developer ID Application: Acme Corp (123)")
    }

    fn slim_ready_signed_by(layout: &Layout, authority: &str) -> (FakeHost, std::path::PathBuf) {
        slim_ready_at(layout.user("Applications/Fat.app"), authority)
    }

    fn slim_ready_at(app: std::path::PathBuf, authority: &str) -> (FakeHost, std::path::PathBuf) {
        use crate::core::slim::testing::{fake_ditto, write_test_app};
        write_test_app(&app, "com.vendor.fat");
        let host = apple_silicon_host();
        host.reply("/usr/bin/codesign -dv", &format!("Authority={authority}\n"));
        host.reply("/usr/bin/codesign --verify", "");
        host.reply("/usr/sbin/spctl", "");
        host.reply("/usr/bin/ditto", "");
        fake_ditto(&host, &app);
        (host, app)
    }

    fn doctor_report(args: &[&str], host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Doctor(ref doctor_args)) = cli.command else {
            panic!("not a doctor command");
        };
        let context = ScanContext {
            layout,
            host,
            admin: crate::core::admin::test_session(),
        };
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

    const BREW_INFO_LINE: &str = "/usr/bin/env HOMEBREW_NO_AUTO_UPDATE=1";

    fn brew_read(layout: &Layout, args: &str) -> String {
        let brew = layout.system("opt/homebrew/bin/brew");
        format!("{ENV_START} {} {args}", brew.display())
    }

    fn brew(args: &[&str], host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let context = ScanContext {
            layout,
            host,
            admin: crate::core::admin::test_session(),
        };
        run("", |console| {
            handle_brew(
                &cli,
                true,
                &IgnoreStore::default(),
                &Config::default(),
                &context,
                console,
            )
        })
    }

    const ENV_START: &str =
        "/usr/bin/env HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ENV_HINTS=1 NONINTERACTIVE=1";

    /// A Mac whose Cellar holds the two unused dependencies of the fixture, an old `bun` and an
    /// old `fd`, which is pinned.
    fn brew_cleanup_mac() -> (tempfile::TempDir, Layout, FakeHost) {
        let (temp, layout) = homebrew::testing::with_brew("opt/homebrew");
        let prefix = layout.system("opt/homebrew");
        for (folder, bytes) in [
            ("ada-url/4.0.0_1", 90_000),
            ("brotli/1.2.1", 40_000),
            ("bun/1.4.0", 20_000),
            ("bun/1.4.2", 5_000),
            ("fd/10.0.0", 8_000),
            ("fd/10.5.0", 5_000),
        ] {
            let folder = prefix.join("Cellar").join(folder);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("data"), vec![1_u8; bytes]).unwrap();
        }
        let brew = prefix.join("bin/brew");
        let old = |name: &str, version: &str| {
            format!(
                "Would remove: {}/Cellar/{name}/{version} (3 files, 9.9MB)\n",
                prefix.display()
            )
        };
        let cleanup = old("bun", "1.4.0") + &old("fd", "10.0.0");
        let host = FakeHost::default();
        host.reply(
            &format!("{ENV_START} {} info --json=v2 --installed", brew.display()),
            BREW_INFO,
        );
        host.reply(
            &format!("{ENV_START} {} autoremove --dry-run", brew.display()),
            AUTOREMOVE_DRY_RUN,
        );
        host.reply(
            &format!("{ENV_START} {} cleanup --dry-run", brew.display()),
            &cleanup,
        );
        (temp, layout, host)
    }

    /// Makes brew succeed at `args` and delete `folder`, like the real removal would.
    fn brew_removes(host: &FakeHost, layout: &Layout, args: &str, folder: &str) {
        let brew = layout.system("opt/homebrew/bin/brew");
        let line = format!(
            "{ENV_START} HOMEBREW_NO_AUTOREMOVE=1 {} {args}",
            brew.display()
        );
        host.reply(&line, "");
        let folder = layout.system("opt/homebrew/Cellar").join(folder);
        host.when(&line, move || std::fs::remove_dir_all(&folder).unwrap());
    }

    fn brew_cleanup(args: &[&str], typed: &str, host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let context = ScanContext {
            layout,
            host,
            admin: crate::core::admin::test_session(),
        };
        run(typed, |console| {
            handle_brew_cleanup(&cli, true, &IgnoreStore::default(), &context, console)
        })
    }

    fn removal_calls(host: &FakeHost) -> Vec<String> {
        host.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.contains("HOMEBREW_NO_AUTOREMOVE=1"))
            .cloned()
            .collect()
    }

    #[test]
    fn brew_cleanup_dry_run_shows_the_rows_and_runs_only_the_three_reads() {
        let (_temp, layout, host) = brew_cleanup_mac();

        let ran = brew_cleanup(&["brew", "cleanup", "--dry-run"], "", &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert!(ran.out.contains("=== Homebrew Cleanup ==="));
        assert!(ran.out.contains("[Unused dependencies]"));
        assert!(ran.out.contains("[Old versions]"));
        assert!(ran.out.contains("[x]"));
        assert!(ran.out.contains("ada-url 4.0.0_1"));
        assert!(ran.out.contains("bun 1.4.0"));
        assert!(ran.out.contains("fd 10.0.0"));
        let calls = host.calls.lock().unwrap();
        let tails: Vec<&str> = calls
            .iter()
            .map(|call| call.rsplit_once("bin/brew ").unwrap().1)
            .collect();
        assert_eq!(
            tails,
            [
                "info --json=v2 --installed",
                "autoremove --dry-run",
                "cleanup --dry-run"
            ]
        );
    }

    #[test]
    fn brew_cleanup_force_removes_the_selected_rows_and_logs_each_package() {
        let (_temp, layout, host) = brew_cleanup_mac();
        let cellar = layout.system("opt/homebrew/Cellar");
        let before = |name: &str| DirSizer::size_of(&cellar.join(name));
        let (ada, brotli, bun) = (before("ada-url"), before("brotli"), before("bun"));
        brew_removes(&host, &layout, "uninstall ada-url", "ada-url");
        brew_removes(&host, &layout, "uninstall brotli", "brotli");
        brew_removes(&host, &layout, "cleanup bun", "bun/1.4.0");

        let ran = brew_cleanup(&["brew", "cleanup", "--force"], "", &host, &layout);

        assert!(ran.ok, "{}{}", ran.out, ran.err);
        let mut removals: Vec<String> = removal_calls(&host)
            .iter()
            .map(|call| call.rsplit_once("bin/brew ").unwrap().1.to_string())
            .collect();
        removals.sort();
        assert_eq!(
            removals,
            ["cleanup bun", "uninstall ada-url", "uninstall brotli"]
        );
        assert!(cellar.join("fd/10.0.0").exists());

        let runs = runs_in(&layout);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].command, "brew cleanup");
        assert_eq!(runs[0].mode, "force");
        let mut items: Vec<(String, u64)> = runs[0]
            .items
            .iter()
            .inspect(|item| assert_eq!(item.outcome, crate::core::history::OutcomeKind::Removed))
            .map(|item| (item.path.clone(), item.bytes))
            .collect();
        items.sort();
        let freed_bun = bun - DirSizer::size_of(&cellar.join("bun"));
        assert_eq!(
            items,
            [
                ("ada-url 4.0.0_1".to_string(), ada),
                ("brotli 1.2.1".to_string(), brotli),
                ("bun 1.4.0".to_string(), freed_bun),
            ]
        );
    }

    #[test]
    fn brew_cleanup_json_prints_the_scan_with_the_task_of_each_row() {
        let (_temp, layout, host) = brew_cleanup_mac();

        let ran = brew_cleanup(&["brew", "cleanup", "--json"], "", &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        let json: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(json["id"], "brew-cleanup");
        let groups = json["groups"].as_array().unwrap();
        assert_eq!(groups[0]["tool"], "Unused dependencies");
        assert_eq!(groups[1]["tool"], "Old versions");
        let brew = layout.system("opt/homebrew/bin/brew");
        let cellar = layout.system("opt/homebrew/Cellar");
        assert_eq!(
            groups[0]["items"][0]["brew"],
            serde_json::json!({
                "action": "uninstall",
                "name": "ada-url",
                "brew": brew,
                "cellar": cellar.join("ada-url"),
            })
        );
        let old = &groups[1]["items"];
        assert_eq!(old[0]["brew"]["action"], "cleanup");
        assert_eq!(old[0]["brew"]["name"], "bun");
        assert_eq!(old[1]["selected"], false);
        assert_eq!(host.calls.lock().unwrap().len(), 3);
    }

    #[test]
    fn a_plain_brew_cleanup_asks_first_and_a_no_removes_nothing() {
        let (_temp, layout, host) = brew_cleanup_mac();

        let ran = brew_cleanup(&["brew", "cleanup", "--plain"], "\nno\n", &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert!(ran.out.contains("Type 'yes' to confirm"));
        assert!(ran.out.ends_with("Operation cancelled.\n"));
        assert!(removal_calls(&host).is_empty());
        assert!(!layout.history_path().exists());
    }

    #[test]
    fn brew_cleanup_reports_a_failed_read_as_an_error() {
        let (_temp, layout) = homebrew::testing::with_brew("opt/homebrew");
        let host = FakeHost::default();
        host.fail(ENV_START, "Error: broken tap\n");

        let ran = brew_cleanup(&["brew", "cleanup", "--dry-run"], "", &host, &layout);

        assert!(!ran.ok);
        assert_eq!(
            ran.err,
            "Error: Homebrew did not list its packages: Error: broken tap\n"
        );
        assert!(ran.out.is_empty());
    }

    fn brew_uninstall(args: &[&str], typed: &str, host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Brew(crate::cli::BrewArgs {
            action: Some(BrewAction::Uninstall { ref name }),
        })) = cli.command
        else {
            panic!("not a brew uninstall command");
        };
        let context = ScanContext {
            layout,
            host,
            admin: crate::core::admin::test_session(),
        };
        run(typed, |console| {
            handle_brew_uninstall(name, &cli, &IgnoreStore::default(), &context, console)
        })
    }

    fn brotli_size(layout: &Layout) -> u64 {
        DirSizer::size_of(&layout.system("opt/homebrew/Cellar/brotli"))
    }

    fn brotli_question(layout: &Layout) -> String {
        format!(
            "Remove brotli 1.2.1 ({}) with brew uninstall? [y/N] ",
            crate::core::bytes::format_bytes(brotli_size(layout))
        )
    }

    fn mentions_uninstall(host: &FakeHost) -> bool {
        host.calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.contains("uninstall"))
    }

    #[test]
    fn brew_uninstall_refuses_a_cask_a_used_formula_and_a_pinned_one_even_with_force() {
        let refusals = [
            (
                "ghostty",
                "Error: ghostty is a cask. Remove it with taert uninstall.\n",
            ),
            ("go", "Error: go stays: golangci-lint, gosec use it.\n"),
            ("fd", "Error: fd is pinned. Run brew unpin fd first.\n"),
        ];

        for (name, text) in refusals {
            let (_temp, layout, host) = brew_cleanup_mac();

            let ran = brew_uninstall(&["brew", "uninstall", name, "--force"], "", &host, &layout);

            assert!(!ran.ok, "{name}");
            assert_eq!(ran.err, text);
            assert!(ran.out.is_empty(), "{}", ran.out);
            assert!(!mentions_uninstall(&host), "{name}");
            assert!(!layout.history_path().exists());
        }
    }

    #[test]
    fn brew_uninstall_reports_a_missing_name_and_a_missing_homebrew_as_errors() {
        let (_temp, layout, host) = brew_cleanup_mac();

        let ran = brew_uninstall(
            &["brew", "uninstall", "nope", "--force"],
            "",
            &host,
            &layout,
        );

        assert!(!ran.ok);
        assert_eq!(ran.err, "Error: nope is not installed.\n");
        assert!(!mentions_uninstall(&host));

        let (_temp, layout) = sandbox();
        let host = FakeHost::default();

        let ran = brew_uninstall(
            &["brew", "uninstall", "nope", "--force"],
            "",
            &host,
            &layout,
        );

        assert!(!ran.ok);
        assert_eq!(ran.err, "Error: Homebrew is not installed.\n");
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn brew_uninstall_force_removes_one_formula_and_logs_it() {
        let (_temp, layout, host) = brew_cleanup_mac();
        brew_removes(&host, &layout, "uninstall brotli", "brotli");
        let before = brotli_size(&layout);

        let ran = brew_uninstall(
            &["brew", "uninstall", "brotli", "--force"],
            "",
            &host,
            &layout,
        );

        assert!(ran.ok, "{}{}", ran.out, ran.err);
        let brew = layout.system("opt/homebrew/bin/brew");
        assert_eq!(
            removal_calls(&host),
            [format!(
                "{ENV_START} HOMEBREW_NO_AUTOREMOVE=1 {} uninstall brotli",
                brew.display()
            )]
        );
        assert!(!ran.out.contains("[y/N]"));
        let runs = runs_in(&layout);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].command, "brew uninstall brotli");
        assert_eq!(runs[0].mode, "force");
        assert_eq!(runs[0].items.len(), 1);
        assert_eq!(
            runs[0].items[0].outcome,
            crate::core::history::OutcomeKind::Removed
        );
        assert_eq!(runs[0].items[0].path, "brotli 1.2.1");
        assert_eq!(runs[0].items[0].bytes, before);
    }

    #[test]
    fn brew_uninstall_fails_when_the_run_is_refused_because_a_formula_uses_it_by_then() {
        let (_temp, layout, host) = brew_cleanup_mac();
        let brew = layout.system("opt/homebrew/bin/brew");
        host.reply(
            &format!("{ENV_START} {} info --json=v2 --installed", brew.display()),
            &BREW_INFO.replacen(
                "\"dependencies\": [\n        \"shellcheck\"",
                "\"dependencies\": [\n        \"brotli\",\n        \"shellcheck\"",
                1,
            ),
        );

        let ran = brew_uninstall(
            &["brew", "uninstall", "brotli", "--force"],
            "",
            &host,
            &layout,
        );

        assert!(!ran.ok, "{}", ran.out);
        assert!(
            ran.err.contains("brotli stays: actionlint use it."),
            "{}",
            ran.err
        );
        assert!(removal_calls(&host).is_empty());
        let runs = runs_in(&layout);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].command, "brew uninstall brotli");
        assert_eq!(runs[0].items.len(), 1);
        assert_eq!(
            runs[0].items[0].outcome,
            crate::core::history::OutcomeKind::Failed
        );
    }

    #[test]
    fn brew_uninstall_asks_first_and_only_a_yes_removes() {
        for (typed, removes) in [
            ("y\n", true),
            (" Yes \n", true),
            ("YES\n", true),
            ("n\n", false),
            ("\n", false),
            ("yep\n", false),
            ("", false),
        ] {
            let (_temp, layout, host) = brew_cleanup_mac();
            brew_removes(&host, &layout, "uninstall brotli", "brotli");
            let question = brotli_question(&layout);

            let ran = brew_uninstall(&["brew", "uninstall", "brotli"], typed, &host, &layout);

            assert!(ran.ok, "{typed:?}: {}{}", ran.out, ran.err);
            assert!(ran.out.starts_with(&question), "{typed:?}: {}", ran.out);
            assert_eq!(
                removal_calls(&host).len(),
                usize::from(removes),
                "{typed:?}"
            );
            if removes {
                assert_eq!(runs_in(&layout)[0].mode, "interactive");
                assert_eq!(runs_in(&layout)[0].items[0].path, "brotli 1.2.1");
            } else {
                assert!(ran.out.ends_with("Operation cancelled.\n"), "{typed:?}");
                assert!(!mentions_uninstall(&host));
                assert!(!layout.history_path().exists());
            }
        }
    }

    #[test]
    fn brew_uninstall_dry_run_only_says_what_it_would_do() {
        let (_temp, layout, host) = brew_cleanup_mac();

        let ran = brew_uninstall(
            &["brew", "uninstall", "brotli", "--dry-run"],
            "",
            &host,
            &layout,
        );

        assert!(ran.ok, "{}", ran.err);
        let size = crate::core::bytes::format_bytes(brotli_size(&layout));
        assert_eq!(
            ran.out,
            format!("Would remove brotli 1.2.1 ({size}) with brew uninstall.\n")
        );
        assert!(!mentions_uninstall(&host));
        assert!(!layout.history_path().exists());
    }

    #[test]
    fn brew_uninstall_json_prints_the_plan_and_removes_nothing() {
        let (_temp, layout, host) = brew_cleanup_mac();

        let ran = brew_uninstall(
            &["brew", "uninstall", "brotli", "--json"],
            "",
            &host,
            &layout,
        );

        assert!(ran.ok, "{}", ran.err);
        let json: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(json["id"], "brew-uninstall");
        let row = &json["groups"][0]["items"][0];
        assert_eq!(row["selected"], true);
        assert_eq!(row["brew"]["action"], "uninstall");
        assert_eq!(row["brew"]["name"], "brotli");
        assert_eq!(
            row["brew"]["cellar"],
            serde_json::json!(layout.system("opt/homebrew/Cellar/brotli"))
        );
        assert!(!mentions_uninstall(&host));
        assert!(!layout.history_path().exists());
    }

    const UPGRADE_OUTDATED: &str = r#"{"formulae": [
 {"name": "bun", "current_version": "1.4.4_1"},
 {"name": "fd", "current_version": "10.6.0", "pinned": true}],
 "casks": [{"name": "ghostty", "current_version": "1.3.2"}]}"#;

    /// A Mac whose `brew update`, `brew info`, `brew outdated`, and upgrades answer.
    fn brew_upgrade_mac(outdated: &str) -> (tempfile::TempDir, Layout, FakeHost) {
        brew_upgrade_mac_failing(outdated, &[])
    }

    /// Like `brew_upgrade_mac`, but each `(arguments, stderr)` of `failing` fails.
    fn brew_upgrade_mac_failing(
        outdated: &str,
        failing: &[(&str, &str)],
    ) -> (tempfile::TempDir, Layout, FakeHost) {
        let (temp, layout) = homebrew::testing::with_brew("opt/homebrew");
        let host = FakeHost::default();
        let removing = format!("{ENV_START} HOMEBREW_NO_AUTOREMOVE=1");
        let brew = layout.system("opt/homebrew/bin/brew");
        for (args, stderr) in failing {
            let line = if args.starts_with("upgrade") {
                format!("{removing} {} {args}", brew.display())
            } else {
                brew_read(&layout, args)
            };
            host.fail(&line, stderr);
        }
        host.reply(&brew_read(&layout, "update"), "");
        host.reply(&brew_read(&layout, "info --json=v2 --installed"), BREW_INFO);
        host.reply(&brew_read(&layout, "outdated --json=v2"), outdated);
        for kind in ["formula", "cask"] {
            host.reply(
                &format!("{removing} {} upgrade --{kind}", brew.display()),
                "",
            );
        }
        (temp, layout, host)
    }

    fn brew_upgrade(args: &[&str], typed: &str, host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Brew(crate::cli::BrewArgs {
            action: Some(BrewAction::Upgrade { ref name }),
        })) = cli.command
        else {
            panic!("not a brew upgrade command");
        };
        let context = ScanContext {
            layout,
            host,
            admin: crate::core::admin::test_session(),
        };
        run(typed, |console| {
            handle_brew_upgrade(name.as_deref(), &cli, &context, console)
        })
    }

    /// What brew was asked to do, without the environment and the path of brew.
    fn brew_commands(host: &FakeHost) -> Vec<String> {
        host.calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.rsplit_once("bin/brew ").unwrap().1.to_string())
            .collect()
    }

    fn upgrades(host: &FakeHost) -> Vec<String> {
        brew_commands(host)
            .into_iter()
            .filter(|command| command.starts_with("upgrade "))
            .collect()
    }

    const READS: [&str; 2] = ["info --json=v2 --installed", "outdated --json=v2"];

    #[test]
    fn brew_upgrade_force_upgrades_one_formula() {
        let (_temp, layout, host) = brew_upgrade_mac(UPGRADE_OUTDATED);

        let ran = brew_upgrade(&["brew", "upgrade", "bun", "--force"], "", &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert_eq!(ran.out, "Upgraded bun to 1.4.4_1.\n");
        assert!(ran.err.is_empty());
        assert_eq!(
            brew_commands(&host),
            ["update", READS[0], READS[1], "upgrade --formula bun"]
        );
        let brew = layout.system("opt/homebrew/bin/brew");
        assert_eq!(
            host.calls.lock().unwrap()[3],
            format!(
                "{ENV_START} HOMEBREW_NO_AUTOREMOVE=1 {} upgrade --formula bun",
                brew.display()
            )
        );
        assert!(!layout.history_path().exists());
    }

    #[test]
    fn brew_upgrade_without_a_name_upgrades_every_outdated_package_but_a_pinned_one() {
        let (_temp, layout, host) = brew_upgrade_mac(UPGRADE_OUTDATED);

        let ran = brew_upgrade(&["brew", "upgrade", "--force"], "", &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert_eq!(
            ran.out,
            "Upgraded bun to 1.4.4_1.\nUpgraded ghostty to 1.3.2.\n"
        );
        assert_eq!(
            brew_commands(&host),
            [
                "update",
                READS[0],
                READS[1],
                "upgrade --formula bun",
                "upgrade --cask ghostty"
            ]
        );
        assert!(!layout.history_path().exists());
    }

    #[test]
    fn brew_upgrade_without_a_name_and_nothing_to_upgrade_says_so() {
        let (_temp, layout, host) = brew_upgrade_mac(r#"{"formulae": [], "casks": []}"#);

        let ran = brew_upgrade(&["brew", "upgrade"], "", &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert_eq!(ran.out, "Everything is up to date.\n");
        assert!(upgrades(&host).is_empty());
    }

    #[test]
    fn brew_upgrade_refuses_a_missing_an_up_to_date_and_a_pinned_package_even_with_force() {
        let json = r#"{"formulae": [{"name": "fd", "current_version": "10.6.0", "pinned": true}],
 "casks": [{"name": "ghostty", "current_version": "1.3.2", "pinned": true}]}"#;
        let refusals = [
            ("nope", "Error: nope is not installed.\n"),
            ("go", "Error: go is up to date.\n"),
            ("fd", "Error: fd is pinned. Run brew unpin fd first.\n"),
            (
                "ghostty",
                "Error: ghostty is pinned. Run brew unpin ghostty first.\n",
            ),
        ];

        for (name, text) in refusals {
            let (_temp, layout, host) = brew_upgrade_mac(json);

            let ran = brew_upgrade(&["brew", "upgrade", name, "--force"], "", &host, &layout);

            assert!(!ran.ok, "{name}");
            assert_eq!(ran.err, text);
            assert!(ran.out.is_empty(), "{}", ran.out);
            assert!(upgrades(&host).is_empty(), "{name}");
            assert!(!layout.history_path().exists());
        }
    }

    #[test]
    fn brew_upgrade_without_homebrew_is_an_error() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();

        let ran = brew_upgrade(&["brew", "upgrade", "bun", "--force"], "", &host, &layout);

        assert!(!ran.ok);
        assert_eq!(ran.err, "Error: Homebrew is not installed.\n");
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn brew_upgrade_asks_first_and_only_a_yes_upgrades() {
        for (typed, upgrades_it) in [
            ("y\n", true),
            (" Yes \n", true),
            ("YES\n", true),
            ("n\n", false),
            ("\n", false),
            ("yep\n", false),
            ("", false),
        ] {
            let (_temp, layout, host) = brew_upgrade_mac(UPGRADE_OUTDATED);

            let ran = brew_upgrade(&["brew", "upgrade"], typed, &host, &layout);

            assert!(ran.ok, "{typed:?}: {}{}", ran.out, ran.err);
            assert!(
                ran.out.starts_with(
                    "bun 1.4.2 → 1.4.4_1\nghostty 1.3.1 → 1.3.2\nUpgrade 2 packages? [y/N] "
                ),
                "{typed:?}: {}",
                ran.out
            );
            assert_eq!(upgrades(&host).len(), if upgrades_it { 2 } else { 0 });
            if !upgrades_it {
                assert!(ran.out.ends_with("Operation cancelled.\n"), "{typed:?}");
            }
        }
    }

    #[test]
    fn brew_upgrade_of_one_package_asks_with_a_singular() {
        let (_temp, layout, host) = brew_upgrade_mac(UPGRADE_OUTDATED);

        let ran = brew_upgrade(&["brew", "upgrade", "bun"], "n\n", &host, &layout);

        assert!(
            ran.out
                .starts_with("bun 1.4.2 → 1.4.4_1\nUpgrade 1 package? [y/N] "),
            "{}",
            ran.out
        );
    }

    #[test]
    fn brew_upgrade_dry_run_runs_no_update_and_no_upgrade() {
        let (_temp, layout, host) = brew_upgrade_mac(UPGRADE_OUTDATED);

        let ran = brew_upgrade(&["brew", "upgrade", "--dry-run"], "", &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert_eq!(
            ran.out,
            "Would upgrade bun 1.4.2 → 1.4.4_1.\nWould upgrade ghostty 1.3.1 → 1.3.2.\n"
        );
        assert_eq!(brew_commands(&host), READS);
        assert!(!layout.history_path().exists());
    }

    #[test]
    fn brew_upgrade_json_prints_the_plan_and_runs_no_update_and_no_upgrade() {
        let (_temp, layout, host) = brew_upgrade_mac(UPGRADE_OUTDATED);

        let ran = brew_upgrade(&["brew", "upgrade", "--json"], "", &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        let json: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"name": "bun", "kind": "formula", "version": "1.4.2", "latest_version": "1.4.4_1"},
                {"name": "ghostty", "kind": "cask", "version": "1.3.1", "latest_version": "1.3.2"},
            ])
        );
        assert_eq!(brew_commands(&host), READS);
    }

    #[test]
    fn a_failed_update_is_reported_and_the_upgrade_goes_on() {
        let (_temp, layout, host) = brew_upgrade_mac_failing(
            UPGRADE_OUTDATED,
            &[("update", "Warning: x\nError: Could not resolve host\n")],
        );

        let ran = brew_upgrade(&["brew", "upgrade", "bun", "--force"], "", &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert_eq!(
            ran.err,
            "Could not check for updates: Error: Could not resolve host\n"
        );
        assert_eq!(ran.out, "Upgraded bun to 1.4.4_1.\n");
        assert_eq!(
            brew_commands(&host),
            ["update", READS[0], READS[1], "upgrade --formula bun"]
        );
    }

    #[test]
    fn a_failed_update_is_reported_before_a_refusal() {
        let (_temp, layout, host) = brew_upgrade_mac_failing(
            UPGRADE_OUTDATED,
            &[("update", "Error: Could not resolve host\n")],
        );

        let ran = brew_upgrade(&["brew", "upgrade", "nope", "--force"], "", &host, &layout);

        assert!(!ran.ok);
        assert_eq!(
            ran.err,
            "Could not check for updates: Error: Could not resolve host\nError: nope is not installed.\n"
        );
    }

    #[test]
    fn a_failed_upgrade_does_not_stop_the_next_one() {
        let (_temp, layout, host) = brew_upgrade_mac_failing(
            UPGRADE_OUTDATED,
            &[("upgrade --formula bun", "Error: broken\n")],
        );

        let ran = brew_upgrade(&["brew", "upgrade", "--force"], "", &host, &layout);

        assert!(!ran.ok);
        assert_eq!(ran.out, "Upgraded ghostty to 1.3.2.\n");
        assert_eq!(ran.err, "Could not upgrade bun: Error: broken\n");
        assert_eq!(
            upgrades(&host),
            ["upgrade --formula bun", "upgrade --cask ghostty"]
        );
    }

    #[test]
    fn a_failed_cask_upgrade_says_how_to_run_it_in_a_terminal() {
        let (_temp, layout, host) = brew_upgrade_mac_failing(
            UPGRADE_OUTDATED,
            &[(
                "upgrade --cask ghostty",
                "Error: sudo: a password is required\n",
            )],
        );

        let ran = brew_upgrade(&["brew", "upgrade", "--force"], "", &host, &layout);

        assert!(!ran.ok);
        assert_eq!(ran.out, "Upgraded bun to 1.4.4_1.\n");
        assert_eq!(
            ran.err,
            "Could not upgrade ghostty: Error: sudo: a password is required If it needs your password, run brew upgrade --cask ghostty in a terminal.\n"
        );
        assert_eq!(
            brew_commands(&host),
            [
                "update",
                READS[0],
                READS[1],
                "upgrade --formula bun",
                "upgrade --cask ghostty"
            ]
        );
    }

    #[test]
    fn a_failed_formula_upgrade_has_no_terminal_hint() {
        let (_temp, layout, host) = brew_upgrade_mac_failing(
            UPGRADE_OUTDATED,
            &[("upgrade --formula bun", "Error: broken\n")],
        );

        let ran = brew_upgrade(&["brew", "upgrade", "bun", "--force"], "", &host, &layout);

        assert!(!ran.ok);
        assert_eq!(ran.err, "Could not upgrade bun: Error: broken\n");
    }

    #[test]
    fn brew_json_has_the_documented_keys() {
        let (_temp, layout) = homebrew::testing::with_brew("opt/homebrew");
        let host = FakeHost::default();
        host.reply(&brew_read(&layout, "info"), BREW_INFO);
        host.reply(&brew_read(&layout, "outdated"), BREW_OUTDATED_BUN);

        let ran = brew(&["brew", "--json"], &host, &layout);

        assert!(ran.ok);
        let json: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        let packages = json["packages"].as_array().unwrap();
        assert_eq!(packages.len(), 12);
        assert_eq!(json["notes"], serde_json::json!([]));
        let mut keys: Vec<&str> = packages[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "apps",
                "kind",
                "latest_version",
                "name",
                "on_request",
                "outdated",
                "pinned",
                "size_bytes",
                "used_by",
                "version"
            ]
        );
        let ghostty = packages.iter().find(|p| p["name"] == "ghostty").unwrap();
        assert_eq!(ghostty["kind"], "cask");
        assert_eq!(ghostty["apps"], serde_json::json!(["Ghostty.app"]));
        let bun = packages.iter().find(|p| p["name"] == "bun").unwrap();
        assert_eq!(bun["latest_version"], "1.4.4_1");
        assert_eq!(ghostty["latest_version"], serde_json::Value::Null);
        let go = packages.iter().find(|p| p["name"] == "go").unwrap();
        assert_eq!(go["kind"], "formula");
        assert_eq!(go["used_by"], serde_json::json!(["golangci-lint", "gosec"]));
    }

    #[test]
    fn brew_plain_prints_one_row_per_package() {
        let (_temp, layout) = homebrew::testing::with_brew("opt/homebrew");
        let host = FakeHost::default();
        host.reply(&brew_read(&layout, "info"), BREW_INFO);
        host.reply(&brew_read(&layout, "outdated"), BREW_OUTDATED_BUN);

        let ran = brew(&["brew", "--plain"], &host, &layout);

        assert!(ran.ok);
        assert!(ran.out.starts_with("Formulae\n"));
        assert!(ran.out.contains("\nCasks (remove with taert uninstall)\n"));
        let row = |name: &str| {
            ran.out
                .lines()
                .find(|line| line.trim_start().starts_with(&format!("{name} ")))
                .unwrap()
        };
        let go: Vec<&str> = row("go").split_whitespace().collect();
        assert_eq!(&go[..2], ["go", "1.27.3"]);
        assert_eq!(&go[4..], ["on", "request", "-", "-", "2"]);
        assert!(row("bun").contains("1.4.4_1"));
        assert!(!row("bun").contains("outdated"));
        assert!(row("fd").contains("pinned"));
        assert!(row("ada-url").contains("dependency"));
        assert_eq!(ran.out.lines().filter(|l| l.starts_with("  ")).count(), 12);
    }

    #[test]
    fn brew_json_and_plain_never_run_brew_update() {
        for flag in ["--json", "--plain"] {
            let (_temp, layout) = homebrew::testing::with_brew("opt/homebrew");
            let host = FakeHost::default();
            host.reply(&brew_read(&layout, "info"), BREW_INFO);
            host.reply(&brew_read(&layout, "outdated"), BREW_OUTDATED_BUN);

            let ran = brew(&["brew", flag], &host, &layout);

            assert!(ran.ok);
            assert!(!host.ran(&brew_read(&layout, "update")), "{flag}");
            assert_eq!(host.calls.lock().unwrap().len(), 2, "{flag}");
        }
    }

    #[test]
    fn brew_plain_says_when_homebrew_is_missing() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();

        let ran = brew(&["brew", "--plain"], &host, &layout);

        assert!(ran.ok);
        assert_eq!(ran.out, "Note: Homebrew is not installed.\n");
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn brew_json_says_when_homebrew_is_missing() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();

        let ran = brew(&["brew", "--json"], &host, &layout);

        assert!(ran.ok);
        let json: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(json["packages"], serde_json::json!([]));
        assert_eq!(
            json["notes"],
            serde_json::json!(["Homebrew is not installed."])
        );
    }

    #[test]
    fn brew_reports_a_failed_read_as_an_error_in_every_mode() {
        let (_temp, layout) = homebrew::testing::with_brew("opt/homebrew");
        let host = FakeHost::default();
        host.fail(BREW_INFO_LINE, "Error: broken tap");

        for mode in ["--plain", "--json"] {
            let ran = brew(&["brew", mode], &host, &layout);

            assert!(!ran.ok);
            assert!(ran.out.is_empty());
            assert_eq!(
                ran.err,
                "Error: Homebrew did not list its packages: Error: broken tap\n"
            );
        }
    }

    fn uninstall(args: &[&str], host: &FakeHost, layout: &Layout) -> Ran {
        uninstall_typing(args, "", host, layout)
    }

    fn uninstall_typing(args: &[&str], typed: &str, host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Uninstall(ref uninstall_args)) = cli.command else {
            panic!("not an uninstall command");
        };
        let context = ScanContext {
            layout,
            host,
            admin: crate::core::admin::test_session(),
        };
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
        let context = ScanContext {
            layout,
            host,
            admin: crate::core::admin::test_session(),
        };
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
        let host = crate::core::slim::testing::user_host();
        host.reply(
            "/usr/sbin/sysctl hw.optional.arm64",
            "hw.optional.arm64: 1\n",
        );
        host
    }

    #[test]
    fn schedule_set_and_off_use_the_given_host_and_layout() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.reply("/bin/launchctl", "");

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
    fn schedule_set_exits_with_failure_when_the_load_fails() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();
        host.fail("/bin/launchctl bootstrap", "Bootstrap failed: 5");

        let ran = schedule(&["schedule", "set"], &host, &layout);

        assert!(!ran.ok);
        assert!(
            ran.err.contains("Error: launchctl bootstrap failed"),
            "{}",
            ran.err
        );
        assert!(!layout.config_path().exists());
    }

    #[test]
    fn schedule_set_refuses_uninstall() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();

        let ran = schedule(
            &["schedule", "set", "--categories", "uninstall"],
            &host,
            &layout,
        );

        assert!(!ran.ok);
        assert!(ran.err.contains("cannot be scheduled"), "{}", ran.err);
        assert!(!schedule::agent_plist_path(&layout).exists());
        assert!(!layout.config_path().exists());
        assert!(!host.ran("/bin/launchctl"));
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
        let scan: serde_json::Value = serde_json::from_str(&json.out).unwrap();
        assert_eq!(scan["items"][0]["label"], "com.example.demo");
    }

    fn layout_with_a_daemon() -> (tempfile::TempDir, Layout, FakeHost) {
        let (temp, layout) = sandbox();
        let folder = layout.system("Library/LaunchDaemons");
        std::fs::create_dir_all(&folder).unwrap();
        let mut daemon = plist::Dictionary::new();
        daemon.insert("Label".into(), "com.example.daemon".into());
        daemon.insert("Program".into(), "/bin/ls".into());
        plist::Value::Dictionary(daemon)
            .to_file_xml(folder.join("com.example.daemon.plist"))
            .unwrap();
        let host = FakeHost::idle();
        *host.tty.lock().unwrap() = true;
        host.reply(
            "/bin/launchctl print system",
            "system = {\n services = {\n 0 - com.example.daemon\n }\n}\n",
        );
        host.reply(
            "/bin/launchctl print-disabled system",
            "disabled services = {\n \"com.example.daemon\" => enabled\n}\n",
        );
        (temp, layout, host)
    }

    #[test]
    fn startup_switching_a_daemon_asks_for_the_password_first() {
        for action in ["disable", "enable"] {
            let (_temp, layout, host) = layout_with_a_daemon();

            startup(&["startup", action, "com.example.daemon"], &host, &layout);

            assert_eq!(host.sudo_asks(), 1, "{action}");
            assert!(host.ran("sudo -v switch system daemons"), "{action}");
            assert_eq!(host.admin_scripts.lock().unwrap().len(), 1, "{action}");
        }
    }

    #[test]
    fn startup_stops_at_a_refused_password_before_any_root_step() {
        let (_temp, layout, host) = layout_with_a_daemon();
        *host.sudo_refused.lock().unwrap() = true;

        let ran = startup(
            &["startup", "disable", "com.example.daemon"],
            &host,
            &layout,
        );

        assert!(!ran.ok);
        assert!(
            ran.err.contains(crate::core::admin::NOT_GIVEN),
            "{}",
            ran.err
        );
        assert_eq!(host.sudo_asks(), 1);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }

    #[test]
    fn startup_switching_a_read_only_daemon_does_not_ask_for_the_password() {
        let (_temp, layout, host) = layout_with_a_daemon();
        let ignore = IgnoreStore {
            roots: vec!["/bin/ls".into()],
        };

        let ran = startup_ignoring(
            &["startup", "disable", "com.example.daemon"],
            &ignore,
            &host,
            &layout,
        );

        assert!(!ran.ok);
        assert_eq!(host.sudo_asks(), 0, "{}", ran.err);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }

    #[test]
    fn startup_dry_run_does_not_ask_for_the_password() {
        let (_temp, layout, host) = layout_with_a_daemon();

        let ran = startup(
            &["startup", "disable", "com.example.daemon", "--dry-run"],
            &host,
            &layout,
        );

        assert!(ran.ok, "{}", ran.err);
        assert_eq!(host.sudo_asks(), 0);
    }

    #[test]
    fn startup_switching_a_user_agent_does_not_ask_for_the_password() {
        let (_temp, layout) = layout_with_a_launch_agent();
        let host = FakeHost::idle();
        *host.tty.lock().unwrap() = true;

        startup(&["startup", "disable", "com.example.demo"], &host, &layout);

        assert_eq!(host.sudo_asks(), 0);
    }

    #[test]
    fn startup_all_asks_once_before_it_reads_the_background_records() {
        for args in [
            &["startup", "--all", "--json"][..],
            &["startup", "--all", "--plain"],
            &["startup", "--all", "--json", "--dry-run"],
        ] {
            let (_temp, layout, host) = layout_with_a_daemon();
            startup(args, &host, &layout);

            assert_eq!(host.sudo_asks(), 1, "{args:?}");
            assert!(host.ran("sudo -v read background task records"), "{args:?}");
            assert_eq!(host.admin_scripts.lock().unwrap().len(), 1, "{args:?}");
        }
    }

    #[test]
    fn startup_all_with_a_daemon_switch_asks_once() {
        let (_temp, layout, host) = layout_with_a_daemon();

        startup(
            &["startup", "--all", "disable", "com.example.daemon"],
            &host,
            &layout,
        );

        assert_eq!(host.sudo_asks(), 1);
        assert_eq!(host.admin_scripts.lock().unwrap().len(), 2);
    }

    const TOUCH_ID_TIP: &str = "Tip: sudo can use Touch ID.";

    fn enrolled(host: &FakeHost) {
        host.reply(
            "/usr/bin/bioutil -c",
            "User 501:\t2 biometric template(s)\n",
        );
    }

    #[test]
    fn the_touch_id_tip_prints_once_for_two_prepares_in_one_run() {
        let (_temp, layout, host) = layout_with_a_daemon();
        enrolled(&host);
        host.fail(SUDO_PROBE, "");

        let ran = startup(
            &["startup", "--all", "disable", "com.example.daemon"],
            &host,
            &layout,
        );

        assert_eq!(host.sudo_asks(), 1);
        assert_eq!(ran.err.matches(TOUCH_ID_TIP).count(), 1, "{}", ran.err);
        assert!(!ran.out.contains(TOUCH_ID_TIP));
    }

    #[test]
    fn no_touch_id_tip_when_taert_did_not_ask_for_the_password() {
        let (_temp, layout, host) = layout_with_a_daemon();
        enrolled(&host);
        host.reply(SUDO_PROBE, "");

        let ran = startup(
            &["startup", "disable", "com.example.daemon"],
            &host,
            &layout,
        );

        assert!(host.ran("/usr/bin/sudo -n true"));
        assert_eq!(host.sudo_asks(), 0);
        assert!(!ran.err.contains(TOUCH_ID_TIP), "{}", ran.err);
        assert_eq!(host.count("/usr/bin/bioutil"), 0);
    }

    #[test]
    fn a_forced_uninstall_prints_the_touch_id_tip_after_asking() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = password_host();
        with_a_receipt(&layout);
        enrolled(&host);

        let ran = uninstall(&["uninstall", "Taert Fixture", "--force"], &host, &layout);

        assert_eq!(host.sudo_asks(), 1);
        assert_eq!(ran.err.matches(TOUCH_ID_TIP).count(), 1, "{}", ran.err);
    }

    #[test]
    fn startup_all_stops_reading_at_a_refused_password() {
        let (_temp, layout, host) = layout_with_a_daemon();
        *host.sudo_refused.lock().unwrap() = true;

        startup(&["startup", "--all", "--json"], &host, &layout);

        assert_eq!(host.sudo_asks(), 1);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }

    #[test]
    fn startup_json_carries_the_notes() {
        let (_temp, layout) = layout_with_a_launch_agent();
        let host = FakeHost::idle();
        host.fail("/usr/bin/osascript", "Not authorized (-1743)");

        let json = startup(&["startup", "--json"], &host, &layout);

        let scan: serde_json::Value = serde_json::from_str(&json.out).unwrap();
        let note = scan["notes"][0].as_str().unwrap();
        assert!(note.contains("Automation permission"), "{note}");
    }

    #[test]
    fn startup_disable_refuses_a_name_that_fits_two_items() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::idle();
        host.reply(
            "/usr/bin/osascript",
            "Twin\t/Applications/A.app\nTwin\t/Applications/B.app\n",
        );

        let ran = startup(&["startup", "disable", "Twin"], &host, &layout);

        assert!(!ran.ok);
        assert!(ran.err.contains("fits several items"), "{}", ran.err);
        assert!(ran.err.contains("loginitem.Twin"));
        assert!(!host.ran("/bin/launchctl disable"));
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
        assert!(!host.ran("/usr/bin/codesign"));
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
        assert!(!host.ran("/usr/bin/codesign"));
    }

    #[test]
    fn slim_asks_before_rewriting_in_a_terminal() {
        let (_temp, layout) = sandbox();
        let (host, _app) = slim_ready(&layout);

        let ran = slim_typed(&["slim", "Fat"], "n\n", false, &host, &layout);

        assert!(ran.ok);
        assert!(ran.out.contains("Slim Fat? [y/N]: "));
        assert!(ran.out.contains("Cancelled."));
        assert!(!host.ran("/usr/bin/ditto"));
    }

    #[test]
    fn slim_goes_on_after_a_yes_in_a_terminal() {
        let (_temp, layout) = sandbox();
        let (host, _app) = slim_ready(&layout);

        let ran = slim_typed(&["slim", "Fat"], "y\n", false, &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert!(ran.out.contains("Slimmed Fat, saved"));
        assert!(host.ran("/usr/bin/ditto"));
    }

    #[test]
    fn slim_json_without_force_refuses() {
        let (_temp, layout) = sandbox();
        let (host, _app) = slim_ready(&layout);

        let ran = slim(&["slim", "Fat", "--json"], &host, &layout);

        assert!(!ran.ok);
        let json: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(json["error"], "confirmation needed, pass --force");
        assert_eq!(json["app"], "Fat");
        assert!(!host.ran("/usr/bin/ditto"));
    }

    #[test]
    fn slim_with_force_skips_the_question() {
        let (_temp, layout) = sandbox();
        let (host, _app) = slim_ready(&layout);

        let ran = slim_typed(&["slim", "Fat", "--force"], "", false, &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert!(!ran.out.contains("Slim Fat?"));
        assert!(host.ran("/usr/bin/ditto"));
    }

    /// The allocated size of the binary in an app made by `slim_ready`.
    fn binary_size(app: &std::path::Path) -> u64 {
        let binary = app.join("Contents/MacOS/testapp");
        crate::core::sizer::DirSizer::allocated_size(&std::fs::metadata(binary).unwrap())
    }

    /// Gives the app made by `slim_ready` an Intel slice large enough to free blocks.
    fn refused_trash_summary(app: &std::path::Path) -> String {
        let old = app.with_file_name("Fat.app.taert-slim-tmp/Fat.app");
        let done = crate::core::slim::SlimDone {
            saved: binary_size(&old).saturating_sub(binary_size(app)),
            old_app: Some((old, "denied".to_string())),
            holding_left: None,
            remover: crate::core::slim::Remover::User,
        };
        done.summary("Fat")
    }

    #[test]
    fn slim_says_where_the_old_app_is_when_the_trash_refuses() {
        let (_temp, layout) = sandbox();
        let (host, app) = slim_ready(&layout);
        host.fail_trash("denied");

        let plain = slim(&["slim", "Fat", "--force"], &host, &layout);

        assert!(plain.ok, "{}", plain.err);
        assert!(
            plain
                .out
                .contains(&format!("{}\n", refused_trash_summary(&app))),
            "{}",
            plain.out
        );
        assert!(!plain.out.contains("space comes back"));
    }

    #[test]
    fn slim_says_what_it_saved_when_the_old_app_is_in_the_trash() {
        let (_temp, layout) = sandbox();
        let (host, app) = slim_ready(&layout);
        crate::core::slim::testing::make_slim_save_space(&app);
        host.simulate_trash();
        let before = binary_size(&app);

        let ran = slim(&["slim", "Fat", "--force"], &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        let done = crate::core::slim::SlimDone {
            saved: before - binary_size(&app),
            old_app: None,
            holding_left: None,
            remover: crate::core::slim::Remover::User,
        };
        assert!(
            ran.out.contains(&format!(
                "{}\nClones share blocks, so the space comes back when the Trash is emptied.\n",
                done.summary("Fat")
            )),
            "{}",
            ran.out
        );
    }

    #[test]
    fn slim_names_the_holding_folder_it_could_not_remove() {
        let (_temp, layout) = sandbox();
        let (host, app) = slim_ready(&layout);
        let before = binary_size(&app);

        let ran = slim(&["slim", "Fat", "--force"], &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        let holding = app.with_file_name("Fat.app.taert-slim-tmp");
        let error = std::fs::remove_dir(&holding).unwrap_err().to_string();
        let done = crate::core::slim::SlimDone {
            saved: before.saturating_sub(binary_size(&app)),
            old_app: None,
            holding_left: Some((holding, error)),
            remover: crate::core::slim::Remover::User,
        };
        assert!(
            ran.out.contains(&format!("{}\n", done.summary("Fat"))),
            "{}",
            ran.out
        );
    }

    #[test]
    fn slim_picker_says_where_the_old_app_is_when_the_trash_refuses() {
        let (_temp, layout) = sandbox();
        let (host, app) = slim_ready(&layout);
        crate::core::slim::testing::make_slim_save_space(&app);
        host.fail_trash("denied");

        let ran = slim_typed(&["slim", "--force"], "all\n", true, &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert!(
            ran.out
                .contains(&format!("{}\n", refused_trash_summary(&app))),
            "{}",
            ran.out
        );
        assert!(ran.out.contains("Total saved: "), "{}", ran.out);
        assert!(!ran.out.contains("space comes back"), "{}", ran.out);
    }

    #[test]
    fn slim_picker_says_the_space_comes_back_when_the_old_app_is_in_the_trash() {
        let (_temp, layout) = sandbox();
        let (host, app) = slim_ready(&layout);
        crate::core::slim::testing::make_slim_save_space(&app);
        host.simulate_trash();
        let before = binary_size(&app);

        let ran = slim_typed(&["slim", "--force"], "all\n", true, &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        let done = crate::core::slim::SlimDone {
            saved: before - binary_size(&app),
            old_app: None,
            holding_left: None,
            remover: crate::core::slim::Remover::User,
        };
        assert!(
            ran.out.contains(&format!("{}\n", done.summary("Fat"))),
            "{}",
            ran.out
        );
        assert!(
            ran.out.contains(&format!(
                "Total saved: {}. Clones share blocks, so the space comes back when the Trash is emptied.",
                format_bytes(done.saved)
            )),
            "{}",
            ran.out
        );
    }

    #[test]
    fn slim_json_list_checks_each_app_once() {
        let (_temp, layout) = sandbox();
        let (host, _app) = slim_ready(&layout);

        let ran = slim(&["slim", "--json"], &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        let checks = host
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("/usr/bin/codesign --verify"))
            .count();
        assert_eq!(checks, 1);
    }

    /// What a crash or a refused Trash call leaves: the old app in the holding folder.
    fn leave_old_app(app: &std::path::Path) -> std::path::PathBuf {
        let old = app.with_file_name("Fat.app.taert-slim-tmp/Fat.app");
        crate::core::slim::testing::write_test_app(&old, "com.vendor.fat");
        old
    }

    #[test]
    fn slim_json_list_leaves_out_the_old_app_in_a_holding_folder() {
        let (_temp, layout) = sandbox();
        let (host, app) = slim_ready(&layout);
        leave_old_app(&app);

        let ran = slim(&["slim", "--json"], &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        let json: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        let paths: Vec<&str> = json
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec![app.to_str().unwrap()]);
    }

    #[test]
    fn slim_by_name_names_the_holding_folder_left_from_an_earlier_run() {
        let (_temp, layout) = sandbox();
        let (host, app) = slim_ready(&layout);
        let old = leave_old_app(&app);

        let ran = slim(&["slim", "Fat", "--force"], &host, &layout);

        assert!(!ran.ok);
        assert!(
            ran.err.contains(&format!(
                "{} is left from an earlier run",
                old.parent().unwrap().display()
            )),
            "{}",
            ran.err
        );
        assert!(!host.ran("/usr/bin/ditto"));
        assert!(old.join("Contents/MacOS/testapp").exists());
    }

    #[test]
    fn slim_json_list_refuses_every_app_when_the_process_list_fails() {
        let (_temp, layout) = sandbox();
        let (_ready, _app) = slim_ready(&layout);
        let host = FakeHost::default();
        host.reply(
            "/usr/sbin/sysctl hw.optional.arm64",
            "hw.optional.arm64: 1\n",
        );
        host.fail("/bin/ps", "");

        let ran = slim(&["slim", "--json"], &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        let json: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        let list = json.as_array().unwrap();
        assert!(!list.is_empty());
        for entry in list {
            assert_eq!(entry["eligible"], false);
            assert!(
                entry["reason"]
                    .as_str()
                    .unwrap()
                    .contains("Could not list running programs")
            );
        }
    }

    #[test]
    fn slim_json_names_the_old_app_only_when_the_trash_refuses() {
        let (_temp, layout) = sandbox();
        let (host, app) = slim_ready(&layout);
        host.fail_trash("denied");
        let old = app.with_file_name("Fat.app.taert-slim-tmp/Fat.app");

        let refused = slim(&["slim", "Fat", "--force", "--json"], &host, &layout);

        let json: serde_json::Value = serde_json::from_str(&refused.out).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["old_app_path"], old.to_str().unwrap());

        let (_temp, layout) = sandbox();
        let (host, _app) = slim_ready(&layout);
        host.simulate_trash();

        let moved = slim(&["slim", "Fat", "--force", "--json"], &host, &layout);

        let json: serde_json::Value = serde_json::from_str(&moved.out).unwrap();
        assert_eq!(json["success"], true);
        assert!(json.get("old_app_path").is_none());
    }

    #[test]
    fn slim_stops_when_the_process_list_fails() {
        let (_temp, layout) = sandbox();
        let (_ready, _app) = slim_ready(&layout);
        let host = FakeHost::default();
        host.reply(
            "/usr/sbin/sysctl hw.optional.arm64",
            "hw.optional.arm64: 1\n",
        );
        host.fail("/bin/ps", "");

        let ran = slim(&["slim", "Fat", "--force"], &host, &layout);

        assert!(!ran.ok);
        assert!(ran.err.contains("Could not list running programs"));
        assert!(!host.ran("/usr/bin/ditto"));
    }

    #[test]
    fn test_env_var_has_no_effect() {
        let (_temp, layout) = sandbox();
        let (host, _app) = slim_ready_signed_by(&layout, "Mac Developer: Someone");
        unsafe { std::env::set_var("TAERT_SLIM_ALLOW_ADHOC", "1") };

        let ran = slim(&["slim", "Fat", "--force"], &host, &layout);

        unsafe { std::env::remove_var("TAERT_SLIM_ALLOW_ADHOC") };
        assert!(!ran.ok);
        assert!(ran.err.contains("Not signed with a Developer ID"));
        assert!(!host.ran("/usr/bin/ditto"));
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

    fn layout_with_a_package_app() -> (tempfile::TempDir, Layout, FakeHost, PathBuf) {
        let (temp, layout) = sandbox();
        let path = package_app(&layout);
        let host = FakeHost::idle();
        package_listing(&host);
        (temp, layout, host, path)
    }

    #[test]
    fn uninstall_finds_an_app_from_a_package_by_name_bundle_id_and_path() {
        let (_temp, layout, host, path) = layout_with_a_package_app();

        for term in [
            "Acme AutoUpdate",
            "com.acme.autoupdate2",
            path.to_str().unwrap(),
        ] {
            let ran = uninstall(&["uninstall", term, "--dry-run"], &host, &layout);

            assert!(ran.ok, "{term}: {}", ran.err);
            assert!(ran.out.contains("Acme AutoUpdate.app"), "{term}");
            assert!(ran.out.contains("Dry run completed"), "{term}");
        }
    }

    #[test]
    fn the_uninstall_list_has_a_section_for_the_folder_of_a_package_app() {
        let (_temp, layout, host, _path) = layout_with_a_package_app();
        fake_app(&layout.system("Applications/Big.app"), "com.big.one", "big");

        let ran = uninstall(&["uninstall", "--dry-run"], &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        let section = crate::core::path_utils::prettify_path(&layout.system(ACME_PREFIX));
        let applications = ran.out.find("Big.app").unwrap();
        let folder = ran.out.find(&format!("[{section}]")).unwrap();
        assert!(applications < folder, "{}", ran.out);
        assert!(ran.out.contains("Acme AutoUpdate.app"), "{}", ran.out);
    }

    #[test]
    fn the_uninstall_json_list_has_the_package_of_an_app_from_a_package() {
        let (_temp, layout, host, _path) = layout_with_a_package_app();

        let ran = uninstall(&["uninstall", "--json"], &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        let list: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(list[0]["bundle_id"], "com.acme.autoupdate2");
        assert_eq!(list[0]["package"], ACME_PACKAGE);
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
    fn uninstall_with_permanent_never_uses_the_trash() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = FakeHost::idle();
        host.simulate_trash();

        let ran = uninstall(
            &["uninstall", "Taert Fixture", "--force", "--permanent"],
            &host,
            &layout,
        );

        assert!(ran.ok, "{}", ran.err);
        assert!(host.trashed().is_empty());
        assert!(!layout.user("Applications/Taert Fixture.app").exists());
    }

    #[test]
    fn uninstall_without_permanent_uses_the_trash() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = FakeHost::idle();
        host.simulate_trash();

        let ran = uninstall(&["uninstall", "Taert Fixture", "--force"], &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert!(
            host.trashed()
                .contains(&layout.user("Applications/Taert Fixture.app"))
        );
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
        let bundle_rows: Vec<_> = ran
            .out
            .lines()
            .filter(|line| line.contains("Taert Fixture.app"))
            .collect();
        assert!(!bundle_rows.is_empty(), "{}", ran.out);
        assert!(
            bundle_rows.iter().all(|line| line.ends_with("(Trash)")),
            "{}",
            ran.out
        );
        assert!(ran.out.contains("to the Trash."), "{}", ran.out);
        assert!(!ran.out.contains("Permanent deletion"), "{}", ran.out);
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

    const SUDO_PROBE: &str = "/usr/bin/sudo -n true";

    /// Writes a package receipt of the fixture app, which only root can remove.
    fn with_a_receipt(layout: &Layout) -> PathBuf {
        let receipt = layout.system("private/var/db/receipts/is.taert.fixture.plist");
        std::fs::create_dir_all(receipt.parent().unwrap()).unwrap();
        std::fs::write(&receipt, b"x").unwrap();
        receipt
    }

    /// The fake root step removes `receipt`, as the real one does.
    fn root_removes(host: &FakeHost, receipt: PathBuf) {
        *host.admin_effect.lock().unwrap() = Some(Box::new(move || {
            let _ = std::fs::remove_file(&receipt);
        }));
    }

    fn password_host() -> std::sync::Arc<FakeHost> {
        let host = FakeHost::idle();
        host.simulate_trash();
        host.fail(SUDO_PROBE, "");
        *host.tty.lock().unwrap() = true;
        std::sync::Arc::new(host)
    }

    #[test]
    fn a_forced_uninstall_with_root_files_asks_once_before_the_first_admin_script() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = password_host();
        let receipt = with_a_receipt(&layout);
        let asked_before_script = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (seen, watched) = (asked_before_script.clone(), host.clone());
        *host.admin_effect.lock().unwrap() = Some(Box::new(move || {
            seen.store(watched.sudo_asks(), std::sync::atomic::Ordering::SeqCst);
            let _ = std::fs::remove_file(&receipt);
        }));

        let ran = uninstall(&["uninstall", "Taert Fixture", "--force"], &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert_eq!(host.sudo_asks(), 1);
        assert_eq!(
            asked_before_script.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(host.admin_scripts.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_plain_uninstall_with_root_files_asks_once_after_yes() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = password_host();
        root_removes(&host, with_a_receipt(&layout));

        let ran = uninstall_typing(
            &["uninstall", "Taert Fixture", "--plain"],
            "\nyes\n",
            &host,
            &layout,
        );

        assert!(ran.ok, "{}", ran.err);
        assert_eq!(host.sudo_asks(), 1);
    }

    #[test]
    fn a_plain_uninstall_answered_no_asks_nothing() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = password_host();
        with_a_receipt(&layout);

        let ran = uninstall_typing(
            &["uninstall", "Taert Fixture", "--plain"],
            "\nno\n",
            &host,
            &layout,
        );

        assert!(ran.ok, "{}", ran.err);
        assert!(ran.out.ends_with("Operation cancelled.\n"), "{}", ran.out);
        assert_eq!(host.sudo_asks(), 0);
    }

    #[test]
    fn a_refused_password_adds_no_text_and_the_report_says_what_stayed() {
        let forced_uninstall = |prepare: &dyn Fn(&FakeHost)| {
            let (temp, layout) = layout_with_the_fixture_app();
            let host = password_host();
            with_a_receipt(&layout);
            prepare(&host);
            let ran = uninstall(&["uninstall", "Taert Fixture", "--force"], &host, &layout);
            let base = temp.path().canonicalize().unwrap();
            let unrooted = |text: &str| text.replace(&*base.to_string_lossy(), "<temp>");
            (host, unrooted(&ran.out), unrooted(&ran.err))
        };

        let (host, out, err) = forced_uninstall(&|host| {
            *host.sudo_refused.lock().unwrap() = true;
        });
        let (_, report_out, report_err) = forced_uninstall(&|host| {
            *host.tty.lock().unwrap() = false;
            *host.admin_result.lock().unwrap() = Some(admin::NOT_GIVEN.to_string());
        });

        assert_eq!(host.sudo_asks(), 1);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
        assert_eq!(err.matches(admin::NOT_GIVEN).count(), 1, "{err}");
        assert_eq!(out, report_out);
        assert_eq!(err, report_err);
    }

    #[test]
    fn an_uninstall_without_root_files_asks_nothing() {
        let (_temp, layout) = layout_with_the_fixture_app();
        let host = password_host();

        let ran = uninstall(&["uninstall", "Taert Fixture", "--force"], &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert_eq!(host.sudo_asks(), 0);
    }

    #[test]
    fn dry_run_and_json_never_ask_for_the_password() {
        for flag in ["--dry-run", "--json"] {
            let (_temp, layout) = layout_with_the_fixture_app();
            let host = password_host();
            with_a_receipt(&layout);

            let ran = uninstall(
                &["uninstall", "Taert Fixture", flag, "--force"],
                &host,
                &layout,
            );

            assert!(ran.ok, "{flag}: {}", ran.err);
            assert_eq!(host.sudo_asks(), 0, "{flag}");
        }
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
    fn clean_orphans_dry_run_lists_rows_and_removes_nothing() {
        let (_temp, layout) = sandbox();
        let orphan = fill(&layout, "Library/Caches/com.gone.app");
        age(&orphan, 40);

        let host = FakeHost::default();
        host.reply("mdfind", "");

        let ran = clean(
            &["clean", "orphans", "--dry-run", "--plain"],
            "",
            &host,
            &layout,
        );
        assert!(ran.ok);
        assert!(ran.out.contains("com.gone.app"));
        assert!(ran.out.contains("(Trash)"));
        assert!(orphan.exists());
    }

    #[test]
    fn clean_orphans_json_has_the_group() {
        let (_temp, layout) = sandbox();
        let orphan = fill(&layout, "Library/Caches/com.gone.app");
        age(&orphan, 40);

        let host = FakeHost::default();
        host.reply("mdfind", "");

        let ran = clean(&["clean", "orphans", "--json"], "", &host, &layout);
        assert!(ran.ok);
        let scan: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        let has_group = scan["groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["tool"] == "com.gone.app");
        assert!(has_group);
    }

    #[test]
    fn clean_without_a_category_includes_orphans() {
        let (_temp, layout) = sandbox();
        let orphan = fill(&layout, "Library/Caches/com.gone.app");
        age(&orphan, 40);

        let host = FakeHost::default();
        host.reply("mdfind", "");

        let ran = clean(&["clean", "--json"], "", &host, &layout);
        assert!(ran.ok);
        let scan: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        let has_group = scan["groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["tool"] == "com.gone.app");
        assert!(has_group);
    }

    #[test]
    fn orphans_start_unselected_without_the_category() {
        let (_temp, layout) = sandbox();
        let orphan = fill(&layout, "Library/Caches/com.gone.app");
        age(&orphan, 40);

        let host = FakeHost::default();
        host.reply("mdfind", "");

        let ran = clean(&["clean", "--force"], "", &host, &layout);
        assert!(ran.ok);
        assert!(host.trashed().is_empty());
        assert!(orphan.exists());
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
        assert!(saved.roots.contains(&path));

        let removed = ignore(&["ignore", "remove", &path], &layout);

        assert!(removed.ok);
        assert_eq!(removed.out, format!("Removed from ignore list: {path}\n"));
        let saved = IgnoreStore::load_from(&layout.ignore_path()).unwrap();
        assert!(!saved.roots.contains(&path));
    }

    #[test]
    fn history_show_names_the_formula_of_a_run_logged_before() {
        let (_temp, layout) = sandbox();
        let session =
            HistoryLog::new(layout.history_path()).start_run("brew uninstall", "force", Some(1));
        session.record_item(&ItemOutcome {
            path: "terraform 1.5.7".into(),
            bytes: 1,
            outcome: crate::core::history::OutcomeKind::Removed,
            error: None,
        });
        let run_id = session.run_id().to_string();
        session.finish();

        let shown = history(&["history", "show", &run_id], &layout);

        assert!(shown.ok);
        assert!(
            shown.out.contains("Command: brew uninstall terraform\n"),
            "{}",
            shown.out
        );
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

    #[test]
    fn a_dry_run_never_opens_the_interactive_screen() {
        let cli = parse(&["--dry-run"]);
        let mut app = ui::app::App::new(IgnoreStore::default(), Config::default());
        let ran = run("", |console| run_tui(&mut app, &cli, console));

        assert!(!ran.ok);
        assert_eq!(
            ran.err,
            "Error: --dry-run does not work in the interactive screen. Add --plain to see the preview.\n"
        );
        assert!(ran.out.is_empty());
    }

    #[test]
    fn analyze_with_dry_run_in_a_terminal_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path_str = temp.path().display().to_string();
        let cli = parse(&["analyze", &path_str, "--dry-run"]);
        let Some(Command::Analyze(ref args)) = cli.command else {
            panic!("not an analyze command");
        };
        let ran = run("", |console| {
            handle_analyze(
                args.path.clone(),
                args.depth,
                &cli,
                false,
                &IgnoreStore::default(),
                &Config::default(),
                console,
            )
        });

        assert!(!ran.ok);
        assert_eq!(
            ran.err,
            "Error: --dry-run does not work in the interactive screen. Add --plain to see the preview.\n"
        );
        assert!(ran.out.is_empty());
    }

    use crate::core::compress::testing::system_app;

    fn run_compress_cli(args: &[&str], host: &FakeHost, layout: &Layout) -> Ran {
        let cli = parse(args);
        let Some(Command::Compress(ref compress_args)) = cli.command else {
            panic!("not a compress command");
        };
        let context = ScanContext {
            layout,
            host,
            admin: crate::core::admin::test_session(),
        };
        let system_owner = crate::core::host::fake::real_uid();
        run("", |console| {
            handle_compress(
                compress_args,
                &cli,
                &IgnoreStore::default(),
                &context,
                system_owner,
                console,
            )
        })
    }

    fn system_host() -> FakeHost {
        let host = FakeHost::idle();
        host.set_uid(crate::core::host::fake::real_uid() + 1);
        host.fail(SUDO_PROBE, "");
        *host.tty.lock().unwrap() = true;
        host
    }

    #[test]
    fn a_dry_run_on_an_app_of_the_system_estimates_and_asks_nothing() {
        let (_temp, layout) = sandbox();
        let app = system_app(&layout);
        let host = system_host();

        let ran = run_compress_cli(
            &["compress", "--dry-run", app.to_str().unwrap()],
            &host,
            &layout,
        );

        assert!(ran.ok, "{}", ran.err);
        assert!(ran.out.contains("Compression Estimate"), "{}", ran.out);
        assert!(ran.out.contains("Compressed files: 1"), "{}", ran.out);
        assert_eq!(host.sudo_asks(), 0);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }

    #[test]
    fn compressing_and_undoing_an_app_of_the_system_go_through_the_helper_each_after_one_password()
    {
        let (_temp, layout) = sandbox();
        let app = system_app(&layout);
        let host = system_host();
        let report = crate::core::compress::CompressReport {
            processed_files: 1,
            saved_bytes: 1000,
            ..Default::default()
        };
        *host.admin_stdout.lock().unwrap() = serde_json::to_string(&report).unwrap();

        let ran = run_compress_cli(&["compress", app.to_str().unwrap()], &host, &layout);
        let undone = run_compress_cli(
            &["compress", "--undo", app.to_str().unwrap()],
            &host,
            &layout,
        );

        assert!(ran.ok, "{}", ran.err);
        assert!(undone.ok, "{}", undone.err);
        assert!(ran.out.contains("Compressed files: 1"), "{}", ran.out);
        let scripts = host.admin_scripts.lock().unwrap();
        assert_eq!(scripts.len(), 2, "{scripts:?}");
        assert!(scripts[0].contains("admin-helper compress --home"));
        assert!(!scripts[0].contains("--undo"));
        assert!(scripts[1].contains("admin-helper compress --undo"));
        assert!(scripts[0].contains(app.to_str().unwrap()));
        assert_eq!(host.sudo_asks(), 2);
        assert!(
            host.calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call.contains("sudo -v compress apps that belong to the system"))
        );
    }

    #[test]
    fn a_refused_password_fails_the_app_of_the_system_and_says_why() {
        let (_temp, layout) = sandbox();
        let app = system_app(&layout);
        let host = system_host();
        *host.sudo_refused.lock().unwrap() = true;

        let ran = run_compress_cli(&["compress", app.to_str().unwrap()], &host, &layout);

        assert!(!ran.ok);
        assert!(ran.err.contains(admin::NOT_GIVEN), "{}", ran.err);
        assert_eq!(host.sudo_asks(), 1);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }

    /// "Fat" in `/Applications`, which looks like an app of root: its files belong to the test
    /// user, who is the system owner here, and the host user is another one.
    fn slim_system_ready(layout: &Layout) -> (FakeHost, std::path::PathBuf) {
        let (host, app) = slim_ready_at(
            layout.system("Applications/Fat.app"),
            "Developer ID Application: Acme Corp (123)",
        );
        host.set_uid(crate::core::host::fake::real_uid() + 1);
        host.fail(SUDO_PROBE, "");
        *host.tty.lock().unwrap() = true;
        let answer = serde_json::json!({
            "saved": 1000,
            "old_app": layout.system("Applications/Fat.taert-old.app"),
        });
        *host.admin_stdout.lock().unwrap() = answer.to_string();
        (host, app)
    }

    fn slim_as_system(args: &[&str], typed: &str, host: &FakeHost, layout: &Layout) -> Ran {
        let system_owner = crate::core::host::fake::real_uid();
        slim_typed_for(args, typed, false, host, layout, system_owner)
    }

    #[test]
    fn a_dry_run_on_an_app_of_root_asks_nothing() {
        let (_temp, layout) = sandbox();
        let (host, _app) = slim_system_ready(&layout);

        let ran = slim_as_system(&["slim", "Fat", "--dry-run"], "", &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        assert!(ran.out.contains("Would slim Fat, saving "), "{}", ran.out);
        assert!(
            ran.out
                .contains("of Intel code. Original will go to Trash.")
        );
        assert_eq!(host.sudo_asks(), 0);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }

    #[test]
    fn slimming_an_app_of_root_asks_then_the_password_then_runs_the_helper() {
        let (_temp, layout) = sandbox();
        let (host, app) = slim_system_ready(&layout);

        let ran = slim_as_system(&["slim", "Fat"], "y\n", &host, &layout);

        assert!(ran.ok, "{}{}", ran.out, ran.err);
        assert!(ran.out.starts_with("Slim Fat? [y/N]: "), "{}", ran.out);
        assert!(ran.out.contains("Slimmed Fat, saved "), "{}", ran.out);
        assert_eq!(host.sudo_asks(), 1);
        let calls = host.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|call| call == "sudo -v slim apps that belong to the system")
        );
        let scripts = host.admin_scripts.lock().unwrap();
        assert_eq!(scripts.len(), 1, "{scripts:?}");
        assert!(scripts[0].contains("admin-helper slim"), "{scripts:?}");
        assert!(scripts[0].contains(app.to_str().unwrap()));
        assert!(!calls.iter().any(|call| call.starts_with("/usr/bin/ditto")));
    }

    #[test]
    fn declining_the_slim_of_an_app_of_root_asks_for_no_password() {
        let (_temp, layout) = sandbox();
        let (host, _app) = slim_system_ready(&layout);

        let ran = slim_as_system(&["slim", "Fat"], "n\n", &host, &layout);

        assert!(ran.out.contains("Cancelled."), "{}", ran.out);
        assert_eq!(host.sudo_asks(), 0);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }

    #[test]
    fn the_plain_picker_sends_an_app_of_root_to_the_helper() {
        let (_temp, layout) = sandbox();
        let (host, app) = slim_system_ready(&layout);
        let system_owner = crate::core::host::fake::real_uid();

        let ran = slim_typed_for(
            &["slim", "--force"],
            "all\n",
            true,
            &host,
            &layout,
            system_owner,
        );

        assert!(ran.ok, "{}{}", ran.out, ran.err);
        assert!(ran.out.contains("Slimmed Fat, saved "), "{}", ran.out);
        assert_eq!(host.sudo_asks(), 1);
        let scripts = host.admin_scripts.lock().unwrap();
        assert_eq!(scripts.len(), 1, "{scripts:?}");
        assert!(scripts[0].contains("admin-helper slim"), "{scripts:?}");
        assert!(scripts[0].contains(app.to_str().unwrap()));
        let calls = host.calls.lock().unwrap();
        assert!(!calls.iter().any(|call| call.starts_with("/usr/bin/ditto")));
    }

    #[test]
    fn a_refused_password_fails_the_slim_of_an_app_of_root() {
        let (_temp, layout) = sandbox();
        let (host, _app) = slim_system_ready(&layout);
        *host.sudo_refused.lock().unwrap() = true;

        let ran = slim_as_system(&["slim", "Fat"], "y\n", &host, &layout);

        assert!(!ran.ok);
        assert!(
            ran.err
                .contains(&format!("failed at step 1: {}", admin::NOT_GIVEN)),
            "{}",
            ran.err
        );
        assert_eq!(host.sudo_asks(), 1);
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }

    #[test]
    fn the_json_list_names_needs_admin_only_for_an_app_of_root() {
        let (_temp, layout) = sandbox();
        let (host, _app) = slim_system_ready(&layout);
        let store = layout.system("Applications/Store.app");
        crate::core::slim::testing::write_test_app(&store, "com.vendor.store");
        std::fs::create_dir_all(store.join("Contents/_MASReceipt")).unwrap();

        let ran = slim_as_system(&["slim", "--json"], "", &host, &layout);

        assert!(ran.ok, "{}", ran.err);
        let json: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        let row = |name: &str| {
            json.as_array()
                .unwrap()
                .iter()
                .find(|row| row["name"] == name)
                .unwrap()
        };
        assert_eq!(row("Fat")["eligible"], true);
        assert_eq!(row("Fat")["needs_admin"], true);
        assert_eq!(row("Store")["eligible"], false);
        assert!(row("Store").get("needs_admin").is_none());
        assert_eq!(host.sudo_asks(), 0);
    }

    #[test]
    fn the_json_list_has_no_needs_admin_for_the_apps_of_the_user() {
        let (_temp, layout) = sandbox();
        let (host, _app) = slim_ready(&layout);
        let system_owner = crate::core::host::fake::real_uid() + 1;

        let ran = slim_typed_for(&["slim", "--json"], "", false, &host, &layout, system_owner);

        assert!(ran.ok, "{}", ran.err);
        let json: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        let fat = json
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "Fat")
            .unwrap();
        assert_eq!(fat["eligible"], true, "{fat}");
        assert!(fat.get("needs_admin").is_none(), "{fat}");
    }
}
