//! Leaf formatting: size humanization, the chart color palette, and small ANSI wrappers. This has
//! no knowledge of the report layout, which keeps it easy to unit-test in isolation.

// Distinct tile/arc colors (a Tableau-style palette) with matching legend swatches; the overflow
// "other" slice uses OTHER_COLOR.
pub(crate) const PALETTE: [(u8, u8, u8); 12] = [
    (0x4e, 0x79, 0xa7), (0xf2, 0x8e, 0x2b), (0x59, 0xa1, 0x4f), (0xe1, 0x57, 0x59),
    (0x76, 0xb7, 0xb2), (0xed, 0xc9, 0x48), (0xb0, 0x7a, 0xa1), (0xff, 0x9d, 0xa7),
    (0x9c, 0x75, 0x5f), (0x86, 0xbc, 0xb6), (0xd4, 0xa6, 0xc8), (0x79, 0x9d, 0xd5),
];
pub(crate) const OTHER_COLOR: (u8, u8, u8) = (0x60, 0x60, 0x66);

// ANSI foreground codes used by the size heat-map.
const RED: i32 = 31;
const GREEN: i32 = 32;
const YELLOW: i32 = 33;
const CYAN: i32 = 36;

/// Human-readable size, e.g. "1.23M", with no ANSI.
pub(crate) fn human_plain(size: u64) -> String {
    match size {
        s if s >= 1_000_000_000 => format!("{}G", round2(size, 1_000_000_000)),
        s if s >= 1_000_000 => format!("{}M", round2(size, 1_000_000)),
        s if s >= 1_000 => format!("{}K", round2(size, 1_000)),
        _ => size.to_string()
    }
}

fn round2(size: u64, divisor: u64) -> f64 {
    ((size as f64 / divisor as f64) * 100.0).round() / 100.0
}

/// A magnitude-colored size, for the header totals.
pub(crate) fn size_text(size: u64, colors: bool) -> String {
    paint(&human_plain(size), magnitude_color(size), size >= 1_000_000_000, colors)
}

/// Heat-map a size: red (huge) -> yellow -> green -> cyan (small).
pub(crate) fn magnitude_color(size: u64) -> i32 {
    match size {
        s if s >= 1_000_000_000 => RED,
        s if s >= 1_000_000 => YELLOW,
        s if s >= 1_000 => GREEN,
        _ => CYAN
    }
}

pub(crate) fn paint(s: &str, color: i32, bold: bool, colors: bool) -> String {
    if !colors {
        return s.to_string();
    }
    let weight = if bold { "\x1B[1m" } else { "" };
    format!("{}\x1B[{}m{}\x1B[0m", weight, color, s)
}

pub(crate) fn bold(s: &str, colors: bool) -> String {
    if colors {
        format!("\x1B[1m{}\x1B[0m", s)
    } else {
        s.to_string()
    }
}

pub(crate) fn dim(s: &str, colors: bool) -> String {
    if colors {
        format!("\x1B[2m{}\x1B[0m", s)
    } else {
        s.to_string()
    }
}

pub(crate) fn color_swatch(color: (u8, u8, u8), colors: bool) -> String {
    if colors {
        format!("\x1B[38;2;{};{};{}m██\x1B[0m", color.0, color.1, color.2)
    } else {
        "██".to_string()
    }
}

/// Shorten a string to `max` characters by eliding the middle, keeping both ends visible.
pub(crate) fn truncate_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let budget = max - 1;
    let front = budget / 2;
    let back = budget - front;
    let head: String = chars[..front].iter().collect();
    let tail: String = chars[chars.len() - back..].iter().collect();
    format!("{}…{}", head, tail)
}

/// Append spaces to a colored string so its visible width reaches `width` (a format specifier can't
/// do this because it would count the ANSI bytes).
pub(crate) fn pad_visible(colored: &str, visible: usize, width: usize) -> String {
    if visible >= width {
        colored.to_string()
    } else {
        format!("{}{}", colored, " ".repeat(width - visible))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_plain_units_and_boundaries() {
        assert_eq!(human_plain(0), "0");
        assert_eq!(human_plain(999), "999");
        assert_eq!(human_plain(1_000), "1K");
        assert_eq!(human_plain(1_500), "1.5K");
        assert_eq!(human_plain(999_999), "1000K");
        assert_eq!(human_plain(1_000_000), "1M");
        assert_eq!(human_plain(1_000_000_000), "1G");
        assert_eq!(human_plain(1_234_567_890), "1.23G");
    }

    #[test]
    fn magnitude_color_boundaries() {
        assert_eq!(magnitude_color(999), CYAN);
        assert_eq!(magnitude_color(1_000), GREEN);
        assert_eq!(magnitude_color(1_000_000), YELLOW);
        assert_eq!(magnitude_color(1_000_000_000), RED);
    }

    #[test]
    fn truncate_middle_keeps_both_ends() {
        assert_eq!(truncate_middle("abcdef", 10), "abcdef"); // fits
        assert_eq!(truncate_middle("abcdefghij", 5), "ab…ij");
        assert_eq!(truncate_middle("abcdefghij", 1), "…");
        // Never exceeds the budget.
        assert!(truncate_middle("a-very-long-filename.so", 8).chars().count() <= 8);
    }

    #[test]
    fn styling_is_a_noop_without_colors() {
        assert_eq!(paint("x", RED, true, false), "x");
        assert_eq!(dim("x", false), "x");
        assert_eq!(bold("x", false), "x");
        assert_eq!(color_swatch(OTHER_COLOR, false), "██");
    }

    #[test]
    fn pad_visible_reaches_target_width() {
        assert_eq!(pad_visible("abc", 3, 6), "abc   ");
        assert_eq!(pad_visible("abc", 3, 3), "abc"); // already wide enough
    }
}
