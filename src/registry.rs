//! Pid files for detached runs: `$DEEMO_HOME/run/<label>.<pid>.pid`.
//!
//! `stop` signals the process group. Detach calls `setsid`, so the registered
//! pid is the session leader and `killpg` reaches descendants that stayed in
//! that group. A launcher that forks the real server without calling `setsid`
//! again dies with it.
//!
//! `kill <port>` signals foreign pids one at a time — their group may be the
//! user's shell — and deemo's own pids as a group.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

struct Entry {
    pub pid: u32,
    pub label: String,
    pub started: String,
    pub cmd: String,
    pub log: PathBuf,
    pub file: PathBuf, // the pid-file itself
}

fn run_dir(home: &Path) -> PathBuf {
    home.join("run")
}

/// Persist a started process. One file per instance: `<label>.<pid>.pid`.
pub fn register(
    home: &Path,
    label: &str,
    pid: u32,
    started: &str,
    cmd: &str,
    log: &Path,
) -> Result<()> {
    let dir = run_dir(home);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let file = dir.join(format!("{label}.{pid}.pid"));
    fs::write(
        &file,
        format!(
            "pid={pid}\nlabel={label}\nstarted={started}\ncmd={cmd}\nlog={}\n",
            log.display()
        ),
    )
    .with_context(|| format!("cannot write {}", file.display()))?;
    Ok(())
}

fn parse_entry(file: &Path) -> Option<Entry> {
    let text = fs::read_to_string(file).ok()?;
    let mut pid = None;
    let mut label = None;
    let mut started = None;
    let mut cmd = None;
    let mut log = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "pid" => pid = value.parse().ok(),
            "label" => label = Some(value.to_string()),
            "started" => started = Some(value.to_string()),
            "cmd" => cmd = Some(value.to_string()),
            "log" => log = Some(PathBuf::from(value)),
            _ => {}
        }
    }
    Some(Entry {
        pid: pid?,
        label: label?,
        started: started.unwrap_or_default(),
        cmd: cmd.unwrap_or_default(),
        log: log?,
        file: file.to_path_buf(),
    })
}

/// What deemo can honestly say about a registered process.
///
/// The distinction matters: only `Dead` may be swept. `Unknown` (Windows has
/// no cheap std-only probe) must keep its registration, or every management
/// command would delete the whole registry on the first call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    /// Constructed only by the unix probe; windows cannot probe, so keep the
    /// variant (match arms need it) without demanding a constructor there.
    #[cfg_attr(not(unix), allow(dead_code))]
    Alive,
    Dead,
    /// No probe available on this platform — neither alive nor dead.
    /// (Only *constructed* where there is no probe; unix still matches it.)
    #[cfg_attr(unix, allow(dead_code))]
    Unknown,
}

/// Is this pid alive?
///
/// A zombie counts as **not** alive: it holds no sockets or files any more
/// (a port it used is free) and signalling it does nothing, so without the
/// state check `kill(pid, 0)` would report a corpse as running and make a
/// successful kill look "stubborn". Registered entries never sit in that
/// state for long (detached children are reaped by init/launchd after deemo
/// exits); the check matters for `deemo kill <PORT>`, whose targets are
/// arbitrary processes with arbitrary parents. Pid reuse within a short
/// window is theoretically possible and accepted.
#[cfg(unix)]
fn liveness(pid: u32) -> Liveness {
    let alive = unsafe { libc::kill(pid as i32, 0) == 0 } && !zombie(pid);
    if alive {
        Liveness::Alive
    } else {
        Liveness::Dead
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    liveness(pid) == Liveness::Alive
}

/// Process state is `Z` — exited, not yet reaped by anyone.
#[cfg(target_os = "linux")]
fn zombie(pid: u32) -> bool {
    // /proc/<pid>/stat: the state is the first field after the
    // parenthesised command name (which may itself contain spaces).
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z'))
        })
        .unwrap_or(false)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn zombie(pid: u32) -> bool {
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as i32;
    let filled = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    filled == size && info.pbsi_status == libc::SZOMB
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "macos", target_os = "ios"))
))]
fn zombie(_pid: u32) -> bool {
    false // no cheap portable state probe here; worst case a reaped-too-late
          // target is reported as stubborn once
}

#[cfg(not(unix))]
fn liveness(_pid: u32) -> Liveness {
    // Windows has no cheap std-only liveness probe. Reporting `Unknown`
    // instead of guessing keeps `ps` truthful ("unknown", not "running")
    // and — crucially — keeps the registration: `stop` still works through
    // `taskkill /F /T`, and sweeping what we cannot probe would delete the
    // registry on the first management call.
    Liveness::Unknown
}

/// All registered entries + what each process's liveness is.
fn list(home: &Path) -> Vec<(Entry, Liveness)> {
    let dir = run_dir(home);
    let mut entries: Vec<(Entry, Liveness)> = fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "pid"))
                .filter_map(|p| {
                    let entry = parse_entry(&p)?;
                    let liveness = liveness(entry.pid);
                    Some((entry, liveness))
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by(|a, b| a.0.label.cmp(&b.0.label).then(a.0.pid.cmp(&b.0.pid)));
    entries
}

/// How a stop attempt ended.
enum StopOutcome {
    /// SIGTERM alone took the group down.
    Terminated,
    /// SIGTERM was ignored; SIGKILL took the group down.
    /// (The escalation path is unix-only; windows stops through taskkill.)
    #[cfg_attr(not(unix), allow(dead_code))]
    Killed,
    /// Still alive after SIGKILL (or unsignalable): give up honestly.
    #[cfg_attr(not(unix), allow(dead_code))]
    Stubborn,
}

#[cfg(unix)]
const TERM_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(unix)]
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// First probe delay while waiting for a signalled process to disappear. A
/// cooperative server is gone in ~2 ms, so starting at 50 ms would make every
/// `stop` look like it ignored the signal; the delay doubles up to the old
/// cadence, which is what a target that ignores SIGTERM still sees.
#[cfg(unix)]
const FIRST_POLL: std::time::Duration = std::time::Duration::from_micros(500);
#[cfg(unix)]
const MAX_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// One `deemo stop` argument. A number is a pid, anything else a label.
enum Selector {
    Label(String),
    Pid(u32),
}

impl Selector {
    fn parse(raw: &str) -> Selector {
        match raw.parse::<u32>() {
            Ok(pid) => Selector::Pid(pid),
            Err(_) => Selector::Label(raw.to_string()),
        }
    }

    fn matches(&self, entry: &Entry) -> bool {
        match self {
            Selector::Label(label) => &entry.label == label,
            Selector::Pid(pid) => entry.pid == *pid,
        }
    }

    /// Phrase for the "matched nothing" message.
    fn describe(&self) -> String {
        match self {
            Selector::Label(label) => format!("label '{label}'"),
            Selector::Pid(pid) => format!("pid {pid}"),
        }
    }
}

/// Stop the process(es) each argument selects: a label stops every
/// registration under it, a pid stops exactly one. Returns an exit code.
pub fn stop(home: &Path, targets: &[String]) -> i32 {
    // Dead registrations must not linger (nor show up as "is not running"):
    // sweep them before matching, like `ps` does.
    sweep_dead(home);
    let entries = list(home);
    let mut failed = false;
    // `deemo stop pnpm 66089` selects one entry twice: stop it once.
    let mut handled: HashSet<u32> = HashSet::new();
    for raw in targets {
        let selector = Selector::parse(raw);
        let hits: Vec<&Entry> = entries
            .iter()
            .filter(|(entry, _)| selector.matches(entry))
            .map(|(entry, _)| entry)
            .collect();
        if hits.is_empty() {
            eprintln!("deemo: no process with {}", selector.describe());
            failed = true;
            continue;
        }
        for entry in hits {
            if !handled.insert(entry.pid) {
                continue;
            }
            match liveness(entry.pid) {
                Liveness::Dead => {
                    eprintln!("deemo: {} (pid {}) is not running", entry.label, entry.pid);
                }
                // Alive (unix) and Unknown (Windows, no probe) both take the
                // same road: `terminate` is the platform's stop primitive.
                Liveness::Alive | Liveness::Unknown => {
                    match terminate(Target::Group(entry.pid)) {
                        StopOutcome::Terminated => eprintln!(
                            "deemo: stopped {} (pid {}, {})",
                            entry.label,
                            entry.pid,
                            stop_method(true)
                        ),
                        StopOutcome::Killed => eprintln!(
                            "deemo: stopped {} (pid {}, SIGKILL after grace period)",
                            entry.label, entry.pid
                        ),
                        StopOutcome::Stubborn => {
                            failed = true;
                            eprintln!(
                                "deemo: {} (pid {}) survived SIGTERM and SIGKILL; keeping its pid file",
                                entry.label, entry.pid
                            );
                            continue; // keep the registration so the user can retry
                        }
                    }
                }
            }
            let _ = fs::remove_file(&entry.file);
        }
    }
    i32::from(failed)
}

/// Group for `stop` and for pids deemo started (`setsid` made that group
/// theirs alone). A single pid for everyone else: `killpg` on a server
/// started in a terminal would also kill the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// The whole process group led by this pid.
    Group(u32),
    /// A single process.
    Pid(u32),
}

impl Target {
    /// The pid everything else is derived from (only windows needs it as a
    /// value: `taskkill` takes a pid, not a group).
    #[cfg(not(unix))]
    fn pid(self) -> u32 {
        match self {
            Target::Group(pid) | Target::Pid(pid) => pid,
        }
    }
}

/// SIGTERM the target; escalate to SIGKILL when the grace period passes.
/// Returns how the target ended up.
#[cfg(unix)]
fn terminate(target: Target) -> StopOutcome {
    if !target.send(libc::SIGTERM) && !target.gone() {
        // e.g. EPERM; nothing more we can do
        return StopOutcome::Stubborn;
    }
    if wait_gone(target, TERM_GRACE) {
        return StopOutcome::Terminated;
    }
    target.send(libc::SIGKILL);
    if wait_gone(target, KILL_GRACE) {
        StopOutcome::Killed
    } else {
        StopOutcome::Stubborn
    }
}

#[cfg(unix)]
impl Target {
    /// Deliver `sig`; `false` = the kernel refused it (ESRCH / EPERM).
    fn send(&self, sig: i32) -> bool {
        match *self {
            // killpg reaches every member of the group in one blow — see the
            // module header for why that is the right unit for `stop`.
            Target::Group(pgid) => unsafe { libc::killpg(pgid as i32, sig) == 0 },
            Target::Pid(pid) => unsafe { libc::kill(pid as i32, sig) == 0 },
        }
    }

    /// Nothing responds to a probe any more.
    fn gone(&self) -> bool {
        match *self {
            // kill(pgid, 0) succeeds while ANY member of the group exists,
            // so this is the all-clear test for the whole tree (the leader
            // alone can die first).
            Target::Group(pgid) => unsafe { libc::killpg(pgid as i32, 0) != 0 },
            Target::Pid(pid) => !alive(pid),
        }
    }
}

#[cfg(unix)]
fn wait_gone(target: Target, grace: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        if target.gone() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    target.gone()
}

#[cfg(not(unix))]
fn terminate(target: Target) -> StopOutcome {
    // No std-only signal API on Windows; taskkill /F /T (tree) is the
    // pragmatic route. Liveness is unknowable here, so report success.
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &target.pid().to_string()])
        .status();
    StopOutcome::Terminated
}

/// POSIX `kill -s SIG`: deliver exactly this signal and stop there — no
/// escalation. That is what makes `-s HUP` a *reload* instead of a shutdown.
#[cfg(unix)]
fn signal_once(target: Target, sig: i32) -> Result<(), String> {
    if target.send(sig) {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

#[cfg(not(unix))]
fn signal_once(_target: Target, sig: i32) -> Result<(), String> {
    Err(crate::signal::display(sig) + " cannot be sent on Windows; drop -s to use taskkill")
}

/// How `terminate` stops a group on this platform — quoted in `stop`'s
/// report so the message never claims a signal the platform never sent.
#[cfg(unix)]
fn stop_method(group: bool) -> &'static str {
    if group {
        "SIGTERM to process group"
    } else {
        "SIGTERM"
    }
}

#[cfg(not(unix))]
fn stop_method(_group: bool) -> &'static str {
    "taskkill /F /T"
}

/// Remove pid-files whose processes are **known** to be dead (housekeeping
/// for ps). `Unknown` registrations are kept — sweeping them would erase the
/// registry on platforms without a liveness probe.
fn sweep_dead(home: &Path) {
    for (entry, liveness) in list(home) {
        if liveness == Liveness::Dead {
            let _ = fs::remove_file(entry.file);
        }
    }
}

/// Cap for the label column: a label is the row's key (you `stop` by it), so
/// be generous, but an absurd label must not eat the whole terminal.
const LABEL_MAX: usize = 32;
/// Never truncate a command shorter than this to squeeze a row into the
/// terminal — a useless stub ("b…") is worse than one wrapped line.
const CMD_MIN: usize = 16;

/// `deemo ps` — human table. Exit code 1 when nothing is running.
pub fn ps(home: &Path, verbose: bool) -> i32 {
    sweep_dead(home);
    let rows = list(home);
    if rows.is_empty() {
        println!("no processes started by deemo");
        return 1;
    }

    let color = std::io::stdout().is_terminal();
    let width = output_width();

    struct Row {
        label: String,
        pid: String,
        status: &'static str,
        age: String,   // relative age; the raw `started` stamp in verbose
        ports: String, // "4322", "3000,3001", or "-" when the cmd names none
        cmd: String,   // truncated to the terminal width (already)
        log: String,   // verbose only
    }

    let mut data: Vec<Row> = rows
        .iter()
        .map(|(e, liveness)| {
            let status = match liveness {
                Liveness::Alive => "running",
                Liveness::Dead => "dead",
                Liveness::Unknown => "unknown",
            };
            let age = if verbose {
                e.started.clone()
            } else {
                age_of(&e.started).map_or_else(|| "?".to_string(), format_age)
            };
            let found = ports_in(&e.cmd);
            Row {
                label: truncate(&e.label, LABEL_MAX),
                pid: e.pid.to_string(),
                status,
                age,
                ports: if found.is_empty() {
                    "-".to_string()
                } else {
                    found.join(",")
                },
                cmd: e.cmd.clone(),
                log: e.log.display().to_string(),
            }
        })
        .collect();

    let mut label_w = "LABEL".len();
    let mut age_w = if verbose {
        "STARTED".len()
    } else {
        "AGE".len()
    };
    let mut port_w = "PORT".len();
    for r in &data {
        label_w = label_w.max(r.label.chars().count());
        age_w = age_w.max(r.age.chars().count());
        port_w = port_w.max(r.ports.chars().count());
    }
    let pid_w = data
        .iter()
        .map(|r| r.pid.len())
        .max()
        .unwrap_or(3)
        .max("PID".len());
    let status_w = "running".len();
    // Everything left of COMMAND (single-space separated, two after PID).
    let head = label_w + 1 + pid_w + 2 + status_w + 1 + age_w + 1 + port_w + 1;

    for r in &mut data {
        // verbose rows end in "  -> <log>"; reserve its width when cutting.
        let suffix = if verbose {
            4 + r.log.chars().count()
        } else {
            0
        };
        if let Some(w) = width {
            let budget = w.saturating_sub(head).saturating_sub(suffix);
            if budget >= CMD_MIN {
                r.cmd = truncate(&r.cmd, budget);
            }
            // Narrower than that: print the command whole, one wrapped line.
        }
    }

    let age_header = if verbose { "STARTED" } else { "AGE" };
    let cmd_header = if verbose { "COMMAND -> LOG" } else { "COMMAND" };
    println!(
        "{:<label_w$} {:>pid_w$}  {:<status_w$} {:>age_w$} {:>port_w$} {cmd_header}",
        "LABEL", "PID", "STATUS", age_header, "PORT",
    );
    for r in &data {
        // Pad the status by its visible width: color codes add bytes, and
        // Rust's width filler would count them.
        let status = if color {
            colorize_status(r.status)
        } else {
            r.status.to_string()
        };
        let status = format!("{status}{}", " ".repeat(status_w - r.status.len()));
        let line = format!(
            "{:<label_w$} {:>pid_w$}  {status} {:>age_w$} {:>port_w$} {}",
            r.label, r.pid, r.age, r.ports, r.cmd,
        );
        if verbose {
            println!("{line}  -> {}", r.log);
        } else {
            println!("{line}");
        }
    }
    0
}

/// Cut `s` to `max` display chars, marking the cut with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Ports named on the recorded command line (`--port 4322`, `--port=4322`,
/// `-p 4322`, `-p4322`, `PORT=4322`), first-seen order, deduplicated. Pure
/// string parsing — no kernel probing — and it covers the dev-server case
/// deemo exists for; the `python -m http.server 8000` style (pure positional)
/// honestly reports none rather than guessing.
fn ports_in(cmd: &str) -> Vec<String> {
    let mut ports: Vec<String> = Vec::new();
    let mut tokens = cmd.split_whitespace();
    while let Some(tok) = tokens.next() {
        let port = if tok == "--port" || tok == "-p" {
            tokens.next().and_then(|t| t.parse::<u16>().ok())
        } else if let Some(v) = tok.strip_prefix("--port=") {
            v.parse::<u16>().ok()
        } else if let Some(v) = tok.strip_prefix("-p") {
            // -p4322 (an attached non-number like -path just fails to parse)
            v.parse::<u16>().ok()
        } else if let Some(v) = tok.strip_prefix("PORT=") {
            v.parse::<u16>().ok()
        } else {
            None
        };
        if let Some(p) = port {
            if p > 0 {
                let s = p.to_string();
                if !ports.contains(&s) {
                    ports.push(s);
                }
            }
        }
    }
    ports
}

/// Age of a `started` stamp (local ISO, as `register` writes it). `None`
/// when the stamp is unparsable or (clock skew) in the future.
fn age_of(started: &str) -> Option<std::time::Duration> {
    let naive = chrono::NaiveDateTime::parse_from_str(started, "%Y-%m-%dT%H:%M:%S").ok()?;
    let then = naive.and_local_timezone(chrono::Local).latest()?;
    chrono::Local::now()
        .signed_duration_since(then)
        .to_std()
        .ok()
}

/// "45s" / "12m" / "5h" / "3d" / "2w" — the coarsest unit that fits.
fn format_age(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else if secs < 7 * 86_400 {
        format!("{}d", secs / 86_400)
    } else {
        format!("{}w", secs / (7 * 86_400))
    }
}

/// Width cap for ps output. `COLUMNS` wins — it travels into pipes, where
/// the tty is gone and ioctl cannot help; then the tty itself; piped with no
/// `COLUMNS` means no cap (a file consumer handles long lines itself).
fn output_width() -> Option<usize> {
    if let Some(n) = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        return Some(n.max(40));
    }
    #[cfg(unix)]
    {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
        if ok && ws.ws_col > 0 {
            return Some(ws.ws_col as usize);
        }
    }
    if std::io::stdout().is_terminal() {
        Some(80)
    } else {
        None
    }
}

/// Status color for a tty table: green running, red dead, yellow unknown.
fn colorize_status(status: &str) -> String {
    let code = match status {
        "running" => "\x1b[32m",
        "dead" => "\x1b[31m",
        _ => "\x1b[33m",
    };
    format!("{code}{status}\x1b[0m")
}

/// `deemo kill <PORT>...` — stop whatever is bound to the port(s).
///
/// Default policy is `stop`'s: SIGTERM, 5 s grace, SIGKILL, then an honest
/// report if a target survives both. `-s` switches to POSIX `kill -s`
/// semantics (exactly that signal, no escalation — `-s HUP` reloads), and
/// `--dry-run` only lists the pids, like `lsof -t`.
pub fn kill(home: &Path, ports: &[u16], signal: Option<&str>, dry_run: bool) -> i32 {
    let sig = match signal.map(crate::signal::parse).transpose() {
        Ok(sig) => sig,
        Err(e) => {
            eprintln!("deemo: {e}");
            return 2; // usage error, clap's convention
        }
    };

    // Which pids did deemo itself start? Those are session leaders (detach
    // ran setsid), so their process group is their own — signalling it also
    // takes their launcher tree down, exactly like `stop`. Every other pid
    // is signalled alone: its group may be your shell's.
    let managed: HashMap<u32, String> = list(home)
        .into_iter()
        .filter(|&(_, liveness)| liveness != Liveness::Dead)
        .map(|(entry, _)| (entry.pid, entry.label))
        .collect();

    let mut failed = false;
    let mut empty: Vec<u16> = Vec::new();
    // Each pid is listed once, under the first port it was found on.
    let mut plan: Vec<(u16, u32)> = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();

    match crate::port::holders(ports) {
        Ok(found) => {
            for (&port, pids) in ports.iter().zip(found) {
                if pids.is_empty() {
                    empty.push(port);
                    continue;
                }
                for pid in pids {
                    if seen.insert(pid) {
                        plan.push((port, pid));
                    }
                }
            }
        }
        Err(e) => {
            let list: Vec<String> = ports.iter().map(u16::to_string).collect();
            eprintln!(
                "deemo: cannot list the processes on port {}: {e}",
                list.join(", ")
            );
            failed = true;
        }
    }

    if dry_run {
        for (_, pid) in &plan {
            println!("{pid}");
        }
    } else {
        for &(port, pid) in &plan {
            let group = managed.contains_key(&pid);
            let target = if group {
                Target::Group(pid)
            } else {
                Target::Pid(pid)
            };
            let subject = match managed.get(&pid) {
                Some(label) => format!("{label} (pid {pid})"),
                None => format!("pid {pid}"),
            };

            if let Some(sig) = sig {
                match signal_once(target, sig) {
                    Ok(()) => eprintln!(
                        "deemo: port {port}: sent {} to {subject}",
                        crate::signal::display(sig)
                    ),
                    Err(e) => {
                        eprintln!("deemo: port {port}: cannot signal {subject}: {e}");
                        failed = true;
                    }
                }
                continue;
            }

            let unit = stop_method(group);
            match terminate(target) {
                StopOutcome::Terminated => {
                    eprintln!("deemo: port {port}: stopped {subject} ({unit})")
                }
                StopOutcome::Killed => {
                    eprintln!("deemo: port {port}: stopped {subject} (SIGKILL after grace period)")
                }
                StopOutcome::Stubborn => {
                    failed = true;
                    eprintln!("deemo: port {port}: {subject} survived SIGTERM and SIGKILL");
                }
            }
        }
    }

    for port in &empty {
        eprintln!("deemo: nothing is using port {port}");
    }
    if !empty.is_empty() {
        failed = true;
    }

    // Housekeeping: a killed pid may have been a deemo-managed process —
    // drop the registration of everything that just died, like ps/stop do.
    sweep_dead(home);
    i32::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_marks_the_cut() {
        assert_eq!(truncate("short", 8), "short");
        assert_eq!(truncate("exactly-8!", 10), "exactly-8!");
        assert_eq!(truncate("0123456789", 8), "0123456…");
        assert_eq!(truncate("日本語のラベル名", 5), "日本語の…");
    }

    #[test]
    fn ports_in_finds_the_flag_forms() {
        assert_eq!(ports_in("pnpm dev --port 3010"), ["3010"]);
        assert_eq!(
            ports_in("astro preview --host 127.0.0.1 --port=4322"),
            ["4322"]
        );
        assert_eq!(ports_in("serve -p 8080"), ["8080"]);
        assert_eq!(ports_in("serve -p8080"), ["8080"]);
        assert_eq!(ports_in("PORT=9000 node server.mjs"), ["9000"]);
        assert_eq!(
            ports_in("pnpm --filter admin dev --port 8181 --strictPort"),
            ["8181"]
        );
    }

    #[test]
    fn ports_in_skips_garbage_and_dedupes() {
        assert!(ports_in("node server/server.mjs").is_empty());
        assert!(ports_in("cmd --port nope").is_empty());
        assert!(ports_in("cmd --port 0").is_empty());
        assert!(ports_in("cmd --port 70000").is_empty());
        assert!(ports_in("cmd -path/to/x --port").is_empty());
        assert_eq!(ports_in("x --port 3000 -p 3000 --port=3000"), ["3000"]);
        assert_eq!(ports_in("x --port 3000 --port 4322"), ["3000", "4322"]);
    }

    #[test]
    fn age_uses_the_coarsest_unit() {
        let a = |s| format_age(std::time::Duration::from_secs(s));
        assert_eq!(a(45), "45s");
        assert_eq!(a(60 * 12), "12m");
        assert_eq!(a(3_600 * 5), "5h");
        assert_eq!(a(86_400 * 3), "3d");
        assert_eq!(a(86_400 * 7 * 2), "2w");
    }

    #[test]
    fn age_of_parses_the_registered_stamp() {
        // The registration format register() writes.
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        assert!(age_of(&now).unwrap() < std::time::Duration::from_secs(5));
        assert!(age_of("not-a-time").is_none());
    }
}
