use std::io::{stderr, IsTerminal, Write};
use std::time::{Duration, Instant};
use crate::view::human_size;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BAR_WIDTH: usize = 20;
const REDRAW_INTERVAL: Duration = Duration::from_millis(80);

/**
 * A live scan indicator drawn on stderr. We never know the total work up front (that's what the
 * scan is measuring), so completion is approximated and the counts are the honest part.
 */
pub struct Progress {
    enabled: bool,
    frame: usize,
    dirs: u64,
    bytes: u64,
    fraction: f64,
    last_draw: Instant,
}

impl Progress {
    pub fn new(enabled: bool) -> Self {
        Progress {
            // Only draw when explicitly enabled AND stderr is a terminal, so piping stays clean.
            enabled: enabled && stderr().is_terminal(),
            frame: 0,
            dirs: 0,
            bytes: 0,
            fraction: 0.0,
            last_draw: Instant::now(),
        }
    }

    /**
     * Record one completed directory. `pending` is the number of scans still outstanding.
     */
    pub fn update(&mut self, pending: usize, bytes: u64) {
        self.dirs += 1;
        self.bytes += bytes;

        if !self.enabled {
            return;
        }

        // We can't know the total up front, so approximate completion as
        // processed / (processed + pending), clamped so the bar never runs backwards when a
        // directory reveals many new subdirectories. It reaches 1.0 exactly when pending hits 0.
        let total = self.dirs + pending as u64;
        let fraction = self.dirs as f64 / total as f64;
        if fraction > self.fraction {
            self.fraction = fraction;
        }

        if self.last_draw.elapsed() >= REDRAW_INTERVAL {
            self.draw();
        }
    }

    fn draw(&mut self) {
        self.frame = (self.frame + 1) % FRAMES.len();
        self.last_draw = Instant::now();

        let filled = ((self.fraction * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
        let bar: String = "█".repeat(filled) + &"░".repeat(BAR_WIDTH - filled);
        let pct = (self.fraction * 100.0).round() as u64;

        let line = format!(
            "{} [{}] {:>3}%  {} dirs  {}",
            FRAMES[self.frame], bar, pct, self.dirs, human_size(self.bytes)
        );

        let mut err = stderr();
        // Left-pad to a fixed width to overwrite any leftover from a previous, longer line.
        let _ = write!(err, "\r{:<70}", line);
        let _ = err.flush();
    }

    /**
     * Clear the indicator line so it doesn't linger above the report.
     */
    pub fn finish(&mut self) {
        if !self.enabled {
            return;
        }
        let mut err = stderr();
        let _ = write!(err, "\r{:<70}\r", "");
        let _ = err.flush();
    }
}
