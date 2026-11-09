//! Commands taert runs on the Mac, behind a trait so tests never touch the real system.

use crate::core::admin::{self, NOT_GIVEN};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_foundation::{
    NSFileManager, NSNumber, NSString, NSURL, NSURLIsUbiquitousItemKey, NSURLResourceKey,
    NSURLUbiquitousItemDownloadingStatusCurrent, NSURLUbiquitousItemDownloadingStatusDownloaded,
    NSURLUbiquitousItemDownloadingStatusKey, NSURLUbiquitousItemHasUnresolvedConflictsKey,
    NSURLUbiquitousItemIsUploadedKey, NSURLUbiquitousItemIsUploadingKey,
};
use std::io::{IsTerminal, Read};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const ADMIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// What iCloud says about one file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ICloudState {
    pub ubiquitous: bool,
    pub downloaded: bool,
    pub uploaded: bool,
    pub uploading: bool,
    pub conflicts: bool,
}

impl ICloudState {
    pub const EVICTABLE: Self = Self {
        ubiquitous: true,
        downloaded: true,
        uploaded: true,
        uploading: false,
        conflicts: false,
    };

    /// Why the local copy must stay, or None when iCloud can download it again.
    pub fn refusal(&self) -> Option<&'static str> {
        if !self.ubiquitous {
            Some("iCloud does not keep this file")
        } else if !self.downloaded {
            Some("the file is only in iCloud")
        } else if self.uploading {
            Some("iCloud is still uploading the file")
        } else if !self.uploaded {
            Some("iCloud has not uploaded the file yet")
        } else if self.conflicts {
            Some("the file has unresolved conflicts")
        } else {
            None
        }
    }
}

pub trait Host: Send + Sync {
    fn run(&self, program: &str, args: &[&str], timeout: Duration) -> Output;
    /// Runs a shell script as root, asking for a password at most once.
    fn run_admin(&self, script: &str, prompt: &str) -> Result<(), String>;
    /// Like `run_admin`, and returns what the script printed. The output comes through a pipe,
    /// so root never writes to a path the user controls.
    fn run_admin_output(
        &self,
        script: &str,
        prompt: &str,
        timeout: Duration,
    ) -> Result<String, String>;
    /// Sends the signal as asked. Only `signal` calls it.
    fn raw_signal(&self, pid: u32, signal: i32) -> Result<(), String>;
    /// Sends only to a pid `signal_pid` accepts.
    fn signal(&self, pid: u32, signal: i32) -> Result<(), String> {
        signal_pid(pid)?;
        self.raw_signal(pid, signal)
    }
    fn pause(&self, duration: Duration);
    /// Moves a file or folder to the system Trash.
    fn trash(&self, path: &std::path::Path) -> Result<(), String>;
    /// Reads what iCloud says about a file.
    fn icloud_state(&self, path: &std::path::Path) -> Result<ICloudState, String>;
    /// Asks iCloud to remove the local copy of a file. The file stays in iCloud.
    fn evict(&self, path: &std::path::Path) -> Result<(), String>;
    fn uid(&self) -> u32;
    fn pid(&self) -> u32;
    /// Whether stdin and stderr are a terminal, so sudo can ask for a password there.
    fn has_tty(&self) -> bool;
    /// Whether the user is in the `admin` group, the only one the default sudoers lets use sudo.
    /// True when it cannot tell.
    fn is_admin(&self) -> bool;
    /// Asks for the password with `sudo -v` on the terminal. True when sudo accepted it.
    fn sudo_validate(&self, reason: &str) -> bool;
}

pub struct MacHost;

fn groups_have_admin(text: &str) -> bool {
    text.split_whitespace().any(|group| group == "admin")
}

fn admin_from(output: &Output) -> bool {
    !output.ok || groups_have_admin(&output.stdout)
}

fn read_pipe(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

impl Host for MacHost {
    fn run(&self, program: &str, args: &[&str], timeout: Duration) -> Output {
        let spawned = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                return Output {
                    ok: false,
                    stdout: String::new(),
                    stderr: error.to_string(),
                };
            }
        };

        // A full pipe would stall the command, so read while waiting for it.
        let stdout = read_pipe(child.stdout.take());
        let stderr = read_pipe(child.stderr.take());

        let started = Instant::now();
        let ok = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.success(),
                Ok(None) if started.elapsed() < timeout => thread::sleep(POLL_INTERVAL),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
            }
        };

        Output {
            ok,
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
        }
    }

    fn run_admin(&self, script: &str, prompt: &str) -> Result<(), String> {
        self.run_admin_output(script, prompt, ADMIN_TIMEOUT)
            .map(drop)
    }

    fn run_admin_output(
        &self,
        script: &str,
        prompt: &str,
        timeout: Duration,
    ) -> Result<String, String> {
        let output = if unsafe { libc::geteuid() } == 0 {
            self.run("/bin/sh", &["-c", script], timeout)
        } else if admin::probe(self) {
            let args = ["-n", "/bin/sh", "-c", script];
            self.run("/usr/bin/sudo", &args, timeout)
        } else {
            if admin::session().is_refused() {
                return Err(NOT_GIVEN.to_string());
            }
            // The dialog works over the full-screen TUI, where sudo has no prompt to show.
            let args = [
                "-e",
                "on run argv",
                "-e",
                "do shell script (item 1 of argv) with prompt (item 2 of argv) with administrator privileges",
                "-e",
                "end run",
                "--",
                script,
                prompt,
            ];
            self.run("/usr/bin/osascript", &args, timeout)
        };

        if output.ok {
            return Ok(output.stdout);
        }
        Err(if output.stderr.contains("-128") {
            NOT_GIVEN.to_string()
        } else {
            output.stderr.trim().to_string()
        })
    }

    fn raw_signal(&self, pid: u32, signal: i32) -> Result<(), String> {
        let pid = signal_pid(pid)?;
        if unsafe { libc::kill(pid, signal) } == -1 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    fn pause(&self, duration: Duration) {
        thread::sleep(duration);
    }

    fn trash(&self, path: &std::path::Path) -> Result<(), String> {
        let mut ctx = trash::TrashContext::default();
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        ctx.set_delete_method(DeleteMethod::NsFileManager);
        ctx.delete(path).map_err(|e| e.to_string())
    }

    fn icloud_state(&self, path: &std::path::Path) -> Result<ICloudState, String> {
        let url = file_url(path)?;
        autoreleasepool(|_| {
            // SAFETY: reads Foundation's key constants
            let keys = unsafe {
                (
                    NSURLIsUbiquitousItemKey,
                    NSURLUbiquitousItemIsUploadedKey,
                    NSURLUbiquitousItemIsUploadingKey,
                    NSURLUbiquitousItemHasUnresolvedConflictsKey,
                    NSURLUbiquitousItemDownloadingStatusKey,
                )
            };
            let ubiquitous = flag(&url, keys.0)?;
            let uploaded = flag(&url, keys.1)?;
            let uploading = flag(&url, keys.2)?;
            let conflicts = flag(&url, keys.3)?;
            let status = resource(&url, keys.4)?;
            let downloaded = status
                .and_then(|value| value.downcast::<NSString>().ok())
                .is_some_and(|status| {
                    // SAFETY: the statuses are Foundation's own constants.
                    unsafe {
                        status.isEqualToString(NSURLUbiquitousItemDownloadingStatusCurrent)
                            || status
                                .isEqualToString(NSURLUbiquitousItemDownloadingStatusDownloaded)
                    }
                });
            Ok(ICloudState {
                ubiquitous,
                downloaded,
                uploaded,
                uploading,
                conflicts,
            })
        })
    }

    fn evict(&self, path: &std::path::Path) -> Result<(), String> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !meta.file_type().is_file() {
            return Err("only a file's local copy can be removed".to_string());
        }
        let url = file_url(path)?;
        autoreleasepool(|_| {
            NSFileManager::defaultManager()
                .evictUbiquitousItemAtURL_error(&url)
                .map_err(|e| e.to_string())
        })
    }

    fn uid(&self) -> u32 {
        unsafe { libc::getuid() }
    }

    fn pid(&self) -> u32 {
        std::process::id()
    }

    fn has_tty(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    fn is_admin(&self) -> bool {
        admin_from(&self.run("/usr/bin/id", &["-Gn"], Duration::from_secs(3)))
    }

    fn sudo_validate(&self, reason: &str) -> bool {
        use std::os::unix::process::CommandExt;
        eprintln!("taert needs administrator rights to {reason}.");
        let mut command = Command::new("/usr/bin/sudo");
        command
            .args(["-v", "-p", "Password for %u: "])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        // Ctrl-C at the prompt must stop sudo only, so taert ignores it meanwhile.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
        let previous = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
        let accepted = command.status().is_ok_and(|status| status.success());
        unsafe {
            libc::signal(libc::SIGINT, previous);
        }
        accepted
    }
}

fn file_url(path: &std::path::Path) -> Result<Retained<NSURL>, String> {
    NSURL::from_file_path(path).ok_or_else(|| format!("not a file path: {}", path.display()))
}

fn resource(url: &NSURL, key: &NSURLResourceKey) -> Result<Option<Retained<AnyObject>>, String> {
    let mut value = None;
    // SAFETY: an untyped object pointer fits every resource key
    unsafe { url.getResourceValue_forKey_error(&mut value, key) }.map_err(|e| e.to_string())?;
    Ok(value)
}

fn flag(url: &NSURL, key: &NSURLResourceKey) -> Result<bool, String> {
    Ok(resource(url, key)?
        .and_then(|value| value.downcast::<NSNumber>().ok())
        .is_some_and(|number| number.boolValue()))
}

/// The pid as `kill` takes it, or an error for pid 0 (a whole process group), pid 1 (launchd),
/// and a pid too large for `pid_t`, which would turn negative and reach a group or every process.
pub fn signal_pid(pid: u32) -> Result<libc::pid_t, String> {
    match libc::pid_t::try_from(pid) {
        Ok(target) if target > 1 => Ok(target),
        _ => Err(format!("Refusing to signal pid {pid}")),
    }
}

/// Quotes a string for /bin/sh.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use crate::core::admin::NOT_GIVEN;
    use std::collections::{HashMap, VecDeque};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// The uid of the user running the tests.
    pub fn real_uid() -> u32 {
        unsafe { libc::getuid() }
    }

    type Effect = Box<dyn Fn() + Send>;
    type RunEffect = Arc<dyn Fn() + Send + Sync>;

    /// Answers commands by the start of their line and records what ran.
    #[derive(Default)]
    pub struct FakeHost {
        pub calls: Mutex<Vec<String>>,
        pub replies: Mutex<Vec<(String, VecDeque<Output>)>>,
        pub admin_scripts: Mutex<Vec<String>>,
        pub admin_prompts: Mutex<Vec<String>>,
        pub admin_result: Mutex<Option<String>>,
        pub admin_stdout: Mutex<String>,
        pub admin_effect: Mutex<Option<Effect>>,
        pub run_effects: Mutex<Vec<(String, RunEffect)>>,
        pub signals: Mutex<Vec<(u32, i32)>>,
        pub signal_error: Mutex<Option<String>>,
        pub trashed: Mutex<Vec<PathBuf>>,
        pub trash_error: Mutex<Option<String>>,
        icloud: Mutex<HashMap<PathBuf, ICloudState>>,
        icloud_error: Mutex<Option<String>>,
        evicted: Mutex<Vec<PathBuf>>,
        evict_errors: Mutex<HashMap<PathBuf, String>>,
        pub simulate_trash: Mutex<bool>,
        pub uid: Mutex<Option<u32>>,
        pub pid: Mutex<Option<u32>>,
        pub tty: Mutex<bool>,
        pub sudo_refused: Mutex<bool>,
        pub not_admin: Mutex<bool>,
        /// Set when `sudo_validate` returned false; root scripts are refused after it.
        password_refused: Mutex<bool>,
    }

    impl FakeHost {
        /// A Mac where `ps` works, only the system runs, and no file is open.
        pub fn idle() -> Self {
            let host = Self::default();
            host.reply("/bin/ps", "    1     0 /sbin/launchd\n");
            host.reply("/usr/sbin/lsof", "");
            host
        }

        /// Later calls keep getting the last reply given for a command.
        pub fn reply(&self, command: &str, stdout: &str) {
            self.push(
                command,
                Output {
                    ok: true,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                },
            );
        }

        pub fn fail(&self, command: &str, stderr: &str) {
            self.push(
                command,
                Output {
                    ok: false,
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                },
            );
        }

        /// A command that prints `stdout` and still reports failure.
        pub fn fail_after_printing(&self, command: &str, stdout: &str) {
            self.push(
                command,
                Output {
                    ok: false,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                },
            );
        }

        fn push(&self, command: &str, output: Output) {
            let mut replies = self.replies.lock().unwrap();
            match replies.iter_mut().find(|(known, _)| known == command) {
                Some((_, queue)) => queue.push_back(output),
                None => replies.push((command.to_string(), VecDeque::from([output]))),
            }
        }

        /// Runs `effect` when a command starts, like the real tool would.
        pub fn when(&self, command: &str, effect: impl Fn() + Send + Sync + 'static) {
            let entry = (command.to_string(), Arc::new(effect) as RunEffect);
            self.run_effects.lock().unwrap().push(entry);
        }

        pub fn ran(&self, command: &str) -> bool {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.starts_with(command))
        }

        pub fn position(&self, command: &str) -> Option<usize> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .position(|line| line.starts_with(command))
        }

        pub fn count(&self, command: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.starts_with(command))
                .count()
        }

        /// How many times `sudo -v` asked for the password.
        pub fn sudo_asks(&self) -> usize {
            self.count("sudo -v")
        }

        pub fn fail_trash(&self, error: &str) {
            *self.trash_error.lock().unwrap() = Some(error.to_string());
        }

        pub fn trashed(&self) -> Vec<PathBuf> {
            self.trashed.lock().unwrap().clone()
        }

        pub fn set_icloud(&self, path: &Path, state: ICloudState) {
            self.icloud
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), state);
        }

        pub fn fail_icloud(&self, error: &str) {
            *self.icloud_error.lock().unwrap() = Some(error.to_string());
        }

        pub fn fail_evict(&self, path: &Path, error: &str) {
            let mut errors = self.evict_errors.lock().unwrap();
            errors.insert(path.to_path_buf(), error.to_string());
        }

        pub fn evicted(&self) -> Vec<PathBuf> {
            self.evicted.lock().unwrap().clone()
        }

        pub fn simulate_trash(&self) {
            *self.simulate_trash.lock().unwrap() = true;
        }

        pub fn set_uid(&self, uid: u32) {
            *self.uid.lock().unwrap() = Some(uid);
        }

        /// The fake host with the uid of the user running the tests, for tests whose files must pass an owner check.
        pub fn as_real_user(self) -> Self {
            self.set_uid(real_uid());
            self
        }

        pub fn set_pid(&self, pid: u32) {
            *self.pid.lock().unwrap() = Some(pid);
        }
    }

    impl Host for FakeHost {
        fn run(&self, program: &str, args: &[&str], _timeout: Duration) -> Output {
            let line = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.lock().unwrap().push(line.clone());
            let effects: Vec<_> = self
                .run_effects
                .lock()
                .unwrap()
                .iter()
                .filter(|(command, _)| line.starts_with(command.as_str()))
                .map(|(_, effect)| effect.clone())
                .collect();
            for effect in effects {
                effect();
            }
            let mut replies = self.replies.lock().unwrap();
            let found = replies
                .iter_mut()
                .find(|(known, _)| line.starts_with(known.as_str()));
            match found {
                Some((_, queue)) if queue.len() > 1 => queue.pop_front().unwrap_or_default(),
                Some((_, queue)) => queue.front().cloned().unwrap_or_default(),
                None => Output::default(),
            }
        }

        fn run_admin(&self, script: &str, prompt: &str) -> Result<(), String> {
            if *self.password_refused.lock().unwrap() {
                return Err(NOT_GIVEN.to_string());
            }
            self.admin_scripts.lock().unwrap().push(script.to_string());
            self.admin_prompts.lock().unwrap().push(prompt.to_string());
            if let Some(effect) = self.admin_effect.lock().unwrap().as_ref() {
                effect();
            }
            match self.admin_result.lock().unwrap().clone() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn run_admin_output(
            &self,
            script: &str,
            prompt: &str,
            _timeout: Duration,
        ) -> Result<String, String> {
            self.run_admin(script, prompt)?;
            Ok(self.admin_stdout.lock().unwrap().clone())
        }

        fn raw_signal(&self, pid: u32, signal: i32) -> Result<(), String> {
            self.signals.lock().unwrap().push((pid, signal));
            match self.signal_error.lock().unwrap().clone() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn pause(&self, _duration: Duration) {}

        fn trash(&self, path: &Path) -> Result<(), String> {
            self.trashed.lock().unwrap().push(path.to_path_buf());
            if let Some(error) = self.trash_error.lock().unwrap().clone() {
                return Err(error);
            }
            if *self.simulate_trash.lock().unwrap() {
                if path.is_dir() && !path.is_symlink() {
                    std::fs::remove_dir_all(path).map_err(|e| e.to_string())?;
                } else {
                    std::fs::remove_file(path).map_err(|e| e.to_string())?;
                }
            }
            Ok(())
        }

        fn icloud_state(&self, path: &Path) -> Result<ICloudState, String> {
            if let Some(error) = self.icloud_error.lock().unwrap().clone() {
                return Err(error);
            }
            let known = self.icloud.lock().unwrap().get(path).copied();
            Ok(known.unwrap_or_default())
        }

        fn evict(&self, path: &Path) -> Result<(), String> {
            self.evicted.lock().unwrap().push(path.to_path_buf());
            match self.evict_errors.lock().unwrap().get(path) {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }

        fn uid(&self) -> u32 {
            self.uid.lock().unwrap().unwrap_or(501)
        }

        fn pid(&self) -> u32 {
            self.pid.lock().unwrap().unwrap_or(u32::MAX)
        }

        fn has_tty(&self) -> bool {
            *self.tty.lock().unwrap()
        }

        fn is_admin(&self) -> bool {
            !*self.not_admin.lock().unwrap()
        }

        fn sudo_validate(&self, reason: &str) -> bool {
            self.calls.lock().unwrap().push(format!("sudo -v {reason}"));
            let accepted = !*self.sudo_refused.lock().unwrap();
            if !accepted {
                *self.password_refused.lock().unwrap() = true;
            }
            accepted
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_admin_group_is_found_by_whole_word() {
        assert!(groups_have_admin("staff everyone admin\n"));
        assert!(!groups_have_admin("staff everyone"));
        assert!(!groups_have_admin("administrators"));
    }

    #[test]
    fn a_failed_group_lookup_counts_as_admin() {
        let failed = Output {
            ok: false,
            ..Default::default()
        };
        let staff = Output {
            ok: true,
            stdout: "staff".to_string(),
            ..Default::default()
        };

        assert!(admin_from(&failed));
        assert!(!admin_from(&staff));
    }

    #[test]
    fn a_command_reports_what_it_printed() {
        let output = MacHost.run("/bin/echo", &["hello"], Duration::from_secs(5));

        assert!(output.ok);
        assert_eq!(output.stdout.trim(), "hello");
    }

    #[test]
    fn a_failing_command_is_not_ok_and_keeps_its_error() {
        let script = "echo broken >&2; exit 3";
        let output = MacHost.run("/bin/sh", &["-c", script], Duration::from_secs(5));

        assert!(!output.ok);
        assert_eq!(output.stderr.trim(), "broken");
    }

    #[test]
    fn a_missing_program_is_not_ok() {
        let output = MacHost.run("/nowhere/program", &[], Duration::from_secs(5));

        assert!(!output.ok);
        assert!(!output.stderr.is_empty());
    }

    #[test]
    fn a_command_past_its_time_is_stopped() {
        let started = Instant::now();
        let output = MacHost.run("/bin/sleep", &["30"], Duration::from_millis(100));

        assert!(!output.ok);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn output_larger_than_a_pipe_does_not_hang_a_command() {
        let script = "head -c 300000 /dev/zero | tr '\\0' 'x'";
        let output = MacHost.run("/bin/sh", &["-c", script], Duration::from_secs(10));

        assert!(output.ok);
        assert_eq!(output.stdout.len(), 300_000);
    }

    #[test]
    fn fake_host_reports_501_without_a_set_uid() {
        let host = fake::FakeHost::default();
        assert_eq!(host.uid(), 501);
        host.set_uid(777);
        assert_eq!(host.uid(), 777);
    }

    #[test]
    fn fake_host_has_no_real_pid() {
        let host = fake::FakeHost::default();
        assert_eq!(host.pid(), u32::MAX);
        host.set_pid(4100);
        assert_eq!(host.pid(), 4100);
    }

    #[test]
    fn quoting_survives_the_shell() {
        let tricky = "it's $HOME `id` \"x\" ; rm -rf *";
        let line = format!("printf %s {}", shell_quote(tricky));

        let output = MacHost.run("/bin/sh", &["-c", &line], Duration::from_secs(5));

        assert_eq!(output.stdout, tricky);
    }

    #[test]
    fn signal_to_a_missing_process_is_an_error() {
        let result = MacHost.signal(2_000_000_000, 0);

        assert!(result.unwrap_err().contains("No such process"));
    }

    #[test]
    fn only_a_pid_of_one_process_other_than_launchd_can_be_signalled() {
        assert_eq!(signal_pid(4242), Ok(4242));
        for pid in [0, 1, 1 << 31, u32::MAX] {
            assert!(signal_pid(pid).is_err(), "{pid}");
        }
    }

    #[test]
    fn pid_0_and_pid_1_are_never_signalled() {
        let host = fake::FakeHost::default();

        for pid in [0, 1] {
            assert!(host.signal(pid, libc::SIGTERM).is_err());
        }

        assert!(host.signals.lock().unwrap().is_empty());
    }

    #[test]
    fn icloud_state_allows_eviction_only_when_settled() {
        assert_eq!(ICloudState::EVICTABLE.refusal(), None);
        assert!(ICloudState::default().refusal().is_some());

        let e = ICloudState::EVICTABLE;
        let flipped = [
            ICloudState {
                ubiquitous: false,
                ..e
            },
            ICloudState {
                downloaded: false,
                ..e
            },
            ICloudState {
                uploaded: false,
                ..e
            },
            ICloudState {
                uploading: true,
                ..e
            },
            ICloudState {
                conflicts: true,
                ..e
            },
        ];
        for state in flipped {
            assert!(state.refusal().is_some(), "{state:?}");
        }
    }

    #[test]
    fn the_fake_host_knows_no_icloud_file_until_told() {
        let host = fake::FakeHost::default();
        let path = std::path::Path::new("/nowhere/a.txt");

        assert_eq!(host.icloud_state(path), Ok(ICloudState::default()));
        host.set_icloud(path, ICloudState::EVICTABLE);
        assert_eq!(host.icloud_state(path), Ok(ICloudState::EVICTABLE));
        host.fail_icloud("no answer");
        assert_eq!(host.icloud_state(path), Err("no answer".to_string()));
    }

    #[test]
    fn the_fake_host_records_evictions_and_fails_on_request() {
        let temp = tempfile::tempdir().unwrap();
        let (a, b) = (temp.path().join("a"), temp.path().join("b"));
        std::fs::write(&a, "one").unwrap();
        std::fs::write(&b, "two").unwrap();
        let host = fake::FakeHost::default();
        host.fail_evict(&b, "busy");

        assert_eq!(host.evict(&a), Ok(()));
        assert_eq!(host.evict(&b), Err("busy".to_string()));

        assert_eq!(host.evicted(), vec![a.clone(), b.clone()]);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "two");
    }

    #[test]
    fn mac_host_never_evicts_a_folder() {
        let temp = tempfile::tempdir().unwrap();

        let result = MacHost.evict(temp.path());

        assert_eq!(
            result,
            Err("only a file's local copy can be removed".to_string())
        );
        assert!(temp.path().is_dir());
    }

    #[test]
    fn mac_host_reads_a_temp_file_as_not_kept_in_icloud() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("a.txt");
        std::fs::write(&file, "text").unwrap();

        let state = MacHost.icloud_state(&file).unwrap();

        assert!(!state.ubiquitous);
        assert!(state.refusal().is_some());
    }
}
