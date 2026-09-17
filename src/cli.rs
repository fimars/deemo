use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Timestamp {
    /// None (just `<label>.log`, later runs append to it)
    Off,
    /// `<label>-YYYYmmdd.log` (one file per day, later runs append)
    Day,
    /// `<label>-YYYYmmdd-HHMMSS.mmm.log` (one file per run; the default)
    Run,
}

#[derive(Debug, Subcommand)]
pub enum Manage {
    /// List processes started by deemo (detached ones may still be running).
    Ps,
    /// Stop detached process(es) by label (sends SIGTERM on unix).
    Stop {
        /// Label(s) of the detached process(es) to stop.
        #[arg(value_name = "LABEL", required = true)]
        labels: Vec<String>,
    },
    /// Show the newest log file of a label: its path plus the last ~4 KiB.
    Logs {
        #[arg(value_name = "LABEL")]
        label: String,
    },
}

#[derive(Debug, Parser)]
#[command(
    name = "deemo",
    version,
    about = "Run (or pipe through) a daemonized process with its output persisted to ~/.deemo.",
    after_help = "Modes:
  deemo -- <CMD> [ARGS...]         detach: CMD runs in the background in its
                                   own session (survives terminal close),
                                   stdout+stderr go to the log file; returns
                                   to the shell immediately
  deemo --foreground -- <CMD>...   stay attached: live passthrough + log,
                                   child's exit code becomes $?
  cmd | deemo                      pipe mode: captures stdout only (POSIX
                                   pipes never carry stderr; use 2>&1)

Management:
  deemo ps                         list started processes
  deemo stop <LABEL>               stop detached process(es) by label
  deemo logs <LABEL>               newest log file of a label, tail included

Examples:
  deemo -- python -m http.server 8000
  deemo --foreground --label httpd -- python -m http.server 8000
  python -m http.server 8000 2>&1 | deemo

Note: when stdout is not a tty many programs block-buffer their output
(detach mode included, since the log file is not a tty). Use `python -u`,
`PYTHONUNBUFFERED=1` or `stdbuf -oL <cmd>` for realtime logs. Ctrl+C in
--foreground drains the stream; press it again to force quit."
)]
pub struct Cli {
    /// Label used for the log filename and `deemo stop`.
    /// Defaults to the child's program name, or `session` in pipe mode.
    #[arg(short, long, value_name = "NAME")]
    pub label: Option<String>,

    /// Passthrough only; do not write any log files.
    #[arg(long)]
    pub yolo: bool,

    /// Log directory. Overrides $DEEMO_HOME; defaults to ~/.deemo.
    #[arg(short, long, value_name = "DIR")]
    pub dir: Option<std::path::PathBuf>,

    /// Log file naming scheme.
    #[arg(long, value_enum, default_value_t = Timestamp::Run)]
    pub timestamp: Timestamp,

    /// Keep at most N newest log files with the same label (0 = unlimited).
    /// Applied on startup.
    #[arg(long, default_value_t = 0, value_name = "N")]
    pub keep: u32,

    /// Foreground only: do not echo the stream to stdout (only write the log).
    #[arg(long)]
    pub quiet: bool,

    /// Pipe mode: fork to the background and return to the shell at once;
    /// the pump keeps running until every pipe writer closes (also covers
    /// upstreams that daemonize in the background). Needs a unix shell.
    #[arg(short = 'B', long)]
    pub background: bool,

    /// Stay attached to this terminal instead of detaching into the
    /// background (implies streaming output here + exit code propagation).
    #[arg(short = 'F', long)]
    pub foreground: bool,

    /// Command to run. Everything after `--` is the command.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "CMD"
    )]
    pub command: Vec<String>,

    /// Management subcommands (ps / stop / logs).
    #[command(subcommand)]
    pub manage: Option<Manage>,
}

impl Cli {
    /// Resolved log label: `--label` > child program name > `session`.
    pub fn label_string(&self) -> String {
        if let Some(label) = &self.label {
            return label.clone();
        }
        if let Some(program) = self.command.first() {
            if let Some(base) = std::path::Path::new(program).file_name() {
                return base.to_string_lossy().into_owned();
            }
        }
        "session".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("deemo").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn label_defaults() {
        assert_eq!(cli(&[]).label_string(), "session"); // pipe mode
        assert_eq!(
            cli(&["--", "python3", "-m", "http.server"]).label_string(),
            "python3"
        ); // child program name
        assert_eq!(cli(&["--label", "x", "--", "sh"]).label_string(), "x");
    }

    #[test]
    fn subcommands_parse() {
        assert!(matches!(cli(&["ps"]).manage, Some(Manage::Ps)));
        assert!(matches!(
            cli(&["stop", "a", "b"]).manage,
            Some(Manage::Stop { .. })
        ));
        assert!(matches!(
            cli(&["logs", "a"]).manage,
            Some(Manage::Logs { .. })
        ));
    }

    #[test]
    fn double_dash_escapes_subcommand_names() {
        // `deemo -- ps` must run the program `ps`, not the management command.
        let parsed = cli(&["--", "ps", "-ef"]);
        assert!(parsed.manage.is_none());
        assert_eq!(parsed.command, vec!["ps", "-ef"]);
    }
}
