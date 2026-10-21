mod cli;
mod port;
mod registry;
mod signal;
mod sink;

use std::fs;
use std::io::{self, BufRead, IsTerminal, Read, Write};
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

/// Read size for the pump, and the cap on how much output may sit in deemo's
/// buffers. One read becomes one write: a writer that pauses after a line is
/// seen line by line, a burst is written in slabs instead of a syscall (or
/// three) per line. 64 KiB is also the largest lag `tail -f` would see.
const CHUNK: usize = 64 * 1024;

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
        Some(Manage::Ps { verbose }) => return registry::ps(&home, *verbose),
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
/// - 2nd press: exit immediately, forfeiting at most one buffered slab.
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

/// Queue bytes for the log and stdout. A log failure only loses logging;
/// a stdout failure stops the pump (EPIPE being the graceful one).
fn emit(
    buf: &[u8],
    stdout: &mut io::Stdout,
    quiet: bool,
    sink: &mut Option<sink::Sink>,
) -> Result<(), Stop> {
    write_log(sink, buf);
    if quiet {
        return Ok(());
    }
    stdout.write_all(buf).map_err(stdout_stopped)
}

/// Push everything queued so far out. Called right before a read that may
/// block, so a line is visible as soon as the writer pauses.
fn emit_flush(
    stdout: &mut io::Stdout,
    quiet: bool,
    sink: &mut Option<sink::Sink>,
) -> Result<(), Stop> {
    flush_log(sink);
    if quiet {
        return Ok(());
    }
    stdout.flush().map_err(stdout_stopped)
}

fn stdout_stopped(e: io::Error) -> Stop {
    if e.kind() == io::ErrorKind::BrokenPipe {
        Stop::BrokenPipe
    } else {
        eprintln!("deemo: error writing stdout: {e}");
        Stop::StdoutError
    }
}

fn write_log(sink: &mut Option<sink::Sink>, bytes: &[u8]) {
    log_op(sink, |s| s.write(bytes));
}

fn flush_log(sink: &mut Option<sink::Sink>) {
    log_op(sink, |s| s.flush());
}

fn log_op(sink: &mut Option<sink::Sink>, op: impl FnOnce(&mut sink::Sink) -> std::io::Result<()>) {
    let broken = match sink.as_mut() {
        Some(s) => op(s).is_err(),
        None => false,
    };
    if broken {
        // Degrade to pure passthrough; never kill the user's stream because
        // of a disk problem. Warn only once.
        *sink = None;
        eprintln!("deemo: warning: log write failed, continuing without logging");
    }
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
            // Hint only when the child registered a pid file to answer it.
            if let Some(s) = &sink {
                eprintln!("deemo: log: {}", s.path.display());
                eprintln!("deemo: stop with: deemo stop {label}");
            }
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
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout();
    let mut buf = vec![0u8; CHUNK];
    let mut passthrough_dead = false;

    loop {
        let filled = match stdin.read(&mut buf) {
            Ok(0) => break, // EOF: every writer of the pipe is gone
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("deemo: error reading stdin: {e}");
                return EXIT_ERROR;
            }
        };
        let slab = &buf[..filled];
        if passthrough_dead {
            // terminal gone (detached run): keep only writing the log
            write_log(sink, slab);
            flush_log(sink);
            continue;
        }
        match emit(slab, &mut stdout, cli.quiet, sink)
            .and_then(|()| emit_flush(&mut stdout, cli.quiet, sink))
        {
            Ok(()) => {}
            Err(Stop::BrokenPipe) => {
                if detached {
                    passthrough_dead = true;
                    eprintln!("deemo: terminal gone; switching to log-only");
                } else {
                    flush_log(sink); // keep what was read before the reader left
                    return EXIT_OK; // graceful, like `rg | head`
                }
            }
            Err(Stop::StdoutError) => {
                if detached {
                    passthrough_dead = true;
                } else {
                    flush_log(sink);
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
fn supervise(cli: &Cli, sink: Option<sink::Sink>) -> i32 {
    let cmd = &cli.command;
    eprintln!("deemo: supervising: {}", cmd.join(" "));

    let mut child = match spawn_child(cmd) {
        Ok(c) => c,
        Err(e) => return spawn_error(&cmd[0], e),
    };
    let _ = CHILD_PID.set(child.id());

    // One reader thread per captured fd; whole lines are merged through one
    // channel, so the other stream can never split a line in the log.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let streams: Vec<Box<dyn io::Read + Send>> = vec![
        Box::new(child.stdout.take().expect("stdout was piped")),
        Box::new(child.stderr.take().expect("stderr was piped")),
    ];
    for stream in streams {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = io::BufReader::with_capacity(CHUNK, stream);
            loop {
                // Hand each line over by value: the pump is the only consumer
                // and a fresh buffer beats cloning the previous one.
                let mut buf = Vec::with_capacity(256);
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(buf).is_err() {
                            break; // receiver is gone
                        }
                    }
                }
            }
        });
    }
    drop(tx);

    let mut out = Out::new(cli.quiet, sink);
    let stopped = loop {
        let line = match rx.try_recv() {
            Ok(line) => line,
            // Nothing in flight: push the buffer out, then blocking is safe.
            Err(mpsc::TryRecvError::Empty) => {
                if let Err(stop) = out.flush() {
                    break Some(stop);
                }
                match rx.recv() {
                    Ok(line) => line,
                    Err(_) => break None, // both fds are closed
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => break None,
        };
        if let Err(stop) = out.write(&line) {
            break Some(stop);
        }
    };

    match stopped {
        // Downstream closed: stop the child with it (it would only die of
        // EPIPE anyway) and leave quietly — `| head` is not an error.
        Some(Stop::BrokenPipe) => {
            let _ = child.kill();
            out.flush_log();
            EXIT_OK
        }
        Some(Stop::StdoutError) => {
            let _ = child.kill();
            out.flush_log();
            report_log(&out.sink);
            EXIT_ERROR
        }
        None => match out.flush() {
            // `| head` is not an error, even when the reader left during the
            // final flush.
            Err(Stop::BrokenPipe) => EXIT_OK,
            Err(Stop::StdoutError) => {
                report_log(&out.sink);
                EXIT_ERROR
            }
            // Both fds are closed: the child exited (or closed its streams).
            Ok(()) => match child.wait() {
                Ok(status) => {
                    eprintln!("deemo: child exited ({status})");
                    report_log(&out.sink);
                    status_code(&status)
                }
                Err(e) => {
                    eprintln!("deemo: error waiting for child: {e}");
                    report_log(&out.sink);
                    EXIT_ERROR
                }
            },
        },
    }
}

/// The foreground pump's output side: stdout and the log, both buffered, so a
/// burst costs one write per slab instead of one per line. `flush` runs
/// whenever the line channel runs dry — right before the pump would block.
struct Out {
    quiet: bool,
    stdout: io::BufWriter<io::StdoutLock<'static>>,
    sink: Option<sink::Sink>,
}

impl Out {
    fn new(quiet: bool, sink: Option<sink::Sink>) -> Out {
        Out {
            quiet,
            stdout: io::BufWriter::with_capacity(CHUNK, io::stdout().lock()),
            sink,
        }
    }

    fn write(&mut self, line: &[u8]) -> Result<(), Stop> {
        if !self.quiet {
            if let Err(e) = self.stdout.write_all(line) {
                return Err(stdout_stopped(e));
            }
        }
        write_log(&mut self.sink, line);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Stop> {
        // Log first: a closed downstream must not cost the log the bytes that
        // were already read.
        self.flush_log();
        if self.quiet {
            return Ok(());
        }
        self.stdout.flush().map_err(stdout_stopped)
    }

    fn flush_log(&mut self) {
        flush_log(&mut self.sink);
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
    // The `stop with:` hint is only honest when a registration exists to
    // answer it (--yolo and a failed log open both leave us unregistered).
    let mut registered = false;
    if let Some(path) = log_path.as_deref() {
        match registry::register(
            home,
            &cli.label_string(),
            pid,
            &started,
            &cmd.join(" "),
            path,
        ) {
            Ok(()) => registered = true,
            Err(e) => eprintln!("deemo: warning: could not register pid file: {e:#}"),
        }
    }
    eprintln!("deemo: detached {cmd:?} (pid {pid})");
    if let Some(p) = log_path.as_deref() {
        eprintln!("deemo: log: {}", p.display());
    }
    if registered {
        eprintln!("deemo: stop with: deemo stop {}", cli.label_string());
    }
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
