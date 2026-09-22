//! Shared harness for deemo's black-box tests.
//!
//! Rules of the suite:
//! - Drive the real binary; assert only observable contract — exit codes,
//!   stdout/stderr text, files under `$DEEMO_HOME`, process state.
//! - Wait for *conditions* with generous deadlines (`poll_until`), never for
//!   wall-clock budgets ("must finish within N ms"): a loaded machine must
//!   not turn green tests red. Causality ("deemo returned while the child
//!   was still running") replaces duration assertions.
//! - Every test carries a comment naming the README/DESIGN sentence it pins.

// The harness is shared by every test crate; each consumer uses a subset.
#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use assert_cmd::Command;

/// The binary under test as an assert_cmd `Command`.
pub fn deemo() -> Command {
    Command::cargo_bin("deemo").unwrap()
}

/// The binary under test as a plain `Command` (spawn / try_wait control).
pub fn bin() -> StdCommand {
    StdCommand::new(env!("CARGO_BIN_EXE_deemo"))
}

/// A fresh, isolated `$DEEMO_HOME`.
pub struct Home {
    dir: tempfile::TempDir,
}

impl Default for Home {
    fn default() -> Self {
        Self::new()
    }
}

impl Home {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// assert_cmd `Command` with `DEEMO_HOME` already set.
    pub fn deemo(&self) -> Command {
        let mut cmd = deemo();
        cmd.env("DEEMO_HOME", self.dir.path());
        cmd
    }

    /// plain `Command` with `DEEMO_HOME` already set.
    pub fn bin(&self) -> StdCommand {
        let mut cmd = bin();
        cmd.env("DEEMO_HOME", self.dir.path());
        cmd
    }
}

/// Log file names under `$DEEMO_HOME/logs`, sorted (names sort in time order
/// for a single naming scheme, oldest first).
pub fn log_names(home: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(home.join("logs"))
        .unwrap_or_else(|_| panic!("logs dir must exist under {}", home.display()))
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Poll until some log file of `prefix` satisfies `check`; return that
/// content. Checks *every* matching file — with several runs of one label
/// the newest run is not necessarily the first name, and an append-mode
/// file may hold several runs at once.
pub fn wait_for_log<F: Fn(&str) -> bool>(home: &Path, prefix: &str, check: F) -> String {
    let mut found = String::new();
    poll_until(&format!("log {prefix:?} to satisfy the check"), || {
        for name in log_names(home).iter().filter(|n| n.starts_with(prefix)) {
            if let Ok(content) = fs::read_to_string(home.join("logs").join(name)) {
                if check(&content) {
                    found = content;
                    return true;
                }
            }
        }
        false
    });
    found
}

/// The one and only log file whose name starts with `prefix`.
pub fn read_single_log(home: &Path, prefix: &str) -> String {
    let names = log_names(home);
    assert_eq!(names.len(), 1, "expected one log file, got {names:?}");
    assert!(
        names[0].starts_with(prefix),
        "log {names:?} must start with {prefix:?}"
    );
    fs::read_to_string(home.join("logs").join(&names[0])).unwrap()
}

/// Poll `check` until it holds (max 10 s), else panic naming `what`.
pub fn poll_until(what: &str, check: impl FnMut() -> bool) {
    poll_within(what, Duration::from_secs(10), check);
}

/// Same, with an explicit deadline.
pub fn poll_within(what: &str, within: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    loop {
        if check() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Deliver `sig` to a single pid.
#[cfg(unix)]
pub fn send_signal(pid: u32, sig: i32) {
    unsafe {
        libc::kill(pid as i32, sig);
    }
}

/// SIGKILL a whole process group. Only safe for children deemo *detached*
/// (setsid gave them a group of their own) — never the test's own group.
#[cfg(unix)]
pub fn kill_group(pid: u32) {
    unsafe {
        libc::killpg(pid as i32, libc::SIGKILL);
    }
}

/// Foreign pids whose command line contains `needle`, excluding `except`
/// (the deemo process under test usually matches its own child's cmdline).
#[cfg(unix)]
pub fn pids_matching(needle: &str, except: u32) -> Vec<u32> {
    let out = StdCommand::new("pgrep")
        .args(["-f", needle])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .filter(|&p| p != except)
        .collect()
}

/// Bind below the ephemeral range. A port from `:0` sits in that range, and a
/// concurrent test's client socket may then use the same number as its source
/// port — `lsof` would report that client as a binder of our port.
#[cfg(unix)]
pub fn bind_quiet() -> std::net::TcpListener {
    (20_000..30_000u16)
        .step_by(97)
        .find_map(|p| std::net::TcpListener::bind(("127.0.0.1", p)).ok())
        .expect("no free port in 20000..30000")
}

/// A port nothing else holds, released for the caller to hand to a child.
///
/// Each call consumes a fresh number from a process-wide descending seed:
/// parallel tests must never be handed the same port — one of them *kills*
/// its port, which would murder another test's server. 29000..30000 sits
/// below every platform's ephemeral range, so the rest of the system leaves
/// it alone.
#[cfg(unix)]
pub fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(29_999);
    loop {
        let candidate = NEXT.fetch_sub(1, Ordering::Relaxed);
        assert!(candidate >= 29_000, "free_port: seed exhausted");
        if std::net::TcpListener::bind(("127.0.0.1", candidate)).is_ok() {
            return candidate; // bound, then dropped: free for the caller
        }
    }
}
