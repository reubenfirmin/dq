# qtools - query what's using your machine

## About

`qtools` is a small, growing suite of fast, visual command-line tools for answering "what the hell is
eating my \<resource\>?" on Linux. Each tool is a terse two-letter `_q` ("query") utility, and they
share a common rendering core: real donut charts on graphics-capable terminals, colored text bars
everywhere else, and JSON for machines. All written in Rust.

The tools:
* **`dq`** (disk query) - what's using my disk space? A faster, smarter `du`.
* **`pq`** (process query) - what's eating my CPU, memory, or swap right now? A clustered, visual take on `top`, with a friendlier `pkill` built in.

More `_q` query tools will land in the same repo, sharing the same core.

## dq (disk query)

`dq` is an improved, faster alternative to `du`, intended to quickly answer the question that most of us
actually use `du` for: "what the hell is using all my disk space?"

It contains the following improvements:
* It's much faster (recurses the tree across a thread pool)
* It skips virtual filesystems like /proc and /sys
* It (always, and by default) skips paths mounted on other devices from the starting path (du has this option, but it's not a well known param)
* It formats numbers as human readable by default
* It sorts the output by file size, high to low, even when formatting them to be human readable
* It visualizes results: rendered donut charts on graphics-capable terminals (kitty, Ghostty, iTerm2, sixel), and a scaled per-directory bar chart everywhere else
* When files sitting directly in the scanned directory add up to a meaningful share of the tree, it breaks that "in this dir" total down into its biggest files
* It (by default) only returns results for directories using at least 1% of all files under the path
* It colors output when writing to a terminal and drops the colors automatically when piped or redirected
* It won't complain about directories that it doesn't have permission to access; for example, running against / as a regular user will work, but the size will be less than if you run with sudo
* It can emit JSON (`--json`) for machine consumption

### Demo

![demo](./demo.gif)

### Running & Options

    dq dir

* dir - directory to scan (defaults to the current directory)

Optimization:
* --threads - how many threads to execute when recursing the tree (best option is likely a function of number of cores)

Formatting:
* -v - verbose; list every non-empty directory, not just those over 1%
* -V - extra verbose; also include directories that are 0 bytes
* --json - emit machine-readable JSON instead of the human report
* --noprogress - don't draw the progress indicator

In human mode, `dq` colors sizes and bars by magnitude and truncates long paths to the terminal width
so rows never wrap. Colors are drawn only when stdout is a terminal, so piping or redirecting yields
plain text automatically (no flag needed). For machine consumption, use `--json`.

The header shows the scanned path, the recursive `total`, and how much of that is in files sitting
directly `in this dir` (not in any subdirectory). When that direct total is at least 1% of the tree,
an `in this dir:` section lists the biggest of those files (top 12 by default; `-v` lists them all).
This is what catches cases like a directory whose size is dominated by a few large files rather than
subdirectories. The same data is available under `in_this_dir` in `--json` output.

On terminals that support a raster graphics protocol (kitty, Ghostty, iTerm2, WezTerm, Konsole, or
sixel terminals like foot), dq draws real **donut charts** via [viuer](https://crates.io/crates/viuer):
one for the big folders and one for the `in this dir:` files (each top item a colored arc, the
remainder folded into an "other" arc), each with a color-keyed legend. The layout is responsive: the
two donuts sit **side by side** when the terminal is wide enough, and **stack** otherwise. Capability
detection runs a protocol probe with a short timeout (so a non-answering terminal can't hang it), and
everything degrades to the plain text bars/rows above when a protocol isn't detected, when
piped/redirected, or with `--json`. Set `DQ_DEBUG=1` to see what was detected on stderr.

While a scan runs, `dq` draws a live progress indicator (spinner, running directory count, and bytes
seen) on stderr, so it never pollutes stdout and stays out of the way when you pipe the output. Because
the total isn't known until the scan finishes, the percentage is an approximation that reaches 100% on
completion. It's shown only when stderr is a terminal, and can be turned off with `--noprogress`.

Examples:

`dq /tmp`

`dq -v --threads 50 /tmp`

`dq --json / > sizes.json`

`dq / | less`  (colors are dropped automatically when the output isn't a terminal)

## pq (process query)

`pq` answers "what's eating my CPU, memory, or swap right now?" with the same treatment `dq` gives disk:
fast, clustered, visual (donut with a text fallback), and JSON for machines. It reads `/proc`, so it's
Linux-only.

Its headline trick is **identity-aware clustering**. `java`, `python`, `node`, `electron`, and `sh` are
runtimes, not applications, so `pq` digs into the command line to find the real identity: a JVM running a
Gradle daemon (and its worker JVMs) shows up as one `gradle` cluster, not a pile of `java`; Chrome's
browser process plus its dozens of renderers collapse into one `chrome (N procs)`. Separate Chromium
instances are told apart by their profile, so an automation browser reads as `chrome (some-profile)`
rather than merging into your main one. `-v` expands a cluster to its member processes.

Metrics (the active one drives the sort, the donut, and the cluster weighting; all columns are always shown):
* --cpu - sort/chart by CPU (default); measured as a live delta over a short interval (100% = one core)
* --memory - sort/chart by resident memory (RSS)
* --swap - sort/chart by swap used, i.e. what's actually paged out (`VmSwap`)

The header shows total CPU across cores and system memory `used / total` (and swap `used / total` when the
system has swap). On a graphics terminal (kitty, Ghostty, iTerm2, sixel, ...) the top clusters render as a
donut; otherwise, or when piped or `--json`, you get text bars. In `--cpu` mode the donut is a set of
concentric per-core rings, each core's ring colored by which process ran on it, so you can see how work
spreads across cores (a ring arc is a share of *one* core, while the legend `%` is a share of the *whole*
machine).

### pq --kill: a friendlier pkill

`pq --kill <pattern>` fixes the everyday `pkill` frustrations:
* It matches the **resolved identity, the comm, and the full command line** (case-insensitive), so
  `pq --kill gradle` finds the JVM Gradle daemon that `pkill gradle` misses (its `comm` is just `java`).
* It **expands each match to its whole process subtree**, so worker/child processes go too, not orphaned.
* It **previews** the matching process tree (pid, user, cpu%, mem, command) and asks to **confirm** before
  doing anything (`--dry-run` to only look, `-y` to skip the prompt).
* It **escalates**: SIGTERM, wait `--grace` seconds (default 4), then SIGKILL the survivors.
* It **never signals** pq itself, your shell / its ancestor chain, pid 1, or unrelated sibling jobs (so
  typing the pattern into your shell doesn't sweep in your other background jobs).

pq options:
* -n, --top N - clusters to show (default 15)
* -v, --verbose - expand clusters to member processes
* --interval MS - CPU sample interval (default 400)
* --json - machine-readable JSON
* --kill PATTERN - kill matching process trees, with --dry-run, -y/--yes, -x/--exact, --signal, --grace
* a bare PATTERN filters the report to matching clusters

Per-cluster memory and swap sum each member's RSS/`VmSwap`, which over-counts shared pages (the same
caveat as most process viewers).

## Status

Beta.

`dq` works, and with its skipping optimizations it can be at least 10x as fast as `du`. `pq` works and is
in daily use.

The formatting, chart, clustering, and JSON logic has unit and integration tests (`cargo test`), but the
disk scan itself is not yet proven correct against `du` (see below), and neither tool has been broadly
battle-tested across systems.

TODOs:

Possible bug: need to understand what `du` does differently; `dq` reports numbers consistently a bit higher than `du`'s;
manual checks of directories that I've done with `ls` also agree with `dq`. Is `du` wrong or just doing something subtly
different? TBD.

Improvement: option to roll up summaries by common directory.

Possible feature: an interactive mode (hover/drill-down over the treemap/donuts).

## Building

```
./build.sh      # builds ./dq and ./pq via `cargo build --release`
./install.sh    # copies ./dq and ./pq to ~/.local/bin
```

`install.sh` installs into `~/.local/bin` (no sudo required). Make sure that directory is on your `PATH`.
