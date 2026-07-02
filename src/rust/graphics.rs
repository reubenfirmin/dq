//! Terminal graphics-protocol capability probe, shared by dq and pq: both render a donut when the
//! terminal supports raster graphics (kitty / iTerm2 / sixel) and fall back to text otherwise.

use crate::term::{save_tty_mode, restore_tty_mode, drain_input, set_noecho};

/**
 * Whether the terminal supports any of the raster graphics protocols viuer can drive.
 *
 * The probes work by protocol query (kitty escape query, sixel via DA1) with a device-attributes
 * fallback, so any live terminal answers quickly regardless of $TERM. But viuer reads the reply with
 * a blocking read, so a terminal that never answers would hang the caller. We therefore run the
 * probe on a helper thread with a deadline (a truly dead tty is the only thing that hits it), and
 * restore the tty mode afterwards in case a timed-out probe left it raw.
 */
pub fn supported() -> bool {
    let debug = std::env::var_os("DQ_DEBUG").is_some();
    let saved = save_tty_mode();
    // Suppress echo for the whole probe window: viuer restores the terminal to cooked/echo between
    // its reads, so a late query reply (e.g. the DA1 hang-guard fallback) would otherwise echo to the
    // screen before we can drain it. We put the original mode back below.
    if let Some(mode) = &saved {
        set_noecho(mode);
    }

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let kitty = viuer::get_kitty_support();
        let iterm = viuer::is_iterm_supported();
        // Sixel's DA1 probe is only needed (and only worth its query) when kitty/iTerm say no.
        let sixel = if kitty != viuer::KittySupport::None || iterm {
            false
        } else {
            viuer::is_sixel_supported()
        };
        let _ = tx.send((kitty, iterm, sixel));
    });

    let outcome = rx.recv_timeout(std::time::Duration::from_millis(1500));
    if let Some(mode) = &saved {
        // A short settle so a slightly-late query reply is buffered (echo is still off), then discard
        // any unconsumed reply before restoring the original mode, so it can never leak to screen.
        std::thread::sleep(std::time::Duration::from_millis(15));
        drain_input(mode);
        restore_tty_mode(mode);
    }

    match outcome {
        Ok((kitty, iterm, sixel)) => {
            let supported = kitty != viuer::KittySupport::None || iterm || sixel;
            if debug {
                let kitty = match kitty {
                    viuer::KittySupport::None => "none",
                    viuer::KittySupport::Local => "local",
                    viuer::KittySupport::Remote => "remote"
                };
                eprintln!("dq[debug]: kitty={} iterm={} sixel={} -> {}", kitty, iterm, sixel, supported);
            }
            supported
        }
        Err(_) => {
            if debug {
                eprintln!("dq[debug]: capability probe timed out (>1500ms); terminal never replied");
            }
            false
        }
    }
}

/// Log a viuer render failure to stderr when `DQ_DEBUG` is set, so a failed draw isn't silent.
pub fn debug_print_failure(e: &viuer::ViuError) {
    if std::env::var_os("DQ_DEBUG").is_some() {
        eprintln!("dq[debug]: viuer::print failed: {e}");
    }
}
