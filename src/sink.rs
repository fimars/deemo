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
    /// Attach to an already-created log file (detach/foreground paths).
    pub fn from_path(path: PathBuf) -> Sink {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("log file was just created and must be reopenable");
        Sink { file, path }
    }

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

    /// Append raw bytes (already contains the newline, or EOF-truncated tail).
    #[inline]
    pub fn write_line(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all(bytes)
    }
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
    let prefix = format!("{label}-");
    let exact = format!("{label}.log");

    let mut files: Vec<PathBuf> = fs::read_dir(&logs_dir)
        .with_context(|| format!("failed to read log directory {}", logs_dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                return false;
            };
            name.ends_with(".log") && (name == exact || name.starts_with(&prefix))
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
