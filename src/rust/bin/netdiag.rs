//! Per-socket TCP traffic counters via the sock_diag netlink API (the same source `ss -i`
//! reads), keyed by socket inode so they join onto the /proc/net socket table. Pure libc, no
//! new dependencies, works unprivileged (socket stats are world-readable; only pid attribution
//! needs root). The kernel returns a tcp_info blob per socket; tcpi_bytes_acked (sent) and
//! tcpi_bytes_received live at fixed offsets. UDP has no such counters, so traffic is TCP-only.
//! All protocol parsing is in pure functions driven by byte buffers (unit-tested); only
//! `tcp_traffic` touches a real netlink socket, and any failure degrades to an empty map.

use std::collections::HashMap;

const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLM_F_REQUEST_DUMP: u16 = 0x301; // NLM_F_REQUEST | NLM_F_DUMP
const NLMSG_DONE: u16 = 3;
const NLMSG_ERROR: u16 = 2;
const INET_DIAG_INFO: u16 = 2;
// Fixed offsets of the two u64 counters inside struct tcp_info (linux/tcp.h): the fields before
// them are 8 u8s, then 26 u32s (112 bytes), then 2 u64 pacing rates; naturally aligned.
const TCPI_BYTES_ACKED_OFF: usize = 120;
const TCPI_BYTES_RECEIVED_OFF: usize = 128;

/// One sock_diag dump request for `family` (AF_INET / AF_INET6): nlmsghdr + inet_diag_req_v2
/// asking for every TCP state with the INET_DIAG_INFO extension.
fn build_request(family: u8, seq: u32) -> [u8; 72] {
    let mut b = [0u8; 72];
    b[0..4].copy_from_slice(&72u32.to_ne_bytes()); // nlmsg_len
    b[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    b[6..8].copy_from_slice(&NLM_F_REQUEST_DUMP.to_ne_bytes());
    b[8..12].copy_from_slice(&seq.to_ne_bytes());
    // nlmsg_pid stays 0 (the kernel addresses us by socket)
    b[16] = family; // inet_diag_req_v2.sdiag_family
    b[17] = libc::IPPROTO_TCP as u8;
    b[18] = 1 << (INET_DIAG_INFO - 1); // idiag_ext: request tcp_info
    b[20..24].copy_from_slice(&u32::MAX.to_ne_bytes()); // idiag_states: all
    b
}

/// Parse one recv buffer of netlink messages into `out` ((tx, rx) by inode). Returns true when
/// the dump is finished (NLMSG_DONE or NLMSG_ERROR seen, or the buffer is malformed).
fn parse_dump(buf: &[u8], out: &mut HashMap<u64, (u64, u64)>) -> bool {
    let mut off = 0;
    while off + 16 <= buf.len() {
        let len = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        let kind = u16::from_ne_bytes(buf[off + 4..off + 6].try_into().unwrap());
        if len < 16 || off + len > buf.len() {
            return true; // malformed: stop rather than loop forever
        }
        match kind {
            NLMSG_DONE | NLMSG_ERROR => return true,
            _ => parse_diag_msg(&buf[off + 16..off + len], out)
        }
        off += (len + 3) & !3; // NLMSG_ALIGN
    }
    false
}

/// One inet_diag_msg payload: the socket inode sits at bytes 68..72 (after the 4 header bytes
/// and the 48-byte sockid and three u32s), rtattrs follow from byte 72.
fn parse_diag_msg(msg: &[u8], out: &mut HashMap<u64, (u64, u64)>) {
    if msg.len() < 72 {
        return;
    }
    let inode = u32::from_ne_bytes(msg[68..72].try_into().unwrap()) as u64;
    if inode == 0 {
        return; // TIME_WAIT and friends: nothing to join against
    }
    let mut off = 72;
    while off + 4 <= msg.len() {
        let rta_len = u16::from_ne_bytes(msg[off..off + 2].try_into().unwrap()) as usize;
        let rta_type = u16::from_ne_bytes(msg[off + 2..off + 4].try_into().unwrap());
        if rta_len < 4 || off + rta_len > msg.len() {
            return;
        }
        if rta_type == INET_DIAG_INFO {
            let info = &msg[off + 4..off + rta_len];
            // Older kernels ship a shorter tcp_info; only read counters that are present.
            if info.len() >= TCPI_BYTES_RECEIVED_OFF + 8 {
                let tx = u64::from_ne_bytes(info[TCPI_BYTES_ACKED_OFF..TCPI_BYTES_ACKED_OFF + 8].try_into().unwrap());
                let rx = u64::from_ne_bytes(info[TCPI_BYTES_RECEIVED_OFF..TCPI_BYTES_RECEIVED_OFF + 8].try_into().unwrap());
                // bytes_acked starts at 1 (it counts the SYN); subtract it so idle sockets read 0.
                out.insert(inode, (tx.saturating_sub(1), rx));
            }
            return;
        }
        off += (rta_len + 3) & !3;
    }
}

/// Dump every TCP socket's traffic counters: inode -> (bytes sent, bytes received). Any netlink
/// failure returns what was collected so far (possibly empty); the caller renders blanks.
pub fn tcp_traffic() -> HashMap<u64, (u64, u64)> {
    let mut out = HashMap::new();
    unsafe {
        let fd = libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, libc::NETLINK_SOCK_DIAG);
        if fd < 0 {
            return out;
        }
        for (i, family) in [libc::AF_INET as u8, libc::AF_INET6 as u8].into_iter().enumerate() {
            let req = build_request(family, i as u32 + 1);
            if libc::send(fd, req.as_ptr().cast(), req.len(), 0) < 0 {
                continue;
            }
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0);
                if n <= 0 {
                    break;
                }
                if parse_dump(&buf[..n as usize], &mut out) {
                    break;
                }
            }
        }
        libc::close(fd);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic netlink dump: one diag message for `inode` with a full-size tcp_info
    /// carrying the given counters, followed by NLMSG_DONE.
    fn synthetic_dump(inode: u32, bytes_acked: u64, bytes_received: u64) -> Vec<u8> {
        let info_len = 4 + 200; // rta header + a modern-sized tcp_info
        let msg_len = 16 + 72 + info_len;
        let mut b = vec![0u8; ((msg_len + 3) & !3) + 16];
        b[0..4].copy_from_slice(&(msg_len as u32).to_ne_bytes());
        b[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        b[16 + 68..16 + 72].copy_from_slice(&inode.to_ne_bytes());
        let rta = 16 + 72;
        b[rta..rta + 2].copy_from_slice(&(info_len as u16).to_ne_bytes());
        b[rta + 2..rta + 4].copy_from_slice(&INET_DIAG_INFO.to_ne_bytes());
        let info = rta + 4;
        b[info + TCPI_BYTES_ACKED_OFF..info + TCPI_BYTES_ACKED_OFF + 8].copy_from_slice(&bytes_acked.to_ne_bytes());
        b[info + TCPI_BYTES_RECEIVED_OFF..info + TCPI_BYTES_RECEIVED_OFF + 8].copy_from_slice(&bytes_received.to_ne_bytes());
        let done = (msg_len + 3) & !3;
        b[done..done + 4].copy_from_slice(&16u32.to_ne_bytes());
        b[done + 4..done + 6].copy_from_slice(&NLMSG_DONE.to_ne_bytes());
        b
    }

    #[test]
    fn parses_counters_and_stops_at_done() {
        let buf = synthetic_dump(4242, 1001, 2000);
        let mut out = HashMap::new();
        assert!(parse_dump(&buf, &mut out)); // DONE seen
        assert_eq!(out.get(&4242), Some(&(1000, 2000))); // SYN byte subtracted from tx
    }

    #[test]
    fn short_tcp_info_and_garbage_are_skipped() {
        // A diag message whose tcp_info is too short for the counters contributes nothing.
        let mut buf = synthetic_dump(7, 500, 600);
        // Shrink the rta_len so the info blob ends before the counters.
        let rta = 16 + 72;
        buf[rta..rta + 2].copy_from_slice(&64u16.to_ne_bytes());
        let mut out = HashMap::new();
        parse_dump(&buf, &mut out);
        assert!(out.is_empty());
        // Pure garbage terminates without panicking or looping.
        let mut out2 = HashMap::new();
        assert!(parse_dump(&[0xFFu8; 40], &mut out2));
        assert!(out2.is_empty());
    }

    #[test]
    fn live_dump_sees_own_connection() {
        // Create a real localhost TCP pair; the dump must contain this process's socket inode.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = std::net::TcpStream::connect(addr).unwrap();
        let (_server, _) = listener.accept().unwrap();
        let traffic = tcp_traffic();
        assert!(!traffic.is_empty(), "sock_diag dump returned nothing");
        let me = std::process::id() as i32;
        let inodes = crate::net::inode_map(&[me]);
        assert!(
            inodes.keys().any(|inode| traffic.contains_key(inode)),
            "none of this process's sockets appeared in the dump"
        );
    }
}
