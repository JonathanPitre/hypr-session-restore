/// CLI integration tests.
///
/// These spawn the actual hypr-persist binary and verify its output.
/// Commands that need Hyprland are expected to fail gracefully.
#[cfg(test)]
mod tests {
    use std::process::Command;

    fn cargo_bin() -> Command {
        let mut cmd = Command::new("cargo");
        cmd.args(["run", "--quiet", "--"]);
        cmd.env_remove("HYPRLAND_INSTANCE_SIGNATURE");
        cmd
    }

    #[test]
    fn cli_help() {
        let output = cargo_bin().arg("--help").output().unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("hypr-persist"));
        assert!(stdout.contains("save"));
        assert!(stdout.contains("restore"));
        assert!(stdout.contains("list"));
        assert!(stdout.contains("delete"));
        assert!(stdout.contains("resolve"));
        assert!(stdout.contains("status"));
    }

    #[test]
    fn cli_version() {
        let output = cargo_bin().arg("--version").output().unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("hypr-persist"));
        assert!(stdout.contains("0.1.2"));
    }

    #[test]
    fn cli_list_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("test.toml");
        let session_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        std::fs::write(
            &config_path,
            format!(
                "[general]\nsession_dir = \"{}\"\n",
                session_dir.to_string_lossy()
            ),
        )
        .unwrap();

        let output = cargo_bin()
            .args(["--config", config_path.to_str().unwrap(), "list"])
            .output()
            .unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("No saved sessions"));
    }

    #[test]
    fn cli_list_with_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        // Write a fake session file
        std::fs::write(
            session_dir.join("work.toml"),
            "[session]\nname = \"work\"\ntimestamp = 1700000000\n",
        )
        .unwrap();

        let config_path = dir.path().join("test.toml");
        std::fs::write(
            &config_path,
            format!(
                "[general]\nsession_dir = \"{}\"\n",
                dir.path().to_string_lossy()
            ),
        )
        .unwrap();

        let output = cargo_bin()
            .args(["--config", config_path.to_str().unwrap(), "list"])
            .output()
            .unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("work"));
    }

    #[test]
    fn cli_delete_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let config_path = dir.path().join("test.toml");
        std::fs::write(
            &config_path,
            format!(
                "[general]\nsession_dir = \"{}\"\n",
                dir.path().to_string_lossy()
            ),
        )
        .unwrap();

        let output = cargo_bin()
            .args([
                "--config",
                config_path.to_str().unwrap(),
                "delete",
                "nonexistent",
            ])
            .output()
            .unwrap();

        // Should succeed (no error, just a warning)
        assert!(output.status.success());
    }

    #[test]
    fn cli_save_without_hyprland() {
        let output = cargo_bin().args(["save"]).output().unwrap();

        // Should fail because Hyprland is not running (no HYPRLAND_INSTANCE_SIGNATURE)
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("HYPRLAND_INSTANCE_SIGNATURE")
                || stderr.contains("Hyprland")
                || stderr.contains("not set")
        );
    }

    #[test]
    fn cli_restore_missing_session() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let config_path = dir.path().join("test.toml");
        std::fs::write(
            &config_path,
            format!(
                "[general]\nsession_dir = \"{}\"\n",
                dir.path().to_string_lossy()
            ),
        )
        .unwrap();

        let output = cargo_bin()
            .args([
                "--config",
                config_path.to_str().unwrap(),
                "restore",
                "nonexistent",
            ])
            .output()
            .unwrap();

        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("not found"));
    }

    #[test]
    fn cli_status_without_hyprland() {
        let output = cargo_bin().args(["status"]).output().unwrap();

        // status should report Hyprland is not reachable and exit non-zero
        // (it calls process::exit(1) when it can't connect)
        assert!(!output.status.success());
    }

    #[test]
    fn cli_resolve_without_hyprland() {
        // resolve should still attempt .desktop file lookup even without Hyprland
        // (it tries to get pid from hyprctl but falls back to -1)
        let output = cargo_bin().args(["resolve", "firefox"]).output().unwrap();

        // May succeed (finds .desktop file) or fail (no Hyprland for pid lookup)
        // Either way it shouldn't panic
        let _ = output.status;
    }

    #[test]
    fn cli_invalid_subcommand() {
        let output = cargo_bin().arg("frobnicate").output().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("error") || stderr.contains("unrecognized"));
    }

    #[test]
    fn cli_pin_shows_star_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("work.toml"),
            "[session]\nname = \"work\"\ntimestamp = 1700000000\n",
        )
        .unwrap();

        let config_path = dir.path().join("test.toml");
        std::fs::write(
            &config_path,
            format!(
                "[general]\nsession_dir = \"{}\"\n",
                dir.path().to_string_lossy()
            ),
        )
        .unwrap();
        let cfg = config_path.to_str().unwrap();

        let missing = cargo_bin()
            .args(["--config", cfg, "pin", "missing"])
            .output()
            .unwrap();
        assert!(!missing.status.success());

        let pinned = cargo_bin()
            .args(["--config", cfg, "pin", "work"])
            .output()
            .unwrap();
        assert!(
            pinned.status.success(),
            "{}",
            String::from_utf8_lossy(&pinned.stderr)
        );

        let listed = cargo_bin()
            .args(["--config", cfg, "list"])
            .output()
            .unwrap();
        let stdout = String::from_utf8(listed.stdout).unwrap();
        assert!(stdout.contains("* work"), "{stdout}");

        let cleared = cargo_bin()
            .args(["--config", cfg, "pin", "--clear"])
            .output()
            .unwrap();
        assert!(cleared.status.success());
        let listed = cargo_bin()
            .args(["--config", cfg, "list"])
            .output()
            .unwrap();
        let stdout = String::from_utf8(listed.stdout).unwrap();
        assert!(stdout.contains("work"));
        assert!(!stdout.contains('*'), "{stdout}");
    }
}
