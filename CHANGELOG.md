# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- Every command is faster, measured by `scripts/bench.py` against a release
  build (200k lines / 13.5 MB, Apple M4). Piping through deemo: 427 → 19 ms
  (`--quiet` 305 → 20, `--yolo` 100 → 18); `--foreground`: 435 → 28 ms. The
  pump moves 64 KiB slabs instead of paying three syscalls per line, and
  flushes before the read that could block, so a paused writer still sees its
  line immediately. Foreground still merges whole lines.
- `kill <PORT>` answers from the process table instead of an `lsof` exec on
  macOS, and one scan covers every port in the argument list: a 10-port
  `--dry-run` is 264 → 7 ms. `lsof` stays as the fallback if the table cannot
  be read.
- `stop` and `kill` see a cooperative exit in ~1.5 ms instead of ~55 ms: the
  escalation poll starts at 0.5 ms and doubles to the old 50 ms. The 5 s and
  1 s windows are unchanged.
- `ps` and `stop` read the registry once instead of twice, `ps` reads the
  clock once per table and writes the table in one write: 4000 registrations
  78 → 33 ms (16 → 8 µs per entry), `stop` with an absent label over 400
  registrations 15 → 10 ms.
- `ps` is readable again. The table now sizes its columns to the
  content (a label longer than the old hard-coded 16 chars collapsed
  every column after it), shows the age relative to now (`3d`, `5h`)
  instead of the 19-character absolute stamp, and extracts the port
  from the recorded command line (`--port N`, `--port=N`, `-p N`,
  `PORT=N`) into its own column. The log path left the default table —
  `deemo logs <LABEL>` resolves it; `ps -v` still prints it, with the
  absolute start time. Long commands are cut with an ellipsis to the
  terminal width (`COLUMNS` wins over the tty, piped output is left
  uncapped), and a tty gets a colored status.
- The suite is green on all three CI platforms. Three latent test defects:
  the `/proc/net` parser fixture used `0x1F90` (8080) while claiming 8000 —
  and was gated to Linux, so no other platform ever ran it; `job_kill`
  imported unix-only helpers unconditionally, which failed the Windows
  build; and the macOS binder test plus the tmux e2e raced `python3 -m
  http.server`'s `getfqdn()` reverse-DNS lookup between `bind()` and
  `listen()` — on CI runners that lookup stalls for up to a minute, so the
  tests measured the runner's DNS instead of deemo. The e2e workload is now
  `scripts/e2e-server.py` (bind, listen, same stderr request log), the
  binder test listens from the test process itself, and e2e failures dump
  the captured logs.
- Three more races inside the suite itself: `job_detach`'s session test read
  the log before the child had written its second line; `kill --dry-run`
  demanded an *exact* pid list while a sibling test's mid-spawn child still
  carried an inherited copy of the listener fd — the kernel reports that
  holder truthfully (as `lsof`/`fuser` would); and the same test dropped its
  listener into the pool every `bind_quiet` scan starts from, so a late
  sibling could re-grab the port before the "reads as free" check. The
  assertions now pin the contract instead: the binder listed exactly once, a
  released port read as free (exit 1), and a client connected to the port
  never listed — with the free-check's ports drawn from a pool no other test
  scans.

### Fixed

- `kill <PORT>` no longer reports "survived SIGTERM and SIGKILL" for a
  process that is already dead. On macOS libproc answers `ESRCH` for a
  zombie, so the probe read a corpse as alive and the command burned the
  full 5 s + 1 s grace before exiting 1 (with the process long gone).
- `kill` reads a port only from TCP and UDP sockets. The value at the same
  offset in a unix-domain or kernel-control socket is unrelated data — a
  zero there made `kill 0` match every kernel-control socket.

## [0.2.0] - 2026-09-22

### Fixed

- `deemo logs <LABEL>` follows the newest file for that label. Name order was
  reversed, so a label with more than one log showed the oldest.
- **Windows: a management command no longer deletes the whole registry.**
  There is no cheap std-only liveness probe on Windows, so every process
  counted as dead while `ps`/`stop`/`kill` swept dead registrations up front
  — the first `deemo ps` emptied `$DEEMO_HOME/run/`. Liveness is now
  three-state: `ps` shows `unknown` (never a guessed `dead`/`running`), only
  a *known*-dead registration is swept, and `stop` still signals through
  `taskkill /F /T`.
- `deemo -- <CMD>` (and `--background`) no longer print `stop with: ...` when
  nothing was registered — `--yolo` and a failed log open leave no pid file,
  so the advertised command was guaranteed to exit 1.
- Docs corrected where they contradicted the behaviour (each now pinned by a
  test): pipe mode exits `0` on EOF rather than the upstream's status;
  `ps`/`stop`/`logs` exit `1` on empty results; `--yolo` documents that it
  registers nothing; a purely numeric label can only be stopped by pid.

### Added

- **`deemo stop` accepts a pid as well as a label.** A label stops every
  process registered under it; a purely numeric argument stops exactly the
  process with that pid (the one `deemo ps` prints). Labels default to the
  program name, so two runs of the same launcher (`pnpm dev:admin`,
  `pnpm dev:merchant`) share one — the pid is how to stop just one.
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

### Changed

- The test suite is split by user job (`tests/job_<feature>.rs`) with
  cross-cutting contracts in `tests/contract_*.rs` (exit codes, SIGINT,
  `$HOME`/log rules) and Windows coverage in `tests/platform_windows.rs`,
  sharing `tests/support/`. New: four SIGINT scenarios and the Windows
  `detach → ps → stop` round-trip. Every test pins the README/DESIGN
  sentence it enforces; fixed wall-clock assertions that flaked on slow
  spawns.
- The tmux end-to-end script declares its scope — only what a real tty can
  prove — and gained a real Ctrl+C keystroke test; CI now runs it on macOS
  as well as Linux.

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
