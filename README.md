# qtools - query what's using your machine

`qtools` is a small, growing suite of fast, visual command-line tools for answering "what the hell is
eating my \<resource\>?" on Linux. Each tool is a terse two-letter `_q` ("query") utility, sharing a
common rendering core: donut charts on graphics-capable terminals, colored text bars everywhere else,
JSON for machines. Written in Rust.

* **`dq`** (disk query) - what's using my disk space? A faster, smarter `du`.
* **`pq`** (process query) - what's eating my CPU, memory, or swap? A clustered, visual `top`, with a
  friendlier `pkill` built in.

## Getting started

```
$ ./build.sh && ./install.sh   # build + install dq and pq to ~/.local/bin (make sure it's on PATH)

$ pq                            # what's eating my CPU/memory/swap right now
$ dq                            # what's using disk space in the current directory
```

## dq (disk query)

![dq](./dq.png)

Recurses across a thread pool (10x+ faster than `du`), skips virtual filesystems (`/proc`, `/sys`) and
other-device mounts, sorts by size, formats human-readable by default. Only shows directories using at
least 1% of the tree (`-v`/`-V` to see more). When files sitting directly in a scanned directory add up
to a meaningful share, it breaks that "in this dir" total down into its biggest files. Renders donut
charts on graphics-capable terminals (kitty, Ghostty, iTerm2, WezTerm, Konsole, sixel) or bar charts
elsewhere; `QTOOLS_DEBUG=1` shows what was detected. Colors drop automatically when piped/redirected.

    dq [dir]              # scan dir (default: cwd)
    dq --threads N         # thread count
    dq -v / -V              # verbose / extra verbose (show more/all directories)
    dq --json                # machine-readable output
    dq --noprogress           # skip the progress indicator

Examples:

    dq /tmp
    dq -v --threads 50 /tmp
    dq --json / > sizes.json

## pq (process query)

![pq](./pq.png)

Reads `/proc` (Linux only) and clusters processes by resolved identity, not just executable name: a JVM
running a Gradle daemon groups as `gradle`, not a pile of `java`; Chrome's renderer swarm collapses into
one `chrome (N procs)` (separate profiles/instances are told apart). `-v` expands a cluster to its
member processes.

    pq --cpu / --memory / --swap   # sort/chart metric (default: cpu)
    pq -n N                          # clusters to show (default 15)
    pq -v                             # expand clusters to member processes
    pq --interval MS                   # CPU sample interval (default 400ms)
    pq --json                           # machine-readable output
    pq PATTERN                           # filter report to matching clusters

Per-cluster memory/swap sums each member's RSS/`VmSwap`, which over-counts shared pages (same caveat as
most process viewers).

### pq --kill: a friendlier pkill

Matches the resolved identity, comm, and full command line (case-insensitive) - so `pq --kill gradle`
finds the JVM Gradle daemon that `pkill gradle` misses. Expands each match to its whole process subtree.
Previews the matching tree and confirms before acting. Escalates: SIGTERM, wait `--grace` seconds
(default 4), then SIGKILL survivors. Never signals pq itself, your shell/its ancestors, pid 1, or
unrelated sibling jobs.

    pq --kill PATTERN [--dry-run] [-y/--yes] [-x/--exact] [--signal SIG] [--grace SECONDS]
