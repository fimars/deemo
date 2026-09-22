//! ⑦ logs — "newest log: path + last ~4 KiB".
//! README: `deemo logs <LABEL> # newest log: path + last ~4 KiB`.

#![cfg(unix)]

mod support;

use predicates::prelude::*;
use support::{wait_for_log, Home};

/// README: the *newest* log of the label — its path first, then the content.
/// Two runs, two files: `logs` must pick the greater (later) name — the
/// 0.1.1 regression where name order was reversed.
#[test]
fn logs_shows_the_newest_log_of_a_label() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "lg", "--", "sh", "-c", "echo run-one"])
        .assert()
        .success();
    wait_for_log(home.path(), "lg-", |c| c.contains("run-one"));
    home.deemo()
        .args(["--label", "lg", "--", "sh", "-c", "echo run-two"])
        .assert()
        .success();
    wait_for_log(home.path(), "lg-", |c| c.contains("run-two"));

    let out = home.deemo().args(["logs", "lg"]).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);

    // First line: the path of the greatest name (= newest run).
    let newest = support::log_names(home.path()).pop().unwrap();
    let path_line = text.lines().next().unwrap();
    assert!(
        path_line.ends_with(&newest),
        "expected {newest} first, got {path_line:?}"
    );
    // The newest run's content is what we read.
    assert!(text.contains("run-two"));
}

/// README "Exit codes": `logs`, no log for the label → 1, with a message
/// naming the label.
#[test]
fn logs_without_a_file_for_the_label_exits_1() {
    let home = Home::new();
    home.deemo()
        .args(["logs", "no-such-label"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no log file found for label 'no-such-label'",
        ));
}

/// README: "path + last ~4 KiB" — a big log is tailed, announced as a tail
/// (with the true total), and the head is not printed.
#[test]
fn logs_tails_only_the_last_4_kib_of_a_big_file() {
    let home = Home::new();
    let head = "HEAD-MARKER\n";
    let pad = "x".repeat(10_000);
    let tail = "TAIL-MARKER\n";
    home.deemo()
        .args(["--label", "big", "--timestamp", "off"])
        .write_stdin(format!("{head}{pad}{tail}"))
        .assert()
        .success();

    home.deemo()
        .args(["logs", "big"])
        .assert()
        .success()
        .stdout(predicate::str::contains("TAIL-MARKER"))
        .stdout(predicate::str::contains("… (showing last"))
        .stdout(predicate::str::contains("of 10024 bytes"))
        .stdout(predicate::str::contains("HEAD-MARKER").not());
}
