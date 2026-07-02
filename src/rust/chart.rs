//! Shared chart building: turn (label, value) items into palette-colored segments, ready to render
//! as a donut (via `donut::slices`) or as bar/legend rows. Generalizes dq's folder/file legend
//! building so `pq` can reuse the same rendering primitives for process clusters.

use crate::donut;

const PALETTE: [(u8, u8, u8); 12] = crate::style::PALETTE;
const OTHER: (u8, u8, u8) = crate::style::OTHER_COLOR;

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
        out.push(Segment { label: "other".to_string(), value: other, color: OTHER });
    }
    out
}

/// Convert segments into `donut::Slice`s for rasterizing.
pub fn slices(segments: &[Segment]) -> Vec<donut::Slice> {
    segments.iter().map(|s| (s.value, s.color)).collect()
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
