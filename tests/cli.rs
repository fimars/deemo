use std::fs;
use std::io::Write as _;
use std::process::Stdio;

use assert_cmd::Command;
use predicates::prelude::*;
use std::process::Command as StdCommand;

fn deemo() -> Command {
    Command::cargo_bin("deemo").unwrap()
}

fn log_names(home: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(home.join("logs"))
        .unwrap_or_else(|_| panic!("logs dir must exist under {}", home.display()))
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Poll the log file of `prefix` until `check` holds, or fail after 5s.
fn wait_for_log<F: Fn(&str) -> bool>(home: &std::path::Path, prefix: &str, check: F) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(content) = log_names(home)
            .iter()
            .find(|n| n.starts_with(prefix))
            .and_then(|n| fs::read_to_string(home.join("logs").join(n)).ok())
        {
            if check(&content) {
                return content;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for log content of {prefix:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn read_single_log(home: &std::path::Path, prefix: &str) -> String {
    let names = log_names(home);
    assert_eq!(names.len(), 1, "expected one log file, got {names:?}");
    assert!(
        names[0].starts_with(prefix),
        "log {names:?} must start with {prefix:?}"
    );
    fs::read_to_string(home.join("logs").join(&names[0])).unwrap()
}

// --- pipe mode ---------------------------------------------------------------

#[test]
fn passes_through_and_writes_log() {
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["--label", "t1"])
        .write_stdin("hello\nworld\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello\nworld\n"))
        .stderr(predicate::str::contains(
            "pipe mode: waiting for stdin to close",
        ));

    let content = read_single_log(home.path(), "t1-");
    assert_eq!(content, "hello\nworld\n");
}

#[test]
fn passes_through_large_stream_intact() {
    let home = tempfile::tempdir().unwrap();
    let input: String = (0..100_000).map(|i| format!("{i}\n")).collect();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["--label", "big", "--timestamp", "off"])
        .write_stdin(input.as_bytes())
        .assert()
        .success()
        .stdout(predicate::eq(input.as_str()));
    assert_eq!(
        fs::read_to_string(home.path().join("logs").join("big.log")).unwrap(),
        input
    );
}

#[test]
fn broken_pipe_downstream_is_graceful_zero() {
    // `... | deemo | head` must NOT be an error (ripgrep's convention:
    // a broken pipe means graceful termination, exit 0, no stderr noise).
    let home = tempfile::tempdir().unwrap();
    let mut child = StdCommand::new(env!("CARGO_BIN_EXE_deemo"))
        .env("DEEMO_HOME", home.path())
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

#[test]
fn yolo_keeps_no_files() {
    let home = tempfile::tempdir().unwrap(); // stays empty
    deemo()
        .env("DEEMO_HOME", home.path())
        .arg("--yolo")
        .write_stdin("x\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("x"))
        .stderr(predicate::str::contains("yolo"));
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
}

#[test]
fn deemo_home_env_and_dir_override() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    deemo()
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

#[test]
fn timestamp_off_appends_to_single_file() {
    let home = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        deemo()
            .env("DEEMO_HOME", home.path())
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

#[test]
fn timestamp_day_naming() {
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["--label", "d", "--timestamp", "day"])
        .write_stdin("x\n")
        .assert()
        .success();
    let name = &log_names(home.path())[0];
    let date = &name["d-".len()..name.len() - ".log".len()];
    assert_eq!(date.len(), 8, "YYYYmmdd expected, got {name:?}");
    assert!(date.bytes().all(|b| b.is_ascii_digit()), "{name:?}");
}

#[test]
fn keep_prunes_old_files() {
    let home = tempfile::tempdir().unwrap();
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
    deemo()
        .env("DEEMO_HOME", home.path())
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

#[test]
fn quiet_writes_log_without_echo() {
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["--label", "q", "--quiet"])
        .write_stdin("secret\n")
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    assert_eq!(read_single_log(home.path(), "q-"), "secret\n");
}

#[test]
fn handles_missing_trailing_newline() {
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["--label", "tail", "--timestamp", "off"])
        .write_stdin("no-newline")
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(home.path().join("logs").join("tail.log")).unwrap(),
        "no-newline"
    );
}

// --- supervisor mode ----------------------------------------------------------

#[test]
#[cfg(unix)]
fn supervisor_captures_both_fds_and_propagates_exit_code() {
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
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

#[test]
#[cfg(unix)]
fn supervisor_waits_for_both_fds() {
    // stdout closes immediately, stderr keeps emitting after a pause: deemo
    // must not quit before the stderr reader hits EOF, and must report the
    // child's exit code.
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
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

#[test]
#[cfg(unix)]
fn supervisor_propagates_signal_exit_codes() {
    let home = tempfile::tempdir().unwrap();
    // child dies by SIGINT -> 128+2, by SIGTERM -> 128+15
    deemo()
        .env("DEEMO_HOME", home.path())
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
    deemo()
        .env("DEEMO_HOME", home.path())
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

#[test]
#[cfg(unix)]
fn supervisor_yolo_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args([
            "--yolo",
            "--foreground",
            "--",
            "sh",
            "-c",
            "echo hi; exit 2",
        ])
        .assert()
        .failure()
        .code(2)
        .stdout(predicate::str::contains("hi"))
        .stderr(predicate::str::contains("yolo"));
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
}

// --- detach mode (default) ----------------------------------------------------

#[test]
#[cfg(unix)]
fn detach_returns_immediately_and_child_persists() {
    let home = tempfile::tempdir().unwrap();
    let t0 = std::time::Instant::now();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args([
            "--label",
            "dt",
            "--",
            "sh",
            "-c",
            "sleep 0.6; echo done; exit 5",
        ])
        .assert()
        .success(); // does NOT wait for the child
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(400),
        "detach must return immediately"
    );
    // the child outlives deemo and keeps writing the log
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let content = read_single_log(home.path(), "dt-");
    assert_eq!(content, "done\n");
}

#[test]
#[cfg(unix)]
fn detach_child_runs_in_its_own_session() {
    // setsid => the child becomes a session leader, so its pgid equals its pid.
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
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
    let content = wait_for_log(home.path(), "sid-", |c| c.contains("pid="));
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

#[test]
#[cfg(unix)]
fn ps_and_stop_manage_detached_processes() {
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["--label", "lifecycle", "--", "sleep", "30"])
        .assert()
        .success();

    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["ps"])
        .assert()
        .success()
        .stdout(predicate::str::contains("lifecycle"))
        .stdout(predicate::str::contains("running"));

    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["stop", "lifecycle"])
        .assert()
        .success()
        .stderr(predicate::str::contains("stopped"));

    // after stop: not running anymore, pid file swept
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["ps"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("no processes"));
}

#[test]
#[cfg(unix)]
fn stop_kills_the_whole_process_group_not_just_the_leader() {
    // Regression for `deemo stop` leaving the real server alive: a launcher
    // like dshx spawns the actual workload as a child (here: a shell with two
    // background sleeps standing in for pnpm -> node -> bound port). Killing
    // only the registered pid would orphan the rest; the stop must take down
    // the whole group deemo's setsid created.
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
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

    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["stop", "tree"])
        .assert()
        .success()
        .stderr(predicate::str::contains("stopped"));

    // No member of the tree may survive (pgrep prints nothing once all gone).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let out = StdCommand::new("pgrep")
            .args(["-f", "sleep 6[12]\\.73[1]"])
            .output()
            .unwrap();
        if out.stdout.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "group members survived deemo stop: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // ... and the pid file is gone with them.
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["ps"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("no processes"));
}

#[test]
#[cfg(unix)]
fn stop_sweeps_dead_registrations_instead_of_keeping_them() {
    // A stale pid file (crashed child, or a pre-0.1.1 leftover) must be
    // removed by `stop` too, not just by `ps`.
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["--label", "gone", "--", "sh", "-c", "exit 0"])
        .assert()
        .success();
    std::thread::sleep(std::time::Duration::from_millis(300)); // child exits

    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["stop", "gone"])
        .assert()
        .code(1); // label no longer exists -> "no process with label"
    assert!(home.path().join("run").read_dir().unwrap().next().is_none());
}

#[test]
#[cfg(unix)]
fn detach_default_label_is_program_name() {
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["--", "sh", "-c", "exit 0"])
        .assert()
        .success();
    let names = log_names(home.path());
    assert_eq!(names.len(), 1);
    assert!(names[0].starts_with("sh-"), "got {names:?}");
}

#[test]
#[cfg(unix)]
fn detach_command_not_found_is_127() {
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["--", "definitely-not-a-real-cmd-xyz"])
        .assert()
        .failure()
        .code(127);
}

#[test]
#[cfg(unix)]
fn logs_shows_newest_log_of_label() {
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["--label", "lg", "--", "sh", "-c", "echo hello-from-detach"])
        .assert()
        .success();
    wait_for_log(home.path(), "lg-", |c| c.contains("hello-from-detach"));
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["logs", "lg"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello-from-detach"));
}

// --- kill by port ---------------------------------------------------------------

/// A port nothing else holds: bind to an ephemeral port, then release it.
#[cfg(unix)]
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[test]
#[cfg(unix)]
fn kill_dry_run_lists_the_pid_binding_the_port() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // A second port held by the same process must be reported once: dry-run
    // output is a pid list to feed to other tools, not a per-port listing.
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other_port = other.local_addr().unwrap().port();
    let home = tempfile::tempdir().unwrap();
    let expect = format!("{}\n", std::process::id());

    deemo()
        .env("DEEMO_HOME", home.path())
        .args([
            "kill",
            &port.to_string(),
            &other_port.to_string(),
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::eq(expect.as_str()));

    // a released port reads as free (and exits 1, like lsof with no match)
    drop(listener);
    drop(other);
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["kill", &port.to_string(), "--dry-run"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("nothing is using port"));
}

#[test]
#[cfg(unix)]
fn kill_sends_exactly_the_requested_signal_without_escalating() {
    // SIGCONT to a running process is a no-op, so this test process — which
    // holds the port — must survive `-s CONT`: no SIGTERM→SIGKILL upgrade.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let home = tempfile::tempdir().unwrap();

    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["kill", &port.to_string(), "-s", "CONT"])
        .assert()
        .success()
        .stderr(predicate::str::contains(format!(
            "sent SIGCONT to pid {}",
            std::process::id()
        )));

    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["kill", &port.to_string(), "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(std::process::id().to_string()));
}

#[test]
#[cfg(unix)]
fn kill_rejects_unknown_signals_as_a_usage_error() {
    let home = tempfile::tempdir().unwrap();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["kill", "8000", "-s", "NOPE"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unknown signal"));
}

#[test]
fn kill_without_a_port_is_a_usage_error() {
    deemo().args(["kill"]).assert().failure().code(2);
}

#[test]
#[cfg(unix)]
fn kill_frees_the_port_and_sweeps_the_managed_pid_file() {
    // The e2e workload: a real listener, started through deemo so it is also
    // a managed pid (kill must then use the group, like `stop`, and sweep the
    // registration). Skipped — not failed — without python3.
    if StdCommand::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: python3 not found, cannot bind a port");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let port = free_port();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args([
            "--label",
            "killee",
            "--",
            "python3",
            "-m",
            "http.server",
            &port.to_string(),
            "--bind",
            "127.0.0.1",
        ])
        .assert()
        .success();

    // wait until the server actually holds the port
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let found = deemo()
            .env("DEEMO_HOME", home.path())
            .args(["kill", &port.to_string(), "--dry-run"])
            .output()
            .unwrap();
        if !found.stdout.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "http.server never bound port {port}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["kill", &port.to_string()])
        .assert()
        .success()
        .stderr(predicate::str::contains("stopped killee"));

    // the port is free again…
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "port {port} still bound after kill"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // …and its registration went with it (housekeeping reuse).
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["ps"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("no processes"));
}

// --- pipe mode + --background -------------------------------------------------

#[test]
#[cfg(unix)]
fn pipe_background_returns_immediately_and_captures_stream() {
    // The exact user scenario: upstream is a wrapper that daemonizes
    // something in the background, keeping the pipe write end open.
    let home = tempfile::tempdir().unwrap();
    let t0 = std::time::Instant::now();
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["--label", "pb", "--background"])
        .write_stdin("captured-line\n") // stdin closed after write
        .assert()
        .success();
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(2),
        "--background must return immediately"
    );
    let content = wait_for_log(home.path(), "pb-", |c| c.contains("captured-line"));
    assert!(content.contains("captured-line"));
}

#[test]
#[cfg(unix)]
fn pipe_background_survives_open_write_ends_and_is_manageable() {
    // A writer that stays open keeps the detached pump alive (visible in ps);
    // closing the last writer ends it.
    let home = tempfile::tempdir().unwrap();
    let mut child = StdCommand::new(env!("CARGO_BIN_EXE_deemo"))
        .env("DEEMO_HOME", home.path())
        .args(["--label", "pump", "--background"])
        .stdin(Stdio::piped()) // our write end stays open below
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    // try_wait: does NOT close our stdin (unlike wait()), keeping the write end open
    let code = loop {
        match child.try_wait().unwrap() {
            Some(status) => break status.code(),
            None => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "--background must return immediately"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    };
    assert_eq!(code, Some(0), "must return at once");

    // write end still open -> pump alive -> ps lists it
    deemo()
        .env("DEEMO_HOME", home.path())
        .args(["ps"])
        .assert()
        .success()
        .stdout(predicate::str::contains("pump"))
        .stdout(predicate::str::contains("running"));

    // last writer closes -> pump reads EOF and exits on its own
    drop(child.stdin.take());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let out = deemo()
            .env("DEEMO_HOME", home.path())
            .args(["ps"])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&out.stdout).contains("no processes") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pump must exit when all writers close"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
