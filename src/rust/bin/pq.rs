//! pq: query and cluster the processes eating your CPU/memory. Companion to dq, sharing the same
//! rendering primitives (style/donut/term/graphics/chart) from the qtools library.
mod proc; mod identity; mod cluster; mod report; mod kill; mod net; mod net_report; mod netdiag;

use std::collections::HashMap;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::time::Duration;

use clap::Parser;
use qtools::{graphics, style, term};

use cluster::Metric;

const EXAMPLES: &str = "\
Examples:
  pq                        what is eating cpu/memory right now
  pq -m -v chrome           memory view, clusters expanded, only chrome
  pq --net                  who is serving / talking on the network
  pq --net -v --listen      every listening socket, one row each
  pq --port 8080            who owns port 8080
  pq --port 8080 --kill     kill whatever is LISTENING on port 8080
  pq --kill gradle          kill the gradle tree (preview + confirm)";

#[derive(Parser, Debug)]
#[command(name = "pq", about = "pq: query and cluster the processes eating your CPU/memory or talking on the network.", after_help = EXAMPLES)]
struct Args {
    /// Clusters to show (default 15; --listen defaults to all, so no listener is ever cut off)
    #[arg(short = 'n', long)] top: Option<usize>,
    /// Expand clusters to their member processes (per-connection rows in --net mode)
    #[arg(short, long)] verbose: bool,
    /// CPU sample interval in milliseconds
    #[arg(long, default_value_t = 400)] interval: u64,
    /// Emit machine-readable JSON
    #[arg(long)] json: bool,
    /// Sort and chart by memory instead of CPU
    #[arg(short = 'm', long, help_heading = "Metric (default report)")] memory: bool,
    /// Sort and chart by swap usage instead of CPU
    #[arg(short = 's', long, help_heading = "Metric (default report)")] swap: bool,
    /// Sort and chart by CPU (default)
    #[arg(short = 'c', long, help_heading = "Metric (default report)")] cpu: bool,
    /// Report TCP/UDP connections instead of cpu/mem clusters (sorted by traffic, then
    /// connection count; metric flags do not change the sort). World-exposed listeners are
    /// highlighted
    #[arg(long, help_heading = "Net mode")] net: bool,
    /// Only sockets with this local port (implies --net)
    #[arg(long, help_heading = "Net mode")] port: Option<u16>,
    /// Only listening sockets (implies --net)
    #[arg(long, help_heading = "Net mode")] listen: bool,
    /// Kill matching process trees (preview + confirm); pattern optional with --port/--listen
    #[arg(long, num_args = 0..=1, default_missing_value = "", help_heading = "Kill")] kill: Option<String>,
    /// Preview only, do not signal
    #[arg(long, help_heading = "Kill")] dry_run: bool,
    /// Skip the confirmation prompt
    #[arg(short = 'y', long, help_heading = "Kill")] yes: bool,
    /// Match the identity exactly instead of substring
    #[arg(short = 'x', long, help_heading = "Kill")] exact: bool,
    /// Initial signal
    #[arg(long, default_value = "TERM", help_heading = "Kill")] signal: String,
    /// Seconds to wait before SIGKILL
    #[arg(long, default_value_t = 4, help_heading = "Kill")] grace: u64,
    /// SIGKILL immediately (no TERM, no grace)
    #[arg(short = '9', long = "sigkill", help_heading = "Kill")] sigkill: bool,
    /// Filter the report to matching clusters
    pattern: Option<String>,
}

fn main() {
    let args = Args::parse();

    if args.kill.is_some() {
        let pattern = kill_pattern(&args);
        if pattern.is_none() && args.port.is_none() && !args.listen {
            eprintln!("pq: --kill needs a pattern or a --net filter (--port / --listen)");
            std::process::exit(1);
        }
        if net_mode(&args) {
            run_net_kill(&args, pattern);
        } else {
            run_kill(pattern.unwrap(), &args);
        }
        return;
    }

    if net_mode(&args) {
        run_net_report(&args);
        return;
    }

    let metric = if args.swap { Metric::Swap } else if args.memory { Metric::Memory } else { Metric::Cpu };

    // Always take the two-sample CPU delta (even in --memory mode) so both the cpu and memory
    // columns are real numbers, not a fake 0. The interval is small; --kill exits before here.
    // The per-core /proc/stat samples ride along at the same two points, for --cpu's per-core rings.
    let prev = proc::snapshot();
    let stat0 = proc::cpu_stat();
    std::thread::sleep(std::time::Duration::from_millis(args.interval));
    let cur = proc::snapshot();
    let stat1 = proc::cpu_stat();
    let procs = proc::cpu_percent(&prev, &cur, args.interval as f64 / 1000.0);
    let core_loads = proc::per_core_busy(&stat0, &stat1);
    // Which process(es) actually ran on each core, for the --cpu donut's process-colored rings.
    let attribution = proc::per_core_attribution(&procs);

    let mut clusters = cluster::cluster(&procs, metric);
    // System-wide, computed before any pattern filter: the header's cpu% is a system total, matching
    // the mem/swap totals beside it (which are always system-wide), so a filtered report never shows
    // a small filtered cpu% next to unfiltered mem/swap.
    let cpu_total: f64 = clusters.iter().map(|c| c.cpu).sum();

    if let Some(pattern) = &args.pattern {
        let needle = pattern.to_lowercase();
        clusters.retain(|c| {
            c.identity.to_lowercase().contains(&needle)
                || c.members.iter().any(|m| {
                    m.cmd.to_lowercase().contains(&needle) || m.comm.to_lowercase().contains(&needle)
                })
        });
    }

    let (mem_used, mem_total) = proc::mem_info();
    let (swap_used, swap_total) = proc::swap_info();

    // Colors/width are only meaningful on an interactive terminal; JSON and piped output stay plain.
    let interactive = std::io::stdout().is_terminal() && !args.json;
    // Graphics need an interactive terminal that also speaks a raster protocol; the capability probe
    // is only worth running when we might actually use it.
    let graphics = interactive && graphics::supported();
    let opts = report::ReportOpts {
        metric,
        top: args.top.unwrap_or(15),
        verbose: args.verbose,
        colors: interactive,
        width: if interactive { term::stdout_width() } else { None }
    };

    let mut out = BufWriter::new(io::stdout());
    let result = if args.json {
        report::write_json(&mut out, &clusters, mem_used, mem_total, swap_used, swap_total, cpu_total, &opts)
    } else if graphics {
        report::write_graphics(&mut out, &clusters, mem_used, mem_total, swap_used, swap_total, cpu_total, &core_loads, &attribution, &opts)
    } else {
        report::write_text(&mut out, &clusters, mem_used, mem_total, swap_used, swap_total, cpu_total, &opts)
    }.and_then(|_| out.flush());

    match result {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => {
            eprintln!("pq: error writing output: {}", e);
            std::process::exit(1);
        }
    }
}

/// --port and --listen imply --net, so `pq --port 8080` works without the extra flag.
fn net_mode(args: &Args) -> bool {
    args.net || args.port.is_some() || args.listen
}

/// The --kill pattern, if a non-empty one was given (bare `--kill` parses as Some("")).
fn kill_pattern(args: &Args) -> Option<&str> {
    args.kill.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// One socket sample: read the tables, attribute inodes to the sampled pids. Returns the
/// attributed sockets and how many /proc/net tables were readable (0 = not Linux / no procfs).
fn sample_sockets(procs: &[proc::Proc]) -> (Vec<(net::Sock, Option<i32>)>, usize) {
    let (socks, tables_read) = net::read_sockets();
    let pids: Vec<i32> = procs.iter().map(|p| p.pid).collect();
    let inodes = net::inode_map(&pids);
    (net::attribute(socks, &inodes), tables_read)
}

/// `--net` report: sample processes (same two-sample cpu delta as the normal report, so the cpu
/// column is real), read and attribute sockets, filter, cluster, render.
fn run_net_report(args: &Args) {
    let prev = proc::snapshot();
    std::thread::sleep(Duration::from_millis(args.interval));
    let procs = proc::cpu_percent(&prev, &proc::snapshot(), args.interval as f64 / 1000.0);

    let (attributed, tables_read) = sample_sockets(&procs);
    if tables_read == 0 {
        eprintln!("pq: cannot read /proc/net (is this Linux?)");
        std::process::exit(1);
    }
    let filtered = net::filter_socks(attributed, args.port, args.listen);
    // Per-socket byte counters from sock_diag (what ss -i reads); failure degrades to blanks.
    let traffic = netdiag::tcp_traffic();
    let report = net::net_clusters(&procs, filtered, args.pattern.as_deref(), traffic);

    let (mem_used, mem_total) = proc::mem_info();
    let interactive = std::io::stdout().is_terminal() && !args.json;
    let graphics = interactive && graphics::supported();
    let opts = net_report::NetOpts {
        // --listen means "show me what's serving": never cut a listener off by default.
        top: args.top.unwrap_or(if args.listen { usize::MAX } else { 15 }),
        verbose: args.verbose,
        colors: interactive,
        width: if interactive { term::stdout_width() } else { None },
        port: args.port
    };

    let mut out = BufWriter::new(io::stdout());
    let result = if args.json {
        net_report::write_json(&mut out, &report, mem_used, mem_total, &opts)
    } else if args.port.is_some() || (args.pattern.is_some() && !args.verbose) {
        // A targeted question (a port, or a named app): answer it with detail cards, not a
        // donut. -v opts back into the raw per-connection grid.
        net_report::write_net_detail(&mut out, &report, &procs, mem_used, mem_total, &opts)
    } else if graphics {
        net_report::write_graphics(&mut out, &report, mem_used, mem_total, &opts)
    } else {
        net_report::write_text(&mut out, &report, mem_used, mem_total, &opts)
    }.and_then(|_| out.flush());

    match result {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => {
            eprintln!("pq: error writing output: {}", e);
            std::process::exit(1);
        }
    }
}

/// Net-mode `--kill`: with --port, only an attributed LISTENING socket on that port selects (the
/// spec's "kill what is listening" rule, with diagnostics for the no-listener and cannot-attribute
/// cases); otherwise the selection is every attributed socket passing --listen, narrowed by the
/// pattern if one was given.
fn run_net_kill(args: &Args, pattern: Option<&str>) {
    let self_pid = std::process::id() as i32;
    let prev = proc::snapshot();
    std::thread::sleep(Duration::from_millis(args.interval));
    let procs = proc::cpu_percent(&prev, &proc::snapshot(), args.interval as f64 / 1000.0);

    let (attributed, tables_read) = sample_sockets(&procs);
    if tables_read == 0 {
        eprintln!("pq: cannot read /proc/net (is this Linux?)");
        std::process::exit(1);
    }

    let owners: Vec<(net::Sock, i32)> = match args.port {
        Some(port) => match net::port_kill_candidates(&attributed, port) {
            net::PortCandidates::Listeners(owners) => owners,
            net::PortCandidates::Unattributable { uid } => {
                eprintln!(
                    "pq: port {} is held by uid {} ({}) but the process cannot be identified; re-run with sudo",
                    port, uid, kill::username(uid)
                );
                std::process::exit(1);
            }
            net::PortCandidates::NoListener(others) => {
                if others.is_empty() {
                    println!("pq: no sockets with local port {}", port);
                } else {
                    println!("pq: no listener on port {}; these sockets merely have it as their local port:", port);
                    for s in &others {
                        println!("  {}", s.label());
                    }
                    println!("pq: not signaling (--port --kill only targets listeners)");
                }
                return;
            }
        },
        None => net::filter_socks(attributed, None, args.listen).into_iter()
            .filter_map(|(s, pid)| pid.map(|p| (s, p)))
            .collect()
    };

    let targets = kill::select_owning(&procs, &owners, pattern, args.exact, self_pid);
    if targets.is_empty() {
        println!("pq: no matching socket owners");
        return;
    }
    let headline = match args.port {
        Some(port) => format!("own port {}", port),
        None => "own matching sockets".to_string()
    };
    preview_and_kill(&targets, &procs, args, &headline);
}

/// `--kill <pattern>`: sample, select the matching subtree(s), preview, confirm, then escalate
/// SIGTERM -> (wait `--grace`) -> SIGKILL. Never signals pq itself, its ancestor chain, or pid 1
/// (enforced by `kill::select`).
fn run_kill(pattern: &str, args: &Args) {
    if pattern.trim().is_empty() {
        eprintln!("pq: --kill needs a non-empty pattern");
        std::process::exit(1);
    }

    let self_pid = std::process::id() as i32;

    // A real two-sample delta so the preview's cpu% column is meaningful, same cadence as the
    // normal report.
    let prev = proc::snapshot();
    std::thread::sleep(Duration::from_millis(args.interval));
    let procs = proc::cpu_percent(&prev, &proc::snapshot(), args.interval as f64 / 1000.0);

    let targets = kill::select(&procs, pattern, args.exact, self_pid);

    if targets.is_empty() {
        println!("pq: no processes match \"{}\"", pattern);
        return;
    }
    preview_and_kill(&targets, &procs, args, &format!("match \"{}\"", pattern));
}

/// Preview the targets (with a socket column when net mode supplied one), confirm unless -y,
/// then escalate. Shared by pattern kill and net kill.
fn preview_and_kill(targets: &[kill::Target], procs: &[proc::Proc], args: &Args, headline: &str) {
    let by_pid: HashMap<i32, &proc::Proc> = procs.iter().map(|p| (p.pid, p)).collect();

    let has_sock = targets.iter().any(|t| t.sock.is_some());

    println!("pq: {} process(es) {}:", targets.len(), headline);
    for t in targets {
        let (cpu, rss) = by_pid.get(&t.pid).map(|p| (p.cpu_pct, p.rss)).unwrap_or((0.0, 0));
        if has_sock {
            let sock = t.sock.as_deref().unwrap_or("");
            println!(
                "  {:>7} {:<10} {:>5.1}%  {:>8}  {:<14} {:<32} {}",
                t.pid, kill::username(t.uid), cpu, style::human_plain(rss), t.identity, sock, t.cmd
            );
        } else {
            println!(
                "  {:>7} {:<10} {:>5.1}%  {:>8}  {:<14} {}",
                t.pid, kill::username(t.uid), cpu, style::human_plain(rss), t.identity, t.cmd
            );
        }
    }

    if args.dry_run {
        return;
    }
    if !args.yes && !confirm() {
        eprintln!("pq: aborted");
        return;
    }
    let (sig, grace) = if args.sigkill {
        (libc::SIGKILL, 0)
    } else {
        (kill::parse_signal(&args.signal), args.grace)
    };
    let result = kill::escalate(&kill::RealSender, targets, sig, grace, &|d| std::thread::sleep(d));
    println!(
        "pq: terminated {}, killed {} via SIGKILL, {} not permitted",
        result.terminated.len(), result.killed.len(), result.failed.len()
    );
    if !result.failed.is_empty() {
        std::process::exit(1);
    }
}

/// Prompt `Proceed? [y/N]` on stderr and read a line from stdin; only "y"/"yes" (any case) proceeds.
fn confirm() -> bool {
    eprint!("Proceed? [y/N] ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn port_and_listen_imply_net_mode() {
        let a = Args::try_parse_from(["pq", "--port", "8080"]).unwrap();
        assert!(net_mode(&a));
        let a = Args::try_parse_from(["pq", "--listen"]).unwrap();
        assert!(net_mode(&a));
        let a = Args::try_parse_from(["pq"]).unwrap();
        assert!(!net_mode(&a));
    }

    #[test]
    fn bare_kill_parses_as_no_pattern() {
        let a = Args::try_parse_from(["pq", "--port", "8080", "--kill"]).unwrap();
        assert_eq!(a.kill.as_deref(), Some(""));
        assert_eq!(kill_pattern(&a), None);
    }

    #[test]
    fn kill_with_pattern_still_works() {
        let a = Args::try_parse_from(["pq", "--kill", "gradle"]).unwrap();
        assert_eq!(kill_pattern(&a), Some("gradle"));
    }

    #[test]
    fn port_rejects_non_numeric() {
        assert!(Args::try_parse_from(["pq", "--port", "http"]).is_err());
    }
}
