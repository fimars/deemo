# deemo

**daemonize with housekeeping** — start any command as a background daemon with
its output persisted to `~/.deemo/logs/`, and get your shell back immediately.

```bash
deemo -- python -m http.server 8000
```

```
deemo: detached ["python", "-m", "http.server", "8000"] (pid 71630)
deemo: log: /Users/you/.deemo/logs/python-20260916-111527.491.log
deemo: stop with: deemo stop python
$
```

The server keeps running in its own session (survives terminal close), stdout
and stderr both land in the log file.

## Install

No crates.io release — install straight from this repository:

```bash
# from git (requires the Rust toolchain)
cargo install --git https://github.com/fimars/deemo --locked

# or, without a Rust toolchain: grab a prebuilt binary from GitHub Releases
#   https://github.com/fimars/deemo/releases

# or from a local clone
cargo install --path .
```

## Modes

### Detach (default) — `deemo -- <CMD>`

- runs the command **in the background, in its own session** (`setsid` on unix,
  `DETACHED_PROCESS` on Windows): closing the terminal or pressing Ctrl+C does
  not touch it
- child's stdout **and** stderr go straight to the log file (built-in `2>&1`)
- deemo returns to the shell immediately and prints pid, log path and the
  matching `stop` command
- stdin is connected to /dev/null

### Management

```bash
deemo ps                 # list started processes (label, pid, alive, log)
deemo stop <LABEL>       # SIGTERM to the whole process group (SIGKILL after a
                         # grace period) — launcher + real server + all
                         # descendants die together; pid file removed only
                         # once the tree is confirmed gone
deemo logs <LABEL>       # newest log file of a label: path + last ~4 KiB
```

`stop` targets the **process group**, not a single pid: detach mode puts the
child in its own session (`setsid`), so the registered pid is the group
leader and every descendant that did not daemonize itself away is stopped
with it. Launchers that spawn the actual workload (deno/npm/pnpm wrappers)
are therefore stopped completely. A command that deliberately double-forks
into its own session escapes any process-group supervisor — deemo included —
and is the author's responsibility.

### Foreground — `deemo --foreground -- <CMD>`

Stay attached, like a smart `tee`: live passthrough + logging, Ctrl+C drains
the stream so the child's dying words land in the log, and the **child's exit
code becomes `$?`**. Use it for interactive debugging.

### Pipe — `cmd | deemo`

The classic unix way for stdin. Only stdout physically flows through a pipe,
so add `2>&1` if you want stderr captured (POSIX limitation, not deemo's).

⚠️ **Pipe mode stays attached** — it is a filter in your pipeline. POSIX
pipes only signal EOF when **every writer closes**, so if your command
daemonizes in the background (spawns a server and exits), the pipe writer
fd survives inside the daemon and deemo keeps waiting. That waiting is
announced on startup. Two ways out:

- **you wanted a daemon? use detach mode instead**: `deemo -- <CMD>`
- **detach the pump itself**: `dshx | deemo --background` — deemo forks into
  its own session immediately and keeps reading the pipe (and logging) until
  every writer closes; your shell returns at once. It shows up in `deemo ps`
  and is stopped with `deemo stop <LABEL>`. If the terminal later closes, it
  silently drops passthrough and keeps logging. (unix only)
- plain shell backgrounding also works: `dshx | deemo &`

> **Buffering applies everywhere**: when stdout is not a tty (a pipe, or the
> log file in detach mode) many programs switch to 4–8 KB block buffering.
> Fix it at the source: `python -u ...`, `PYTHONUNBUFFERED=1`, or
> `stdbuf -oL <cmd>`.

## Options

| Option | Meaning |
|---|---|
| `--label <NAME>` | Log filename prefix + stop key (default: child's program name, `session` in pipe mode) |
| `--yolo` | Write **no** log files (detach: output to /dev/null) |
| `--dir <DIR>` | Log directory; overrides `$DEEMO_HOME` |
| `--timestamp <off\|day\|run>` | File naming: `<label>.log` / `<label>-YYYYmmdd.log` / `<label>-YYYYmmdd-HHMMSS.mmm.log` (default `run`) |
| `--keep <N>` | Keep at most N newest files per label on startup (0 = unlimited) |
| `--background` | Pipe mode only: detach the pump (see Pipe section) |
| `--foreground` | Stay attached (see above); adds `--quiet` (log only, no echo) |
| `--quiet` | Foreground/pipe only: do not echo to stdout |

Environment: `DEEMO_HOME` — deemo root (default `~/.deemo`); logs live in
`logs/`, pid registry in `run/`.

## Exit codes

| Case | Exit code |
|---|---|
| Detach mode, spawned fine | `0` (the child's fate is deemo's business no longer) |
| Command not found | `127` |
| Foreground / pipe mode | child's own exit code (signal death N → `128+N`) |
| Pipe mode, Ctrl+C | `130` |
| Pipe mode, downstream closed (`... \| deemo \| head`) | `0` — graceful termination, like ripgrep |

## Examples

```bash
# daemonize a server, get the shell back
deemo -- ./my-server --port 8080

# named instance, only keep the 10 most recent logs
deemo --label server --keep 10 -- ./my-server

# what is it doing right now?
deemo logs server

# done with it
deemo stop server

# foreground: watch it live, get its exit code when it ends
deemo --foreground -- ./my-server

# classic pipe, stderr included
python -m http.server 8000 2>&1 | deemo

# custom location
DEEMO_HOME=/var/log/deemo deemo -- ./my-server
```

## Notes

- In detach mode, Ctrl+C after starting only affects the (already exited)
  launcher — the daemon does not receive it. To stop it: `deemo stop <label>`.
- Merged stdout/stderr line ordering is non-deterministic (same as shell
  `2>&1`).
- On Windows, `deemo stop` uses `taskkill /F /T` (hard tree stop); liveness in
  `deemo ps` is unix-precise, best-effort elsewhere.
- Names collide between management commands and child programs? Always invoke
  children with `--`: `deemo -- ps -ef` runs `ps`, while `deemo ps` lists
  managed processes.

## Development

```bash
cargo test                     # 25 integration tests (process lifecycle, signals, IO)
scripts/e2e-tmux.sh            # end-to-end smoke through a real tmux terminal:
                               # daemonize `python3 -m http.server`, verify HTTP
                               # 200, `ps`, stderr capture in `logs`, `stop`,
                               # tty usage guard, and terminal-close survival
```

## Platform notes

Works on macOS, Linux and Windows. `~` resolves per-OS home directory.

## License

MIT
