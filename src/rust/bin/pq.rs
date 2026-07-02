//! pq: query and cluster the processes eating your CPU/memory. Companion to dq, sharing the same
//! rendering primitives (style/donut/term/graphics/chart) from the qtools library.
mod proc; mod identity; mod cluster; mod report; mod kill;

use std::collections::HashMap;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::time::Duration;

use clap::Parser;
use qtools::{graphics, style, term};

use cluster::Metric;

#[derive(Parser, Debug)]
#[command(name = "pq", about = "pq: query and cluster the processes eating your CPU/memory.")]
struct Args {
    /// Sort and chart by memory instead of CPU
    #[arg(short = 'm', long)] memory: bool,
    /// Sort and chart by swap usage instead of CPU
    #[arg(short = 's', long)] swap: bool,
    /// Sort and chart by CPU (default)
    #[arg(short = 'c', long)] cpu: bool,
    /// Clusters to show
    #[arg(short = 'n', long, default_value_t = 15)] top: usize,
    /// Expand clusters to their member processes
    #[arg(short, long)] verbose: bool,
    /// CPU sample interval in milliseconds
    #[arg(long, default_value_t = 400)] interval: u64,
    /// Emit machine-readable JSON
    #[arg(long)] json: bool,
    /// Kill matching process trees (preview + confirm)
    #[arg(long)] kill: Option<String>,
    /// With --kill: preview only, do not signal
    #[arg(long)] dry_run: bool,
    /// With --kill: skip the confirmation prompt
    #[arg(short = 'y', long)] yes: bool,
    /// With --kill: match the identity exactly instead of substring
    #[arg(short = 'x', long)] exact: bool,
    /// With --kill: initial signal (default TERM)
    #[arg(long, default_value = "TERM")] signal: String,
    /// With --kill: seconds to wait before SIGKILL
    #[arg(long, default_value_t = 4)] grace: u64,
    /// With --kill: SIGKILL immediately (no TERM, no grace)
    #[arg(short = '9', long = "sigkill")] sigkill: bool,
    /// Filter the report to matching clusters
    pattern: Option<String>,
}

fn main() {
    let args = Args::parse();

    if let Some(pattern) = args.kill.clone() {
        run_kill(&pattern, &args);
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

    if let Some(pattern) = &args.pattern {
        let needle = pattern.to_lowercase();
        clusters.retain(|c| {
            c.identity.to_lowercase().contains(&needle)
                || c.members.iter().any(|m| m.cmd.to_lowercase().contains(&needle))
        });
    }

    let (mem_used, mem_total) = proc::mem_info();
    let (swap_used, swap_total) = proc::swap_info();
    let cpu_total: f64 = clusters.iter().map(|c| c.cpu).sum();

    // Colors/width are only meaningful on an interactive terminal; JSON and piped output stay plain.
    let interactive = std::io::stdout().is_terminal() && !args.json;
    // Graphics need an interactive terminal that also speaks a raster protocol; the capability probe
    // is only worth running when we might actually use it.
    let graphics = interactive && graphics::supported();
    let opts = report::ReportOpts {
        metric,
        top: args.top,
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

    let by_pid: HashMap<i32, &proc::Proc> = procs.iter().map(|p| (p.pid, p)).collect();

    println!("pq: {} process(es) match \"{}\":", targets.len(), pattern);
    for t in &targets {
        let (cpu, rss) = by_pid.get(&t.pid).map(|p| (p.cpu_pct, p.rss)).unwrap_or((0.0, 0));
        println!(
            "  {:>7} {:<10} {:>5.1}%  {:>8}  {:<14} {}",
            t.pid,
            kill::username(t.uid),
            cpu,
            style::human_plain(rss),
            t.identity,
            t.cmd
        );
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
    let result = kill::escalate(&kill::RealSender, &targets, sig, grace, &|d| std::thread::sleep(d));

    println!(
        "pq: terminated {}, killed {} via SIGKILL, {} not permitted",
        result.terminated.len(),
        result.killed.len(),
        result.failed.len()
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
