mod cli;
mod port;
mod registry;
mod signal;
mod sink;

use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::OnceLock;

use clap::Parser;
use cli::{Cli, Manage};

static CHILD_PID: OnceLock<u32> = OnceLock::new();
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_CMD_NOT_FOUND: i32 = 127;
const EXIT_INTERRUPTED: i32 = 130; // 128 + SIGINT, the unix convention

/// Why the pump loop should stop.
enum Stop {
    /// Downstream closed (`... | deemo | head`): graceful termination, like
    /// ripgrep treats a broken pipe — silent, exit 0, not an error.
    BrokenPipe,
    /// Any other stdout failure: fatal.
    StdoutError,
}

fn main() {
    let cli = Cli::parse();
    std::process::exit(run(&cli));
}

fn run(cli: &Cli) -> i32 {
    // --- management subcommands ------------------------------------------
    let home = sink::resolve_home(cli.dir.as_deref());
    match &cli.manage {
        Some(Manage::Ps) => return registry::ps(&home),
        Some(Manage::Stop { targets }) => return registry::stop(&home, targets),
        Some(Manage::Kill {
            ports,
            signal,
            dry_run,
        }) => return registry::kill(&home, ports, signal.as_deref(), *dry_run),
        Some(Manage::Logs { label }) => return sink::logs(&home, label),
        None => {}
    }

    // --- pipe mode: no command, we are downstream of a pipe --------------
    if cli.command.is_empty() {
        // Bare `deemo` in a terminal must not silently block on stdin —
        // that reads as "not daemonizing". Pipe mode requires an actual pipe.
        if io::stdin().is_terminal() {
            eprintln!("deemo: no command given and stdin is a terminal; nothing to do.");
            eprintln!("  start a daemon:     deemo -- <CMD> [ARGS...]");
            eprintln!("  capture a pipeline: <cmd> | deemo");
            eprintln!("  manage processes:   deemo ps | deemo stop <LABEL> | deemo kill <PORT> | deemo logs <LABEL>");
            return 2; // clap's usage-error convention
        }
        // Pipe semantics: EOF arrives only when *every* writer of the pipe
        // closes — including daemons the upstream spawned in the background.
        // Say so, so waiting never looks like hanging.
        if cli.background {
            return pipe_background(cli, &home);
        }
        eprintln!(
            "deemo: pipe mode: waiting for stdin to close (all writers must exit; Ctrl+C twice to quit early)"
        );
        let mut sink = prepare_log(cli, &home, LogMode::Pipe);
        install_ctrlc();
        return pump_pipe(cli, &mut sink, false);
    }

    // --- run modes: detach (default) or foreground ------------------------
    let log = prepare_log(cli, &home, LogMode::Spawn);
    if cli.foreground {
        install_ctrlc();
        supervise(cli, log)
    } else {
        detach(cli, &home, log)
    }
}

/// Where a prepared log is announced. The file is the same either way.
enum LogMode {
    /// Pipe: print the path now. A failure leaves pure passthrough.
    Pipe,
    /// Detach / foreground: the mode itself prints the path after spawn.
    Spawn,
}

/// Open this run's log and prune older files for the label.
/// `None` is `--yolo`, or the file could not be created (already reported).
fn prepare_log(cli: &Cli, home: &std::path::Path, mode: LogMode) -> Option<sink::Sink> {
    if cli.yolo {
        eprintln!("deemo: --yolo, not writing any logs");
        return None;
    }
    let label = cli.label_string();
    let sink = match sink::Sink::open(home, &label, cli.timestamp) {
        Ok(sink) => sink,
        Err(e) => {
            let fallback = match mode {
                LogMode::Pipe => "passthrough only",
                LogMode::Spawn => "running without logs",
            };
            eprintln!("deemo: warning: cannot open log file, {fallback}: {e:#}");
            return None;
        }
    };
    if matches!(mode, LogMode::Pipe) {
        eprintln!("deemo: logging to {}", sink.path.display());
    }
    if cli.keep > 0 {
        match sink::prune_old_logs(home, &label, cli.keep, &sink.path) {
            Ok(0) => {}
            Ok(pruned) => eprintln!("deemo: pruned {pruned} old log file(s)"),
            Err(e) => eprintln!("deemo: warning: could not prune old logs: {e:#}"),
        }
    }
    Some(sink)
}

/// Ctrl+C semantics (pipe & foreground modes):
/// - 1st press: remember it, keep draining the stream so the child's final
///   output (shutdown logs, stack traces) is captured. If the child is still
///   alive after a short grace period (e.g. someone ran `kill -INT <deemo-pid>`
///   targeting deemo alone), SIGINT is forwarded to it. The delay avoids a
///   second premature SIGINT cutting the child's graceful shutdown short.
/// - 2nd press: exit immediately (the log is flushed line by line, so nothing
///   already written is lost).
fn install_ctrlc() {
    ctrlc::set_handler(|| {
        if !INTERRUPTED.swap(true, Ordering::SeqCst) {
            eprintln!("\ndeemo: interrupted; press Ctrl+C again to force quit");
            if let Some(pid) = CHILD_PID.get() {
                let pid = *pid;
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    #[cfg(unix)]
                    unsafe {
                        // kill(pid, 0) = liveness probe; no-op on corpses.
                        if libc::kill(pid as i32, 0) == 0 {
                            libc::kill(pid as i32, libc::SIGINT);
                        }
                    }
                    #[cfg(not(unix))]
                    let _ = pid;
                });
            }
        } else {
            std::process::exit(EXIT_INTERRUPTED);
        }
    })
    .unwrap_or_else(|e| eprintln!("deemo: warning: could not install Ctrl+C handler: {e}"));
}

/// Write one chunk to stdout (unless quiet) and the log file.
fn emit_line(
    buf: &[u8],
    stdout: &mut io::Stdout,
    quiet: bool,
    sink: &mut Option<sink::Sink>,
) -> Result<(), Stop> {
    if !quiet {
        if let Err(e) = stdout.write_all(buf).and_then(|()| stdout.flush()) {
            return if e.kind() == io::ErrorKind::BrokenPipe {
                Err(Stop::BrokenPipe)
            } else {
                eprintln!("deemo: error writing stdout: {e}");
                Err(Stop::StdoutError)
            };
        }
    }
    let log_broken = match sink.as_mut() {
        Some(s) => s.write_line(buf).is_err(),
        None => false,
    };
    if log_broken {
        // Degrade to pure passthrough; never kill the user's stream because of
        // a disk problem. Warn only once.
        *sink = None;
        eprintln!("deemo: warning: log write failed, continuing without logging");
    }
    Ok(())
}

fn report_log(sink: &Option<sink::Sink>) {
    if let Some(s) = sink {
        eprintln!("deemo: log saved to {}", s.path.display());
    }
}

/// Pipe mode + `--background`: fork ourselves; the parent returns to the
/// shell immediately, the child (own session, registered in the pid file
/// store) keeps pumping the pipe into the log until ALL writers close —
/// even if the upstream daemonized something and left the write end open.
/// If the terminal goes away, the child silently drops passthrough and
/// keeps only logging.
#[cfg(unix)]
fn pipe_background(cli: &Cli, home: &std::path::Path) -> i32 {
    let mut sink = prepare_log(cli, home, LogMode::Pipe);
    let label = cli.label_string();

    match unsafe { libc::fork() } {
        -1 => {
            eprintln!("deemo: fork failed: {}", io::Error::last_os_error());
            EXIT_ERROR
        }
        0 => {
            // child: new session (survives terminal close), then pump.
            if unsafe { libc::setsid() } == -1 {
                eprintln!(
                    "deemo: warning: setsid failed: {}",
                    io::Error::last_os_error()
                );
            }
            install_ctrlc();
            let started = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
            let pid = std::process::id();
            if let Some(s) = &sink {
                let _ = registry::register(home, &label, pid, &started, "<piped>", &s.path);
            }
            let code = pump_pipe(cli, &mut sink, true);
            std::process::exit(code);
        }
        pid => {
            eprintln!("deemo: backgrounded (pid {pid}); will exit when every pipe writer closes");
            if let Some(s) = &sink {
                eprintln!("deemo: log: {}", s.path.display());
            }
            eprintln!("deemo: stop with: deemo stop {label}");
            EXIT_OK
        }
    }
}

#[cfg(not(unix))]
fn pipe_background(_cli: &Cli, _home: &std::path::Path) -> i32 {
    eprintln!("deemo: --background in pipe mode is not supported on this platform");
    EXIT_ERROR
}

/// Pipe mode: `cmd | deemo`. stdout of the upstream arrives on our stdin;
/// its stderr never enters the pipe (that's POSIX, use `2>&1` or
/// `deemo -- cmd` to capture it). Block on reads until EOF.
///
/// `detached` (pipe + --background): this instance lives in its own session
/// and outlives the terminal; losing stdout degrades to log-only instead of
/// killing the pump.
fn pump_pipe(cli: &Cli, sink: &mut Option<sink::Sink>, detached: bool) -> i32 {
    let stdin = io::stdin();
    let mut reader = io::BufReader::new(stdin.lock());
    let mut stdout = io::stdout();
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut passthrough_dead = false;

    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break, // EOF: every writer of the pipe is gone
            Ok(_) => {}
            Err(e) => {
                eprintln!("deemo: error reading stdin: {e}");
                return EXIT_ERROR;
            }
        }
        if passthrough_dead {
            // terminal gone (detached run): keep only writing the log
            let log_broken = match sink.as_mut() {
                Some(s) => s.write_line(&buf).is_err(),
                None => false,
            };
            if log_broken {
                *sink = None;
            }
            continue;
        }
        match emit_line(&buf, &mut stdout, cli.quiet, sink) {
            Ok(()) => {}
            Err(Stop::BrokenPipe) => {
                if detached {
                    passthrough_dead = true;
                    eprintln!("deemo: terminal gone; switching to log-only");
                } else {
                    return EXIT_OK; // graceful, like `rg | head`
                }
            }
            Err(Stop::StdoutError) => {
                if detached {
                    passthrough_dead = true;
                } else {
                    report_log(sink);
                    return EXIT_ERROR;
                }
            }
        }
    }
    let _ = stdout.flush();
    report_log(sink);
    if INTERRUPTED.load(Ordering::SeqCst) {
        EXIT_INTERRUPTED // Ctrl+C reached us while the stream was still open
    } else {
        EXIT_OK
    }
}

/// Foreground mode: `deemo --foreground -- <CMD>`. Spawn the child, capture
/// both stdout and stderr (merged, like `2>&1` built in), and exit with the
/// child's exit code.
fn supervise(cli: &Cli, mut sink: Option<sink::Sink>) -> i32 {
    let cmd = &cli.command;
    eprintln!("deemo: supervising: {}", cmd.join(" "));

    let mut child = match spawn_child(cmd) {
        Ok(c) => c,
        Err(e) => return spawn_error(&cmd[0], e),
    };
    let _ = CHILD_PID.set(child.id());

    // One reader thread per captured fd; lines are merged through a channel.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let streams: Vec<Box<dyn io::Read + Send>> = vec![
        Box::new(child.stdout.take().expect("stdout was piped")),
        Box::new(child.stderr.take().expect("stderr was piped")),
    ];
    for stream in streams {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = io::BufReader::new(stream);
            let mut buf = Vec::with_capacity(8 * 1024);
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(buf.clone()).is_err() {
                            break; // receiver is gone
                        }
                    }
                }
            }
        });
    }
    drop(tx);

    let mut stdout = io::stdout();
    while let Ok(line) = rx.recv() {
        match emit_line(&line, &mut stdout, cli.quiet, &mut sink) {
            Ok(()) => {}
            // Downstream closed: stop the child with it (it would only die of
            // EPIPE anyway) and leave quietly — `| head` is not an error.
            Err(Stop::BrokenPipe) => {
                let _ = child.kill();
                return EXIT_OK;
            }
            Err(Stop::StdoutError) => {
                let _ = child.kill();
                report_log(&sink);
                return EXIT_ERROR;
            }
        }
    }

    // Both fds are closed: the child exited (or closed its streams).
    match child.wait() {
        Ok(status) => {
            eprintln!("deemo: child exited ({status})");
            report_log(&sink);
            status_code(&status)
        }
        Err(e) => {
            eprintln!("deemo: error waiting for child: {e}");
            report_log(&sink);
            EXIT_ERROR
        }
    }
}

/// Detach mode (default): `deemo -- <CMD>` returns to the shell immediately.
/// The child gets its own session (survives terminal close) and its
/// stdout/stderr point directly at the log file — deemo steps out of the
/// data path entirely, so it can exit right after spawning.
fn detach(cli: &Cli, home: &std::path::Path, log: Option<sink::Sink>) -> i32 {
    let cmd = &cli.command;
    let (log_file, log_path) = match log {
        Some(sink) => {
            let (file, path) = sink.into_file();
            (file, Some(path))
        }
        None => (open_null_write(), None),
    };

    let mut command = Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .stdin(Stdio::from(open_null_read()))
        .stdout(Stdio::from(
            log_file.try_clone().expect("log fd can be cloned"),
        ))
        .stderr(Stdio::from(log_file));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            // Own session: no controlling terminal, terminal-close SIGHUP
            // never reaches the child. setsid works because a freshly forked
            // child is never a process-group leader.
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS: no console attached; CREATE_NEW_PROCESS_GROUP:
        // Ctrl+C in this console does not reach the child.
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }

    let started = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return spawn_error(&cmd[0], e),
    };
    let pid = child.id();
    if let Some(path) = log_path.as_deref() {
        if let Err(e) = registry::register(
            home,
            &cli.label_string(),
            pid,
            &started,
            &cmd.join(" "),
            path,
        ) {
            eprintln!("deemo: warning: could not register pid file: {e:#}");
        }
    }
    eprintln!("deemo: detached {cmd:?} (pid {pid})");
    if let Some(p) = log_path.as_deref() {
        eprintln!("deemo: log: {}", p.display());
    }
    eprintln!("deemo: stop with: deemo stop {}", cli.label_string());
    EXIT_OK
}

fn spawn_child(cmd: &[String]) -> io::Result<std::process::Child> {
    Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn spawn_error(program: &str, e: io::Error) -> i32 {
    if e.kind() == io::ErrorKind::NotFound {
        eprintln!("deemo: command not found: {program}");
        EXIT_CMD_NOT_FOUND
    } else {
        eprintln!("deemo: failed to spawn {program}: {e}");
        EXIT_ERROR
    }
}

#[cfg(unix)]
fn open_null_read() -> fs::File {
    fs::File::open("/dev/null").expect("/dev/null must exist")
}

#[cfg(unix)]
fn open_null_write() -> fs::File {
    fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .expect("/dev/null must be writable")
}

#[cfg(not(unix))]
fn open_null_read() -> fs::File {
    fs::File::open("NUL").expect("NUL must exist")
}

#[cfg(not(unix))]
fn open_null_write() -> fs::File {
    fs::OpenOptions::new()
        .write(true)
        .open("NUL")
        .expect("NUL must be writable")
}

#[cfg(unix)]
fn status_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code,
        None => 128 + status.signal().unwrap_or(1),
    }
}

#[cfg(not(unix))]
fn status_code(status: &std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}
