# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

- `deemo logs <LABEL>` follows the newest file for that label. Name order was
  reversed, so a label with more than one log showed the oldest.

### Added

- **`deemo kill <PORT>...`** — free a port without hunting for the pid:
  discovers every process that *binds* it (TCP + UDP, IPv4 + IPv6) and stops
  it with `deemo stop`'s policy — SIGTERM, 5 s grace, SIGKILL, then an honest
  report if a target survives both. Several ports may be given at once.
  - `-s <SIG>` switches to POSIX `kill -s` semantics: exactly that signal, no
    escalation, so `-s HUP` reloads instead of shutting down
    (`TERM`/`SIGTERM`/`-9`/number all parse).
  - `--dry-run` prints the pids that would be killed, one per line, and
    kills nothing — the `lsof -t` view for scripting.
  - clients merely connected *to* the port are never touched; only the
    processes that bind it are targeted.
  - pids deemo started are signalled as their process group (detach makes
    them session leaders); foreign pids one by one, because their group may
    be the shell's.
  - discovery is native on Linux (`/proc/net/*` + `/proc/<pid>/fd`), via
    `lsof -F pn` on macOS/BSD and `netstat -ano` on Windows.
- `deemo kill` sweeps the registrations of the processes it killed, so
  `deemo ps` stays truthful; a zombie now counts as dead in `ps`/`stop`/
  `kill` liveness checks (it holds no sockets, and signalling it does
  nothing — without the check a successful kill could be reported as
  "survived SIGTERM and SIGKILL").

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
