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
//!
//! macOS answers this through libproc instead of an `lsof` exec, and keeps
//! `lsof` only as the fallback; DESIGN.md "Kill is a port" has the why.

use std::io;

/// Pids of every process with a socket bound locally to any of `ports` (TCP +
/// UDP, IPv4 + IPv6, any state): one list per port, in request order, each
/// sorted and deduplicated. The scan is paid once, not once per port — `kill
/// 8000 3000` walks the process table once.
pub fn holders(ports: &[u16]) -> io::Result<Vec<Vec<u32>>> {
    let mut found = holders_impl(ports)?;
    for pids in &mut found {
        pids.sort_unstable();
        pids.dedup();
    }
    Ok(found)
}

/// Slot of `port` in the request, for the parsers that only see one address.
fn slot_of(ports: &[u16], port: u16) -> Option<usize> {
    ports.iter().position(|&p| p == port)
}

/// macOS/iOS: walk the process table through libproc instead of paying the
/// ~25 ms an `lsof` exec costs. `lsof` stays as the fallback.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn holders_impl(ports: &[u16]) -> io::Result<Vec<Vec<u32>>> {
    match libproc_holders(ports) {
        // A usable walk is an answer, including "nobody holds it".
        Ok(Some(found)) => Ok(found),
        // The process table was unreadable, or not one fd looked like the
        // layout we parse: ask lsof rather than report a busy port as free.
        Ok(None) | Err(_) => lsof_holders(ports),
    }
}

/// macOS/iOS: every process's socket fds, matched on the local port.
/// `Ok(None)` = the fd layout is not what this code expects (fall back).
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn libproc_holders(ports: &[u16]) -> io::Result<Option<Vec<Vec<u32>>>> {
    // `proc_listallpids(NULL, 0)` returns the number of processes; the second
    // call fills the buffer (the return value is a count, not bytes).
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Err(io::Error::other("proc_listallpids failed"));
    }
    let mut pids = vec![0 as libc::pid_t; count as usize];
    let listed = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids[..]) as _,
        )
    };
    if listed <= 0 {
        return Err(io::Error::other("proc_listallpids failed"));
    }
    pids.truncate(listed as usize);

    let mut fd_bytes: Vec<u8> = Vec::new();
    let mut sockets = 0u32;
    let mut recognized = 0u32;
    let mut found = vec![Vec::new(); ports.len()];
    for &pid in &pids {
        if pid <= 0 {
            continue;
        }
        let Some(fds) = list_fds(pid, &mut fd_bytes) else {
            continue; // exited, or another user's process
        };
        for fd in fd_bytes[..fds].chunks_exact(FD_INFO_SIZE) {
            if u32_at(fd, 4) != libc::PROX_FDTYPE_SOCKET as u32 {
                continue;
            }
            sockets += 1;
            let Some(info) = socket_info(pid, i32_at(fd, 0)) else {
                continue;
            };
            let kind = i32_at(&info, OFF_KIND);
            if !(SOCKINFO_GENERIC..=SOCKINFO_VSOCK).contains(&kind) {
                continue;
            }
            recognized += 1;
            // Only these two arms of `soi_proto` start with an `in_sockinfo`.
            // A unix socket starts with kernel pointers, vsock keeps host-order
            // ports, kern_ctl an id — reading `insi_lport` there would match a
            // random port (a 0 in that word would make `kill 0` hit everything).
            if kind != SOCKINFO_IN && kind != SOCKINFO_TCP {
                continue;
            }
            // Only the local side: a client of this port has a random one.
            let lport = u16::from_be(i32_at(&info, OFF_LPORT) as u16);
            if let Some(slot) = slot_of(ports, lport) {
                found[slot].push(pid as u32);
            }
        }
    }
    // The const asserts catch a layout move at build time; this catches one at
    // run time (fds from a kernel whose fields we do not recognize at all).
    if sockets > 0 && recognized == 0 {
        return Ok(None);
    }
    Ok(Some(found))
}

/// fd records of one process. `None` when it is gone or unreadable. Grows the
/// buffer while the kernel fills it completely (the return value is bytes).
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn list_fds(pid: libc::pid_t, buf: &mut Vec<u8>) -> Option<usize> {
    if buf.is_empty() {
        buf.resize(FD_INFO_SIZE * 1024, 0);
    }
    loop {
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                buf.as_mut_ptr().cast(),
                buf.len() as libc::c_int,
            )
        };
        if filled <= 0 {
            return None;
        }
        let filled = filled as usize;
        if filled < buf.len() || buf.len() >= FD_INFO_SIZE * 32_768 {
            return Some(filled);
        }
        buf.resize(buf.len() * 2, 0);
    }
}

/// `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)` for one fd.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn socket_info(pid: libc::pid_t, fd: i32) -> Option<[u8; SOCKET_FDINFO_SIZE]> {
    let mut info = [0u8; SOCKET_FDINFO_SIZE];
    let filled = unsafe {
        libc::proc_pidfdinfo(
            pid,
            fd,
            PROC_PIDFDSOCKETINFO,
            info.as_mut_ptr().cast(),
            SOCKET_FDINFO_SIZE as libc::c_int,
        )
    };
    (filled as usize == SOCKET_FDINFO_SIZE).then_some(info)
}

/// A native-endian 4-byte field out of a kernel-filled buffer (the only field
/// width this file reads).
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn i32_at(buf: &[u8], offset: usize) -> i32 {
    i32::from_ne_bytes(buf[offset..offset + 4].try_into().expect("4 bytes"))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn u32_at(buf: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(buf[offset..offset + 4].try_into().expect("4 bytes"))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
const FD_INFO_SIZE: usize = std::mem::size_of::<libc::proc_fdinfo>(); // 8

/// `PROC_PIDFDSOCKETINFO` from `sys/proc_info.h` (libc stops at LISTFDS).
#[cfg(any(target_os = "macos", target_os = "ios"))]
const PROC_PIDFDSOCKETINFO: libc::c_int = 3;

/// `enum SOCKINFO_*` from `sys/proc_info.h`. `_GENERIC..=_VSOCK` is the range
/// of kinds that still means "this fd is laid out as we expect"; only `_IN`
/// (UDP) and `_TCP` carry the `in_sockinfo` this code reads a port from.
#[cfg(any(target_os = "macos", target_os = "ios"))]
const SOCKINFO_GENERIC: i32 = 0;
#[cfg(any(target_os = "macos", target_os = "ios"))]
const SOCKINFO_IN: i32 = 1;
#[cfg(any(target_os = "macos", target_os = "ios"))]
const SOCKINFO_TCP: i32 = 2;
#[cfg(any(target_os = "macos", target_os = "ios"))]
const SOCKINFO_VSOCK: i32 = 7;

/// The part of `struct socket_fdinfo` (sys/proc_info.h) this code reads. The
/// header's `socket_info` is 768 bytes inside it and `proc_fileinfo` is 24 in
/// front. `insi_lport` is the second field of the `soi_proto` union for both
/// `SOCKINFO_IN` (UDP) and `SOCKINFO_TCP`, because a `tcp_sockinfo` starts
/// with an `in_sockinfo`.
#[cfg(any(target_os = "macos", target_os = "ios"))]
#[repr(C)]
struct SocketFdInfo {
    pfi: [u8; 24],
    psi: SocketInfo,
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[repr(C)]
struct SocketInfo {
    soi_stat: [u8; 136],
    soi_so: u64,
    soi_pcb: u64,
    soi_type: i32,
    soi_protocol: i32,
    soi_family: i32,
    soi_options: i16,
    soi_linger: i16,
    soi_state: i16,
    soi_qlen: i16,
    soi_incqlen: i16,
    soi_qlimit: i16,
    soi_timeo: i16,
    soi_error: u16,
    soi_oobmark: u32,
    soi_rcv: [u8; 24],
    soi_snd: [u8; 24],
    soi_kind: i32,
    rfu_1: u32,
    insi_fport: i32,
    insi_lport: i32,
    rfu_2: [u8; 768 - 248],
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
const SOCKET_FDINFO_SIZE: usize = std::mem::size_of::<SocketFdInfo>();
#[cfg(any(target_os = "macos", target_os = "ios"))]
const OFF_KIND: usize =
    std::mem::offset_of!(SocketFdInfo, psi) + std::mem::offset_of!(SocketInfo, soi_kind);
#[cfg(any(target_os = "macos", target_os = "ios"))]
const OFF_LPORT: usize =
    std::mem::offset_of!(SocketFdInfo, psi) + std::mem::offset_of!(SocketInfo, insi_lport);

#[cfg(any(target_os = "macos", target_os = "ios"))]
const _: () = {
    assert!(std::mem::size_of::<SocketInfo>() == 768);
    assert!(SOCKET_FDINFO_SIZE == 792);
    assert!(OFF_KIND == 256);
    assert!(OFF_LPORT == 268);
};

/// Linux: no external tool needed — the kernel lays the socket tables out in
/// `/proc/net/{tcp,tcp6,udp,udp6}` (one inode per socket) and
/// `/proc/<pid>/fd` tells us which process owns an inode.
#[cfg(target_os = "linux")]
fn holders_impl(ports: &[u16]) -> io::Result<Vec<Vec<u32>>> {
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
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
        socket_inodes(&text, ports, &mut index);
    }
    if !any_table {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "/proc/net/* is not readable, so port holders cannot be listed",
        ));
    }
    let mut found = vec![Vec::new(); ports.len()];
    if index.is_empty() {
        return Ok(found); // nobody binds any of them: no need to walk /proc
    }
    pids_with_sockets(&index, &mut found);
    Ok(found)
}

/// macOS/BSD ship lsof in the base system; macOS uses it only as the fallback
/// for the libproc walk. Field output (`-F pn`) instead of the default table:
/// one `p<pid>` record marker plus an `n<address>` line per file — robust
/// against process names containing spaces, which shift the columns of the
/// tabular format. Several `-i` filters are ORed, so one exec covers every
/// requested port.
#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "macos", target_os = "ios"))
))]
fn holders_impl(ports: &[u16]) -> io::Result<Vec<Vec<u32>>> {
    lsof_holders(ports)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn lsof_holders(ports: &[u16]) -> io::Result<Vec<Vec<u32>>> {
    let mut cmd = std::process::Command::new("lsof");
    cmd.args(["-nP", "-F", "pn"]);
    for port in ports {
        cmd.args(["-i", &format!(":{port}")]);
    }
    let out = cmd.output().map_err(|e| {
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
    Ok(pids_from_lsof(&String::from_utf8_lossy(&out.stdout), ports))
}

/// Windows: `netstat -ano` is the portable port table; the owning pid is the
/// last column (UDP has no state column, hence "last", not "fifth").
#[cfg(windows)]
fn holders_impl(ports: &[u16]) -> io::Result<Vec<Vec<u32>>> {
    let out = std::process::Command::new("netstat")
        .arg("-ano")
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("cannot run netstat: {e}")))?;
    Ok(pids_from_netstat(
        &String::from_utf8_lossy(&out.stdout),
        ports,
    ))
}

/// The local port of an address spec.
///
/// Accepts what the three backends emit: `*:8000`, `127.0.0.1:8000`,
/// `[::1]:8000`, a peer after `->` and a trailing state such as
/// `(LISTEN)`. Only the part *before* `->` counts — that is the side the
/// process bound itself to.
#[cfg(any(test, not(target_os = "linux")))]
fn local_port(addr: &str) -> Option<u16> {
    let first = addr.split_whitespace().next()?; // drop " (LISTEN)"
    let local = first.split_once("->").map_or(first, |(l, _)| l); // drop the peer
    local.rsplit_once(':')?.1.parse().ok() // no host:port shape → None
}

/// Parse `lsof -F pn` output: `p<pid>` opens a record, `n<address>` names
/// one file of that process.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
fn pids_from_lsof(text: &str, ports: &[u16]) -> Vec<Vec<u32>> {
    let mut found = vec![Vec::new(); ports.len()];
    let mut pid: Option<u32> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            pid = rest.parse().ok();
        } else if let Some(name) = line.strip_prefix('n') {
            let slot = pid.zip(local_port(name).and_then(|p| slot_of(ports, p)));
            if let Some((pid, slot)) = slot {
                found[slot].push(pid);
            }
        }
    }
    found
}

/// Parse `netstat -ano`: proto, local address, remote address, [state,] pid.
#[cfg(any(test, not(unix)))]
fn pids_from_netstat(text: &str, ports: &[u16]) -> Vec<Vec<u32>> {
    let mut found = vec![Vec::new(); ports.len()];
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (Some(local), Some(pid)) = (fields.get(1), fields.last()) else {
            continue;
        };
        let slot = local_port(local).and_then(|p| slot_of(ports, p));
        if let (Some(slot), Ok(pid)) = (slot, pid.parse()) {
            found[slot].push(pid);
        }
    }
    found
}

/// Map socket inodes in one `/proc/net/*` table to the slot of the port they
/// are bound to. Line 0 is the header; `local_address` (field 1) is
/// `<hex addr>:<hex port>`, the inode is field 9.
#[cfg(any(test, target_os = "linux"))]
fn socket_inodes(table: &str, ports: &[u16], index: &mut std::collections::HashMap<String, usize>) {
    for line in table.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some((_, hex_port)) = fields.get(1).and_then(|f| f.rsplit_once(':')) else {
            continue;
        };
        let Some(slot) = u16::from_str_radix(hex_port, 16)
            .ok()
            .and_then(|p| slot_of(ports, p))
        else {
            continue;
        };
        if let Some(inode) = fields.get(9) {
            index.entry(inode.to_string()).or_insert(slot);
        }
    }
}

/// The processes owning any of these socket inodes (the fd symlink reads
/// `socket:[<inode>]`). fd directories we may not read belong to other
/// users — skipped, exactly like an unprivileged `lsof` skips them.
#[cfg(target_os = "linux")]
fn pids_with_sockets(index: &std::collections::HashMap<String, usize>, found: &mut [Vec<u32>]) {
    let slots = index.values().max().map_or(0, |slot| slot + 1);
    let mut hits: Vec<usize> = Vec::new();
    let Ok(dirs) = std::fs::read_dir("/proc") else {
        return;
    };
    for dir in dirs.flatten() {
        let Ok(pid) = dir.file_name().to_str().unwrap_or_default().parse::<u32>() else {
            continue; // not a pid directory
        };
        let Ok(fds) = std::fs::read_dir(dir.path().join("fd")) else {
            continue;
        };
        hits.clear();
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let inode = link
                .to_str()
                .and_then(|l| l.strip_prefix("socket:[").and_then(|i| i.strip_suffix(']')));
            let Some(&slot) = inode.and_then(|i| index.get(i)) else {
                continue;
            };
            // One process can hold sockets of several requested ports; stop
            // only once every port present in the tables has matched.
            if !hits.contains(&slot) {
                hits.push(slot);
                if hits.len() == slots {
                    break;
                }
            }
        }
        for &slot in &hits {
            found[slot].push(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_port_reads_only_the_binding_side() {
        assert_eq!(local_port("*:8000"), Some(8000));
        assert_eq!(local_port("127.0.0.1:8000"), Some(8000));
        assert_eq!(local_port("[::1]:8000 (LISTEN)"), Some(8000));
        assert_eq!(
            local_port("*:8000->10.0.0.9:53144 (ESTABLISHED)"),
            Some(8000)
        );
        assert_eq!(local_port("*:8001"), Some(8001));
        assert_ne!(
            local_port("*:18000"),
            Some(8000),
            "suffix match is not enough"
        );
        // a client that connects TO the port has a random local port
        assert_ne!(local_port("127.0.0.1:53144->127.0.0.1:8000"), Some(8000));
        assert_eq!(local_port("PIPE"), None);
        assert_eq!(local_port(""), None);
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
        assert_eq!(pids_from_lsof(out, &[8000]), vec![vec![101, 103]]);
        assert_eq!(pids_from_lsof(out, &[9000]), vec![Vec::<u32>::new()]);
        // every requested port gets its own list from one exec
        assert_eq!(
            pids_from_lsof(out, &[9000, 8000]),
            vec![Vec::<u32>::new(), vec![101, 103]]
        );
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
        assert_eq!(pids_from_netstat(out, &[8000]), vec![vec![111, 222]]); // 333 is a client
        assert_eq!(pids_from_netstat(out, &[5000]), vec![vec![444]]);
        assert_eq!(pids_from_netstat(out, &[9000]), vec![Vec::<u32>::new()]);
    }

    // Not gated to linux: the parser is pure string handling, and running it
    // on every platform is what keeps a fixture bug from hiding behind a cfg.
    #[test]
    fn procfs_tables_are_matched_on_the_hex_local_port() {
        // 0x1F40 = 8000, 0x0016 = 22, 0xC001 = 49153 (row 1's peer port)
        let table = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 123456\n\
   1: 0100007F:0016 0100007F:C001 01 00000000:00000000 00:00000000 00000000     0        0 654321\n";
        let mut index = std::collections::HashMap::new();
        socket_inodes(table, &[8000], &mut index);
        assert_eq!(index.get("123456"), Some(&0));
        assert_eq!(index.len(), 1);
        // only the local side counts: row 1 binds 22 locally …
        index.clear();
        socket_inodes(table, &[22], &mut index);
        assert_eq!(index.get("654321"), Some(&0));
        // … while its peer port 49153 belongs to the other end of nothing here
        index.clear();
        socket_inodes(table, &[0xC001], &mut index);
        assert!(index.is_empty());
        // one read of the table covers every requested port
        index.clear();
        socket_inodes(table, &[8000, 22], &mut index);
        assert_eq!(index.get("123456"), Some(&0));
        assert_eq!(index.get("654321"), Some(&1));
    }
}
