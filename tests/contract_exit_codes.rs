//! Cross-cutting A — the README "Exit codes" table as code.
//!
//! Every row of the table is pinned somewhere; this file owns the rows that
//! belong to no single job, and maps the rest:
//!
//! | README row | pinned by |
//! |---|---|
//! | Detach spawned → 0            | job_detach::detach_returns_before_the_child_finishes |
//! | Command not found → 127       | job_detach::detach_command_not_found_is_127, job_foreground::foreground_command_not_found_is_127 |
//! | Foreground: child's code      | job_foreground::foreground_merges_both_fds_and_returns_child_code |
//! | Foreground: signal N → 128+N  | job_foreground::foreground_returns_signal_deaths_as_128_plus_n |
//! | Pipe, EOF → 0                 | job_pipe::pipe_upstream_failure_still_exits_zero |
//! | Pipe, Ctrl+C → 130            | contract_signals::pipe_sigint_drains_then_exits_130_on_eof |
//! | Pipe, `\| head` → 0           | job_pipe::pipe_downstream_broken_pipe_exits_zero |
//! | ps, nothing running → 1       | ps_with_nothing_running_exits_1 (below) |
//! | stop, no target matched → 1   | job_stop::stop_reports_an_unknown_pid |
//! | logs, no log → 1              | logs_without_a_file_for_the_label_exits_1 (job_logs) |
//! | kill: free port / survived → 1| job_kill::kill_dry_run_lists_the_pid_binding_the_port, kill_mixed_ports_… |
//! | kill: bad -s / no port → 2    | job_kill::kill_rejects_unknown_signals…, kill_without_a_port… |
//! | Bare `deemo` in a terminal → 2| scripts/e2e-tmux.sh T1 (needs a real tty) |

mod support;

use predicates::prelude::*;

/// README row: `ps` with nothing running is 1 — "did not do what was asked"
/// without being an error message at the user.
#[test]
fn ps_with_nothing_running_exits_1() {
    let home = support::Home::new();
    home.deemo()
        .args(["ps"])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("no processes started by deemo"));
}
