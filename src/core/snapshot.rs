use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::core::state::StateManager;
use crate::core::store::{MAX_SNAPSHOT_BYTES, PrivateDir};
use crate::models::{SessionFile, SessionMeta, TrackedWindow, WindowEntry};

const MAX_WINDOWS: usize = 512;
const MAX_SESSION_NAME_LEN: usize = 128;

pub struct SnapshotEngine {
    dir: PrivateDir,
    per_window_launch: bool,
}

pub struct SessionSummary {
    pub name: String,
    pub timestamp: i64,
    pub pinned: bool,
}

fn validate_window_count(count: usize) -> Result<()> {
    if count > MAX_WINDOWS {
        bail!("session has {count} windows (limit is {MAX_WINDOWS})");
    }
    Ok(())
}

/// `^[A-Za-z0-9][A-Za-z0-9._ -]{0,127}$`, with `..` and trailing dots rejected.
fn sanitize_session_name(name: &str) -> Result<&str> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("session name cannot be empty");
    };
    if !first.is_ascii_alphanumeric() {
        bail!("invalid session name '{name}'");
    }
    let mut len = 1usize;
    for ch in chars {
        len += 1;
        if len > MAX_SESSION_NAME_LEN
            || !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ' ' | '-'))
        {
            bail!("invalid session name '{name}'");
        }
    }
    if name.contains("..") || name.ends_with('.') {
        bail!("invalid session name '{name}'");
    }
    Ok(name)
}

fn session_leaf(name: &str) -> String {
    format!("{name}.toml")
}

fn predecessor_leaf(name: &str) -> String {
    format!(".{name}.toml.prev")
}

const PIN_LEAF: &str = ".pin";

fn resolve_cwds_for_windows(windows: &[&TrackedWindow]) -> Vec<Option<String>> {
    use std::collections::HashMap;

    let mut pid_cwds: HashMap<i64, Vec<String>> = HashMap::new();
    let mut pid_cursor: HashMap<i64, usize> = HashMap::new();

    for w in windows {
        if w.pid > 0 {
            pid_cwds
                .entry(w.pid)
                .or_insert_with(|| crate::resolver::cwd::resolve_all_shell_cwds(w.pid));
        }
    }

    windows
        .iter()
        .map(|w| {
            if w.pid <= 0 {
                return None;
            }
            let cwds = pid_cwds.get(&w.pid)?;
            let idx = pid_cursor.entry(w.pid).or_insert(0);
            let cwd = cwds.get(*idx).cloned();
            *idx += 1;
            cwd
        })
        .collect()
}

fn window_to_entry(w: &TrackedWindow, cwd: Option<String>) -> WindowEntry {
    WindowEntry {
        app_id: w.app_id.clone(),
        launch_cmd: w.launch_cmd.clone(),
        workspace: w.workspace.clone(),
        monitor: Some(w.monitor.clone()).filter(|s| !s.is_empty()),
        floating: w.floating,
        fullscreen: w.fullscreen,
        position: Some(w.position),
        size: Some(w.size),
        cwd,
        profile: w.profile.clone(),
    }
}

impl SnapshotEngine {
    pub fn new(config: &Config) -> Result<Self> {
        Self::open(config, false)
    }

    /// `readonly` skips directory creation and mode repair.
    pub fn open(config: &Config, readonly: bool) -> Result<Self> {
        let session_dir = config.session_dir();
        let dir = PrivateDir::open(&session_dir, readonly)?;

        Ok(Self {
            dir,
            per_window_launch: config.general.per_window_launch,
        })
    }

    pub fn save(&self, state: &StateManager, name: &str) -> Result<PathBuf> {
        let name = sanitize_session_name(name)?;
        let count = state.window_count();
        if count == 0 {
            bail!("refusing to save an empty capture");
        }
        validate_window_count(count)?;
        let eligible = self.capture_windows(state);
        if eligible.is_empty() {
            bail!(
                "refusing to save an empty capture: no tracked window had a resolvable launch command"
            );
        }

        let cwds = resolve_cwds_for_windows(&eligible);
        let entries: Vec<WindowEntry> = eligible
            .iter()
            .zip(cwds)
            .map(|(w, cwd)| window_to_entry(w, cwd))
            .collect();

        let session_file = SessionFile {
            session: SessionMeta {
                name: name.to_string(),
                timestamp: chrono::Utc::now().timestamp(),
            },
            windows: entries,
        };

        let content =
            toml::to_string_pretty(&session_file).context("serializing session to TOML")?;

        if u64::try_from(content.len()).unwrap_or(u64::MAX) > MAX_SNAPSHOT_BYTES {
            bail!("serialized session exceeds {MAX_SNAPSHOT_BYTES} bytes");
        }

        let leaf = session_leaf(name);
        self.preserve_predecessor(name)?;
        self.dir
            .atomic_write(&leaf, content.as_bytes())
            .with_context(|| format!("writing session '{name}'"))?;
        let final_path = self.dir.path().join(&leaf);

        tracing::info!(
            "saved session '{name}' ({} apps) to {}",
            session_file.windows.len(),
            final_path.display()
        );

        Ok(final_path)
    }

    pub const fn per_window_launch(&self) -> bool {
        self.per_window_launch
    }

    pub fn capture_windows<'a>(&self, state: &'a StateManager) -> Vec<&'a TrackedWindow> {
        let mut seen = std::collections::HashSet::new();
        state
            .windows()
            .into_iter()
            .filter(|w| {
                !w.launch_cmd.is_empty()
                    && (self.per_window_launch || seen.insert((&w.app_id, &w.profile)))
            })
            .collect()
    }

    pub fn load(&self, name: &str) -> Result<SessionFile> {
        let name = sanitize_session_name(name)?;
        let leaf = session_leaf(name);
        let path = self.dir.path().join(&leaf);
        let bytes = self
            .dir
            .read_regular(&leaf)?
            .with_context(|| format!("reading session file {}", path.display()))?;
        let content = String::from_utf8(bytes)
            .with_context(|| format!("session file {} is not utf-8", path.display()))?;
        let session: SessionFile =
            toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;
        validate_window_count(session.windows.len())?;
        Ok(session)
    }

    pub fn list(&self) -> Result<Vec<SessionSummary>> {
        let pinned = self.pinned_name().ok().flatten();
        let mut sessions = Vec::new();

        let names = self
            .dir
            .list_names()
            .with_context(|| format!("reading session dir {}", self.dir.path().display()))?;

        for name in names {
            if name.starts_with('.')
                || !Path::new(&name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
            {
                continue;
            }
            let Some(stem) = Path::new(&name).file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(Some(bytes)) = self.dir.read_regular(&name) else {
                continue;
            };
            let Ok(content) = String::from_utf8(bytes) else {
                continue;
            };
            if let Ok(session) = toml::from_str::<SessionFile>(&content) {
                sessions.push(SessionSummary {
                    name: stem.to_string(),
                    timestamp: session.session.timestamp,
                    pinned: pinned.as_deref() == Some(stem),
                });
            }
        }

        sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        Ok(sessions)
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        let name = sanitize_session_name(name)?;
        if self.pinned_name()?.as_deref() == Some(name) {
            self.clear_pin()?;
        }
        let leaf = session_leaf(name);
        let path = self.dir.path().join(&leaf);
        if self.dir.exists_regular(&leaf) {
            self.dir
                .unlink(&leaf)
                .with_context(|| format!("deleting session {}", path.display()))?;
            tracing::info!("deleted session '{name}'");
        } else {
            tracing::warn!("session '{name}' not found at {}", path.display());
        }
        self.dir
            .unlink(&predecessor_leaf(name))
            .with_context(|| format!("deleting predecessor for '{name}'"))?;
        Ok(())
    }

    pub fn exists(&self, name: &str) -> bool {
        sanitize_session_name(name)
            .ok()
            .is_some_and(|name| self.dir.exists_regular(&session_leaf(name)))
    }

    pub fn session_dir(&self) -> &Path {
        self.dir.path()
    }

    /// Pin `name`. The session file must already exist.
    pub fn pin(&self, name: &str) -> Result<()> {
        let name = sanitize_session_name(name)?;
        if !self.dir.exists_regular(&session_leaf(name)) {
            bail!("session '{name}' not found");
        }
        self.dir
            .atomic_write(PIN_LEAF, format!("{name}\n").as_bytes())
            .context("writing pin")?;
        Ok(())
    }

    pub fn clear_pin(&self) -> Result<()> {
        self.dir.unlink(PIN_LEAF).context("clearing pin")
    }

    pub fn pinned_name(&self) -> Result<Option<String>> {
        let Some(bytes) = self.dir.read_regular(PIN_LEAF)? else {
            return Ok(None);
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!("ignoring invalid pin: {err}");
                return Ok(None);
            }
        };
        let name = text.strip_suffix('\n').unwrap_or(&text);
        if name.is_empty() {
            return Ok(None);
        }
        match sanitize_session_name(name) {
            Ok(name) => Ok(Some(name.to_string())),
            Err(err) => {
                tracing::warn!("ignoring invalid pin: {err}");
                Ok(None)
            }
        }
    }

    /// `Some(name)` must exist. `None` is the pin, or the newest session.
    pub fn resolve_restore_target(&self, explicit: Option<&str>) -> Result<String> {
        if let Some(name) = explicit {
            let name = sanitize_session_name(name)?;
            if !self.exists(name) {
                bail!(
                    "session '{name}' not found in {}",
                    self.dir.path().display()
                );
            }
            return Ok(name.to_string());
        }
        if let Some(name) = self.pinned_name()? {
            if !self.exists(&name) {
                bail!("pinned session '{name}' not found");
            }
            return Ok(name);
        }
        self.list()?
            .into_iter()
            .next()
            .map(|session| session.name)
            .context("no saved session")
    }

    /// `Some(name)` is returned unchanged and does not touch the pin.
    /// `None` is the pinned session, or `"last"`.
    pub fn save_target(&self, explicit: Option<&str>) -> Result<String> {
        if let Some(name) = explicit {
            return Ok(name.to_string());
        }
        Ok(self.pinned_name()?.unwrap_or_else(|| "last".to_string()))
    }

    fn preserve_predecessor(&self, name: &str) -> Result<()> {
        let leaf = session_leaf(name);
        let Some(bytes) = self.dir.read_regular(&leaf)? else {
            return Ok(());
        };
        self.dir
            .atomic_write(&predecessor_leaf(name), &bytes)
            .with_context(|| format!("writing predecessor for '{name}'"))?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn new_with_dir(session_dir: &Path, per_window_launch: bool) -> Result<Self> {
        let dir = PrivateDir::open(session_dir, false)?;
        Ok(Self {
            dir,
            per_window_launch,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn invalid_pin_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();
        for content in [b"../invalid\n".as_slice(), b"work\nextra", b"\xff"] {
            std::fs::write(dir.path().join(".pin"), content).unwrap();
            assert_eq!(engine.pinned_name().unwrap(), None);
        }
    }

    use super::*;
    use crate::config::Config;

    fn make_state_with_windows(windows: Vec<(&str, &str, &str, bool)>) -> StateManager {
        let mut config = Config::default();
        config.general.session_dir = "/tmp/unused".to_string();
        let mut state = StateManager::new(&config);

        for (addr, app_id, workspace, floating) in windows {
            state.add(TrackedWindow {
                address: addr.to_string(),
                app_id: app_id.to_string(),
                launch_cmd: format!("{app_id}-cmd"),
                workspace: workspace.to_string(),
                monitor: String::new(),
                position: (100, 200),
                size: (800, 600),
                floating,
                fullscreen: false,
                pid: 0,
                profile: None,
            });
        }

        state
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        let state = make_state_with_windows(vec![
            ("0xa", "firefox", "1", false),
            ("0xb", "code", "2", false),
        ]);

        let path = engine.save(&state, "test").unwrap();
        assert!(path.exists());

        let loaded = engine.load("test").unwrap();
        assert_eq!(loaded.session.name, "test");
        assert!(loaded.session.timestamp > 0);
        assert_eq!(loaded.windows.len(), 2);
    }

    #[test]
    fn save_deduplicates_by_app_id() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        let state = make_state_with_windows(vec![
            ("0xa", "firefox", "1", false),
            ("0xb", "firefox", "2", false),
            ("0xc", "code", "3", false),
        ]);

        engine.save(&state, "dedup").unwrap();
        let loaded = engine.load("dedup").unwrap();

        assert_eq!(loaded.windows.len(), 2);
        let app_ids: Vec<&str> = loaded.windows.iter().map(|w| w.app_id.as_str()).collect();
        assert!(app_ids.contains(&"firefox"));
        assert!(app_ids.contains(&"code"));
    }

    #[test]
    fn save_dedup_distinguishes_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        let mut config = Config::default();
        config.general.session_dir = "/tmp/unused".to_string();
        let mut state = StateManager::new(&config);

        state.add(TrackedWindow {
            address: "0xa".to_string(),
            app_id: "firefox".to_string(),
            launch_cmd: "firefox".to_string(),
            workspace: "1".to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (1920, 1080),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: Some("-P work".to_string()),
        });
        state.add(TrackedWindow {
            address: "0xb".to_string(),
            app_id: "firefox".to_string(),
            launch_cmd: "firefox".to_string(),
            workspace: "2".to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (1920, 1080),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: Some("-P personal".to_string()),
        });
        state.add(TrackedWindow {
            address: "0xc".to_string(),
            app_id: "firefox".to_string(),
            launch_cmd: "firefox".to_string(),
            workspace: "3".to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (1920, 1080),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: Some("-P work".to_string()),
        });

        engine.save(&state, "profiles").unwrap();
        let loaded = engine.load("profiles").unwrap();

        // Two distinct profiles: "-P work" and "-P personal"
        assert_eq!(loaded.windows.len(), 2);
        let profiles: Vec<Option<&str>> = loaded
            .windows
            .iter()
            .map(|w| w.profile.as_deref())
            .collect();
        assert!(profiles.contains(&Some("-P work")));
        assert!(profiles.contains(&Some("-P personal")));
    }

    #[test]
    fn profile_field_roundtrips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), true).unwrap();

        let mut config = Config::default();
        config.general.session_dir = "/tmp/unused".to_string();
        let mut state = StateManager::new(&config);

        state.add(TrackedWindow {
            address: "0xa".to_string(),
            app_id: "firefox".to_string(),
            launch_cmd: "firefox".to_string(),
            workspace: "1".to_string(),
            monitor: String::new(),
            position: (100, 200),
            size: (800, 600),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: Some("-no-remote -P dev".to_string()),
        });
        state.add(TrackedWindow {
            address: "0xb".to_string(),
            app_id: "chromium".to_string(),
            launch_cmd: "chromium".to_string(),
            workspace: "2".to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (1920, 1080),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: Some("--profile-directory=Profile 1".to_string()),
        });
        state.add(TrackedWindow {
            address: "0xc".to_string(),
            app_id: "code".to_string(),
            launch_cmd: "code".to_string(),
            workspace: "3".to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (1920, 1080),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: None,
        });

        engine.save(&state, "profile-rt").unwrap();
        let loaded = engine.load("profile-rt").unwrap();

        assert_eq!(loaded.windows.len(), 3);

        let firefox = loaded
            .windows
            .iter()
            .find(|w| w.app_id == "firefox")
            .unwrap();
        assert_eq!(firefox.profile.as_deref(), Some("-no-remote -P dev"));

        let chromium = loaded
            .windows
            .iter()
            .find(|w| w.app_id == "chromium")
            .unwrap();
        assert_eq!(
            chromium.profile.as_deref(),
            Some("--profile-directory=Profile 1")
        );

        let code = loaded.windows.iter().find(|w| w.app_id == "code").unwrap();
        assert!(code.profile.is_none());
    }

    #[test]
    fn save_per_window_launch_keeps_all() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), true).unwrap();

        let state = make_state_with_windows(vec![
            ("0xa", "firefox", "1", false),
            ("0xb", "firefox", "2", false),
        ]);

        engine.save(&state, "perwin").unwrap();
        let loaded = engine.load("perwin").unwrap();
        assert_eq!(loaded.windows.len(), 2);
    }

    #[test]
    fn save_refuses_capture_without_resolvable_launch_commands() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        let mut config = Config::default();
        config.general.session_dir = "/tmp/unused".to_string();
        let mut state = StateManager::new(&config);
        state.add(TrackedWindow {
            address: "0xa".to_string(),
            app_id: "unknown".to_string(),
            launch_cmd: String::new(),
            workspace: "1".to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (0, 0),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: None,
        });

        let err = engine.save(&state, "empty").unwrap_err();
        assert!(
            err.to_string()
                .contains("no tracked window had a resolvable launch command"),
            "{err}"
        );
        assert!(!engine.exists("empty"));
    }

    #[test]
    fn save_floating_includes_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        let state = make_state_with_windows(vec![("0xa", "nautilus", "1", true)]);

        engine.save(&state, "float").unwrap();
        let loaded = engine.load("float").unwrap();

        let win = &loaded.windows[0];
        assert!(win.floating);
        assert_eq!(win.position, Some((100, 200)));
        assert_eq!(win.size, Some((800, 600)));
    }

    #[test]
    fn save_non_floating_includes_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);

        engine.save(&state, "nonfloat").unwrap();
        let loaded = engine.load("nonfloat").unwrap();

        let win = &loaded.windows[0];
        assert!(!win.floating);
        assert_eq!(win.position, Some((100, 200)));
        assert_eq!(win.size, Some((800, 600)));
    }

    #[test]
    fn exists_true_after_save() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        assert!(!engine.exists("test"));

        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        engine.save(&state, "test").unwrap();

        assert!(engine.exists("test"));
    }

    #[test]
    fn delete_removes_session() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        engine.save(&state, "deleteme").unwrap();

        assert!(engine.exists("deleteme"));
        engine.delete("deleteme").unwrap();
        assert!(!engine.exists("deleteme"));
    }

    #[test]
    fn delete_unlinks_predecessor_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), true).unwrap();
        let first = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        engine.save(&first, "work").unwrap();
        let second = make_state_with_windows(vec![
            ("0xa", "firefox", "1", false),
            ("0xb", "code", "2", false),
        ]);
        engine.save(&second, "work").unwrap();
        assert!(
            engine
                .dir
                .read_regular(".work.toml.prev")
                .unwrap()
                .is_some()
        );

        engine.delete("work").unwrap();
        assert!(!engine.exists("work"));
        assert!(
            engine
                .dir
                .read_regular(".work.toml.prev")
                .unwrap()
                .is_none()
        );

        engine
            .dir
            .atomic_write(".orphan.toml.prev", b"stale")
            .unwrap();
        engine.delete("orphan").unwrap();
        assert!(
            engine
                .dir
                .read_regular(".orphan.toml.prev")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn delete_nonexistent_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();
        engine.delete("nonexistent").unwrap();
    }

    #[test]
    fn list_returns_sessions_sorted_by_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        std::fs::write(
            dir.path().join("first.toml"),
            "[session]\nname = \"first\"\ntimestamp = 1000\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("second.toml"),
            "[session]\nname = \"second\"\ntimestamp = 2000\n",
        )
        .unwrap();

        let sessions = engine.list().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "second");
        assert_eq!(sessions[1].name, "first");
    }

    #[test]
    fn list_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        let sessions = engine.list().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn restore_fallback_uses_file_stem_instead_of_embedded_name() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();
        std::fs::write(
            dir.path().join("copy.toml"),
            "[session]\nname = \"original\"\ntimestamp = 1000\n",
        )
        .unwrap();

        let target = engine.resolve_restore_target(None).unwrap();
        assert_eq!(target, "copy");
        assert_eq!(engine.load(&target).unwrap().session.name, "original");
    }

    #[test]
    fn copied_session_is_listed_and_pinned_by_its_own_stem() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();
        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        let original = engine.save(&state, "original").unwrap();
        std::fs::copy(original, dir.path().join("copy.toml")).unwrap();
        engine.pin("copy").unwrap();

        let sessions = engine.list().unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|row| row.name == "copy" && row.pinned));
        assert!(
            sessions
                .iter()
                .any(|row| row.name == "original" && !row.pinned)
        );
    }

    #[test]
    fn list_ignores_hidden_files() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        engine.save(&state, "visible").unwrap();

        std::fs::write(dir.path().join(".interrupted.toml.tmp"), "garbage").unwrap();

        let sessions = engine.list().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "visible");
    }

    #[test]
    fn load_nonexistent_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        let result = engine.load("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn loaded_session_window_count_is_bounded() {
        validate_window_count(MAX_WINDOWS).unwrap();
        let err = validate_window_count(MAX_WINDOWS + 1).unwrap_err();
        assert!(err.to_string().contains("limit is 512"), "{err}");
    }

    #[test]
    fn save_rejects_session_name_ending_in_dot() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();
        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        let err = engine.save(&state, "a.").unwrap_err();
        assert!(
            err.to_string().contains("invalid session name 'a.'"),
            "{err}"
        );
    }

    #[test]
    fn save_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        engine.save(&state, "atomic").unwrap();

        let tmp_path = dir.path().join(".atomic.toml.tmp");
        assert!(!tmp_path.exists());

        let final_path = dir.path().join("atomic.toml");
        assert!(final_path.exists());
    }

    #[test]
    fn rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();

        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);

        assert!(engine.save(&state, "../escape").is_err());
        assert!(engine.save(&state, "../../etc/cron.d/pwned").is_err());
        assert!(engine.save(&state, ".hidden").is_err());
        assert!(engine.save(&state, "foo/bar").is_err());
        assert!(engine.save(&state, "").is_err());
        assert!(engine.load("../etc/passwd").is_err());
        assert!(engine.delete("../../nope").is_err());
        assert!(!engine.exists("../escape"));
    }

    #[test]
    fn rejects_empty_capture() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();
        let mut config = Config::default();
        config.general.session_dir = "/tmp/unused".to_string();
        let state = StateManager::new(&config);

        assert!(engine.save(&state, "empty-capture").is_err());
        assert!(!engine.exists("empty-capture"));
    }

    #[test]
    fn rejects_too_many_windows() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), true).unwrap();
        let mut config = Config::default();
        config.general.session_dir = "/tmp/unused".to_string();
        let mut state = StateManager::new(&config);
        for i in 0..=MAX_WINDOWS {
            state.add(TrackedWindow {
                address: format!("0x{i:x}"),
                app_id: format!("app{i}"),
                launch_cmd: format!("cmd{i}"),
                workspace: "1".to_string(),
                monitor: String::new(),
                position: (0, 0),
                size: (1, 1),
                floating: false,
                fullscreen: false,
                pid: 0,
                profile: None,
            });
        }

        assert!(engine.save(&state, "too-many").is_err());
        assert!(!engine.exists("too-many"));
    }

    #[test]
    fn session_name_charset_and_length() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), true).unwrap();
        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);

        assert!(engine.save(&state, "good.name_1-ok").is_ok());
        assert!(engine.save(&state, "my session").is_ok());
        assert!(
            engine
                .save(&state, &"a".repeat(MAX_SESSION_NAME_LEN))
                .is_ok()
        );
        assert!(
            engine
                .save(&state, &"a".repeat(MAX_SESSION_NAME_LEN + 1))
                .is_err()
        );
        assert!(engine.save(&state, "a..b").is_err());
        assert!(engine.save(&state, "-lead").is_err());
        assert!(engine.save(&state, "semi;colon").is_err());
        assert!(engine.save(&state, "tab\there").is_err());
        assert!(engine.save(&state, ".hidden").is_err());
    }

    #[test]
    fn second_save_keeps_predecessor_and_list_length() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), true).unwrap();
        let first = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        engine.save(&first, "work").unwrap();
        let original = engine.dir.read_regular("work.toml").unwrap().unwrap();

        let second = make_state_with_windows(vec![
            ("0xa", "firefox", "1", false),
            ("0xb", "code", "2", false),
        ]);
        engine.save(&second, "work").unwrap();

        let prev = engine.dir.read_regular(".work.toml.prev").unwrap().unwrap();
        assert_eq!(prev, original);
        assert_eq!(engine.load("work").unwrap().windows.len(), 2);
        let listed = engine.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "work");
    }

    #[test]
    fn pin_roundtrip_marks_list_and_delete_unpins() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();
        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        engine.save(&state, "work").unwrap();

        assert!(engine.pinned_name().unwrap().is_none());
        assert!(engine.pin("missing").is_err());
        engine.pin("work").unwrap();
        assert_eq!(engine.pinned_name().unwrap().as_deref(), Some("work"));
        let sessions = engine.list().unwrap();
        assert_eq!(sessions[0].name, "work");
        assert!(sessions[0].pinned);

        engine.delete("work").unwrap();
        assert!(!engine.exists("work"));
        assert!(engine.pinned_name().unwrap().is_none());
    }

    #[test]
    fn resolve_prefers_pin_over_newer_session() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();
        std::fs::write(
            dir.path().join("oldpin.toml"),
            "[session]\nname = \"oldpin\"\ntimestamp = 1000\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("newer.toml"),
            "[session]\nname = \"newer\"\ntimestamp = 9000\n",
        )
        .unwrap();

        engine.pin("oldpin").unwrap();
        assert_eq!(engine.resolve_restore_target(None).unwrap(), "oldpin");
        engine.clear_pin().unwrap();
        assert_eq!(engine.resolve_restore_target(None).unwrap(), "newer");
        assert_eq!(
            engine.resolve_restore_target(Some("oldpin")).unwrap(),
            "oldpin"
        );
        assert!(engine.resolve_restore_target(Some("missing")).is_err());
    }

    #[test]
    fn save_target_uses_pin_only_when_unnamed() {
        let dir = tempfile::tempdir().unwrap();
        let engine = SnapshotEngine::new_with_dir(dir.path(), false).unwrap();
        let state = make_state_with_windows(vec![("0xa", "firefox", "1", false)]);
        engine.save(&state, "work").unwrap();

        assert_eq!(engine.save_target(None).unwrap(), "last");
        assert_eq!(engine.save_target(Some("custom")).unwrap(), "custom");
        assert!(engine.pinned_name().unwrap().is_none());

        engine.pin("work").unwrap();
        assert_eq!(engine.save_target(None).unwrap(), "work");
        assert_eq!(engine.save_target(Some("other")).unwrap(), "other");
        assert_eq!(engine.pinned_name().unwrap().as_deref(), Some("work"));
    }
}
