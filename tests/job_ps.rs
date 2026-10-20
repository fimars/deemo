//! ④ ps — "the manager": what deemo started, and whether it still runs.
//! README: `deemo ps # label, pid, status, age, port, command`; exit `1`
//! when nothing is running; DESIGN: registrations live in
//! `run/<label>.<pid>.pid`.

#![cfg(unix)]

mod support;

use predicates::prelude::*;
use support::{poll_until, wait_for_log, Home};

/// README: ps lists label, pid, status ("running"), age, port, and the
/// command; the log path stays behind `ps -v` / `deemo logs <LABEL>`.
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
        .stdout(predicate::str::contains("AGE"))
        .stdout(predicate::str::contains("PORT"))
        .stdout(predicate::str::contains("listed"))
        .stdout(predicate::str::contains("running"))
        .stdout(predicate::str::contains("sleep 30"));

    home.deemo().args(["stop", "listed"]).assert().success();
}

/// README: `deemo ps -v` brings the log path and the absolute start time
/// back (the default table trades them for the relative age).
#[test]
fn ps_verbose_shows_log_path_and_absolute_start() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "vrb", "--", "sleep", "30"])
        .assert()
        .success();

    home.deemo()
        .args(["ps", "-v"])
        .assert()
        .success()
        .stdout(predicate::str::contains("STARTED"))
        .stdout(predicate::str::contains("COMMAND -> LOG"))
        .stdout(predicate::str::contains(".log"))
        .stdout(predicate::str::contains(" -> "));

    home.deemo().args(["stop", "vrb"]).assert().success();
}

/// A registered command that names a port (`--port N`, `--port=N`, `-p N`)
/// gets a PORT column entry — the dev-server identity at a glance.
#[test]
fn ps_extracts_the_port_from_the_command_line() {
    let home = Home::new();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let run = home.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(
        run.join(format!("svc.{}.pid", child.id())),
        format!(
            "pid={}\nlabel=svc\nstarted=2020-01-01T00:00:00\ncmd=node server.mjs --port 8123\nlog=/tmp/svc.log\n",
            child.id()
        ),
    )
    .unwrap();

    home.deemo()
        .args(["ps"])
        .assert()
        .success()
        .stdout(predicate::str::contains("svc"))
        .stdout(predicate::str::contains("8123"));

    child.kill().unwrap();
    child.wait().unwrap();
}

/// Rows fit the terminal width: COLUMNS travels into pipes (where no tty
/// exists to ask), the long command is cut with an ellipsis, and no printed
/// line exceeds the budget.
#[test]
fn ps_truncates_rows_to_the_columns_budget() {
    let home = Home::new();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let run = home.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(
        run.join(format!("wide.{}.pid", child.id())),
        format!(
            "pid={}\nlabel=wide\nstarted=2020-01-01T00:00:00\ncmd=bash -lc cd /very/deep/path && exec pnpm --filter some-admin dev --host 127.0.0.1 --port 8181 --strictPort\nlog=/tmp/wide.log\n",
            child.id()
        ),
    )
    .unwrap();

    let out = home
        .deemo()
        .env("COLUMNS", "60")
        .args(["ps"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains('…'),
        "the long command must be cut: {stdout}"
    );
    for line in stdout.lines() {
        assert!(
            line.chars().count() <= 60,
            "line wider than COLUMNS=60 ({width}): {line:?}",
            width = line.chars().count()
        );
    }

    child.kill().unwrap();
    child.wait().unwrap();
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
