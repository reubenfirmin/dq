use std::fs::{File, OpenOptions};
use std::io::{stdout, IsTerminal};
use std::os::unix::io::AsRawFd;

/**
 * Column width of the terminal attached to stdout, or None if stdout isn't a terminal.
 *
 * Done with a direct TIOCGWINSZ ioctl so we don't pull in a crate just for this. The whole tool is
 * already Linux-only (it reads st_dev and skips /proc, /sys), so the hard-coded request is fine.
 */
pub fn stdout_width() -> Option<usize> {
    let out = stdout();
    if !out.is_terminal() {
        return None;
    }

    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }

    const TIOCGWINSZ: u64 = 0x5413;

    let mut ws = Winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    let ret = unsafe { ioctl(out.as_raw_fd(), TIOCGWINSZ, &mut ws as *mut Winsize) };

    if ret == 0 && ws.ws_col > 0 {
        Some(ws.ws_col as usize)
    } else {
        None
    }
}

/**
 * A snapshot of the controlling terminal's line mode, held so it can be restored.
 */
pub struct TtyMode {
    file: File,
    termios: libc::termios
}

/**
 * Snapshot the controlling terminal's mode via /dev/tty, so cooked mode can be restored if a
 * capability probe leaves the terminal in raw mode after timing out.
 */
pub fn save_tty_mode() -> Option<TtyMode> {
    let file = OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    let ok = unsafe { libc::tcgetattr(file.as_raw_fd(), &mut termios) == 0 };
    if ok {
        Some(TtyMode { file, termios })
    } else {
        None
    }
}

pub fn restore_tty_mode(mode: &TtyMode) {
    unsafe {
        libc::tcsetattr(mode.file.as_raw_fd(), libc::TCSANOW, &mode.termios);
    }
}
