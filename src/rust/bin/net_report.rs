//! Text + JSON reporting for pq's --net mode.
//!
//! Three human views, one JSON view:
//! * The overview grid (`write_text`/`write_graphics`): one row per identity cluster with its
//!   listening ports and outbound peer-port summary in aligned columns. No cpu/mem here, that
//!   is the default report's job; net mode answers "who serves what, who talks to what".
//! * The detail cards (`write_net_detail`): for targeted queries (`--port N`, or a PATTERN
//!   without -v), a plain-English answer plus one boxed card per owning cluster showing each
//!   process (user, command line) and its sockets with reachability arrows: WORLD (red,
//!   bound to all interfaces), local (loopback), iface (a specific address).
//! * `-v` expands the grid to raw per-connection rows.
//! * JSON always includes everything (machines make their own UIs).

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use qtools::style::{self, bold, dim, human_plain, paint};
use qtools::{chart, donut, graphics};

use crate::kill;
use crate::net;
use crate::proc;

use chart::{DONUT_PX, MIN_LABEL_WIDTH, RULE_WIDTH};

/// Rendering knobs, the net-mode analogue of report::ReportOpts (no metric: net mode always
/// sorts by connection count). `port` is the --port filter, echoed in the header and used by
/// the detail view's phrasing and hints.
pub struct NetOpts {
    pub top: usize,
    pub verbose: bool,
    pub colors: bool,
    pub width: Option<usize>,
    pub port: Option<u16>
}

const CONNS_WIDTH: usize = 5;
const TX_WIDTH: usize = 8;
const RX_WIDTH: usize = 8;
const PROC_WIDTH: usize = 26;
const LISTEN_WIDTH: usize = 26;
// Fixed part of a grid row before the talking column: conns, tx, rx, process, listening, each
// followed by a 2-space gutter.
const NET_ROW_PREFIX: usize = CONNS_WIDTH + 2 + TX_WIDTH + 2 + RX_WIDTH + 2 + PROC_WIDTH + 2 + LISTEN_WIDTH + 2;
/// SGR color codes (style::paint takes SGR codes, not 256-color indexes).
const WORLD_COLOR: i32 = 91; // bright red: world-exposed listeners deserve the attention
const LOCAL_COLOR: i32 = 32; // green: loopback-only, nothing to worry about
const IFACE_COLOR: i32 = 33; // yellow: bound to one specific address
const ESTAB_COLOR: i32 = 36; // cyan: established traffic lines in the detail cards

/// Header line: `pq | net [| port N] | N conns | N listening [| N world] | mem used / total`,
/// then a rule. The world segment (listeners bound to all interfaces) only appears when there
/// are any; the port segment echoes a --port filter.
fn write_header<W: Write>(out: &mut W, r: &net::NetReport, mem_used: u64, mem_total: u64, port: Option<u16>, colors: bool) -> io::Result<()> {
    let sep = dim("\u{2502}", colors);
    write!(out, "{}  {}  net", bold("pq", colors), sep)?;
    if let Some(p) = port {
        write!(out, "  {}  port {}", sep, p)?;
    }
    write!(out, "  {}  {} conns  {}  {} listening", sep, r.total_conns, sep, r.total_listening)?;
    if r.total_world > 0 {
        write!(out, "  {}  {}", sep, paint(&format!("{} world", r.total_world), WORLD_COLOR, false, colors))?;
    }
    writeln!(out, "  {}  mem {} / {}", sep, human_plain(mem_used), human_plain(mem_total))?;
    writeln!(out, "{}", dim(&"-".repeat(RULE_WIDTH), colors))
}

/// Human overview: header, then the grid, `-v` expanding per-connection rows, then per-uid rows
/// for unattributed sockets, the sudo hint when attribution was incomplete, and a usage footer.
pub fn write_text<W: Write>(out: &mut W, r: &net::NetReport, mem_used: u64, mem_total: u64, opts: &NetOpts) -> io::Result<()> {
    write_header(out, r, mem_used, mem_total, opts.port, opts.colors)?;
    write_body(out, r, opts, false)
}

/// The overview grid beneath the header/rule; shared with write_graphics, which passes
/// `swatches: true` after drawing the donut so every row carries the color swatch of its slice
/// (palette color for the top clusters in ring order, gray for anything folded into the "other"
/// arc: rank-13+ clusters and unattributed sockets).
fn write_body<W: Write>(out: &mut W, r: &net::NetReport, opts: &NetOpts, swatches: bool) -> io::Result<()> {
    let colors = opts.colors;
    // A swatch occupies 2 cells plus the 2-space gutter that follows it.
    let indent = if swatches { "    " } else { "" };
    let row_prefix = NET_ROW_PREFIX + indent.len();
    let heading = format!("{}{:>cw$}  {:>tw$}  {:>rw$}  {:<pw$}  {:<lw$}  talking to (peer port x conns)",
        indent, "conns", "tx", "rx", "process", "listening on",
        cw = CONNS_WIDTH, tw = TX_WIDTH, rw = RX_WIDTH, pw = PROC_WIDTH, lw = LISTEN_WIDTH);
    writeln!(out, "{}", dim(&heading, colors))?;

    let top: Vec<&net::NetCluster> = r.clusters.iter().take(opts.top).collect();

    for (rank, c) in top.iter().enumerate() {
        if swatches {
            // Same color the donut gave this cluster: palette by rank, gray once it fell into
            // the "other" arc (rank beyond the palette).
            let color = if rank < style::PALETTE.len() { style::PALETTE[rank] } else { style::OTHER_COLOR };
            write!(out, "{}  ", style::color_swatch(color, colors))?;
        }
        let mut name = c.identity.clone();
        if c.proc_count > 1 {
            name.push_str(&format!(" ({})", c.proc_count));
        }
        let (tx, rx) = c.traffic(&r.traffic);
        write_grid_row(
            out,
            c.conns.len(),
            (tx, rx),
            &name,
            &c.listen_ports,
            &c.world_ports,
            &outbound_summary(c.conns.iter().map(|(s, _)| s), &r.listen_port_set),
            row_prefix,
            opts
        )?;
        if opts.verbose {
            for (sock, pid) in &c.conns {
                writeln!(out, "{}{:>cw$}  {}", indent, "",
                    conn_line(sock, Some(*pid), &r.listen_port_set, &r.traffic, colors), cw = CONNS_WIDTH)?;
            }
        }
    }

    // The cutoff: clusters beyond -n exist in the totals (and in the donut's gray arc) but have
    // no row of their own; say so instead of letting them silently disappear.
    if r.clusters.len() > top.len() {
        let hidden = &r.clusters[top.len()..];
        let hidden_conns: usize = hidden.iter().map(|c| c.conns.len()).sum();
        // Hidden LISTENERS are called out by port: a serving process must never vanish silently.
        let (hidden_listen, _) = listening_ports(hidden.iter().flat_map(|c| c.conns.iter().map(|(s, _)| s)));
        if swatches {
            write!(out, "{}  ", style::color_swatch(style::OTHER_COLOR, colors))?;
        }
        let msg = if hidden_listen.is_empty() {
            format!("+ {} more cluster(s), {} conns (raise -n to see them)", hidden.len(), hidden_conns)
        } else {
            format!("+ {} more cluster(s), {} conns, still LISTENING on {} (raise -n, or --listen for every listener)",
                hidden.len(), hidden_conns, listen_cell(&hidden_listen, 40))
        };
        writeln!(out, "{}", dim(&msg, colors))?;
    }

    for (uid, socks) in &r.unattributed {
        if swatches {
            write!(out, "{}  ", style::color_swatch(style::OTHER_COLOR, colors))?;
        }
        let (ports, world) = listening_ports(socks.iter());
        let traffic = socks.iter().fold((0u64, 0u64), |(tx, rx), s| {
            let (t, rr) = r.traffic.get(&s.inode).copied().unwrap_or((0, 0));
            (tx + t, rx + rr)
        });
        // Truncate the username, never the marker: "(unattributed)" must survive whole.
        let user = style::truncate_middle(&kill::username(*uid), PROC_WIDTH.saturating_sub(15));
        write_grid_row(
            out,
            socks.len(),
            traffic,
            &format!("{} (unattributed)", user),
            &ports,
            &world,
            &outbound_summary(socks.iter(), &r.listen_port_set),
            row_prefix,
            opts
        )?;
        if opts.verbose {
            for sock in socks {
                writeln!(out, "{}{:>cw$}  {}", indent, "",
                    conn_line(sock, None, &r.listen_port_set, &r.traffic, colors), cw = CONNS_WIDTH)?;
            }
        }
    }

    if r.unattributed_count > 0 {
        writeln!(out, "{}", dim(
            &format!("{} connection(s) not attributed to a process; run with sudo for full attribution",
                r.unattributed_count),
            colors))?;
    }

    writeln!(out, "{}", dim(
        "try: pq --net NAME (deep dive)    pq --net --listen (every listener)    pq --port N [--kill] (inspect / free a port)",
        colors))
}

/// One grid row: conns count, tx/rx bytes (blank when zero, so listeners stay quiet), process
/// name, listening ports (world ones painted red), outbound summary. Cells are padded plain and
/// painted afterward so column math never sees escape bytes.
#[allow(clippy::too_many_arguments)]
fn write_grid_row<W: Write>(out: &mut W, conns: usize, (tx, rx): (u64, u64), name: &str, listen_ports: &[u16], world_ports: &[u16], talking: &str, row_prefix: usize, opts: &NetOpts) -> io::Result<()> {
    let colors = opts.colors;
    let name_cell = format!("{:<w$}", style::truncate_middle(name, PROC_WIDTH), w = PROC_WIDTH);
    let listen_plain = format!("{:<w$}", listen_cell(listen_ports, LISTEN_WIDTH), w = LISTEN_WIDTH);
    let listen_colored = if colors {
        world_ports.iter().fold(listen_plain, |l, p| paint_port(&l, *p, WORLD_COLOR))
    } else {
        listen_plain
    };
    let talking = match opts.width {
        Some(w) => style::truncate_middle(talking, w.saturating_sub(row_prefix).max(MIN_LABEL_WIDTH)),
        None => talking.to_string()
    };
    writeln!(out, "{:>cw$}  {:>tw$}  {:>rw$}  {}  {}  {}",
        conns, bytes_cell(tx), bytes_cell(rx), name_cell, listen_colored, talking,
        cw = CONNS_WIDTH, tw = TX_WIDTH, rw = RX_WIDTH)
}

/// Human bytes, or blank for zero: a listener that never spoke should not shout "0".
fn bytes_cell(v: u64) -> String {
    if v == 0 { String::new() } else { human_plain(v) }
}

/// The listening-ports cell: as many `:port` tokens as fit the column, then a `(+n)` overflow
/// marker so nothing silently disappears.
fn listen_cell(ports: &[u16], width: usize) -> String {
    let mut out = String::new();
    for (i, p) in ports.iter().enumerate() {
        let tok = format!("{}:{}", if i > 0 { " " } else { "" }, p);
        let remaining = ports.len() - i;
        // Reserve room for the overflow marker unless everything left still fits.
        let reserve = if remaining > 1 { 6 } else { 0 };
        if !out.is_empty() && out.len() + tok.len() + reserve > width {
            out.push_str(&format!(" (+{})", remaining));
            break;
        }
        out.push_str(&tok);
    }
    out
}

/// Listening ports (sorted, deduped) and the world-exposed subset for a plain socket list,
/// mirroring what NetCluster precomputes for clusters.
fn listening_ports<'a>(socks: impl Iterator<Item = &'a net::Sock>) -> (Vec<u16>, Vec<u16>) {
    let mut ports = Vec::new();
    let mut world = Vec::new();
    for s in socks {
        if s.is_listening() {
            ports.push(s.local_port);
            if s.is_world_exposed() {
                world.push(s.local_port);
            }
        }
    }
    ports.sort_unstable();
    ports.dedup();
    world.sort_unstable();
    world.dedup();
    (ports, world)
}

// -------------------------------------------------------------------------------------------------
// Detail cards: the answer to a targeted question (--port N, or a PATTERN without -v).
// -------------------------------------------------------------------------------------------------

/// Detail view: a plain-English answer line, then one boxed card per owning cluster (each
/// process with its user and command line, then its sockets with reachability arrows), then
/// unattributed sockets, the sudo hint, and an actionable kill hint.
pub fn write_net_detail<W: Write>(out: &mut W, r: &net::NetReport, procs: &[proc::Proc], mem_used: u64, mem_total: u64, opts: &NetOpts) -> io::Result<()> {
    let colors = opts.colors;
    write_header(out, r, mem_used, mem_total, opts.port, colors)?;

    if r.clusters.is_empty() && r.unattributed.is_empty() {
        match opts.port {
            Some(port) => writeln!(out, "nothing on this host has local port {}", port)?,
            None => writeln!(out, "nothing matches")?
        }
        return Ok(());
    }

    if let Some(port) = opts.port {
        writeln!(out, "{}", port_sentence(r, port, colors))?;
        writeln!(out)?;
    }

    let by_pid: HashMap<i32, &proc::Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let width = opts.width.unwrap_or(76).clamp(44, 100);

    for c in r.clusters.iter().take(opts.top) {
        write_cluster_card(out, c, &by_pid, &r.listen_port_set, &r.traffic, width, colors)?;
    }

    for (uid, socks) in &r.unattributed {
        writeln!(out, "{}", dim(&format!("user {} (unattributed)", kill::username(*uid)), colors))?;
        for sock in socks {
            writeln!(out, "    {}", conn_line(sock, None, &r.listen_port_set, &r.traffic, colors))?;
        }
    }

    if r.unattributed_count > 0 {
        writeln!(out, "{}", dim(
            &format!("{} connection(s) not attributed to a process; run with sudo for full attribution",
                r.unattributed_count),
            colors))?;
    }

    if r.total_listening > 0 {
        let hint = match opts.port {
            Some(port) => format!("free the port: pq --port {} --kill    every connection: pq --net -v", port),
            None => {
                // Name the actual match when there is exactly one, so the hint is paste-ready.
                let name = if r.clusters.len() == 1 { r.clusters[0].identity.as_str() } else { "NAME" };
                format!("kill it: pq --net --kill {}    every connection: pq --net {} -v", name, name)
            }
        };
        writeln!(out, "{}", dim(&hint, colors))?;
    }
    Ok(())
}

/// The plain-English answer for a port query: who holds it and how exposed it is.
fn port_sentence(r: &net::NetReport, port: u16, colors: bool) -> String {
    let mut holders: Vec<String> = r.clusters.iter().map(|c| c.identity.clone()).collect();
    for (uid, _) in &r.unattributed {
        holders.push(format!("an unidentified {} process", kill::username(*uid)));
    }
    let holders = holders.join(" and ");
    let est = r.total_conns - r.total_listening;
    let any_world = r.clusters.iter().any(|c| !c.world_ports.is_empty())
        || r.unattributed.iter().any(|(_, ss)| ss.iter().any(net::Sock::is_world_exposed));
    let exposure = if any_world {
        paint("open to the WORLD (bound to all interfaces)", WORLD_COLOR, false, colors)
    } else if r.total_listening > 0 {
        "bound to local/specific addresses only".to_string()
    } else {
        "no listeners, only established traffic".to_string()
    };
    format!(
        "port {} is held by {}: {} listening, {} established, {}",
        port, bold(&holders, colors), r.total_listening, est, exposure
    )
}

/// One boxed card: the cluster identity in the border, then per-pid blocks (bold meta line,
/// dimmed command line, socket rows with scope arrows and plain-language notes). Pid blocks are
/// capped so a 45-process browser does not produce a wall; the busiest pids (most sockets) win.
fn write_cluster_card<W: Write>(out: &mut W, c: &net::NetCluster, by_pid: &HashMap<i32, &proc::Proc>, listen_ports: &HashSet<u16>, traffic: &HashMap<u64, (u64, u64)>, width: usize, colors: bool) -> io::Result<()> {
    const MAX_PID_BLOCKS: usize = 4;
    let inner = width.saturating_sub(4); // "│ " and " │"

    // Group sockets per pid, busiest pid first.
    let mut per_pid: HashMap<i32, Vec<&net::Sock>> = HashMap::new();
    for (s, pid) in &c.conns {
        per_pid.entry(*pid).or_default().push(s);
    }
    let mut pids: Vec<(i32, Vec<&net::Sock>)> = per_pid.into_iter().collect();
    pids.sort_by_key(|(pid, socks)| (std::cmp::Reverse(socks.len()), *pid));

    // Content lines as (colored, visible-width) pairs; pad_visible closes the box later.
    let mut lines: Vec<(String, usize)> = Vec::new();
    for (i, (pid, socks)) in pids.iter().take(MAX_PID_BLOCKS).enumerate() {
        if i > 0 {
            lines.push((String::new(), 0));
        }
        let meta = match by_pid.get(pid) {
            Some(p) => format!("pid {}  user {}  {:.0}% cpu  {} mem",
                pid, kill::username(p.uid), p.cpu_pct, human_plain(p.rss)),
            None => format!("pid {}", pid)
        };
        lines.push((bold(&meta, colors), meta.chars().count()));
        if let Some(p) = by_pid.get(pid) {
            // A kernel thread or vanished process has no cmdline; fall back to comm.
            let cmd = if p.cmdline.is_empty() { format!("[{}]", p.comm) } else { p.cmdline.join(" ") };
            let cmd = style::truncate_middle(&cmd, inner);
            lines.push((dim(&cmd, colors), cmd.chars().count()));
        }
        socket_lines(socks, listen_ports, traffic, inner, colors, &mut lines);
    }
    if pids.len() > MAX_PID_BLOCKS {
        let hidden: usize = pids[MAX_PID_BLOCKS..].iter().map(|(_, s)| s.len()).sum();
        lines.push((String::new(), 0));
        let note = format!("+ {} more procs holding {} socket(s); -v lists every connection",
            pids.len() - MAX_PID_BLOCKS, hidden);
        lines.push((dim(&note, colors), note.chars().count()));
    }

    // The box itself.
    let title = format!(" {} ", c.identity);
    let fill = width.saturating_sub(3 + title.chars().count());
    writeln!(out, "{}{}{}",
        dim("\u{250c}\u{2500}", colors),
        bold(&title, colors),
        dim(&format!("{}\u{2510}", "\u{2500}".repeat(fill)), colors))?;
    for (colored, visible) in lines {
        writeln!(out, "{} {} {}",
            dim("\u{2502}", colors),
            style::pad_visible(&colored, visible, inner),
            dim("\u{2502}", colors))?;
    }
    writeln!(out, "{}", dim(&format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(width.saturating_sub(2))), colors))
}

/// Socket rows for one pid block: aligned scope/direction arrows into the endpoint, with a
/// plain-language reachability note (plus traffic counters for established connections).
/// Listeners first (the input is already sorted that way).
fn socket_lines(socks: &[&net::Sock], listen_ports: &HashSet<u16>, traffic: &HashMap<u64, (u64, u64)>, inner: usize, colors: bool, lines: &mut Vec<(String, usize)>) {
    let mut rows: Vec<(String, i32, String)> = Vec::new();
    for s in socks {
        let proto = match s.proto { net::Proto::Tcp => "tcp", net::Proto::Udp => "udp" };
        if s.is_listening() {
            let (tag, color, arrow, note) = listener_scope(s);
            rows.push((format!("{:<5} {} {} {}", tag, arrow, proto, s.local_str()), color, note.to_string()));
        } else {
            let dirn = s.direction(listen_ports);
            let note = match traffic.get(&s.inode) {
                Some(&(tx, rx)) if tx + rx > 0 =>
                    format!("{} tx / {} rx", human_plain(tx), human_plain(rx)),
                _ => "established".to_string()
            };
            // The peer is the story; an outbound socket's ephemeral local address is noise.
            // Inbound keeps the local port so you can see which listener accepted it.
            let lead = if dirn == "in" {
                format!("{:<5} \u{25c0}\u{2500}\u{2500} {} {} -> :{}", dirn, proto, s.peer_str(), s.local_port)
            } else {
                format!("{:<5} \u{2500}\u{2500}\u{25b6} {} {}", dirn, proto, s.peer_str())
            };
            rows.push((lead, ESTAB_COLOR, note));
        }
    }
    let lead_w = rows.iter().map(|(l, _, _)| l.chars().count()).max().unwrap_or(0);
    for (lead, color, note) in rows {
        let with_note = lead_w + 3 + note.len() <= inner;
        let visible = if with_note { lead_w + 3 + note.len() } else { lead.chars().count().min(inner) };
        let padded = format!("{:<w$}", lead, w = if with_note { lead_w } else { 0 });
        let colored = if with_note {
            format!("{}   {}", paint(&padded, color, false, colors), dim(&note, colors))
        } else {
            paint(&padded, color, false, colors)
        };
        lines.push((colored, visible));
    }
}

/// Reachability scope of a listener's bind address: (tag, SGR color, arrow, note).
fn listener_scope(s: &net::Sock) -> (&'static str, i32, &'static str, &'static str) {
    if s.local.is_unspecified() {
        ("WORLD", WORLD_COLOR, "\u{2550}\u{2550}\u{25b6}", "reachable from anywhere")
    } else if s.local.is_loopback() {
        ("local", LOCAL_COLOR, "\u{2500}\u{2500}\u{25b6}", "this machine only")
    } else {
        ("iface", IFACE_COLOR, "\u{2500}\u{2500}\u{25b6}", "only via this specific address")
    }
}

/// One per-connection line for -v and unattributed listings: direction tag (listen/in/out),
/// owning pid (when known), the socket label, and a [world] marker on listeners bound to all
/// interfaces. The marker is textual so piped output keeps it; it turns red when colors are on.
fn conn_line(sock: &net::Sock, pid: Option<i32>, listen_ports: &HashSet<u16>, traffic: &HashMap<u64, (u64, u64)>, colors: bool) -> String {
    let pid_part = pid.map(|p| format!("pid {}  ", p)).unwrap_or_default();
    let mut text = format!("{:<6}  {}{}", sock.direction(listen_ports), pid_part, sock.label());
    if let Some(&(tx, rx)) = traffic.get(&sock.inode) {
        if tx + rx > 0 {
            text.push_str(&format!("  tx {} rx {}", human_plain(tx), human_plain(rx)));
        }
    }
    let mut line = dim(&text, colors);
    if sock.is_world_exposed() {
        line.push_str("  ");
        line.push_str(&paint("[world]", WORLD_COLOR, false, colors));
    }
    line
}

/// Compact outbound summary: the top peer ports of connections dialed out, e.g.
/// ":443 x28 :53 x2 (+3)". Inbound established connections are excluded: they are already
/// represented by the listening port. Empty when nothing dialed.
fn outbound_summary<'a>(socks: impl Iterator<Item = &'a net::Sock>, listen_ports: &HashSet<u16>) -> String {
    let mut counts: HashMap<u16, usize> = HashMap::new();
    for s in socks {
        if s.direction(listen_ports) == "out" {
            *counts.entry(s.peer_port).or_insert(0) += 1;
        }
    }
    if counts.is_empty() {
        return String::new();
    }
    let mut peers: Vec<(u16, usize)> = counts.into_iter().collect();
    peers.sort_by_key(|&(port, n)| (std::cmp::Reverse(n), port));
    let shown = peers.iter().take(3)
        .map(|(port, n)| if *n > 1 { format!(":{} x{}", port, n) } else { format!(":{}", port) })
        .collect::<Vec<_>>().join(" ");
    let extra = peers.len().saturating_sub(3);
    if extra > 0 {
        format!("{} (+{})", shown, extra)
    } else {
        shown
    }
}

/// Paint occurrences of `:port` in `label` that end at a digit boundary, so painting ":53"
/// never recolors the front of ":5432".
fn paint_port(label: &str, port: u16, color: i32) -> String {
    let token = format!(":{}", port);
    let mut out = String::with_capacity(label.len() + 16);
    let mut rest = label;
    while let Some(idx) = rest.find(&token) {
        let end = idx + token.len();
        let boundary = rest[end..].chars().next().is_none_or(|c| !c.is_ascii_digit());
        out.push_str(&rest[..idx]);
        if boundary {
            out.push_str(&paint(&token, color, false, true));
        } else {
            out.push_str(&token);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Machine-readable JSON. Connections are always included (JSON is for machines; no -v gating),
/// each as {pid, proto, state, listening, direction, world, local, peer} with peer empty for
/// listeners.
pub fn write_json<W: Write>(out: &mut W, r: &net::NetReport, mem_used: u64, mem_total: u64, opts: &NetOpts) -> io::Result<()> {
    writeln!(out, "{{")?;
    writeln!(out, "  \"mode\": \"net\",")?;
    writeln!(out, "  \"total_conns\": {},", r.total_conns)?;
    writeln!(out, "  \"total_listening\": {},", r.total_listening)?;
    writeln!(out, "  \"total_world\": {},", r.total_world)?;
    writeln!(out, "  \"unattributed_conns\": {},", r.unattributed_count)?;
    writeln!(out, "  \"mem_used\": {},", mem_used)?;
    writeln!(out, "  \"mem_total\": {},", mem_total)?;
    writeln!(out, "  \"clusters\": [")?;
    let top: Vec<&net::NetCluster> = r.clusters.iter().take(opts.top).collect();
    for (i, c) in top.iter().enumerate() {
        writeln!(out, "    {{")?;
        writeln!(out, "      \"identity\": \"{}\",", style::json_escape(&c.identity))?;
        writeln!(out, "      \"conns\": {},", c.conns.len())?;
        writeln!(out, "      \"listeners\": {},", c.listeners())?;
        writeln!(out, "      \"established\": {},", c.established())?;
        let ports = c.listen_ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ");
        writeln!(out, "      \"listening\": [{}],", ports)?;
        let world = c.world_ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ");
        writeln!(out, "      \"world_ports\": [{}],", world)?;
        let (tx, rx) = c.traffic(&r.traffic);
        writeln!(out, "      \"bytes_tx\": {},", tx)?;
        writeln!(out, "      \"bytes_rx\": {},", rx)?;
        writeln!(out, "      \"cpu\": {},", round2(c.cpu))?;
        writeln!(out, "      \"mem\": {},", c.rss)?;
        writeln!(out, "      \"proc_count\": {},", c.proc_count)?;
        writeln!(out, "      \"connections\": [")?;
        for (j, (s, pid)) in c.conns.iter().enumerate() {
            write!(out, "        {}", conn_json(s, Some(*pid), &r.listen_port_set, &r.traffic))?;
            writeln!(out, "{}", if j + 1 < c.conns.len() { "," } else { "" })?;
        }
        writeln!(out, "      ]")?;
        write!(out, "    }}")?;
        writeln!(out, "{}", if i + 1 < top.len() { "," } else { "" })?;
    }
    writeln!(out, "  ],")?;
    writeln!(out, "  \"unattributed\": [")?;
    for (i, (uid, socks)) in r.unattributed.iter().enumerate() {
        writeln!(out, "    {{")?;
        writeln!(out, "      \"uid\": {},", uid)?;
        writeln!(out, "      \"user\": \"{}\",", style::json_escape(&kill::username(*uid)))?;
        writeln!(out, "      \"conns\": {},", socks.len())?;
        writeln!(out, "      \"connections\": [")?;
        for (j, s) in socks.iter().enumerate() {
            write!(out, "        {}", conn_json(s, None, &r.listen_port_set, &r.traffic))?;
            writeln!(out, "{}", if j + 1 < socks.len() { "," } else { "" })?;
        }
        writeln!(out, "      ]")?;
        write!(out, "    }}")?;
        writeln!(out, "{}", if i + 1 < r.unattributed.len() { "," } else { "" })?;
    }
    writeln!(out, "  ]")?;
    writeln!(out, "}}")
}

fn conn_json(s: &net::Sock, pid: Option<i32>, listen_ports: &HashSet<u16>, traffic: &HashMap<u64, (u64, u64)>) -> String {
    let proto = match s.proto { net::Proto::Tcp => "tcp", net::Proto::Udp => "udp" };
    let (tx, rx) = traffic.get(&s.inode).copied().unwrap_or((0, 0));
    format!(
        "{{\"pid\": {}, \"proto\": \"{}\", \"state\": \"{}\", \"listening\": {}, \"direction\": \"{}\", \"world\": {}, \"tx\": {}, \"rx\": {}, \"local\": \"{}\", \"peer\": \"{}\"}}",
        pid.map(|p| p.to_string()).unwrap_or_else(|| "null".into()),
        proto, s.state_label(), s.is_listening(), s.direction(listen_ports),
        s.is_world_exposed(), tx, rx, s.local_str(), s.peer_str()
    )
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Donut segments for one ring: top clusters valued by `count`, then one gray "other" arc for
/// the remainder up to `total` (smaller clusters plus unattributed sockets), so the ring always
/// sums to `total`. No "unused" arc: unlike cpu/mem there is no capacity concept.
fn conn_segments(r: &net::NetReport, top: usize, count: impl Fn(&net::NetCluster) -> u64, total: u64) -> Vec<chart::Segment> {
    let top_clusters: Vec<&net::NetCluster> = r.clusters.iter().take(top.min(style::PALETTE.len())).collect();
    let mut segs: Vec<chart::Segment> = top_clusters.iter().enumerate()
        .map(|(i, c)| chart::Segment { label: c.identity.clone(), value: count(c), color: style::PALETTE[i] })
        .collect();
    let sum: u64 = segs.iter().map(|s| s.value).sum();
    segs.push(chart::Segment {
        label: "other".to_string(),
        value: total.saturating_sub(sum),
        color: style::OTHER_COLOR
    });
    segs
}

/// Draw the connections donut, then the grid beneath it as the legend (each row carries its
/// slice's color swatch). When the socket set has both listeners and established connections, a
/// two-ring donut separates the stories: outer ring = established (who talks), inner ring =
/// listeners (who serves), same cluster color on both, labeled beneath the image. Otherwise a
/// single ring of total connections says it all. Falls back to the plain grid (never re-writing
/// the header) when there is nothing to chart or the image fails to render.
pub fn write_graphics<W: Write>(out: &mut W, r: &net::NetReport, mem_used: u64, mem_total: u64, opts: &NetOpts) -> io::Result<()> {
    write_header(out, r, mem_used, mem_total, opts.port, opts.colors)?;
    if r.total_conns == 0 {
        return write_body(out, r, opts, false);
    }
    let cols = opts.width.unwrap_or(80).clamp(16, 40) as u32;
    let config = viuer::Config {
        absolute_offset: false,
        width: Some(cols),
        height: Some((cols / 2).max(8)),
        ..Default::default()
    };
    let total_lsn = r.total_listening as u64;
    let total_est = (r.total_conns - r.total_listening) as u64;
    let two_ring = total_est > 0 && total_lsn > 0;
    // Flush before viuer::print: it writes straight to the terminal, bypassing out's buffer.
    out.flush()?;
    let image = if two_ring {
        let outer = conn_segments(r, opts.top, |c| c.established() as u64, total_est);
        let inner = conn_segments(r, opts.top, |c| c.listeners() as u64, total_lsn);
        donut::build_two_ring_image(&chart::slices(&outer), &chart::slices(&inner), DONUT_PX)
    } else {
        let segs = conn_segments(r, opts.top, |c| c.conns.len() as u64, r.total_conns as u64);
        donut::build_donut_image(&chart::slices(&segs), DONUT_PX)
    };
    let drew = match viuer::print(&image, &config) {
        Ok(_) => true,
        Err(e) => {
            graphics::debug_print_failure(&e);
            false
        }
    };
    if drew && two_ring {
        writeln!(out, "{}", dim("outer ring: established (talking)    inner ring: listening (serving)", opts.colors))?;
    }
    // Swatches only make sense beneath a donut that actually rendered.
    write_body(out, r, opts, drew)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{NetCluster, NetReport, Proto, Sock, SockState};

    fn sock(port: u16, state: SockState) -> Sock {
        Sock {
            proto: Proto::Tcp, state,
            local: "127.0.0.1".parse().unwrap(), local_port: port,
            peer: "0.0.0.0".parse().unwrap(), peer_port: 0,
            uid: 1000, inode: 1
        }
    }

    /// A bound-but-unconnected UDP socket on the wildcard address: state Other(7) with no peer,
    /// which is_listening() treats as listening, state_label() renders as "BOUND", and
    /// is_world_exposed() flags (0.0.0.0 bind).
    fn udp_bound(port: u16) -> Sock {
        Sock {
            proto: Proto::Udp, state: SockState::Other(7),
            local: "0.0.0.0".parse().unwrap(), local_port: port,
            peer: "0.0.0.0".parse().unwrap(), peer_port: 0,
            uid: 1000, inode: 2
        }
    }

    /// An outbound established connection (ephemeral local port, real peer).
    fn estab(local_port: u16, peer_port: u16) -> Sock {
        Sock {
            proto: Proto::Tcp, state: SockState::Established,
            local: "192.168.1.5".parse().unwrap(), local_port,
            peer: "34.107.243.93".parse().unwrap(), peer_port,
            uid: 1000, inode: 3
        }
    }

    fn report() -> NetReport {
        NetReport {
            clusters: vec![NetCluster {
                identity: "nginx".into(), cpu: 2.0, rss: 50_000_000, proc_count: 4,
                conns: vec![
                    (sock(80, SockState::Listen), 500),
                    (sock(443, SockState::Listen), 500),
                    (udp_bound(53), 500),
                    (estab(44000, 443), 500)
                ],
                listen_ports: vec![53, 80, 443],
                world_ports: vec![53]
            }],
            unattributed: vec![(989, vec![sock(5432, SockState::Listen)])],
            total_conns: 5,
            total_listening: 4,
            unattributed_count: 1,
            listen_port_set: [53u16, 80, 443, 5432].into_iter().collect(),
            total_world: 1,
            // The established conn (inode 3) moved 1.5M out, 300K in.
            traffic: [(3u64, (1_500_000u64, 300_000u64))].into_iter().collect()
        }
    }

    fn opts() -> NetOpts {
        NetOpts { top: 15, verbose: false, colors: false, width: None, port: None }
    }

    #[test]
    fn grid_shows_totals_ports_and_hints() {
        let mut buf = Vec::new();
        write_text(&mut buf, &report(), 14_000_000, 31_000_000, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("5 conns"));
        assert!(s.contains("4 listening"));
        assert!(s.contains("1 world")); // header world count (plain text when colors off)
        assert!(s.contains("listening on")); // grid column headings
        assert!(s.contains("talking to"));
        assert!(s.contains(&human_plain(1_500_000))); // tx bytes in the grid
        assert!(s.contains(&human_plain(300_000))); // rx bytes in the grid
        assert!(s.contains("nginx (4)"));
        assert!(s.contains(":53 :80 :443")); // listening cell
        assert!(s.contains(":443")); // talking cell (outbound peer port)
        assert!(!s.contains("cpu")); // no cpu/mem in net mode
        assert!(s.contains("(unattributed)"));
        assert!(s.contains(":5432")); // unattributed rows show their ports too
        assert!(s.contains("run with sudo"));
        assert!(s.contains("pq --net --listen (every listener)")); // usage footer
        assert!(!s.contains('\u{1b}')); // colors:false -> no ANSI
        assert!(!s.contains("127.0.0.1:80")); // per-connection rows only with -v
    }

    #[test]
    fn world_count_absent_when_zero() {
        let mut r = report();
        r.total_world = 0;
        let mut buf = Vec::new();
        write_text(&mut buf, &r, 0, 0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains("world"));
    }

    #[test]
    fn verbose_grid_lists_connections_with_direction() {
        let mut buf = Vec::new();
        let o = NetOpts { verbose: true, ..opts() };
        write_text(&mut buf, &report(), 0, 0, &o).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("listen  pid 500  tcp LISTEN  127.0.0.1:80"));
        assert!(s.contains("out     pid 500  tcp ESTAB  192.168.1.5:44000 -> 34.107.243.93:443"));
        assert!(s.contains("[world]")); // the wildcard-bound UDP listener
    }

    #[test]
    fn no_hint_when_everything_attributed() {
        let mut r = report();
        r.unattributed.clear();
        r.unattributed_count = 0;
        let mut buf = Vec::new();
        write_text(&mut buf, &r, 0, 0, &opts()).unwrap();
        assert!(!String::from_utf8(buf).unwrap().contains("run with sudo"));
    }

    #[test]
    fn listen_cell_overflows_gracefully() {
        let ports: Vec<u16> = (1..=10).map(|i| i * 1000).collect();
        let cell = listen_cell(&ports, 26);
        assert!(cell.len() <= 26, "cell must fit its column: {:?} ({})", cell, cell.len());
        assert!(cell.contains("(+"), "overflow must be marked: {cell}");
        assert!(listen_cell(&[80, 443], 26).eq(":80 :443")); // short lists render whole
        assert!(listen_cell(&[], 26).is_empty());
    }

    #[test]
    fn detail_answers_port_query_with_cards() {
        let procs = vec![proc::Proc {
            pid: 500, ppid: 1, comm: "nginx".into(),
            cmdline: vec!["/usr/sbin/nginx".into(), "-g".into(), "daemon off;".into()],
            rss: 50_000_000, swap: 0, uid: 0, cpu_pct: 2.0
        }];
        let o = NetOpts { port: Some(80), ..opts() };
        let mut buf = Vec::new();
        write_net_detail(&mut buf, &report(), &procs, 0, 0, &o).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("port 80 is held by nginx")); // the plain-English answer
        assert!(s.contains("open to the WORLD"));
        assert!(s.contains('\u{250c}') && s.contains('\u{2514}')); // a real box
        assert!(s.contains("pid 500  user root  2% cpu"));
        assert!(s.contains("/usr/sbin/nginx -g daemon off;")); // full command line
        assert!(s.contains("WORLD \u{2550}\u{2550}\u{25b6} udp 0.0.0.0:53"));
        assert!(s.contains("local \u{2500}\u{2500}\u{25b6} tcp 127.0.0.1:80"));
        assert!(s.contains("this machine only")); // plain-language reachability note
        assert!(s.contains("out   \u{2500}\u{2500}\u{25b6} tcp 34.107.243.93:443")); // peer, not noise
        assert!(s.contains(&format!("{} tx / {} rx",
            human_plain(1_500_000), human_plain(300_000)))); // traffic on the established row
        assert!(s.contains("free the port: pq --port 80 --kill")); // actionable hint
        assert!(s.contains("(unattributed)"));
        assert!(s.contains("run with sudo"));
    }

    #[test]
    fn detail_caps_pid_blocks() {
        let mut r = report();
        // 6 pids, one socket each: only the first 4 get blocks, the rest are summarized.
        r.clusters[0].conns = (0..6).map(|i| (sock(8000 + i as u16, SockState::Listen), 100 + i)).collect();
        let mut buf = Vec::new();
        write_net_detail(&mut buf, &r, &[], 0, 0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("+ 2 more procs holding 2 socket(s)"));
    }

    #[test]
    fn detail_reports_empty_port() {
        let r = NetReport {
            clusters: vec![], unattributed: vec![], total_conns: 0, total_listening: 0,
            unattributed_count: 0, listen_port_set: HashSet::new(), total_world: 0,
            traffic: HashMap::new()
        };
        let o = NetOpts { port: Some(9999), ..opts() };
        let mut buf = Vec::new();
        write_net_detail(&mut buf, &r, &[], 0, 0, &o).unwrap();
        assert!(String::from_utf8(buf).unwrap().contains("nothing on this host has local port 9999"));
    }

    #[test]
    fn json_report_is_wellformed_and_always_includes_connections() {
        let mut buf = Vec::new();
        write_json(&mut buf, &report(), 14_000_000, 31_000_000, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"mode\": \"net\""));
        assert!(s.contains("\"total_conns\": 5"));
        assert!(s.contains("\"total_world\": 1"));
        assert!(s.contains("\"identity\": \"nginx\""));
        assert!(s.contains("\"listeners\": 3"));
        assert!(s.contains("\"established\": 1"));
        assert!(s.contains("\"listening\": [53, 80, 443]")); // the cluster's listen_ports
        assert!(s.contains("\"world_ports\": [53]"));
        assert!(s.contains("\"bytes_tx\": 1500000")); // cluster traffic sums
        assert!(s.contains("\"bytes_rx\": 300000"));
        assert!(s.contains("\"tx\": 1500000")); // per-connection counters
        assert!(s.contains("\"local\": \"127.0.0.1:80\"")); // no -v gating in JSON
        assert!(s.contains("\"listening\": true")); // per-connection listening field
        assert!(s.contains("\"state\": \"BOUND\"")); // UDP bound socket, consistent with text
        assert!(s.contains("\"direction\": \"listen\""));
        assert!(s.contains("\"direction\": \"out\"")); // the established conn
        assert!(s.contains("\"world\": true")); // the wildcard-bound listener
        assert!(s.contains("\"unattributed\""));
        assert!(s.contains("\"uid\": 989"));
    }

    #[test]
    fn cutoff_row_names_hidden_clusters() {
        let mut r = report();
        r.clusters.push(NetCluster {
            identity: "extra".into(), cpu: 0.0, rss: 0, proc_count: 1,
            conns: vec![(sock(9000, SockState::Listen), 1)],
            listen_ports: vec![9000], world_ports: vec![]
        });
        let o = NetOpts { top: 1, ..opts() };
        let mut buf = Vec::new();
        write_text(&mut buf, &r, 0, 0, &o).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // The hidden cluster is LISTENING, so the cutoff row must name its port.
        assert!(s.contains("+ 1 more cluster(s), 1 conns, still LISTENING on :9000"));
        assert!(!s.contains("extra")); // the cluster itself stays hidden

        // Hidden clusters with no listeners get the plain cutoff wording.
        let mut r2 = report();
        r2.clusters.push(NetCluster {
            identity: "chatty".into(), cpu: 0.0, rss: 0, proc_count: 1,
            conns: vec![(estab(50000, 443), 1)],
            listen_ports: vec![], world_ports: vec![]
        });
        let mut buf2 = Vec::new();
        write_text(&mut buf2, &r2, 0, 0, &o).unwrap();
        let s2 = String::from_utf8(buf2).unwrap();
        assert!(s2.contains("(raise -n to see them)"));
        assert!(!s2.contains("still LISTENING"));
    }

    #[test]
    fn swatches_mark_rows_in_graphics_body() {
        let mut buf = Vec::new();
        write_body(&mut buf, &report(), &opts(), true).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // colors:false renders swatches as plain blocks: one for nginx, one for unattributed.
        assert_eq!(s.matches("\u{2588}\u{2588}").count(), 2);
        // Text mode has none.
        let mut buf2 = Vec::new();
        write_body(&mut buf2, &report(), &opts(), false).unwrap();
        assert!(!String::from_utf8(buf2).unwrap().contains('\u{2588}'));
    }

    #[test]
    fn outbound_summary_groups_and_caps_peer_ports() {
        let listen: HashSet<u16> = [80u16].into_iter().collect();
        let mut c = report().clusters.remove(0);
        // Add more outbound conns: two more to 443, one each to 4 distinct ports.
        for (i, peer) in [443u16, 443, 993, 465, 587, 22].iter().enumerate() {
            c.conns.push((estab(50000 + i as u16, *peer), 500));
        }
        let s = outbound_summary(c.conns.iter().map(|(s, _)| s), &listen);
        assert!(s.starts_with(":443 x3"), "443 should lead with its count: {s}");
        assert!(s.contains("(+2)"), "overflow beyond top 3 is counted: {s}");
        // Listeners alone produce no summary.
        assert!(outbound_summary([sock(80, SockState::Listen)].iter(), &listen).is_empty());
    }

    #[test]
    fn paint_port_respects_digit_boundaries() {
        // ":53" must not recolor the front of ":5432".
        let painted = paint_port("x  :53 :5432", 53, WORLD_COLOR);
        assert!(painted.contains(":5432"));
        assert_eq!(painted.matches('\u{1b}').count(), 2); // one color-on, one reset
    }

    #[test]
    fn conn_segments_sum_to_total_conns() {
        let r = report(); // nginx has 4 conns, 1 unattributed, total 5
        let segs = conn_segments(&r, 15, |c| c.conns.len() as u64, r.total_conns as u64);
        assert_eq!(segs.len(), 2); // nginx + other
        assert_eq!(segs[0].label, "nginx");
        assert_eq!(segs[0].value, 4);
        assert_eq!(segs[1].label, "other");
        assert_eq!(segs[1].value, 1); // the unattributed socket
        let sum: u64 = segs.iter().map(|s| s.value).sum();
        assert_eq!(sum, r.total_conns as u64);
    }

    #[test]
    fn two_ring_segments_split_established_and_listeners() {
        let r = report();
        let est = conn_segments(&r, 15, |c| c.established() as u64, (r.total_conns - r.total_listening) as u64);
        assert_eq!(est[0].value, 1); // nginx's outbound conn
        assert_eq!(est[1].value, 0); // nothing else established
        let lsn = conn_segments(&r, 15, |c| c.listeners() as u64, r.total_listening as u64);
        assert_eq!(lsn[0].value, 3); // nginx's three listeners
        assert_eq!(lsn[1].value, 1); // the unattributed postgres listener
    }
}
