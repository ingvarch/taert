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
    fn signal(&self, pid: u32, signal: i32);
    fn pause(&self, duration: Duration);
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
        let output = if unsafe { libc::geteuid() } == 0 {
            self.run("/bin/sh", &["-c", script], ADMIN_TIMEOUT)
        } else if self.run("/usr/bin/sudo", &["-n", "true"], SUDO_PROBE).ok {
            let args = ["-n", "/bin/sh", "-c", script];
            self.run("/usr/bin/sudo", &args, ADMIN_TIMEOUT)
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
            self.run("/usr/bin/osascript", &args, ADMIN_TIMEOUT)
        };

        if output.ok {
            return Ok(());
        }
        Err(if output.stderr.contains("-128") {
            "The administrator password was not given".to_string()
        } else {
            output.stderr.trim().to_string()
        })
    }

    fn signal(&self, pid: u32, signal: i32) {
        unsafe {
            libc::kill(pid as libc::pid_t, signal);
        }
    }

    fn pause(&self, duration: Duration) {
        thread::sleep(duration);
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
    use std::sync::Mutex;

    type Effect = Box<dyn Fn() + Send>;

    /// Answers commands by the start of their line and records what ran.
    #[derive(Default)]
    pub struct FakeHost {
        pub calls: Mutex<Vec<String>>,
        pub replies: Mutex<Vec<(String, VecDeque<Output>)>>,
        pub admin_scripts: Mutex<Vec<String>>,
        pub admin_result: Mutex<Option<String>>,
        pub admin_effect: Mutex<Option<Effect>>,
        pub run_effects: Mutex<Vec<(String, Effect)>>,
        pub signals: Mutex<Vec<(u32, i32)>>,
    }

    impl FakeHost {
        /// A Mac where `ps` works and only the system runs.
        pub fn idle() -> Self {
            let host = Self::default();
            host.reply("/bin/ps", "    1     0 /sbin/launchd\n");
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

        fn push(&self, command: &str, output: Output) {
            let mut replies = self.replies.lock().unwrap();
            match replies.iter_mut().find(|(known, _)| known == command) {
                Some((_, queue)) => queue.push_back(output),
                None => replies.push((command.to_string(), VecDeque::from([output]))),
            }
        }

        /// Runs `effect` when a command starts, like the real tool would.
        pub fn when(&self, command: &str, effect: impl Fn() + Send + 'static) {
            let entry = (
                command.to_string(),
                Box::new(effect) as Box<dyn Fn() + Send>,
            );
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
    }

    impl Host for FakeHost {
        fn run(&self, program: &str, args: &[&str], _timeout: Duration) -> Output {
            let line = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.lock().unwrap().push(line.clone());
            for (command, effect) in self.run_effects.lock().unwrap().iter() {
                if line.starts_with(command.as_str()) {
                    effect();
                }
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

        fn signal(&self, pid: u32, signal: i32) {
            self.signals.lock().unwrap().push((pid, signal));
        }

        fn pause(&self, _duration: Duration) {}
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
    fn quoting_survives_the_shell() {
        let tricky = "it's $HOME `id` \"x\" ; rm -rf *";
        let line = format!("printf %s {}", shell_quote(tricky));

        let output = MacHost.run("/bin/sh", &["-c", &line], Duration::from_secs(5));

        assert_eq!(output.stdout, tricky);
    }
}
