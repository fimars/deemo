//! ⑥ kill — free a port, whoever holds it, deemo-started or not.
//! README: `deemo kill <PORT>`; "Only processes that *bind* the port
//! locally are targeted (servers), never clients merely connected to it";
//! "`kill` takes a port, including processes deemo did not start."

mod support;

use predicates::prelude::*;
use std::process::Command as StdCommand;
use support::{bind_quiet, free_port, poll_until, Home};

/// README: `--dry-run` "prints the pids that would be killed, one per line".
/// A pid holding two of the given ports appears once — the output is a pid
/// list to feed other tools, not a per-port listing. A released port reads
/// as free and exits 1, like `lsof` with no match.
#[cfg(unix)]
#[test]
fn kill_dry_run_lists_the_pid_binding_the_port() {
    let listener = bind_quiet();
    let port = listener.local_addr().unwrap().port();
    let other = bind_quiet();
    let other_port = other.local_addr().unwrap().port();
    let home = Home::new();
    let expect = format!("{}\n", std::process::id());

    home.deemo()
        .args([
            "kill",
            &port.to_string(),
            &other_port.to_string(),
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::eq(expect.as_str()));

    drop(listener);
    drop(other);
    home.deemo()
        .args(["kill", &port.to_string(), "--dry-run"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("nothing is using port"));
}

/// README: `-s` "that signal only; no SIGKILL afterwards" — `-s HUP` must be
/// a reload, not a shutdown: SIGCONT here is a no-op, the holder survives
/// and is still listed afterwards.
#[cfg(unix)]
#[test]
fn kill_sends_exactly_the_requested_signal_without_escalating() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let home = Home::new();

    home.deemo()
        .args(["kill", &port.to_string(), "-s", "CONT"])
        .assert()
        .success()
        .stderr(predicate::str::contains(format!(
            "sent SIGCONT to pid {}",
            std::process::id()
        )));

    home.deemo()
        .args(["kill", &port.to_string(), "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(std::process::id().to_string()));
}

/// README "Exit codes": `kill`, bad `-s` → 2 (usage error — before anything
/// is signalled).
#[cfg(unix)]
#[test]
fn kill_rejects_unknown_signals_as_a_usage_error() {
    let home = Home::new();
    home.deemo()
        .args(["kill", "8000", "-s", "NOPE"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unknown signal"));
}

/// README "Exit codes": `kill`, no port → 2.
#[test]
fn kill_without_a_port_is_a_usage_error() {
    support::deemo().args(["kill"]).assert().failure().code(2);
}

/// Does python3 exist? (Tests that need a real server skip without it.)
#[cfg(unix)]
fn have_python3() -> bool {
    StdCommand::new("python3").arg("--version").output().is_ok()
}

/// The pid deemo reports for a detach: `deemo: detached [...] (pid N)` is
/// the registered child — the same pid `kill --dry-run` must list.
#[cfg(unix)]
fn detached_child_pid(err: &str) -> u32 {
    err.split("(pid ")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .and_then(|s| s.parse().ok())
        .expect("detach must report the child pid")
}

/// README: kill frees the port AND housekeeping keeps `ps` truthful — a
/// deemo-started holder goes down as a group (detach gave it a private
/// session) and its registration is swept with it.
#[cfg(unix)]
#[test]
fn kill_frees_the_port_and_sweeps_the_managed_pid_file() {
    if !have_python3() {
        eprintln!("skipping: python3 not found, cannot bind a port");
        return;
    }
    let home = Home::new();
    let port = free_port();
    let out = home
        .bin()
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
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());

    // wait until the server actually holds the port
    poll_until("http.server to bind the port", || {
        let found = home
            .deemo()
            .args(["kill", &port.to_string(), "--dry-run"])
            .output()
            .unwrap();
        !found.stdout.is_empty()
    });

    home.deemo()
        .args(["kill", &port.to_string()])
        .assert()
        .success()
        .stderr(predicate::str::contains("stopped killee"));

    // the port is free again…
    poll_until("the port to become free", || {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    });
    // …and its registration went with it.
    home.deemo()
        .args(["ps"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("no processes"));
}

/// Several ports at once: the occupied one goes, the free one is reported,
/// and the exit code is 1 because *something asked for did not happen*
/// (README: "`kill`: port free … → 1").
#[cfg(unix)]
#[test]
fn kill_mixed_ports_stills_kill_and_reports_the_free_one() {
    let held = bind_quiet();
    let held_port = held.local_addr().unwrap().port();
    let free = free_port(); // released for us — nobody holds it
    let home = Home::new();

    home.deemo()
        .args([
            "kill",
            &free.to_string(),
            &held_port.to_string(),
            "-s",
            "CONT", // no-op signal: the holder is this very test process
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(format!(
            "nothing is using port {free}"
        )))
        .stderr(predicate::str::contains(format!(
            "sent SIGCONT to pid {}",
            std::process::id()
        )));
    // "held" is still alive and still bound (CONT is a no-op by design)
    assert_eq!(held.local_addr().unwrap().port(), held_port);
}

/// DESIGN: "Only sockets whose **local** address is that port are targets …
/// A client connected *to* the port has some other local port and is left
/// alone." Live proof: discovery sees both sides of the connection, deemo
/// keeps the binder. Skipped without python3.
#[cfg(unix)]
#[test]
fn kill_targets_binders_not_clients_connected_to_the_port() {
    if !have_python3() {
        eprintln!("skipping: python3 not found");
        return;
    }
    let home = Home::new();
    let port = free_port();
    let out = home
        .bin()
        .args([
            "--label",
            "binder",
            "--",
            "python3",
            "-m",
            "http.server",
            &port.to_string(),
            "--bind",
            "127.0.0.1",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    let server_pid = detached_child_pid(&String::from_utf8_lossy(&out.stderr));

    poll_until("http.server to bind the port", || {
        let found = home
            .deemo()
            .args(["kill", &port.to_string(), "--dry-run"])
            .output()
            .unwrap();
        !found.stdout.is_empty()
    });

    // A client connects TO the port (local side ephemeral) and reports back
    // through a marker file — no reliance on lsof being installed.
    let marker = home.path().join("client-connected");
    let mut client = StdCommand::new("python3")
        .args([
            "-c",
            &format!(
                "import socket,time; \
                 s=socket.create_connection(('127.0.0.1',{port}),2); \
                 open({marker:?},'w').write('ok'); time.sleep(30)"
            ),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    poll_until("the client to connect to the port", || marker.exists());

    let out = home
        .deemo()
        .args(["kill", &port.to_string(), "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "port is held: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let listed: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        listed,
        vec![server_pid.to_string().as_str()],
        "only the binder is a target, never the client (pid {})",
        client.id()
    );

    // Cleanup: server through deemo's group stop, client killed directly.
    home.deemo().args(["stop", "binder"]).assert().success();
    let _ = client.kill();
    let _ = client.wait();
}
