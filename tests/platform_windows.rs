//! Windows behavioural coverage — the contracts that only run where the
//! product claims support. Without these, `cargo test` on windows-latest
//! compiled but verified almost nothing, which is how the "sweep deletes
//! every registration because liveness is unknowable" bug survived.
//!
//! Pinned by DESIGN: "liveness is *unknown* there: ps shows `unknown` …
//! registrations are never swept as dead, and stop still signals through
//! taskkill."

#![cfg(windows)]

mod support;

use predicates::prelude::*;
use support::{poll_until, Home};

/// detach → ps (as `unknown`, not `running`, and not hidden) → stop (through
/// taskkill) → ps empty. The round trip every platform must support.
#[test]
fn windows_detach_ps_stop_round_trip() {
    let home = Home::new();
    home.deemo()
        .args(["--label", "winrt", "--", "ping", "-n", "60", "127.0.0.1"])
        .assert()
        .success();

    home.deemo()
        .args(["ps"])
        .assert()
        .success()
        .stdout(predicate::str::contains("winrt"))
        .stdout(predicate::str::contains("unknown")) // no probe: never claims "running"

    ;

    home.deemo()
        .args(["stop", "winrt"])
        .assert()
        .success()
        .stderr(predicate::str::contains("stopped winrt"))
        .stderr(predicate::str::contains("taskkill /F /T"));

    poll_until("the registration to be gone", || {
        let out = home.deemo().args(["ps"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains("no processes")
    });
}

/// README `--background`: "Needs a unix shell" — Windows must say so and
/// exit 1 instead of pretending.
#[test]
fn windows_pipe_background_is_unsupported() {
    let home = Home::new();
    home.deemo()
        .args(["--background"])
        .write_stdin("x\n")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("not supported on this platform"));
}

/// README `-s`: POSIX `kill -s` semantics — no per-process signals on
/// Windows, so it is a usage error (exit 2) with the platform named.
#[test]
fn windows_explicit_signal_is_a_usage_error() {
    let home = Home::new();
    home.deemo()
        .args(["kill", "8000", "-s", "TERM"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("needs a unix platform"));
}
