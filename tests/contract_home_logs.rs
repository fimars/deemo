//! Cross-cutting B + C — where files live, how they are named/kept, and how
//! deemo degrades when it cannot log. DESIGN: "`--dir`, which wins" over
//! `$DEEMO_HOME` over `~/.deemo`; "If the log cannot be created, the command
//! still runs and a warning is printed"; README `--yolo`: "No log files".

mod support;

use predicates::prelude::*;
use std::fs;
use support::{log_names, Home};

/// README `--dir`: "Overrides $DEEMO_HOME" — the env var must stay unused.
#[test]
fn dir_flag_overrides_deemo_home_env() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    support::deemo()
        .env("DEEMO_HOME", a.path())
        .args(["--label", "env", "--dir"])
        .arg(b.path()) // --dir wins over $DEEMO_HOME
        .write_stdin("z\n")
        .assert()
        .success();
    assert!(
        !a.path().join("logs").exists(),
        "DEEMO_HOME must stay unused when --dir is given"
    );
    assert_eq!(log_names(b.path()).len(), 1);
}

/// DESIGN: "`$DEEMO_HOME` defaults to `~/.deemo`" — with neither flag nor
/// env, the log appears under `$HOME/.deemo/logs`.
#[cfg(unix)]
#[test]
fn home_defaults_to_dot_deemo_under_home() {
    let fake_home = tempfile::tempdir().unwrap();
    support::deemo()
        .env_remove("DEEMO_HOME")
        .env("HOME", fake_home.path())
        .args(["--label", "dflt"])
        .write_stdin("x\n")
        .assert()
        .success();
    assert_eq!(log_names(&fake_home.path().join(".deemo")).len(), 1);
}

/// An empty `$DEEMO_HOME` is "not set" (a shell-exported empty value must
/// not redirect logs into a directory with no name).
#[cfg(unix)]
#[test]
fn empty_deemo_home_falls_back_to_the_default() {
    let fake_home = tempfile::tempdir().unwrap();
    support::deemo()
        .env("DEEMO_HOME", "")
        .env("HOME", fake_home.path())
        .args(["--label", "dflt"])
        .write_stdin("x\n")
        .assert()
        .success();
    assert_eq!(log_names(&fake_home.path().join(".deemo")).len(), 1);
}

/// README `--timestamp off`: `<label>.log`, later runs append.
#[test]
fn timestamp_off_appends_to_single_file() {
    let home = Home::new();
    for _ in 0..2 {
        home.deemo()
            .args(["--label", "off", "--timestamp", "off"])
            .write_stdin("line\n")
            .assert()
            .success();
    }
    assert_eq!(log_names(home.path()).len(), 1);
    assert_eq!(
        fs::read_to_string(home.path().join("logs").join("off.log")).unwrap(),
        "line\nline\n"
    );
}

/// README `--timestamp day`: `<label>-YYYYmmdd.log`, one file per day,
/// later runs append.
#[test]
fn timestamp_day_names_and_appends_one_file_per_day() {
    let home = Home::new();
    for _ in 0..2 {
        home.deemo()
            .args(["--label", "d", "--timestamp", "day"])
            .write_stdin("x\n")
            .assert()
            .success();
    }
    let names = log_names(home.path());
    assert_eq!(names.len(), 1, "one file per day, got {names:?}");
    let name = &names[0];
    let date = &name["d-".len()..name.len() - ".log".len()];
    assert_eq!(date.len(), 8, "YYYYmmdd expected, got {name:?}");
    assert!(date.bytes().all(|b| b.is_ascii_digit()), "{name:?}");
    assert_eq!(
        fs::read_to_string(home.path().join("logs").join(name)).unwrap(),
        "x\nx\n",
        "later runs append"
    );
}

/// README `--keep N`: "Keep N newest logs for this label" — applied on
/// startup, the file just opened counts and is never the one deleted.
#[test]
fn keep_prunes_old_files() {
    let home = Home::new();
    let logs = home.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    // Seed four old logs (names sort chronologically); --keep 2 must keep the
    // newest seeded file plus the new one.
    for ts in [
        "20200101-000000.000",
        "20200102-000000.000",
        "20200103-000000.000",
        "20200104-000000.000",
    ] {
        fs::write(logs.join(format!("kp-{ts}.log")), "old\n").unwrap();
    }
    home.deemo()
        .args(["--label", "kp", "--keep", "2"])
        .write_stdin("new\n")
        .assert()
        .success();

    let names = log_names(home.path());
    assert_eq!(
        names.len(),
        2,
        "expected newest seeded + new, got {names:?}"
    );
    assert!(names[0].starts_with("kp-20200104"));
    assert_eq!(
        fs::read_to_string(home.path().join("logs").join(&names[0])).unwrap(),
        "old\n"
    );
    assert_eq!(
        fs::read_to_string(home.path().join("logs").join(&names[1])).unwrap(),
        "new\n"
    );
}

/// README: "Keep N newest logs **for this label**" — pruning is scoped; a
/// neighbouring label's history is none of this run's business.
#[test]
fn keep_leaves_other_labels_alone() {
    let home = Home::new();
    let logs = home.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    for ts in [
        "20200101-000000.000",
        "20200102-000000.000",
        "20200103-000000.000",
    ] {
        fs::write(logs.join(format!("kp2-{ts}.log")), "old-kp2\n").unwrap();
    }
    for ts in ["20200101-000000.000", "20200102-000000.000"] {
        fs::write(logs.join(format!("other-{ts}.log")), "old-other\n").unwrap();
    }

    home.deemo()
        .args(["--label", "kp2", "--keep", "1"])
        .write_stdin("new\n")
        .assert()
        .success();

    let names = log_names(home.path());
    let kp2: Vec<_> = names.iter().filter(|n| n.starts_with("kp2-")).collect();
    let other: Vec<_> = names.iter().filter(|n| n.starts_with("other-")).collect();
    assert_eq!(kp2.len(), 1, "keep 1 leaves only the new file: {names:?}");
    assert_eq!(other.len(), 2, "other labels untouched: {names:?}");
    for name in other {
        assert_eq!(
            fs::read_to_string(logs.join(name)).unwrap(),
            "old-other\n",
            "{name} content intact"
        );
    }
}

/// README `--yolo`: "No log files" — the home directory stays empty, while
/// passthrough still works.
#[test]
fn yolo_writes_no_files_at_all() {
    let home = Home::new(); // stays empty
    home.deemo()
        .arg("--yolo")
        .write_stdin("x\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("x"))
        .stderr(predicate::str::contains("yolo"));
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
}

/// DESIGN Logs: pipe mode degrades to "passthrough only" — a disk problem
/// never costs the user their stream, and the warning says exactly what
/// still works.
#[test]
fn pipe_log_failure_degrades_to_passthrough() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "x/y"]) // cannot open logs/x/y-….log
        .write_stdin("still-here\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("still-here"))
        .stderr(predicate::str::contains("cannot open log file"))
        .stderr(predicate::str::contains("passthrough only"));
    // logs/ was created, nothing was written into it.
    assert_eq!(fs::read_dir(home.path().join("logs")).unwrap().count(), 0);
}
