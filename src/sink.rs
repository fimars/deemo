use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Resolve the deemo home directory:
/// `--dir` > `$DEEMO_HOME` > `~/.deemo`.
pub fn resolve_home(override_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.to_path_buf();
    }
    if let Some(home) = std::env::var_os("DEEMO_HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".deemo")
}

/// A log file sink. Holds the open file so the main loop can append lines.
pub struct Sink {
    file: fs::File,
    pub path: PathBuf,
}

impl Sink {
    /// Create the logs directory and open this run's log file (append mode,
    /// so re-runs with the same name never clobber each other).
    pub fn open(home: &Path, label: &str, scheme: crate::cli::Timestamp) -> Result<Sink> {
        let logs_dir = home.join("logs");
        fs::create_dir_all(&logs_dir)
            .with_context(|| format!("failed to create log directory {}", logs_dir.display()))?;

        let filename = match scheme {
            crate::cli::Timestamp::Off => format!("{label}.log"),
            crate::cli::Timestamp::Day => {
                format!("{label}-{}.log", chrono::Local::now().format("%Y%m%d"))
            }
            crate::cli::Timestamp::Run => format!(
                "{label}-{}.log",
                chrono::Local::now().format("%Y%m%d-%H%M%S%.3f")
            ),
        };
        let path = logs_dir.join(&filename);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open log file {}", path.display()))?;

        Ok(Sink { file, path })
    }

    /// Append raw bytes (already contains the newline, or an EOF-truncated tail).
    pub fn write_line(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all(bytes)
    }

    /// Hand the open file to a detached child. The path stays for the pid file.
    pub fn into_file(self) -> (fs::File, PathBuf) {
        (self.file, self.path)
    }
}

/// A file deemo would have written for `label` (`<label>.log` or `<label>-*.log`).
fn belongs_to(name: &str, label: &str) -> bool {
    name == format!("{label}.log")
        || (name.starts_with(&format!("{label}-")) && name.ends_with(".log"))
}

/// Delete the oldest log files of the same label, keeping at most `keep`
/// files in total (the current file included — its name carries the newest
/// timestamp, so it sorts last and is never pruned). `keep == 0` means
/// unlimited.
pub fn prune_old_logs(home: &Path, label: &str, keep: u32, current: &Path) -> Result<Vec<PathBuf>> {
    if keep == 0 {
        return Ok(Vec::new());
    }
    let logs_dir = home.join("logs");

    let mut files: Vec<PathBuf> = fs::read_dir(&logs_dir)
        .with_context(|| format!("failed to read log directory {}", logs_dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| belongs_to(name, label))
        })
        .collect();
    files.sort();

    let excess = files.len().saturating_sub(keep as usize);
    let mut removed = Vec::new();
    for path in files.into_iter().take(excess) {
        if path == current {
            continue; // never prune the file we are writing to
        }
        if fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    Ok(removed)
}

/// Newest log for a label. Names embed the timestamp, so the greatest name is
/// the newest run. `--timestamp off` is the single `<label>.log` file.
fn latest_log(home: &Path, label: &str) -> Option<PathBuf> {
    let mut best: Option<PathBuf> = None;
    let entries = fs::read_dir(home.join("logs")).ok()?;
    for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !belongs_to(name, label) {
            continue;
        }
        let newer = best
            .as_ref()
            .is_none_or(|current| path.file_name() > current.file_name());
        if newer {
            best = Some(path);
        }
    }
    best
}

/// Print the path and the last ~4 KiB of a log file.
fn show_log(path: &Path) -> Result<()> {
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

/// `deemo logs <LABEL>`
pub fn logs(home: &Path, label: &str) -> i32 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_name_wins_and_other_labels_do_not() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join("logs");
        fs::create_dir(&logs).unwrap();
        fs::write(logs.join("srv-20200101-000000.000.log"), "old\n").unwrap();
        fs::write(logs.join("srv-20260922-120000.000.log"), "new\n").unwrap();
        fs::write(logs.join("other-20260922-120000.000.log"), "nope\n").unwrap();
        let path = latest_log(home.path(), "srv").unwrap();
        assert_eq!(path.file_name().unwrap(), "srv-20260922-120000.000.log");
    }
}
