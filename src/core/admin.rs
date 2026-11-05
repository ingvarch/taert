//! One sudo session per run: ask for the password once, keep the ticket alive, end it on exit.

use crate::core::admin_helper;
use crate::core::apps::InstalledApp;
use crate::core::compress::CompressReport;
use crate::core::host::{Host, MacHost, shell_quote};
use crate::core::layout::Layout;
use crate::core::slim::{self, Remover, SlimDone};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const SUDO_TIMEOUT: Duration = Duration::from_secs(3);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const HELPER_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const SLIM_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const SLIM_PROMPT: &str = "taert wants to slim an app";
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
    hint_taken: bool,
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
        if !host.has_tty() || !host.is_admin() {
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
        !state.refused
            && state.ticket.is_none()
            && !probe(host)
            && host.has_tty()
            && host.is_admin()
    }

    pub fn is_refused(&self) -> bool {
        self.lock().refused
    }

    pub fn asked(&self) -> bool {
        self.lock().asked
    }

    /// True once, after taert's own `sudo -v` succeeded, so a tip shows at most once per run.
    pub fn take_hint(&self) -> bool {
        let mut state = self.lock();
        let due = state.asked && !state.hint_taken;
        state.hint_taken |= due;
        due
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
    let script = format!(
        "{HELPER_PATH} {} admin-helper compress{} --home {} {}",
        quoted(&helper_exe()?)?,
        if undo { " --undo" } else { "" },
        quoted(&layout.home)?,
        quoted(bundle)?,
    );
    let prompt = if undo { UNDO_PROMPT } else { COMPRESS_PROMPT };
    let answer = host.run_admin_output(&script, prompt, HELPER_TIMEOUT)?;
    parse_helper_answer(&answer)
}

/// The taert program that the script starts as root.
fn helper_exe() -> Result<PathBuf, String> {
    std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .map_err(|e| format!("Could not find the taert program: {e}"))
}

/// The `PATH` every script of the helper sets for itself, so root never runs a program from
/// the `PATH` of the user.
const HELPER_PATH: &str = "PATH=/usr/bin:/bin:/usr/sbin:/sbin";

/// The path quoted for the script. A lossy copy would name another file.
fn quoted(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(shell_quote)
        .ok_or_else(|| "the path is not valid UTF-8".to_string())
}

fn parse_helper_answer<T: serde::de::DeserializeOwned>(answer: &str) -> Result<T, String> {
    parse_step_answer(answer).map_err(|(_, error)| error)
}

/// The answer of the helper, or the error with the step that failed (step 1 when it names none).
fn parse_step_answer<T: serde::de::DeserializeOwned>(answer: &str) -> Result<T, (usize, String)> {
    let unreadable = || (1, unreadable_answer(answer));
    let value: serde_json::Value = serde_json::from_str(answer).map_err(|_| unreadable())?;
    if let Some(error) = value.get("error").and_then(|error| error.as_str()) {
        let step = value
            .get("step")
            .and_then(|step| step.as_u64())
            .map_or(1, |step| step as usize);
        return Err((step, error.to_string()));
    }
    serde_json::from_value(value).map_err(|_| unreadable())
}

fn unreadable_answer(answer: &str) -> String {
    format!("The admin helper gave an answer taert cannot read: {answer}")
}

const BIOUTIL_TIMEOUT: Duration = Duration::from_secs(5);

const COPY_TEMPLATE_HINT: &str = "Tip: sudo can use Touch ID. Copy /etc/pam.d/sudo_local.template to /etc/pam.d/sudo_local and remove the # before pam_tid.so.";
const ADD_LINE_HINT: &str = "Tip: sudo can use Touch ID. Add the line auth sufficient pam_tid.so at the top of /etc/pam.d/sudo_local.";

/// A tip for turning on Touch ID for sudo, when fingerprints are enrolled and PAM does not use them.
pub fn touch_id_hint(host: &dyn Host, layout: &Layout) -> Option<&'static str> {
    let local_path = layout.system("etc/pam.d/sudo_local");
    let read = |path| std::fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let local = read(&local_path);
    let sudo = read(&layout.system("etc/pam.d/sudo"));
    if [&local, &sudo]
        .into_iter()
        .any(|text| text.as_ref().is_ok_and(|text| enables_touch_id(text)))
    {
        return None;
    }
    if !has_fingerprints(host) {
        return None;
    }
    Some(if local_path.symlink_metadata().is_ok() {
        ADD_LINE_HINT
    } else {
        COPY_TEMPLATE_HINT
    })
}

fn enables_touch_id(pam: &str) -> bool {
    pam.lines().any(|line| {
        line.find("pam_tid.so")
            .is_some_and(|at| !line[..at].contains('#'))
    })
}

fn has_fingerprints(host: &dyn Host) -> bool {
    let output = host.run("/usr/bin/bioutil", &["-c"], BIOUTIL_TIMEOUT);
    if !output.ok {
        return false;
    }
    let own = format!("User {}:", host.uid());
    output.stdout.lines().any(|line| {
        line.strip_prefix(&own)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|count| count.parse::<u32>().ok())
            .is_some_and(|count| count > 0)
    })
}

#[derive(serde::Deserialize)]
struct SlimAnswer {
    saved: u64,
    old_app: PathBuf,
    handover_error: Option<String>,
}

/// Slims one root-owned app in taert run as root, then moves the old app to the Trash as the
/// user. The helper leaves the old app in `/Applications`; any other place is refused.
pub fn run_slim_helper(
    host: &dyn Host,
    layout: &Layout,
    app: &InstalledApp,
) -> Result<SlimDone, (usize, String)> {
    let script = slim_script(host, layout, &app.path).map_err(|error| (1, error))?;
    let answer = host
        .run_admin_output(&script, SLIM_PROMPT, SLIM_TIMEOUT)
        .map_err(|error| (1, error))?;
    let done: SlimAnswer = parse_step_answer(&answer)?;
    let old_app = done.old_app.as_path();
    let in_applications = old_app.parent() == Some(layout.system("Applications").as_path())
        && match (old_app.file_name(), app.path.file_stem()) {
            (Some(old), Some(name)) => slim::is_old_copy_name(old, name),
            _ => false,
        };
    let in_holding = old_app.file_name() == app.path.file_name()
        && old_app.parent().is_some_and(|holding| {
            holding.file_name().is_some()
                && holding.parent() == Some(admin_helper::holdings(layout).as_path())
        });
    let kept = |error, remover| SlimDone {
        saved: done.saved,
        old_app: Some((done.old_app.clone(), error)),
        holding_left: None,
        remover,
    };
    match done.handover_error {
        None if in_applications => Ok(slim::trash_old_app(done.old_app, done.saved, host)),
        Some(error) if in_applications => Ok(kept(error, Remover::Admin)),
        Some(error) if in_holding => Ok(kept(error, Remover::AdminHolding)),
        _ => Err((1, unreadable_answer(&answer))),
    }
}

fn slim_script(host: &dyn Host, layout: &Layout, app: &Path) -> Result<String, String> {
    Ok(format!(
        "{HELPER_PATH} {} admin-helper slim --home {} --owner {} --group {} {}",
        quoted(&helper_exe()?)?,
        quoted(&layout.home)?,
        host.uid(),
        unsafe { libc::getgid() },
        quoted(app)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bytes::format_bytes;
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
    fn a_standard_account_gets_the_dialog_and_no_sudo_prompt() {
        let (host, session) = fixture(true);
        host.fail(PROBE, "");
        *host.not_admin.lock().unwrap() = true;
        *host.sudo_refused.lock().unwrap() = true;

        assert!(!session.will_prompt(&*host));
        assert_eq!(session.prepare(&*host, "remove files"), Ok(()));
        session.end(&*host);

        assert_eq!(host.sudo_asks(), 0);
        assert!(!session.is_refused());
        assert!(!session.will_prompt(&*host));
        assert_eq!(host.run_admin("rm x", "p"), Ok(()));
        assert_eq!(
            *host.admin_scripts.lock().unwrap(),
            vec!["rm x".to_string()]
        );
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

    const BIOUTIL: &str = "/usr/bin/bioutil -c";
    const COPY_HINT: &str = "Tip: sudo can use Touch ID. Copy /etc/pam.d/sudo_local.template to /etc/pam.d/sudo_local and remove the # before pam_tid.so.";
    const ADD_HINT: &str = "Tip: sudo can use Touch ID. Add the line auth sufficient pam_tid.so at the top of /etc/pam.d/sudo_local.";
    const ENROLLED: &str =
        "User 501:\t2 biometric template(s)\nOperation performed successfully.\n";

    fn pam_sandbox(files: &[(&str, &str)]) -> (tempfile::TempDir, Layout) {
        let (dir, layout) = crate::core::apps::testing::sandbox();
        for (name, text) in files {
            let path = layout.system(&format!("etc/pam.d/{name}"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        (dir, layout)
    }

    #[test]
    fn no_hint_when_sudo_local_enables_touch_id() {
        let (_dir, layout) =
            pam_sandbox(&[("sudo_local", "auth       sufficient     pam_tid.so\n")]);
        let host = FakeHost::default();
        host.reply(BIOUTIL, ENROLLED);

        assert_eq!(touch_id_hint(&host, &layout), None);
        assert_eq!(host.count("/usr/bin/bioutil"), 0);
    }

    #[test]
    fn no_hint_when_sudo_enables_touch_id() {
        let (_dir, layout) = pam_sandbox(&[(
            "sudo",
            "auth sufficient pam_tid.so\nauth required pam_opendirectory.so\n",
        )]);
        let host = FakeHost::default();
        host.reply(BIOUTIL, ENROLLED);

        assert_eq!(touch_id_hint(&host, &layout), None);
        assert_eq!(host.count("/usr/bin/bioutil"), 0);
    }

    #[test]
    fn a_commented_line_does_not_enable_touch_id() {
        let (_dir, layout) = pam_sandbox(&[
            ("sudo_local", "#auth sufficient pam_tid.so\n"),
            ("sudo", "auth required pam_opendirectory.so\n"),
        ]);
        let host = FakeHost::default();
        host.reply(BIOUTIL, ENROLLED);

        assert_eq!(touch_id_hint(&host, &layout), Some(ADD_HINT));
        assert_eq!(host.count("/usr/bin/bioutil"), 1);
        assert!(host.ran(BIOUTIL));
    }

    #[test]
    fn a_comment_after_the_line_still_counts() {
        let (_dir, layout) =
            pam_sandbox(&[("sudo_local", "auth sufficient pam_tid.so # touch id\n")]);
        let host = FakeHost::default();
        host.reply(BIOUTIL, ENROLLED);

        assert_eq!(touch_id_hint(&host, &layout), None);
    }

    #[test]
    fn without_sudo_local_the_hint_says_to_copy_the_template() {
        let (_dir, layout) = pam_sandbox(&[("sudo", "auth required pam_opendirectory.so\n")]);
        let host = FakeHost::default();
        host.reply(BIOUTIL, ENROLLED);

        assert_eq!(touch_id_hint(&host, &layout), Some(COPY_HINT));
    }

    #[test]
    fn an_unreadable_sudo_local_still_gets_the_add_line_hint() {
        let (_dir, layout) = pam_sandbox(&[]);
        let path = layout.system("etc/pam.d/sudo_local");
        std::fs::create_dir_all(path).unwrap();
        let host = FakeHost::default();
        host.reply(BIOUTIL, ENROLLED);

        assert_eq!(touch_id_hint(&host, &layout), Some(ADD_HINT));
    }

    #[test]
    fn a_non_utf8_byte_in_a_comment_does_not_hide_an_active_line() {
        let (_dir, layout) = pam_sandbox(&[]);
        let path = layout.system("etc/pam.d/sudo_local");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"# caf\xe9\nauth sufficient pam_tid.so\n").unwrap();
        let host = FakeHost::default();
        host.reply(BIOUTIL, ENROLLED);

        assert_eq!(touch_id_hint(&host, &layout), None);
    }

    #[test]
    fn no_hint_without_fingerprints() {
        let (_dir, layout) = pam_sandbox(&[]);

        let none = FakeHost::default();
        none.reply(BIOUTIL, "User 501:\t0 biometric template(s)\n");
        assert_eq!(touch_id_hint(&none, &layout), None);

        let failed = FakeHost::default();
        failed.fail(BIOUTIL, "no biometrics");
        assert_eq!(touch_id_hint(&failed, &layout), None);

        let failed_with_text = FakeHost::default();
        failed_with_text.fail_after_printing(BIOUTIL, "User 501:\t2 biometric template(s)\n");
        assert_eq!(touch_id_hint(&failed_with_text, &layout), None);

        let other_user = FakeHost::default();
        other_user.reply(BIOUTIL, "User 502:\t2 biometric template(s)\n");
        assert_eq!(touch_id_hint(&other_user, &layout), None);

        let other_text = FakeHost::default();
        other_text.reply(BIOUTIL, "Biometrics are not available\n");
        assert_eq!(touch_id_hint(&other_text, &layout), None);
    }

    #[test]
    fn the_hint_follows_the_uid_of_the_host() {
        let (_dir, layout) = pam_sandbox(&[]);
        let host = FakeHost::default();
        host.set_uid(502);
        host.reply(
            BIOUTIL,
            "User 501:\t1 biometric template(s)\nUser 502:\t3 biometric template(s)\n",
        );

        assert_eq!(touch_id_hint(&host, &layout), Some(COPY_HINT));
    }

    #[test]
    fn the_hint_is_taken_once_and_only_after_taert_asked() {
        let (host, session) = fixture(true);
        host.fail(PROBE, "");
        assert!(!session.take_hint());

        session.prepare(&*host, "x").unwrap();
        assert!(session.take_hint());
        assert!(!session.take_hint());
        session.prepare(&*host, "x").unwrap();
        assert!(!session.take_hint());
    }

    #[test]
    fn a_ticket_from_the_terminal_gives_no_hint() {
        let (host, session) = fixture(true);
        host.reply(PROBE, "");
        session.prepare(&*host, "x").unwrap();

        assert!(!session.take_hint());
    }

    /// The layout of `helper_layout` with its root in `dir`, so no path of the real Mac is used.
    fn slim_layout(dir: &Path) -> Layout {
        Layout {
            root: dir.to_path_buf(),
            ..helper_layout()
        }
    }

    /// An app at `<dir>/Applications/Foo.app` and the answer of a helper that slimmed it and
    /// moved the old copy to `<dir>/Applications/<old name>`.
    fn slimmed(dir: &Path, old_name: &str) -> (InstalledApp, PathBuf, String) {
        let app = crate::core::slim::testing::app_for(
            &dir.join("Applications/Foo.app"),
            "com.vendor.foo",
        );
        let old = dir.join("Applications").join(old_name);
        let answer = serde_json::json!({ "saved": 4096, "old_app": old });
        (app, old, answer.to_string())
    }

    fn slim_host(answer: &str) -> FakeHost {
        let host = FakeHost::default();
        *host.admin_stdout.lock().unwrap() = answer.to_string();
        host.set_uid(501);
        host
    }

    #[test]
    fn the_slim_script_sets_its_own_path_and_quotes_every_part() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _, answer) = slimmed(dir.path(), "Foo.taert-old.app");
        let host = slim_host(&answer);

        run_slim_helper(&host, &slim_layout(dir.path()), &app).unwrap();

        let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
        let gid = unsafe { libc::getgid() };
        assert_eq!(
            host.admin_scripts.lock().unwrap()[0],
            format!(
                "PATH=/usr/bin:/bin:/usr/sbin:/sbin {} admin-helper slim --home '/Users/it'\\''s me' --owner 501 --group {gid} {}",
                shell_quote(&exe.to_string_lossy()),
                shell_quote(&app.path.to_string_lossy()),
            )
        );
        assert_eq!(
            *host.admin_prompts.lock().unwrap(),
            ["taert wants to slim an app"]
        );
    }

    #[test]
    fn the_parent_trashes_the_returned_old_app_as_the_user_with_one_root_call() {
        for old_name in ["Foo.taert-old.app", "Foo.taert-old 2.app"] {
            let dir = tempfile::tempdir().unwrap();
            let (app, old, answer) = slimmed(dir.path(), old_name);
            let host = slim_host(&answer);

            let done = run_slim_helper(&host, &slim_layout(dir.path()), &app).unwrap();

            assert_eq!(
                done,
                SlimDone {
                    saved: 4096,
                    old_app: None,
                    holding_left: None,
                    remover: Remover::User,
                }
            );
            assert_eq!(host.trashed(), [old]);
            assert_eq!(host.admin_scripts.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn the_parent_refuses_an_old_app_outside_applications_or_with_another_name_and_trashes_nothing()
    {
        let dir = tempfile::tempdir().unwrap();
        let (app, _, _) = slimmed(dir.path(), "Foo.taert-old.app");
        let root = dir.path();
        for old in [
            root.join("Applications/Vendor/Foo.taert-old.app"),
            root.join("Library/Foo.taert-old.app"),
            root.join("Applications/../Foo.taert-old.app"),
            PathBuf::from("/Users/x/Foo.taert-old.app"),
            root.join("Applications/Bar.taert-old.app"),
            root.join("Applications/Foo.app"),
            root.join("Applications/Foo"),
            root.join("Applications/Foo.taert-old"),
            root.join("Applications/Foo.taert-old 1.app"),
            root.join("Applications/Foo.taert-old 02.app"),
            root.join("Applications/Foo.taert-old x.app"),
            root.join("Applications/Foobar.taert-old.app"),
        ] {
            let answer = serde_json::json!({ "saved": 4096, "old_app": old }).to_string();
            let host = slim_host(&answer);

            let (step, error) = run_slim_helper(&host, &slim_layout(root), &app).unwrap_err();

            assert_eq!(step, 1, "{old:?}");
            assert!(error.contains("cannot read"), "{old:?}: {error}");
            assert!(host.trashed().is_empty(), "{old:?}");
        }
    }

    #[test]
    fn a_failed_trash_keeps_the_old_app_in_the_result_and_asks_for_nothing_more() {
        let dir = tempfile::tempdir().unwrap();
        let (app, old, answer) = slimmed(dir.path(), "Foo.taert-old.app");
        let host = slim_host(&answer);
        host.fail_trash("denied");

        let done = run_slim_helper(&host, &slim_layout(dir.path()), &app).unwrap();

        assert_eq!(done.saved, 4096);
        assert_eq!(done.old_app, Some((old, "denied".to_string())));
        assert_eq!(done.holding_left, None);
        assert_eq!(host.admin_scripts.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_old_app_that_could_not_be_handed_over_is_not_sent_to_the_trash() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _, _) = slimmed(dir.path(), "Foo.taert-old.app");
        let holding = dir
            .path()
            .join("Library/Application Support/taert/slim/1.0");
        let old = holding.join("Foo.app");
        let answer = serde_json::json!({
            "saved": 4096, "old_app": old, "handover_error": "denied",
        });
        let host = slim_host(&answer.to_string());

        let done = run_slim_helper(&host, &slim_layout(dir.path()), &app).unwrap();

        assert_eq!(done.old_app, Some((old, "denied".to_string())));
        assert_eq!(done.holding_left, None);
        assert_eq!(done.remover, Remover::AdminHolding);
        assert_eq!(
            done.summary("Foo"),
            format!(
                "Slimmed Foo, saved {}. taert could not give you the old app (denied). An administrator must remove the folder {}, which holds it.",
                format_bytes(4096),
                holding.display()
            )
        );
        assert!(host.trashed().is_empty());
    }

    #[test]
    fn an_old_app_in_applications_that_root_still_owns_is_left_for_finder() {
        let dir = tempfile::tempdir().unwrap();
        let (app, old, _) = slimmed(dir.path(), "Foo.taert-old.app");
        let answer = serde_json::json!({
            "saved": 4096, "old_app": old, "handover_error": "denied",
        });
        let host = slim_host(&answer.to_string());

        let done = run_slim_helper(&host, &slim_layout(dir.path()), &app).unwrap();

        assert_eq!(done.old_app, Some((old.clone(), "denied".to_string())));
        assert_eq!(done.remover, Remover::Admin);
        assert_eq!(
            done.summary("Foo"),
            format!(
                "Slimmed Foo, saved {}. The old app is at {}, but taert could not give it to you (denied). Move it to the Trash in Finder, which asks for an administrator password.",
                format_bytes(4096),
                old.display()
            )
        );
        assert!(host.trashed().is_empty());
    }

    #[test]
    fn an_old_app_left_by_root_elsewhere_is_refused_and_nothing_is_trashed() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _, _) = slimmed(dir.path(), "Foo.taert-old.app");
        let slim = dir.path().join("Library/Application Support/taert/slim");
        for old in [
            dir.path().join("Library/Foo.app"),
            slim.join("Foo.app"),
            slim.join("1.0/Bar.app"),
            slim.join("1.0/Contents/Foo.app"),
            slim.join("../1.0/Foo.app"),
            slim.join("1.0/../Foo.app"),
            dir.path().join("Applications/Vendor/Foo.taert-old.app"),
        ] {
            let answer = serde_json::json!({
                "saved": 4096, "old_app": old, "handover_error": "denied",
            });
            let host = slim_host(&answer.to_string());

            let (step, error) = run_slim_helper(&host, &slim_layout(dir.path()), &app).unwrap_err();

            assert_eq!(step, 1, "{old:?}");
            assert!(error.contains("cannot read"), "{old:?}: {error}");
            assert!(host.trashed().is_empty(), "{old:?}");
        }
    }

    #[test]
    fn a_refused_password_stops_the_slim_at_step_one() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _, answer) = slimmed(dir.path(), "Foo.taert-old.app");
        let host = slim_host(&answer);
        *host.sudo_refused.lock().unwrap() = true;
        assert!(!host.sudo_validate("slim"));

        let result = run_slim_helper(&host, &slim_layout(dir.path()), &app);

        assert_eq!(result.unwrap_err(), (1, NOT_GIVEN.to_string()));
        assert!(host.admin_scripts.lock().unwrap().is_empty());
        assert!(host.trashed().is_empty());
    }

    #[test]
    fn an_error_answer_keeps_its_step_and_trashes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _, _) = slimmed(dir.path(), "Foo.taert-old.app");
        for (answer, expected) in [
            (r#"{"error":"no arm","step":3}"#, (3, "no arm")),
            (r#"{"error":"stopped"}"#, (1, "stopped")),
        ] {
            let host = slim_host(answer);

            let result = run_slim_helper(&host, &slim_layout(dir.path()), &app);

            assert_eq!(result.unwrap_err(), (expected.0, expected.1.to_string()));
            assert!(host.trashed().is_empty());
        }
    }

    #[test]
    fn an_answer_that_is_not_the_slim_answer_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _, _) = slimmed(dir.path(), "Foo.taert-old.app");
        for answer in ["Segmentation fault", "{}", r#"{"saved": 1}"#] {
            let host = slim_host(answer);

            let (step, error) = run_slim_helper(&host, &slim_layout(dir.path()), &app).unwrap_err();

            assert_eq!(step, 1);
            assert!(error.contains("cannot read"), "{error}");
            assert!(host.trashed().is_empty());
        }
    }

    #[test]
    fn a_path_that_is_not_utf8_is_refused_before_the_slim_prompt() {
        use std::os::unix::ffi::OsStrExt;
        let host = slim_host("{}");
        let app = crate::core::slim::testing::app_for(
            Path::new(std::ffi::OsStr::from_bytes(b"/Applications/\xff.app")),
            "com.vendor.x",
        );

        let result = run_slim_helper(&host, &helper_layout(), &app);

        assert_eq!(
            result.unwrap_err(),
            (1, "the path is not valid UTF-8".to_string())
        );
        assert!(host.admin_scripts.lock().unwrap().is_empty());
    }
}
