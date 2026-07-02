//! /proc sampling: one snapshot per process (pid, ppid, comm, cmdline, rss, uid, cumulative cpu
//! jiffies), plus a pure merge of two snapshots into per-pid cpu%. Kept pure and testable: reading
//! /proc is isolated in `snapshot()`/`mem_info()`, while `cpu_percent_with_tck` (the core the tests
//! drive) takes plain data in and out.

use std::collections::HashMap;
use std::fs;
use std::sync::OnceLock;

/// One /proc/<pid> reading. `cpu_jiffies` is the raw cumulative utime+stime.
#[derive(Debug, Clone)]
pub struct Sample {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub cmdline: Vec<String>,
    pub rss: u64,
    pub swap: u64,
    pub uid: u32,
    pub cpu_jiffies: u64
}

/// A process with cpu% already computed over an interval, ready for clustering.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub cmdline: Vec<String>,
    pub rss: u64,
    pub swap: u64,
    // Not read yet in Stage 2's report; carried through for the Stage 4 kill preview's user column.
    #[allow(dead_code)]
    pub uid: u32,
    pub cpu_pct: f64
}

/// One pass over /proc: every numeric entry that we can read becomes a `Sample`. Per-pid races
/// (the process exits mid-read) or permission errors just drop that pid, never fail the whole scan.
pub fn snapshot() -> Vec<Sample> {
    let mut out = Vec::new();
    let entries = match fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(_) => return out
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue
        };
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let pid: i32 = match name.parse() {
            Ok(p) => p,
            Err(_) => continue
        };
        if let Some(sample) = read_sample(pid) {
            out.push(sample);
        }
    }
    out
}

fn read_sample(pid: i32) -> Option<Sample> {
    let (comm, ppid, cpu_jiffies) = read_stat(pid)?;
    let (rss, swap, uid) = read_status(pid);
    let cmdline = read_cmdline(pid);
    Some(Sample { pid, ppid, comm, cmdline, rss, swap, uid, cpu_jiffies })
}

/// Split a /proc/<pid>/stat (or task/<tid>/stat) line into its comm and space-separated fields:
/// comm is between the first `(` and the last `)` (it can itself contain spaces/parens), and the
/// fields start at index 0 = state, immediately after the closing paren.
fn parse_stat(raw: &str) -> Option<(&str, Vec<&str>)> {
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    let comm = &raw[open + 1..close];
    let fields: Vec<&str> = raw[close + 1..].trim_start().split_whitespace().collect();
    Some((comm, fields))
}

/// Parse /proc/<pid>/stat: ppid is field 1, utime/stime are fields 11/12 (0-indexed after comm).
fn read_stat(pid: i32) -> Option<(String, i32, u64)> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (comm, fields) = parse_stat(&raw)?;
    let ppid: i32 = fields.get(1)?.parse().ok()?;
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((comm.to_string(), ppid, utime + stime))
}

/// Parse /proc/<pid>/status for VmRSS and VmSwap (KB -> bytes) and the real uid (first number on
/// the Uid line). Missing fields (e.g. a kernel thread with no VmRSS, or a process never swapped
/// so it has no VmSwap line) default to 0.
fn read_status(pid: i32) -> (u64, u64, u32) {
    let raw = match fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(raw) => raw,
        Err(_) => return (0, 0, 0)
    };
    let mut rss = 0u64;
    let mut swap = 0u64;
    let mut uid = 0u32;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            if let Some(kb) = rest.trim().split_whitespace().next() {
                rss = kb.parse::<u64>().unwrap_or(0) * 1024;
            }
        } else if let Some(rest) = line.strip_prefix("VmSwap:") {
            if let Some(kb) = rest.trim().split_whitespace().next() {
                swap = kb.parse::<u64>().unwrap_or(0) * 1024;
            }
        } else if let Some(rest) = line.strip_prefix("Uid:") {
            if let Some(first) = rest.trim().split_whitespace().next() {
                uid = first.parse::<u32>().unwrap_or(0);
            }
        }
    }
    (rss, swap, uid)
}

/// Parse /proc/<pid>/cmdline: NUL-separated argv, empty for kernel threads.
fn read_cmdline(pid: i32) -> Vec<String> {
    let raw = match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(raw) => raw,
        Err(_) => return Vec::new()
    };
    raw.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Clock ticks per second (jiffies/sec), from sysconf, cached after the first call.
pub fn clk_tck() -> u64 {
    static TCK: OnceLock<u64> = OnceLock::new();
    *TCK.get_or_init(|| {
        let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if tck > 0 { tck as u64 } else { 100 }
    })
}

/// Online CPU count (at least 1), from sysconf, cached. Used to normalize cpu% to 0-100% of total
/// capacity rather than the per-core convention where one busy core reads 100%.
pub fn ncores() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        if n > 0 { n as usize } else { 1 }
    })
}

/// Merge two snapshots into per-pid cpu% over `elapsed_secs`, normalized to 0-100% of total CPU
/// (divided across all online cores), using the live clk_tck.
pub fn cpu_percent(prev: &[Sample], cur: &[Sample], elapsed_secs: f64) -> Vec<Proc> {
    let mut procs = cpu_percent_with_tck(prev, cur, elapsed_secs, clk_tck());
    let cores = ncores().max(1) as f64;
    for p in &mut procs {
        p.cpu_pct /= cores;
    }
    procs
}

/// The pure core of `cpu_percent`: only processes present in `cur` are kept (vanished pids are
/// dropped); a pid with no entry in `prev` (newly appeared) gets 0% rather than a spurious spike.
pub fn cpu_percent_with_tck(prev: &[Sample], cur: &[Sample], elapsed_secs: f64, tck: u64) -> Vec<Proc> {
    let prev_j: HashMap<i32, u64> = prev.iter().map(|s| (s.pid, s.cpu_jiffies)).collect();
    let denom = elapsed_secs * tck as f64; // jiffies of one core over the interval
    cur.iter().map(|s| {
        let delta = s.cpu_jiffies.saturating_sub(*prev_j.get(&s.pid).unwrap_or(&s.cpu_jiffies));
        let cpu_pct = if denom > 0.0 { delta as f64 / denom * 100.0 } else { 0.0 };
        Proc {
            pid: s.pid, ppid: s.ppid, comm: s.comm.clone(), cmdline: s.cmdline.clone(),
            rss: s.rss, swap: s.swap, uid: s.uid, cpu_pct
        }
    }).collect()
}

/// Per-core cumulative CPU jiffies from /proc/stat: one `(busy, total)` pair per `cpuN` line (the
/// aggregate `cpu ` line is skipped), in core order. `total` sums every field on the line; `busy` is
/// `total` minus idle (field 4, 0-indexed after the `cpuN` token) and iowait (field 5).
pub fn cpu_stat() -> Vec<(u64, u64)> {
    let raw = match fs::read_to_string("/proc/stat") {
        Ok(raw) => raw,
        Err(_) => return Vec::new()
    };
    let mut out = Vec::new();
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let label = match fields.next() {
            Some(l) => l,
            None => continue
        };
        if !(label.len() > 3 && label.starts_with("cpu") && label[3..].chars().next().is_some_and(|c| c.is_ascii_digit())) {
            continue;
        }
        let nums: Vec<u64> = fields.filter_map(|f| f.parse().ok()).collect();
        let total: u64 = nums.iter().sum();
        let idle_all = nums.get(3).copied().unwrap_or(0) + nums.get(4).copied().unwrap_or(0);
        let busy = total.saturating_sub(idle_all);
        out.push((busy, total));
    }
    out
}

/// Per-core CPU load (0-100) between two `cpu_stat()` samples: for each core index present in both,
/// the busy fraction of the total-jiffies delta over the interval, clamped to 0..=100. A core with no
/// jiffies delta (e.g. sampled too close together) reads 0 rather than dividing by zero.
pub fn per_core_busy(prev: &[(u64, u64)], cur: &[(u64, u64)]) -> Vec<f64> {
    prev.iter().zip(cur.iter()).map(|(&(pbusy, ptotal), &(cbusy, ctotal))| {
        let dbusy = cbusy.saturating_sub(pbusy);
        let dtotal = ctotal.saturating_sub(ptotal);
        let load = if dtotal > 0 { (dbusy as f64 / dtotal as f64) * 100.0 } else { 0.0 };
        load.clamp(0.0, 100.0)
    }).collect()
}

/// Per-thread (utime+stime, last-ran-on core) pairs for every task under /proc/<pid>/task, read once
/// per active process. `None` if the task directory can't be read at all (process exited mid-read);
/// `Some(vec![])` if it's readable but empty (shouldn't happen for a live process, but distinguishing
/// it from `None` lets the caller fall back the same way either way).
fn read_thread_stats(pid: i32) -> Option<Vec<(i32, u64)>> {
    let entries = fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue
        };
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let tid: i32 = match name.parse() {
            Ok(t) => t,
            Err(_) => continue
        };
        if let Some((weight, core)) = read_task_stat(pid, tid) {
            out.push((core, weight));
        }
    }
    Some(out)
}

/// Parse /proc/<pid>/task/<tid>/stat for (utime+stime, processor): same field layout as `read_stat`
/// (comm stripped, fields 0-indexed from `state`), plus field 36 = processor, the core the thread
/// last ran on (verified empirically against `taskset`/`ps -o psr=`: field 39 in the 1-indexed man
/// page numbering, which lands at index 36 once comm/pid are stripped as `read_stat` does).
fn read_task_stat(pid: i32, tid: i32) -> Option<(u64, i32)> {
    let raw = fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")).ok()?;
    let (_, fields) = parse_stat(&raw)?;
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let processor: i32 = fields.get(36)?.parse().ok()?;
    Some((utime + stime, processor))
}

/// The process-level "last ran on" core from /proc/<pid>/stat itself (same field 36), used as a
/// fallback when per-thread stats aren't available.
fn read_stat_processor(pid: i32) -> Option<i32> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = parse_stat(&raw)?;
    fields.get(36)?.parse().ok()
}

/// Distribute `total` across `threads` (each a `(core, cumulative-jiffy weight)` pair): weighted by
/// each thread's share of the summed weight, merging entries that land on the same core. If every
/// thread has zero weight (e.g. sampled before any jiffies accrued), split `total` evenly across the
/// distinct cores the threads occupy instead of dividing by a zero sum.
fn distribute(total: f64, threads: &[(i32, u64)]) -> Vec<(i32, f64)> {
    if threads.is_empty() {
        return Vec::new();
    }
    let sum_weight: u64 = threads.iter().map(|&(_, w)| w).sum();
    let mut acc: HashMap<i32, f64> = HashMap::new();
    if sum_weight == 0 {
        let mut distinct: Vec<i32> = Vec::new();
        for &(core, _) in threads {
            if !distinct.contains(&core) {
                distinct.push(core);
            }
        }
        let share = total / distinct.len() as f64;
        for core in distinct {
            *acc.entry(core).or_insert(0.0) += share;
        }
    } else {
        for &(core, weight) in threads {
            *acc.entry(core).or_insert(0.0) += total * (weight as f64 / sum_weight as f64);
        }
    }
    acc.into_iter().collect()
}

/// Attribute each active process's CPU usage to the cores it actually ran on, for the `--cpu` donut's
/// per-core process coloring. Returns, per core index (0..ncores()), a list of `(pid, per_core_cpu_pct)`
/// where the percentage is 0-100 of that single core (not of total system capacity).
///
/// Only processes with `cpu_pct > 0.0` are considered (idle processes contribute nothing, and this
/// keeps the per-process `/proc/<pid>/task` walk cheap: it only runs for processes that were actually
/// busy this interval). For each such process, `cpu_pct` (already normalized to 0-100 of total
/// capacity) is un-normalized back to per-core units by multiplying by `ncores()`, then distributed
/// across cores by that process's threads' cumulative jiffies on each core. If the thread walk fails
/// (process exited mid-read), the whole total falls back to the process-level "last ran on" core from
/// its own stat; if that's unavailable too, the process is skipped entirely.
pub fn per_core_attribution(procs: &[Proc]) -> Vec<Vec<(i32, f64)>> {
    let n = ncores();
    let mut result: Vec<HashMap<i32, f64>> = vec![HashMap::new(); n];
    for p in procs {
        if p.cpu_pct <= 0.0 {
            continue;
        }
        let total = p.cpu_pct * ncores() as f64;
        match read_thread_stats(p.pid) {
            Some(threads) if !threads.is_empty() => {
                for (core, contribution) in distribute(total, &threads) {
                    if core >= 0 && (core as usize) < n {
                        *result[core as usize].entry(p.pid).or_insert(0.0) += contribution;
                    }
                }
            }
            _ => {
                if let Some(core) = read_stat_processor(p.pid) {
                    if core >= 0 && (core as usize) < n {
                        *result[core as usize].entry(p.pid).or_insert(0.0) += total;
                    }
                }
            }
        }
    }
    result.into_iter().map(|m| m.into_iter().collect()).collect()
}

/// System memory used/total in bytes, from /proc/meminfo (used = MemTotal - MemAvailable).
pub fn mem_info() -> (u64, u64) {
    let raw = match fs::read_to_string("/proc/meminfo") {
        Ok(raw) => raw,
        Err(_) => return (0, 0)
    };
    let mut total = 0u64;
    let mut available = 0u64;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = parse_kb(rest);
        }
    }
    let used = total.saturating_sub(available);
    (used, total)
}

/// System swap used/total in bytes, from /proc/meminfo (used = SwapTotal - SwapFree).
pub fn swap_info() -> (u64, u64) {
    let raw = match fs::read_to_string("/proc/meminfo") {
        Ok(raw) => raw,
        Err(_) => return (0, 0)
    };
    let mut total = 0u64;
    let mut free = 0u64;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("SwapTotal:") {
            total = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("SwapFree:") {
            free = parse_kb(rest);
        }
    }
    let used = total.saturating_sub(free);
    (used, total)
}

fn parse_kb(field: &str) -> u64 {
    field.trim().split_whitespace().next()
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percent_from_two_samples() {
        // clk_tck 100: 50 jiffies over 0.5s = 100 jiffies/s of one core busy = 100%.
        let prev = vec![Sample{pid:1,ppid:0,comm:"a".into(),cmdline:vec![],rss:10,swap:0,uid:0,cpu_jiffies:100}];
        let cur  = vec![Sample{pid:1,ppid:0,comm:"a".into(),cmdline:vec![],rss:20,swap:5,uid:0,cpu_jiffies:150}];
        let procs = cpu_percent_with_tck(&prev,&cur,0.5,100);
        assert_eq!(procs.len(),1);
        assert!((procs[0].cpu_pct - 100.0).abs() < 0.01);
        assert_eq!(procs[0].rss, 20); // newer field kept
        assert_eq!(procs[0].swap, 5); // newer field kept
    }

    #[test]
    fn cpu_percent_ignores_vanished_and_new() {
        let prev = vec![Sample{pid:1,ppid:0,comm:"a".into(),cmdline:vec![],rss:10,swap:0,uid:0,cpu_jiffies:100},
                        Sample{pid:2,ppid:0,comm:"b".into(),cmdline:vec![],rss:10,swap:0,uid:0,cpu_jiffies:0}];
        let cur  = vec![Sample{pid:2,ppid:0,comm:"b".into(),cmdline:vec![],rss:10,swap:0,uid:0,cpu_jiffies:10},
                        Sample{pid:3,ppid:0,comm:"c".into(),cmdline:vec![],rss:10,swap:0,uid:0,cpu_jiffies:5}];
        // pid1 vanished -> dropped; pid3 new (no prev) -> cpu 0; pid2 present in both.
        let procs = cpu_percent_with_tck(&prev,&cur,1.0,100);
        let by = |p:i32| procs.iter().find(|x|x.pid==p);
        assert!(by(1).is_none());
        assert!((by(2).unwrap().cpu_pct - 10.0).abs() < 0.01);
        assert_eq!(by(3).unwrap().cpu_pct, 0.0);
    }

    #[test]
    fn snapshot_includes_this_process() {
        assert!(snapshot().iter().any(|s| s.pid == std::process::id() as i32));
    }

    #[test]
    fn per_core_attribution_smoke() {
        // Real /proc on this machine: just assert it runs, returns one entry per core, and every
        // contribution is non-negative (doesn't panic/underflow on live data).
        let s = snapshot();
        let procs = cpu_percent(&s, &s, 1.0); // same snapshot twice -> 0% for everyone, still exercises the shape
        let attribution = per_core_attribution(&procs);
        assert_eq!(attribution.len(), ncores());
        for core in &attribution {
            for &(_, pct) in core {
                assert!(pct >= 0.0);
            }
        }
    }

    #[test]
    fn per_core_busy_from_two_samples() {
        // One core, 50 busy jiffies out of 100 total delta -> 50%.
        let prev = vec![(0u64, 0u64)];
        let cur = vec![(50u64, 100u64)];
        let loads = per_core_busy(&prev, &cur);
        assert_eq!(loads.len(), 1);
        assert!((loads[0] - 50.0).abs() < 0.01);

        // A core with no total delta reads 0.0 rather than panicking or NaN.
        let prev2 = vec![(10u64, 200u64)];
        let cur2 = vec![(10u64, 200u64)];
        assert_eq!(per_core_busy(&prev2, &cur2), vec![0.0]);
    }

    #[test]
    fn distribute_weighted_split() {
        let threads = [(0, 50u64), (1, 50u64)];
        let mut d = distribute(100.0, &threads);
        d.sort_by_key(|&(c, _)| c);
        assert_eq!(d.len(), 2);
        assert!((d[0].1 - 50.0).abs() < 0.01);
        assert!((d[1].1 - 50.0).abs() < 0.01);

        // Uneven weights split proportionally.
        let threads2 = [(2, 25u64), (5, 75u64)];
        let mut d2 = distribute(80.0, &threads2);
        d2.sort_by_key(|&(c, _)| c);
        assert!((d2[0].1 - 20.0).abs() < 0.01); // core 2: 80 * 25/100
        assert!((d2[1].1 - 60.0).abs() < 0.01); // core 5: 80 * 75/100
    }

    #[test]
    fn distribute_zero_weight_even_split() {
        let threads = [(0, 0u64), (1, 0u64), (0, 0u64)];
        let mut d = distribute(90.0, &threads);
        d.sort_by_key(|&(c, _)| c);
        assert_eq!(d.len(), 2); // distinct cores 0 and 1, core 0's two entries merged
        assert!((d[0].1 - 45.0).abs() < 0.01);
        assert!((d[1].1 - 45.0).abs() < 0.01);
    }

    #[test]
    fn distribute_single_thread() {
        let threads = [(3, 10u64)];
        assert_eq!(distribute(75.0, &threads), vec![(3, 75.0)]);
    }

    #[test]
    fn distribute_empty_is_empty() {
        assert_eq!(distribute(50.0, &[]), Vec::new());
    }

    #[test]
    fn swap_info_smoke() {
        // Real /proc/meminfo on this machine; just assert it parses to something sane
        // (total >= used, and doesn't panic/underflow).
        let (used, total) = swap_info();
        assert!(used <= total);
    }
}
