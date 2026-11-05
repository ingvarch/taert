//! One sudo session per run: ask for the password once, keep the ticket alive, end it on exit.

use crate::core::host::{Host, MacHost};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const SUDO_TIMEOUT: Duration = Duration::from_secs(3);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::host::fake::FakeHost;
    use std::time::Instant;

    const PROBE: &str = "/usr/bin/sudo -n true";
    const KEEP_ALIVE: &str = "/usr/bin/sudo -n -v";
    const KILL: &str = "/usr/bin/sudo -k";

    fn count(host: &FakeHost, command: &str) -> usize {
        host.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with(command))
            .count()
    }

    fn asks(host: &FakeHost) -> usize {
        count(host, "sudo -v")
    }

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

        assert_eq!(asks(&host), 0);
        assert_eq!(count(&host, KILL), 0);
        assert!(!session.asked());
    }

    #[test]
    fn without_a_ticket_it_asks_once_and_ends_it_on_exit() {
        let (host, session) = fixture(true);
        host.fail(PROBE, "");

        assert_eq!(session.prepare(&*host, "remove files"), Ok(()));
        assert_eq!(session.prepare(&*host, "remove files"), Ok(()));
        assert_eq!(count(&host, PROBE), 1);
        assert!(session.asked());
        session.end(&*host);

        assert!(host.ran("sudo -v remove files"));
        assert_eq!(asks(&host), 1);
        assert_eq!(count(&host, KILL), 1);
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
        assert_eq!(count(&host, PROBE), 1);

        assert!(session.is_refused());
        assert!(!session.asked());
        assert_eq!(asks(&host), 1);
        session.end(&*host);
        assert_eq!(count(&host, KILL), 0);
    }

    #[test]
    fn without_a_terminal_it_asks_nothing() {
        let (host, session) = fixture(false);
        host.fail(PROBE, "");

        assert_eq!(session.prepare(&*host, "remove files"), Ok(()));
        session.end(&*host);

        assert_eq!(asks(&host), 0);
        assert_eq!(count(&host, KILL), 0);
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

        assert!(wait_until(|| count(&host, KEEP_ALIVE) >= 2));
        session.end(&*host);

        let after_end = count(&host, KEEP_ALIVE);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(count(&host, KEEP_ALIVE), after_end);
    }

    #[test]
    fn a_ticket_from_the_terminal_is_kept_alive_too() {
        let (host, session) = fixture(true);
        host.reply(PROBE, "");
        session.prepare(&*host, "x").unwrap();

        assert!(wait_until(|| count(&host, KEEP_ALIVE) >= 1));
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

        assert_eq!(count(&host, KILL), 1);
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
}
