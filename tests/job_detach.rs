//! ① detach — `deemo -- <CMD>`: the job is to get the shell back.
//! README: "Closing the terminal leaves it running. stdout and stderr both
//! go to the log." DESIGN: "deemo exits after spawn and is not in the data
//! path"; "a daemon that reads stdin would otherwise stall on a closed
//! terminal"; "--yolo … does not register the process".

#![cfg(unix)]

mod support;

use predicates::prelude::*;
use support::{log_names, read_single_log, wait_for_log, Home};

/// README: detach "returns to the shell immediately" — causally, not by a
/// wall-clock budget: deemo must be back BEFORE the child's later output
/// exists, and the child must then outlive deemo and keep logging.
#[test]
fn detach_returns_before_the_child_finishes() {
    let home = Home::new();
    home.deemo()
        .args([
            "--label",
            "dt",
            "--",
            "sh",
            "-c",
            "sleep 1.5; echo done; exit 5",
        ])
        .assert()
        .success(); // does NOT wait for the child

    // At the instant deemo is gone, the child has not produced its output:
    // had deemo waited, "done" would already be in the log.
    let at_exit = std::fs::read_to_string(log_names_path(&home, "dt-")).unwrap_or_default();
    assert!(
        !at_exit.contains("done"),
        "deemo must exit while the child is still running"
    );

    // ... and the child keeps the log after us.
    let content = wait_for_log(home.path(), "dt-", |c| c.contains("done"));
    assert_eq!(content, "done\n");
}

/// Helper for the test above: the single log path with the given prefix.
fn log_names_path(home: &Home, prefix: &str) -> std::path::PathBuf {
    let names = log_names(home.path());
    assert_eq!(names.len(), 1, "expected one log file, got {names:?}");
    assert!(
        names[0].starts_with(prefix),
        "log {names:?} must start with {prefix:?}"
    );
    home.path().join("logs").join(&names[0])
}

/// DESIGN: "On unix the child calls setsid before exec" — the child is a
/// session (and group) leader, which is what makes `stop` a group kill.
#[test]
fn detach_child_runs_in_its_own_session() {
    let home = Home::new();
    home.deemo()
        .args([
            "--label",
            "sid",
            "--",
            "sh",
            "-c",
            "echo pid=$$; ps -o pgid= -p $$",
        ])
        .assert()
        .success();
    // The condition is the content we need — BOTH lines — not "the first
    // line appeared": the poll can catch the log between `echo` and `ps`.
    let content = wait_for_log(home.path(), "sid-", |c| {
        let mut probe = c.lines().map(str::trim);
        probe.next().is_some_and(|l| l.starts_with("pid="))
            && probe.next().is_some_and(|l| l.parse::<u32>().is_ok())
    });
    let mut lines = content.lines().map(|l| l.trim());
    let pid: u32 = lines
        .next()
        .unwrap()
        .strip_prefix("pid=")
        .unwrap()
        .parse()
        .unwrap();
    let pgid: u32 = lines.next().unwrap().parse().unwrap();
    assert_eq!(pid, pgid, "setsid must give the child its own session");
}

/// README `--label`: "Default: program name". A path is reduced to its file
/// name (`/usr/local/bin/foo` → `foo`).
#[test]
fn detach_default_label_is_program_name() {
    let home = Home::new();
    home.deemo()
        .args(["--", "sh", "-c", "exit 0"])
        .assert()
        .success();
    let names = log_names(home.path());
    assert_eq!(names.len(), 1);
    assert!(names[0].starts_with("sh-"), "got {names:?}");
}

/// README "Exit codes": Command not found → 127 (the shell's convention).
#[test]
fn detach_command_not_found_is_127() {
    let home = Home::new();
    home.deemo()
        .args(["--", "definitely-not-a-real-cmd-xyz"])
        .assert()
        .failure()
        .code(127);
}

/// DESIGN: "stdin is /dev/null … A daemon that reads stdin would otherwise
/// stall on a closed terminal" — a read gets EOF at once, never a hang.
#[test]
fn detach_child_reads_eof_on_stdin_immediately() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "eof", "--", "sh", "-c", "read x || echo got-eof"])
        .assert()
        .success();
    let content = wait_for_log(home.path(), "eof-", |c| c.contains("got-eof"));
    assert_eq!(content, "got-eof\n");
}

/// README `--yolo`: "No log files and no registration: nothing shows in ps,
/// nothing to stop". DESIGN: "there is nothing for `stop` to find, so the
/// `stop with:` hint is withheld" — deemo must not print a command that is
/// guaranteed to fail.
#[test]
fn detach_yolo_is_unmanageable_and_prints_no_stop_hint() {
    let home = Home::new();
    let out = home
        .bin()
        .args([
            "--yolo", "--label", "yolo-run", "--", "sh", "-c", "sleep 10",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--yolo"), "must announce yolo: {err}");
    assert!(
        !err.contains("stop with"),
        "no stop hint without a registration: {err}"
    );

    // The child is provably alive …
    let pid: u32 = err
        .split("(pid ")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .and_then(|s| s.parse().ok())
        .expect("detach must report the child pid");
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0, "child still alive");

    // … yet nothing is registered, so it is invisible to management.
    home.deemo()
        .args(["ps"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("no processes"));

    support::kill_group(pid); // no registration to stop it with — that's the point
}

/// DESIGN Logs: "If the log cannot be created, the command still runs and a
/// warning is printed." A label with a path separator cannot open its file;
/// the run must proceed, unregistered (no lying hint), honestly reported.
#[test]
fn detach_log_failure_still_runs_but_registers_nothing() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "a/b", "--", "echo", "ran-ok"])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("cannot open log file")
                .and(predicate::str::contains("running without logs")),
        )
        .stderr(predicate::str::contains("stop with").not());

    assert!(
        !home.path().join("logs").join("a").exists(),
        "nothing could be written"
    );
    home.deemo()
        .args(["ps"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("no processes"));
}

/// Sanity of the harness itself: a plain detach writes exactly one log.
#[test]
fn detach_writes_a_single_run_log() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "once", "--", "echo", "hi"])
        .assert()
        .success();
    wait_for_log(home.path(), "once-", |c| c.contains("hi"));
    assert_eq!(read_single_log(home.path(), "once-"), "hi\n");
}
