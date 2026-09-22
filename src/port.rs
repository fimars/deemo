//! Which processes hold a TCP/UDP port — the `lsof -nP -i :PORT` /
//! `fuser PORT/tcp` lookup, without the pipeline.
//!
//! Only sockets whose **local** address carries the port are reported: a
//! client that merely connects *to* the port has some random local port and
//! must not be caught in the blast radius. (`lsof -i :PORT` matches either
//! side of a connection; deemo filters to the binding side, like `fuser`.)
//!
//! Without root only your own processes are visible — other users' sockets
//! live behind file descriptors we may not read. Same limitation as lsof/fuser.

use std::io;

/// Pids of every process with a socket bound locally to `port` (TCP + UDP,
/// IPv4 + IPv6, any state), sorted and deduplicated.
pub fn holders(port: u16) -> io::Result<Vec<u32>> {
    let mut pids = holders_impl(port)?;
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

/// Linux: no external tool needed — the kernel lays the socket tables out in
/// `/proc/net/{tcp,tcp6,udp,udp6}` (one inode per socket) and
/// `/proc/<pid>/fd` tells us which process owns an inode.
#[cfg(target_os = "linux")]
fn holders_impl(port: u16) -> io::Result<Vec<u32>> {
    let mut inodes = std::collections::HashSet::new();
    let mut any_table = false;
    for table in [
        "/proc/net/tcp",
        "/proc/net/tcp6",
        "/proc/net/udp",
        "/proc/net/udp6",
    ] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        any_table = true;
        inodes.extend(socket_inodes(&text, port));
    }
    if !any_table {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "/proc/net/* is not readable, so port holders cannot be listed",
        ));
    }
    if inodes.is_empty() {
        return Ok(Vec::new());
    }
    Ok(pids_with_sockets(&inodes))
}

/// macOS/BSD ship lsof in the base system. Field output (`-F pn`) instead of
/// the default table: one `p<pid>` record marker plus an `n<address>` line
/// per file — robust against process names containing spaces, which shift
/// the columns of the tabular format.
#[cfg(all(unix, not(target_os = "linux")))]
fn holders_impl(port: u16) -> io::Result<Vec<u32>> {
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-F", "pn", "-i"])
        .arg(format!(":{port}"))
        .output()
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "lsof not found in PATH (needed to list port holders on this platform)",
                )
            } else {
                e
            }
        })?;
    // Empty stdout with complaints on stderr is a real failure; an empty
    // result with a clean stderr simply means nobody holds the port (lsof
    // may even exit non-zero for that).
    if out.stdout.is_empty() && !out.stderr.is_empty() {
        return Err(io::Error::other(format!(
            "lsof: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(pids_from_lsof(&String::from_utf8_lossy(&out.stdout), port))
}

/// Windows: `netstat -ano` is the portable port table; the owning pid is the
/// last column (UDP has no state column, hence "last", not "fifth").
#[cfg(windows)]
fn holders_impl(port: u16) -> io::Result<Vec<u32>> {
    let out = std::process::Command::new("netstat")
        .arg("-ano")
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("cannot run netstat: {e}")))?;
    Ok(pids_from_netstat(
        &String::from_utf8_lossy(&out.stdout),
        port,
    ))
}

/// The local side of an address spec names `port`?
///
/// Accepts what the three backends emit: `*:8000`, `127.0.0.1:8000`,
/// `[::1]:8000`, a peer after `->` and a trailing state such as
/// `(LISTEN)`. Only the part *before* `->` counts — that is the side the
/// process bound itself to.
#[cfg(any(test, not(target_os = "linux")))]
fn local_port(addr: &str, port: u16) -> bool {
    let first = addr.split_whitespace().next().unwrap_or(""); // drop " (LISTEN)"
    let local = first.split_once("->").map_or(first, |(l, _)| l); // drop the peer
    let Some((_, p)) = local.rsplit_once(':') else {
        return false; // no host:port shape at all (a pipe, a path, …)
    };
    p.parse() == Ok(port)
}

/// Parse `lsof -F pn` output: `p<pid>` opens a record, `n<address>` names
/// one file of that process.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
fn pids_from_lsof(text: &str, port: u16) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut pid: Option<u32> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            pid = rest.parse().ok();
        } else if let Some(name) = line.strip_prefix('n') {
            if let Some(p) = pid {
                if local_port(name, port) {
                    pids.push(p);
                }
            }
        }
    }
    pids
}

/// Parse `netstat -ano`: proto, local address, remote address, [state,] pid.
#[cfg(any(test, not(unix)))]
fn pids_from_netstat(text: &str, port: u16) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (Some(local), Some(pid)) = (fields.get(1), fields.last()) else {
            continue;
        };
        if local_port(local, port) {
            if let Ok(pid) = pid.parse() {
                pids.push(pid);
            }
        }
    }
    pids
}

/// Socket inodes in one `/proc/net/*` table bound locally to `port`.
/// Line 0 is the header; `local_address` (field 1) is
/// `<hex addr>:<hex port>`, the inode is field 9.
#[cfg(target_os = "linux")]
fn socket_inodes(table: &str, port: u16) -> Vec<String> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (_, hex_port) = fields.get(1)?.rsplit_once(':')?;
            if u16::from_str_radix(hex_port, 16).ok()? != port {
                return None;
            }
            Some(fields.get(9)?.to_string())
        })
        .collect()
}

/// The processes owning any of these socket inodes (the fd symlink reads
/// `socket:[<inode>]`). fd directories we may not read belong to other
/// users — skipped, exactly like an unprivileged `lsof` skips them.
#[cfg(target_os = "linux")]
fn pids_with_sockets(inodes: &std::collections::HashSet<String>) -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(dirs) = std::fs::read_dir("/proc") else {
        return pids;
    };
    'procs: for dir in dirs.flatten() {
        let Ok(pid) = dir.file_name().to_str().unwrap_or_default().parse::<u32>() else {
            continue; // not a pid directory
        };
        let Ok(fds) = std::fs::read_dir(dir.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let inode = link
                .to_str()
                .and_then(|l| l.strip_prefix("socket:[").and_then(|i| i.strip_suffix(']')));
            if inode.is_some_and(|i| inodes.contains(i)) {
                pids.push(pid);
                continue 'procs;
            }
        }
    }
    pids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_port_matches_only_the_binding_side() {
        assert!(local_port("*:8000", 8000));
        assert!(local_port("127.0.0.1:8000", 8000));
        assert!(local_port("[::1]:8000 (LISTEN)", 8000));
        assert!(local_port("*:8000->10.0.0.9:53144 (ESTABLISHED)", 8000));
        assert!(!local_port("*:8001", 8000));
        assert!(!local_port("*:18000", 8000), "suffix match is not enough");
        // a client that connects TO the port has a random local port
        assert!(!local_port("127.0.0.1:53144->127.0.0.1:8000", 8000));
        assert!(!local_port("PIPE", 8000));
        assert!(!local_port("", 8000));
    }

    #[test]
    fn lsof_field_output_reports_binders_not_clients() {
        let out = "\
p101
n*:8000
p102
n127.0.0.1:53144->127.0.0.1:8000
p103
n127.0.0.1:8000->10.0.0.9:53144
n/Users/you/.deemo
";
        assert_eq!(pids_from_lsof(out, 8000), vec![101, 103]);
        assert!(pids_from_lsof(out, 9000).is_empty());
    }

    #[test]
    fn netstat_output_reports_binders_not_clients() {
        let out = "\
Active Connections\n\
\n\
  TCP    0.0.0.0:8000              0.0.0.0:0              LISTENING       111\n\
  TCP    127.0.0.1:8000            127.0.0.1:53144        ESTABLISHED     222\n\
  TCP    127.0.0.1:53145           127.0.0.1:8000         ESTABLISHED     333\n\
  UDP    0.0.0.0:5000              *:*                                    444\n";
        assert_eq!(pids_from_netstat(out, 8000), vec![111, 222]); // 333 is a client
        assert_eq!(pids_from_netstat(out, 5000), vec![444]);
        assert!(pids_from_netstat(out, 9000).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_tables_are_matched_on_the_hex_local_port() {
        // 0x1F90 = 8000, 0x0016 = 22
        let table = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 123456\n\
   1: 0100007F:0016 0100007F:C001 01 00000000:00000000 00:00000000 00000000     0        0 654321\n";
        assert_eq!(socket_inodes(table, 8000), vec!["123456".to_string()]);
        assert!(socket_inodes(table, 8001).is_empty());
    }
}
