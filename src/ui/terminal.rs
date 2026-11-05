//! Leaving the TUI for a program that needs the terminal, such as the sudo prompt.

use std::io::{self, stdout};
use std::time::Duration;

use crossterm::cursor::Show;
use crossterm::event;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::core::admin::{self, AdminSession, NOT_GIVEN};
use crate::core::host::Host;

pub(super) fn enter_tui() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)
}

pub(super) fn leave_tui() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen, Show);
}

/// Gives the terminal to `run`, then takes it back and drops the keys typed meanwhile.
pub fn release_terminal(run: &mut dyn FnMut()) {
    leave_tui();
    run();
    let _ = enter_tui();
    while event::poll(Duration::ZERO).unwrap_or(false) {
        let _ = event::read();
    }
}

pub struct AdminGate {
    session: &'static AdminSession,
    release: fn(&mut dyn FnMut()),
    pub redraw_all: bool,
}

impl AdminGate {
    pub fn new(session: &'static AdminSession, release: fn(&mut dyn FnMut())) -> Self {
        Self {
            session,
            release,
            redraw_all: false,
        }
    }

    pub fn production() -> Self {
        Self::new(admin::session(), release_terminal)
    }

    pub fn ask(&mut self, host: &dyn Host, reason: &str) -> Result<(), String> {
        if !self.session.will_prompt(host) {
            return self.session.prepare(host, reason);
        }
        // A release that never runs the closure gave sudo no chance to ask.
        let mut result = Err(NOT_GIVEN.to_string());
        (self.release)(&mut || result = self.session.prepare(host, reason));
        self.redraw_all = true;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::admin::test_session;
    use crate::core::host::fake::FakeHost;
    use std::cell::Cell;

    const PROBE: &str = "/usr/bin/sudo -n true";

    thread_local! {
        static RELEASES: Cell<usize> = const { Cell::new(0) };
    }

    fn counting_release(run: &mut dyn FnMut()) {
        RELEASES.with(|n| n.set(n.get() + 1));
        run();
    }

    fn counting_release_without_running(_run: &mut dyn FnMut()) {
        RELEASES.with(|n| n.set(n.get() + 1));
    }

    fn releases() -> usize {
        RELEASES.with(Cell::get)
    }

    fn gate() -> AdminGate {
        gate_with(counting_release)
    }

    fn gate_with(release: fn(&mut dyn FnMut())) -> AdminGate {
        RELEASES.with(|n| n.set(0));
        AdminGate::new(test_session(), release)
    }

    fn host(tty: bool) -> FakeHost {
        let host = FakeHost::default();
        *host.tty.lock().unwrap() = tty;
        host
    }

    fn asks(host: &FakeHost) -> usize {
        host.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("sudo -v"))
            .count()
    }

    #[test]
    fn a_held_ticket_keeps_the_screen() {
        let host = host(true);
        host.reply(PROBE, "");
        let mut gate = gate();

        assert_eq!(gate.ask(&host, "remove files"), Ok(()));

        assert_eq!(releases(), 0);
        assert_eq!(asks(&host), 0);
        assert!(!gate.redraw_all);
    }

    #[test]
    fn without_a_ticket_sudo_asks_inside_the_released_terminal() {
        let host = host(true);
        host.fail(PROBE, "");
        let mut gate = gate();

        assert_eq!(gate.ask(&host, "remove files"), Ok(()));

        assert_eq!(releases(), 1);
        assert_eq!(asks(&host), 1);
        assert!(gate.redraw_all);
    }

    #[test]
    fn sudo_asks_only_through_the_released_terminal() {
        let host = host(true);
        host.fail(PROBE, "");
        let mut gate = gate_with(counting_release_without_running);

        assert_eq!(gate.ask(&host, "remove files"), Err(NOT_GIVEN.to_string()));

        assert_eq!(releases(), 1);
        assert_eq!(asks(&host), 0);
    }

    #[test]
    fn a_refusal_is_returned_and_not_asked_again() {
        let host = host(true);
        host.fail(PROBE, "");
        *host.sudo_refused.lock().unwrap() = true;
        let mut gate = gate();

        assert_eq!(gate.ask(&host, "remove files"), Err(NOT_GIVEN.to_string()));
        assert_eq!(releases(), 1);
        assert!(gate.redraw_all);
        assert_eq!(gate.ask(&host, "remove files"), Err(NOT_GIVEN.to_string()));

        assert_eq!(releases(), 1);
        assert_eq!(asks(&host), 1);
    }

    #[test]
    fn without_a_terminal_nothing_is_released_or_asked() {
        let host = host(false);
        host.fail(PROBE, "");
        let mut gate = gate();

        assert_eq!(gate.ask(&host, "remove files"), Ok(()));

        assert_eq!(releases(), 0);
        assert_eq!(asks(&host), 0);
        assert!(!gate.redraw_all);
    }
}
