//! One sudo session per run: ask for the password once, keep the ticket alive, end it on exit.

use crate::core::compress::CompressReport;
use crate::core::host::{Host, MacHost, shell_quote};
use crate::core::layout::Layout;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const SUDO_TIMEOUT: Duration = Duration::from_secs(3);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const HELPER_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const COMPRESS_PROMPT: &str = "taert wants to compress an app";
const UNDO_PROMPT: &str = "taert wants to undo the compression of an app";

pub const NOT_GIVEN: &str = "The administrator password was not given";

pub struct AdminSession {
    keep_alive_every: Duration,
    keep_alive_host: Arc<dyn Host>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    refused: bool,
    asked: bool,
    ticket: Option<Ticket>,
}

struct Ticket {
    started_by_taert: bool,
    stop: Sender<()>,
    keep_alive: JoinHandle<()>,
}

static SESSION: LazyLock<AdminSession> =
    LazyLock::new(|| AdminSession::new(Duration::from_secs(60), Arc::new(MacHost)));

pub fn session() -> &'static AdminSession {
    &SESSION
}

/// A session of its own for a test, so none shares the real one.
#[cfg(test)]
pub fn test_session() -> &'static AdminSession {
    let host = Arc::new(crate::core::host::fake::FakeHost::default());
    Box::leak(Box::new(AdminSession::new(Duration::from_secs(60), host)))
}

impl AdminSession {
    pub fn new(keep_alive_every: Duration, keep_alive_host: Arc<dyn Host>) -> AdminSession {
        AdminSession {
            keep_alive_every,
            keep_alive_host,
            state: Mutex::new(State::default()),
        }
    }

    pub fn prepare(&self, host: &dyn Host, reason: &str) -> Result<(), String> {
        let mut state = self.lock();
        if state.refused {
            return Err(NOT_GIVEN.to_string());
        }
        if state.ticket.is_some() {
            return Ok(());
        }
        if probe(host) {
            state.ticket = Some(self.start_keep_alive(false));
            return Ok(());
        }
        if !host.has_tty() {
            return Ok(());
        }
        if host.sudo_validate(reason) {
            state.asked = true;
            state.ticket = Some(self.start_keep_alive(true));
            Ok(())
        } else {
            state.refused = true;
            Err(NOT_GIVEN.to_string())
        }
    }

    pub fn will_prompt(&self, host: &dyn Host) -> bool {
        let state = self.lock();
        !state.refused && state.ticket.is_none() && !probe(host) && host.has_tty()
    }

    pub fn is_refused(&self) -> bool {
        self.lock().refused
    }

    pub fn asked(&self) -> bool {
        self.lock().asked
    }

    pub fn end(&self, host: &dyn Host) {
        let Some(ticket) = self.lock().ticket.take() else {
            return;
        };
        drop(ticket.stop);
        let _ = ticket.keep_alive.join();
        if ticket.started_by_taert {
            host.run("/usr/bin/sudo", &["-k"], SUDO_TIMEOUT);
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn start_keep_alive(&self, started_by_taert: bool) -> Ticket {
        let (stop, stopped) = mpsc::channel::<()>();
        let (every, host) = (self.keep_alive_every, self.keep_alive_host.clone());
        let keep_alive = thread::spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(every) {
                host.run("/usr/bin/sudo", &["-n", "-v"], KEEP_ALIVE_TIMEOUT);
            }
        });
        Ticket {
            started_by_taert,
            stop,
            keep_alive,
        }
    }
}

/// Whether sudo runs without asking: the terminal has a ticket, or taert runs as root.
pub(crate) fn probe(host: &dyn Host) -> bool {
    host.run("/usr/bin/sudo", &["-n", "true"], SUDO_TIMEOUT).ok
}

/// Compresses or unpacks one root-owned bundle in taert run as root.
pub fn run_compress_helper(
    host: &dyn Host,
    layout: &Layout,
    bundle: &Path,
    undo: bool,
) -> Result<CompressReport, String> {
    let exe = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .map_err(|e| format!("Could not find the taert program: {e}"))?;
    let script = format!(
        "PATH=/usr/bin:/bin:/usr/sbin:/sbin {} admin-helper compress{} --home {} {}",
        quoted(&exe)?,
        if undo { " --undo" } else { "" },
        quoted(&layout.home)?,
        quoted(bundle)?,
    );
    let prompt = if undo { UNDO_PROMPT } else { COMPRESS_PROMPT };
    let answer = host.run_admin_output(&script, prompt, HELPER_TIMEOUT)?;
    parse_helper_answer(&answer)
}

/// The path quoted for the script. A lossy copy would name another file.
fn quoted(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(shell_quote)
        .ok_or_else(|| "the path is not valid UTF-8".to_string())
}

fn parse_helper_answer(answer: &str) -> Result<CompressReport, String> {
    let unreadable = || format!("The admin helper gave an answer taert cannot read: {answer}");
    let value: serde_json::Value = serde_json::from_str(answer).map_err(|_| unreadable())?;
    if let Some(error) = value.get("error").and_then(|error| error.as_str()) {
        return Err(error.to_string());
    }
    serde_json::from_value(value).map_err(|_| unreadable())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::host::fake::FakeHost;
    use std::time::Instant;

    const PROBE: &str = "/usr/bin/sudo -n true";
    const KEEP_ALIVE: &str = "/usr/bin/sudo -n -v";
    const KILL: &str = "/usr/bin/sudo -k";

    fn fixture(tty: bool) -> (Arc<FakeHost>, AdminSession) {
        let host = Arc::new(FakeHost::default());
        *host.tty.lock().unwrap() = tty;
        let session = AdminSession::new(Duration::from_millis(10), host.clone());
        (host, session)
    }

    fn wait_until(condition: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn a_ticket_the_terminal_has_is_used_and_kept() {
        let (host, session) = fixture(true);
        host.reply(PROBE, "");

        assert_eq!(session.prepare(&*host, "remove files"), Ok(()));
        session.end(&*host);

        assert_eq!(host.sudo_asks(), 0);
        assert_eq!(host.count(KILL), 0);
        assert!(!session.asked());
    }

    #[test]
    fn without_a_ticket_it_asks_once_and_ends_it_on_exit() {
        let (host, session) = fixture(true);
        host.fail(PROBE, "");

        assert_eq!(session.prepare(&*host, "remove files"), Ok(()));
        assert_eq!(session.prepare(&*host, "remove files"), Ok(()));
        assert_eq!(host.count(PROBE), 1);
        assert!(session.asked());
        session.end(&*host);

        assert!(host.ran("sudo -v remove files"));
        assert_eq!(host.sudo_asks(), 1);
        assert_eq!(host.count(KILL), 1);
    }

    #[test]
    fn a_refusal_is_remembered() {
        let (host, session) = fixture(true);
        host.fail(PROBE, "");
        *host.sudo_refused.lock().unwrap() = true;

        assert_eq!(
            session.prepare(&*host, "remove files"),
            Err(NOT_GIVEN.to_string())
        );
        assert_eq!(
            session.prepare(&*host, "remove files"),
            Err(NOT_GIVEN.to_string())
        );
        assert_eq!(host.count(PROBE), 1);

        assert!(session.is_refused());
        assert!(!session.asked());
        assert_eq!(host.sudo_asks(), 1);
        session.end(&*host);
        assert_eq!(host.count(KILL), 0);
    }

    #[test]
    fn without_a_terminal_it_asks_nothing() {
        let (host, session) = fixture(false);
        host.fail(PROBE, "");

        assert_eq!(session.prepare(&*host, "remove files"), Ok(()));
        session.end(&*host);

        assert_eq!(host.sudo_asks(), 0);
        assert_eq!(host.count(KILL), 0);
    }

    #[test]
    fn will_prompt_only_when_sudo_would_ask() {
        let (host, session) = fixture(true);
        host.fail(PROBE, "");
        assert!(session.will_prompt(&*host));
        session.prepare(&*host, "x").unwrap();
        assert!(!session.will_prompt(&*host));

        let (host, session) = fixture(true);
        host.reply(PROBE, "");
        assert!(!session.will_prompt(&*host));

        let (host, session) = fixture(false);
        host.fail(PROBE, "");
        assert!(!session.will_prompt(&*host));

        let (host, session) = fixture(true);
        host.fail(PROBE, "");
        *host.sudo_refused.lock().unwrap() = true;
        let _ = session.prepare(&*host, "x");
        assert!(!session.will_prompt(&*host));
    }

    #[test]
    fn the_keep_alive_runs_while_the_ticket_is_held_and_stops_at_the_end() {
        let (host, session) = fixture(true);
        host.fail(PROBE, "");
        session.prepare(&*host, "x").unwrap();

        assert!(wait_until(|| host.count(KEEP_ALIVE) >= 2));
        session.end(&*host);

        let after_end = host.count(KEEP_ALIVE);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(host.count(KEEP_ALIVE), after_end);
    }

    #[test]
    fn a_ticket_from_the_terminal_is_kept_alive_too() {
        let (host, session) = fixture(true);
        host.reply(PROBE, "");
        session.prepare(&*host, "x").unwrap();

        assert!(wait_until(|| host.count(KEEP_ALIVE) >= 1));
        session.end(&*host);
    }

    #[test]
    fn end_runs_sudo_k_once_and_does_nothing_without_a_ticket() {
        let (host, session) = fixture(true);
        session.end(&*host);
        assert!(host.calls.lock().unwrap().is_empty());

        host.fail(PROBE, "");
        session.prepare(&*host, "x").unwrap();
        session.end(&*host);
        session.end(&*host);

        assert_eq!(host.count(KILL), 1);
    }

    #[test]
    fn the_fake_host_refuses_root_scripts_after_a_refused_password() {
        let host = FakeHost::default();
        *host.sudo_refused.lock().unwrap() = true;

        assert!(!host.sudo_validate("remove files"));

        assert_eq!(host.run_admin("rm x", "p"), Err(NOT_GIVEN.to_string()));
        assert_eq!(
            host.run_admin_output("rm x", "p", Duration::from_secs(1)),
            Err(NOT_GIVEN.to_string())
        );
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }

    fn helper_layout() -> Layout {
        Layout {
            home: "/Users/it's me".into(),
            root: "/".into(),
            user_cache: None,
        }
    }

    const REPORT: &str = r#"{"scanned_files":3,"processed_files":2,"original_bytes":100,"compressed_bytes":40,"saved_bytes":60,"original_blocks":0,"compressed_blocks":0,"saved_blocks":0,"skipped":{"already_compressed":0,"symlink_or_hardlink":0,"cloned":0,"cloud_or_dataless":0,"has_xattr":0,"not_user_owned":0,"incompressible":0,"not_compressed":0,"locked":0,"open_for_writing":0},"failures":[],"dry_run":false,"undo":false}"#;

    #[test]
    fn the_helper_script_sets_its_own_path_and_quotes_every_part() {
        let host = FakeHost::default();
        *host.admin_stdout.lock().unwrap() = REPORT.to_string();
        let bundle = Path::new("/Applications/My \"quoted\" it's.app");

        run_compress_helper(&host, &helper_layout(), bundle, false).unwrap();
        run_compress_helper(&host, &helper_layout(), bundle, true).unwrap();

        let scripts = host.admin_scripts.lock().unwrap();
        let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
        let start = format!(
            "PATH=/usr/bin:/bin:/usr/sbin:/sbin {} admin-helper compress",
            shell_quote(&exe.to_string_lossy())
        );
        let rest = r#" --home '/Users/it'\''s me' '/Applications/My "quoted" it'\''s.app'"#;
        assert_eq!(scripts[0], format!("{start}{rest}"));
        assert_eq!(scripts[1], format!("{start} --undo{rest}"));
    }

    #[test]
    fn the_prompt_says_whether_the_helper_compresses_or_undoes() {
        let host = FakeHost::default();
        *host.admin_stdout.lock().unwrap() = REPORT.to_string();
        let bundle = Path::new("/Applications/A.app");

        run_compress_helper(&host, &helper_layout(), bundle, false).unwrap();
        run_compress_helper(&host, &helper_layout(), bundle, true).unwrap();

        assert_eq!(
            *host.admin_prompts.lock().unwrap(),
            [
                "taert wants to compress an app",
                "taert wants to undo the compression of an app"
            ]
        );
    }

    #[test]
    fn an_error_answer_becomes_an_error() {
        let host = FakeHost::default();
        *host.admin_stdout.lock().unwrap() = r#"{"error": "x"}"#.to_string();

        let result = run_compress_helper(
            &host,
            &helper_layout(),
            Path::new("/Applications/A.app"),
            false,
        );

        assert_eq!(result.unwrap_err(), "x");
    }

    #[test]
    fn a_report_answer_becomes_the_report() {
        let host = FakeHost::default();
        *host.admin_stdout.lock().unwrap() = REPORT.to_string();

        let report = run_compress_helper(
            &host,
            &helper_layout(),
            Path::new("/Applications/A.app"),
            false,
        )
        .unwrap();

        assert_eq!(report.processed_files, 2);
        assert_eq!(report.saved_bytes, 60);
    }

    #[test]
    fn an_answer_that_is_not_json_is_an_error() {
        let host = FakeHost::default();
        *host.admin_stdout.lock().unwrap() = "Segmentation fault".to_string();

        let result = run_compress_helper(
            &host,
            &helper_layout(),
            Path::new("/Applications/A.app"),
            false,
        );

        assert!(result.unwrap_err().contains("Segmentation fault"));
    }

    #[test]
    fn a_refused_password_is_reported_as_it_is() {
        let host = FakeHost::default();
        *host.sudo_refused.lock().unwrap() = true;
        assert!(!host.sudo_validate("compress"));

        let result = run_compress_helper(
            &host,
            &helper_layout(),
            Path::new("/Applications/A.app"),
            false,
        );

        assert_eq!(
            result.unwrap_err(),
            "The administrator password was not given"
        );
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }

    #[test]
    fn a_path_that_is_not_utf8_is_refused_before_the_prompt() {
        use std::os::unix::ffi::OsStrExt;
        let host = FakeHost::default();
        *host.admin_stdout.lock().unwrap() = REPORT.to_string();
        let not_utf8 = Path::new(std::ffi::OsStr::from_bytes(b"/Applications/\xff.app"));
        let home_not_utf8 = Layout {
            home: std::ffi::OsStr::from_bytes(b"/Users/\xff").into(),
            ..helper_layout()
        };

        let bundle = run_compress_helper(&host, &helper_layout(), not_utf8, false);
        let home = run_compress_helper(
            &host,
            &home_not_utf8,
            Path::new("/Applications/A.app"),
            false,
        );

        assert_eq!(bundle.unwrap_err(), "the path is not valid UTF-8");
        assert_eq!(home.unwrap_err(), "the path is not valid UTF-8");
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }
}
