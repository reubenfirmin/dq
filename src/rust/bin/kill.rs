//! Kill mode: pure selection (match + subtree expansion + safety exclusions) and signal
//! escalation (TERM, wait, KILL survivors) behind a `SignalSender` trait so the logic is
//! unit-tested without touching real processes.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::identity;
use crate::proc;

/// One process selected for signaling: enough to preview and to signal.
#[derive(Debug, Clone)]
pub struct Target {
    pub pid: i32,
    pub identity: String,
    pub cmd: String,
    pub uid: u32,
}

/// Match `pattern` (case-insensitive substring of resolved identity, comm, or full cmdline;
/// exact identity match if `exact`) against every process, then expand each match to its full
/// subtree (descendants), excluding `self_pid`, its ancestor chain, and pid 1.
pub fn select(procs: &[proc::Proc], pattern: &str, exact: bool, self_pid: i32) -> Vec<Target> {
    if pattern.trim().is_empty() {
        return Vec::new();
    }
    let needle = pattern.to_lowercase();

    // pid -> children, for subtree expansion.
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p.pid);
    }
    // pid -> Proc, for lookups while expanding/excluding.
    let by_pid: HashMap<i32, &proc::Proc> = procs.iter().map(|p| (p.pid, p)).collect();

    let matches_pattern = |p: &proc::Proc| -> bool {
        let ident = identity::resolve(&p.comm, &p.cmdline);
        if exact {
            ident.to_lowercase() == needle
        } else {
            ident.to_lowercase().contains(&needle)
                || p.comm.to_lowercase().contains(&needle)
                || p.cmdline.join(" ").to_lowercase().contains(&needle)
        }
    };

    // Compute exclusions first (self, self's ancestor chain, pid 1). These must never be signaled,
    // AND must not seed subtree expansion: otherwise matching the invoking shell (whose command line
    // contains the pattern you just typed) would sweep in the shell's OTHER children, killing
    // unrelated jobs. So an excluded process never acts as a match root.
    let excluded = excluded_set(self_pid, &by_pid);

    let mut selected: HashSet<i32> = HashSet::new();
    for p in procs {
        if excluded.contains(&p.pid) {
            continue;
        }
        if matches_pattern(p) {
            expand_subtree(p.pid, &children, &mut selected);
        }
    }

    let mut pids: Vec<i32> = selected.difference(&excluded).copied().collect();
    pids.sort_unstable();
    pids.into_iter()
        .filter_map(|pid| by_pid.get(&pid))
        .map(|p| Target {
            pid: p.pid,
            identity: identity::resolve(&p.comm, &p.cmdline),
            cmd: p.cmdline.join(" "),
            uid: p.uid,
        })
        .collect()
}

/// The pids that must never be signaled or used as a match root: pid 1, `self_pid`, and every
/// ancestor of `self_pid` (our shell/terminal chain). Walks ppid up to the root, cycle-guarded.
fn excluded_set(self_pid: i32, by_pid: &HashMap<i32, &proc::Proc>) -> HashSet<i32> {
    let mut excluded: HashSet<i32> = HashSet::new();
    excluded.insert(1);
    let mut cur = self_pid;
    loop {
        if !excluded.insert(cur) {
            break; // already seen: cycle guard (shouldn't happen with real /proc data)
        }
        match by_pid.get(&cur) {
            Some(p) if p.ppid != cur && p.ppid > 0 => cur = p.ppid,
            _ => break,
        }
    }
    excluded
}

/// DFS the process tree rooted at `pid`, inserting every descendant (and `pid` itself) into
/// `out`. A visited-set guards against cycles in malformed /proc data.
fn expand_subtree(pid: i32, children: &HashMap<i32, Vec<i32>>, out: &mut HashSet<i32>) {
    let mut stack = vec![pid];
    while let Some(p) = stack.pop() {
        if !out.insert(p) {
            continue; // already visited
        }
        if let Some(kids) = children.get(&p) {
            for &k in kids {
                stack.push(k);
            }
        }
    }
}

/// Sends signals and probes liveness. Isolated behind a trait so `escalate`'s logic is unit
/// tested with a mock, and only `RealSender` touches real processes.
pub trait SignalSender {
    fn signal(&self, pid: i32, sig: i32) -> bool;
    fn alive(&self, pid: i32) -> bool;
}

/// The real sender: `libc::kill`. `signal` returns false on any error (e.g. EPERM, ESRCH).
/// `alive` probes with signal 0 (a no-op signal that does not perturb the target).
pub struct RealSender;

impl SignalSender for RealSender {
    fn signal(&self, pid: i32, sig: i32) -> bool {
        unsafe { libc::kill(pid, sig) == 0 }
    }

    fn alive(&self, pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }
}

/// Outcome of an escalation pass.
#[derive(Debug, Default)]
pub struct Killed {
    /// Exited on the initial signal.
    pub terminated: Vec<i32>,
    /// Survived the initial signal and had to be SIGKILLed.
    pub killed: Vec<i32>,
    /// The initial signal could not be delivered (e.g. EPERM).
    pub failed: Vec<i32>,
}

/// Send `first_sig` to every target, wait up to `grace_secs` (polling `alive` afterward), then
/// SIGKILL any survivor. `sleep` is injected so tests run instantly with a no-op.
pub fn escalate<S: SignalSender>(
    s: &S,
    targets: &[Target],
    first_sig: i32,
    grace_secs: u64,
    sleep: &dyn Fn(Duration),
) -> Killed {
    let mut out = Killed::default();
    let mut pending: Vec<i32> = Vec::new();

    for t in targets {
        if s.signal(t.pid, first_sig) {
            pending.push(t.pid);
        } else {
            out.failed.push(t.pid);
        }
    }

    // Nothing survives SIGKILL, so skip the grace wait entirely when that's the first signal.
    if first_sig != libc::SIGKILL {
        sleep(Duration::from_secs(grace_secs));
    }

    for pid in pending {
        if s.alive(pid) {
            s.signal(pid, libc::SIGKILL);
            out.killed.push(pid);
        } else {
            out.terminated.push(pid);
        }
    }

    out
}

/// Resolve a uid to its /etc/passwd username, falling back to the numeric uid if the lookup
/// fails (no matching entry, or the file cannot be read). Used only for the kill preview; not
/// unit-tested since it touches the filesystem, mirroring `proc::snapshot`.
pub fn username(uid: u32) -> String {
    std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                let mut fields = line.split(':');
                let name = fields.next()?;
                let found_uid: u32 = fields.nth(1)?.parse().ok()?;
                (found_uid == uid).then(|| name.to_string())
            })
        })
        .unwrap_or_else(|| uid.to_string())
}

/// Parse a `--signal` argument (bare name, "SIG"-prefixed name, or numeric) into a signal number.
/// Defaults to SIGTERM for anything unrecognized, so a typo never silently no-ops.
pub fn parse_signal(raw: &str) -> i32 {
    let s = raw.trim();
    if let Ok(n) = s.parse::<i32>() {
        if (1..=64).contains(&n) {
            return n;
        }
    }
    let up = s.to_uppercase();
    let up = up.strip_prefix("SIG").unwrap_or(&up);
    match up {
        "TERM" => libc::SIGTERM,
        "KILL" => libc::SIGKILL,
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "STOP" => libc::SIGSTOP,
        "CONT" => libc::SIGCONT,
        _ => libc::SIGTERM,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn p(pid: i32, ppid: i32, comm: &str, cmd: &[&str], cpu: f64, rss: u64) -> proc::Proc {
        proc::Proc {
            pid,
            ppid,
            comm: comm.into(),
            cmdline: cmd.iter().map(|s| s.to_string()).collect(),
            rss,
            swap: 0,
            uid: 0,
            cpu_pct: cpu,
        }
    }

    #[test]
    fn substring_matches_cmdline_not_just_comm() {
        let procs = vec![
            p(100, 1, "java", &["java", "org.gradle...GradleDaemon"], 0.0, 0),
            p(101, 100, "java", &["java", "worker"], 0.0, 0), // child -> included via subtree
            p(200, 1, "vim", &["vim", "build.gradle"], 0.0, 0), // also matches "gradle"
        ];
        let t = select(&procs, "gradle", false, 999);
        let pids: Vec<i32> = t.iter().map(|x| x.pid).collect();
        assert!(pids.contains(&100) && pids.contains(&101) && pids.contains(&200));
    }

    #[test]
    fn excludes_self_and_ancestors_and_init() {
        // init(1) and pq's shell chain (50) and self (60) all "match" via the shared token, but must be
        // excluded; only the unrelated match (70) survives, and matching init must not sweep everything.
        let procs = vec![
            p(1, 0, "init", &["/sbin/init", "job"], 0.0, 0),
            p(50, 1, "bash", &["bash", "job"], 0.0, 0),      // ancestor shell
            p(60, 50, "pq", &["pq", "--kill", "job"], 0.0, 0), // self
            p(70, 1, "x", &["x", "job"], 0.0, 0),            // the real target
        ];
        let t = select(&procs, "job", false, 60);
        let pids: Vec<i32> = t.iter().map(|x| x.pid).collect();
        assert!(!pids.contains(&1) && !pids.contains(&50) && !pids.contains(&60));
        assert!(pids.contains(&70));
    }

    #[test]
    fn empty_pattern_matches_nothing() {
        let procs = vec![p(70, 1, "x", &["x", "run"], 0.0, 0)];
        assert!(select(&procs, "", false, 999).is_empty());
        assert!(select(&procs, "   ", false, 999).is_empty());
    }

    #[test]
    fn ancestor_shell_matching_pattern_does_not_sweep_siblings() {
        // The invoking shell's cmdline contains the pattern you typed, so it "matches"; it must not
        // seed its subtree, or unrelated sibling jobs (pid 61) would be swept in with the real target.
        let procs = vec![
            p(50, 1, "bash", &["bash", "-c", "pq --kill target"], 0.0, 0), // ancestor shell
            p(60, 50, "pq", &["pq", "--kill", "target"], 0.0, 0),          // self
            p(61, 50, "head", &["head", "-4"], 0.0, 0),                    // unrelated sibling
            p(70, 1, "target", &["target", "run"], 0.0, 0),               // the real match
        ];
        let t = select(&procs, "target", false, 60);
        let pids: Vec<i32> = t.iter().map(|x| x.pid).collect();
        assert_eq!(pids, vec![70]);
    }

    #[test]
    fn exact_matches_identity_only() {
        let procs = vec![
            p(10, 1, "java", &["java", "-jar", "/opt/foo-1.2.jar"], 0.0, 0),
            p(11, 1, "vim", &["vim", "foo-1.2.jar.txt"], 0.0, 0),
        ];
        let t = select(&procs, "foo-1.2", true, 999);
        let pids: Vec<i32> = t.iter().map(|x| x.pid).collect();
        assert_eq!(pids, vec![10]);
    }

    fn t(pid: i32) -> Target {
        Target { pid, identity: "x".into(), cmd: "x".into(), uid: 0 }
    }

    struct Mock {
        survive_term: RefCell<HashSet<i32>>,
        eperm: i32,
    }
    impl SignalSender for Mock {
        fn signal(&self, pid: i32, sig: i32) -> bool {
            if pid == self.eperm {
                return false; // EPERM
            }
            if sig == libc::SIGKILL {
                self.survive_term.borrow_mut().remove(&pid);
            }
            true
        }
        fn alive(&self, pid: i32) -> bool {
            self.survive_term.borrow().contains(&pid)
        }
    }

    #[test]
    fn escalates_term_then_kill() {
        let m = Mock {
            survive_term: [201].into_iter().collect::<HashSet<_>>().into(),
            eperm: 203,
        };
        let targets = vec![t(200), t(201), t(203)];
        let k = escalate(&m, &targets, libc::SIGTERM, 0, &|_| {});
        assert!(k.terminated.contains(&200));
        assert!(k.killed.contains(&201)); // survived TERM -> KILLed
        assert!(k.failed.contains(&203)); // EPERM
    }

    #[test]
    fn parse_signal_names_and_numbers() {
        assert_eq!(parse_signal("TERM"), libc::SIGTERM);
        assert_eq!(parse_signal("SIGKILL"), libc::SIGKILL);
        assert_eq!(parse_signal("9"), 9);
        assert_eq!(parse_signal("0"), libc::SIGTERM);
        assert_eq!(parse_signal("bogus"), libc::SIGTERM);
    }

    #[test]
    fn escalate_with_sigkill_first_does_not_wait() {
        let m = Mock {
            survive_term: [201].into_iter().collect::<HashSet<_>>().into(),
            eperm: 203,
        };
        let targets = vec![t(200), t(201), t(203)];
        let slept = RefCell::new(false);
        let k = escalate(&m, &targets, libc::SIGKILL, 4, &|_| { *slept.borrow_mut() = true; });
        assert!(!*slept.borrow(), "sleep should not be invoked when first_sig is SIGKILL");
        assert!(k.terminated.contains(&200));
        assert!(k.terminated.contains(&201)); // signal() already removes it from survive_term for SIGKILL
        assert!(k.failed.contains(&203));
    }
}
