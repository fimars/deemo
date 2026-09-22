//! ④ ps — "the manager": what deemo started, and whether it still runs.
//! README: `deemo ps # label, pid, status, log`; exit `1` when nothing is
//! running; DESIGN: registrations live in `run/<label>.<pid>.pid`.

#![cfg(unix)]

mod support;

use predicates::prelude::*;
use support::{poll_until, wait_for_log, Home};

/// README: ps lists label, pid, status ("running") and the log path.
#[test]
fn ps_lists_a_running_detach_with_label_pid_and_status() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "listed", "--", "sleep", "30"])
        .assert()
        .success();

    home.deemo()
        .args(["ps"])
        .assert()
        .success()
        .stdout(predicate::str::contains("LABEL")) // header row
        .stdout(predicate::str::contains("listed"))
        .stdout(predicate::str::contains("running"))
        .stdout(predicate::str::contains(".log")); // cmd -> log

    home.deemo().args(["stop", "listed"]).assert().success();
}

/// DESIGN: the pid file holds `pid`, `label`, `started`, `cmd`, `log` — the
/// record `ps` and `stop` read back later.
#[test]
fn detach_writes_a_readable_registration_record() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "rec", "--", "echo", "reg-ok"])
        .assert()
        .success();

    let run = home.path().join("run");
    poll_until("the registration file to exist", || {
        std::fs::read_dir(&run)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    });

    let entry: Vec<_> = std::fs::read_dir(&run).unwrap().collect();
    assert_eq!(entry.len(), 1, "one pid file per instance");
    let path = entry[0].as_ref().unwrap().path();
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("rec.") && name.ends_with(".pid"),
        "run/<label>.<pid>.pid, got {name:?}"
    );

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("pid="), "{text}");
    assert!(text.contains("label=rec"), "{text}");
    assert!(text.contains("started="), "{text}");
    assert!(text.contains("cmd=echo reg-ok"), "{text}");
    assert!(text.contains("log="), "{text}");

    wait_for_log(home.path(), "rec-", |c| c.contains("reg-ok"));
}

/// DESIGN: "After a kill, registrations whose pids are gone are swept, so ps
/// does not keep a corpse" — a stale pid file (crashed child) is housekept,
/// and ps reports the empty truth with exit 1.
#[test]
fn ps_sweeps_registrations_whose_process_is_gone() {
    let home = Home::new();
    let run = home.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let stale = run.join("stale.4294967294.pid");
    std::fs::write(
        &stale,
        "pid=4294967294\nlabel=stale\nstarted=2020-01-01T00:00:00\ncmd=sleep 99\nlog=/tmp/x.log\n",
    )
    .unwrap();

    home.deemo()
        .args(["ps"])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("no processes started by deemo"));
    assert!(!stale.exists(), "the corpse registration must be swept");
}
