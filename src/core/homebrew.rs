//! Lists the Homebrew packages and the space they take. Reads only.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::bytes::format_bytes;
use super::host::{Host, Output};
use super::layout::Layout;
use super::scanner::{CategoryScan, ScanItem, ToolGroup};
use super::sizer::DirSizer;

pub const SCAN_ID: &str = "brew";
const INFO_TIMEOUT: Duration = Duration::from_secs(60);
const GROUP_FORMULAE: &str = "Formulae";
const GROUP_CASKS: &str = "Casks · Remove with taert uninstall";
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

#[derive(Debug, Clone, Default, Serialize)]
pub struct Inventory {
    pub packages: Vec<Package>,
    pub notes: Vec<String>,
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
pub fn run_brew(host: &dyn Host, brew: &Path, args: &[&str], timeout: Duration) -> Output {
    let brew = brew.display().to_string();
    let mut command = vec![
        "HOMEBREW_NO_AUTO_UPDATE=1",
        "HOMEBREW_NO_ENV_HINTS=1",
        "NONINTERACTIVE=1",
        brew.as_str(),
    ];
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
        path: prefix.join("Cellar").join(&raw.name),
    }
}

fn cask_package(raw: &RawCask, prefix: &Path) -> Package {
    Package {
        kind: PackageKind::Cask,
        name: raw.token.clone(),
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

/// Lists what Homebrew has installed, with the size of each folder.
pub fn scan(layout: &Layout, host: &dyn Host) -> Result<Inventory, String> {
    let Some(brew) = find(layout) else {
        return Ok(Inventory {
            packages: Vec::new(),
            notes: vec![NOT_INSTALLED.to_string()],
        });
    };

    let output = run_brew(
        host,
        &brew.path,
        &["info", "--json=v2", "--installed"],
        INFO_TIMEOUT,
    );
    if !output.ok {
        return Err(format!(
            "Homebrew did not list its packages: {}",
            output.stderr.trim()
        ));
    }

    let mut packages = parse_packages(&output.stdout, &brew.prefix)?;
    for package in &mut packages {
        package.size_bytes = DirSizer::size_of(&package.path);
    }
    packages.sort_by(|a, b| {
        (a.kind, Reverse(a.size_bytes), &a.name).cmp(&(b.kind, Reverse(b.size_bytes), &b.name))
    });
    Ok(Inventory {
        packages,
        notes: Vec::new(),
    })
}

fn label(package: &Package) -> String {
    let mut label = format!("{} {}", package.name, package.version);
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

fn row(package: &Package) -> ScanItem {
    ScanItem {
        path: PathBuf::from(label(package)),
        size_bytes: package.size_bytes,
        modified: None,
        selected: false,
        contents_only: false,
        identity: None,
        docker: None,
        role: None,
        also: Vec::new(),
    }
}

/// The packages as a list to browse. Nothing in it can be removed.
pub fn list_scan(inventory: &Inventory) -> CategoryScan {
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

    /// A package on request that nothing uses.
    pub fn package(kind: PackageKind, name: &str, version: &str, bytes: u64) -> Package {
        Package {
            kind,
            name: name.to_string(),
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
    use super::testing::{package, with_brew};
    use super::*;
    use crate::core::apps::testing::sandbox;
    use crate::core::host::fake::FakeHost;
    use crate::core::sizer::DirSizer;
    use std::fs;

    // From brew info --json=v2 --installed on a Mac with Homebrew 7.0.12, cut down to 10 formulae and 2 casks. "pinned" was set to true on fd by hand, because no formula there was pinned.
    const BREW_INFO: &str = include_str!("../../tests/fixtures/brew_info.json");

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

    #[test]
    fn plain_text_leaves_out_an_empty_section() {
        let inventory = Inventory {
            packages: vec![package(PackageKind::Cask, "ghostty", "1.3.1", 0)],
            notes: Vec::new(),
        };

        let text = plain_text(&inventory);

        assert!(text.starts_with("Casks (remove with taert uninstall)\n"));
        assert!(!text.contains("Formulae"));
        assert_eq!(plain_text(&Inventory::default()), "");
    }
}
