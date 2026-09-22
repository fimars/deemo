# deemo

Run a command in the background and keep its output in `~/.deemo`.

```bash
deemo -- python -m http.server 8000
```

```
deemo: detached ["python", "-m", "http.server", "8000"] (pid 71630)
deemo: log: /Users/you/.deemo/logs/python-20260916-111527.491.log
deemo: stop with: deemo stop python
```

Closing the terminal leaves it running. stdout and stderr both go to the log.

## Install

```bash
cargo install --git https://github.com/fimars/deemo --locked
```

Prebuilt binaries: [GitHub Releases](https://github.com/fimars/deemo/releases).
From a clone: `cargo install --path .`.

## Commands

```bash
deemo -- <CMD>                 # detach (default): own session, shell returns now
deemo --foreground -- <CMD>    # stay attached; $? is the child's exit code
<cmd> | deemo                  # pipe filter; stdout only (add 2>&1 for stderr)
<cmd> | deemo --background     # detach the pipe pump (unix)

deemo ps                       # label, pid, status, log
deemo stop <LABEL>             # stop it (and what it spawned)
deemo logs <LABEL>             # newest log: path + last ~4 KiB
deemo kill <PORT>              # free a port
deemo kill 8000 3000           # several ports
deemo kill 8000 --dry-run      # print pids, one per line
deemo kill 8000 -s HUP         # that signal only; no SIGKILL afterwards
```

`stop` takes a label. `kill` takes a port, including processes deemo did not start.

## Options

| Option | Meaning |
|---|---|
| `--label <NAME>` | Log prefix and `stop` key. Default: program name, or `session` in pipe mode |
| `--timestamp <off\|day\|run>` | `<label>.log`, `<label>-YYYYmmdd.log`, or one file per run (default `run`) |
| `--keep <N>` | Keep N newest logs for this label (0 = unlimited) |
| `--dir <DIR>` | Overrides `$DEEMO_HOME` |
| `--yolo` | No log files |
| `--quiet` | Log only, no echo (foreground and pipe) |
| `-F`, `--foreground` | Stay attached |
| `-B`, `--background` | Pipe mode: detach the pump |

`$DEEMO_HOME` defaults to `~/.deemo`. Logs are in `logs/`, pid files in `run/`.

## Exit codes

| | |
|---|---|
| Detach spawned | `0` |
| Command not found | `127` |
| Foreground / pipe | child's code; signal N → `128+N` |
| Pipe, Ctrl+C | `130` |
| Pipe, downstream closed (`\| head`) | `0` |
| `kill`: port free, or target survived | `1` |
| `kill`: bad `-s`, or no port | `2` |
| Bare `deemo` in a terminal | `2` |

## Gotchas

- Stop a daemon with `deemo stop <label>`. Ctrl+C does not reach it.
- `deemo ps` is the manager. `deemo -- ps -ef` runs `ps`.
- Pipes carry stdout only. Use `2>&1`, or detach mode, to capture stderr.
- Output is block-buffered when it is not a tty: `python -u`, `PYTHONUNBUFFERED=1`, or `stdbuf -oL`.
- A pipe stays open until every writer closes. For a daemon, use `deemo -- <CMD>` or `deemo --background`.

Why stop, kill, and detach behave this way: [DESIGN.md](DESIGN.md).

## Development

```bash
cargo test
scripts/e2e-tmux.sh    # tmux smoke: http.server, ps, logs, stop, kill
```

macOS, Linux, and Windows. MIT.
