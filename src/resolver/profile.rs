use std::path::Path;

use crate::core::quote::shell_quote;

const FIREFOX_BROWSERS: &[&str] = &[
    "firefox",
    "firefox-esr",
    "floorp",
    "librewolf",
    "waterfox",
    "zen",
    "zen-browser",
];

const CHROMIUM_BROWSERS: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "brave",
    "brave-browser",
    "microsoft-edge",
    "vivaldi",
    "opera",
];

enum BrowserFamily {
    Firefox,
    Chromium,
}

/// Detect browser profile arguments from `/proc/<pid>/cmdline`.
///
/// Returns the profile flags to append to the launch command at restore time,
/// as shell words. Bare when every byte needs no quoting, POSIX single-quoted
/// otherwise. Examples: `"-P work"`, `"-no-remote -P dev"`,
/// `"'--profile-directory=Profile 1'"`.
pub fn detect_browser_profile(pid: i64) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).to_string())
        .collect();
    detect_profile_from_args(&args)
}

/// Pure logic: detect browser profile flags from a parsed command line.
///
/// Some launchers (e.g. Arch's chromium wrapper) produce a single-arg cmdline
/// where all tokens are joined with spaces.  When that happens, re-split on
/// whitespace so classification and extraction still work.
pub fn detect_profile_from_args(args: &[String]) -> Option<String> {
    if args.is_empty() {
        return None;
    }

    if let Some(result) = try_detect(args) {
        return Some(result);
    }

    // Fallback: if single arg contains spaces, re-split
    if args.len() == 1 && args[0].contains(' ') {
        let split: Vec<String> = args[0].split_whitespace().map(String::from).collect();
        return try_detect(&split);
    }

    None
}

fn try_detect(args: &[String]) -> Option<String> {
    let (family, arg_start) = classify_browser(args)?;
    match family {
        BrowserFamily::Firefox => extract_firefox_profile(&args[arg_start..]),
        BrowserFamily::Chromium => extract_chromium_profile(&args[arg_start..]),
    }
}

/// Identify whether the command is a known browser and which family it belongs to.
/// Returns the family and the index where browser-specific args begin.
fn classify_browser(args: &[String]) -> Option<(BrowserFamily, usize)> {
    let basename = Path::new(&args[0])
        .file_name()?
        .to_string_lossy()
        .to_lowercase();

    if FIREFOX_BROWSERS.iter().any(|&b| basename == b) {
        return Some((BrowserFamily::Firefox, 1));
    }
    if CHROMIUM_BROWSERS.iter().any(|&b| basename == b) {
        return Some((BrowserFamily::Chromium, 1));
    }

    if basename == "flatpak" {
        return classify_flatpak_browser(args);
    }

    None
}

/// For `flatpak run [options] <app.id> [browser args...]`, identify the
/// browser family from the app ID and return the arg start index.
fn classify_flatpak_browser(args: &[String]) -> Option<(BrowserFamily, usize)> {
    for (i, arg) in args.iter().enumerate().skip(1) {
        if arg.starts_with('-') || arg == "run" {
            continue;
        }
        if !arg.contains('.') {
            continue;
        }

        let lower = arg.to_lowercase();
        let segments: Vec<&str> = lower.split('.').collect();

        if segments.iter().any(|s| FIREFOX_BROWSERS.contains(s))
            || lower.contains("firefox")
            || lower.contains("zen_browser")
            || lower.contains("floorp")
            || lower.contains("librewolf")
            || lower.contains("waterfox")
        {
            return Some((BrowserFamily::Firefox, i + 1));
        }

        if segments.iter().any(|s| CHROMIUM_BROWSERS.contains(s))
            || lower.contains("chrom")
            || lower.contains("brave")
            || lower.contains("vivaldi")
        {
            return Some((BrowserFamily::Chromium, i + 1));
        }

        break;
    }
    None
}

/// Extract Firefox-family profile flags: `-P <name>`, `-profile <path>`,
/// and the companion `-no-remote` flag needed for multi-profile sessions.
fn extract_firefox_profile(args: &[String]) -> Option<String> {
    let mut profile_args: Vec<String> = Vec::new();
    let mut no_remote = false;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];

        if arg == "-P" && i + 1 < args.len() {
            profile_args = vec!["-P".to_string(), args[i + 1].clone()];
            i += 2;
            continue;
        }

        if (arg == "--profile" || arg == "-profile") && i + 1 < args.len() {
            profile_args = vec!["-profile".to_string(), args[i + 1].clone()];
            i += 2;
            continue;
        }

        // `-Pname` (no space) — only valid for the short form
        if let Some(name) = arg.strip_prefix("-P")
            && !name.is_empty()
        {
            profile_args = vec!["-P".to_string(), name.to_string()];
        }

        if arg == "-no-remote" || arg == "--no-remote" {
            no_remote = true;
        }

        i += 1;
    }

    if profile_args.is_empty() {
        return None;
    }
    if no_remote {
        profile_args.insert(0, "-no-remote".to_string());
    }
    Some(join_profile_argv(&profile_args))
}

/// Extract Chromium-family profile flag: `--profile-directory=<name>`.
fn extract_chromium_profile(args: &[String]) -> Option<String> {
    args.iter()
        .find(|arg| arg.starts_with("--profile-directory="))
        .map(|arg| shell_quote(arg))
}

/// Join profile argv with the crate shell-word rule.
fn join_profile_argv(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(val: &str) -> String {
        val.to_string()
    }

    // --- Firefox family ---

    #[test]
    fn firefox_named_profile() {
        let args = vec![s("firefox"), s("-P"), s("work")];
        assert_eq!(detect_profile_from_args(&args), Some(s("-P work")));
    }

    #[test]
    fn firefox_named_profile_no_space() {
        let args = vec![s("firefox"), s("-Pwork")];
        assert_eq!(detect_profile_from_args(&args), Some(s("-P work")));
    }

    #[test]
    fn firefox_profile_path() {
        let args = vec![
            s("firefox"),
            s("-profile"),
            s("/home/user/.mozilla/firefox/abc.work"),
        ];
        assert_eq!(
            detect_profile_from_args(&args),
            Some(s("-profile /home/user/.mozilla/firefox/abc.work"))
        );
    }

    #[test]
    fn firefox_double_dash_profile() {
        let args = vec![
            s("firefox"),
            s("--profile"),
            s("/home/user/.mozilla/firefox/abc.work"),
        ];
        assert_eq!(
            detect_profile_from_args(&args),
            Some(s("-profile /home/user/.mozilla/firefox/abc.work"))
        );
    }

    #[test]
    fn firefox_no_remote_with_profile() {
        let args = vec![s("firefox"), s("-no-remote"), s("-P"), s("work")];
        assert_eq!(
            detect_profile_from_args(&args),
            Some(s("-no-remote -P work"))
        );
    }

    #[test]
    fn firefox_profile_with_no_remote_after() {
        let args = vec![s("firefox"), s("-P"), s("work"), s("-no-remote")];
        assert_eq!(
            detect_profile_from_args(&args),
            Some(s("-no-remote -P work"))
        );
    }

    #[test]
    fn firefox_bare_dash_p_ignored() {
        // `-P` without a value opens the profile manager — don't persist that
        let args = vec![s("firefox"), s("-P")];
        assert_eq!(detect_profile_from_args(&args), None);
    }

    #[test]
    fn firefox_no_profile_flags() {
        let args = vec![s("firefox"), s("--safe-mode")];
        assert_eq!(detect_profile_from_args(&args), None);
    }

    #[test]
    fn firefox_absolute_path_exe() {
        let args = vec![s("/usr/bin/firefox"), s("-P"), s("personal")];
        assert_eq!(detect_profile_from_args(&args), Some(s("-P personal")));
    }

    #[test]
    fn firefox_no_remote_without_profile_ignored() {
        let args = vec![s("firefox"), s("-no-remote")];
        assert_eq!(detect_profile_from_args(&args), None);
    }

    // --- Firefox-family variants ---

    #[test]
    fn floorp_profile() {
        let args = vec![s("floorp"), s("-P"), s("google")];
        assert_eq!(detect_profile_from_args(&args), Some(s("-P google")));
    }

    #[test]
    fn librewolf_profile() {
        let args = vec![s("librewolf"), s("-P"), s("default")];
        assert_eq!(detect_profile_from_args(&args), Some(s("-P default")));
    }

    #[test]
    fn waterfox_profile() {
        let args = vec![s("waterfox"), s("-P"), s("main")];
        assert_eq!(detect_profile_from_args(&args), Some(s("-P main")));
    }

    #[test]
    fn zen_profile() {
        let args = vec![s("zen"), s("-P"), s("dev")];
        assert_eq!(detect_profile_from_args(&args), Some(s("-P dev")));
    }

    // --- Chromium family ---

    #[test]
    fn chromium_profile_directory() {
        let args = vec![s("chromium"), s("--profile-directory=Profile 1")];
        assert_eq!(
            detect_profile_from_args(&args),
            Some(s("'--profile-directory=Profile 1'"))
        );
    }

    #[test]
    fn chrome_profile_directory() {
        let args = vec![s("google-chrome"), s("--profile-directory=Default")];
        assert_eq!(
            detect_profile_from_args(&args),
            Some(s("--profile-directory=Default"))
        );
    }

    #[test]
    fn brave_profile_directory() {
        let args = vec![s("brave-browser"), s("--profile-directory=Profile 2")];
        assert_eq!(
            detect_profile_from_args(&args),
            Some(s("'--profile-directory=Profile 2'"))
        );
    }

    #[test]
    fn chromium_no_profile_flag() {
        let args = vec![s("chromium"), s("--incognito")];
        assert_eq!(detect_profile_from_args(&args), None);
    }

    // --- Flatpak ---

    #[test]
    fn flatpak_firefox_profile() {
        let args = vec![
            s("flatpak"),
            s("run"),
            s("org.mozilla.firefox"),
            s("-P"),
            s("work"),
        ];
        assert_eq!(detect_profile_from_args(&args), Some(s("-P work")));
    }

    #[test]
    fn flatpak_firefox_with_flags() {
        let args = vec![
            s("/usr/bin/flatpak"),
            s("run"),
            s("--branch=stable"),
            s("org.mozilla.firefox"),
            s("-no-remote"),
            s("-P"),
            s("dev"),
        ];
        assert_eq!(
            detect_profile_from_args(&args),
            Some(s("-no-remote -P dev"))
        );
    }

    #[test]
    fn flatpak_zen_browser_profile() {
        let args = vec![
            s("flatpak"),
            s("run"),
            s("app.zen_browser.zen"),
            s("-P"),
            s("coding"),
        ];
        assert_eq!(detect_profile_from_args(&args), Some(s("-P coding")));
    }

    #[test]
    fn flatpak_chromium_profile() {
        let args = vec![
            s("flatpak"),
            s("run"),
            s("org.chromium.Chromium"),
            s("--profile-directory=Work"),
        ];
        assert_eq!(
            detect_profile_from_args(&args),
            Some(s("--profile-directory=Work"))
        );
    }

    #[test]
    fn flatpak_non_browser_ignored() {
        let args = vec![
            s("flatpak"),
            s("run"),
            s("org.gnome.Nautilus"),
            s("-P"),
            s("foo"),
        ];
        assert_eq!(detect_profile_from_args(&args), None);
    }

    // --- Single-arg cmdline (launcher wrappers) ---

    #[test]
    fn chromium_single_arg_cmdline() {
        let args = vec![s(
            "/usr/lib/chromium/chromium --profile-directory=WorkProfile",
        )];
        assert_eq!(
            detect_profile_from_args(&args),
            Some(s("--profile-directory=WorkProfile"))
        );
    }

    #[test]
    fn firefox_single_arg_cmdline() {
        let args = vec![s("/usr/lib/firefox/firefox -no-remote -P work")];
        assert_eq!(
            detect_profile_from_args(&args),
            Some(s("-no-remote -P work"))
        );
    }

    #[test]
    fn single_arg_non_browser_ignored() {
        let args = vec![s("/usr/bin/code --some-flag value")];
        assert_eq!(detect_profile_from_args(&args), None);
    }

    // --- Non-browsers ---

    #[test]
    fn non_browser_ignored() {
        let args = vec![s("code"), s("-P"), s("myproject")];
        assert_eq!(detect_profile_from_args(&args), None);
    }

    #[test]
    fn terminal_ignored() {
        let args = vec![s("ghostty"), s("--some-flag")];
        assert_eq!(detect_profile_from_args(&args), None);
    }

    #[test]
    fn empty_args() {
        assert_eq!(detect_profile_from_args(&[]), None);
    }

    #[test]
    fn detect_profile_from_args_round_trips_through_shell_words() {
        let firefox_multi = vec![s("firefox"), s("-no-remote"), s("-P"), s("dev")];
        let stored = detect_profile_from_args(&firefox_multi).unwrap();
        assert_eq!(
            crate::core::quote::shell_words(&stored).unwrap(),
            [s("-no-remote"), s("-P"), s("dev")]
        );

        let chromium = vec![s("chromium"), s("--profile-directory=Profile 1")];
        let stored = detect_profile_from_args(&chromium).unwrap();
        assert_eq!(
            crate::core::quote::shell_words(&stored).unwrap(),
            [s("--profile-directory=Profile 1")]
        );

        // Bare `-P work` stays one stored string; shell words are the two argv elements.
        let firefox_simple = vec![s("firefox"), s("-P"), s("work")];
        let stored = detect_profile_from_args(&firefox_simple).unwrap();
        assert_eq!(stored, "-P work");
        assert_eq!(
            crate::core::quote::shell_words(&stored).unwrap(),
            [s("-P"), s("work")]
        );
    }
}
