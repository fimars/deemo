//! ⑤ stop — SIGTERM to the whole process group, grace, SIGKILL, honest
//! report. README: "A label stops every process registered under it … to
//! stop just one, read its pid off `deemo ps` and pass that." DESIGN:
//! "a numeric argument is a pid, never a label".

#![cfg(unix)]

mod support;

use predicates::prelude::*;
use std::fs;
use support::{poll_until, Home};

/// Start two detached `sleep`s under one label and return their pids,
/// ascending — the `pnpm dev:admin` + `pnpm dev:merchant` shape: two
/// processes, one label.
fn start_two_dup(home: &Home) -> (u32, u32) {
    for _ in 0..2 {
        home.deemo()
            .args(["--label", "dup", "--", "sleep", "30"])
            .assert()
            .success();
    }
    let mut pids: Vec<u32> = fs::read_dir(home.path().join("run"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter_map(|name| {
            name.strip_prefix("dup.")
                .and_then(|rest| rest.strip_suffix(".pid"))
                .and_then(|pid| pid.parse().ok())
        })
        .collect();
    pids.sort_unstable();
    assert_eq!(pids.len(), 2, "expected two registrations, got {pids:?}");
    (pids[0], pids[1])
}

/// The pid file deemo keeps for a `dup` registration.
fn dup_pid_file(home: &Home, pid: u32) -> std::path::PathBuf {
    home.path().join("run").join(format!("dup.{pid}.pid"))
}

/// README: "A pid names one process … A label is the unit: it takes down
/// every registration under it."
#[test]
fn stop_takes_a_label_or_a_pid() {
    let home = Home::new();
    let (first, second) = start_two_dup(&home);

    // A pid names one process: it goes, the other keeps running.
    home.deemo()
        .arg("stop")
        .arg(first.to_string())
        .assert()
        .success()
        .stderr(predicate::str::contains(format!(
            "stopped dup (pid {first}"
        )));
    assert!(!dup_pid_file(&home, first).exists());
    assert!(dup_pid_file(&home, second).exists());

    // A label is the unit: it takes down every registration under it.
    home.deemo()
        .args(["stop", "dup"])
        .assert()
        .success()
        .stderr(predicate::str::contains(format!(
            "stopped dup (pid {second}"
        )));
    home.deemo()
        .args(["ps"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("no processes"));
}

/// README "Exit codes": `stop`, no target matched → 1 (reported per target,
/// the run still stops what it *did* match).
#[test]
fn stop_reports_an_unknown_pid() {
    let home = Home::new();
    home.deemo()
        .args(["stop", "4294967294"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no process with pid 4294967294"));
}

/// The headline 0.1.1 fix: a launcher spawns the real workload as a child
/// (pnpm → node → bound port). Killing only the registered pid would orphan
/// the rest — the stop must take the whole group setsid created.
#[test]
fn stop_kills_the_whole_process_group_not_just_the_leader() {
    let home = Home::new();
    home.deemo()
        .args([
            "--label",
            "tree",
            "--",
            "sh",
            "-c",
            "sleep 61.731 & sleep 62.731 & wait",
        ])
        .assert()
        .success();
    std::thread::sleep(std::time::Duration::from_millis(300)); // let the shell fork

    home.deemo()
        .args(["stop", "tree"])
        .assert()
        .success()
        .stderr(predicate::str::contains("stopped"));

    // No member of the tree may survive (pgrep prints nothing once all gone).
    poll_until("every group member to die", || {
        let out = std::process::Command::new("pgrep")
            .args(["-f", "sleep 6[12]\\.73[1]"])
            .output()
            .unwrap();
        out.stdout.is_empty()
    });

    // ... and the pid file is gone with them.
    home.deemo()
        .args(["ps"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("no processes"));
}

/// 0.1.1: SIGTERM alone must not be trusted — a target that ignores it goes
/// down to SIGKILL after the grace period, and the report says so (which is
/// also why this test takes ~6 s: it waits out the real 5 s + 1 s policy).
#[test]
fn stop_escalates_to_sigkill_when_sigterm_is_ignored() {
    let home = Home::new();
    home.deemo()
        .args([
            "--label",
            "stubborn",
            "--",
            "sh",
            "-c",
            "trap '' TERM; while :; do sleep 0.5; done", // whole group ignores SIGTERM
        ])
        .assert()
        .success();
    std::thread::sleep(std::time::Duration::from_millis(300)); // let the trap install

    home.deemo()
        .args(["stop", "stubborn"])
        .assert()
        .success()
        .stderr(predicate::str::contains("SIGKILL after grace period"));

    // The registration went with the group.
    assert!(home.path().join("run").read_dir().unwrap().next().is_none());
    home.deemo()
        .args(["ps"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("no processes"));
}

/// 0.1.1: a stale pid file must be removed by `stop` too, not just by `ps`
/// — and a label whose process already exited matches nothing → exit 1.
#[test]
fn stop_sweeps_dead_registrations_instead_of_keeping_them() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "gone", "--", "sh", "-c", "exit 0"])
        .assert()
        .success();
    std::thread::sleep(std::time::Duration::from_millis(300)); // child exits

    home.deemo().args(["stop", "gone"]).assert().code(1); // label no longer exists -> "no process with label"
    assert!(home.path().join("run").read_dir().unwrap().next().is_none());
}

/// Multiple targets in one invocation: matches stop, misses are reported,
/// and the exit code says "something did not happen" — partial success.
#[test]
fn stop_stops_matches_and_reports_the_missing_in_one_run() {
    let home = Home::new();
    for label in ["aa", "bb"] {
        home.deemo()
            .args(["--label", label, "--", "sleep", "30"])
            .assert()
            .success();
    }

    home.deemo()
        .args(["stop", "aa", "nosuch-label"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("stopped aa"))
        .stderr(predicate::str::contains(
            "no process with label 'nosuch-label'",
        ));

    // aa is gone, bb untouched. (Match on the padded label column — the
    // tempdir in the log path may itself contain "aa" or "bb".)
    let out = home.deemo().args(["ps"]).output().unwrap();
    let ps = String::from_utf8_lossy(&out.stdout);
    let listed = |label: &str| {
        ps.lines()
            .any(|l| l.starts_with(label) && l[label.len()..].starts_with(' '))
    };
    assert!(!listed("aa"), "aa must be stopped: {ps}");
    assert!(listed("bb"), "bb must survive: {ps}");

    home.deemo().args(["stop", "bb"]).assert().success();
}

/// The dedupe rule (registry): `deemo stop dup <its-pid>` selects one entry
/// twice — it must be stopped once, with one honest report.
#[test]
fn stop_stops_a_process_selected_twice_only_once() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "dup", "--", "sleep", "30"])
        .assert()
        .success();
    let pid: u32 = fs::read_dir(home.path().join("run"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .find_map(|name| {
            name.strip_prefix("dup.")
                .and_then(|rest| rest.strip_suffix(".pid"))
                .and_then(|pid| pid.parse().ok())
        })
        .expect("registration for label dup");

    let out = home
        .deemo()
        .args(["stop", "dup", &pid.to_string()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(err.matches("stopped dup").count(), 1, "stopped once: {err}");
    assert!(!home
        .path()
        .join("run")
        .join(format!("dup.{pid}.pid"))
        .exists());
}

/// DESIGN: "A label that is all digits is therefore reachable only as a
/// pid" — `stop 123` must be read as *pid* 123 (which is not it), fail
/// honestly, and leave the process alone; the pid off `ps` is the way.
#[test]
fn stop_reads_a_numeric_label_as_a_pid() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "123", "--", "sleep", "30"])
        .assert()
        .success();
    let pid: u32 = fs::read_dir(home.path().join("run"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .find_map(|name| {
            name.strip_prefix("123.")
                .and_then(|rest| rest.strip_suffix(".pid"))
                .and_then(|pid| pid.parse().ok())
        })
        .expect("registration for label 123");

    // As a *label* it is unreachable: the number is taken as a pid …
    home.deemo()
        .args(["stop", "123"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no process with pid 123"));

    // … the process is untouched and still listed …
    home.deemo()
        .args(["ps"])
        .assert()
        .success()
        .stdout(predicate::str::contains("running"));

    // … and its pid (from `ps`) is the handle that works.
    home.deemo()
        .arg("stop")
        .arg(pid.to_string())
        .assert()
        .success()
        .stderr(predicate::str::contains("stopped 123"));
    assert!(
        !home
            .path()
            .join("run")
            .join(format!("123.{pid}.pid"))
            .exists(),
        "pid file gone"
    );
}
