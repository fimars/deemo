//! Cross-cutting A — Ctrl+C semantics. DESIGN: "1st press: remember it,
//! keep draining the stream so the child's final output is captured … If
//! the child is still alive 250 ms later, SIGINT is forwarded. 2nd press:
//! exit immediately." README: "Pipe, Ctrl+C → 130".
//!
//! Signals are delivered programmatically (`kill -INT <deemo-pid>`) — the
//! ctrlc crate handles them process-wide, so no real tty is needed here;
//! the tmux e2e covers the actual keystroke path.

#![cfg(unix)]

mod support;

use std::io::Read as _;
use std::process::Stdio;
use std::time::{Duration, Instant};

use support::{pids_matching, poll_until, send_signal, wait_for_log, Home};

/// Bounded wait for deemo to exit; on timeout kill it *and* the named
/// strays, then fail — a hung run must not leak background processes.
fn wait_exit_or_kill(child: &mut std::process::Child, strays: &[u32]) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code().unwrap_or(-1);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            for &pid in strays {
                send_signal(pid, libc::SIGKILL);
            }
            panic!("deemo did not exit within 10s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// First press in foreground: deemo says so, keeps draining, and 250 ms
/// later forwards SIGINT to a child that is still alive — the child's final
/// output must reach stdout and the log, and its exit code is deemo's.
#[test]
fn foreground_first_sigint_is_forwarded_and_final_output_survives() {
    let home = Home::new();
    let mut child = home
        .bin()
        .args([
            "--label",
            "sig1",
            "--foreground",
            "--",
            "sh",
            "-c",
            "trap 'echo cleanup-done; exit 0' INT; while :; do sleep 0.1; done",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deemo_pid = child.id();

    // Wait until the supervised child exists — which also means deemo has
    // recorded CHILD_PID for the 250 ms forward.
    let mut sh_pid = 0u32;
    poll_until("the supervised child to spawn", || {
        match pids_matching("trap 'echo cleanup-done", deemo_pid).first() {
            Some(&p) => {
                sh_pid = p;
                true
            }
            None => false,
        }
    });

    send_signal(deemo_pid, libc::SIGINT);
    let code = wait_exit_or_kill(&mut child, &[sh_pid]);

    // Collect what the streams saw (small output, still buffered).
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();

    assert_eq!(code, 0, "child exited via its trap: {err}");
    assert!(
        err.contains("press Ctrl+C again"),
        "first press must announce itself: {err}"
    );
    assert!(
        out.contains("cleanup-done"),
        "the forwarded SIGINT's final output must be drained: {out}"
    );
    wait_for_log(home.path(), "sig1-", |c| c.contains("cleanup-done"));
}

/// Second press forces deemo out with 130 at once — even when the child
/// ignores SIGINT entirely (it was never meant to be cut short).
#[test]
fn foreground_second_sigint_forces_exit_130() {
    let home = Home::new();
    let mut child = home
        .bin()
        .args([
            "--label",
            "sig2",
            "--foreground",
            "--",
            "sh",
            "-c",
            "trap '' INT; exec sleep 23.731", // one pid, SIGINT ignored (inherited)
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deemo_pid = child.id();

    let mut stray = 0u32;
    poll_until("the INT-ignoring child to spawn", || {
        match pids_matching("sleep 23.731", deemo_pid).first() {
            Some(&p) => {
                stray = p;
                true
            }
            None => false,
        }
    });

    send_signal(deemo_pid, libc::SIGINT);
    std::thread::sleep(Duration::from_millis(400)); // first press lands, forward runs (and is ignored)
    send_signal(deemo_pid, libc::SIGINT);

    let code = wait_exit_or_kill(&mut child, &[stray]);
    // Cleanup first, assert after — no strays left by a failed assertion.
    send_signal(stray, libc::SIGKILL);

    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert_eq!(code, 130, "second press exits 130 at once: {err}");
    assert!(err.contains("press Ctrl+C again"), "{err}");
}

/// README: "Pipe, Ctrl+C → 130" — first press keeps draining (lines written
/// *after* the interrupt still reach log and stdout), EOF ends the pump,
/// and the interrupted flag makes it 130.
#[test]
fn pipe_sigint_drains_then_exits_130_on_eof() {
    let home = Home::new();
    let mut child = home
        .bin()
        .args(["--label", "pipesig"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deemo_pid = child.id();
    let mut stdin = child.stdin.take().unwrap();

    use std::io::Write as _;
    stdin.write_all(b"before-int\n").unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300)); // let the handler install

    send_signal(deemo_pid, libc::SIGINT);
    stdin.write_all(b"after-int\n").unwrap();
    stdin.flush().unwrap(); // must still be drained after the interrupt
    std::thread::sleep(Duration::from_millis(300));
    drop(stdin); // every writer closed → EOF

    let code = wait_exit_or_kill(&mut child, &[]);

    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();

    assert_eq!(code, 130, "interrupted pipe exits 130 on EOF: {err}");
    assert!(
        out.contains("before-int") && out.contains("after-int"),
        "drain keeps both lines: {out}"
    );
    assert!(err.contains("press Ctrl+C again"), "{err}");
    wait_for_log(home.path(), "pipesig-", |c| {
        c.contains("before-int") && c.contains("after-int")
    });
}
