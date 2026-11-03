//! The process table from `ps`, and the checks on it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::host::{Host, Output};

pub(crate) const PS: &str = "/bin/ps";
const TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn list(host: &dyn Host) -> Output {
    host.run(PS, &["-axo", "pid=,ppid=,comm="], TIMEOUT)
}

/// The process table, or `None` when `ps` failed and the list cannot be trusted.
pub(crate) fn read(host: &dyn Host) -> Option<Vec<(u32, u32, String)>> {
    let listing = list(host);
    listing.ok.then(|| parse(&listing.stdout))
}

pub(crate) fn parse(stdout: &str) -> Vec<(u32, u32, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.trim_start().splitn(2, char::is_whitespace);
            let pid = fields.next()?.parse().ok()?;
            let rest = fields.next()?.trim_start();
            let (parent, program) = rest.split_once(char::is_whitespace)?;
            Some((pid, parent.parse().ok()?, program.trim_start().to_string()))
        })
        .collect()
}

pub(crate) fn is_inside_any(program: &str, folders: &[PathBuf]) -> bool {
    folders
        .iter()
        .any(|folder| Path::new(program).starts_with(folder))
}

/// True when a program from the bundle runs. Letter case is ignored, and the
/// bundle's real path counts as well as the path given.
pub(crate) fn bundle_is_running(table: &[(u32, u32, String)], bundle: &Path) -> bool {
    let lower = |path: &Path| PathBuf::from(path.to_string_lossy().to_lowercase());
    let mut names = vec![lower(bundle)];
    if let Ok(canonical) = bundle.canonicalize() {
        names.push(lower(&canonical));
    }
    table.iter().any(|(_, _, comm)| {
        let program = lower(Path::new(comm));
        names.iter().any(|name| program.starts_with(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keeps_the_program_path_as_printed() {
        let table = "   42     1 /Applications/Two  Spaces.app/Contents/MacOS/x\n";

        assert_eq!(
            parse(table),
            vec![(
                42,
                1,
                "/Applications/Two  Spaces.app/Contents/MacOS/x".to_string()
            )]
        );
    }
}
