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
fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 && !zombie(pid) }
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
    // proc_pidinfo returns the bytes filled in on success and 0 on failure —
    // and "no such process" is not alive either.
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
fn alive(_pid: u32) -> bool {
    // Windows has no cheap std-only liveness probe; reported as "unknown"
    // rather than guessing. `deemo stop` still works via taskkill.
    false
}

/// All registered entries + whether each process is still alive.
fn list(home: &Path) -> Vec<(Entry, bool)> {
    let dir = run_dir(home);
    let mut entries: Vec<(Entry, bool)> = fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "pid"))
                .filter_map(|p| {
                    let entry = parse_entry(&p)?;
                    let alive = alive(entry.pid);
                    Some((entry, alive))
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
    Killed,
    /// Still alive after SIGKILL (or unsignalable): give up honestly.
    Stubborn,
}

const TERM_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

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
            if alive(entry.pid) {
                match terminate(Target::Group(entry.pid)) {
                    StopOutcome::Terminated => eprintln!(
                        "deemo: stopped {} (pid {}, SIGTERM to process group)",
                        entry.label, entry.pid
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
            } else {
                eprintln!("deemo: {} (pid {}) is not running", entry.label, entry.pid);
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

/// Remove pid-files whose processes are no longer alive (housekeeping for ps).
fn sweep_dead(home: &Path) {
    for (entry, alive) in list(home) {
        if !alive {
            let _ = fs::remove_file(entry.file);
        }
    }
}

/// `deemo ps` — human table. Exit code 1 when nothing is running.
pub fn ps(home: &Path) -> i32 {
    sweep_dead(home);
    let rows = list(home);
    if rows.is_empty() {
        println!("no processes started by deemo");
        return 1;
    }
    println!(
        "{:<16} {:>7}  {:<9} {:<19} COMMAND / LOG",
        "LABEL", "PID", "STATUS", "STARTED"
    );
    for (e, alive) in &rows {
        let status = if *alive { "running" } else { "dead" };
        let detail = format!("{} -> {}", e.cmd, e.log.display());
        println!(
            "{:<16} {:>7}  {:<9} {:<19} {}",
            e.label, e.pid, status, e.started, detail
        );
    }
    0
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
        .filter(|&(_, alive)| alive)
        .map(|(entry, _)| (entry.pid, entry.label))
        .collect();

    let mut failed = false;
    let mut empty: Vec<u16> = Vec::new();
    // Each pid is listed once, under the first port it was found on.
    let mut plan: Vec<(u16, u32)> = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();

    for &port in ports {
        match crate::port::holders(port) {
            Ok(pids) if pids.is_empty() => empty.push(port),
            Ok(pids) => {
                for pid in pids {
                    if seen.insert(pid) {
                        plan.push((port, pid));
                    }
                }
            }
            Err(e) => {
                eprintln!("deemo: cannot list the processes on port {port}: {e}");
                failed = true;
            }
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

            let unit = if group {
                "SIGTERM to process group"
            } else {
                "SIGTERM"
            };
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
