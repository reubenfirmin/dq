# dq - find what's eating your disk space

## About

`dq` ("disk quantify") is an improved, faster alternative to `du`, written in Rust. It's intended to quickly
answer the question that most of us actually use `du` for: "what the hell is using all my disk space?"

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

## Demo

![demo](./demo.gif)

## Running & Options

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

## Status

Beta.

It works, and with its skipping optimizations it can be at least 10x as fast as `du`.

The formatting, chart, and JSON logic has unit and integration tests (`cargo test`), but the scan itself is not yet
proven correct against `du` (see below), and it hasn't been broadly battle-tested across filesystems.

TODOs:

Possible bug: need to understand what `du` does differently; `dq` reports numbers consistently a bit higher than `du`'s;
manual checks of directories that I've done with `ls` also agree with `dq`. Is `du` wrong or just doing something subtly
different? TBD.

Improvement: option to roll up summaries by common directory.

Possible feature: an interactive mode (hover/drill-down over the treemap/donuts).

## Building

```
./build.sh      # produces ./dq via `cargo build --release`
./install.sh    # copies ./dq to ~/.local/bin
```

`install.sh` installs into `~/.local/bin` (no sudo required). Make sure that directory is on your `PATH`.
