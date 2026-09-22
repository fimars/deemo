# Design

Decisions behind the commands in the [README](README.md). The README is the contract; this file is why it looks that way.

## Detach

`deemo -- <CMD>` is the default because the job is to get the shell back.

On unix the child calls `setsid` before `exec`. A fresh child is not a process-group leader, so `setsid` succeeds and the child becomes a session leader with no controlling terminal. Terminal close sends `SIGHUP` to the old session; the daemon is not in it. On Windows the same idea is `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`.

stdin is `/dev/null` (or `NUL`). A daemon that reads stdin would otherwise stall on a closed terminal. stdout and stderr are the log file itself, so deemo exits after spawn and is not in the data path. Line order between the two streams is nondeterministic, same as shell `2>&1`.

The pid file is written only when a log exists. `--yolo` discards output and does not register the process. Without a registration there is nothing for `stop` to find, so the `stop with:` hint is withheld too — deemo never hands out a command that is guaranteed to fail.

## Stop is a process group

Detach's pid is the session and group leader. `deemo stop` sends `SIGTERM` to that group (`killpg`), waits 5 seconds, then `SIGKILL`, and waits 1 second more. The pid file is removed only after the group is gone. A survivor keeps its pid file and `stop` exits 1, so a retry still has a target.

A label is not unique: `pnpm dev:admin` and `pnpm dev:merchant` both default to `pnpm`, and `stop pnpm` takes both down. That is what a label means — one label, one unit. To stop just one, read its pid off `deemo ps` and pass that: a numeric argument is a pid, never a label. A label that is all digits is therefore reachable only as a pid; rename it via `--label`.

The group is the unit because launchers (`npm`, `pnpm`, `deno`, `dshx`) spawn the real server as a child. Killing only the registered pid orphans that child and leaves the port bound.

A program that double-forks into a new session leaves the group on purpose. No process-group supervisor, deemo included, can follow it. That is the program's own lifecycle.

On Windows, `taskkill /F /T` is a hard tree kill. There is no cheap liveness probe, so liveness is *unknown* there: `ps` shows `unknown` instead of `running`, registrations are never swept as dead, and `stop` still signals through `taskkill`. Sweeping what cannot be probed would delete the registry on the very first `ps`.

## Kill is a port

`deemo kill` exists for `Address already in use` when the label is unknown or the process was not started by deemo.

Only sockets whose **local** address is that port are targets (TCP and UDP, IPv4 and IPv6, any state). A client connected *to* the port has some other local port and is left alone. `lsof -i :PORT` matches both sides; deemo keeps the binding side, same as `fuser`.

Foreign pids are signalled one at a time. A server started in a terminal shares its process group with the shell, and `killpg` would take the shell down with it. Pids deemo itself started are signalled as a group, because detach already gave them a private session.

Without root, other users' sockets are invisible. Same limit as `lsof` and `fuser`.

Discovery:

- Linux reads `/proc/net/{tcp,tcp6,udp,udp6}` and matches socket inodes under `/proc/<pid>/fd`. No external tool.
- macOS and other BSDs run `lsof -nP -F pn`. Field output, not the table: process names with spaces shift columns.
- Windows runs `netstat -ano`. The pid is the last column, because UDP rows have no state column.

`-s` is POSIX `kill -s`: one signal, no escalation, so `-s HUP` reloads. Names accept an optional `SIG` prefix and any case; numbers accept an optional `-`. Signal `0` is rejected because it is `kill`'s existence probe, not a delivered signal. `--dry-run` prints each pid once, even if it holds several of the given ports.

A zombie counts as dead. It holds no sockets, and signalling it does nothing. Treating `kill(pid, 0) == 0` as alive would report a successful kill as "survived SIGTERM and SIGKILL".

After a kill, registrations whose pids are gone are swept, so `ps` does not keep a corpse.

## Pipe

A pipe is a filter. It stays in the foreground and exits `0` on EOF — with
its own status, like `tee`: a pipe cannot carry the upstream's exit code, and
a pipeline's `$?` is the last command's by shell convention anyway. EOF
arrives only when every writer closes. If the upstream daemonizes and keeps the write end, deemo waits, and it says so at startup. The way out is detach mode, or `--background`.

`--background` (unix) forks the pump into its own session and returns immediately. The child is registered and shows up in `ps`. If the terminal disappears, passthrough is dropped and logging continues until the writers close.

Downstream `EPIPE` (`… | deemo | head`) exits 0 with no error on stderr, the same convention as ripgrep: the reader stopped asking.

Ctrl+C in pipe and foreground mode sets a flag and keeps draining, so the child's last lines reach the log. If the child is still alive 250 ms later, `SIGINT` is forwarded. A second Ctrl+C exits 130 immediately. The delay is there so a second signal does not cut a graceful shutdown short.

Foreground merges the child's stdout and stderr on one channel and returns the child's code. Signal death N becomes `128+N`.

## Logs

One function opens the log for every mode. Naming:

| `--timestamp` | File |
|---|---|
| `run` (default) | `<label>-YYYYmmdd-HHMMSS.mmm.log` |
| `day` | `<label>-YYYYmmdd.log`, later runs append |
| `off` | `<label>.log`, later runs append |

`--keep N` runs at startup. Names sort in time order for a single scheme, oldest first, and the file just opened sorts last so it is not deleted. `deemo logs` prints the greatest matching name, which is the newest run. Files of another label are ignored.

If the log cannot be created, the command still runs and a warning is printed. A later write error drops logging and keeps the stream.

`$DEEMO_HOME` (or `--dir`, which wins) is the root. `logs/` holds logs, `run/<label>.<pid>.pid` holds `pid`, `label`, `started`, `cmd`, and `log`.

## Exit codes

Detach exits 0 once the child has been spawned. The child's later fate is `ps` / `stop` / the log, not `$?`.

`127` is the shell's "command not found". `2` is clap's usage error: bare `deemo` on a terminal, `kill` with no port, or an unknown `-s`. `1` means the operation did not do what was asked: a free port, a process that survives both signals, `ps` with nothing to list, `stop` matching nothing, `logs` with no file for the label. Pipe mode exits `0` on EOF — see Pipe above.
