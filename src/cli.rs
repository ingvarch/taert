//! Command-line argument definitions using clap.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "taert",
    about = "Fast, transparent macOS disk cleanup and storage explorer",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Run cleanup in plain text mode (no TUI)
    #[arg(long, global = true)]
    pub plain: bool,

    /// Preview what would be removed without deleting anything
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Output results in JSON format
    #[arg(long, global = true)]
    pub json: bool,

    /// Non-interactive unattended mode: permanently delete default selected items
    #[arg(long, global = true)]
    pub force: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scan and clean developer caches, user junk, AI caches, or Trash
    Clean(CleanArgs),

    /// Find and remove build artifacts inside project directories
    Purge(PurgeArgs),

    /// Find and remove Docker images, volumes, networks and build cache no container uses
    Docker,

    /// Remove apps together with everything they left on disk
    Uninstall(UninstallArgs),

    /// Browse directory disk usage without modifying files (read-only)
    Analyze(AnalyzeArgs),

    /// Display memory and APFS storage status
    Optimize,

    /// Review and switch off login and background items
    Startup(StartupArgs),

    /// Manage protected paths that cannot be scanned or cleaned
    Ignore(IgnoreArgs),

    /// Show current configuration and paths
    Config(ConfigArgs),
}

#[derive(Debug, Args, Default)]
pub struct ConfigArgs {
    /// Set appearance theme: auto, dark, or light
    #[arg(long)]
    pub appearance: Option<String>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum CleanCategory {
    Dev,
    Junk,
    Ai,
    Trash,
}

#[derive(Debug, Args)]
pub struct CleanArgs {
    /// Specific category to scan: dev, junk, ai, or trash (scans all if omitted)
    pub category: Option<CleanCategory>,
}

#[derive(Debug, Args)]
pub struct PurgeArgs {
    /// Root path to scan for project artifacts (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Filter by artifact age in days (defaults to 7)
    #[arg(long)]
    pub older_than: Option<u64>,
}

#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Apps to remove, by name or bundle id (shows the list of apps if omitted)
    pub names: Vec<String>,
}

#[derive(Debug, Args)]
pub struct AnalyzeArgs {
    /// Path to analyze (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Maximum folder depth to display in plain mode
    #[arg(long, default_value = "1")]
    pub depth: usize,
}

#[derive(Debug, Args)]
pub struct IgnoreArgs {
    #[command(subcommand)]
    pub action: IgnoreAction,
}

#[derive(Debug, Subcommand)]
pub enum IgnoreAction {
    /// Protect a path from cleanup
    Add { path: PathBuf },
    /// Remove protection from a path
    Remove { path: PathBuf },
    /// List all protected paths
    List,
}

#[derive(Debug, Args, Default)]
pub struct StartupArgs {
    /// Include Apple services and background task management entries (requires root password)
    #[arg(long)]
    pub all: bool,

    #[command(subcommand)]
    pub action: Option<StartupAction>,
}

#[derive(Debug, Subcommand)]
pub enum StartupAction {
    /// Disable a startup service or login item
    Disable {
        /// Label of the launchd service or name of the login item
        label: String,
    },
    /// Enable a startup service or login item
    Enable {
        /// Label of the launchd service or name of the login item
        label: String,
    },
}
