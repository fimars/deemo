# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project adheres to [Semantic Versioning](https://semver.org/).

## [0.1.1] - 2026-09-17

### Fixed

- **`deemo stop` now kills the whole process group, not just the registered
  pid.** Launchers that spawn the real server as a child (e.g. `dshx` →
  `pnpm dsh web`) used to leave the server orphaned and still bound to its
  port: SIGTERM took down only the launcher, and the pid file was removed
  immediately, so the surviving tree became unmanageable. Since detach mode
  starts the child in its own session (`setsid`), the registered pid is the
  group leader — `killpg` now reaches every descendant in one blow.
- `deemo stop` verifies the tree actually died: bounded SIGTERM grace (5 s),
  then SIGKILL escalation (1 s), and only then is the pid file removed. A
  process that survives both is reported honestly (exit code 1, registration
  kept for retry).
- `deemo stop` also sweeps dead registrations up front (previously only
  `deemo ps` did), so stale pid files no longer linger as "is not running".
- Windows: `taskkill /F /T` (tree kill) instead of `/F` only.

## [0.1.0] - 2026-09-16

### Added

- **Detach mode (default)**: `deemo -- <CMD>` runs the command as a background
  daemon in its own session (survives terminal close), stdout+stderr go
  straight to the log file, the shell returns immediately, and a pid registry
  under `$DEEMO_HOME/run/` tracks what is running.
- Management commands: `deemo ps` (list + liveness), `deemo stop <LABEL>`
  (SIGTERM / taskkill), `deemo logs <LABEL>` (newest log + last ~4 KiB).
- Pipe mode detach: `cmd | deemo --background` forks the pump into its own
  session and returns immediately; the pump exits by itself once every pipe
  writer closes (covers upstreams that daemonize while holding the write
  end). Terminal loss degrades to log-only instead of killing the pump.
- Pipe mode: `cmd | deemo` captures the upstream's stdout, passthrough
  byte-exact and real-time.
- Foreground mode: `deemo --foreground -- <CMD>` streams both fds live and
  propagates the child's exit code (128+N for signal deaths).
- Ctrl+C semantics (foreground): first press drains the stream and forwards
  SIGINT to a still-alive child after a short grace period; second press
  force-exits with 130. Downstream EPIPE is graceful (exit 0, like ripgrep).
- Options: `--label`, `--yolo`, `--dir`, `--timestamp off|day|run`,
  `--keep N`, `--quiet`, `--foreground`; `DEEMO_HOME` environment variable.
