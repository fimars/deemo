//! Process registry for detached runs: a small pid-file store under
//! `$DEEMO_HOME/run/` plus listing, liveness probing and stopping.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub struct Entry {
    pub pid: u32,
    pub label: String,
    pub started: String,
    pub cmd: String,
    pub log: PathBuf,
    pub file: PathBuf, // the pid-file itself
}

pub fn run_dir(home: &Path) -> PathBuf {
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

/// Is this pid alive? (Zombies are impossible here: detached children are
/// reaped by init/launchd after deemo exits. Pid reuse within a short window
/// is theoretically possible and accepted.)
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
pub fn alive(_pid: u32) -> bool {
    // Windows has no cheap std-only liveness probe; reported as "unknown"
    // rather than guessing. `deemo stop` still works via taskkill.
    false
}

/// All registered entries + whether each process is still alive.
pub fn list(home: &Path) -> Vec<(Entry, bool)> {
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

/// Stop all processes with the given label(s). Returns an exit code.
pub fn stop(home: &Path, labels: &[String]) -> i32 {
    let all = list(home);
    let mut missing = Vec::new();
    for label in labels {
        let mut hits = 0usize;
        for (entry, alive) in &all {
            if &entry.label != label {
                continue;
            }
            hits += 1;
            if *alive {
                kill(entry.pid);
                eprintln!(
                    "deemo: stopped {} (pid {}, SIGTERM)",
                    entry.label, entry.pid
                );
            } else {
                eprintln!("deemo: {} (pid {}) is not running", entry.label, entry.pid);
            }
            let _ = fs::remove_file(&entry.file);
        }
        if hits == 0 {
            missing.push(label.clone());
        }
    }
    for label in &missing {
        eprintln!("deemo: no process with label '{label}'");
    }
    if missing.is_empty() {
        0
    } else {
        1
    }
}

#[cfg(unix)]
fn kill(pid: u32) {
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
}

#[cfg(not(unix))]
fn kill(pid: u32) {
    // No std-only signal API on Windows; taskkill /F is the pragmatic route.
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status();
}

/// Remove pid-files whose processes are no longer alive (housekeeping for ps).
pub fn sweep_dead(home: &Path) {
    for (entry, alive) in list(home) {
        if !alive {
            let _ = fs::remove_file(entry.file);
        }
    }
}

/// Newest log file for a label (filenames embed the timestamp, so sorting
/// names sorts by time). `--timestamp off` resolves to the single file.
pub fn latest_log(home: &Path, label: &str) -> Option<PathBuf> {
    let logs = home.join("logs");
    let mut best: Option<PathBuf> = None;
    if let Ok(rd) = fs::read_dir(&logs) {
        for p in rd.filter_map(|e| e.ok()).map(|e| e.path()) {
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let matches = name.starts_with(&format!("{label}-")) || name == format!("{label}.log");
            if matches && best.as_ref().is_none_or(|b| b.file_name() > p.file_name()) {
                best = Some(p);
            }
        }
    }
    best
}

/// Print the path and the last ~4 KiB of a log file.
pub fn show_log(path: &Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(4096);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    println!("{}", path.display());
    if start > 0 {
        println!("… (showing last {} of {} bytes)", buf.len(), len);
    }
    print!("{}", String::from_utf8_lossy(&buf));
    if !buf.ends_with(b"\n") {
        println!();
    }
    Ok(())
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

/// `deemo stop ...`
pub fn stop_cmd(home: &Path, labels: &[String]) -> i32 {
    stop(home, labels)
}

/// `deemo logs <LABEL>`
pub fn logs_cmd(home: &Path, label: &str) -> i32 {
    match latest_log(home, label) {
        Some(path) => match show_log(&path) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("deemo: {e:#}");
                1
            }
        },
        None => {
            eprintln!("deemo: no log file found for label '{label}'");
            1
        }
    }
}
