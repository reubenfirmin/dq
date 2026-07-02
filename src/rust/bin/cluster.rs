//! Clustering: roll processes up by resolved identity, folding bare-runtime children (and children
//! that resolve to the same identity as their parent) into the parent's cluster via tree
//! inheritance. Chrome parent + N renderers become one `chrome` cluster; a Gradle daemon plus its
//! workers become one `gradle` cluster.

use std::collections::HashMap;

use crate::identity;
use crate::proc;

/// One process inside a cluster.
pub struct Member {
    pub pid: i32,
    pub cpu: f64,
    pub rss: u64,
    pub swap: u64,
    pub cmd: String,
    pub comm: String
}

/// A group of processes sharing an effective identity.
pub struct Cluster {
    pub identity: String,
    pub cpu: f64,
    pub rss: u64,
    pub swap: u64,
    pub members: Vec<Member>
}

/// Which metric drives sorting/weighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric { Cpu, Memory, Swap }

/// Every pid's effective identity (its own resolved identity, with bare-runtime and
/// same-as-parent children folded into their parent), memoized across the whole process list.
/// Shared by `cluster` and by net mode's socket-to-cluster join, so both group identically.
pub fn effective_identities(procs: &[proc::Proc]) -> HashMap<i32, String> {
    let own_identity: HashMap<i32, String> = procs.iter()
        .map(|p| (p.pid, identity::resolve(&p.comm, &p.cmdline)))
        .collect();
    let parent: HashMap<i32, i32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();

    let mut effective: HashMap<i32, String> = HashMap::new();
    for p in procs {
        effective_identity(p.pid, &own_identity, &parent, &mut effective);
    }
    effective
}

/// Resolve, attribute (with parent inheritance for bare-runtime/same-as-parent children), aggregate,
/// and sort descending by the active metric.
pub fn cluster(procs: &[proc::Proc], metric: Metric) -> Vec<Cluster> {
    let effective = effective_identities(procs);

    let mut groups: HashMap<String, (f64, u64, u64, Vec<Member>)> = HashMap::new();
    for p in procs {
        let key = effective.get(&p.pid).cloned().unwrap_or_else(|| p.comm.clone());
        let entry = groups.entry(key).or_insert((0.0, 0, 0, Vec::new()));
        entry.0 += p.cpu_pct;
        entry.1 += p.rss;
        entry.2 += p.swap;
        entry.3.push(Member { pid: p.pid, cpu: p.cpu_pct, rss: p.rss, swap: p.swap, cmd: p.cmdline.join(" "), comm: p.comm.clone() });
    }

    let mut clusters: Vec<Cluster> = groups.into_iter()
        .map(|(identity, (cpu, rss, swap, members))| Cluster { identity, cpu, rss, swap, members })
        .collect();

    clusters.sort_by(|a, b| metric_ordering(metric, a.cpu, a.rss, a.swap, b.cpu, b.rss, b.swap));
    clusters
}

/// Descending ordering by the active metric's cpu/rss/swap value - shared by cluster ranking and
/// the `-v` member listing, so both sort consistently.
pub fn metric_ordering(metric: Metric, cpu_a: f64, rss_a: u64, swap_a: u64, cpu_b: f64, rss_b: u64, swap_b: u64) -> std::cmp::Ordering {
    match metric {
        Metric::Cpu => cpu_b.partial_cmp(&cpu_a).unwrap_or(std::cmp::Ordering::Equal),
        Metric::Memory => rss_b.cmp(&rss_a),
        Metric::Swap => swap_b.cmp(&swap_a)
    }
}

/// Compute (and memoize) `pid`'s effective identity: its own identity, unless that identity is a
/// bare runtime or matches its parent's own identity, in which case fold into the parent's effective
/// identity. Cycle-guarded (a pid can't be its own ancestor in /proc, but we guard anyway) and
/// memoized so a deep tree is only walked once per pid.
fn effective_identity(pid: i32, own: &HashMap<i32, String>, parent: &HashMap<i32, i32>, memo: &mut HashMap<i32, String>) -> String {
    if let Some(cached) = memo.get(&pid) {
        return cached.clone();
    }
    // Reserve a placeholder to guard against cycles (shouldn't happen in a real /proc tree, but a
    // synthetic/test tree could form one); if we re-enter for the same pid, just use its own identity.
    let own_id = match own.get(&pid) {
        Some(id) => id.clone(),
        None => return String::new()
    };

    let ppid = parent.get(&pid).copied();
    let parent_own = ppid.and_then(|pp| own.get(&pp).cloned());
    let folds_into_parent = ppid.is_some()
        && ppid != Some(pid)
        && (identity::is_bare_runtime(&own_id) || parent_own.as_deref() == Some(own_id.as_str()));

    let result = if folds_into_parent {
        let pp = ppid.unwrap();
        if own.contains_key(&pp) {
            effective_identity(pp, own, parent, memo)
        } else {
            own_id
        }
    } else {
        own_id
    };

    memo.insert(pid, result.clone());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, comm: &str, cmd: &[&str], cpu: f64, rss: u64) -> proc::Proc {
        proc::Proc {
            pid, ppid, comm: comm.into(), cmdline: cmd.iter().map(|s| s.to_string()).collect(),
            rss, swap: 0, uid: 0, cpu_pct: cpu
        }
    }

    #[test]
    fn daemon_and_generic_worker_cluster_together() {
        let procs = vec![
            p(100, 1, "java", &["java", "org.gradle...GradleDaemon"], 10.0, 1000),
            p(101, 100, "java", &["java", "-cp", "/tmp/worker.jar"], 20.0, 2000), // generic child -> inherits gradle
            p(200, 1, "postgres", &["postgres", "-D", "/data"], 5.0, 500),
        ];
        let mut cs = cluster(&procs, Metric::Cpu);
        cs.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap());
        assert_eq!(cs[0].identity, "gradle");
        assert!((cs[0].cpu - 30.0).abs() < 0.01);
        assert_eq!(cs[0].rss, 3000);
        assert_eq!(cs[0].swap, 0);
        assert_eq!(cs[0].members.len(), 2);
        assert_eq!(cs[1].identity, "postgres");
    }

    #[test]
    fn sorts_by_swap_when_metric_is_swap() {
        let mut procs = vec![
            p(100, 1, "alpha", &["alpha"], 1.0, 100),
            p(200, 1, "beta", &["beta"], 1.0, 100),
        ];
        procs[0].swap = 10;
        procs[1].swap = 500;
        let cs = cluster(&procs, Metric::Swap);
        assert_eq!(cs[0].identity, "beta");
        assert_eq!(cs[0].swap, 500);
        assert_eq!(cs[1].identity, "alpha");
        assert_eq!(cs[1].swap, 10);
    }

    #[test]
    fn effective_identities_folds_children_into_parent() {
        let procs = vec![
            p(100, 1, "java", &["java", "org.gradle...GradleDaemon"], 0.0, 0),
            p(101, 100, "java", &["java", "-cp", "/tmp/worker.jar"], 0.0, 0),
            p(200, 1, "postgres", &["postgres", "-D", "/data"], 0.0, 0),
        ];
        let ids = effective_identities(&procs);
        assert_eq!(ids.get(&100).map(String::as_str), Some("gradle"));
        assert_eq!(ids.get(&101).map(String::as_str), Some("gradle"));
        assert_eq!(ids.get(&200).map(String::as_str), Some("postgres"));
    }
}
