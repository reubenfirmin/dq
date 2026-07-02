//! /proc/net socket tables: pure parsing of the kernel's hex-encoded tcp/tcp6/udp/udp6 format,
//! socket-inode to pid attribution via /proc/<pid>/fd, and the net-mode clustering that joins
//! sockets onto identity clusters. Mirrors proc.rs's pattern: impure /proc reads are isolated in
//! thin readers; everything else takes plain data in and out and is unit tested.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::cluster;
use crate::proc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto { Tcp, Udp }

/// Kernel socket states we distinguish (values from include/net/tcp_states.h: 0A=LISTEN,
/// 01=ESTABLISHED). UDP reuses the same field: a bound-but-unconnected UDP socket reads
/// TCP_CLOSE (07), surfaced here as Other(7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SockState { Listen, Established, Other(u8) }

/// One socket table row. `inode` joins it to an owning process via /proc/<pid>/fd.
#[derive(Debug, Clone)]
pub struct Sock {
    pub proto: Proto,
    pub state: SockState,
    pub local: IpAddr,
    pub local_port: u16,
    pub peer: IpAddr,
    pub peer_port: u16,
    pub uid: u32,
    pub inode: u64
}

impl Sock {
    /// "Listening": a TCP socket in LISTEN, or a bound-but-unconnected UDP socket (state
    /// TCP_CLOSE with no peer), which is what serves inbound datagrams.
    pub fn is_listening(&self) -> bool {
        match self.proto {
            Proto::Tcp => self.state == SockState::Listen,
            Proto::Udp => self.state == SockState::Other(7) && self.peer_port == 0
        }
    }

    /// Listening on an unspecified address (0.0.0.0 or ::): reachable on every interface, i.e.
    /// world-exposed as far as this host's binding goes (the firewall may still block it; pq
    /// does not interrogate firewall rules).
    pub fn is_world_exposed(&self) -> bool {
        self.is_listening() && self.local.is_unspecified()
    }

    /// Traffic direction relative to this host: "listen" for listeners, "in" for an established
    /// connection accepted on a port this host listens on, "out" for everything else (ephemeral
    /// local port, so this process initiated the connection). A heuristic, but a solid one: the
    /// only miss is a client that deliberately binds its source port to a locally-listened port.
    pub fn direction(&self, listen_ports: &HashSet<u16>) -> &'static str {
        if self.is_listening() {
            "listen"
        } else if listen_ports.contains(&self.local_port) {
            "in"
        } else {
            "out"
        }
    }

    /// Short human label for previews and -v rows, e.g. "tcp LISTEN  0.0.0.0:8080" or
    /// "tcp ESTAB  192.168.1.5:44322 -> 34.107.243.93:443".
    pub fn label(&self) -> String {
        let proto = match self.proto { Proto::Tcp => "tcp", Proto::Udp => "udp" };
        if self.peer_port == 0 {
            format!("{} {}  {}", proto, self.state_label(), self.local_str())
        } else {
            format!("{} {}  {} -> {}", proto, self.state_label(), self.local_str(), self.peer_str())
        }
    }

    /// Short state string: "LISTEN"/"ESTAB"/"OTHER" for TCP, "BOUND"/"UDP" for UDP (bound vs.
    /// connected), matching what the text report prints for BOUND UDP listeners.
    pub fn state_label(&self) -> &'static str {
        match (self.proto, self.state) {
            (Proto::Tcp, SockState::Listen) => "LISTEN",
            (Proto::Tcp, SockState::Established) => "ESTAB",
            (Proto::Tcp, SockState::Other(_)) => "OTHER",
            (Proto::Udp, _) => if self.is_listening() { "BOUND" } else { "UDP" }
        }
    }

    /// Local endpoint as "ip:port", always present (every socket row has a local address).
    pub fn local_str(&self) -> String {
        format!("{}:{}", self.local, self.local_port)
    }

    /// Peer as "ip:port", or empty when there is no peer (listeners, unconnected UDP), so JSON
    /// and table cells can render it directly.
    pub fn peer_str(&self) -> String {
        if self.peer_port == 0 { String::new() } else { format!("{}:{}", self.peer, self.peer_port) }
    }
}

/// Parse one /proc/net/{tcp,tcp6,udp,udp6} table. Line layout (header skipped):
/// `sl local_address rem_address st tx:rx tr:tm->when retrnsmt uid timeout inode ...`,
/// i.e. fields[1]=local, [2]=peer, [3]=state, [7]=uid, [9]=inode. A line that fails to parse is
/// skipped rather than failing the table. Rows with inode 0 (TIME_WAIT and other kernel-held
/// sockets not attached to any process) are dropped: they cannot be attributed or killed.
pub fn parse_net_table(raw: &str, proto: Proto) -> Vec<Sock> {
    raw.lines().skip(1).filter_map(|line| parse_net_line(line, proto)).collect()
}

fn parse_net_line(line: &str, proto: Proto) -> Option<Sock> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let (local, local_port) = split_addr(fields.get(1)?)?;
    let (peer, peer_port) = split_addr(fields.get(2)?)?;
    let state = match u8::from_str_radix(fields.get(3)?, 16).ok()? {
        0x0A => SockState::Listen,
        0x01 => SockState::Established,
        n => SockState::Other(n)
    };
    let uid: u32 = fields.get(7)?.parse().ok()?;
    let inode: u64 = fields.get(9)?.parse().ok()?;
    if inode == 0 {
        return None;
    }
    Some(Sock { proto, state, local, local_port, peer, peer_port, uid, inode })
}

/// Split "0100007F:1F90" into (address, port). The port is 4 hex chars in natural (big-endian)
/// reading order.
fn split_addr(field: &str) -> Option<(IpAddr, u16)> {
    let (addr_hex, port_hex) = field.split_once(':')?;
    Some((parse_addr(addr_hex)?, u16::from_str_radix(port_hex, 16).ok()?))
}

/// Decode a hex-encoded address as the kernel prints it: 8 chars for IPv4 (one little-endian
/// u32), 32 chars for IPv6 (four little-endian u32 groups). A v4-mapped v6 address collapses to
/// its v4 form so ::ffff:127.0.0.1 and 127.0.0.1 filter and display identically.
fn parse_addr(hex: &str) -> Option<IpAddr> {
    match hex.len() {
        8 => {
            let n = u32::from_str_radix(hex, 16).ok()?;
            Some(IpAddr::V4(Ipv4Addr::from(n.to_le_bytes())))
        }
        32 => {
            let mut bytes = [0u8; 16];
            for i in 0..4 {
                let n = u32::from_str_radix(&hex[i * 8..(i + 1) * 8], 16).ok()?;
                bytes[i * 4..(i + 1) * 4].copy_from_slice(&n.to_le_bytes());
            }
            let v6 = Ipv6Addr::from(bytes);
            Some(match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => IpAddr::V6(v6)
            })
        }
        _ => None
    }
}

/// All TCP/UDP sockets from the four /proc/net tables, plus how many tables were actually
/// readable. An unreadable table contributes nothing; `tables_read == 0` means /proc/net itself
/// is unavailable and the caller decides how loudly to fail.
pub fn read_sockets() -> (Vec<Sock>, usize) {
    let tables = [
        ("/proc/net/tcp", Proto::Tcp), ("/proc/net/tcp6", Proto::Tcp),
        ("/proc/net/udp", Proto::Udp), ("/proc/net/udp6", Proto::Udp)
    ];
    let mut out = Vec::new();
    let mut tables_read = 0;
    for (path, proto) in tables {
        if let Ok(raw) = fs::read_to_string(path) {
            tables_read += 1;
            out.extend(parse_net_table(&raw, proto));
        }
    }
    (out, tables_read)
}

/// Map socket inodes to owning pids by walking /proc/<pid>/fd symlinks ("socket:[12345]").
/// Per-pid failures (permission denied for other users' processes, or the pid exiting mid-scan)
/// are skipped silently, same policy as proc::snapshot(): without root only the caller's own
/// processes attribute, and the report surfaces that as unattributed rows rather than an error.
/// A socket shared across forked processes maps to whichever pid was scanned last.
pub fn inode_map(pids: &[i32]) -> HashMap<u64, i32> {
    let mut map = HashMap::new();
    for &pid in pids {
        let entries = match fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(entries) => entries,
            Err(_) => continue
        };
        for entry in entries.flatten() {
            if let Ok(target) = fs::read_link(entry.path()) {
                if let Some(inode) = target.to_str().and_then(parse_socket_link) {
                    map.insert(inode, pid);
                }
            }
        }
    }
    map
}

/// "socket:[12345]" -> 12345.
fn parse_socket_link(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

/// Pure join of sockets onto owning pids (None = could not attribute).
pub fn attribute(socks: Vec<Sock>, inodes: &HashMap<u64, i32>) -> Vec<(Sock, Option<i32>)> {
    socks.into_iter().map(|s| {
        let pid = inodes.get(&s.inode).copied();
        (s, pid)
    }).collect()
}

/// Apply the report-mode filters: `port` keeps sockets whose LOCAL port matches (any state, so
/// the report answers "who owns port N" broadly; the stricter listeners-only rule for --kill
/// lives in `port_kill_candidates`), and `listen` keeps only listening sockets.
pub fn filter_socks(attributed: Vec<(Sock, Option<i32>)>, port: Option<u16>, listen: bool) -> Vec<(Sock, Option<i32>)> {
    attributed.into_iter()
        .filter(|(s, _)| port.is_none_or(|p| s.local_port == p))
        .filter(|(s, _)| !listen || s.is_listening())
        .collect()
}

/// One identity cluster's network view: the base cluster's cpu/rss/member-count aggregates plus
/// every socket attributed to a member pid.
pub struct NetCluster {
    pub identity: String,
    pub cpu: f64,
    pub rss: u64,
    pub proc_count: usize,
    pub conns: Vec<(Sock, i32)>,
    pub listen_ports: Vec<u16>,
    /// The subset of listen_ports bound to an unspecified address (world-exposed).
    pub world_ports: Vec<u16>
}

impl NetCluster {
    /// Listening sockets in this cluster (TCP LISTEN + bound UDP).
    pub fn listeners(&self) -> usize {
        self.conns.iter().filter(|(s, _)| s.is_listening()).count()
    }

    /// Established (non-listening) sockets in this cluster.
    pub fn established(&self) -> usize {
        self.conns.len() - self.listeners()
    }

    /// Summed (tx, rx) bytes over this cluster's sockets, joined by inode against a sock_diag
    /// traffic map. Sockets without counters (UDP, very old kernels) contribute nothing.
    pub fn traffic(&self, traffic: &HashMap<u64, (u64, u64)>) -> (u64, u64) {
        self.conns.iter().fold((0, 0), |(tx, rx), (s, _)| {
            let (t, r) = traffic.get(&s.inode).copied().unwrap_or((0, 0));
            (tx + t, rx + r)
        })
    }
}

/// Everything the net report renders. Totals are computed over the full (post --port/--listen,
/// pre-pattern) socket set, mirroring how pq's header cpu% stays system-wide under a pattern
/// filter. `unattributed` is display data (hidden under a pattern, which sockets cannot match);
/// `unattributed_count` is always the true count, driving the sudo hint.
pub struct NetReport {
    pub clusters: Vec<NetCluster>,
    pub unattributed: Vec<(u32, Vec<Sock>)>,
    pub total_conns: usize,
    pub total_listening: usize,
    pub unattributed_count: usize,
    /// Every local port with a listener in the (pre-pattern) socket set, for direction tagging.
    pub listen_port_set: HashSet<u16>,
    /// Listeners bound to an unspecified address (world-exposed), pre-pattern.
    pub total_world: usize,
    /// Per-socket (tx, rx) byte counters from sock_diag, keyed by inode (TCP only).
    pub traffic: HashMap<u64, (u64, u64)>
}

/// Join attributed sockets onto identity clusters (reusing `cluster::cluster` for the cpu/rss
/// aggregates so a socket-owning child folds into the same cluster pq shows elsewhere), keeping
/// only clusters that own at least one socket. Sorted by traffic (tx+rx bytes) descending, then
/// connection count, then cpu, so the heaviest talkers lead when counters are available and the
/// order stays sane when they are not (UDP-only, counters unavailable). `pattern` filters
/// clusters exactly like pq's report filter (identity, or any member's comm/cmdline,
/// case-insensitive substring).
pub fn net_clusters(procs: &[proc::Proc], attributed: Vec<(Sock, Option<i32>)>, pattern: Option<&str>, traffic: HashMap<u64, (u64, u64)>) -> NetReport {
    let base = cluster::cluster(procs, cluster::Metric::Cpu);
    let identity_of = cluster::effective_identities(procs);

    let needle = pattern.map(str::to_lowercase);
    let matches: HashMap<&str, bool> = base.iter().map(|c| {
        let ok = needle.as_deref().is_none_or(|n| {
            c.identity.to_lowercase().contains(n)
                || c.members.iter().any(|m| m.cmd.to_lowercase().contains(n) || m.comm.to_lowercase().contains(n))
        });
        (c.identity.as_str(), ok)
    }).collect();

    let total_conns = attributed.len();
    let total_listening = attributed.iter().filter(|(s, _)| s.is_listening()).count();
    let listen_port_set: HashSet<u16> = attributed.iter()
        .filter(|(s, _)| s.is_listening()).map(|(s, _)| s.local_port).collect();
    let total_world = attributed.iter().filter(|(s, _)| s.is_world_exposed()).count();

    let mut by_identity: HashMap<String, Vec<(Sock, i32)>> = HashMap::new();
    let mut unattributed_map: HashMap<u32, Vec<Sock>> = HashMap::new();
    for (sock, pid) in attributed {
        match pid.and_then(|p| identity_of.get(&p).cloned().map(|id| (id, p))) {
            Some((id, p)) => by_identity.entry(id).or_default().push((sock, p)),
            None => unattributed_map.entry(sock.uid).or_default().push(sock)
        }
    }
    let unattributed_count: usize = unattributed_map.values().map(Vec::len).sum();

    let mut clusters: Vec<NetCluster> = base.iter().filter_map(|c| {
        if !matches.get(c.identity.as_str()).copied().unwrap_or(true) {
            return None;
        }
        let mut conns = by_identity.remove(&c.identity)?;
        conns.sort_by_key(|(s, _)| (!s.is_listening(), s.local_port, s.peer_port));
        let mut listen_ports: Vec<u16> = conns.iter()
            .filter(|(s, _)| s.is_listening()).map(|(s, _)| s.local_port).collect();
        listen_ports.sort_unstable();
        listen_ports.dedup();
        let mut world_ports: Vec<u16> = conns.iter()
            .filter(|(s, _)| s.is_world_exposed()).map(|(s, _)| s.local_port).collect();
        world_ports.sort_unstable();
        world_ports.dedup();
        Some(NetCluster {
            identity: c.identity.clone(), cpu: c.cpu, rss: c.rss,
            proc_count: c.members.len(), conns, listen_ports, world_ports
        })
    }).collect();
    clusters.sort_by(|a, b| {
        let (atx, arx) = a.traffic(&traffic);
        let (btx, brx) = b.traffic(&traffic);
        (btx + brx).cmp(&(atx + arx))
            .then(b.conns.len().cmp(&a.conns.len()))
            .then(b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal))
    });

    let unattributed = if pattern.is_some() {
        Vec::new()
    } else {
        let mut v: Vec<(u32, Vec<Sock>)> = unattributed_map.into_iter().collect();
        v.sort_by_key(|b| std::cmp::Reverse(b.1.len()));
        v
    };

    NetReport { clusters, unattributed, total_conns, total_listening, unattributed_count, listen_port_set, total_world, traffic }
}

/// The three outcomes of kill-by-port selection: attributed listeners ready to signal, a
/// listener whose owner could not be identified, or no listener at all (with any non-listening
/// local-port matches carried along for the diagnostic listing).
#[derive(Debug)]
pub enum PortCandidates {
    Listeners(Vec<(Sock, i32)>),
    Unattributable { uid: u32 },
    NoListener(Vec<Sock>)
}

/// Kill-by-port candidate selection, implementing the spec's "kill what is listening" rule:
/// only listening sockets (TCP LISTEN, bound UDP) on the local port are candidates, so a client
/// whose ephemeral local port happens to equal N is never swept in. Attribution is never guessed:
/// if ANY listener on the port could not be attributed to a pid, the whole port is a hard error,
/// because attribution is per-pid all-or-nothing, so an unattributed listener almost always
/// belongs to a different process entirely, one that would silently survive the kill.
pub fn port_kill_candidates(attributed: &[(Sock, Option<i32>)], port: u16) -> PortCandidates {
    let listeners: Vec<&(Sock, Option<i32>)> = attributed.iter()
        .filter(|(s, _)| s.local_port == port && s.is_listening()).collect();
    if listeners.is_empty() {
        return PortCandidates::NoListener(
            attributed.iter().filter(|(s, _)| s.local_port == port).map(|(s, _)| s.clone()).collect()
        );
    }
    if let Some((s, _)) = listeners.iter().find(|(_, pid)| pid.is_none()) {
        return PortCandidates::Unattributable { uid: s.uid };
    }
    let owned: Vec<(Sock, i32)> = listeners.iter()
        .filter_map(|(s, pid)| pid.map(|p| (s.clone(), p))).collect();
    PortCandidates::Listeners(owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP4: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 34567 1 0000000000000000 100 0 0 10 0\n   1: 0501A8C0:AD22 5DF36B22:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 34568 1 0000000000000000 100 0 0 10 0\n   2: 0100007F:0016 00000000:0000 06 00000000:00000000 00:00000000 00000000     0        0 0 1 0000000000000000 100 0 0 10 0\n";

    const TCP6: &str = "  sl  local_address rem_address st ...\n   0: 00000000000000000000000001000000:0050 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 45678 1\n   1: 0000000000000000FFFF00000100007F:01BB 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 45679 1\n";

    const UDP4: &str = "  sl  local_address rem_address st ...\n  71: 00000000:0044 00000000:0000 07 00000000:00000000 00:00000000 00000000   998        0 23456 2\n";

    fn p(pid: i32, ppid: i32, comm: &str, cmd: &[&str], cpu: f64, rss: u64) -> crate::proc::Proc {
        crate::proc::Proc {
            pid, ppid, comm: comm.into(), cmdline: cmd.iter().map(|s| s.to_string()).collect(),
            rss, swap: 0, uid: 0, cpu_pct: cpu
        }
    }

    fn gradle_and_postgres() -> Vec<crate::proc::Proc> {
        vec![
            p(100, 1, "java", &["java", "org.gradle...GradleDaemon"], 10.0, 1000),
            p(101, 100, "java", &["java", "-cp", "/tmp/worker.jar"], 20.0, 2000),
            p(200, 1, "postgres", &["postgres", "-D", "/data"], 5.0, 500),
        ]
    }

    #[test]
    fn sockets_join_identity_clusters() {
        let mut estab = tsock(44000, SockState::Established, 1000, 12);
        estab.peer_port = 443;
        let attributed = vec![
            (tsock(8080, SockState::Listen, 1000, 10), Some(101)), // gradle child
            (estab, Some(100)),                                    // gradle daemon
            (tsock(5432, SockState::Listen, 26, 11), Some(200)),   // postgres
        ];
        let r = net_clusters(&gradle_and_postgres(), attributed, None, HashMap::new());
        assert_eq!(r.total_conns, 3);
        assert_eq!(r.total_listening, 2);
        assert_eq!(r.unattributed_count, 0);
        assert_eq!(r.clusters.len(), 2);
        // gradle first: 2 conns beats postgres's 1.
        assert_eq!(r.clusters[0].identity, "gradle");
        assert_eq!(r.clusters[0].conns.len(), 2);
        assert_eq!(r.clusters[0].listen_ports, vec![8080]);
        assert_eq!(r.clusters[0].proc_count, 2);
        assert!((r.clusters[0].cpu - 30.0).abs() < 0.01);
        // Listeners sort before established within a cluster.
        assert!(r.clusters[0].conns[0].0.is_listening());
        assert_eq!(r.clusters[1].identity, "postgres");
    }

    #[test]
    fn socketless_clusters_are_dropped_and_unattributed_group_by_uid() {
        let attributed = vec![
            (tsock(80, SockState::Listen, 33, 20), None),
            (tsock(81, SockState::Listen, 33, 21), None),
        ];
        let r = net_clusters(&gradle_and_postgres(), attributed, None, HashMap::new());
        assert!(r.clusters.is_empty()); // no process owned a socket
        assert_eq!(r.unattributed.len(), 1);
        assert_eq!(r.unattributed[0].0, 33);
        assert_eq!(r.unattributed[0].1.len(), 2);
        assert_eq!(r.unattributed_count, 2);
        assert_eq!(r.total_conns, 2);
    }

    #[test]
    fn pattern_filters_clusters_but_totals_stay_global() {
        let attributed = vec![
            (tsock(8080, SockState::Listen, 1000, 10), Some(100)),
            (tsock(5432, SockState::Listen, 26, 11), Some(200)),
            (tsock(99, SockState::Listen, 33, 12), None),
        ];
        let r = net_clusters(&gradle_and_postgres(), attributed, Some("postgres"), HashMap::new());
        assert_eq!(r.clusters.len(), 1);
        assert_eq!(r.clusters[0].identity, "postgres");
        // Unattributed rows cannot match a pattern: hidden from display, still counted.
        assert!(r.unattributed.is_empty());
        assert_eq!(r.unattributed_count, 1);
        assert_eq!(r.total_conns, 3); // header totals are pre-pattern, like pq's cpu total today
    }

    #[test]
    fn clusters_sort_by_traffic_before_conn_count() {
        let mut heavy = tsock(50000, SockState::Established, 1000, 77);
        heavy.peer_port = 443;
        let attributed = vec![
            // gradle: two sockets, zero bytes; postgres: one socket, 5MB moved.
            (tsock(8080, SockState::Listen, 1000, 10), Some(100)),
            (tsock(8081, SockState::Listen, 1000, 11), Some(100)),
            (heavy, Some(200)),
        ];
        let traffic: HashMap<u64, (u64, u64)> = [(77u64, (5_000_000u64, 1_000u64))].into_iter().collect();
        let r = net_clusters(&gradle_and_postgres(), attributed, None, traffic);
        assert_eq!(r.clusters[0].identity, "postgres"); // bytes beat connection count
        assert_eq!(r.clusters[0].traffic(&r.traffic), (5_000_000, 1_000));
    }

    #[test]
    fn direction_and_world_exposure() {
        let listen: HashSet<u16> = [80u16].into_iter().collect();
        let lsn = tsock(80, SockState::Listen, 0, 1);
        assert_eq!(lsn.direction(&listen), "listen");
        assert!(!lsn.is_world_exposed()); // tsock binds 127.0.0.1, loopback only

        let mut inbound = tsock(80, SockState::Established, 0, 2);
        inbound.peer_port = 55555;
        assert_eq!(inbound.direction(&listen), "in"); // accepted on a listened port

        let mut outbound = tsock(44000, SockState::Established, 0, 3);
        outbound.peer_port = 443;
        assert_eq!(outbound.direction(&listen), "out"); // ephemeral local port

        let mut world = tsock(80, SockState::Listen, 0, 4);
        world.local = "0.0.0.0".parse().unwrap();
        assert!(world.is_world_exposed());
    }

    #[test]
    fn net_clusters_computes_listen_set_and_world_totals() {
        let mut world = tsock(8080, SockState::Listen, 1000, 10);
        world.local = "0.0.0.0".parse().unwrap();
        let attributed = vec![
            (world, Some(100)),                                  // gradle, world-exposed
            (tsock(5432, SockState::Listen, 26, 11), Some(200)), // postgres, loopback only
        ];
        let r = net_clusters(&gradle_and_postgres(), attributed, None, HashMap::new());
        assert_eq!(r.total_world, 1);
        assert!(r.listen_port_set.contains(&8080) && r.listen_port_set.contains(&5432));
        let gradle = r.clusters.iter().find(|c| c.identity == "gradle").unwrap();
        assert_eq!(gradle.world_ports, vec![8080]);
        assert_eq!(gradle.listeners(), 1);
        assert_eq!(gradle.established(), 0);
        let pg = r.clusters.iter().find(|c| c.identity == "postgres").unwrap();
        assert!(pg.world_ports.is_empty());
    }

    #[test]
    fn port_kill_candidates_selects_only_attributed_listeners() {
        let mut estab = tsock(8080, SockState::Established, 1000, 2);
        estab.peer_port = 55555;
        // All listeners on the port are attributed: proceed with every owning pid.
        let all_attributed = vec![
            (tsock(8080, SockState::Listen, 1000, 1), Some(4242)),
            (tsock(8080, SockState::Listen, 1000, 9), Some(4242)),
            (estab.clone(), Some(777)),
        ];
        match port_kill_candidates(&all_attributed, 8080) {
            PortCandidates::Listeners(owners) => {
                assert_eq!(owners.len(), 2);
                assert!(owners.iter().all(|(_, pid)| *pid == 4242));
            }
            other => panic!("expected Listeners, got {:?}", other)
        }
        // A mixed attributed+unattributed port is a hard error: the unattributed listener likely
        // belongs to a different process that would silently survive the kill.
        let mixed = vec![
            (tsock(8080, SockState::Listen, 1000, 1), Some(4242)),
            (tsock(8080, SockState::Listen, 1000, 9), None),
            (estab.clone(), Some(777)),
        ];
        assert!(matches!(port_kill_candidates(&mixed, 8080), PortCandidates::Unattributable { uid: 1000 }));
        // Listener exists but nothing attributed: hard stop with the owning uid.
        let unowned = vec![(tsock(8080, SockState::Listen, 990, 1), None)];
        assert!(matches!(port_kill_candidates(&unowned, 8080), PortCandidates::Unattributable { uid: 990 }));
        // No listener at all: the established socket comes back for the diagnostic listing.
        let no_listener = vec![(estab, Some(777))];
        match port_kill_candidates(&no_listener, 8080) {
            PortCandidates::NoListener(others) => assert_eq!(others.len(), 1),
            other => panic!("expected NoListener, got {:?}", other)
        }
        assert!(matches!(port_kill_candidates(&[], 8080), PortCandidates::NoListener(v) if v.is_empty()));
    }

    #[test]
    fn parses_tcp4_listener_and_established() {
        let socks = parse_net_table(TCP4, Proto::Tcp);
        assert_eq!(socks.len(), 2); // the st=06 (TIME_WAIT) inode-0 row is dropped
        let listen = &socks[0];
        assert_eq!(listen.local, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(listen.local_port, 8080); // 0x1F90
        assert_eq!(listen.state, SockState::Listen);
        assert_eq!(listen.uid, 1000);
        assert_eq!(listen.inode, 34567);
        assert!(listen.is_listening());
        let estab = &socks[1];
        assert_eq!(estab.local, "192.168.1.5".parse::<IpAddr>().unwrap()); // 0501A8C0 LE
        assert_eq!(estab.local_port, 44322); // 0xAD22
        assert_eq!(estab.peer, "34.107.243.93".parse::<IpAddr>().unwrap()); // 5DF36B22 LE
        assert_eq!(estab.peer_port, 443);
        assert_eq!(estab.state, SockState::Established);
        assert!(!estab.is_listening());
    }

    #[test]
    fn parses_tcp6_and_collapses_v4_mapped() {
        let socks = parse_net_table(TCP6, Proto::Tcp);
        assert_eq!(socks.len(), 2);
        assert_eq!(socks[0].local, "::1".parse::<IpAddr>().unwrap());
        assert_eq!(socks[0].local_port, 80);
        // ::ffff:127.0.0.1 collapses to plain 127.0.0.1 so --port and display treat it as v4.
        assert_eq!(socks[1].local, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(socks[1].local_port, 443);
    }

    #[test]
    fn udp_bound_socket_is_listening() {
        let socks = parse_net_table(UDP4, Proto::Udp);
        assert_eq!(socks.len(), 1);
        assert_eq!(socks[0].local_port, 68); // 0x0044
        assert_eq!(socks[0].state, SockState::Other(7));
        assert!(socks[0].is_listening());
        assert_eq!(socks[0].uid, 998);
    }

    #[test]
    fn labels_render_for_listener_and_peer() {
        let socks = parse_net_table(TCP4, Proto::Tcp);
        assert_eq!(socks[0].label(), "tcp LISTEN  127.0.0.1:8080");
        assert_eq!(socks[1].label(), "tcp ESTAB  192.168.1.5:44322 -> 34.107.243.93:443");
        assert_eq!(socks[0].peer_str(), "");
    }

    #[test]
    fn garbage_lines_are_skipped() {
        assert!(parse_net_table("header\nnot a socket line\n", Proto::Tcp).is_empty());
        assert!(parse_net_table("", Proto::Tcp).is_empty());
    }

    /// Test builder: a TCP socket on 127.0.0.1 with the given local port, state, uid, inode.
    fn tsock(port: u16, state: SockState, uid: u32, inode: u64) -> Sock {
        Sock {
            proto: Proto::Tcp, state,
            local: "127.0.0.1".parse().unwrap(), local_port: port,
            peer: "0.0.0.0".parse().unwrap(), peer_port: 0,
            uid, inode
        }
    }

    #[test]
    fn attribute_joins_by_inode() {
        let socks = vec![tsock(80, SockState::Listen, 0, 111), tsock(81, SockState::Listen, 33, 999)];
        let inodes: HashMap<u64, i32> = [(111u64, 4242i32)].into_iter().collect();
        let attributed = attribute(socks, &inodes);
        assert_eq!(attributed[0].1, Some(4242));
        assert_eq!(attributed[1].1, None);
    }

    #[test]
    fn filter_socks_by_port_and_listen() {
        let mut estab = tsock(8080, SockState::Established, 0, 3);
        estab.peer_port = 55555;
        let socks = vec![
            (tsock(8080, SockState::Listen, 0, 1), Some(1)),
            (tsock(443, SockState::Listen, 0, 2), Some(2)),
            (estab, Some(3)),
        ];
        let by_port = filter_socks(socks.clone(), Some(8080), false);
        assert_eq!(by_port.len(), 2); // listener AND established both have local port 8080
        let listening = filter_socks(socks.clone(), Some(8080), true);
        assert_eq!(listening.len(), 1);
        assert_eq!(filter_socks(socks, None, false).len(), 3);
    }

    #[test]
    fn finds_own_listener_attributed_to_self() {
        // Live end-to-end: bind an ephemeral listener, then find it in /proc/net attributed to
        // this very process. Only touches our own pid, so it runs unprivileged.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id() as i32;
        let (socks, tables_read) = read_sockets();
        assert!(tables_read > 0, "no /proc/net table readable");
        let inodes = inode_map(&[me]);
        let attributed = attribute(socks, &inodes);
        assert!(
            attributed.iter().any(|(s, pid)| s.local_port == port && s.is_listening() && *pid == Some(me)),
            "own listener on port {port} not found or not attributed"
        );
    }
}
