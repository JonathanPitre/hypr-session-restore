use crate::core::quote::{shell_quote, shell_words};
use crate::models::DesktopEntry;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct DesktopIndex {
    wm_class: HashMap<String, DesktopEntry>,
    desktop_id: HashMap<String, DesktopEntry>,
    exec_basename: HashMap<String, DesktopEntry>,
}

impl DesktopIndex {
    pub fn build() -> Self {
        Self::build_from_dirs(&Self::app_dirs())
    }

    pub fn build_from_dirs(dirs: &[PathBuf]) -> Self {
        let mut wm_class = HashMap::new();
        let mut desktop_id = HashMap::new();
        let mut exec_basename = HashMap::new();

        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };

            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "desktop")
                    && let Some(de) = parse_desktop_file(&path)
                {
                    if let Some(ref wmc) = de.startup_wm_class {
                        wm_class
                            .entry(wmc.to_lowercase())
                            .or_insert_with(|| de.clone());
                    }

                    desktop_id
                        .entry(de.desktop_id.to_lowercase())
                        .or_insert_with(|| de.clone());

                    if let Some(basename) = exec_basename_of(&de.exec) {
                        exec_basename
                            .entry(basename.to_lowercase())
                            .or_insert_with(|| de.clone());
                    }
                }
            }
        }

        Self {
            wm_class,
            desktop_id,
            exec_basename,
        }
    }

    pub fn len(&self) -> usize {
        self.desktop_id.len()
    }

    /// Look up a window class → `DesktopEntry` using the resolution chain.
    pub fn lookup(&self, class: &str) -> Option<&DesktopEntry> {
        let lower = class.to_lowercase();

        self.wm_class
            .get(&lower)
            .or_else(|| self.desktop_id.get(&lower))
            .or_else(|| self.exec_basename.get(&lower))
    }

    fn app_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();

        if let Some(data_home) = dirs::data_dir() {
            dirs.push(data_home.join("applications"));
        }

        if let Ok(xdg_dirs) = std::env::var("XDG_DATA_DIRS") {
            for dir in xdg_dirs.split(':') {
                dirs.push(PathBuf::from(dir).join("applications"));
            }
        } else {
            dirs.push(PathBuf::from("/usr/local/share/applications"));
            dirs.push(PathBuf::from("/usr/share/applications"));
        }

        // Flatpak export directories
        dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
        if let Some(data_home) = dirs::data_dir() {
            dirs.push(
                data_home
                    .join("flatpak")
                    .join("exports")
                    .join("share")
                    .join("applications"),
            );
        }

        dirs
    }
}

pub fn parse_desktop_file(path: &Path) -> Option<DesktopEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    parse_desktop_content(&content, path)
}

pub fn parse_desktop_content(content: &str, path: &Path) -> Option<DesktopEntry> {
    let mut exec = None;
    let mut startup_wm_class = None;
    let mut no_display = false;
    let mut in_desktop_entry = false;

    for line in content.lines() {
        let line = line.trim();

        if line.starts_with('[') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }

        if !in_desktop_entry {
            continue;
        }

        if let Some(val) = line.strip_prefix("Exec=") {
            exec = Some(clean_exec_line(val));
        } else if let Some(val) = line.strip_prefix("StartupWMClass=") {
            startup_wm_class = Some(val.to_string());
        } else if let Some(val) = line.strip_prefix("NoDisplay=") {
            no_display = val.trim().eq_ignore_ascii_case("true");
        }
    }

    let exec = exec?;

    if no_display && startup_wm_class.is_none() {
        return None;
    }

    let desktop_id = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let is_flatpak = exec.starts_with("/usr/bin/flatpak ")
        || exec.starts_with("flatpak run")
        || path
            .to_string_lossy()
            .contains("flatpak/exports/share/applications");

    let exec = if is_flatpak {
        simplify_flatpak_exec(&exec, &desktop_id).unwrap_or(exec)
    } else {
        exec
    };
    let exec = quote_stored_exec(&exec);

    Some(DesktopEntry {
        exec,
        startup_wm_class,
        desktop_id,
    })
}

/// Re-quote a cleaned Exec line so `shell_words` returns the same elements.
fn quote_stored_exec(exec: &str) -> String {
    let Ok(words) = shell_words(exec) else {
        return shell_quote(exec);
    };
    words
        .iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strip field codes (%f, %F, %u, %U, etc.), env var prefixes,
/// and Flatpak file-forwarding markers (@@, @@u, @@f, etc.) from Exec lines.
pub fn clean_exec_line(exec: &str) -> String {
    let parts: Vec<&str> = exec.split_whitespace().collect();

    // Find first non-env-var token (avoids O(n²) Vec::remove(0))
    let start = parts
        .iter()
        .position(|p| !p.contains('=') || p.starts_with('-'))
        .unwrap_or(parts.len());

    parts[start..]
        .iter()
        .filter(|p| !p.starts_with('%') && !p.starts_with("@@"))
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Simplify a verbose flatpak Exec line to just `flatpak run <app-id>`.
pub fn simplify_flatpak_exec(exec: &str, desktop_id: &str) -> Option<String> {
    let app_id = exec
        .split_whitespace()
        .find(|token| {
            token.contains('.')
                && !token.starts_with('-')
                && !token.starts_with('/')
                && !token.contains('=')
        })
        .unwrap_or(desktop_id);

    if app_id.contains('.') {
        Some(format!("flatpak run {app_id}"))
    } else {
        None
    }
}

/// Extract the basename of the executable from an Exec line.
pub fn exec_basename_of(exec: &str) -> Option<String> {
    let words = shell_words(exec).ok()?;
    let first_token = words.first()?;

    // Handle "flatpak run org.foo.Bar"
    if first_token.contains("flatpak")
        && let Some(app_id) = words.last()
        && app_id.contains('.')
        && !app_id.starts_with('-')
    {
        return Some(app_id.clone());
    }

    let basename = Path::new(first_token)
        .file_name()?
        .to_string_lossy()
        .to_string();

    Some(basename)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn stored_exec_with_literal_apostrophe_is_one_shell_word() {
        let exec = "/opt/GRMDB/app/O'Brien";
        assert_eq!(shell_words(&quote_stored_exec(exec)).unwrap(), [exec]);
    }

    // --- clean_exec_line ---

    #[test]
    fn clean_simple_command() {
        assert_eq!(clean_exec_line("firefox"), "firefox");
    }

    #[test]
    fn clean_strips_field_codes() {
        assert_eq!(clean_exec_line("firefox %u"), "firefox");
        assert_eq!(clean_exec_line("nautilus %U"), "nautilus");
        assert_eq!(clean_exec_line("code %F"), "code");
    }

    #[test]
    fn clean_strips_env_vars() {
        assert_eq!(
            clean_exec_line("MOZ_ENABLE_WAYLAND=1 firefox %u"),
            "firefox"
        );
        assert_eq!(
            clean_exec_line("GDK_BACKEND=wayland MOZ_ENABLE_WAYLAND=1 firefox"),
            "firefox"
        );
    }

    #[test]
    fn clean_preserves_flags() {
        assert_eq!(
            clean_exec_line("/usr/bin/ghostty --gtk-single-instance=true"),
            "/usr/bin/ghostty --gtk-single-instance=true"
        );
    }

    #[test]
    fn clean_strips_flatpak_markers() {
        assert_eq!(
            clean_exec_line("/usr/bin/flatpak run --branch=stable app.zen_browser.zen @@u @@"),
            "/usr/bin/flatpak run --branch=stable app.zen_browser.zen"
        );
    }

    #[test]
    fn clean_empty_string() {
        assert_eq!(clean_exec_line(""), "");
    }

    // --- simplify_flatpak_exec ---

    #[test]
    fn simplify_flatpak_with_flags() {
        let exec = "/usr/bin/flatpak run --branch=stable --arch=x86_64 --command=launch-script.sh --file-forwarding app.zen_browser.zen";
        assert_eq!(
            simplify_flatpak_exec(exec, "app.zen_browser.zen"),
            Some("flatpak run app.zen_browser.zen".to_string())
        );
    }

    #[test]
    fn simplify_already_simple() {
        let exec = "flatpak run org.mozilla.firefox";
        assert_eq!(
            simplify_flatpak_exec(exec, "org.mozilla.firefox"),
            Some("flatpak run org.mozilla.firefox".to_string())
        );
    }

    #[test]
    fn simplify_fallback_to_desktop_id() {
        assert_eq!(
            simplify_flatpak_exec("flatpak run --some-flag", "org.foo.Bar"),
            Some("flatpak run org.foo.Bar".to_string())
        );
    }

    #[test]
    fn simplify_no_dots_in_id() {
        assert_eq!(simplify_flatpak_exec("flatpak run --flag", "nodots"), None);
    }

    // --- exec_basename_of ---

    #[test]
    fn basename_simple() {
        assert_eq!(exec_basename_of("firefox"), Some("firefox".to_string()));
    }

    #[test]
    fn basename_absolute_path() {
        assert_eq!(
            exec_basename_of("/usr/bin/firefox"),
            Some("firefox".to_string())
        );
    }

    #[test]
    fn basename_with_args() {
        assert_eq!(
            exec_basename_of("/usr/bin/ghostty --flag"),
            Some("ghostty".to_string())
        );
    }

    #[test]
    fn basename_flatpak_extracts_app_id() {
        assert_eq!(
            exec_basename_of("flatpak run org.mozilla.firefox"),
            Some("org.mozilla.firefox".to_string())
        );
    }

    #[test]
    fn basename_flatpak_only_flags() {
        assert_eq!(
            exec_basename_of("/usr/bin/flatpak run --flag"),
            Some("flatpak".to_string())
        );
    }

    #[test]
    fn basename_empty() {
        assert_eq!(exec_basename_of(""), None);
    }

    // --- parse_desktop_content ---

    fn make_desktop(content: &str) -> Option<DesktopEntry> {
        parse_desktop_content(content, Path::new("/fake/test-app.desktop"))
    }

    #[test]
    fn parse_standard_desktop_file() {
        let entry = make_desktop(
            "[Desktop Entry]\n\
             Name=Firefox\n\
             Exec=firefox %u\n\
             StartupWMClass=firefox\n\
             Terminal=false\n\
             Type=Application\n",
        )
        .unwrap();

        assert_eq!(entry.exec, "firefox");
        assert_eq!(entry.startup_wm_class.as_deref(), Some("firefox"));
        assert_eq!(entry.desktop_id, "test-app");
    }

    #[test]
    fn parse_no_display_without_wm_class_excluded() {
        let result = make_desktop(
            "[Desktop Entry]\n\
             Name=Hidden\n\
             Exec=hidden-thing\n\
             NoDisplay=true\n",
        );
        assert!(result.is_none());
    }

    #[test]
    fn parse_no_display_with_wm_class_included() {
        let entry = make_desktop(
            "[Desktop Entry]\n\
             Name=Special\n\
             Exec=special\n\
             NoDisplay=true\n\
             StartupWMClass=special\n",
        )
        .unwrap();
        assert_eq!(entry.exec, "special");
    }

    #[test]
    fn parse_no_exec_line() {
        let result = make_desktop(
            "[Desktop Entry]\n\
             Name=NoExec\n\
             Terminal=false\n",
        );
        assert!(result.is_none());
    }

    #[test]
    fn parse_ignores_other_sections() {
        let entry = make_desktop(
            "[Desktop Entry]\n\
             Name=MyApp\n\
             Exec=myapp\n\
             \n\
             [Desktop Action new-window]\n\
             Name=New Window\n\
             Exec=myapp --new-window\n",
        )
        .unwrap();

        assert_eq!(entry.exec, "myapp");
    }

    #[test]
    fn double_quoted_exec_value_round_trips() {
        let entry = make_desktop(
            "[Desktop Entry]\n\
             Name=My App\n\
             Exec=myapp --title \"Hello World\" %u\n",
        )
        .unwrap();

        assert_eq!(entry.exec, "myapp --title 'Hello World'");
        assert_eq!(
            crate::core::quote::shell_words(&entry.exec).unwrap(),
            ["myapp", "--title", "Hello World"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_flatpak_desktop_file() {
        let entry = parse_desktop_content(
            "[Desktop Entry]\n\
             Name=Zen Browser\n\
             Exec=/usr/bin/flatpak run --branch=stable --arch=x86_64 --command=launch-script.sh --file-forwarding app.zen_browser.zen @@u @@\n\
             StartupWMClass=app.zen_browser.zen\n",
            Path::new("/var/lib/flatpak/exports/share/applications/app.zen_browser.zen.desktop"),
        )
        .unwrap();

        assert_eq!(entry.exec, "flatpak run app.zen_browser.zen");
    }

    // --- DesktopIndex::build_from_dirs ---

    #[test]
    fn index_from_temp_dir() {
        let dir = tempfile::tempdir().unwrap();

        let firefox_path = dir.path().join("firefox.desktop");
        let mut f = std::fs::File::create(&firefox_path).unwrap();
        writeln!(
            f,
            "[Desktop Entry]\nName=Firefox\nExec=firefox %u\nStartupWMClass=firefox"
        )
        .unwrap();

        let nautilus_path = dir.path().join("org.gnome.Nautilus.desktop");
        let mut f = std::fs::File::create(&nautilus_path).unwrap();
        writeln!(
            f,
            "[Desktop Entry]\nName=Files\nExec=nautilus\nStartupWMClass=org.gnome.Nautilus"
        )
        .unwrap();

        let index = DesktopIndex::build_from_dirs(&[dir.path().to_path_buf()]);

        assert_eq!(index.len(), 2);

        // Lookup by WM class
        let entry = index.lookup("firefox").unwrap();
        assert_eq!(entry.exec, "firefox");

        // Lookup by desktop ID
        let entry = index.lookup("org.gnome.Nautilus").unwrap();
        assert_eq!(entry.exec, "nautilus");
    }

    #[test]
    fn index_lookup_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("MyApp.desktop");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "[Desktop Entry]\nName=My App\nExec=myapp\nStartupWMClass=MyApp"
        )
        .unwrap();

        let index = DesktopIndex::build_from_dirs(&[dir.path().to_path_buf()]);

        assert!(index.lookup("myapp").is_some());
        assert!(index.lookup("MyApp").is_some());
        assert!(index.lookup("MYAPP").is_some());
    }

    #[test]
    fn index_lookup_by_exec_basename() {
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("com.example.app.desktop");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "[Desktop Entry]\nName=Example\nExec=/usr/bin/example-app --daemon"
        )
        .unwrap();

        let index = DesktopIndex::build_from_dirs(&[dir.path().to_path_buf()]);

        // No WM class match, no desktop ID match "example-app", but exec basename match
        assert!(index.lookup("example-app").is_some());
    }

    #[test]
    fn index_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let index = DesktopIndex::build_from_dirs(&[dir.path().to_path_buf()]);
        assert_eq!(index.len(), 0);
        assert!(index.lookup("anything").is_none());
    }

    #[test]
    fn index_nonexistent_dir() {
        let index = DesktopIndex::build_from_dirs(&[PathBuf::from("/nonexistent/path")]);
        assert_eq!(index.len(), 0);
    }
}
