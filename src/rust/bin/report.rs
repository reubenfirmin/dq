//! Text + JSON reporting for pq: a header with system-wide cpu/mem totals, then one row per cluster
//! (largest first), optionally expanded to member processes with `-v`. Built on the shared `chart`
//! module so the bar-scaling/palette logic matches dq.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use qtools::style::{self, bold, dim, human_plain, paint};
use qtools::{chart, donut, graphics};

use crate::cluster::{Cluster, Metric};

/// Rendering knobs shared by `write_text`/`write_json`, mirroring dq's `FormatOptions`.
pub struct ReportOpts {
    pub metric: Metric,
    pub top: usize,
    pub verbose: bool,
    pub colors: bool,
    pub width: Option<usize>
}

const CPU_WIDTH: usize = 6;
const MEM_WIDTH: usize = 8;
const SWAP_WIDTH: usize = 8;
const COUNT_WIDTH: usize = 4;
const BAR_WIDTH: usize = 20;
const RULE_WIDTH: usize = 52;
// Fixed part of a row before the label: cpu + "  " + mem + "  " + count + "  " + bar + "  ".
const ROW_PREFIX: usize = CPU_WIDTH + 2 + MEM_WIDTH + 2 + COUNT_WIDTH + 2 + BAR_WIDTH + 2;
// Same, but with the swap column inserted right after mem: + "  " + swap.
const ROW_PREFIX_SWAP: usize = ROW_PREFIX + SWAP_WIDTH + 2;
// Graphics legend row fixed part: swatch(2) + 2 + cpu + 2 + mem + 2 + count + 2 before the label.
const LEGEND_PREFIX: usize = 2 + 2 + CPU_WIDTH + 2 + MEM_WIDTH + 2 + COUNT_WIDTH + 2;
// Two-tier legend row fixed part: swatch(2) + 2 + mem + 2 + swap + 2 + count + 2 before the label
// (no cpu column: a two-tier donut is always memory outer / swap inner).
const LEGEND_PREFIX_TWO_TIER: usize = 2 + 2 + MEM_WIDTH + 2 + SWAP_WIDTH + 2 + COUNT_WIDTH + 2;
const MIN_LABEL_WIDTH: usize = 12;
// Square pixel canvas the donut is rasterized into (viuer scales it to the cell box).
const DONUT_PX: u32 = 600;
// Idle/free capacity arc: darker than `style::OTHER_COLOR` (used-by-unshown-processes) so the two
// gray remainders read as visually distinct in both the ring and the legend swatches.
const UNUSED_COLOR: (u8, u8, u8) = (0x2f, 0x2f, 0x36);

/// The active-metric value for a cluster (what sorts and sizes the bar).
fn active_value(c: &Cluster, metric: Metric) -> f64 {
    match metric {
        Metric::Cpu => c.cpu,
        Metric::Memory => c.rss as f64,
        Metric::Swap => c.swap as f64
    }
}

/// Header line shared by every report mode: `pq | cpu NN% | mem used / total [| swap used / total]`,
/// then a rule. The swap segment is only appended when the system has swap configured at all
/// (`swap_total > 0`); a swapless box never shows it.
fn write_header<W: Write>(out: &mut W, mem_used: u64, mem_total: u64, swap_used: u64, swap_total: u64, cpu_total: f64, colors: bool) -> io::Result<()> {
    let sep = dim("\u{2502}", colors);
    write!(
        out,
        "{}  {}  cpu {}  {}  mem {} / {}",
        bold("pq", colors),
        sep,
        paint(&format!("{:.0}%", cpu_total), style::magnitude_color((cpu_total.max(0.0)) as u64 * 1_000_000), cpu_total >= 100.0, colors),
        dim("\u{2502}", colors),
        human_plain(mem_used),
        human_plain(mem_total)
    )?;
    if swap_total > 0 {
        write!(out, "  {}  swap {} / {}", dim("\u{2502}", colors), human_plain(swap_used), human_plain(swap_total))?;
    }
    writeln!(out)?;
    writeln!(out, "{}", dim(&"-".repeat(RULE_WIDTH), colors))
}

/// Human text report: header line with cpu/mem/swap totals, a rule, then top-N cluster rows
/// (largest bar-scaled by the top cluster's active-metric value), with `-v` expanding members
/// indented. The swap column only appears when swap is actually in use (`swap_used > 0`), so
/// swapless or idle-swap systems stay clean.
pub fn write_text<W: Write>(out: &mut W, clusters: &[Cluster], mem_used: u64, mem_total: u64, swap_used: u64, swap_total: u64, cpu_total: f64, opts: &ReportOpts) -> io::Result<()> {
    let colors = opts.colors;
    write_header(out, mem_used, mem_total, swap_used, swap_total, cpu_total, colors)?;
    write_body(out, clusters, swap_used, opts)
}

/// The cluster rows beneath the header/rule: top-N rows (largest bar-scaled by the top cluster's
/// active-metric value), with `-v` expanding members indented. Shared by `write_text` and by
/// `write_graphics`'s text fallback, so both paths write the header exactly once.
fn write_body<W: Write>(out: &mut W, clusters: &[Cluster], swap_used: u64, opts: &ReportOpts) -> io::Result<()> {
    let colors = opts.colors;
    let show_swap = swap_used > 0;
    let row_prefix = if show_swap { ROW_PREFIX_SWAP } else { ROW_PREFIX };

    let top: Vec<&Cluster> = clusters.iter().take(opts.top).collect();
    let largest = top.iter().map(|c| active_value(c, opts.metric)).fold(0.0_f64, f64::max);

    // Column headings, aligned with the data columns below.
    let heading = if show_swap {
        format!("{:>cw$}  {:>mw$}  {:>sw$}  {:>ctw$}  {}  process",
            "cpu", "mem", "swap", "#", " ".repeat(BAR_WIDTH),
            cw = CPU_WIDTH, mw = MEM_WIDTH, sw = SWAP_WIDTH, ctw = COUNT_WIDTH)
    } else {
        format!("{:>cw$}  {:>mw$}  {:>ctw$}  {}  process",
            "cpu", "mem", "#", " ".repeat(BAR_WIDTH),
            cw = CPU_WIDTH, mw = MEM_WIDTH, ctw = COUNT_WIDTH)
    };
    writeln!(out, "{}", dim(&heading, colors))?;

    for c in &top {
        write_cluster_row(out, c, largest, opts, show_swap)?;
        if opts.verbose && c.members.len() > 1 {
            let mut members = c.members.iter().collect::<Vec<_>>();
            members.sort_by(|a, b| match opts.metric {
                Metric::Cpu => b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal),
                Metric::Memory => b.rss.cmp(&a.rss),
                Metric::Swap => b.swap.cmp(&a.swap)
            });
            for m in members {
                let label = format!("pid {}  {}", m.pid, m.cmd);
                let label = match opts.width {
                    Some(w) => style::truncate_middle(&label, w.saturating_sub(row_prefix + 2).max(MIN_LABEL_WIDTH)),
                    None => label
                };
                if show_swap {
                    writeln!(
                        out,
                        "  {:>cw$.0}%  {:>mw$}  {:>sw$}  {:>ctw$}  {}",
                        m.cpu, human_plain(m.rss), human_plain(m.swap), "", dim(&label, colors),
                        cw = CPU_WIDTH - 1, mw = MEM_WIDTH, sw = SWAP_WIDTH, ctw = COUNT_WIDTH
                    )?;
                } else {
                    writeln!(
                        out,
                        "  {:>cw$.0}%  {:>mw$}  {:>ctw$}  {}",
                        m.cpu, human_plain(m.rss), "", dim(&label, colors),
                        cw = CPU_WIDTH - 1, mw = MEM_WIDTH, ctw = COUNT_WIDTH
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn write_cluster_row<W: Write>(out: &mut W, c: &Cluster, largest: f64, opts: &ReportOpts, show_swap: bool) -> io::Result<()> {
    let colors = opts.colors;
    let value = active_value(c, opts.metric);
    let bar_frac = if largest > 0.0 { value / largest } else { 0.0 };
    let filled = ((bar_frac * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);

    let row_prefix = if show_swap { ROW_PREFIX_SWAP } else { ROW_PREFIX };
    let cpu_cell = format!("{:>w$.0}%", c.cpu, w = CPU_WIDTH - 1);
    let mem_cell = format!("{:>w$}", human_plain(c.rss), w = MEM_WIDTH);
    let count_cell = format!("{:>w$}", c.members.len(), w = COUNT_WIDTH);
    let bar_filled = paint(&"#".repeat(filled), style::magnitude_color(1_000_000), false, colors);
    let bar_empty = dim(&".".repeat(BAR_WIDTH - filled), colors);

    let label = match opts.width {
        Some(w) => style::truncate_middle(&c.identity, w.saturating_sub(row_prefix).max(MIN_LABEL_WIDTH)),
        None => c.identity.clone()
    };
    let label = if c.members.len() > 1 { format!("{} ({})", label, plural(c.members.len())) } else { label };

    if show_swap {
        let swap_cell = format!("{:>w$}", human_plain(c.swap), w = SWAP_WIDTH);
        writeln!(out, "{}  {}  {}  {}  {}{}  {}", cpu_cell, mem_cell, swap_cell, count_cell, bar_filled, bar_empty, label)
    } else {
        writeln!(out, "{}  {}  {}  {}{}  {}", cpu_cell, mem_cell, count_cell, bar_filled, bar_empty, label)
    }
}

fn plural(n: usize) -> String {
    format!("{} procs", n)
}

// -------------------------------------------------------------------------------------------------
// Graphics: a donut of the top clusters by the active metric (a two-tier memory/swap donut when
// swap is configured and the metric is memory-family, otherwise a single three-part ring), plus a
// color-keyed legend.
// -------------------------------------------------------------------------------------------------

/// The active-metric value for a cluster, scaled to an integer chart weight. CPU is a percentage
/// (e.g. 12.34); scaling by 100 keeps two decimal places of precision as an integer. Memory and
/// swap are already bytes, so they pass through unchanged.
fn metric_value(c: &Cluster, metric: Metric) -> u64 {
    match metric {
        Metric::Cpu => (c.cpu.max(0.0) * 100.0).round() as u64,
        Metric::Memory => c.rss,
        Metric::Swap => c.swap
    }
}

/// The system-wide "used" amount a ring's colored top-cluster values are drawn against, in the same
/// integer scale as `metric_value` (cpu in hundredths of a percent, memory/swap in bytes).
fn metric_used(cpu_total: f64, mem_used: u64, swap_used: u64, metric: Metric) -> u64 {
    match metric {
        Metric::Cpu => (cpu_total.max(0.0) * 100.0).round() as u64,
        Metric::Memory => mem_used,
        Metric::Swap => swap_used
    }
}

/// The ring's total (its 100%, i.e. capacity): cpu is a flat 100.00% (10000 in the integer scale);
/// memory and swap use the system totals. Colored slices plus "other" plus "unused" always sum to
/// this, so the ring accounts for idle/free capacity as well as what's used.
fn metric_capacity(mem_total: u64, swap_total: u64, metric: Metric) -> u64 {
    match metric {
        Metric::Cpu => 100 * 100,
        Metric::Memory => mem_total,
        Metric::Swap => swap_total
    }
}

/// The clusters to color individually: the front of the (already-sorted) list, capped at `top` and
/// at the palette size, so rank always maps to `style::PALETTE[rank]`. Compute this once and reuse
/// it (via `colored_by`) across both rings of a two-tier donut, so the same cluster gets the same
/// color in each.
fn top_clusters(clusters: &[Cluster], top: usize) -> Vec<&Cluster> {
    clusters.iter().take(top.min(style::PALETTE.len())).collect()
}

/// Pair each of `top` with a value (via `value_fn`) and its rank color.
fn colored_by(top: &[&Cluster], value_fn: impl Fn(&Cluster) -> u64) -> Vec<(String, u64, (u8, u8, u8))> {
    top.iter().enumerate().map(|(i, c)| (c.identity.clone(), value_fn(c), style::PALETTE[i])).collect()
}

/// Build a ring's three-part segments so they always sum to `capacity` (the ring's 100%): the
/// already-colored top clusters, then a gray "other" arc for capacity used by processes not shown
/// (`used - sum(colored)`, colored `style::OTHER_COLOR`), then a dark "unused" arc for idle/free
/// capacity (`capacity - max(sum(colored), used)`, colored `UNUSED_COLOR`). Both remainder arcs
/// saturate at 0, so independently-rounded inputs (e.g. per-cluster cpu% vs. the system cpu total)
/// can never drive a slice negative.
fn ring_segments(colored: &[(String, u64, (u8, u8, u8))], used: u64, capacity: u64) -> Vec<chart::Segment> {
    let mut out: Vec<chart::Segment> = colored.iter()
        .map(|(label, value, color)| chart::Segment { label: label.clone(), value: *value, color: *color })
        .collect();
    let sum: u64 = colored.iter().map(|(_, v, _)| *v).sum();
    out.push(chart::Segment { label: "other".to_string(), value: used.saturating_sub(sum), color: style::OTHER_COLOR });
    out.push(chart::Segment { label: "unused".to_string(), value: capacity.saturating_sub(sum.max(used)), color: UNUSED_COLOR });
    out
}

/// Map every process's pid to the color it should be drawn with in the per-core CPU rings, matching
/// the legend: pids belonging to one of the top clusters (same rank/limit `top_clusters` uses) get
/// that cluster's palette color, and every other pid gets `style::OTHER_COLOR`.
fn pid_colors(clusters: &[Cluster], top_len: usize) -> HashMap<i32, (u8, u8, u8)> {
    let mut map = HashMap::new();
    for (i, c) in clusters.iter().enumerate() {
        let color = if i < top_len { style::PALETTE[i] } else { style::OTHER_COLOR };
        for m in &c.members {
            map.insert(m.pid, color);
        }
    }
    map
}

/// Build one ring's process-colored slices for core `k`: the per-pid attribution aggregated by
/// color (in hundredths of that core's percent), then an "other" slice for busy time the
/// attribution didn't account for, then an "idle" slice for the rest of the core's capacity. Both
/// remainder slices saturate at 0, same rationale as `ring_segments`: independently-measured
/// quantities (the real `/proc/stat` busy% vs. the approximated per-thread attribution) can
/// disagree slightly.
fn core_ring_slices(attrib: &[(i32, f64)], busy_pct: f64, colors: &HashMap<i32, (u8, u8, u8)>) -> Vec<donut::Slice> {
    let mut by_color: HashMap<(u8, u8, u8), f64> = HashMap::new();
    for &(pid, cpu) in attrib {
        let color = colors.get(&pid).copied().unwrap_or(style::OTHER_COLOR);
        *by_color.entry(color).or_insert(0.0) += cpu;
    }

    // Emit the palette colors in rank order (same as the legend), so a given cluster sits at the
    // same angle on every ring and a process's use across cores reads as an aligned color band.
    let mut slices: Vec<donut::Slice> = Vec::new();
    let mut palette_sum = 0.0_f64;
    for &color in style::PALETTE.iter() {
        if let Some(&cpu) = by_color.get(&color) {
            if cpu > 0.0 {
                slices.push(((cpu * 100.0).round() as u64, color));
                palette_sum += cpu;
            }
        }
    }

    // Everything else on this core (processes not in a top cluster, plus busy time the per-thread
    // attribution missed) collapses into one "other" arc; idle fills the rest to the core's 100%.
    let other = ((busy_pct - palette_sum).max(0.0) * 100.0).round() as u64;
    slices.push((other, style::OTHER_COLOR));
    let idle = ((100.0 - busy_pct).max(0.0) * 100.0).round() as u64;
    slices.push((idle, UNUSED_COLOR));
    slices
}

/// Build the per-core rings for the `--cpu` donut: process-colored slices when attribution data is
/// available, or the old 2-slice heat/idle rings as a fallback (e.g. attribution came back empty
/// because every process was idle, or this run predates any samples).
fn build_cpu_rings(core_loads: &[f64], attribution: &[Vec<(i32, f64)>], clusters: &[Cluster], top_len: usize) -> Vec<Vec<donut::Slice>> {
    let has_attribution = attribution.iter().any(|core| !core.is_empty());
    if !has_attribution {
        return core_loads.iter().map(|&load| {
            let clamped = load.clamp(0.0, 100.0);
            let busy = (clamped * 100.0).round() as u64;
            let idle = 10_000u64.saturating_sub(busy);
            vec![(busy, donut::heat(clamped)), (idle, UNUSED_COLOR)]
        }).collect();
    }

    let colors = pid_colors(clusters, top_len);
    core_loads.iter().enumerate().map(|(k, &busy)| {
        let attrib: &[(i32, f64)] = attribution.get(k).map(|v| v.as_slice()).unwrap_or(&[]);
        core_ring_slices(attrib, busy.clamp(0.0, 100.0), &colors)
    }).collect()
}

/// Draw the donut, then a legend beneath it. Three shapes, depending on the active metric: a ring
/// per CPU core for `Metric::Cpu` (falling back to the single three-part ring if `core_loads` is
/// somehow empty), a two-tier memory/swap ring for the memory-family metrics when swap is
/// configured, or a single three-part ring otherwise. Self-contained: it writes the header exactly
/// once and falls back internally to the text body (never re-writing the header) whenever there's
/// nothing to chart or the image fails to render.
#[allow(clippy::too_many_arguments)]
pub fn write_graphics<W: Write>(out: &mut W, clusters: &[Cluster], mem_used: u64, mem_total: u64, swap_used: u64, swap_total: u64, cpu_total: f64, core_loads: &[f64], attribution: &[Vec<(i32, f64)>], opts: &ReportOpts) -> io::Result<()> {
    let colors = opts.colors;
    write_header(out, mem_used, mem_total, swap_used, swap_total, cpu_total, colors)?;

    let top = top_clusters(clusters, opts.top);
    let cols = opts.width.unwrap_or(80).clamp(16, 40) as u32;
    let config = viuer::Config {
        absolute_offset: false,
        width: Some(cols),
        height: Some((cols / 2).max(8)),
        ..Default::default()
    };

    // One ring per CPU core for the cpu metric (as long as we actually have per-core samples; on a
    // platform quirk where we don't, fall through to the single three-part ring below instead).
    if opts.metric == Metric::Cpu && !core_loads.is_empty() {
        // Flush before viuer::print: it writes the image straight to the terminal, bypassing
        // `out`'s buffer, so anything still buffered here would land BELOW the donut instead of
        // above it.
        out.flush()?;
        let rings = build_cpu_rings(core_loads, attribution, clusters, top.len());
        let image = donut::build_core_rings_image(&rings, DONUT_PX);
        if let Err(e) = viuer::print(&image, &config) {
            graphics::debug_print_failure(&e);
            return write_body(out, clusters, swap_used, opts);
        }
        // Still show the process-cluster legend beneath the per-core rings (cpu%/mem/count/identity
        // + other/unused), so it's clear which apps are driving the load.
        let capacity = metric_capacity(mem_total, swap_total, opts.metric);
        let used = metric_used(cpu_total, mem_used, swap_used, opts.metric);
        let colored = colored_by(&top, |c| metric_value(c, opts.metric));
        let segs = ring_segments(&colored, used, capacity);
        return write_legend(out, &segs, clusters, opts);
    }

    // Two-tier (memory outer, swap inner) only makes sense for the memory-family metrics, and only
    // when the system actually has swap configured; otherwise a single three-part ring for the
    // active metric is drawn instead.
    if matches!(opts.metric, Metric::Memory | Metric::Swap) && swap_total > 0 {
        if mem_total == 0 {
            return write_body(out, clusters, swap_used, opts);
        }
        let outer_colored = colored_by(&top, |c| metric_value(c, Metric::Memory));
        let inner_colored = colored_by(&top, |c| metric_value(c, Metric::Swap));
        let outer_segs = ring_segments(&outer_colored, mem_used, mem_total);
        let inner_segs = ring_segments(&inner_colored, swap_used, swap_total);

        out.flush()?;
        let image = donut::build_two_ring_image(&chart::slices(&outer_segs), &chart::slices(&inner_segs), DONUT_PX);
        if let Err(e) = viuer::print(&image, &config) {
            graphics::debug_print_failure(&e);
            return write_body(out, clusters, swap_used, opts);
        }
        write_legend_two_tier(out, &top, clusters, &outer_segs, &inner_segs, opts)
    } else {
        let capacity = metric_capacity(mem_total, swap_total, opts.metric);
        if capacity == 0 {
            return write_body(out, clusters, swap_used, opts);
        }
        let used = metric_used(cpu_total, mem_used, swap_used, opts.metric);
        let colored = colored_by(&top, |c| metric_value(c, opts.metric));
        let segs = ring_segments(&colored, used, capacity);

        out.flush()?;
        let image = donut::build_donut_image(&chart::slices(&segs), DONUT_PX);
        if let Err(e) = viuer::print(&image, &config) {
            graphics::debug_print_failure(&e);
            return write_body(out, clusters, swap_used, opts);
        }
        write_legend(out, &segs, clusters, opts)
    }
}

/// The legend beneath a single-ring donut: one row per segment with its swatch, cpu%, mem, member
/// count, and (truncated) identity. The "other" segment's cpu/mem/count are the sum over every
/// cluster not kept as its own slice; the "unused" segment shows the idle/free magnitude in the
/// active-metric column, so together the rows account for the whole ring.
fn write_legend<W: Write>(out: &mut W, segs: &[chart::Segment], clusters: &[Cluster], opts: &ReportOpts) -> io::Result<()> {
    let colors = opts.colors;
    let by_identity: HashMap<&str, &Cluster> = clusters.iter().map(|c| (c.identity.as_str(), c)).collect();
    let kept: HashSet<&str> = segs.iter().filter(|s| s.label != "other" && s.label != "unused").map(|s| s.label.as_str()).collect();

    // Column headings, aligned with the legend columns below (the swatch occupies 2 cells).
    let heading = format!("{}  {:>cw$}  {:>mw$}  {:>ctw$}  process",
        "  ", "cpu", "mem", "#",
        cw = CPU_WIDTH, mw = MEM_WIDTH, ctw = COUNT_WIDTH);
    writeln!(out, "{}", dim(&heading, colors))?;

    for seg in segs {
        let (mut cpu, mut rss, count) = if seg.label == "other" {
            clusters.iter().filter(|c| !kept.contains(c.identity.as_str()))
                .fold((0.0, 0u64, 0usize), |(cpu, rss, count), c| (cpu + c.cpu, rss + c.rss, count + c.members.len()))
        } else {
            match by_identity.get(seg.label.as_str()) {
                Some(c) => (c.cpu, c.rss, c.members.len()),
                None => (0.0, 0, 0)
            }
        };

        // The "other" arc is used-by-unshown-processes and the "unused" arc is idle/free capacity;
        // show each magnitude in the active-metric column rather than leave it as the (irrelevant
        // or zero) real cluster data, which would badly under-report a large arc.
        if seg.label == "other" || seg.label == "unused" {
            match opts.metric {
                Metric::Memory => rss = seg.value,
                Metric::Cpu => cpu = seg.value as f64 / 100.0,
                Metric::Swap => {}
            }
        }

        let label = match opts.width {
            Some(w) => style::truncate_middle(&seg.label, w.saturating_sub(LEGEND_PREFIX).max(MIN_LABEL_WIDTH)),
            None => seg.label.clone()
        };

        writeln!(
            out,
            "{}  {:>cw$.0}%  {:>mw$}  {:>ctw$}  {}",
            style::color_swatch(seg.color, colors),
            cpu, human_plain(rss), count, dim(&label, colors),
            cw = CPU_WIDTH - 1, mw = MEM_WIDTH, ctw = COUNT_WIDTH
        )?;
    }
    Ok(())
}

/// The legend beneath a two-tier (memory outer, swap inner) donut: one row per colored cluster with
/// its rss, swap, and member count, then an "other" row (used by processes not shown) and an
/// "unused" row (idle/free capacity in both memory and swap), reading the magnitudes straight off
/// the already-computed ring segments so the legend always matches what's drawn.
fn write_legend_two_tier<W: Write>(out: &mut W, top: &[&Cluster], clusters: &[Cluster], outer: &[chart::Segment], inner: &[chart::Segment], opts: &ReportOpts) -> io::Result<()> {
    let colors = opts.colors;

    let heading = format!("{}  {:>mw$}  {:>sw$}  {:>ctw$}  process",
        "  ", "mem", "swap", "#",
        mw = MEM_WIDTH, sw = SWAP_WIDTH, ctw = COUNT_WIDTH);
    writeln!(out, "{}", dim(&heading, colors))?;

    for (i, c) in top.iter().enumerate() {
        write_two_tier_row(out, outer[i].color, outer[i].value, inner[i].value, Some(c.members.len()), &c.identity, opts)?;
    }

    // "other" is used by clusters not kept as their own colored slice; its count is the real number
    // of member processes folded in there (matching the single-ring legend's "other" semantics).
    let kept: HashSet<&str> = top.iter().map(|c| c.identity.as_str()).collect();
    let other_count: usize = clusters.iter().filter(|c| !kept.contains(c.identity.as_str())).map(|c| c.members.len()).sum();
    let other_idx = top.len();
    write_two_tier_row(out, outer[other_idx].color, outer[other_idx].value, inner[other_idx].value, Some(other_count), "other", opts)?;

    let unused_idx = top.len() + 1;
    write_two_tier_row(out, outer[unused_idx].color, outer[unused_idx].value, inner[unused_idx].value, None, "unused", opts)
}

/// One two-tier legend row: swatch, mem, swap, count (blank for "unused", which isn't processes),
/// identity.
fn write_two_tier_row<W: Write>(out: &mut W, color: (u8, u8, u8), rss: u64, swap: u64, count: Option<usize>, identity: &str, opts: &ReportOpts) -> io::Result<()> {
    let colors = opts.colors;
    let label = match opts.width {
        Some(w) => style::truncate_middle(identity, w.saturating_sub(LEGEND_PREFIX_TWO_TIER).max(MIN_LABEL_WIDTH)),
        None => identity.to_string()
    };
    let count_cell = count.map(|n| n.to_string()).unwrap_or_default();
    writeln!(
        out,
        "{}  {:>mw$}  {:>sw$}  {:>ctw$}  {}",
        style::color_swatch(color, colors),
        human_plain(rss), human_plain(swap), count_cell, dim(&label, colors),
        mw = MEM_WIDTH, sw = SWAP_WIDTH, ctw = COUNT_WIDTH
    )
}

/// Machine-readable JSON: `{ cpu_total, mem_used, mem_total, swap_used, swap_total, clusters: [...] }`,
/// top-N clusters, each with its members (pid, cpu, mem, swap, cmd).
pub fn write_json<W: Write>(out: &mut W, clusters: &[Cluster], mem_used: u64, mem_total: u64, swap_used: u64, swap_total: u64, cpu_total: f64, opts: &ReportOpts) -> io::Result<()> {
    writeln!(out, "{{")?;
    writeln!(out, "  \"cpu_total\": {},", round2(cpu_total))?;
    writeln!(out, "  \"mem_used\": {},", mem_used)?;
    writeln!(out, "  \"mem_total\": {},", mem_total)?;
    writeln!(out, "  \"swap_used\": {},", swap_used)?;
    writeln!(out, "  \"swap_total\": {},", swap_total)?;
    writeln!(out, "  \"clusters\": [")?;

    let top: Vec<&Cluster> = clusters.iter().take(opts.top).collect();
    for (i, c) in top.iter().enumerate() {
        writeln!(out, "    {{")?;
        writeln!(out, "      \"identity\": \"{}\",", style::json_escape(&c.identity))?;
        writeln!(out, "      \"cpu\": {},", round2(c.cpu))?;
        writeln!(out, "      \"mem\": {},", c.rss)?;
        writeln!(out, "      \"swap\": {},", c.swap)?;
        writeln!(out, "      \"count\": {},", c.members.len())?;
        writeln!(out, "      \"members\": [")?;
        for (j, m) in c.members.iter().enumerate() {
            write!(
                out,
                "        {{\"pid\": {}, \"cpu\": {}, \"mem\": {}, \"swap\": {}, \"cmd\": \"{}\"}}",
                m.pid, round2(m.cpu), m.rss, m.swap, style::json_escape(&m.cmd)
            )?;
            writeln!(out, "{}", if j + 1 < c.members.len() { "," } else { "" })?;
        }
        writeln!(out, "      ]")?;
        write!(out, "    }}")?;
        writeln!(out, "{}", if i + 1 < top.len() { "," } else { "" })?;
    }

    writeln!(out, "  ]")?;
    writeln!(out, "}}")?;
    Ok(())
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::Member;

    fn opts() -> ReportOpts {
        ReportOpts { metric: Metric::Cpu, top: 15, verbose: false, colors: false, width: None }
    }

    #[test]
    fn metric_value_scales_cpu_and_passes_through_memory_and_swap() {
        let c = Cluster { identity: "a".into(), cpu: 12.34, rss: 5_000, swap: 900, members: vec![] };
        assert_eq!(metric_value(&c, Metric::Cpu), 1234);
        assert_eq!(metric_value(&c, Metric::Memory), 5_000);
        assert_eq!(metric_value(&c, Metric::Swap), 900);
    }

    #[test]
    fn metric_capacity_and_used_scale_cpu_and_pass_through_memory_and_swap() {
        assert_eq!(metric_capacity(999, 500, Metric::Cpu), 10_000);
        assert_eq!(metric_capacity(999, 500, Metric::Memory), 999);
        assert_eq!(metric_capacity(999, 500, Metric::Swap), 500);
        assert_eq!(metric_used(12.34, 999, 500, Metric::Cpu), 1234);
        assert_eq!(metric_used(12.34, 999, 500, Metric::Memory), 999);
        assert_eq!(metric_used(12.34, 999, 500, Metric::Swap), 500);
    }

    #[test]
    fn top_clusters_caps_at_top_and_at_the_palette_size() {
        let clusters: Vec<Cluster> = (0..20)
            .map(|i| Cluster { identity: format!("c{i}"), cpu: 0.0, rss: 0, swap: 0, members: vec![] })
            .collect();
        // opts.top (15) exceeds the palette (12): capped at the palette size.
        let top = top_clusters(&clusters, 15);
        assert_eq!(top.len(), style::PALETTE.len());
        assert_eq!(top[0].identity, "c0");
        // A smaller opts.top caps it further.
        assert_eq!(top_clusters(&clusters, 3).len(), 3);
    }

    #[test]
    fn colored_by_assigns_palette_colors_by_rank() {
        let clusters = vec![
            Cluster { identity: "a".into(), cpu: 0.0, rss: 10, swap: 1, members: vec![] },
            Cluster { identity: "b".into(), cpu: 0.0, rss: 20, swap: 2, members: vec![] },
        ];
        let top = top_clusters(&clusters, 15);
        let by_rss = colored_by(&top, |c| c.rss);
        assert_eq!(by_rss, vec![
            ("a".to_string(), 10, style::PALETTE[0]),
            ("b".to_string(), 20, style::PALETTE[1]),
        ]);
        // Same identities/colors, different values, when valued by swap instead.
        let by_swap = colored_by(&top, |c| c.swap);
        assert_eq!(by_swap, vec![
            ("a".to_string(), 1, style::PALETTE[0]),
            ("b".to_string(), 2, style::PALETTE[1]),
        ]);
    }

    #[test]
    fn ring_segments_sum_to_capacity() {
        let colored = vec![
            ("a".to_string(), 40u64, style::PALETTE[0]),
            ("b".to_string(), 20u64, style::PALETTE[1]),
        ];
        let segs = ring_segments(&colored, 70, 100);
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[2].label, "other");
        assert_eq!(segs[2].value, 10); // used(70) - colored_sum(60)
        assert_eq!(segs[3].label, "unused");
        assert_eq!(segs[3].value, 30); // capacity(100) - used(70)
        let sum: u64 = segs.iter().map(|s| s.value).sum();
        assert_eq!(sum, 100);
    }

    #[test]
    fn ring_segments_saturate_when_colored_sum_exceeds_used() {
        // Colored clusters sum to 90 but the reported system "used" is only 50 (e.g. independent
        // rounding skew): "other" saturates to 0 rather than underflowing, and "unused" shrinks to
        // capacity minus the colored sum instead of capacity minus used.
        let colored = vec![("a".to_string(), 90u64, style::PALETTE[0])];
        let segs = ring_segments(&colored, 50, 100);
        assert_eq!(segs[1].label, "other");
        assert_eq!(segs[1].value, 0);
        assert_eq!(segs[2].label, "unused");
        assert_eq!(segs[2].value, 10); // 100 - max(90, 50)
        let sum: u64 = segs.iter().map(|s| s.value).sum();
        assert_eq!(sum, 100);
    }

    #[test]
    fn ring_segments_are_empty_parts_when_fully_accounted_for() {
        // Colored clusters already sum to the full "used" and "used" equals capacity: nothing left
        // over for either remainder, but both segments are still present (value 0).
        let colored = vec![("a".to_string(), 100u64, style::PALETTE[0])];
        let segs = ring_segments(&colored, 100, 100);
        assert_eq!(segs[1].value, 0);
        assert_eq!(segs[2].value, 0);
    }

    #[test]
    fn text_report_shows_header_and_cluster() {
        let clusters = vec![Cluster { identity: "gradle".into(), cpu: 30.0, rss: 3_000_000, swap: 0,
            members: vec![Member { pid: 100, cpu: 10.0, rss: 1_000_000, swap: 0, cmd: "java ...".into() }] }];
        let mut buf = Vec::new();
        write_text(&mut buf, &clusters, 14_000_000, 31_000_000, 0, 0, 30.0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("gradle"));
        assert!(s.contains("cpu")); // header mentions cpu total
        assert!(!s.contains('\u{1b}')); // colors:false -> no ANSI
        assert!(!s.contains("swap")); // no swap configured -> header omits it entirely
    }

    #[test]
    fn text_report_header_shows_swap_when_system_has_it() {
        let clusters = vec![Cluster { identity: "gradle".into(), cpu: 30.0, rss: 3_000_000, swap: 0, members: vec![] }];
        let mut buf = Vec::new();
        // System has swap configured (swap_total>0) but none in use: header shows it, table doesn't.
        write_text(&mut buf, &clusters, 14_000_000, 31_000_000, 0, 8_000_000_000, 30.0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("swap"));
    }

    #[test]
    fn text_report_swap_column_appears_when_swap_in_use() {
        let clusters = vec![Cluster { identity: "gradle".into(), cpu: 30.0, rss: 3_000_000, swap: 2_000_000, members: vec![] }];
        let mut buf = Vec::new();
        write_text(&mut buf, &clusters, 14_000_000, 31_000_000, 2_000_000, 8_000_000_000, 30.0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // human_plain(2_000_000) == "2M": once in the header's swap total, once in the row's swap
        // column, since both the system swap_used and this cluster's swap happen to be 2_000_000.
        assert_eq!(s.matches("2M").count(), 2);
    }

    #[test]
    fn json_report_is_wellformed() {
        let clusters = vec![Cluster { identity: "gradle".into(), cpu: 30.0, rss: 3_000_000, swap: 0, members: vec![] }];
        let mut buf = Vec::new();
        write_json(&mut buf, &clusters, 14_000_000, 31_000_000, 0, 0, 30.0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"clusters\""));
        assert!(s.contains("\"identity\": \"gradle\""));
        assert!(s.contains("\"swap_used\""));
    }

    #[test]
    fn json_report_includes_swap_for_a_swapping_cluster() {
        let clusters = vec![Cluster { identity: "gradle".into(), cpu: 30.0, rss: 3_000_000, swap: 2_000_000,
            members: vec![Member { pid: 100, cpu: 30.0, rss: 3_000_000, swap: 2_000_000, cmd: "java ...".into() }] }];
        let mut buf = Vec::new();
        write_json(&mut buf, &clusters, 14_000_000, 31_000_000, 2_000_000, 8_000_000_000, 30.0, &opts()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"swap_used\": 2000000"));
        assert!(s.contains("\"swap\": 2000000"));
    }
}
