//! ② foreground — `deemo --foreground -- <CMD>`: stay attached, live
//! passthrough of both fds, the child's exit code becomes `$?`.
//! README: "Foreground: child's code; signal N → 128+N".

#![cfg(unix)]

mod support;

use predicates::prelude::*;
use std::process::Stdio;
use support::{read_single_log, Home};

/// README: stdout and stderr both stream through (no `2>&1` needed) and the
/// child's exit code is deemo's.
#[test]
fn foreground_merges_both_fds_and_returns_child_code() {
    let home = Home::new();
    home.deemo()
        .args([
            "--label",
            "sp",
            "--foreground",
            "--",
            "sh",
            "-c",
            "echo out-line; echo err-line 1>&2; exit 3",
        ])
        .assert()
        .failure()
        .code(3)
        .stdout(predicate::str::contains("out-line"))
        .stdout(predicate::str::contains("err-line"));

    let content = read_single_log(home.path(), "sp-");
    assert!(content.contains("out-line"));
    assert!(content.contains("err-line"), "stderr must be captured");
}

/// DESIGN Foreground: deemo must not quit while a captured fd is still
/// open — output that arrives after stdout closed still reaches terminal
/// and log, and the exit code is still the child's.
#[test]
fn foreground_waits_for_both_fds() {
    let home = Home::new();
    home.deemo()
        .args([
            "--label",
            "fds",
            "--foreground",
            "--",
            "sh",
            "-c",
            "echo early 1>&2; exec 1>&-; sleep 0.3; echo late 1>&2; exit 7",
        ])
        .assert()
        .failure()
        .code(7)
        .stdout(predicate::str::contains("early"))
        .stdout(predicate::str::contains("late"));
    let content = read_single_log(home.path(), "fds-");
    assert!(content.contains("early") && content.contains("late"));
}

/// README "Exit codes": signal death N → 128+N (SIGINT → 130, SIGTERM → 143).
#[test]
fn foreground_returns_signal_deaths_as_128_plus_n() {
    let home = Home::new();
    home.deemo()
        .args([
            "--label",
            "s1",
            "--foreground",
            "--",
            "sh",
            "-c",
            "kill -INT $$",
        ])
        .assert()
        .failure()
        .code(130);
    home.deemo()
        .args([
            "--label",
            "s2",
            "--foreground",
            "--",
            "sh",
            "-c",
            "kill -TERM $$",
        ])
        .assert()
        .failure()
        .code(143);
}

/// README "Exit codes": Command not found → 127, in every run mode.
#[test]
fn foreground_command_not_found_is_127() {
    let home = Home::new();
    home.deemo()
        .args(["-F", "--", "definitely-not-a-real-cmd-xyz"])
        .assert()
        .failure()
        .code(127);
}

/// README `--quiet`: "Log only, no echo (foreground and pipe)".
#[test]
fn foreground_quiet_logs_without_echo() {
    let home = Home::new();
    home.deemo()
        .args([
            "--label",
            "fq",
            "--foreground",
            "--quiet",
            "--",
            "sh",
            "-c",
            "echo secret",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    assert_eq!(read_single_log(home.path(), "fq-"), "secret\n");
}

/// DESIGN: "Downstream EPIPE … exits 0 … the same convention as ripgrep:
/// the reader stopped asking" — and the child is taken down with it instead
/// of being left writing into a broken pipe forever.
#[test]
fn foreground_downstream_broken_pipe_is_graceful_zero() {
    let home = Home::new();
    let mut child = home
        .bin()
        .args([
            "--label",
            "fgbp",
            "--foreground",
            "--",
            "sh",
            "-c",
            "while :; do echo flood; done",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdout.take()); // close our read end: deemo's next write is EPIPE

    // Bounded wait with guaranteed cleanup — a hung deemo must not leak the
    // flooding child into the background of whoever runs the suite.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait().unwrap() {
            Some(status) => {
                assert_eq!(status.code(), Some(0), "EPIPE must exit 0");
                return;
            }
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("deemo must exit 0 after downstream EPIPE");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}
