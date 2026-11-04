//! Commands taert runs on the Mac, behind a trait so tests never touch the real system.

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const SUDO_PROBE: Duration = Duration::from_secs(3);
const ADMIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
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
    fn uid(&self) -> u32;
    fn pid(&self) -> u32;
}

pub struct MacHost;

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
        } else if self.run("/usr/bin/sudo", &["-n", "true"], SUDO_PROBE).ok {
            let args = ["-n", "/bin/sh", "-c", script];
            self.run("/usr/bin/sudo", &args, timeout)
        } else {
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
            "The administrator password was not given".to_string()
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

    fn uid(&self) -> u32 {
        unsafe { libc::getuid() }
    }

    fn pid(&self) -> u32 {
        std::process::id()
    }
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
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    type Effect = Box<dyn Fn() + Send>;
    type RunEffect = Arc<dyn Fn() + Send + Sync>;

    /// Answers commands by the start of their line and records what ran.
    #[derive(Default)]
    pub struct FakeHost {
        pub calls: Mutex<Vec<String>>,
        pub replies: Mutex<Vec<(String, VecDeque<Output>)>>,
        pub admin_scripts: Mutex<Vec<String>>,
        pub admin_result: Mutex<Option<String>>,
        pub admin_stdout: Mutex<String>,
        pub admin_effect: Mutex<Option<Effect>>,
        pub run_effects: Mutex<Vec<(String, RunEffect)>>,
        pub signals: Mutex<Vec<(u32, i32)>>,
        pub signal_error: Mutex<Option<String>>,
        pub trashed: Mutex<Vec<PathBuf>>,
        pub trash_error: Mutex<Option<String>>,
        pub simulate_trash: Mutex<bool>,
        pub uid: Mutex<Option<u32>>,
        pub pid: Mutex<Option<u32>>,
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

        pub fn fail_trash(&self, error: &str) {
            *self.trash_error.lock().unwrap() = Some(error.to_string());
        }

        pub fn trashed(&self) -> Vec<PathBuf> {
            self.trashed.lock().unwrap().clone()
        }

        pub fn simulate_trash(&self) {
            *self.simulate_trash.lock().unwrap() = true;
        }

        pub fn set_uid(&self, uid: u32) {
            *self.uid.lock().unwrap() = Some(uid);
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

        fn run_admin(&self, script: &str, _prompt: &str) -> Result<(), String> {
            self.admin_scripts.lock().unwrap().push(script.to_string());
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

        fn uid(&self) -> u32 {
            self.uid.lock().unwrap().unwrap_or(501)
        }

        fn pid(&self) -> u32 {
            self.pid.lock().unwrap().unwrap_or(u32::MAX)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
