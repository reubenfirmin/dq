//! Shared chart building: turn (label, value) items into palette-colored segments, ready to render
//! as a donut (via `donut::slices`) or as bar/legend rows. Generalizes dq's folder/file legend
//! building so `pq` can reuse the same rendering primitives for process clusters.

use crate::donut;

const PALETTE: [(u8, u8, u8); 12] = crate::style::PALETTE;
const OTHER: (u8, u8, u8) = crate::style::OTHER_COLOR;

/// Bar width (in characters) for the text-mode proportional bars, shared by dq and pq.
pub const BAR_WIDTH: usize = 20;
/// Width of the header's horizontal rule, shared by dq and pq.
pub const RULE_WIDTH: usize = 52;
/// Floor on a truncated label's width, shared by dq and pq.
pub const MIN_LABEL_WIDTH: usize = 12;
/// Square pixel canvas a donut is rasterized into (viuer scales it to the cell box).
pub const DONUT_PX: u32 = 600;

/// One chart entry: a label, its value, and the color it should be drawn with.
pub struct Segment {
    pub label: String,
    pub value: u64,
    pub color: (u8, u8, u8)
}

/**
 * Build palette-colored segments from (label, value) items, largest kept, remainder folded into
 * one gray "other" segment sized `other = total - kept_sum` (pass total==sum for a plain roll-up).
 */
pub fn segments(items: &[(String, u64)], total: u64, max_slices: usize) -> Vec<Segment> {
    segments_labeled(items, total, max_slices, |_folded| "other".to_string())
}

/// Like `segments`, but the "other" segment's label is computed from the number of items folded
/// into it (e.g. pq's "N smaller files"), instead of the plain "other".
pub fn segments_labeled(items: &[(String, u64)], total: u64, max_slices: usize, other_label: impl Fn(usize) -> String) -> Vec<Segment> {
    let visible: Vec<&(String, u64)> = items.iter().filter(|(_, v)| *v > 0).collect();
    let k = visible.len().min(max_slices).min(PALETTE.len());
    let mut out = Vec::new();
    let mut kept = 0u64;
    for i in 0..k {
        out.push(Segment { label: visible[i].0.clone(), value: visible[i].1, color: PALETTE[i] });
        kept += visible[i].1;
    }
    let other = total.saturating_sub(kept);
    if other > 0 {
        out.push(Segment { label: other_label(visible.len() - k), value: other, color: OTHER });
    }
    out
}

/// Convert segments into `donut::Slice`s for rasterizing.
pub fn slices(segments: &[Segment]) -> Vec<donut::Slice> {
    segments.iter().map(|s| (s.value, s.color)).collect()
}

/// How many of `width` bar cells should be filled for a `frac` (0.0-1.0, or slightly over from
/// independent rounding skew) proportion, rounded and clamped so it never exceeds the bar.
pub fn bar_fill(frac: f64, width: usize) -> usize {
    ((frac * width as f64).round() as usize).min(width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_fold_remainder_into_other() {
        let items: Vec<(String, u64)> = (0..20).map(|i| (format!("p{i}"), 100 - i as u64)).collect();
        let sum: u64 = items.iter().map(|(_, v)| v).sum();
        let segs = segments(&items, sum, 12);
        assert_eq!(segs.len(), 13); // 12 + other
        assert_eq!(segs.last().unwrap().label, "other");
        // other == total - kept
        let kept: u64 = segs[..12].iter().map(|s| s.value).sum();
        assert_eq!(segs.last().unwrap().value, sum - kept);
    }
}
