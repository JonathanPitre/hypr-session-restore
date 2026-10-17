use std::collections::HashMap;

use regex::Regex;

use crate::config::Config;
use crate::models::{HyprClient, TrackedWindow};

pub struct StateManager {
    windows: HashMap<String, TrackedWindow>,
    exclude_patterns: Vec<Regex>,
    include_patterns: Vec<Regex>,
}

pub fn normalize_address(addr: &str) -> String {
    addr.trim_start_matches("0x").to_lowercase()
}

impl StateManager {
    pub fn new(config: &Config) -> Self {
        let exclude_patterns = config
            .rules
            .exclude
            .iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!("invalid exclude pattern '{p}': {e}");
                    None
                }
            })
            .collect();

        let include_patterns = config
            .rules
            .include
            .iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!("invalid include pattern '{p}': {e}");
                    None
                }
            })
            .collect();

        Self {
            windows: HashMap::new(),
            exclude_patterns,
            include_patterns,
        }
    }

    pub fn should_track(&self, class: &str) -> bool {
        if class.is_empty() {
            return false;
        }

        for pattern in &self.exclude_patterns {
            if pattern.is_match(class) {
                return false;
            }
        }

        if !self.include_patterns.is_empty() {
            return self.include_patterns.iter().any(|p| p.is_match(class));
        }

        true
    }

    pub fn add(&mut self, window: TrackedWindow) {
        tracing::debug!(
            "tracking: {} ({}) on workspace {}",
            window.app_id,
            window.address,
            window.workspace
        );
        let key = normalize_address(&window.address);
        self.windows.insert(key, window);
    }

    pub fn remove(&mut self, address: &str) -> Option<TrackedWindow> {
        let key = normalize_address(address);
        let w = self.windows.remove(&key);
        if let Some(ref w) = w {
            tracing::debug!("untracked: {} ({})", w.app_id, address);
        }
        w
    }

    /// Returns whether a tracked window's workspace actually changed.
    #[must_use]
    pub fn update_workspace(&mut self, address: &str, workspace: &str) -> bool {
        let key = normalize_address(address);
        let Some(window) = self.windows.get_mut(&key) else {
            return false;
        };
        if window.workspace == workspace {
            return false;
        }
        window.workspace = workspace.to_string();
        tracing::debug!("updated workspace: {} → {workspace}", window.app_id);
        true
    }

    /// Returns whether a tracked window's floating bit actually changed.
    #[must_use]
    pub fn update_floating(&mut self, address: &str, floating: bool) -> bool {
        let key = normalize_address(address);
        let Some(window) = self.windows.get_mut(&key) else {
            return false;
        };
        if window.floating == floating {
            return false;
        }
        window.floating = floating;
        true
    }

    /// Update position, size, floating, fullscreen and monitor for all tracked
    /// windows from a fresh `j/clients` snapshot. Call before saving to ensure
    /// geometry reflects the user's current layout (Hyprland emits no events
    /// for tiled resize or position changes).
    ///
    /// `monitor_map` and `clients` come from two separate sequential IPC
    /// calls, not one atomic snapshot, so a workspace-to-monitor move landing
    /// between them can leave a client's monitor ID unresolvable even though
    /// its workspace/position already reflect the new assignment. When that
    /// happens we skip the whole update for that window this cycle rather
    /// than save a record with a fresh workspace/position paired with a
    /// stale or missing monitor.
    pub fn refresh_geometry(&mut self, clients: &[HyprClient], monitor_map: &HashMap<i64, String>) {
        let client_map: HashMap<String, &HyprClient> = clients
            .iter()
            .map(|c| (normalize_address(&c.address), c))
            .collect();

        let mut updated = 0usize;
        let mut skipped = 0usize;
        for (key, window) in &mut self.windows {
            let Some(client) = client_map.get(key) else {
                continue;
            };
            let Some(monitor_name) = monitor_map.get(&client.monitor) else {
                tracing::warn!(
                    "skipping geometry refresh for {} ({key}): unresolved monitor id {}",
                    window.app_id,
                    client.monitor
                );
                skipped += 1;
                continue;
            };
            window.position = client.at;
            window.size = client.size;
            window.floating = client.floating;
            window.fullscreen = client.fullscreen_mode > 0;
            window.workspace.clone_from(&client.workspace.name);
            window.monitor.clone_from(monitor_name);
            updated += 1;
        }
        tracing::debug!(
            "refreshed geometry for {updated}/{} windows ({skipped} skipped: unresolved monitor)",
            self.windows.len()
        );
    }

    pub fn windows(&self) -> Vec<&TrackedWindow> {
        self.windows.values().collect()
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, GeneralConfig, RulesConfig};
    use crate::models::HyprWorkspace;
    use std::collections::HashMap;

    fn test_config(exclude: Vec<&str>, include: Vec<&str>) -> Config {
        Config {
            general: GeneralConfig {
                save_interval: 60,
                session_dir: "/tmp/hypr-persist-test".into(),
                restore_on_start: false,
                per_window_launch: false,
                restore_geometry: false,
                restore_layout: true,
                settle_delay: 10,
                max_preexisting_windows: 3,
                notify: true,
            },
            rules: RulesConfig {
                exclude: exclude.into_iter().map(String::from).collect(),
                include: include.into_iter().map(String::from).collect(),
            },
            overrides: HashMap::new(),
            experimental: crate::config::ExperimentalConfig::default(),
        }
    }

    fn make_window(address: &str, app_id: &str, workspace: &str) -> TrackedWindow {
        TrackedWindow {
            address: address.to_string(),
            app_id: app_id.to_string(),
            launch_cmd: format!("{app_id}-cmd"),
            workspace: workspace.to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (800, 600),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: None,
        }
    }

    // --- should_track ---

    #[test]
    fn track_empty_class_returns_false() {
        let state = StateManager::new(&test_config(vec![], vec![]));
        assert!(!state.should_track(""));
    }

    #[test]
    fn track_normal_class() {
        let state = StateManager::new(&test_config(vec![], vec![]));
        assert!(state.should_track("firefox"));
    }

    #[test]
    fn track_excluded_exact() {
        let state = StateManager::new(&test_config(vec![r"^firefox$"], vec![]));
        assert!(!state.should_track("firefox"));
        assert!(state.should_track("firefox-nightly"));
    }

    #[test]
    fn track_excluded_prefix() {
        let state = StateManager::new(&test_config(vec![r"^xdg-desktop-portal.*"], vec![]));
        assert!(!state.should_track("xdg-desktop-portal-gtk"));
        assert!(!state.should_track("xdg-desktop-portal-hyprland"));
        assert!(state.should_track("firefox"));
    }

    #[test]
    fn track_include_allowlist() {
        let state = StateManager::new(&test_config(vec![], vec![r"^firefox$", r"^code$"]));
        assert!(state.should_track("firefox"));
        assert!(state.should_track("code"));
        assert!(!state.should_track("discord"));
    }

    #[test]
    fn track_exclude_takes_precedence_over_include() {
        let state = StateManager::new(&test_config(
            vec![r"^firefox$"],
            vec![r"^firefox$", r"^code$"],
        ));
        assert!(!state.should_track("firefox"));
        assert!(state.should_track("code"));
    }

    #[test]
    fn track_default_excludes() {
        let state = StateManager::new(&Config::default());
        assert!(!state.should_track("xdg-desktop-portal-gtk"));
        assert!(!state.should_track("org.kde.polkitagent"));
        assert!(state.should_track("firefox"));
    }

    #[test]
    fn track_invalid_regex_ignored() {
        let state = StateManager::new(&test_config(vec![r"[invalid"], vec![]));
        assert!(state.should_track("firefox"));
    }

    // --- add / remove ---

    #[test]
    fn add_and_count() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        assert_eq!(state.window_count(), 0);

        state.add(make_window("0xabc", "firefox", "1"));
        assert_eq!(state.window_count(), 1);

        state.add(make_window("0xdef", "code", "2"));
        assert_eq!(state.window_count(), 2);
    }

    #[test]
    fn add_same_address_overwrites() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));

        state.add(make_window("0xabc", "firefox", "1"));
        state.add(make_window("0xabc", "firefox", "3"));
        assert_eq!(state.window_count(), 1);

        let windows = state.windows();
        assert_eq!(windows[0].workspace, "3");
    }

    #[test]
    fn remove_existing() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xabc", "firefox", "1"));

        let removed = state.remove("0xabc");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().app_id, "firefox");
        assert_eq!(state.window_count(), 0);
    }

    #[test]
    fn remove_nonexistent() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        let removed = state.remove("0xnonexistent");
        assert!(removed.is_none());
    }

    #[test]
    fn remove_normalizes_0x_prefix() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xabc", "firefox", "1"));

        let removed = state.remove("abc");
        assert!(removed.is_some());
    }

    #[test]
    fn remove_address_stored_without_prefix() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("abc", "firefox", "1"));

        let removed = state.remove("0xabc");
        assert!(removed.is_some());
    }

    // --- update_workspace ---

    #[test]
    fn update_workspace_existing() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xabc", "firefox", "1"));

        assert!(state.update_workspace("0xabc", "5"));
        assert!(!state.update_workspace("0xabc", "5"));

        let windows = state.windows();
        assert_eq!(windows[0].workspace, "5");
    }

    #[test]
    fn update_workspace_nonexistent_is_noop() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xabc", "firefox", "1"));
        assert!(!state.update_workspace("0xnonexistent", "5"));

        let windows = state.windows();
        assert_eq!(windows[0].workspace, "1");
    }

    // --- update_floating ---

    #[test]
    fn update_floating_toggle() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xabc", "firefox", "1"));

        assert!(!state.windows()[0].floating);
        assert!(!state.update_floating("0xabc", false));
        assert!(!state.update_floating("0xmissing", true));

        assert!(state.update_floating("0xabc", true));
        assert!(state.windows()[0].floating);

        assert!(state.update_floating("0xabc", false));
        assert!(!state.windows()[0].floating);
    }

    // --- windows() ---

    #[test]
    fn windows_returns_all() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xa", "firefox", "1"));
        state.add(make_window("0xb", "code", "2"));

        let windows = state.windows();
        assert_eq!(windows.len(), 2);

        let app_ids: Vec<&str> = windows.iter().map(|w| w.app_id.as_str()).collect();
        assert!(app_ids.contains(&"firefox"));
        assert!(app_ids.contains(&"code"));
    }

    // --- refresh_geometry ---

    fn make_client(address: &str, workspace: &str, monitor: i64, at: (i32, i32)) -> HyprClient {
        HyprClient {
            address: address.to_string(),
            class: "firefox".to_string(),
            pid: 123,
            workspace: HyprWorkspace {
                name: workspace.to_string(),
            },
            monitor,
            at,
            size: (1024, 768),
            floating: false,
            fullscreen_mode: 0,
        }
    }

    #[test]
    fn refresh_geometry_updates_resolved_monitor() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        state.add(make_window("0xabc", "firefox", "1"));

        let clients = vec![make_client("0xabc", "4", 1, (2570, 100))];
        let monitor_map: HashMap<i64, String> = std::iter::once((1, "DP-3".to_string())).collect();

        state.refresh_geometry(&clients, &monitor_map);

        let windows = state.windows();
        assert_eq!(windows[0].workspace, "4");
        assert_eq!(windows[0].position, (2570, 100));
        assert_eq!(windows[0].monitor, "DP-3");
    }

    #[test]
    fn refresh_geometry_skips_window_on_unresolved_monitor() {
        let mut state = StateManager::new(&test_config(vec![], vec![]));
        let mut window = make_window("0xabc", "firefox", "1");
        window.monitor = "DP-1".to_string();
        window.position = (10, 743);
        state.add(window);

        // Client snapshot claims workspace "4" (bound to monitor id 2) and a
        // fresh position, but monitor id 2 is missing from monitor_map — the
        // shape of a workspace-to-monitor move landing between the two
        // separate hyprctl calls.
        let clients = vec![make_client("0xabc", "4", 2, (2570, 100))];
        let monitor_map: HashMap<i64, String> = std::iter::once((1, "DP-1".to_string())).collect();

        state.refresh_geometry(&clients, &monitor_map);

        let windows = state.windows();
        assert_eq!(
            windows[0].workspace, "1",
            "workspace must not update without a resolved monitor"
        );
        assert_eq!(
            windows[0].position,
            (10, 743),
            "position must not update without a resolved monitor"
        );
        assert_eq!(windows[0].monitor, "DP-1");
    }
}
