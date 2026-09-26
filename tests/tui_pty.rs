//! The TUI on a real pty whose terminal never answers a query, the way many don't. Nothing at
//! startup may be left reading stdin: if something is, it eats the first keypress and `q` no
//! longer quits (#32's review: 0 quits in 8 runs).

#![cfg(target_os = "linux")]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};

struct Pty {
    child: Child,
    master: File,
    output: Receiver<Vec<u8>>,
    seen: Vec<u8>,
}

/// Start reses on a fresh pty with a throwaway HOME, config and credentials, and `env` on top.
fn start(env: &[(&str, &str)]) -> (Pty, tempfile::TempDir) {
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
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls between fork and exec: a new session with the pty
    // as its controlling terminal, the way a shell starts a program.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setsid()?;
            rustix::process::ioctl_tiocsctty(std::io::stdin().as_fd())?;
            Ok(())
        });
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

    fn press(&mut self, key: &str) {
        self.master.write_all(key.as_bytes()).unwrap();
        self.master.flush().unwrap();
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
