//! Rasterizing the donut charts to RGBA images. Pure rendering: it takes `(size, color)` slices and
//! produces an `image::DynamicImage`, with no dependency on the rest of the report.

use std::f64::consts::{FRAC_PI_2, TAU};

/// Slice geometry as a fraction of the ring radius.
const OUTER_RADIUS: f64 = 0.47;
const INNER_RADIUS: f64 = 0.27;
/// Radians of background left between adjacent arcs.
const ARC_GAP: f64 = 0.012;
/// Supersampling factor (per axis) for antialiasing the ring/arc edges.
const SS: usize = 4;
/// The dim divider drawn down the gap between the two side-by-side donuts.
const DIVIDER_COLOR: [u8; 4] = [90, 90, 96, 255];

pub(crate) type Slice = (u64, (u8, u8, u8));

/// A single donut on a transparent square canvas, so it floats on the terminal's own background.
pub(crate) fn build_donut_image(slices: &[Slice], w: u32) -> image::DynamicImage {
    let mut img = image::RgbaImage::from_pixel(w, w, image::Rgba([0, 0, 0, 0]));
    let c = w as f64 / 2.0;
    draw_donut(&mut img, slices, c, c, w as f64 * OUTER_RADIUS, w as f64 * INNER_RADIUS);
    image::DynamicImage::ImageRgba8(img)
}

/// Two donuts side by side on one transparent canvas (`col` cells wide each, `gap` cells apart, at
/// `unit` pixels per cell), so they print with a single call and share a divider.
pub(crate) fn build_two_donut_image(left: &[Slice], right: &[Slice], col: u32, gap: u32, unit: u32) -> image::DynamicImage {
    let band = 2 * col + gap;
    let w = band * unit;
    let h = col * unit;
    let mut img = image::RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 0]));

    let outer = OUTER_RADIUS * col as f64 * unit as f64;
    let inner = INNER_RADIUS * col as f64 * unit as f64;
    let cy = col as f64 * unit as f64 / 2.0;
    let lcx = col as f64 * unit as f64 / 2.0;
    let rcx = (col as f64 + gap as f64 + col as f64 / 2.0) * unit as f64;
    draw_donut(&mut img, left, lcx, cy, outer, inner);
    draw_donut(&mut img, right, rcx, cy, outer, inner);

    // A dim vertical divider down the center of the gap, in the image's own coordinate space.
    let divx = ((col as f64 + gap as f64 / 2.0) * unit as f64).round() as i64;
    for y in 0..h {
        for dx in -1..=1 {
            let x = divx + dx;
            if x >= 0 && (x as u32) < w {
                img.put_pixel(x as u32, y, image::Rgba(DIVIDER_COLOR));
            }
        }
    }

    image::DynamicImage::ImageRgba8(img)
}

/// Rasterize one donut centered at (cx, cy) into an existing image.
fn draw_donut(img: &mut image::RgbaImage, slices: &[Slice], cx: f64, cy: f64, outer: f64, inner: f64) {
    let total: u64 = slices.iter().map(|(size, _)| *size).sum();
    if total == 0 {
        return;
    }

    // Arc boundaries, starting at the top and sweeping clockwise.
    let mut bounds: Vec<(f64, f64, (u8, u8, u8))> = Vec::with_capacity(slices.len());
    let mut acc = 0.0_f64;
    for (size, color) in slices {
        let start = -FRAC_PI_2 + acc * TAU;
        acc += *size as f64 / total as f64;
        let end = -FRAC_PI_2 + acc * TAU;
        bounds.push((start, end, *color));
    }

    let (w, h) = (img.width(), img.height());
    let x0 = (cx - outer).floor().max(0.0) as u32;
    let x1 = ((cx + outer).ceil() as i64).clamp(0, w as i64) as u32;
    let y0 = (cy - outer).floor().max(0.0) as u32;
    let y1 = ((cy + outer).ceil() as i64).clamp(0, h as i64) as u32;

    // The arc color at a point, or None if it's outside the ring or in a gap.
    let classify = |px: f64, py: f64| -> Option<(u8, u8, u8)> {
        let dx = px - cx;
        let dy = py - cy;
        let dist = (dx * dx + dy * dy).sqrt();
        if dist < inner || dist > outer {
            return None;
        }
        let angle = dy.atan2(dx);
        for (start, end, color) in &bounds {
            if angle_between(angle, start + ARC_GAP, end - ARC_GAP) {
                return Some(*color);
            }
        }
        None
    };

    // Fill solid pixels fast; only supersample the boundary pixels (where the four corners disagree)
    // so ring/arc edges get an antialiased alpha without paying for the interior.
    let step = 1.0 / SS as f64;
    for y in y0..y1 {
        for x in x0..x1 {
            let (fx, fy) = (x as f64, y as f64);
            let corner = classify(fx, fy);
            if corner == classify(fx + 1.0, fy) && corner == classify(fx, fy + 1.0) && corner == classify(fx + 1.0, fy + 1.0) {
                if let Some((r, g, b)) = corner {
                    img.put_pixel(x, y, image::Rgba([r, g, b, 255]));
                }
                continue;
            }

            let (mut r, mut g, mut b, mut cov) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    if let Some((cr, cg, cb)) = classify(fx + (sx as f64 + 0.5) * step, fy + (sy as f64 + 0.5) * step) {
                        r += cr as u32;
                        g += cg as u32;
                        b += cb as u32;
                        cov += 1;
                    }
                }
            }
            if cov > 0 {
                let alpha = (cov * 255 / (SS * SS) as u32) as u8;
                img.put_pixel(x, y, image::Rgba([(r / cov) as u8, (g / cov) as u8, (b / cov) as u8, alpha]));
            }
        }
    }
}

/// Whether `angle` (from atan2, in [-π, π]) falls within the arc [start, end), handling wraparound.
fn angle_between(angle: f64, start: f64, end: f64) -> bool {
    let mut a = angle;
    while a < start {
        a += TAU;
    }
    a < end
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    #[test]
    fn angle_wraparound() {
        // An arc that crosses the -π/π seam still contains angles on both sides.
        assert!(angle_between(3.0, 2.5, 2.5 + 1.0));
        assert!(angle_between(-3.0, 2.5, 2.5 + 1.5)); // -3.0 == 3.28.. after +TAU
        assert!(!angle_between(0.0, 2.5, 3.0));
    }

    #[test]
    fn images_have_expected_dimensions() {
        let slices: Vec<Slice> = vec![(50, (1, 2, 3)), (30, (4, 5, 6)), (20, (7, 8, 9))];
        assert_eq!(build_donut_image(&slices, 200).dimensions(), (200, 200));
        // col=30, gap=6, unit=10 -> width (2*30+6)*10, height 30*10.
        assert_eq!(build_two_donut_image(&slices, &slices, 30, 6, 10).dimensions(), (660, 300));
    }

    #[test]
    fn empty_and_zero_slices_are_safe() {
        assert_eq!(build_donut_image(&[], 100).dimensions(), (100, 100));
        assert_eq!(build_donut_image(&[(0, (0, 0, 0))], 100).dimensions(), (100, 100));
    }
}
