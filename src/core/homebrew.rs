//! Lists the Homebrew packages and the space they take, and removes unused ones through brew.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::bytes::format_bytes;
use super::host::{Host, Output};
use super::layout::Layout;
use super::scanner::{CategoryScan, ScanItem, ToolGroup};
use super::sizer::DirSizer;

pub const SCAN_ID: &str = "brew";
pub const CLEANUP_ID: &str = "brew-cleanup";
pub const UNINSTALL_ID: &str = "brew-uninstall";
const INFO_TIMEOUT: Duration = Duration::from_secs(60);
const DRY_RUN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const BREW_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const GROUP_FORMULA: &str = "Formula";
const GROUP_FORMULAE: &str = "Formulae";
const GROUP_CASKS: &str = "Casks · Remove with taert uninstall";
const GROUP_UNUSED: &str = "Unused dependencies";
const GROUP_OLD: &str = "Old versions";
const NOT_INSTALLED: &str = "Homebrew is not installed.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageKind {
    Formula,
    Cask,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Package {
    pub kind: PackageKind,
    pub name: String,
    /// The name with its tap (`user/tap/name`), the same as `name` when there is none.
    #[serde(skip)]
    pub full_name: String,
    pub version: String,
    pub size_bytes: u64,
    pub on_request: bool,
    pub outdated: bool,
    pub pinned: bool,
    /// Installed formulae that list this one as a dependency.
    pub used_by: Vec<String>,
    pub apps: Vec<String>,
    #[serde(skip)]
    pub dependencies: Vec<String>,
    #[serde(skip)]
    pub path: PathBuf,
}

/// What a row of the cleanup list asks Homebrew to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BrewTask {
    /// `brew uninstall <name>`, after checking again that nothing uses it.
    Uninstall {
        name: String,
        brew: PathBuf,
        cellar: PathBuf,
    },
    /// `brew cleanup <name>`: removes the old versions of one formula.
    Cleanup {
        name: String,
        brew: PathBuf,
        cellar: PathBuf,
    },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Inventory {
    pub packages: Vec<Package>,
    pub notes: Vec<String>,
    /// The brew that listed the packages, so a row can carry a task.
    #[serde(skip)]
    pub brew: Option<PathBuf>,
}

impl Inventory {
    /// Drops an uninstalled formula and its name from what the others list as used by it.
    pub fn forget(&mut self, name: &str) {
        self.packages
            .retain(|package| !(package.kind == PackageKind::Formula && package.name == name));
        for package in &mut self.packages {
            package.used_by.retain(|user| user != name);
        }
    }
}

pub struct Brew {
    pub prefix: PathBuf,
    pub path: PathBuf,
}

#[derive(Deserialize)]
struct Info {
    #[serde(default)]
    formulae: Vec<RawFormula>,
    #[serde(default)]
    casks: Vec<RawCask>,
}

#[derive(Deserialize)]
struct RawFormula {
    name: String,
    full_name: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    installed: Vec<RawKeg>,
    #[serde(default)]
    outdated: bool,
    #[serde(default)]
    pinned: bool,
}

#[derive(Deserialize)]
struct RawKeg {
    version: String,
    #[serde(default)]
    installed_on_request: bool,
}

#[derive(Deserialize)]
struct RawCask {
    token: String,
    installed: Option<String>,
    #[serde(default)]
    outdated: bool,
    #[serde(default)]
    artifacts: Vec<Value>,
}

/// The first Homebrew prefix that has a `brew`.
pub fn find(layout: &Layout) -> Option<Brew> {
    layout.homebrew_prefixes().into_iter().find_map(|prefix| {
        let path = prefix.join("bin/brew");
        path.exists().then_some(Brew { prefix, path })
    })
}

/// Runs `brew` without auto-update, hints or questions.
fn run_brew(host: &dyn Host, brew: &Path, args: &[&str], timeout: Duration) -> Output {
    run_brew_with(host, brew, &[], args, timeout)
}

/// Runs a read-only brew command that must succeed, and returns what it printed.
fn read(
    host: &dyn Host,
    brew: &Path,
    args: &[&str],
    timeout: Duration,
    what: &str,
) -> Result<String, String> {
    let output = run_brew(host, brew, args, timeout);
    if output.ok {
        Ok(output.stdout)
    } else {
        Err(format!(
            "Homebrew did not list {what}: {}",
            output.stderr.trim()
        ))
    }
}

/// Runs a brew command that removes something. brew removes only what was named, not every unused
/// formula too.
pub(crate) fn run_removing(host: &dyn Host, brew: &Path, args: &[&str]) -> Output {
    run_brew_with(
        host,
        brew,
        &["HOMEBREW_NO_AUTOREMOVE=1"],
        args,
        BREW_TIMEOUT,
    )
}

fn remove(host: &dyn Host, brew: &Path, args: &[&str], failure: &str) -> Result<(), String> {
    let output = run_removing(host, brew, args);
    if output.ok {
        Ok(())
    } else {
        Err(format!("{failure}: {}", output.stderr.trim()))
    }
}

fn run_brew_with(
    host: &dyn Host,
    brew: &Path,
    environment: &[&str],
    args: &[&str],
    timeout: Duration,
) -> Output {
    let brew = brew.display().to_string();
    let mut command = vec![
        "HOMEBREW_NO_AUTO_UPDATE=1",
        "HOMEBREW_NO_ENV_HINTS=1",
        "NONINTERACTIVE=1",
    ];
    command.extend_from_slice(environment);
    command.push(brew.as_str());
    command.extend_from_slice(args);
    host.run("/usr/bin/env", &command, timeout)
}

fn cask_apps(artifacts: &[Value]) -> Vec<String> {
    artifacts
        .iter()
        .filter_map(|artifact| artifact.get("app")?.as_array())
        .flatten()
        .filter_map(|app| app.as_str().map(str::to_string))
        .collect()
}

fn formula_package(raw: &RawFormula, prefix: &Path) -> Package {
    Package {
        kind: PackageKind::Formula,
        name: raw.name.clone(),
        full_name: raw.full_name.clone().unwrap_or_else(|| raw.name.clone()),
        version: raw
            .installed
            .last()
            .map(|keg| keg.version.clone())
            .unwrap_or_default(),
        size_bytes: 0,
        on_request: raw
            .installed
            .first()
            .is_some_and(|keg| keg.installed_on_request),
        outdated: raw.outdated,
        pinned: raw.pinned,
        used_by: Vec::new(),
        apps: Vec::new(),
        dependencies: raw.dependencies.clone(),
        path: cellar_of(prefix, &raw.name),
    }
}

fn cask_package(raw: &RawCask, prefix: &Path) -> Package {
    Package {
        kind: PackageKind::Cask,
        name: raw.token.clone(),
        full_name: raw.token.clone(),
        version: raw.installed.clone().unwrap_or_default(),
        size_bytes: 0,
        on_request: true,
        outdated: raw.outdated,
        pinned: false,
        used_by: Vec::new(),
        apps: cask_apps(&raw.artifacts),
        dependencies: Vec::new(),
        path: prefix.join("Caskroom").join(&raw.token),
    }
}

/// Reads the packages from `brew info --json=v2 --installed`, without sizes.
pub fn parse_packages(json: &str, prefix: &Path) -> Result<Vec<Package>, String> {
    let info: Info = serde_json::from_str(json)
        .map_err(|error| format!("Homebrew printed a package list taert cannot read: {error}"))?;

    let mut users: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for formula in &info.formulae {
        for dependency in &formula.dependencies {
            users
                .entry(dependency)
                .or_default()
                .insert(formula.name.as_str());
        }
    }

    let formulae = info.formulae.iter().map(|raw| {
        let mut package = formula_package(raw, prefix);
        let names = [Some(raw.name.as_str()), raw.full_name.as_deref()];
        let used_by: BTreeSet<&str> = names
            .into_iter()
            .flatten()
            .filter_map(|name| users.get(name))
            .flatten()
            .copied()
            .collect();
        package.used_by = used_by.into_iter().map(str::to_string).collect();
        package
    });
    let casks = info.casks.iter().map(|raw| cask_package(raw, prefix));
    Ok(formulae.chain(casks).collect())
}

/// Asks brew for the installed packages, without sizes.
fn installed(host: &dyn Host, brew: &Path, prefix: &Path) -> Result<Vec<Package>, String> {
    let json = read(
        host,
        brew,
        &["info", "--json=v2", "--installed"],
        INFO_TIMEOUT,
        "its packages",
    )?;
    parse_packages(&json, prefix)
}

/// Lists what Homebrew has installed, with the size of each folder.
pub fn scan(layout: &Layout, host: &dyn Host) -> Result<Inventory, String> {
    let Some(brew) = find(layout) else {
        return Ok(Inventory {
            notes: vec![NOT_INSTALLED.to_string()],
            ..Inventory::default()
        });
    };

    let mut packages = installed(host, &brew.path, &brew.prefix)?;
    for package in &mut packages {
        package.size_bytes = DirSizer::size_of(&package.path);
    }
    packages.sort_by(|a, b| {
        (a.kind, Reverse(a.size_bytes), &a.name).cmp(&(b.kind, Reverse(b.size_bytes), &b.name))
    });
    Ok(Inventory {
        packages,
        notes: Vec::new(),
        brew: Some(brew.path),
    })
}

fn name_version(package: &Package) -> String {
    format!("{} {}", package.name, package.version)
}

/// The name and version a list label starts with, without its details.
pub fn label_name(label: &str) -> &str {
    label.split_once(" · ").map_or(label, |(name, _)| name)
}

fn label(package: &Package) -> String {
    let mut label = name_version(package);
    match package.kind {
        PackageKind::Formula => {
            let origin = if package.on_request {
                "on request"
            } else {
                "dependency"
            };
            write!(label, " · {origin}").unwrap();
            if !package.used_by.is_empty() {
                write!(label, " · used by {}", package.used_by.len()).unwrap();
            }
        }
        PackageKind::Cask if !package.apps.is_empty() => {
            write!(label, " · {}", package.apps.join(", ")).unwrap();
        }
        PackageKind::Cask => {}
    }
    if package.outdated {
        label.push_str(" · outdated");
    }
    if package.pinned {
        label.push_str(" · pinned");
    }
    label
}

fn item(label: String, size_bytes: u64, selected: bool, brew: Option<BrewTask>) -> ScanItem {
    ScanItem {
        path: PathBuf::from(label),
        size_bytes,
        modified: None,
        selected,
        contents_only: false,
        identity: None,
        docker: None,
        brew,
        role: None,
        also: Vec::new(),
    }
}

fn uninstall_task(package: &Package, brew: &Path) -> BrewTask {
    BrewTask::Uninstall {
        name: package.name.clone(),
        brew: brew.to_path_buf(),
        cellar: package.path.clone(),
    }
}

/// The packages as a list to browse. A formula row carries the task that removes it, and no row
/// starts selected.
pub fn list_scan(inventory: &Inventory) -> CategoryScan {
    let row = |package: &Package| {
        let task = match (package.kind, &inventory.brew) {
            (PackageKind::Formula, Some(brew)) => Some(uninstall_task(package, brew)),
            _ => None,
        };
        item(label(package), package.size_bytes, false, task)
    };
    let group = |tool: &str, kind: PackageKind| ToolGroup {
        tool: tool.to_string(),
        items: inventory
            .packages
            .iter()
            .filter(|package| package.kind == kind)
            .map(row)
            .collect(),
    };
    let groups = [
        group(GROUP_FORMULAE, PackageKind::Formula),
        group(GROUP_CASKS, PackageKind::Cask),
    ]
    .into_iter()
    .filter(|group| !group.items.is_empty())
    .collect();

    CategoryScan {
        id: SCAN_ID.to_string(),
        title: "Homebrew".to_string(),
        groups,
        notes: inventory.notes.clone(),
    }
}

fn in_use(name: &str, used_by: &[String]) -> String {
    format!("{name} stays: {} use it.", used_by.join(", "))
}

/// Why taert will not uninstall this package, if it will not.
pub fn refusal(package: &Package) -> Option<String> {
    let name = &package.name;
    if package.kind == PackageKind::Cask {
        Some(format!("{name} is a cask. Remove it with taert uninstall."))
    } else if package.pinned {
        Some(format!("{name} is pinned. Run brew unpin {name} first."))
    } else if !package.used_by.is_empty() {
        Some(in_use(name, &package.used_by))
    } else {
        None
    }
}

/// The refusal of each row of the list that has one, by the label of the row.
pub fn list_refusals(inventory: &Inventory) -> HashMap<PathBuf, String> {
    inventory
        .packages
        .iter()
        .filter_map(|package| Some((PathBuf::from(label(package)), refusal(package)?)))
        .collect()
}

/// One formula to remove, read with one `brew info` call and checked against `refusal`.
pub fn uninstall_plan(
    layout: &Layout,
    host: &dyn Host,
    name: &str,
) -> Result<CategoryScan, String> {
    let brew = find(layout).ok_or(NOT_INSTALLED)?;
    let packages = installed(host, &brew.path, &brew.prefix)?;
    let package = [PackageKind::Formula, PackageKind::Cask]
        .into_iter()
        .find_map(|kind| {
            packages
                .iter()
                .find(|package| package.kind == kind && is_named(package, name))
        })
        .ok_or_else(|| format!("{name} is not installed."))?;
    if let Some(text) = refusal(package) {
        return Err(text);
    }

    let size = DirSizer::size_of(&package.path);
    let task = uninstall_task(package, &brew.path);
    let row = item(name_version(package), size, true, Some(task));
    Ok(CategoryScan {
        id: UNINSTALL_ID.to_string(),
        title: "Homebrew Uninstall".to_string(),
        groups: vec![ToolGroup {
            tool: GROUP_FORMULA.to_string(),
            items: vec![row],
        }],
        notes: Vec::new(),
    })
}

/// The names `brew autoremove --dry-run` lists after its header.
pub fn parse_autoremove(text: &str) -> Vec<String> {
    text.lines()
        .skip_while(|line| !line.starts_with("==> Would autoremove"))
        .skip(1)
        .map(str::trim)
        .take_while(|line| !line.is_empty() && !line.starts_with("==>"))
        .map(str::to_string)
        .collect()
}

/// The old versions in the Cellar that `brew cleanup --dry-run` lists, by formula.
pub fn parse_old_versions(text: &str, prefix: &Path) -> Vec<(String, Vec<PathBuf>)> {
    let cellar = prefix.join("Cellar");
    let mut found: Vec<(String, Vec<PathBuf>)> = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("Would remove: ") else {
            continue;
        };
        let Some((path, _)) = rest.strip_suffix(')').and_then(|s| s.rsplit_once(" (")) else {
            continue;
        };
        let path = Path::new(path);
        let Ok(inside) = path.strip_prefix(&cellar) else {
            continue;
        };
        let parts: Vec<_> = inside.components().collect();
        if parts.len() < 2 || parts.iter().any(|c| !matches!(c, Component::Normal(_))) {
            continue;
        }
        let name = parts[0].as_os_str().to_string_lossy().into_owned();
        match found.iter_mut().find(|(known, _)| *known == name) {
            Some((_, paths)) => paths.push(path.to_path_buf()),
            None => found.push((name, vec![path.to_path_buf()])),
        }
    }
    found
}

fn empty_cleanup(notes: Vec<String>) -> CategoryScan {
    CategoryScan {
        id: CLEANUP_ID.to_string(),
        title: "Homebrew Cleanup".to_string(),
        groups: Vec::new(),
        notes,
    }
}

fn is_named(package: &Package, name: &str) -> bool {
    package.name == name || package.full_name == name
}

fn cellar_of(prefix: &Path, name: &str) -> PathBuf {
    prefix.join("Cellar").join(name)
}

/// A formula comes before the formulae it needs, so each one is already gone when the run reaches
/// what it used. Otherwise the biggest comes first.
fn dependents_first(mut rest: Vec<(&Package, u64)>) -> Vec<(&Package, u64)> {
    rest.sort_by(|a, b| (Reverse(a.1), &a.0.name).cmp(&(Reverse(b.1), &b.0.name)));
    let mut ordered = Vec::with_capacity(rest.len());
    while !rest.is_empty() {
        let next = rest
            .iter()
            .position(|(wanted, _)| {
                !rest
                    .iter()
                    .any(|(user, _)| user.dependencies.iter().any(|d| is_named(wanted, d)))
            })
            .unwrap_or(0);
        ordered.push(rest.remove(next));
    }
    ordered
}

fn unused_group(unused: &[&Package], brew: &Brew) -> ToolGroup {
    let sized = unused
        .iter()
        .map(|package| (*package, DirSizer::size_of(&package.path)))
        .collect();
    let items = dependents_first(sized)
        .into_iter()
        .map(|(package, size)| {
            item(
                name_version(package),
                size,
                !package.pinned,
                Some(uninstall_task(package, &brew.path)),
            )
        })
        .collect();
    ToolGroup {
        tool: GROUP_UNUSED.to_string(),
        items,
    }
}

fn old_versions_group(
    old: &[(String, Vec<PathBuf>)],
    formulae: &[&Package],
    brew: &Brew,
) -> ToolGroup {
    let mut rows: Vec<ScanItem> = old
        .iter()
        .map(|(name, paths)| {
            let versions: Vec<_> = paths
                .iter()
                .filter_map(|path| path.file_name()?.to_str())
                .collect();
            let size = paths.iter().map(|path| DirSizer::size_of(path)).sum();
            let pinned = formulae
                .iter()
                .any(|package| package.name == *name && package.pinned);
            let task = BrewTask::Cleanup {
                name: name.clone(),
                brew: brew.path.clone(),
                cellar: cellar_of(&brew.prefix, name),
            };
            item(
                format!("{name} {}", versions.join(", ")),
                size,
                !pinned,
                Some(task),
            )
        })
        .collect();
    rows.sort_by(|a, b| (Reverse(a.size_bytes), &a.path).cmp(&(Reverse(b.size_bytes), &b.path)));
    ToolGroup {
        tool: GROUP_OLD.to_string(),
        items: rows,
    }
}

/// Unused dependencies and old versions, read with dry runs only.
pub fn cleanup_scan(layout: &Layout, host: &dyn Host) -> Result<CategoryScan, String> {
    let Some(brew) = find(layout) else {
        return Ok(empty_cleanup(vec![NOT_INSTALLED.to_string()]));
    };

    let packages = installed(host, &brew.path, &brew.prefix)?;
    let formulae: Vec<&Package> = packages
        .iter()
        .filter(|package| package.kind == PackageKind::Formula)
        .collect();
    let autoremove = read(
        host,
        &brew.path,
        &["autoremove", "--dry-run"],
        DRY_RUN_TIMEOUT,
        "the unused dependencies",
    )?;
    let cleanup = read(
        host,
        &brew.path,
        &["cleanup", "--dry-run"],
        DRY_RUN_TIMEOUT,
        "the old versions",
    )?;

    let mut unused: Vec<&Package> = Vec::new();
    for name in parse_autoremove(&autoremove) {
        let found = formulae.iter().find(|package| is_named(package, &name));
        if let Some(package) = found
            && !unused.iter().any(|known| known.name == package.name)
        {
            unused.push(package);
        }
    }
    // Only installed formulae, so a name from brew's text never reaches brew as a flag.
    let old: Vec<_> = parse_old_versions(&cleanup, &brew.prefix)
        .into_iter()
        .filter(|(name, _)| formulae.iter().any(|package| package.name == *name))
        .filter(|(name, _)| !unused.iter().any(|package| package.name == *name))
        .collect();

    let mut scan = empty_cleanup(Vec::new());
    scan.groups = [
        unused_group(&unused, &brew),
        old_versions_group(&old, &formulae, &brew),
    ]
    .into_iter()
    .filter(|group| !group.items.is_empty())
    .collect();
    Ok(scan)
}

/// The version folders right below the Cellar folder of a formula, by name; none if it is gone.
fn versions(cellar: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(cellar)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Uninstalls every version of the formula, only if the packages brew lists now still show
/// nothing using it.
fn uninstall(host: &dyn Host, name: &str, brew: &Path, cellar: &Path) -> Result<(), String> {
    let prefix = cellar.parent().and_then(Path::parent).ok_or_else(|| {
        format!(
            "taert cannot find the Homebrew prefix of {}.",
            cellar.display()
        )
    })?;
    let packages = installed(host, brew, prefix)?;
    let Some(package) = packages
        .iter()
        .find(|package| package.kind == PackageKind::Formula && package.name == name)
    else {
        return Err(format!("{name} is no longer installed."));
    };
    if let Some(reason) = refusal(package) {
        return Err(reason);
    }
    let mut left = versions(cellar);
    if left.is_empty() {
        return Err(format!(
            "taert finds no installed version of {name} in {}.",
            cellar.display()
        ));
    }
    // Without --force, each call removes one version. Brew keeps the folder when it holds
    // another file, so no version left means done.
    for _ in 0..left.len() {
        remove(
            host,
            brew,
            &["uninstall", name],
            &format!("Homebrew could not remove {name}"),
        )?;
        let now = versions(cellar);
        if now.is_empty() {
            return Ok(());
        }
        let removed_nothing = now.len() >= left.len();
        left = now;
        if removed_nothing {
            break;
        }
    }
    let verb = if left.len() == 1 { "is" } else { "are" };
    Err(format!(
        "Homebrew removed part of {name}; {} {verb} still installed.",
        left.join(", ")
    ))
}

/// Runs one task through brew and returns the bytes it freed.
pub fn run_task(host: &dyn Host, task: &BrewTask) -> Result<u64, String> {
    let (BrewTask::Uninstall { name, brew, cellar } | BrewTask::Cleanup { name, brew, cellar }) =
        task;
    let before = DirSizer::size_of(cellar);
    match task {
        BrewTask::Uninstall { .. } => uninstall(host, name, brew, cellar)?,
        BrewTask::Cleanup { .. } => remove(
            host,
            brew,
            &["cleanup", name],
            &format!("Homebrew could not clean up {name}"),
        )?,
    }
    Ok(before.saturating_sub(DirSizer::size_of(cellar)))
}

struct Widths {
    name: usize,
    version: usize,
    size: usize,
}

fn plain_section(title: &str, packages: &[&Package], widths: &Widths) -> String {
    let mut text = format!("{title}\n");
    for package in packages {
        let origin = if package.on_request {
            "on request"
        } else {
            "dependency"
        };
        let outdated = if package.outdated { "outdated" } else { "-" };
        let pinned = if package.pinned { "pinned" } else { "-" };
        writeln!(
            text,
            "  {:<name$}  {:<version$}  {:>size$}  {origin}  {outdated:<8}  {pinned:<6}  {}",
            package.name,
            package.version,
            format_bytes(package.size_bytes),
            package.used_by.len(),
            name = widths.name,
            version = widths.version,
            size = widths.size,
        )
        .unwrap();
    }
    text
}

/// One row per package in two sections, with the columns lined up across both.
pub fn plain_text(inventory: &Inventory) -> String {
    let widths = Widths {
        name: inventory
            .packages
            .iter()
            .map(|p| p.name.len())
            .max()
            .unwrap_or(0),
        version: inventory
            .packages
            .iter()
            .map(|p| p.version.len())
            .max()
            .unwrap_or(0),
        size: inventory
            .packages
            .iter()
            .map(|p| format_bytes(p.size_bytes).len())
            .max()
            .unwrap_or(0),
    };
    let section = |title: &str, kind: PackageKind| {
        let packages: Vec<&Package> = inventory
            .packages
            .iter()
            .filter(|package| package.kind == kind)
            .collect();
        (!packages.is_empty()).then(|| plain_section(title, &packages, &widths))
    };
    [
        section("Formulae", PackageKind::Formula),
        section("Casks (remove with taert uninstall)", PackageKind::Cask),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n")
}

#[cfg(test)]
pub mod testing {
    use std::path::PathBuf;

    use super::{Package, PackageKind};
    use crate::core::apps::testing::sandbox;
    use crate::core::layout::Layout;

    // From brew info --json=v2 --installed on a Mac with Homebrew 7.0.12, cut down to 10 formulae and 2 casks. "pinned" was set to true on fd by hand, because no formula there was pinned.
    pub const BREW_INFO: &str = include_str!("../../tests/fixtures/brew_info.json");
    // From brew cleanup --dry-run on a Mac with Homebrew 7.0.12, a few lines. brew prints the Warning line on stderr; it is kept here to check that the parser skips it.
    pub const CLEANUP_DRY_RUN: &str = include_str!("../../tests/fixtures/brew_cleanup_dry_run.txt");
    // Written in the format of Cleanup.autoremove in Library/Homebrew/cleanup.rb (Homebrew 7.0.12), because brew autoremove --dry-run printed nothing on that Mac.
    pub const AUTOREMOVE_DRY_RUN: &str =
        include_str!("../../tests/fixtures/brew_autoremove_dry_run.txt");

    /// A package on request that nothing uses.
    pub fn package(kind: PackageKind, name: &str, version: &str, bytes: u64) -> Package {
        Package {
            kind,
            name: name.to_string(),
            full_name: name.to_string(),
            version: version.to_string(),
            size_bytes: bytes,
            on_request: true,
            outdated: false,
            pinned: false,
            used_by: Vec::new(),
            apps: Vec::new(),
            dependencies: Vec::new(),
            path: PathBuf::new(),
        }
    }

    /// A temporary layout with an empty `bin/brew` under `prefix`.
    pub fn with_brew(prefix: &str) -> (tempfile::TempDir, Layout) {
        let (temp, layout) = sandbox();
        let brew = layout.system(prefix).join("bin/brew");
        std::fs::create_dir_all(brew.parent().unwrap()).unwrap();
        std::fs::write(brew, "").unwrap();
        (temp, layout)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{AUTOREMOVE_DRY_RUN, BREW_INFO, CLEANUP_DRY_RUN, package, with_brew};
    use super::*;
    use crate::core::apps::testing::sandbox;
    use crate::core::host::fake::FakeHost;
    use crate::core::sizer::DirSizer;
    use std::fs;

    const INFO_LINE_START: &str =
        "/usr/bin/env HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ENV_HINTS=1 NONINTERACTIVE=1";

    fn packages() -> Vec<Package> {
        parse_packages(BREW_INFO, Path::new("/opt/homebrew")).unwrap()
    }

    fn named<'a>(packages: &'a [Package], name: &str) -> &'a Package {
        packages
            .iter()
            .find(|package| package.name == name)
            .unwrap_or_else(|| panic!("no package {name}"))
    }

    fn fill(folder: &Path, bytes: usize) {
        fs::create_dir_all(folder).unwrap();
        fs::write(folder.join("data"), vec![1_u8; bytes]).unwrap();
    }

    #[test]
    fn the_fixture_lists_every_formula_and_cask() {
        let packages = packages();
        let formulae = packages.iter().filter(|p| p.kind == PackageKind::Formula);
        let casks = packages.iter().filter(|p| p.kind == PackageKind::Cask);

        assert_eq!(formulae.count(), 10);
        assert_eq!(casks.count(), 2);
    }

    #[test]
    fn a_formula_keeps_its_newest_version_and_the_first_request_flag() {
        let packages = packages();

        let go = named(&packages, "go");
        assert_eq!(go.version, "1.27.3");
        assert!(go.on_request);
        assert!(!go.outdated);
        assert!(!go.pinned);
        assert!(go.apps.is_empty());
        assert_eq!(go.path, Path::new("/opt/homebrew/Cellar/go"));

        let ada = named(&packages, "ada-url");
        assert_eq!(ada.version, "4.0.0_1");
        assert!(!ada.on_request);
        assert!(!named(&packages, "brotli").on_request);
        assert_eq!(named(&packages, "brotli").version, "1.2.1");
    }

    #[test]
    fn outdated_and_pinned_come_from_brew() {
        let packages = packages();

        assert!(named(&packages, "bun").outdated);
        assert!(!named(&packages, "bun").pinned);
        assert!(named(&packages, "fd").pinned);
        assert!(!named(&packages, "fd").outdated);
    }

    #[test]
    fn a_tap_formula_is_listed_by_its_short_name() {
        let packages = packages();

        let crush = named(&packages, "crush");
        assert_eq!(crush.version, "0.99.0");
        assert!(crush.on_request);
        assert!(!crush.outdated);
        assert!(!crush.pinned);
        assert_eq!(crush.path, Path::new("/opt/homebrew/Cellar/crush"));
    }

    #[test]
    fn a_formula_on_request_can_still_be_used_by_another() {
        let packages = packages();

        let shellcheck = named(&packages, "shellcheck");
        assert_eq!(shellcheck.version, "0.11.0");
        assert!(shellcheck.on_request);
    }

    #[test]
    fn a_cask_has_its_version_apps_and_folder() {
        let packages = packages();

        let ghostty = named(&packages, "ghostty");
        assert_eq!(ghostty.kind, PackageKind::Cask);
        assert_eq!(ghostty.version, "1.3.1");
        assert_eq!(ghostty.apps, ["Ghostty.app"]);
        assert!(ghostty.on_request);
        assert!(!ghostty.pinned);
        assert!(ghostty.used_by.is_empty());
        assert_eq!(ghostty.path, Path::new("/opt/homebrew/Caskroom/ghostty"));

        let codex = named(&packages, "codex");
        assert_eq!(codex.version, "0.166.0");
        assert!(codex.apps.is_empty());
    }

    #[test]
    fn used_by_lists_the_installed_formulae_that_depend_on_a_package() {
        let packages = packages();

        assert_eq!(named(&packages, "shellcheck").used_by, ["actionlint"]);
        assert_eq!(named(&packages, "go").used_by, ["golangci-lint", "gosec"]);
        assert!(named(&packages, "brotli").used_by.is_empty());
    }

    fn fixture_inventory() -> Inventory {
        Inventory {
            packages: packages(),
            ..Inventory::default()
        }
    }

    #[test]
    fn forget_drops_the_formula_and_its_name_from_what_it_used() {
        let mut inventory = fixture_inventory();

        inventory.forget("actionlint");

        assert!(inventory.packages.iter().all(|p| p.name != "actionlint"));
        assert!(named(&inventory.packages, "shellcheck").used_by.is_empty());
        inventory.forget("golangci-lint");
        assert_eq!(named(&inventory.packages, "go").used_by, ["gosec"]);
    }

    #[test]
    fn forget_keeps_a_cask_with_the_name_of_the_formula() {
        let mut inventory = Inventory {
            packages: vec![
                package(PackageKind::Formula, "x", "1", 0),
                package(PackageKind::Cask, "x", "1", 0),
            ],
            ..Inventory::default()
        };

        inventory.forget("x");

        assert_eq!(
            inventory.packages,
            [package(PackageKind::Cask, "x", "1", 0)]
        );
    }

    #[test]
    fn forget_of_an_unknown_name_changes_nothing() {
        let mut inventory = fixture_inventory();

        inventory.forget("no-such-formula");

        assert_eq!(inventory.packages, packages());
    }

    #[test]
    fn a_dependency_written_with_the_full_name_counts() {
        let json = r#"{"formulae":[
            {"name":"lib","full_name":"someone/tap/lib","dependencies":[],
             "installed":[{"version":"1","installed_on_request":false}]},
            {"name":"zed","full_name":"zed","dependencies":["someone/tap/lib"],
             "installed":[{"version":"1","installed_on_request":true}]},
            {"name":"app","full_name":"app","dependencies":["lib"],
             "installed":[{"version":"1","installed_on_request":true}]}
        ],"casks":[]}"#;

        let packages = parse_packages(json, Path::new("/opt/homebrew")).unwrap();

        assert_eq!(named(&packages, "lib").used_by, ["app", "zed"]);
    }

    #[test]
    fn a_formula_takes_the_request_flag_from_the_first_keg_and_the_version_from_the_last() {
        let json = r#"{"formulae":[
            {"name":"lib","installed":[
                {"version":"1","installed_on_request":true},
                {"version":"2","installed_on_request":false}]}
        ],"casks":[]}"#;

        let packages = parse_packages(json, Path::new("/opt/homebrew")).unwrap();

        assert!(packages[0].on_request);
        assert_eq!(packages[0].version, "2");
    }

    #[test]
    fn a_cask_lists_only_its_app_names() {
        let json = r#"{"formulae":[],"casks":[
            {"token":"tool","installed":"1","artifacts":[
                {"app":["A.app",{"target":"B.app"}]},{"binary":["x"]}]}
        ]}"#;

        let packages = parse_packages(json, Path::new("/opt/homebrew")).unwrap();

        assert_eq!(packages[0].apps, ["A.app"]);
    }

    #[test]
    fn json_that_does_not_parse_is_an_error() {
        let error = parse_packages("not json", Path::new("/opt/homebrew")).unwrap_err();

        assert!(error.starts_with("Homebrew printed a package list taert cannot read: "));
    }

    #[test]
    fn without_brew_the_scan_is_empty_and_runs_nothing() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();

        let inventory = scan(&layout, &host).unwrap();

        assert!(inventory.packages.is_empty());
        assert_eq!(inventory.notes, ["Homebrew is not installed."]);
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn the_scan_makes_one_brew_info_call() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let host = FakeHost::default();
        host.reply(INFO_LINE_START, BREW_INFO);

        let inventory = scan(&layout, &host).unwrap();

        let brew = layout.system("opt/homebrew/bin/brew");
        assert_eq!(
            *host.calls.lock().unwrap(),
            [format!(
                "{INFO_LINE_START} {} info --json=v2 --installed",
                brew.display()
            )]
        );
        assert_eq!(inventory.packages.len(), 12);
        assert!(inventory.notes.is_empty());
    }

    #[test]
    fn usr_local_is_used_when_opt_homebrew_has_no_brew() {
        let (_temp, layout) = with_brew("usr/local");
        let host = FakeHost::default();
        host.reply(INFO_LINE_START, BREW_INFO);

        let inventory = scan(&layout, &host).unwrap();

        let brew = layout.system("usr/local/bin/brew");
        assert!(host.ran(&format!("{INFO_LINE_START} {}", brew.display())));
        assert_eq!(
            named(&inventory.packages, "go").path,
            layout.system("usr/local/Cellar/go")
        );
    }

    #[test]
    fn opt_homebrew_wins_when_both_exist() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let other = layout.system("usr/local/bin/brew");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(other, "").unwrap();

        assert_eq!(find(&layout).unwrap().prefix, layout.system("opt/homebrew"));
    }

    #[test]
    fn sizes_come_from_the_cellar_and_caskroom_and_the_list_is_ordered() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let prefix = layout.system("opt/homebrew");
        fill(&prefix.join("Cellar/go"), 300_000);
        fill(&prefix.join("Cellar/bun"), 100_000);
        fill(&prefix.join("Cellar/fd"), 100_000);
        fill(&prefix.join("Caskroom/ghostty"), 900_000);
        let host = FakeHost::default();
        host.reply(INFO_LINE_START, BREW_INFO);

        let inventory = scan(&layout, &host).unwrap();

        let go = named(&inventory.packages, "go");
        assert_eq!(go.size_bytes, DirSizer::size_of(&prefix.join("Cellar/go")));
        assert!(go.size_bytes >= 300_000);
        assert_eq!(named(&inventory.packages, "shellcheck").size_bytes, 0);

        let names: Vec<&str> = inventory.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(&names[..3], ["go", "bun", "fd"]);
        assert_eq!(names[3], "actionlint");
        assert_eq!(&names[10..], ["ghostty", "codex"]);
    }

    #[test]
    fn a_failed_brew_info_is_an_error_with_its_message() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let host = FakeHost::default();
        host.fail(INFO_LINE_START, "Error: broken tap\n");

        let error = scan(&layout, &host).unwrap_err();

        assert_eq!(
            error,
            "Homebrew did not list its packages: Error: broken tap"
        );
    }

    #[test]
    fn unreadable_output_from_brew_is_an_error() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let host = FakeHost::default();
        host.reply(INFO_LINE_START, "Warning: something");

        let error = scan(&layout, &host).unwrap_err();

        assert!(error.starts_with("Homebrew printed a package list taert cannot read: "));
    }

    #[test]
    fn list_scan_groups_formulae_and_casks() {
        let packages = packages();
        let inventory = Inventory {
            packages,
            notes: vec!["a note".to_string()],
            ..Inventory::default()
        };

        let scan = list_scan(&inventory);

        assert_eq!(scan.id, SCAN_ID);
        assert_eq!(scan.title, "Homebrew");
        assert_eq!(scan.notes, ["a note"]);
        let tools: Vec<&str> = scan.groups.iter().map(|g| g.tool.as_str()).collect();
        assert_eq!(tools, ["Formulae", "Casks · Remove with taert uninstall"]);
        assert_eq!(scan.groups[0].items.len(), 10);
        assert_eq!(scan.groups[1].items.len(), 2);
    }

    fn rows(scan: &CategoryScan) -> Vec<(String, u64)> {
        scan.groups
            .iter()
            .flat_map(|group| &group.items)
            .map(|item| (item.path.to_string_lossy().into_owned(), item.size_bytes))
            .collect()
    }

    #[test]
    fn list_scan_labels_say_what_matters() {
        let mut packages = packages();
        packages
            .iter_mut()
            .find(|p| p.name == "go")
            .unwrap()
            .size_bytes = 5_000;
        let scan = list_scan(&Inventory {
            packages,
            notes: Vec::new(),
            ..Inventory::default()
        });

        let rows = rows(&scan);
        let label = |name: &str| {
            rows.iter()
                .find(|(label, _)| label.starts_with(&format!("{name} ")))
                .unwrap()
        };

        assert_eq!(
            label("go"),
            &("go 1.27.3 · on request · used by 2".to_string(), 5_000)
        );
        assert_eq!(label("bun").0, "bun 1.4.2 · on request · outdated");
        assert_eq!(label("fd").0, "fd 10.5.0 · on request · pinned");
        assert_eq!(label("ada-url").0, "ada-url 4.0.0_1 · dependency");
        assert_eq!(label("ghostty").0, "ghostty 1.3.1 · Ghostty.app");
        assert_eq!(label("codex").0, "codex 0.166.0");
        assert!(
            scan.groups
                .iter()
                .flat_map(|group| &group.items)
                .all(|row| !row.selected && row.modified.is_none())
        );
    }

    #[test]
    fn list_scan_leaves_out_an_empty_group() {
        let scan = list_scan(&Inventory::default());

        assert!(scan.groups.is_empty());
    }

    #[test]
    fn plain_text_lines_the_columns_up() {
        let formula = |name, version, bytes| package(PackageKind::Formula, name, version, bytes);
        let mut go = formula("go", "1.27.3", 250_100_000);
        go.used_by = vec!["a".to_string(), "b".to_string()];
        let mut ada = formula("ada-url", "4.0.0_1", 3_100_000);
        ada.on_request = false;
        let mut fd = formula("fd", "10.5.0", 2_000_000);
        fd.pinned = true;
        let mut bun = formula("bun", "1.4.2", 60_000_000);
        bun.outdated = true;
        let ghostty = package(PackageKind::Cask, "ghostty", "1.3.1", 50_000_000);
        let inventory = Inventory {
            packages: vec![go, ada, fd, bun, ghostty],
            notes: Vec::new(),
            ..Inventory::default()
        };

        let text = plain_text(&inventory);

        let expected = "\
Formulae
  go       1.27.3   250.1 MB  on request  -         -       2
  ada-url  4.0.0_1    3.1 MB  dependency  -         -       0
  fd       10.5.0     2.0 MB  on request  -         pinned  0
  bun      1.4.2     60.0 MB  on request  outdated  -       0

Casks (remove with taert uninstall)
  ghostty  1.3.1     50.0 MB  on request  -         -       0
";
        assert_eq!(text, expected);
    }

    const REMOVING_LINE_START: &str = "/usr/bin/env HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ENV_HINTS=1 NONINTERACTIVE=1 HOMEBREW_NO_AUTOREMOVE=1";

    #[test]
    fn autoremove_names_follow_the_header() {
        assert_eq!(parse_autoremove(AUTOREMOVE_DRY_RUN), ["ada-url", "brotli"]);
    }

    #[test]
    fn autoremove_text_without_the_header_gives_no_names() {
        assert!(parse_autoremove("ada-url\nbrotli\n").is_empty());
        assert!(parse_autoremove("").is_empty());
    }

    #[test]
    fn autoremove_ignores_what_comes_before_the_header() {
        let text = "stray\n==> Would autoremove 1 unneeded formula:\nada-url\n";

        assert_eq!(parse_autoremove(text), ["ada-url"]);
    }

    #[test]
    fn autoremove_names_stop_at_a_blank_line_or_the_next_header() {
        let text = "==> Would autoremove 2 unneeded formulae:\n  ada-url \nbrotli\n\nlater\n";
        assert_eq!(parse_autoremove(text), ["ada-url", "brotli"]);

        let text = "==> Would autoremove 1 unneeded formula:\nada-url\n==> Other\nbrotli\n";
        assert_eq!(parse_autoremove(text), ["ada-url"]);
    }

    #[test]
    fn old_versions_come_only_from_the_cellar_lines_of_the_fixture() {
        let found = parse_old_versions(CLEANUP_DRY_RUN, Path::new("/opt/homebrew"));

        assert_eq!(
            found,
            [
                (
                    "ada-url".to_string(),
                    vec![PathBuf::from("/opt/homebrew/Cellar/ada-url/4.0.0")]
                ),
                (
                    "brotli".to_string(),
                    vec![PathBuf::from("/opt/homebrew/Cellar/brotli/1.2.0")]
                ),
            ]
        );
    }

    #[test]
    fn old_versions_group_by_formula_in_order_of_first_appearance() {
        let text = "\
Would remove: /opt/homebrew/Cellar/go/1.26.0 (10 files, 1MB)
Would remove: /opt/homebrew/Cellar/bun/1.4.0 (10 files, 1MB)
Would remove: /opt/homebrew/Cellar/go/1.25.0 (10 files, 1MB)
";

        let found = parse_old_versions(text, Path::new("/opt/homebrew"));

        assert_eq!(
            found,
            [
                (
                    "go".to_string(),
                    vec![
                        PathBuf::from("/opt/homebrew/Cellar/go/1.26.0"),
                        PathBuf::from("/opt/homebrew/Cellar/go/1.25.0")
                    ]
                ),
                (
                    "bun".to_string(),
                    vec![PathBuf::from("/opt/homebrew/Cellar/bun/1.4.0")]
                ),
            ]
        );
    }

    #[test]
    fn a_path_with_a_bracket_in_it_keeps_its_whole_name() {
        let text = "Would remove: /opt/homebrew/Cellar/odd (x)/1.0 (3 files, 1KB)\n";

        let found = parse_old_versions(text, Path::new("/opt/homebrew"));

        assert_eq!(
            found,
            [(
                "odd (x)".to_string(),
                vec![PathBuf::from("/opt/homebrew/Cellar/odd (x)/1.0")]
            )]
        );
    }

    #[test]
    fn old_versions_leave_out_other_prefixes_and_paths_that_climb_out() {
        let text = "\
Would remove: /usr/local/Cellar/go/1.0 (1 files, 1KB)
Would remove: /opt/homebrew/Cellar/../../etc/passwd (1 files, 1KB)
Would remove: /opt/homebrew/Cellar/go (1 files, 1KB)
Would remove: /opt/homebrew/Cellar/go/1.0
";

        assert!(parse_old_versions(text, Path::new("/opt/homebrew")).is_empty());
    }

    const READS: [&str; 3] = [
        "info --json=v2 --installed",
        "autoremove --dry-run",
        "cleanup --dry-run",
    ];

    fn prefix_of(layout: &Layout) -> PathBuf {
        layout.system("opt/homebrew")
    }

    fn brew_of(layout: &Layout) -> PathBuf {
        layout.system("opt/homebrew/bin/brew")
    }

    fn line(layout: &Layout, args: &str) -> String {
        format!("{INFO_LINE_START} {} {args}", brew_of(layout).display())
    }

    fn removing_line(layout: &Layout, args: &str) -> String {
        format!("{REMOVING_LINE_START} {} {args}", brew_of(layout).display())
    }

    fn reading_host(layout: &Layout, info: &str, autoremove: &str, cleanup: &str) -> FakeHost {
        let host = FakeHost::default();
        host.reply(&line(layout, READS[0]), info);
        host.reply(&line(layout, READS[1]), autoremove);
        host.reply(&line(layout, READS[2]), cleanup);
        host
    }

    fn info(formulae: &[(&str, &[&str], bool)]) -> String {
        let formulae: Vec<Value> = formulae
            .iter()
            .map(|(name, dependencies, pinned)| {
                serde_json::json!({
                    "name": name,
                    "full_name": name,
                    "dependencies": dependencies,
                    "pinned": pinned,
                    "installed": [{"version": "1", "installed_on_request": false}],
                })
            })
            .collect();
        serde_json::json!({"formulae": formulae, "casks": []}).to_string()
    }

    fn autoremove_of(names: &[&str]) -> String {
        format!(
            "==> Would autoremove {} unneeded formulae:\n{}\n",
            names.len(),
            names.join("\n")
        )
    }

    fn old(layout: &Layout, name: &str, version: &str) -> String {
        format!(
            "Would remove: {}/Cellar/{name}/{version} (3 files, 9.9MB)\n",
            prefix_of(layout).display()
        )
    }

    fn items<'a>(scan: &'a CategoryScan, tool: &str) -> &'a [ScanItem] {
        &scan
            .groups
            .iter()
            .find(|group| group.tool == tool)
            .unwrap_or_else(|| panic!("no group {tool}"))
            .items
    }

    fn labels(items: &[ScanItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| item.path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn without_brew_the_cleanup_scan_is_empty_and_runs_nothing() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();

        let scan = cleanup_scan(&layout, &host).unwrap();

        assert_eq!(scan.id, CLEANUP_ID);
        assert_eq!(scan.title, "Homebrew Cleanup");
        assert!(scan.groups.is_empty());
        assert_eq!(scan.notes, ["Homebrew is not installed."]);
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn the_cleanup_scan_runs_the_three_read_commands_and_nothing_else() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let host = reading_host(&layout, BREW_INFO, AUTOREMOVE_DRY_RUN, CLEANUP_DRY_RUN);

        cleanup_scan(&layout, &host).unwrap();

        let expected: Vec<String> = READS.iter().map(|args| line(&layout, args)).collect();
        assert_eq!(*host.calls.lock().unwrap(), expected);
    }

    #[test]
    fn unused_dependencies_and_old_versions_are_two_groups_with_sizes_from_disk() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let prefix = prefix_of(&layout);
        fill(&prefix.join("Cellar/ada-url/4.0.0_1"), 100_000);
        fill(&prefix.join("Cellar/brotli/1.2.1"), 40_000);
        fill(&prefix.join("Cellar/bun/1.4.0"), 20_000);
        fill(&prefix.join("Cellar/bun/1.4.1"), 30_000);
        fill(&prefix.join("Cellar/go/1.27.2"), 500_000);
        let cleanup = [
            "Warning: Skipping bun: most recent version 1.4.4_1 not installed\n".to_string(),
            old(&layout, "ada-url", "4.0.0"),
            old(&layout, "bun", "1.4.1"),
            old(&layout, "go", "1.27.2"),
            old(&layout, "bun", "1.4.0"),
            "Would remove: /Users/igor/Library/Caches/Homebrew/bun--1.4.1 (336KB)\n".to_string(),
        ]
        .concat();
        let host = reading_host(&layout, BREW_INFO, AUTOREMOVE_DRY_RUN, &cleanup);

        let scan = cleanup_scan(&layout, &host).unwrap();

        let groups: Vec<&str> = scan.groups.iter().map(|g| g.tool.as_str()).collect();
        assert_eq!(groups, ["Unused dependencies", "Old versions"]);
        let unused = items(&scan, "Unused dependencies");
        assert_eq!(labels(unused), ["ada-url 4.0.0_1", "brotli 1.2.1"]);
        assert_eq!(
            unused[0].size_bytes,
            DirSizer::size_of(&prefix.join("Cellar/ada-url"))
        );
        assert!(unused[0].size_bytes >= 100_000);
        assert_eq!(
            unused[0].brew,
            Some(BrewTask::Uninstall {
                name: "ada-url".to_string(),
                brew: brew_of(&layout),
                cellar: prefix.join("Cellar/ada-url"),
            })
        );

        let old_versions = items(&scan, "Old versions");
        assert_eq!(labels(old_versions), ["go 1.27.2", "bun 1.4.1, 1.4.0"]);
        assert_eq!(
            old_versions[1].size_bytes,
            DirSizer::size_of(&prefix.join("Cellar/bun/1.4.1"))
                + DirSizer::size_of(&prefix.join("Cellar/bun/1.4.0"))
        );
        assert!(old_versions[1].size_bytes >= 50_000);
        assert_eq!(
            old_versions[1].brew,
            Some(BrewTask::Cleanup {
                name: "bun".to_string(),
                brew: brew_of(&layout),
                cellar: prefix.join("Cellar/bun"),
            })
        );
        assert!(scan.notes.is_empty());
    }

    #[test]
    fn an_unused_dependency_is_not_listed_again_as_an_old_version() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let cleanup = old(&layout, "ada-url", "4.0.0") + &old(&layout, "brotli", "1.2.0");
        let host = reading_host(&layout, BREW_INFO, AUTOREMOVE_DRY_RUN, &cleanup);

        let scan = cleanup_scan(&layout, &host).unwrap();

        let groups: Vec<&str> = scan.groups.iter().map(|g| g.tool.as_str()).collect();
        assert_eq!(groups, ["Unused dependencies"]);
    }

    #[test]
    fn an_autoremove_name_matches_a_formula_by_its_full_name() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let json = r#"{"formulae":[
            {"name":"lib","full_name":"someone/tap/lib","dependencies":[],
             "installed":[{"version":"2","installed_on_request":false}]}
        ],"casks":[]}"#;
        let host = reading_host(&layout, json, &autoremove_of(&["someone/tap/lib"]), "");

        let scan = cleanup_scan(&layout, &host).unwrap();

        let unused = items(&scan, "Unused dependencies");
        assert_eq!(labels(unused), ["lib 2"]);
        assert!(matches!(
            &unused[0].brew,
            Some(BrewTask::Uninstall { name, .. }) if name == "lib"
        ));
    }

    #[test]
    fn an_old_version_of_a_formula_that_is_not_installed_gives_no_row() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let cleanup = old(&layout, "--prune=all", "1.0") + &old(&layout, "ghost", "1.0");
        let host = reading_host(&layout, BREW_INFO, "", &cleanup);

        let scan = cleanup_scan(&layout, &host).unwrap();

        assert!(scan.groups.is_empty(), "{:?}", scan.groups);
    }

    #[test]
    fn an_autoremove_name_that_is_not_installed_gives_no_row() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let host = reading_host(&layout, BREW_INFO, &autoremove_of(&["ghost"]), "");

        let scan = cleanup_scan(&layout, &host).unwrap();

        assert!(scan.groups.is_empty());
    }

    #[test]
    fn pinned_formulae_start_unselected_and_others_selected_in_both_groups() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let json = info(&[
            ("held", &[], true),
            ("free", &[], false),
            ("keep", &[], true),
            ("tidy", &[], false),
        ]);
        let cleanup = old(&layout, "keep", "0") + &old(&layout, "tidy", "0");
        let host = reading_host(&layout, &json, &autoremove_of(&["held", "free"]), &cleanup);

        let scan = cleanup_scan(&layout, &host).unwrap();

        let selected = |tool: &str| -> Vec<(String, bool)> {
            let mut rows: Vec<_> = items(&scan, tool)
                .iter()
                .map(|i| (i.path.to_string_lossy().into_owned(), i.selected))
                .collect();
            rows.sort();
            rows
        };
        assert_eq!(
            selected("Unused dependencies"),
            [("free 1".to_string(), true), ("held 1".to_string(), false)]
        );
        assert_eq!(
            selected("Old versions"),
            [("keep 0".to_string(), false), ("tidy 0".to_string(), true)]
        );
    }

    #[test]
    fn a_formula_comes_before_the_unused_dependency_it_needs() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let prefix = prefix_of(&layout);
        fill(&prefix.join("Cellar/a/1"), 50_000);
        fill(&prefix.join("Cellar/b/1"), 200_000);
        fill(&prefix.join("Cellar/c/1"), 100_000);
        let json = info(&[("a", &["b"], false), ("b", &[], false), ("c", &[], false)]);
        let host = reading_host(&layout, &json, &autoremove_of(&["b", "a", "c"]), "");

        let scan = cleanup_scan(&layout, &host).unwrap();

        let unused = items(&scan, "Unused dependencies");
        assert_eq!(labels(unused), ["c 1", "a 1", "b 1"]);
    }

    #[test]
    fn old_versions_and_equal_sizes_are_ordered_by_size_then_name() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let prefix = prefix_of(&layout);
        fill(&prefix.join("Cellar/x/1"), 10_000);
        fill(&prefix.join("Cellar/y/1"), 10_000);
        fill(&prefix.join("Cellar/z/1"), 90_000);
        let json = info(&[("x", &[], false), ("y", &[], false), ("z", &[], false)]);
        let cleanup = old(&layout, "y", "1") + &old(&layout, "x", "1") + &old(&layout, "z", "1");
        let host = reading_host(&layout, &json, "", &cleanup);

        let scan = cleanup_scan(&layout, &host).unwrap();

        assert_eq!(labels(items(&scan, "Old versions")), ["z 1", "x 1", "y 1"]);
    }

    #[test]
    fn a_failed_read_command_is_an_error_that_names_it() {
        let cases = [
            (0, "Homebrew did not list its packages: Error: boom"),
            (
                1,
                "Homebrew did not list the unused dependencies: Error: boom",
            ),
            (2, "Homebrew did not list the old versions: Error: boom"),
        ];
        for (failing, expected) in cases {
            let (_temp, layout) = with_brew("opt/homebrew");
            let host = FakeHost::default();
            let replies = [BREW_INFO, AUTOREMOVE_DRY_RUN, CLEANUP_DRY_RUN];
            for (index, (args, stdout)) in READS.iter().zip(replies).enumerate() {
                if index == failing {
                    host.fail(&line(&layout, args), "Error: boom\n");
                } else {
                    host.reply(&line(&layout, args), stdout);
                }
            }

            let error = cleanup_scan(&layout, &host).unwrap_err();

            assert_eq!(error, expected);
        }
    }

    fn task_setup(users: &[&str]) -> (tempfile::TempDir, Layout, FakeHost, PathBuf) {
        let (temp, layout) = with_brew("opt/homebrew");
        let cellar = prefix_of(&layout).join("Cellar/lib");
        fill(&cellar.join("1"), 70_000);
        let mut formulae = vec![("lib", Vec::new(), false)];
        formulae.extend(users.iter().map(|user| (*user, vec!["lib"], false)));
        let formulae: Vec<(&str, &[&str], bool)> = formulae
            .iter()
            .map(|(name, deps, pinned)| (*name, deps.as_slice(), *pinned))
            .collect();
        let host = FakeHost::default();
        host.reply(&line(&layout, READS[0]), &info(&formulae));
        (temp, layout, host, cellar)
    }

    fn lib_uninstall_task(layout: &Layout, cellar: &Path) -> BrewTask {
        BrewTask::Uninstall {
            name: "lib".to_string(),
            brew: brew_of(layout),
            cellar: cellar.to_path_buf(),
        }
    }

    fn cleanup_task(layout: &Layout, cellar: &Path) -> BrewTask {
        BrewTask::Cleanup {
            name: "lib".to_string(),
            brew: brew_of(layout),
            cellar: cellar.to_path_buf(),
        }
    }

    #[test]
    fn an_uninstall_stops_when_another_formula_uses_the_package_again() {
        let (_temp, layout, host, cellar) = task_setup(&["tool", "app"]);

        let error = run_task(&host, &lib_uninstall_task(&layout, &cellar)).unwrap_err();

        assert_eq!(error, "lib stays: app, tool use it.");
        assert_eq!(*host.calls.lock().unwrap(), [line(&layout, READS[0])]);
        assert!(cellar.exists());
    }

    #[test]
    fn an_uninstall_stops_when_the_package_is_pinned_by_then() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let cellar = prefix_of(&layout).join("Cellar/lib");
        fill(&cellar.join("1"), 70_000);
        let host = FakeHost::default();
        let formulae: &[(&str, &[&str], bool)] = &[("lib", &[], true)];
        host.reply(&line(&layout, READS[0]), &info(formulae));

        let error = run_task(&host, &lib_uninstall_task(&layout, &cellar)).unwrap_err();

        assert_eq!(error, "lib is pinned. Run brew unpin lib first.");
        assert_eq!(*host.calls.lock().unwrap(), [line(&layout, READS[0])]);
        assert!(cellar.exists());
    }

    #[test]
    fn an_uninstall_of_a_package_nothing_uses_runs_one_brew_uninstall_and_counts_the_freed_bytes() {
        let (_temp, layout, host, cellar) = task_setup(&[]);
        let before = DirSizer::size_of(&cellar);
        let doomed = cellar.clone();
        host.reply(&removing_line(&layout, "uninstall"), "");
        host.when(&removing_line(&layout, "uninstall"), move || {
            fs::remove_dir_all(&doomed).unwrap();
        });

        let freed = run_task(&host, &lib_uninstall_task(&layout, &cellar)).unwrap();

        assert_eq!(
            *host.calls.lock().unwrap(),
            [
                line(&layout, READS[0]),
                removing_line(&layout, "uninstall lib")
            ]
        );
        assert!(before >= 70_000);
        assert_eq!(freed, before);
    }

    #[test]
    fn an_uninstall_of_a_package_that_is_gone_is_an_error_and_runs_no_uninstall() {
        let (_temp, layout, host, cellar) = task_setup(&[]);
        let task = BrewTask::Uninstall {
            name: "other".to_string(),
            brew: brew_of(&layout),
            cellar: prefix_of(&layout).join("Cellar/other"),
        };

        let error = run_task(&host, &task).unwrap_err();

        assert_eq!(error, "other is no longer installed.");
        assert!(!host.ran(REMOVING_LINE_START));
        assert!(cellar.exists());
    }

    #[test]
    fn a_failed_uninstall_is_an_error_with_the_message_of_brew() {
        let (_temp, layout, host, cellar) = task_setup(&[]);
        host.fail(&removing_line(&layout, "uninstall lib"), "Error: boom\n");

        let error = run_task(&host, &lib_uninstall_task(&layout, &cellar)).unwrap_err();

        assert_eq!(error, "Homebrew could not remove lib: Error: boom");
    }

    fn uninstall_calls(host: &FakeHost) -> Vec<String> {
        host.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.contains(" uninstall "))
            .cloned()
            .collect()
    }

    /// Like brew without `--force`: each call removes one version, and the empty folder after the
    /// last one.
    fn uninstall_removes_one_version_per_call(host: &FakeHost, layout: &Layout, cellar: &Path) {
        let line = removing_line(layout, "uninstall lib");
        host.reply(&line, "");
        let cellar = cellar.to_path_buf();
        host.when(&line, move || {
            let mut left: Vec<PathBuf> = fs::read_dir(&cellar)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            left.sort();
            fs::remove_dir_all(&left[0]).unwrap();
            if left.len() == 1 {
                fs::remove_dir(&cellar).unwrap();
            }
        });
    }

    #[test]
    fn an_uninstall_repeats_brew_uninstall_until_every_version_is_gone() {
        let (_temp, layout, host, cellar) = task_setup(&[]);
        fill(&cellar.join("2"), 30_000);
        let before = DirSizer::size_of(&cellar);
        uninstall_removes_one_version_per_call(&host, &layout, &cellar);

        let freed = run_task(&host, &lib_uninstall_task(&layout, &cellar)).unwrap();

        assert_eq!(
            uninstall_calls(&host),
            [
                removing_line(&layout, "uninstall lib"),
                removing_line(&layout, "uninstall lib")
            ]
        );
        assert!(!cellar.exists());
        assert!(before >= 100_000);
        assert_eq!(freed, before);
    }

    #[test]
    fn an_uninstall_stops_when_a_call_removes_nothing() {
        let (_temp, layout, host, cellar) = task_setup(&[]);
        fill(&cellar.join("2"), 30_000);
        host.reply(&removing_line(&layout, "uninstall lib"), "");

        let error = run_task(&host, &lib_uninstall_task(&layout, &cellar)).unwrap_err();

        assert_eq!(uninstall_calls(&host).len(), 1);
        assert_eq!(
            error,
            "Homebrew removed part of lib; 1, 2 are still installed."
        );
    }

    #[test]
    fn an_uninstall_of_one_version_runs_brew_uninstall_once() {
        let (_temp, layout, host, cellar) = task_setup(&[]);
        host.reply(&removing_line(&layout, "uninstall lib"), "");

        let error = run_task(&host, &lib_uninstall_task(&layout, &cellar)).unwrap_err();

        assert_eq!(uninstall_calls(&host).len(), 1);
        assert_eq!(error, "Homebrew removed part of lib; 1 is still installed.");
    }

    #[test]
    fn an_uninstall_with_no_cellar_folder_is_an_error_and_runs_no_uninstall() {
        let (_temp, layout, host, cellar) = task_setup(&[]);
        fs::remove_dir_all(&cellar).unwrap();

        let error = run_task(&host, &lib_uninstall_task(&layout, &cellar)).unwrap_err();

        assert_eq!(
            error,
            format!(
                "taert finds no installed version of lib in {}.",
                cellar.display()
            )
        );
        assert_eq!(*host.calls.lock().unwrap(), [line(&layout, READS[0])]);
    }

    #[test]
    fn an_uninstall_with_no_version_folder_is_an_error_and_runs_no_uninstall() {
        let (_temp, layout, host, cellar) = task_setup(&[]);
        fs::remove_dir_all(cellar.join("1")).unwrap();
        fs::write(cellar.join("stray"), "kept").unwrap();

        let error = run_task(&host, &lib_uninstall_task(&layout, &cellar)).unwrap_err();

        assert_eq!(
            error,
            format!(
                "taert finds no installed version of lib in {}.",
                cellar.display()
            )
        );
        assert_eq!(*host.calls.lock().unwrap(), [line(&layout, READS[0])]);
    }

    #[test]
    fn an_uninstall_is_done_when_no_version_is_left_even_if_the_folder_stays() {
        let (_temp, layout, host, cellar) = task_setup(&[]);
        fill(&cellar.join("2"), 30_000);
        fs::write(cellar.join("stray"), "kept").unwrap();
        let both = DirSizer::size_of(&cellar.join("1")) + DirSizer::size_of(&cellar.join("2"));
        let uninstall = removing_line(&layout, "uninstall lib");
        host.reply(&uninstall, "");
        let rack = cellar.clone();
        // Like brew when the rack holds a file other than .DS_Store: the rack stays.
        host.when(&uninstall, move || {
            let mut left: Vec<PathBuf> = fs::read_dir(&rack)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.is_dir())
                .collect();
            left.sort();
            fs::remove_dir_all(&left[0]).unwrap();
        });

        let freed = run_task(&host, &lib_uninstall_task(&layout, &cellar)).unwrap();

        assert_eq!(uninstall_calls(&host), [uninstall.clone(), uninstall]);
        assert!(cellar.join("stray").exists());
        assert!(both >= 100_000);
        assert!(freed >= both);
    }

    #[test]
    fn an_uninstall_with_a_cellar_path_too_short_for_a_prefix_is_an_error_and_runs_nothing() {
        let (_temp, layout, host, _cellar) = task_setup(&[]);
        let task = BrewTask::Uninstall {
            name: "lib".to_string(),
            brew: brew_of(&layout),
            cellar: PathBuf::from("/lib"),
        };

        let error = run_task(&host, &task).unwrap_err();

        assert_eq!(error, "taert cannot find the Homebrew prefix of /lib.");
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_cleanup_task_runs_one_brew_cleanup_and_counts_the_freed_bytes() {
        let (_temp, layout, host, cellar) = task_setup(&[]);
        let old_version = cellar.join("1");
        let before = DirSizer::size_of(&cellar);
        host.reply(&removing_line(&layout, "cleanup"), "");
        host.when(&removing_line(&layout, "cleanup lib"), move || {
            fs::remove_dir_all(&old_version).unwrap();
        });

        let freed = run_task(&host, &cleanup_task(&layout, &cellar)).unwrap();

        assert_eq!(
            *host.calls.lock().unwrap(),
            [removing_line(&layout, "cleanup lib")]
        );
        assert_eq!(freed, before - DirSizer::size_of(&cellar));
        assert!(freed >= 70_000);
    }

    #[test]
    fn a_failed_cleanup_is_an_error_with_the_message_of_brew() {
        let (_temp, layout, host, cellar) = task_setup(&[]);
        host.fail(&removing_line(&layout, "cleanup lib"), "Error: boom\n");

        let error = run_task(&host, &cleanup_task(&layout, &cellar)).unwrap_err();

        assert_eq!(error, "Homebrew could not clean up lib: Error: boom");
    }

    #[test]
    fn no_call_has_force_zap_or_ignore_dependencies() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let prefix = prefix_of(&layout);
        fill(&prefix.join("Cellar/ada-url/4.0.0_1"), 1_000);
        fill(&prefix.join("Cellar/bun/1.4.0"), 1_000);
        let cleanup = old(&layout, "bun", "1.4.0");
        let host = reading_host(&layout, BREW_INFO, AUTOREMOVE_DRY_RUN, &cleanup);

        let scan = cleanup_scan(&layout, &host).unwrap();
        let tasks: Vec<_> = scan
            .groups
            .iter()
            .flat_map(|group| &group.items)
            .filter_map(|item| item.brew.as_ref())
            .collect();
        assert_eq!(tasks.len(), 3);
        for task in tasks {
            let _ = run_task(&host, task);
        }

        let calls = host.calls.lock().unwrap();
        assert!(calls.iter().any(|call| call.contains("uninstall ada-url")));
        assert!(calls.iter().any(|call| call.contains("cleanup bun")));
        for call in calls.iter() {
            for flag in ["--force", "--zap", "--ignore-dependencies"] {
                assert!(!call.contains(flag), "{call}");
            }
        }
    }

    #[test]
    fn plain_text_leaves_out_an_empty_section() {
        let inventory = Inventory {
            packages: vec![package(PackageKind::Cask, "ghostty", "1.3.1", 0)],
            notes: Vec::new(),
            ..Inventory::default()
        };

        let text = plain_text(&inventory);

        assert!(text.starts_with("Casks (remove with taert uninstall)\n"));
        assert!(!text.contains("Formulae"));
        assert_eq!(plain_text(&Inventory::default()), "");
    }

    fn refusal_packages() -> Vec<Package> {
        let json = r#"{"formulae":[
            {"name":"plain","dependencies":[],
             "installed":[{"version":"1","installed_on_request":true}]},
            {"name":"fd","dependencies":[],"pinned":true,
             "installed":[{"version":"1","installed_on_request":true}]},
            {"name":"lib","dependencies":[],
             "installed":[{"version":"1","installed_on_request":false}]},
            {"name":"tool","dependencies":["lib"],
             "installed":[{"version":"1","installed_on_request":true}]},
            {"name":"app","dependencies":["lib"],
             "installed":[{"version":"1","installed_on_request":true}]}
        ],"casks":[{"token":"ghostty","installed":"1"}]}"#;
        parse_packages(json, Path::new("/opt/homebrew")).unwrap()
    }

    #[test]
    fn a_cask_is_refused_with_the_texts_of_the_issue() {
        let packages = refusal_packages();

        assert_eq!(
            refusal(named(&packages, "ghostty")).unwrap(),
            "ghostty is a cask. Remove it with taert uninstall."
        );
    }

    #[test]
    fn a_pinned_formula_is_refused() {
        let packages = refusal_packages();

        assert_eq!(
            refusal(named(&packages, "fd")).unwrap(),
            "fd is pinned. Run brew unpin fd first."
        );
    }

    #[test]
    fn a_formula_that_others_use_is_refused_with_the_text_of_run_task() {
        let packages = refusal_packages();
        let (_temp, layout, host, cellar) = task_setup(&["tool", "app"]);

        let from_run_task = run_task(&host, &lib_uninstall_task(&layout, &cellar)).unwrap_err();

        let text = refusal(named(&packages, "lib")).unwrap();
        assert_eq!(text, "lib stays: app, tool use it.");
        assert_eq!(text, from_run_task);
    }

    #[test]
    fn a_plain_formula_is_not_refused() {
        let packages = refusal_packages();

        assert_eq!(refusal(named(&packages, "plain")), None);
        assert_eq!(refusal(named(&packages, "tool")), None);
    }

    #[test]
    fn a_pinned_formula_that_is_also_used_is_refused_as_pinned() {
        let mut package = package(PackageKind::Formula, "lib", "1", 0);
        package.pinned = true;
        package.used_by = vec!["app".to_string()];

        assert_eq!(
            refusal(&package).unwrap(),
            "lib is pinned. Run brew unpin lib first."
        );
    }

    fn plan_host(layout: &Layout, json: &str) -> FakeHost {
        let host = FakeHost::default();
        host.reply(&line(layout, READS[0]), json);
        host
    }

    #[test]
    fn without_brew_the_uninstall_plan_is_an_error_and_runs_nothing() {
        let (_temp, layout) = sandbox();
        let host = FakeHost::default();

        let error = uninstall_plan(&layout, &host, "lib").unwrap_err();

        assert_eq!(error, "Homebrew is not installed.");
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_name_that_is_not_installed_is_an_error_after_one_info_call() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let host = plan_host(&layout, &info(&[("lib", &[], false)]));

        let error = uninstall_plan(&layout, &host, "other").unwrap_err();

        assert_eq!(error, "other is not installed.");
        assert_eq!(*host.calls.lock().unwrap(), [line(&layout, READS[0])]);
    }

    #[test]
    fn a_refused_package_gives_its_refusal_after_one_info_call() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let json = r#"{"formulae":[
            {"name":"fd","dependencies":[],"pinned":true,
             "installed":[{"version":"1","installed_on_request":true}]},
            {"name":"lib","dependencies":[],
             "installed":[{"version":"1","installed_on_request":false}]},
            {"name":"app","dependencies":["lib"],
             "installed":[{"version":"1","installed_on_request":true}]}
        ],"casks":[{"token":"ghostty","installed":"1"}]}"#;
        let expected = [
            (
                "ghostty",
                "ghostty is a cask. Remove it with taert uninstall.",
            ),
            ("fd", "fd is pinned. Run brew unpin fd first."),
            ("lib", "lib stays: app use it."),
        ];

        for (name, text) in expected {
            let host = plan_host(&layout, json);

            let error = uninstall_plan(&layout, &host, name).unwrap_err();

            assert_eq!(error, text);
            assert_eq!(*host.calls.lock().unwrap(), [line(&layout, READS[0])]);
        }
    }

    #[test]
    fn a_plain_formula_gives_a_one_row_scan_with_the_uninstall_task() {
        let (_temp, layout, host, cellar) = task_setup(&[]);

        let scan = uninstall_plan(&layout, &host, "lib").unwrap();

        assert_eq!(scan.id, UNINSTALL_ID);
        assert_eq!(scan.title, "Homebrew Uninstall");
        assert!(scan.notes.is_empty());
        assert_eq!(scan.groups.len(), 1);
        assert_eq!(scan.groups[0].tool, "Formula");
        let rows = &scan.groups[0].items;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, Path::new("lib 1"));
        assert_eq!(rows[0].size_bytes, DirSizer::size_of(&cellar));
        assert!(rows[0].size_bytes >= 70_000);
        assert!(rows[0].selected);
        assert_eq!(rows[0].brew, Some(lib_uninstall_task(&layout, &cellar)));
        assert_eq!(*host.calls.lock().unwrap(), [line(&layout, READS[0])]);
    }

    #[test]
    fn a_formula_is_found_by_its_full_name_and_removed_by_its_short_name() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let cellar = prefix_of(&layout).join("Cellar/lib");
        fill(&cellar.join("1"), 1_000);
        let json = r#"{"formulae":[
            {"name":"lib","full_name":"someone/tap/lib","dependencies":[],
             "installed":[{"version":"1","installed_on_request":true}]}
        ],"casks":[]}"#;
        let host = plan_host(&layout, json);

        let scan = uninstall_plan(&layout, &host, "someone/tap/lib").unwrap();

        let row = &scan.groups[0].items[0];
        assert_eq!(row.path, Path::new("lib 1"));
        assert_eq!(row.brew, Some(lib_uninstall_task(&layout, &cellar)));
    }

    #[test]
    fn the_list_carries_an_uninstall_task_on_formula_rows_when_it_knows_brew() {
        let brew = PathBuf::from("/opt/homebrew/bin/brew");
        let inventory = Inventory {
            packages: packages(),
            brew: Some(brew.clone()),
            ..Inventory::default()
        };

        let scan = list_scan(&inventory);

        let formulae: Vec<&Package> = inventory
            .packages
            .iter()
            .filter(|package| package.kind == PackageKind::Formula)
            .collect();
        let tasks: Vec<_> = scan.groups[0]
            .items
            .iter()
            .map(|i| i.brew.clone())
            .collect();
        let expected: Vec<_> = formulae
            .iter()
            .map(|package| {
                Some(BrewTask::Uninstall {
                    name: package.name.clone(),
                    brew: brew.clone(),
                    cellar: package.path.clone(),
                })
            })
            .collect();
        assert_eq!(tasks, expected);
        assert!(scan.groups[1].items.iter().all(|item| item.brew.is_none()));
        assert!(
            scan.groups
                .iter()
                .flat_map(|group| &group.items)
                .all(|item| !item.selected)
        );
    }

    #[test]
    fn the_list_has_no_task_without_a_brew() {
        let scan = list_scan(&Inventory {
            packages: packages(),
            ..Inventory::default()
        });

        assert!(
            scan.groups
                .iter()
                .flat_map(|group| &group.items)
                .all(|item| item.brew.is_none())
        );
    }

    #[test]
    fn the_scan_remembers_the_brew_it_used_but_the_json_does_not_show_it() {
        let (_temp, layout) = with_brew("opt/homebrew");
        let host = FakeHost::default();
        host.reply(INFO_LINE_START, BREW_INFO);

        let inventory = scan(&layout, &host).unwrap();

        assert_eq!(inventory.brew, Some(brew_of(&layout)));
        let json = serde_json::to_value(&inventory).unwrap();
        assert!(json.get("brew").is_none(), "{json}");
        let without = scan(&sandbox().1, &FakeHost::default()).unwrap();
        assert_eq!(without.brew, None);
    }

    #[test]
    fn the_list_refuses_what_uninstall_plan_refuses() {
        let packages = refusal_packages();
        let inventory = Inventory {
            packages,
            ..Inventory::default()
        };

        let refusals = list_refusals(&inventory);

        let text = |label: &str| refusals.get(Path::new(label)).map(String::as_str);
        assert_eq!(
            text("ghostty 1"),
            Some("ghostty is a cask. Remove it with taert uninstall.")
        );
        assert_eq!(
            text("fd 1 · on request · pinned"),
            Some("fd is pinned. Run brew unpin fd first.")
        );
        assert_eq!(
            text("lib 1 · dependency · used by 2"),
            Some("lib stays: app, tool use it.")
        );
        assert_eq!(text("plain 1 · on request"), None);
        assert_eq!(refusals.len(), 3);
    }
}
