mod view;
mod progress;
mod term;

use std::error::Error;
use std::fs;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::os::unix::fs::MetadataExt;
use threadpool::ThreadPool;
use clap::Parser;
use crate::view::report;
use crate::view::FormatOptions;
use crate::progress::Progress;

// A smarter, faster alternative to du.
#[derive(Parser, Debug)]
#[command(name = "dq", about = "dq: quantify what's eating your disk (a faster du).")]
struct Args {
    /// Emit machine-readable JSON instead of the human report
    #[arg(long)]
    json: bool,

    /// Number of worker threads used to walk the tree
    #[arg(short, long, default_value_t = 50)]
    threads: i32,

    /// Verbose: list every non-empty directory, not just those over 1%
    #[arg(short = 'v', long)]
    nosummary: bool,

    /// Extra verbose: also include zero-size directories
    #[arg(short = 'V', long)]
    zeroes: bool,

    /// Disable the progress indicator
    #[arg(long = "noprogress")]
    noprogress: bool,

    /// Directory to scan (defaults to the current directory)
    #[arg(default_value = ".")]
    dir: String,
}

fn main() {
    let args = Args::parse();
    let dir = args.dir;

    let metadata = match fs::metadata(&dir) {
        Ok(metadata) => metadata,
        Err(e) => {
            eprintln!("dq: cannot access '{}': {}", dir, e);
            std::process::exit(1);
        }
    };
    if !metadata.is_dir() {
        eprintln!("dq: '{}' is not a directory", dir);
        std::process::exit(1);
    }

    let all_results = scan_path(&dir, args.threads.max(1) as usize, metadata.dev(), HashSet::from(["/proc", "/sys"]), !args.noprogress);

    if !all_results.contains_key(&dir) {
        eprintln!("dq: could not read '{}' (permission denied?)", dir);
        std::process::exit(1);
    }

    // A one-line usage hint on stderr, so it's visible on every human run without ever landing in
    // stdout (pipes, redirects, JSON stay clean). Dimmed when stderr is a terminal.
    if !args.json {
        let usage = "usage: dq [OPTIONS] [DIR]   (--help for all options)";
        if std::io::stderr().is_terminal() {
            eprintln!("\x1B[2m{}\x1B[0m", usage);
        } else {
            eprintln!("{}", usage);
        }
    }

    // Colors and path truncation are only meaningful on an interactive terminal; when piped or
    // asked for JSON we emit plain, untruncated output that's easy to redirect or parse.
    let interactive = std::io::stdout().is_terminal() && !args.json;

    // Query the terminal for a graphics protocol (kitty / iTerm2 / sixel). Only when interactive,
    // since detection writes escape queries and reads the replies. Falls back to text when absent.
    let graphics = interactive && graphics_supported();
    if std::env::var_os("DQ_DEBUG").is_some() {
        eprintln!("dq[debug]: interactive={} graphics={}", interactive, graphics);
    }

    // The files sitting directly in the scanned directory, so we can break down the "in this dir"
    // total into its biggest offenders when it's a meaningful chunk of the tree.
    let loose_files = list_direct_files(&dir);

    report(dir, all_results, loose_files, FormatOptions {
        json: args.json,
        nosummary: args.nosummary,
        zeroes: args.zeroes,
        colors: interactive,
        width: if interactive { term::stdout_width() } else { None },
        graphics
    });
}

/**
 * Whether the terminal supports any of the raster graphics protocols viuer can drive.
 *
 * The probes work by protocol query (kitty escape query, sixel via DA1) with a device-attributes
 * fallback, so any live terminal answers quickly regardless of $TERM. But viuer reads the reply with
 * a blocking read, so a terminal that never answers would hang dq. We therefore run the probe on a
 * helper thread with a deadline (a truly dead tty is the only thing that hits it), and restore the
 * tty mode afterwards in case a timed-out probe left it raw.
 */
fn graphics_supported() -> bool {
    let debug = std::env::var_os("DQ_DEBUG").is_some();
    let saved = term::save_tty_mode();

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
    if let Some(mode) = saved {
        term::restore_tty_mode(&mode);
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

/**
 * Iterate a path and its subdirectories, collecting the size of each directory by summing files
 * within it.
 */
fn scan_path(dir: &str, threads: usize, device: u64, blacklist: HashSet<&str>, show_progress: bool) -> HashMap<String, u64> {
    // set up a channel to receive results back from threads
    let (tx, rx) = std::sync::mpsc::channel();

    let pool = ThreadPool::new(threads);

    let mut results = HashMap::new();
    let mut progress = Progress::new(show_progress);

    let path = PathBuf::from(dir);

    // Run until we've received/handled the same number of results as we've submitted. Ideal world
    // we could get more state from the threadpool and not have to track this. We block on recv()
    // rather than spinning: every outstanding task sends exactly one result, so while pending > 0
    // there is always a result on the way.
    let mut pending = 1;
    submit(path, device, &pool, tx.clone());

    while pending > 0 {
        let result = rx.recv().expect("worker channel closed unexpectedly");
        pending -= 1;
        let mut scanned_bytes = 0;
        match result {
            Ok(it) => {
                scanned_bytes = it.size;
                let displayed = it.path.display().to_string();
                results.insert(displayed, it.size);
                for subpath in it.paths {
                    let subdisplay = subpath.display().to_string();
                    if !blacklist.contains(&*subdisplay) {
                        pending += 1;
                        submit(subpath, device, &pool, tx.clone());
                    }
                }
            },
            Err(_) => {
                // we suppress errors because we don't care about folders we don't have
                // access to. TODO add flag to show these
            }
        }
        progress.update(pending, scanned_bytes);
    }
    progress.finish();
    results
}

/**
 * Submits a directory iteration to the worker pool.
 */
fn submit(path: PathBuf, device: u64, pool: &ThreadPool, tx: Sender<Result<DirMetadata, Box<dyn Error + Send + Sync>>>) {
    pool.execute (move || {
        let result = process_directory(&path, device);
        tx.send(result).expect("Couldn't send!");
    });
}

/**
 * Sum the sizes of files in this directory, and collect any direct subpaths.
 */
fn process_directory(dir_path: &Path, device: u64) -> Result<DirMetadata, Box<dyn Error + Send + Sync>> {
    let mut result = DirMetadata {
        path: dir_path.to_path_buf(),
        size: 0,
        paths: Vec::new()
    };

    let mut size = 0;

    for entry in fs::read_dir(dir_path)? {
        let subpath = entry?.path();

        // One stat per entry. symlink_metadata does not follow symlinks, so a single call lets us
        // both detect symlinks and read size/device without re-statting.
        let metadata = match subpath.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue
        };

        if metadata.file_type().is_symlink() {
            continue
        } else if metadata.is_dir() && metadata.dev() == device {
            result.paths.push(subpath);
        } else {
            size += metadata.len();
        }
    }

    result.size = size;
    Ok(result)
}

/**
 * The regular files sitting directly in `dir` (not in subdirectories), largest first. Used to
 * break down the "in this dir" total into its biggest files.
 */
fn list_direct_files(dir: &str) -> Vec<(String, u64)> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.path().symlink_metadata() {
                if metadata.is_file() {
                    files.push((entry.file_name().to_string_lossy().into_owned(), metadata.len()));
                }
            }
        }
    }
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files
}

struct DirMetadata {
    path: PathBuf,
    paths: Vec<PathBuf>,
    size: u64
}