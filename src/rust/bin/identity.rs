//! Identity resolution: turn a process's comm + argv into the real application label, so a JVM
//! running a Gradle daemon shows up as `gradle`, not `java`. Table-driven and pure so adding a new
//! runtime or wrapper is a data change, not new control flow.

/// Runtimes that hide the real application behind a generic interpreter/vm name. Never returned
/// as the identity when a more specific label can be derived from the argv.
pub const RUNTIMES: &[&str] = &["java", "python", "python2", "python3", "node", "deno", "bun", "ruby",
                                "perl", "php", "mono", "dotnet", "sh", "bash", "zsh", "electron"];

/// Commands that just re-exec their arguments under different privileges/scheduling; we strip them
/// and resolve the wrapped command instead.
const WRAPPERS: &[&str] = &["sudo", "env", "nice", "ionice", "timeout", "nohup", "setsid", "doas"];

/// Java main-class / arg substrings that map to a canonical, human-friendly identity.
const JAVA_MARKERS: &[(&str, &str)] = &[
    ("org.gradle", "gradle"), ("GradleDaemon", "gradle"),
    ("org.apache.maven", "maven"),
    ("KotlinCompileDaemon", "kotlin"),
    ("elasticsearch", "elasticsearch"),
];

/// Whether `exe` is a bare runtime name (used both here and by clustering's tree-inheritance rule).
pub fn is_bare_runtime(exe: &str) -> bool {
    RUNTIMES.contains(&exe)
}

/// Resolve a process's real application identity from its comm and argv.
pub fn resolve(comm: &str, cmdline: &[String]) -> String {
    let argv = strip_wrappers(cmdline);
    let arg0 = argv.first();
    let mut exe = arg0.map(|a| exe_name(a, comm)).unwrap_or_else(|| comm.to_string());

    // A version-number basename (e.g. .../claude/versions/2.1.198) is not an app name; use the
    // nearest meaningful directory in the path instead (-> "claude"). Generalizes to version-pinned
    // installs (nvm/asdf/sdkman, `.../app/versions/X.Y.Z/bin`, ...).
    if is_versiony(&exe) {
        if let Some(better) = arg0.and_then(|a| meaningful_from_path(a)) {
            exe = better;
        }
    }

    // Chromium instances are told apart by their --user-data-dir profile, so an automation / second
    // browser (e.g. Playwright's /tmp/siri-repro-profile) does not merge with your main chrome.
    if is_chromium(&exe, comm) {
        return chrome_identity(&argv);
    }

    if !is_bare_runtime(&exe) {
        return exe;
    }

    match exe.as_str() {
        e if e.starts_with("java") => resolve_java(&argv),
        e if e.starts_with("python") || e == "node" || e == "deno" || e == "bun"
            || e == "ruby" || e == "perl" || e == "php"
            || e == "sh" || e == "bash" || e == "zsh" => script_name(&argv).unwrap_or(exe),
        "electron" => electron_app(&argv).unwrap_or(exe),
        _ => exe
    }
}

/// The last path segment of `path` (its basename), or the whole string if there's no separator.
fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// The command name from an argv[0]-style token: its basename, then the first whitespace token.
/// The second step handles processes that rewrite their cmdline into a single space-joined blob
/// (setproctitle, e.g. Chrome's browser process), where argv[0] is the whole command line.
fn command_name(arg: &str) -> String {
    let base = arg.rsplit('/').next().unwrap_or(arg);
    base.split_whitespace().next().unwrap_or(base).to_string()
}

/// The program name for argv[0], preferring the kernel `comm` when argv[0] is a setproctitle blob
/// whose leading token is an absolute path: Chrome renderers rewrite their title to start with the
/// `--user-data-dir` path, so the leading path's basename is a data dir (e.g. "siri-repro-profile"),
/// not the program. `comm` ("chrome") is the trustworthy name there. Otherwise use the basename of
/// the leading token (which also unwraps a "name --flags" blob to just the name).
fn exe_name(arg0: &str, comm: &str) -> String {
    let first = arg0.split_whitespace().next().unwrap_or(arg0);
    if first != arg0 && first.starts_with('/') {
        comm.to_string()
    } else {
        basename(first)
    }
}

/// Whether this process is a Chromium-family browser. Chrome's own `comm` ("chrome"/"chromium") is
/// reliable even when its argv[0] is a rewritten setproctitle blob, so we trust either signal.
fn is_chromium(exe: &str, comm: &str) -> bool {
    matches!(exe, "chrome" | "chromium" | "chromium-browser" | "chrome-linux64")
        || matches!(comm, "chrome" | "chromium")
}

/// Identity for a Chromium process, qualified by its `--user-data-dir` profile so a second/automation
/// browser (Playwright's `/tmp/siri-repro-profile`, a scripted Chrome) reads as `chrome (profile)`
/// and does not merge with your main browser. The default profile, or none, stays plain `chrome`.
fn chrome_identity(argv: &[String]) -> String {
    match chrome_user_data_dir(argv).map(|d| basename(&d)) {
        Some(profile) if !is_default_chrome_profile(&profile) => format!("chrome ({})", profile),
        _ => "chrome".to_string(),
    }
}

/// The Chromium `--user-data-dir`, from an explicit `--user-data-dir=X` flag anywhere in argv, or -
/// for renderers whose title was rewritten to start with the profile path - the leading path token
/// of a setproctitle blob (when that path isn't the chrome executable itself).
fn chrome_user_data_dir(argv: &[String]) -> Option<String> {
    for a in argv {
        if let Some(v) = a.strip_prefix("--user-data-dir=") {
            return Some(v.to_string());
        }
    }
    let a = argv.first()?;
    let first = a.split_whitespace().next().unwrap_or(a);
    if first != a && first.starts_with('/') && !matches!(basename(first).as_str(), "chrome" | "chromium") {
        return Some(first.to_string());
    }
    None
}

/// Whether a `--user-data-dir` basename is the normal browser profile (default config dir), which
/// should stay plain `chrome` rather than being qualified.
fn is_default_chrome_profile(profile: &str) -> bool {
    matches!(profile, "google-chrome" | "google-chrome-beta" | "google-chrome-unstable"
        | "chromium" | "chrome" | "Default")
}

/// A bare version token like "2.1.198", "v14", or "1.20" - a version, not an application name.
fn is_versiony(s: &str) -> bool {
    let s = s.strip_prefix('v').unwrap_or(s);
    !s.is_empty() && s.contains('.') && s.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Generic path segments that never make a good identity, skipped when a binary's own name is
/// uninformative (a version number) and we walk up its path for something meaningful.
const GENERIC_DIRS: &[&str] = &["versions", "version", "bin", "sbin", "current", "releases",
                                "release", "dist", "build", "target", "node_modules", "libexec",
                                "opt", "usr", "local", "share", "lib", "lib64"];

/// The nearest meaningful directory name in `arg0`'s path, skipping the basename, version-like
/// segments, and generic dirs. Used when the basename itself is just a version number.
fn meaningful_from_path(arg0: &str) -> Option<String> {
    let path = arg0.split_whitespace().next().unwrap_or(arg0);
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for seg in segs.iter().rev().skip(1) {
        if GENERIC_DIRS.contains(&seg.to_ascii_lowercase().as_str()) || is_versiony(seg) {
            continue;
        }
        return Some(seg.to_string());
    }
    None
}

fn is_wrapper(exe: &str) -> bool {
    WRAPPERS.contains(&exe)
}

/// A token that looks like a flag (`-x`, `--long`), not a command or script path.
fn is_flag(tok: &str) -> bool {
    tok.starts_with('-')
}

/// A duration-like positional (e.g. `30`, `30s`, `5m`): all ASCII digits, optionally followed by a
/// single unit char in `smhd`. Used to skip the numeric arg some wrappers (`timeout 30 cmd`,
/// `nice 10 cmd`) place before the wrapped command.
fn is_duration_arg(tok: &str) -> bool {
    let digits = tok.trim_end_matches(|c: char| "smhd".contains(c));
    !digits.is_empty()
        && digits.len() >= tok.len().saturating_sub(1)
        && digits.chars().all(|c| c.is_ascii_digit())
}

/// Drop leading wrapper commands (and their flags) so `sudo -E nginx -g ...` resolves as `nginx`.
/// Some wrappers (`timeout 30 cmd`, `nice 10 cmd`) put a numeric positional before the wrapped
/// command; once at least one wrapper has been stripped, also skip a single duration-like token.
fn strip_wrappers(cmdline: &[String]) -> Vec<String> {
    let mut i = 0;
    while i < cmdline.len() {
        let exe = command_name(&cmdline[i]);
        if !is_wrapper(&exe) {
            break;
        }
        // Skip the wrapper itself, then any flags, stopping at the first non-flag token, which is
        // the wrapped command.
        i += 1;
        while i < cmdline.len() && is_flag(&cmdline[i]) {
            i += 1;
        }
    }
    if i > 0 && i < cmdline.len() && is_duration_arg(&cmdline[i]) {
        i += 1;
    }
    cmdline[i.min(cmdline.len())..].to_vec()
}

/// Flags that consume the following token as a value rather than as a class/path candidate (so
/// `-cp /some/classpath` doesn't get mistaken for the main class).
const JAVA_VALUE_FLAGS: &[&str] = &["-cp", "-classpath", "-p", "--module-path", "--add-modules"];

/// Java identity: `-jar <path>` wins, then a known marker substring, then the main class reduced to
/// its last dotted segment. If nothing more specific is found (e.g. a worker with only a bare
/// classpath and no visible main class), fall back to the bare runtime name so tree-inheritance in
/// clustering can fold it into its parent's identity.
fn resolve_java(argv: &[String]) -> String {
    for i in 0..argv.len() {
        if argv[i] == "-jar" {
            if let Some(jar) = argv.get(i + 1) {
                let name = basename(jar);
                return name.strip_suffix(".jar").unwrap_or(&name).to_string();
            }
        }
    }
    for arg in argv {
        for (marker, name) in JAVA_MARKERS {
            if arg.contains(marker) {
                return name.to_string();
            }
        }
    }
    // No -jar, no marker: the main class is the first non-flag, non-value positional after the JVM
    // options (everything after it is the app's own args), reduced to its last dotted segment. Skip
    // both plain flags and value-flags' paired value (e.g. `-cp <classpath>`).
    let mut candidates = Vec::new();
    let mut i = 1; // argv[0] is "java" itself
    while i < argv.len() {
        if JAVA_VALUE_FLAGS.contains(&argv[i].as_str()) {
            i += 2;
            continue;
        }
        if !is_flag(&argv[i]) {
            candidates.push(argv[i].as_str());
        }
        i += 1;
    }
    match candidates.first() {
        Some(main_class) => main_class.rsplit('.').next().unwrap_or(main_class).to_string(),
        None => "java".to_string()
    }
}

/// Script interpreters: the first argv entry after the interpreter itself that isn't a flag.
fn script_name(argv: &[String]) -> Option<String> {
    argv.iter().skip(1).find(|a| !is_flag(a)).map(|a| basename(a))
}

/// Electron/appimage: the first non-flag path argument, taken as the app name.
fn electron_app(argv: &[String]) -> Option<String> {
    argv.iter().skip(1).find(|a| !is_flag(a)).map(|a| basename(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(parts: &[&str]) -> Vec<String> { parts.iter().map(|s| s.to_string()).collect() }

    #[test] fn plain_binary_uses_basename() {
        assert_eq!(resolve("postgres", &v(&["/usr/lib/postgresql/16/bin/postgres","-D","/data"])), "postgres");
    }
    #[test] fn java_jar_uses_jar_name() {
        assert_eq!(resolve("java", &v(&["java","-Xmx1g","-jar","/opt/app/foo-1.2.jar"])), "foo-1.2");
    }
    #[test] fn java_gradle_daemon() {
        assert_eq!(resolve("java", &v(&["java","-cp","...","org.gradle.launcher.daemon.bootstrap.GradleDaemon","8.5"])), "gradle");
    }
    #[test] fn python_script() {
        assert_eq!(resolve("python3", &v(&["python3","/home/me/train.py","--epochs","3"])), "train.py");
    }
    #[test] fn node_script() {
        assert_eq!(resolve("node", &v(&["node","/app/server.js"])), "server.js");
    }
    #[test] fn wrapper_stripped() {
        assert_eq!(resolve("sudo", &v(&["sudo","-E","nginx","-g","daemon off;"])), "nginx");
    }
    #[test] fn empty_cmdline_falls_back_to_comm() {
        assert_eq!(resolve("kworker/0:1", &[]), "kworker/0:1");
    }
    #[test] fn versioned_binary_uses_nearest_app_dir() {
        // Claude's helper: the binary is literally the version number, so use the app dir instead.
        assert_eq!(resolve("2.1.198", &v(&["/home/rfirmin/.local/share/claude/versions/2.1.198","--bg-pty-host","/tmp/x"])), "claude");
        // A version-pinned runtime keeps working: its basename is "node" (not the version), so the
        // normal runtime path resolves the script.
        assert_eq!(resolve("node", &v(&["/home/me/.nvm/versions/node/v20.11.0/bin/node","/app/server.js"])), "server.js");
    }
    #[test] fn is_versiony_detects_versions() {
        assert!(is_versiony("2.1.198"));
        assert!(is_versiony("v14.0"));
        assert!(!is_versiony("claude"));
        assert!(!is_versiony("libc.so.6")); // has letters
        assert!(!is_versiony("7z"));
    }

    #[test] fn setproctitle_blob_uses_first_token() {
        // Chrome's browser process rewrites cmdline into a single space-joined blob.
        assert_eq!(resolve("chrome", &v(&["/opt/google/chrome/chrome --type=renderer --foo"])), "chrome");
        assert_eq!(resolve("chrome", &v(&["chrome --ozone-platform=wayland"])), "chrome");
    }
    #[test] fn chromium_qualified_by_profile() {
        // Playwright/automation chrome: distinguished from the main browser by its user-data-dir,
        // whether it arrives as a setproctitle blob (leading profile path) or an explicit flag.
        assert_eq!(resolve("chrome", &v(&["/tmp/siri-repro-profile --change-stack-guard-on-fork=enable --foo"])), "chrome (siri-repro-profile)");
        assert_eq!(resolve("chrome", &v(&["/opt/google/chrome/chrome", "--type=renderer", "--user-data-dir=/tmp/siri-repro-profile"])), "chrome (siri-repro-profile)");
        // The main browser (default profile, or no --user-data-dir) stays plain "chrome".
        assert_eq!(resolve("chrome", &v(&["/opt/google/chrome/chrome", "--type=renderer", "--user-data-dir=/home/me/.config/google-chrome"])), "chrome");
        assert_eq!(resolve("chrome", &v(&["chrome --ozone-platform=wayland"])), "chrome");
    }
    #[test] fn java_main_class_is_first_positional_not_last() {
        assert_eq!(resolve("java", &v(&["java","-Xmx1g","com.example.Server","--port","8080","config.yml"])), "Server");
    }
    #[test] fn timeout_wrapper_skips_duration() {
        assert_eq!(resolve("timeout", &v(&["timeout","30","java","-jar","/opt/foo.jar"])), "foo");
        assert_eq!(resolve("nice", &v(&["nice","10","postgres","-D","/data"])), "postgres");
    }
}
