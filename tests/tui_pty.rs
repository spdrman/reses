//! The TUI on a real pty whose terminal never answers a query, the way many don't. Nothing at
//! startup may be left reading stdin: if something is, it eats the first keypress and `q` no
//! longer quits (#32's review: 0 quits in 8 runs).
//!
//! It's also where I check the way out (#28, N10): a kill, a hangup or an interrupt ends the app
//! through its own exit, so the pty is left in cooked mode on the normal screen, and ctrl-z
//! hands the terminal back, stops, and takes the screen again on resume.

#![cfg(target_os = "linux")]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, Winsize, tcgetattr, tcsetwinsize};

/// The sequences crossterm writes to enter and leave the alternate screen.
const ENTER: &str = "\u{1b}[?1049h";
const LEAVE: &str = "\u{1b}[?1049l";

struct Pty {
    child: Child,
    master: File,
    /// The app's side of the pty, kept open so the test can read its terminal modes.
    slave: File,
    output: Receiver<Vec<u8>>,
    seen: Vec<u8>,
}

/// Start reses on a fresh pty with a throwaway HOME, config and credentials, and `env` on top.
fn start(env: &[(&str, &str)]) -> (Pty, tempfile::TempDir) {
    start_as(env, Session::New)
}

/// How the app is started relative to the test.
#[derive(Clone, Copy, PartialEq)]
enum Session {
    /// A new session with the pty as its controlling terminal, the way a shell starts it.
    New,
    /// Its own process group in the test's session. A process group whose parent sits in
    /// another session is orphaned, and the kernel then discards a stop signal, so ctrl-z
    /// can only be seen to stop the app this way.
    Group,
}

fn start_as(env: &[(&str, &str)], session: Session) -> (Pty, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let master: OwnedFd = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    tcsetwinsize(
        &master,
        Winsize {
            ws_row: 24,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    let slave_path = ptsname(&master, Vec::new()).unwrap();
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .open(slave_path.to_str().unwrap())
        .unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_reses"));
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home.path())
        .env("TERM", "xterm-256color")
        .env("RESES_CONFIG", home.path().join("config.toml"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            home.path().join("credentials"),
        )
        .env("AWS_CONFIG_FILE", home.path().join("aws-config"))
        .envs(env.iter().copied())
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    match session {
        // SAFETY: only async-signal-safe calls between fork and exec: a new session with the
        // pty as its controlling terminal, the way a shell starts a program.
        Session::New => unsafe {
            cmd.pre_exec(|| {
                rustix::process::setsid()?;
                rustix::process::ioctl_tiocsctty(std::io::stdin().as_fd())?;
                Ok(())
            });
        },
        Session::Group => {
            cmd.process_group(0);
        }
    }
    let child = cmd.spawn().unwrap();

    let master = File::from(master);
    let mut reader = master.try_clone().unwrap();
    let (tx, output) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    (
        Pty {
            child,
            master,
            slave,
            output,
            seen: Vec::new(),
        },
        home,
    )
}

impl Pty {
    /// Wait until the screen output contains `text`. The pty never answers anything the app
    /// asks the terminal: that's the point.
    fn wait_for(&mut self, text: &str, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if String::from_utf8_lossy(&self.seen).contains(text) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.output.recv_timeout(left) {
                Ok(chunk) => self.seen.extend(chunk),
                Err(_) => return false,
            }
        }
    }

    /// How many times `text` has appeared in the output so far, reading what's arrived.
    fn count(&mut self, text: &str) -> usize {
        while let Ok(chunk) = self.output.try_recv() {
            self.seen.extend(chunk);
        }
        String::from_utf8_lossy(&self.seen).matches(text).count()
    }

    /// Wait until `text` has appeared `n` times.
    fn wait_for_count(&mut self, text: &str, n: usize, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while self.count(text) < n {
            if Instant::now() >= deadline {
                return false;
            }
            if let Ok(chunk) = self.output.recv_timeout(Duration::from_millis(20)) {
                self.seen.extend(chunk);
            }
        }
        true
    }

    /// Send `sig` to the app.
    fn signal(&self, sig: Signal) {
        let pid = Pid::from_raw(self.child.id() as i32).unwrap();
        kill_process(pid, sig).unwrap();
    }

    /// Whether the pty is back in cooked mode: echo and line editing on, as a shell leaves it.
    fn cooked(&self) -> bool {
        let modes = tcgetattr(&self.slave).unwrap().local_modes;
        modes.contains(LocalModes::ICANON | LocalModes::ECHO)
    }

    /// Whether the app is stopped, from /proc: the state letter after the command name.
    fn stopped_within(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let stat = format!("/proc/{}/stat", self.child.id());
        while Instant::now() < deadline {
            let s = std::fs::read_to_string(&stat).unwrap_or_default();
            if s.rsplit(") ")
                .next()
                .is_some_and(|rest| rest.starts_with('T'))
            {
                return true;
            }
            std::thread::yield_now();
        }
        false
    }

    fn press(&mut self, key: &str) {
        self.master.write_all(key.as_bytes()).unwrap();
        self.master.flush().unwrap();
    }

    /// True when the app is still running after `d`: it is waiting for a key, not gone already.
    fn still_running_after(&mut self, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return false;
            }
            while let Ok(chunk) = self.output.recv_timeout(Duration::from_millis(20)) {
                self.seen.extend(chunk);
            }
        }
        true
    }

    /// Whether the app exited within `limit`, killing it if not.
    fn exits_within(&mut self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if self.child.try_wait().unwrap().is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return false;
            }
            // Drain the screen so the app never blocks writing to a full pty.
            while let Ok(chunk) = self.output.recv_timeout(Duration::from_millis(20)) {
                self.seen.extend(chunk);
            }
        }
    }
}

fn first_q_quits(env: &[(&str, &str)]) {
    let (mut pty, _home) = start(env);
    assert!(
        pty.wait_for("Accounts", Duration::from_secs(20)),
        "{env:?}: the accounts screen never drew:\n{}",
        String::from_utf8_lossy(&pty.seen)
    );
    // A positive control: without a key it keeps running, so a quick exit below is the q.
    assert!(
        pty.still_running_after(Duration::from_millis(300)),
        "{env:?}: reses exited before any key:\n{}",
        String::from_utf8_lossy(&pty.seen)
    );
    pty.press("q");
    assert!(
        pty.exits_within(Duration::from_secs(5)),
        "{env:?}: the first q did not quit; something is still reading stdin"
    );
}

#[test]
fn the_first_q_quits_on_a_terminal_that_never_answers() {
    first_q_quits(&[]);
}

#[test]
fn the_first_q_quits_on_apple_terminal() {
    first_q_quits(&[("TERM_PROGRAM", "Apple_Terminal")]);
}

#[test]
fn the_first_q_quits_on_a_named_image_terminal_too() {
    // Named as an image terminal, but this pty reports no pixel size, so the header stays text,
    // and nothing is asked of the terminal on the way.
    first_q_quits(&[("TERM_PROGRAM", "iTerm.app")]);
    first_q_quits(&[("TERM", "xterm-kitty"), ("KITTY_WINDOW_ID", "1")]);
}

#[test]
fn reses_logo_text_never_asks_either() {
    first_q_quits(&[("RESES_LOGO", "text"), ("TERM_PROGRAM", "WezTerm")]);
}

/// Start the app and wait for its first screen.
fn started(session: Session) -> (Pty, tempfile::TempDir) {
    let (mut pty, home) = start_as(&[], session);
    assert!(
        pty.wait_for("Accounts", Duration::from_secs(20)),
        "the accounts screen never drew:\n{}",
        String::from_utf8_lossy(&pty.seen)
    );
    assert!(!pty.cooked(), "the app should have the pty in raw mode");
    (pty, home)
}

#[test]
fn a_kill_a_hangup_or_an_interrupt_restores_the_terminal() {
    for sig in [Signal::Term, Signal::Hup, Signal::Int] {
        let (mut pty, _home) = started(Session::New);
        pty.signal(sig);
        assert!(
            pty.exits_within(Duration::from_secs(10)),
            "{sig:?}: the app never exited"
        );
        // Drain what it wrote on the way out.
        pty.wait_for(LEAVE, Duration::from_secs(2));
        assert!(pty.cooked(), "{sig:?}: the pty was left in raw mode");
        let out = String::from_utf8_lossy(&pty.seen).into_owned();
        assert!(
            out.rfind(LEAVE) > out.rfind(ENTER),
            "{sig:?}: the app never left the alternate screen"
        );
    }
}

#[test]
fn ctrl_z_hands_the_terminal_back_and_resume_takes_it_again() {
    let (mut pty, _home) = started(Session::Group);
    assert_eq!(pty.count(ENTER), 1);
    pty.press("\u{1a}");
    assert!(
        pty.wait_for_count(LEAVE, 1, Duration::from_secs(10)),
        "ctrl-z never left the alternate screen"
    );
    assert!(
        pty.stopped_within(Duration::from_secs(10)),
        "ctrl-z didn't stop the app"
    );
    assert!(pty.cooked(), "the pty was left in raw mode while stopped");

    pty.signal(Signal::Cont);
    assert!(
        pty.wait_for_count(ENTER, 2, Duration::from_secs(10)),
        "resuming never took the screen again"
    );
    assert!(
        pty.wait_for_count("Accounts", 2, Duration::from_secs(10)),
        "the screen was never drawn again after resuming"
    );
    assert!(!pty.cooked(), "the app should be back in raw mode");
    pty.press("q");
    assert!(
        pty.exits_within(Duration::from_secs(5)),
        "q didn't quit after resuming"
    );
    assert!(pty.cooked());
}
