//! ③ pipe — `<cmd> | deemo`: a filter. It stays in the foreground, is
//! byte-exact passthrough plus a log, and exits with its own status on EOF.
//! README: "Pipes carry stdout only"; "Pipe, EOF reached → 0"; "Pipe,
//! downstream closed (| head) → 0".

mod support;

use std::io::Write as _;
use std::process::Stdio;

use predicates::prelude::*;
use std::process::Command as StdCommand;
use support::{wait_for_log, Home};

/// README: "capture a pipeline" — passthrough is byte-exact and logged, and
/// the mode announces itself so a waiting pipe never looks like a hang.
#[test]
fn pipe_passes_through_and_writes_log() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "t1"])
        .write_stdin("hello\nworld\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello\nworld\n"))
        .stderr(predicate::str::contains(
            "pipe mode: waiting for stdin to close",
        ));

    let content = support::read_single_log(home.path(), "t1-");
    assert_eq!(content, "hello\nworld\n");
}

/// DESIGN Pipe: block on reads until EOF — no line-size assumptions, no
/// reordering, no dropped tail (100k lines through an 8 KiB buffer).
#[test]
fn pipe_passes_through_large_stream_intact() {
    let home = Home::new();
    let input: String = (0..100_000).map(|i| format!("{i}\n")).collect();
    home.deemo()
        .args(["--label", "big", "--timestamp", "off"])
        .write_stdin(input.as_bytes())
        .assert()
        .success()
        .stdout(predicate::eq(input.as_str()));
    assert_eq!(
        std::fs::read_to_string(home.path().join("logs").join("big.log")).unwrap(),
        input
    );
}

/// DESIGN: downstream EPIPE (`… | deemo | head`) exits 0 with no error —
/// the reader stopped asking, that is not a failure.
#[test]
fn pipe_downstream_broken_pipe_exits_zero() {
    let home = Home::new();
    let mut child = home
        .bin()
        .args(["--label", "bp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take()); // close our read end of deemo's stdout
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(b"line\n".repeat(200_000).as_slice()); // EPIPE on deemo's side
    drop(stdin);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0), "EPIPE must exit 0");
}

/// Log sink: an EOF-truncated last line is kept verbatim (no added newline).
#[test]
fn pipe_keeps_output_without_trailing_newline() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "tail", "--timestamp", "off"])
        .write_stdin("no-newline")
        .assert()
        .success();
    assert_eq!(
        std::fs::read_to_string(home.path().join("logs").join("tail.log")).unwrap(),
        "no-newline"
    );
}

/// README `--quiet`: "Log only, no echo (foreground and pipe)".
#[test]
fn pipe_quiet_writes_log_without_echo() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "q", "--quiet"])
        .write_stdin("secret\n")
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    assert_eq!(support::read_single_log(home.path(), "q-"), "secret\n");
}

/// README "Exit codes": "Pipe, EOF reached → 0". A pipe cannot carry the
/// upstream's status, and a pipeline's `$?` is the last command's anyway —
/// an upstream that fails must not fail the filter.
#[test]
fn pipe_upstream_failure_still_exits_zero() {
    let home = Home::new();
    let mut producer = StdCommand::new("sh")
        .args(["-c", "echo hi; exit 3"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let out = home
        .bin()
        .args(["--label", "upfail"])
        .stdin(producer.stdout.take().unwrap())
        .output()
        .unwrap();
    assert_eq!(
        producer.wait().unwrap().code(),
        Some(3),
        "producer really failed"
    );
    assert_eq!(out.status.code(), Some(0), "the filter exits 0 on EOF");
    assert_eq!(support::read_single_log(home.path(), "upfail-"), "hi\n");
}

// --- pipe mode + --background -------------------------------------------------

/// DESIGN: `--background` forks the pump and returns at once; the parent's
/// work ends there (the "returns immediately" property itself is proven
/// causally by the test below, which keeps a writer open forever).
#[cfg(unix)]
#[test]
fn pipe_background_returns_and_captures_stream() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "pb", "--background"])
        .write_stdin("captured-line\n") // stdin closed after write
        .assert()
        .success();
    let content = wait_for_log(home.path(), "pb-", |c| c.contains("captured-line"));
    assert!(content.contains("captured-line"));
}

/// DESIGN: "A pipe stays open until every writer closes" — with our write
/// end still open the detached pump stays alive (visible in `ps`), while the
/// parent has *already returned*; closing the last writer ends the pump by
/// itself. Causal timing: no budget, only ordering.
#[cfg(unix)]
#[test]
fn pipe_background_survives_open_write_ends_and_is_manageable() {
    let home = Home::new();
    let mut child = home
        .bin()
        .args(["--label", "pump", "--background"])
        .stdin(Stdio::piped()) // our write end stays open below
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // try_wait: does NOT close our stdin (unlike wait()), keeping the write end open
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let code = loop {
        match child.try_wait().unwrap() {
            Some(status) => break status.code(),
            None => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "--background must return while writers are open"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    };
    assert_eq!(code, Some(0), "parent returns at once");

    // write end still open -> pump alive -> ps lists it
    home.deemo()
        .args(["ps"])
        .assert()
        .success()
        .stdout(predicate::str::contains("pump"))
        .stdout(predicate::str::contains("running"));

    // last writer closes -> pump reads EOF and exits on its own
    drop(child.stdin.take());
    support::poll_until("pump to exit when all writers close", || {
        let out = home.deemo().args(["ps"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains("no processes")
    });
}
