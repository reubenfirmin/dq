use std::collections::HashMap;
use std::io::{self, BufWriter, Write};

mod donut;
mod style;

use donut::{build_donut_image, build_two_donut_image};
use style::{bold, color_swatch, dim, human_plain, magnitude_color, pad_visible, paint, size_text, truncate_middle, OTHER_COLOR, PALETTE};

pub struct FormatOptions {
    pub json: bool,
    pub nosummary: bool,
    pub zeroes: bool,
    pub colors: bool,
    /// Terminal width for truncating long labels, or None to leave them untouched.
    pub width: Option<usize>,
    /// The terminal supports a raster graphics protocol (kitty / iTerm2 / sixel).
    pub graphics: bool
}

const SIZE_WIDTH: usize = 8;
const BAR_WIDTH: usize = 20;
const RULE_WIDTH: usize = 52;
// Fixed part of a text row: size + "  " + "100%" + "  " + bar + "  " before the label.
const ROW_PREFIX: usize = SIZE_WIDTH + 2 + 4 + 2 + BAR_WIDTH + 2;
// Legend row fixed part: swatch(2) + 2 + size + 2 + "100%"(4) + 2 before the label.
const LEGEND_PREFIX: usize = 2 + 2 + SIZE_WIDTH + 2 + 4 + 2;
const MIN_LABEL_WIDTH: usize = 12;
// How many "in this dir" files to list before summarizing the rest (text mode).
const LOOSE_LIMIT: usize = 12;
// Square pixel canvas one (stacked) donut is rasterized into (viuer scales it to the cell box).
const DONUT_PX: u32 = 600;
// Minimum terminal width to place the folder and file donuts side by side.
const SIDE_BY_SIDE_MIN: usize = 88;
// Two-column layout: total width cap, the gap (cells) between columns, and image pixels per
// cell-width unit for the side-by-side canvas.
const MAX_BAND: usize = 96;
const COLUMN_GAP: usize = 8;
const DONUT_UNIT_PX: u32 = 14;

/// A directory or file entry as it appears in a chart legend.
struct LegendEntry {
    color: (u8, u8, u8),
    size: u64,
    label: String
}

/// Human-readable size (e.g. "1.23M") with no ANSI, for the progress indicator.
pub fn human_size(size: u64) -> String {
    human_plain(size)
}

/**
 * Output a report of directory sizes, largest first. Writing is buffered and a broken pipe
 * (e.g. piping into `head` or quitting `less` early) exits quietly instead of panicking.
 */
pub fn report(dir: String, results: HashMap<String, u64>, loose_files: Vec<(String, u64)>, options: FormatOptions) {
    // Own a fresh stdout handle (not a held lock) so the graphics renderer can call viuer::print,
    // which locks stdout itself; a held StdoutLock would deadlock it.
    let mut out = BufWriter::new(io::stdout());

    match write_report(&mut out, &dir, &results, &loose_files, &options).and_then(|_| out.flush()) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => {
            eprintln!("dq: error writing output: {}", e);
            std::process::exit(1);
        }
    }
}

fn write_report<W: Write>(out: &mut W, dir: &str, results: &HashMap<String, u64>, loose_files: &[(String, u64)], options: &FormatOptions) -> io::Result<()> {
    let full_size: u64 = results.values().sum();
    // The scanned directory's own entry is its loose-file size; surfaced in the header, not as a row.
    let loose = *results.get(dir).unwrap_or(&0);

    let mut rows: Vec<(&String, u64)> = results.iter()
        .filter(|(path, _)| path.as_str() != dir)
        .map(|(path, size)| (path, *size))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));

    if options.json {
        return write_json(out, dir, loose, full_size, &rows, loose_files, options);
    }

    let colors = options.colors;
    let one_percent = full_size / 100;
    // Break down the "in this dir" total only when it's a meaningful chunk with real files behind it.
    let show_files = loose > one_percent && loose_files.iter().any(|(_, size)| *size > 0);
    let width = options.width.unwrap_or(80);

    // Wide graphics terminals get the two donuts side by side, with the totals folded into the
    // column headers (so the summary isn't repeated). Everything else keeps the one-line summary.
    if options.graphics && show_files && width >= SIDE_BY_SIDE_MIN {
        draw_two_columns(out, dir, &rows, loose_files, full_size, loose, options)?;
        writeln!(out)?;
        return Ok(());
    }

    writeln!(out)?;
    writeln!(
        out,
        "{}  {}  {} total  {}  {} in this dir",
        bold(dir, colors),
        dim("│", colors),
        size_text(full_size, colors),
        dim("│", colors),
        size_text(loose, colors),
    )?;
    writeln!(out, "{}", dim(&"─".repeat(RULE_WIDTH), colors))?;

    // Folders: a stacked donut on a graphics terminal, size/pct/bar rows otherwise.
    if !(options.graphics && draw_donut_block(out, &build_folder_legend(dir, &rows, full_size), full_size, options)?) {
        write_folder_rows(out, dir, &rows, full_size, options)?;
    }

    if show_files {
        writeln!(out)?;
        writeln!(out, "{}", dim("in this dir:", colors))?;
        // Percentages here are shares of the in-this-dir total, so the breakdown sums to ~100%.
        if !(options.graphics && draw_donut_block(out, &build_files_legend(loose_files), loose, options)?) {
            write_file_rows(out, loose_files, loose, options)?;
        }
    }

    writeln!(out)?;
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// Graphics: donuts with a responsive side-by-side / stacked layout.
// -------------------------------------------------------------------------------------------------

/**
 * The wide-terminal layout: folder and file donuts side by side, the grand total and in-this-dir
 * total folded into aligned column headers (so the summary isn't repeated). Falls back to stacked
 * text if the image can't be drawn.
 */
fn draw_two_columns(out: &mut dyn Write, dir: &str, rows: &[(&String, u64)], loose_files: &[(String, u64)], full_size: u64, loose: u64, options: &FormatOptions) -> io::Result<()> {
    let colors = options.colors;
    let folder_legend = build_folder_legend(dir, rows, full_size);
    let files_legend = build_files_legend(loose_files);

    let band = options.width.unwrap_or(80).min(MAX_BAND);
    let gap = COLUMN_GAP;
    let col = (band - gap) / 2;
    // Plain spacing between columns; the only divider is drawn inside the image (in the gap), which
    // can't drift out of alignment with a text glyph across terminal scaling.
    let divider = " ".repeat(gap);

    // Header: path, then aligned column titles carrying the totals, then a rule under each column.
    writeln!(out)?;
    writeln!(out, "{}", bold(dir, colors))?;

    let lhead = format!("{}  {}", dim("big folders", colors), size_text(full_size, colors));
    let lhead_visible = "big folders".len() + 2 + human_plain(full_size).len();
    let rhead = format!("{}  {}", dim("in this dir", colors), size_text(loose, colors));
    writeln!(out, "{}{}{}", pad_visible(&lhead, lhead_visible, col), divider, rhead)?;
    writeln!(out, "{}{}{}", dim(&"─".repeat(col), colors), divider, dim(&"─".repeat(col), colors))?;

    // One combined image so the two donuts stay aligned and share the divider.
    out.flush()?;
    let image = build_two_donut_image(&slices_from_legend(&folder_legend), &slices_from_legend(&files_legend), col as u32, gap as u32, DONUT_UNIT_PX);
    let config = viuer::Config {
        absolute_offset: false,
        // Match the text columns exactly (2*col + gap) so the divider lines up through everything.
        width: Some((2 * col + gap) as u32),
        height: Some(((col / 2).max(6)) as u32),
        ..Default::default()
    };

    if let Err(e) = viuer::print(&image, &config) {
        debug_print_failure(&e);
        write_folder_rows(out, dir, rows, full_size, options)?;
        writeln!(out)?;
        writeln!(out, "{}", dim("in this dir:", colors))?;
        return write_file_rows(out, loose_files, loose, options);
    }

    write_two_legends(out, &folder_legend, full_size, &files_legend, loose, col, gap, options)
}

/**
 * Draw one donut plus its legend. Returns false (having drawn nothing) if the image failed, so the
 * caller can fall back to text rows.
 */
fn draw_donut_block(out: &mut dyn Write, legend: &[LegendEntry], total: u64, options: &FormatOptions) -> io::Result<bool> {
    let slices = slices_from_legend(legend);
    if slices.is_empty() {
        return Ok(false);
    }

    let cols = options.width.unwrap_or(80).clamp(16, 40) as u32;
    let image = build_donut_image(&slices, DONUT_PX);
    let config = viuer::Config {
        absolute_offset: false,
        width: Some(cols),
        height: Some((cols / 2).max(8)),
        ..Default::default()
    };

    if let Err(e) = viuer::print(&image, &config) {
        debug_print_failure(&e);
        return Ok(false);
    }
    write_legend(out, legend, total, options)?;
    Ok(true)
}

fn debug_print_failure(e: &viuer::ViuError) {
    if std::env::var_os("DQ_DEBUG").is_some() {
        eprintln!("dq[debug]: viuer::print failed: {e}");
    }
}

// -------------------------------------------------------------------------------------------------
// Legends
// -------------------------------------------------------------------------------------------------

/**
 * The top folders as legend entries (distinct palette colors), plus one "other" entry for the rest
 * of the tree so the arcs sum to the whole. Percentages will be shares of the grand total.
 */
fn build_folder_legend(dir: &str, rows: &[(&String, u64)], full_size: u64) -> Vec<LegendEntry> {
    let visible: Vec<&(&String, u64)> = rows.iter().filter(|(_, size)| *size > 0).collect();
    let k = visible.len().min(PALETTE.len());

    let mut legend: Vec<LegendEntry> = Vec::new();
    let mut shown = 0u64;
    for i in 0..k {
        let (path, size) = visible[i];
        legend.push(LegendEntry { color: PALETTE[i], size: *size, label: relativize(dir, path) });
        shown += *size;
    }

    // "Other" completes the ring to the grand total (the rest of the tree, including loose files).
    let other = full_size.saturating_sub(shown);
    if other > 0 {
        legend.push(LegendEntry { color: OTHER_COLOR, size: other, label: "other".to_string() });
    }
    legend
}

/**
 * The biggest files sitting directly in the scanned dir as legend entries, with the remainder folded
 * into one "other" entry. Percentages will be shares of the in-this-dir total.
 */
fn build_files_legend(files: &[(String, u64)]) -> Vec<LegendEntry> {
    let visible: Vec<&(String, u64)> = files.iter().filter(|(_, size)| *size > 0).collect();
    let k = visible.len().min(PALETTE.len());

    let mut legend: Vec<LegendEntry> = Vec::new();
    for i in 0..k {
        let (name, size) = visible[i];
        legend.push(LegendEntry { color: PALETTE[i], size: *size, label: name.clone() });
    }

    let other_count = visible.len() - k;
    let other_sum: u64 = visible[k..].iter().map(|(_, size)| *size).sum();
    if other_sum > 0 {
        legend.push(LegendEntry { color: OTHER_COLOR, size: other_sum, label: format!("{} smaller files", other_count) });
    }
    legend
}

fn slices_from_legend(legend: &[LegendEntry]) -> Vec<donut::Slice> {
    legend.iter().map(|e| (e.size, e.color)).collect()
}

/// A single-column color-keyed legend: swatch, size, share of `total`, and the (truncated) label.
fn write_legend(out: &mut dyn Write, legend: &[LegendEntry], total: u64, options: &FormatOptions) -> io::Result<()> {
    let label_w = options.width.map(|w| w.saturating_sub(LEGEND_PREFIX).max(MIN_LABEL_WIDTH));
    for entry in legend {
        writeln!(out, "{}", legend_cell(entry, total, options, label_w, false))?;
    }
    Ok(())
}

/// Two color-keyed legends side by side, each column `col` cells wide with a `gap` between them.
fn write_two_legends(out: &mut dyn Write, left: &[LegendEntry], left_total: u64, right: &[LegendEntry], right_total: u64, col: usize, gap: usize, options: &FormatOptions) -> io::Result<()> {
    let label_w = Some(col.saturating_sub(LEGEND_PREFIX).max(MIN_LABEL_WIDTH));
    let divider = " ".repeat(gap);
    let n = left.len().max(right.len());
    for i in 0..n {
        let lcell = match left.get(i) {
            Some(entry) => legend_cell(entry, left_total, options, label_w, true),
            None => " ".repeat(col)
        };
        let rcell = match right.get(i) {
            Some(entry) => legend_cell(entry, right_total, options, label_w, false),
            None => String::new()
        };
        writeln!(out, "{}{}{}", lcell, divider, rcell)?;
    }
    Ok(())
}

/// One legend row. `label_w` caps (and, when `pad`, space-fills) the label so two-column rows align.
fn legend_cell(entry: &LegendEntry, total: u64, options: &FormatOptions, label_w: Option<usize>, pad: bool) -> String {
    let size_cell = paint(&format!("{:>width$}", human_plain(entry.size), width = SIZE_WIDTH), magnitude_color(entry.size), entry.size >= 1_000_000_000, options.colors);
    let pct_cell = dim(&format!("{:>3}%", percent_round(entry.size, total)), options.colors);
    let label = match label_w {
        Some(w) if pad => format!("{:<w$}", truncate_middle(&entry.label, w)),
        Some(w) => truncate_middle(&entry.label, w),
        None => entry.label.clone()
    };
    format!("{}  {}  {}  {}", color_swatch(entry.color, options.colors), size_cell, pct_cell, label)
}

// -------------------------------------------------------------------------------------------------
// Text rows (the fallback for both sections)
// -------------------------------------------------------------------------------------------------

/// The big folders as size / pct / bar rows. Percentages are shares of the grand total.
fn write_folder_rows(out: &mut dyn Write, dir: &str, rows: &[(&String, u64)], full_size: u64, options: &FormatOptions) -> io::Result<()> {
    let largest = rows.first().map(|(_, size)| *size).unwrap_or(0);
    let summary = !options.nosummary && !options.zeroes;
    let one_percent = full_size / 100;

    for (path, size) in rows {
        let size = *size;
        if summary && size <= one_percent {
            continue;
        }
        if !options.zeroes && size == 0 {
            continue;
        }
        write_entry(out, &relativize(dir, path), size, full_size, largest, options)?;
    }
    Ok(())
}

/// The biggest "in this dir" files as size / pct / bar rows.
fn write_file_rows(out: &mut dyn Write, files: &[(String, u64)], total: u64, options: &FormatOptions) -> io::Result<()> {
    // Files are shown by count (the biggest offenders), not by the 1% cutoff, since a directory can
    // hold a lot of medium files that individually fall under 1% but together dominate it.
    let largest = files.first().map(|(_, size)| *size).unwrap_or(0);
    let limit = if options.nosummary || options.zeroes { usize::MAX } else { LOOSE_LIMIT };

    let mut shown = 0;
    let mut eligible = 0;
    for (name, size) in files {
        let size = *size;
        if !options.zeroes && size == 0 {
            continue;
        }
        eligible += 1;
        if shown >= limit {
            continue;
        }
        write_entry(out, name, size, total, largest, options)?;
        shown += 1;
    }

    if eligible > shown {
        writeln!(out, "{}", dim(&format!("… and {} more", eligible - shown), options.colors))?;
    }
    Ok(())
}

/**
 * A single "size  pct  bar  label" line. `largest` is the biggest size in this group (the bar's full
 * mark); `total` is the percentage denominator.
 */
fn write_entry(out: &mut dyn Write, label: &str, size: u64, total: u64, largest: u64, options: &FormatOptions) -> io::Result<()> {
    let bar_frac = if largest > 0 { size as f64 / largest as f64 } else { 0.0 };

    let cell = format!("{:>width$}", human_plain(size), width = SIZE_WIDTH);
    let size_cell = paint(&cell, magnitude_color(size), size >= 1_000_000_000, options.colors);
    let pct_cell = dim(&format!("{:>3}%", percent_round(size, total)), options.colors);

    let filled = ((bar_frac * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
    let bar_filled = paint(&"█".repeat(filled), magnitude_color(size), false, options.colors);
    let bar_empty = dim(&"░".repeat(BAR_WIDTH - filled), options.colors);

    let label = match options.width {
        Some(width) => truncate_middle(label, width.saturating_sub(ROW_PREFIX).max(MIN_LABEL_WIDTH)),
        None => label.to_string()
    };

    writeln!(out, "{}  {}  {}{}  {}", size_cell, pct_cell, bar_filled, bar_empty, label)
}

// -------------------------------------------------------------------------------------------------
// JSON
// -------------------------------------------------------------------------------------------------

fn write_json<W: Write>(out: &mut W, dir: &str, loose: u64, full_size: u64, rows: &[(&String, u64)], loose_files: &[(String, u64)], options: &FormatOptions) -> io::Result<()> {
    writeln!(out, "{{")?;
    writeln!(out, "  \"path\": \"{}\",", json_escape(dir))?;
    writeln!(out, "  \"total\": {},", full_size)?;
    writeln!(out, "  \"loose\": {},", loose)?;

    write_json_array(out, "entries", "path", rows.iter().map(|(p, s)| (p.as_str(), *s)), full_size, options)?;
    writeln!(out, ",")?;
    write_json_array(out, "in_this_dir", "name", loose_files.iter().map(|(n, s)| (n.as_str(), *s)), loose, options)?;
    writeln!(out)?;

    writeln!(out, "}}")?;
    Ok(())
}

/// Write one `"key": [ {label_key, bytes, percent}, ... ]` array (no trailing newline).
fn write_json_array<'a, W: Write>(out: &mut W, key: &str, label_key: &str, items: impl Iterator<Item = (&'a str, u64)>, total: u64, options: &FormatOptions) -> io::Result<()> {
    write!(out, "  \"{}\": [", key)?;
    let mut first = true;
    for (label, size) in items {
        if !options.zeroes && size == 0 {
            continue;
        }
        write!(out, "{}", if first { "\n" } else { ",\n" })?;
        write!(out, "    {{\"{}\": \"{}\", \"bytes\": {}, \"percent\": {}}}", label_key, json_escape(label), size, percent(size, total))?;
        first = false;
    }
    if !first {
        write!(out, "\n  ")?;
    }
    write!(out, "]")
}

fn percent(size: u64, total: u64) -> f64 {
    if total > 0 { (size as f64 / total as f64 * 10000.0).round() / 100.0 } else { 0.0 }
}

// -------------------------------------------------------------------------------------------------
// Small helpers
// -------------------------------------------------------------------------------------------------

fn percent_round(size: u64, total: u64) -> u64 {
    if total > 0 { (size as f64 / total as f64 * 100.0).round() as u64 } else { 0 }
}

fn relativize(dir: &str, path: &str) -> String {
    let prefix = if dir.ends_with('/') { dir.to_string() } else { format!("{}/", dir) };
    path.strip_prefix(&prefix).unwrap_or(path).to_string()
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c)
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_legend_adds_an_other_arc() {
        let names: Vec<String> = (0..15).map(|i| format!("d{i}")).collect();
        let rows: Vec<(&String, u64)> =
            names.iter().enumerate().map(|(i, n)| (n, 1000 - i as u64 * 10)).collect();
        let legend = build_folder_legend("root", &rows, 20_000);
        assert_eq!(legend.len(), PALETTE.len() + 1);
        assert_eq!(legend.last().unwrap().label, "other");
    }

    #[test]
    fn files_legend_aggregates_overflow() {
        let files: Vec<(String, u64)> = (0..20).map(|i| (format!("f{i}"), 100 - i as u64)).collect();
        let legend = build_files_legend(&files);
        assert_eq!(legend.len(), PALETTE.len() + 1);
        assert!(legend.last().unwrap().label.contains("smaller files"));
        assert!(build_files_legend(&[]).is_empty());
    }

    #[test]
    fn relativize_strips_the_scanned_prefix() {
        assert_eq!(relativize("/usr/lib64", "/usr/lib64/cef"), "cef");
        assert_eq!(relativize("/", "/usr"), "usr");
        assert_eq!(relativize(".", "./target/deps"), "target/deps");
        // Unrelated path is left as-is.
        assert_eq!(relativize("/usr", "/etc/hosts"), "/etc/hosts");
    }

    #[test]
    fn json_escape_handles_specials() {
        assert_eq!(json_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(json_escape("tab\tnl\n"), "tab\\tnl\\n");
        assert_eq!(json_escape("\u{1}"), "\\u0001");
    }

    #[test]
    fn percent_helpers_round_and_guard_zero() {
        assert_eq!(percent_round(1, 4), 25);
        assert_eq!(percent_round(5, 0), 0);
        assert_eq!(percent(1, 3), 33.33);
        assert_eq!(percent(5, 0), 0.0);
    }

    fn opts() -> FormatOptions {
        FormatOptions { json: false, nosummary: false, zeroes: false, colors: false, width: None, graphics: false }
    }

    #[test]
    fn text_report_has_header_rows_and_files_section() {
        let results = HashMap::from([
            ("root".to_string(), 10),
            ("root/a".to_string(), 100),
            ("root/b".to_string(), 50),
        ]);
        let loose = vec![("big.bin".to_string(), 10)];
        let mut buf = Vec::new();
        write_report(&mut buf, "root", &results, &loose, &opts()).unwrap();
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("160 total"), "{out}");
        assert!(out.contains("10 in this dir"), "{out}");
        assert!(out.contains("a\n") || out.contains("a "), "folder row missing: {out}");
        assert!(out.contains("in this dir:"), "{out}");
        assert!(out.contains("big.bin"), "{out}");
        assert!(!out.contains('\u{1b}'), "colors:false must emit no ANSI: {out:?}");
    }

    #[test]
    fn json_report_is_well_formed() {
        let results = HashMap::from([("root".to_string(), 5), ("root/a".to_string(), 95)]);
        let loose = vec![("x".to_string(), 5)];
        let mut opts = opts();
        opts.json = true;
        let mut buf = Vec::new();
        write_report(&mut buf, "root", &results, &loose, &opts).unwrap();
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("\"total\": 100"), "{out}");
        assert!(out.contains("\"entries\": ["), "{out}");
        assert!(out.contains("\"in_this_dir\": ["), "{out}");
        assert!(out.contains("\"name\": \"x\""), "{out}");
    }
}
