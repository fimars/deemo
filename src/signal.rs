//! Signal names for `-s`, spelled the way POSIX `kill -s SIG` and `pkill`
//! spell them (`TERM`, `SIGTERM`, `sigterm`, `-9` all reach the same signal).

/// deemo's signal vocabulary, canonical name first so `display` reports the
/// name people type (`IOT`/`CLD`/`POLL` are aliases of an earlier row).
#[cfg(unix)]
const SIGNALS: &[(&str, i32)] = &[
    ("HUP", libc::SIGHUP),
    ("INT", libc::SIGINT),
    ("QUIT", libc::SIGQUIT),
    ("ILL", libc::SIGILL),
    ("TRAP", libc::SIGTRAP),
    ("ABRT", libc::SIGABRT),
    ("IOT", libc::SIGABRT),
    ("BUS", libc::SIGBUS),
    ("FPE", libc::SIGFPE),
    ("KILL", libc::SIGKILL),
    ("USR1", libc::SIGUSR1),
    ("SEGV", libc::SIGSEGV),
    ("USR2", libc::SIGUSR2),
    ("PIPE", libc::SIGPIPE),
    ("ALRM", libc::SIGALRM),
    ("TERM", libc::SIGTERM),
    ("CHLD", libc::SIGCHLD),
    ("CLD", libc::SIGCHLD),
    ("CONT", libc::SIGCONT),
    ("STOP", libc::SIGSTOP),
    ("TSTP", libc::SIGTSTP),
    ("TTIN", libc::SIGTTIN),
    ("TTOU", libc::SIGTTOU),
    ("URG", libc::SIGURG),
    ("XCPU", libc::SIGXCPU),
    ("XFSZ", libc::SIGXFSZ),
    ("VTALRM", libc::SIGVTALRM),
    ("PROF", libc::SIGPROF),
    ("WINCH", libc::SIGWINCH),
    ("IO", libc::SIGIO),
    ("POLL", libc::SIGIO),
    ("SYS", libc::SIGSYS),
];

/// Parse what `kill`/`pkill` accept after `-s`: a name (with or without the
/// `SIG` prefix, any case) or a number. `0` is rejected on purpose — it is
/// `kill`'s existence probe, not a signal that can be delivered.
#[cfg(unix)]
pub fn parse(text: &str) -> Result<i32, String> {
    let unsigned = text.trim().trim_start_matches('-');
    if let Ok(num) = unsigned.parse::<i32>() {
        return (1..=64)
            .contains(&num)
            .then_some(num)
            .ok_or_else(|| format!("signal number {num} is out of range (1-64)"));
    }
    let upper = unsigned.to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    SIGNALS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, sig)| *sig)
        .ok_or_else(|| format!("unknown signal {text:?} (try TERM, HUP, KILL, or a number)"))
}

#[cfg(not(unix))]
pub fn parse(_text: &str) -> Result<i32, String> {
    // Windows has no per-process signal delivery; deemo stops processes with
    // taskkill there, which is always the forced variant.
    Err("sending an explicit signal needs a unix platform (drop -s)".to_string())
}

/// `SIGTERM`, or `signal 42` when the number is outside the table.
#[cfg(unix)]
pub fn display(sig: i32) -> String {
    match SIGNALS.iter().find(|(_, s)| *s == sig) {
        Some((name, _)) => format!("SIG{name}"),
        None => format!("signal {sig}"),
    }
}

#[cfg(not(unix))]
pub fn display(sig: i32) -> String {
    format!("signal {sig}")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn parses_names_prefixes_aliases_and_numbers() {
        assert_eq!(parse("TERM").unwrap(), libc::SIGTERM);
        assert_eq!(parse("SIGTERM").unwrap(), libc::SIGTERM);
        assert_eq!(parse("sigterm").unwrap(), libc::SIGTERM);
        assert_eq!(parse("-TERM").unwrap(), libc::SIGTERM);
        assert_eq!(parse("HUP").unwrap(), libc::SIGHUP);
        assert_eq!(parse("9").unwrap(), 9);
        assert_eq!(parse("-9").unwrap(), 9);
        assert_eq!(parse("IOT").unwrap(), libc::SIGABRT, "alias resolves");
    }

    #[test]
    fn rejects_nonsense_and_the_existence_probe() {
        assert!(parse("NOPE").is_err());
        assert!(parse("0").is_err(), "signal 0 is kill's liveness probe");
        assert!(parse("65").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn displays_the_canonical_name() {
        assert_eq!(display(libc::SIGTERM), "SIGTERM");
        assert_eq!(display(9), "SIGKILL");
        assert_eq!(display(42), "signal 42");
    }
}
