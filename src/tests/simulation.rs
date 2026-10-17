/// End-to-end session lifecycle simulation.
///
/// These tests exercise the full pipeline without a real Hyprland instance:
///   fixture .desktop files → `AppResolver` → `StateManager` → `SnapshotEngine` → round-trip verify
///
/// This is the closest to a real session save/restore without needing sockets.
#[cfg(test)]
mod tests {
    use std::io::Write;

    use crate::config::Config;
    use crate::core::snapshot::SnapshotEngine;
    use crate::core::state::StateManager;
    use crate::models::{HyprClient, TrackedWindow};
    use crate::resolver::AppResolver;
    use crate::resolver::desktop::DesktopIndex;

    /// Build a `DesktopIndex` from synthetic .desktop files.
    fn setup_desktop_index() -> (tempfile::TempDir, DesktopIndex) {
        let dir = tempfile::tempdir().unwrap();

        let entries = vec![
            (
                "firefox.desktop",
                "[Desktop Entry]\nName=Firefox\nExec=firefox %u\nStartupWMClass=firefox",
            ),
            (
                "com.mitchellh.ghostty.desktop",
                "[Desktop Entry]\nName=Ghostty\nExec=/usr/bin/ghostty\nStartupWMClass=com.mitchellh.ghostty",
            ),
            (
                "org.gnome.Nautilus.desktop",
                "[Desktop Entry]\nName=Files\nExec=nautilus %U\nStartupWMClass=org.gnome.Nautilus",
            ),
            (
                "discord.desktop",
                "[Desktop Entry]\nName=Discord\nExec=/usr/bin/discord\nStartupWMClass=discord",
            ),
            (
                "app.zen_browser.zen.desktop",
                "[Desktop Entry]\nName=Zen Browser\nExec=/usr/bin/flatpak run --branch=stable --arch=x86_64 app.zen_browser.zen @@u @@\nStartupWMClass=app.zen_browser.zen",
            ),
            (
                "xdg-desktop-portal-gtk.desktop",
                "[Desktop Entry]\nName=Portal\nExec=xdg-desktop-portal-gtk\nNoDisplay=true",
            ),
        ];

        for (name, content) in entries {
            let path = dir.path().join(name);
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(content.as_bytes()).unwrap();
        }

        let index = DesktopIndex::build_from_dirs(&[dir.path().to_path_buf()]);
        (dir, index)
    }

    /// Simulate `hyprctl clients -j` output.
    fn fixture_clients() -> Vec<HyprClient> {
        serde_json::from_str(&crate::tests::mock_ipc::fixture_clients_json()).unwrap()
    }

    /// Full lifecycle: resolve → populate state → save → load → verify integrity.
    #[test]
    fn full_session_lifecycle() {
        let (_dir, desktop_index) = setup_desktop_index();

        // Create config with no overrides
        let config = Config::default();
        let mut state = StateManager::new(&config);

        let clients = fixture_clients();
        assert_eq!(clients.len(), 4);

        // Simulate what the daemon does: resolve each client and add to state
        for client in &clients {
            if !state.should_track(&client.class) {
                continue;
            }

            let launch_cmd = desktop_index
                .lookup(&client.class)
                .map(|e| e.exec.clone())
                .unwrap_or_default();

            state.add(TrackedWindow {
                address: client.address.clone(),
                app_id: client.class.clone(),
                launch_cmd,
                workspace: client.workspace.name.clone(),
                monitor: String::new(),
                position: client.at,
                size: client.size,
                floating: client.floating,
                fullscreen: client.fullscreen_mode > 0,
                pid: client.pid,
                profile: None,
            });
        }

        // xdg-desktop-portal-gtk should have been excluded by default rules
        assert_eq!(state.window_count(), 3);

        // Verify resolution worked
        let windows = state.windows();
        let firefox = windows.iter().find(|w| w.app_id == "firefox").unwrap();
        assert_eq!(firefox.launch_cmd, "firefox");
        assert_eq!(firefox.workspace, "1");
        assert!(!firefox.floating);

        let nautilus = windows
            .iter()
            .find(|w| w.app_id == "org.gnome.Nautilus")
            .unwrap();
        assert_eq!(nautilus.launch_cmd, "nautilus");
        assert!(nautilus.floating);
        assert_eq!(nautilus.position, (200, 150));
        assert_eq!(nautilus.size, (900, 600));

        // Save session
        let session_dir = tempfile::tempdir().unwrap();
        let snapshot = SnapshotEngine::new_with_dir(session_dir.path(), false).unwrap();

        let path = snapshot.save(&state, "test-lifecycle").unwrap();
        assert!(path.exists());

        // Load it back
        let loaded = snapshot.load("test-lifecycle").unwrap();
        assert_eq!(loaded.session.name, "test-lifecycle");
        assert_eq!(loaded.windows.len(), 3);

        // Verify loaded session matches what we saved
        let loaded_firefox = loaded
            .windows
            .iter()
            .find(|w| w.app_id == "firefox")
            .unwrap();
        assert_eq!(loaded_firefox.launch_cmd, "firefox");
        assert_eq!(loaded_firefox.workspace, "1");
        assert!(!loaded_firefox.floating);
        assert_eq!(loaded_firefox.position, Some((0, 0)));

        let loaded_nautilus = loaded
            .windows
            .iter()
            .find(|w| w.app_id == "org.gnome.Nautilus")
            .unwrap();
        assert_eq!(loaded_nautilus.launch_cmd, "nautilus");
        assert!(loaded_nautilus.floating);
        assert_eq!(loaded_nautilus.position, Some((200, 150)));
        assert_eq!(loaded_nautilus.size, Some((900, 600)));
    }

    /// Test that overrides take precedence over .desktop file resolution.
    #[test]
    fn lifecycle_with_overrides() {
        let (_dir, _desktop_index) = setup_desktop_index();

        let mut config = Config::default();
        config
            .overrides
            .insert("firefox".to_string(), "my-custom-firefox".to_string());
        config
            .overrides
            .insert("xdg-desktop-portal-gtk".to_string(), String::new()); // skip

        let resolver = AppResolver::new(&config);

        let cmd = resolver.resolve("firefox", -1);
        assert_eq!(cmd, Some("my-custom-firefox".to_string()));
    }

    /// Test flatpak resolution via .desktop files.
    #[test]
    fn lifecycle_flatpak_resolution() {
        let (_dir, desktop_index) = setup_desktop_index();

        let entry = desktop_index.lookup("app.zen_browser.zen").unwrap();
        assert_eq!(entry.exec, "flatpak run app.zen_browser.zen");
    }

    /// Simulate events mutating state, then save and verify.
    #[test]
    fn lifecycle_events_mutate_state() {
        let config = Config::default();
        let mut state = StateManager::new(&config);

        // Start with 2 windows
        state.add(TrackedWindow {
            address: "0xaaa".to_string(),
            app_id: "firefox".to_string(),
            launch_cmd: "firefox".to_string(),
            workspace: "1".to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (1920, 1080),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: None,
        });
        state.add(TrackedWindow {
            address: "0xbbb".to_string(),
            app_id: "code".to_string(),
            launch_cmd: "code".to_string(),
            workspace: "2".to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (1920, 1080),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: None,
        });
        assert_eq!(state.window_count(), 2);

        // Simulate: new window opens
        state.add(TrackedWindow {
            address: "0xccc".to_string(),
            app_id: "discord".to_string(),
            launch_cmd: "discord".to_string(),
            workspace: "3".to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (800, 600),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: None,
        });
        assert_eq!(state.window_count(), 3);

        // Simulate: firefox moves to workspace 5
        state.update_workspace("0xaaa", "5");

        // Simulate: code becomes floating
        state.update_floating("0xbbb", true);

        // Simulate: discord closes
        state.remove("0xccc");
        assert_eq!(state.window_count(), 2);

        // Save and verify final state
        let session_dir = tempfile::tempdir().unwrap();
        let snapshot = SnapshotEngine::new_with_dir(session_dir.path(), false).unwrap();
        snapshot.save(&state, "events").unwrap();

        let loaded = snapshot.load("events").unwrap();
        assert_eq!(loaded.windows.len(), 2);

        let loaded_firefox = loaded
            .windows
            .iter()
            .find(|w| w.app_id == "firefox")
            .unwrap();
        assert_eq!(loaded_firefox.workspace, "5");

        let loaded_code = loaded.windows.iter().find(|w| w.app_id == "code").unwrap();
        assert!(loaded_code.floating);
    }

    /// Deduplication: multiple windows of the same app produce one entry.
    #[test]
    fn lifecycle_deduplication() {
        let config = Config::default();
        let mut state = StateManager::new(&config);

        // 3 Firefox windows on different workspaces
        for (i, ws) in ["1", "2", "3"].iter().enumerate() {
            state.add(TrackedWindow {
                address: format!("0x{i}"),
                app_id: "firefox".to_string(),
                launch_cmd: "firefox".to_string(),
                workspace: ws.to_string(),
                monitor: String::new(),
                position: (0, 0),
                size: (1920, 1080),
                floating: false,
                fullscreen: false,
                pid: 0,
                profile: None,
            });
        }
        assert_eq!(state.window_count(), 3);

        // Save with dedup (default mode)
        let session_dir = tempfile::tempdir().unwrap();
        let snapshot = SnapshotEngine::new_with_dir(session_dir.path(), false).unwrap();
        snapshot.save(&state, "dedup").unwrap();

        let loaded = snapshot.load("dedup").unwrap();
        assert_eq!(loaded.windows.len(), 1);
        assert_eq!(loaded.windows[0].app_id, "firefox");

        // Save with per-window mode
        let snapshot_pw = SnapshotEngine::new_with_dir(session_dir.path(), true).unwrap();
        snapshot_pw.save(&state, "nodedup").unwrap();

        let loaded_pw = snapshot_pw.load("nodedup").unwrap();
        assert_eq!(loaded_pw.windows.len(), 3);
    }

    /// Named sessions: save "work" and "gaming", list both, delete one.
    #[test]
    fn lifecycle_named_sessions() {
        let config = Config::default();
        let mut state = StateManager::new(&config);

        state.add(TrackedWindow {
            address: "0xa".to_string(),
            app_id: "firefox".to_string(),
            launch_cmd: "firefox".to_string(),
            workspace: "1".to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (0, 0),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: None,
        });

        let session_dir = tempfile::tempdir().unwrap();
        let snapshot = SnapshotEngine::new_with_dir(session_dir.path(), false).unwrap();

        snapshot.save(&state, "work").unwrap();

        state.add(TrackedWindow {
            address: "0xb".to_string(),
            app_id: "steam".to_string(),
            launch_cmd: "steam".to_string(),
            workspace: "2".to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (0, 0),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: None,
        });
        snapshot.save(&state, "gaming").unwrap();

        let sessions = snapshot.list().unwrap();
        assert_eq!(sessions.len(), 2);
        let names: Vec<&str> = sessions.iter().map(|s| s.0.as_str()).collect();
        assert!(names.contains(&"work"));
        assert!(names.contains(&"gaming"));

        // Work session should have 1 app
        let work = snapshot.load("work").unwrap();
        assert_eq!(work.windows.len(), 1);

        // Gaming session should have 2 apps
        let gaming = snapshot.load("gaming").unwrap();
        assert_eq!(gaming.windows.len(), 2);

        // Delete work
        snapshot.delete("work").unwrap();
        assert!(!snapshot.exists("work"));
        assert!(snapshot.exists("gaming"));
    }

    /// Verify that the resolver's cache works correctly.
    #[test]
    fn resolver_caching() {
        let config = Config::default();
        let resolver = AppResolver::new(&config);

        let first = resolver.resolve("firefox", -1);
        let second = resolver.resolve("firefox", -1);
        assert_eq!(first, second);
    }
}
