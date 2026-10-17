use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "Config::default_general")]
    pub general: GeneralConfig,
    #[serde(default)]
    pub rules: RulesConfig,
    #[serde(default)]
    pub overrides: HashMap<String, String>,
    #[serde(default)]
    pub experimental: ExperimentalConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct GeneralConfig {
    #[serde(default = "default_save_interval")]
    pub save_interval: u64,
    #[serde(default = "default_session_dir")]
    pub session_dir: String,
    #[serde(default = "default_true")]
    pub restore_on_start: bool,
    #[serde(default = "default_true")]
    pub per_window_launch: bool,
    #[serde(default = "default_true")]
    pub restore_geometry: bool,
    #[serde(default = "default_true")]
    pub restore_layout: bool,
    #[serde(default = "default_settle_delay")]
    pub settle_delay: u64,
    #[serde(default = "default_max_preexisting_windows")]
    pub max_preexisting_windows: u32,
    #[serde(default = "default_true")]
    pub notify: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExperimentalConfig {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesConfig {
    #[serde(default = "default_excludes")]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub include: Vec<String>,
}

impl Default for RulesConfig {
    fn default() -> Self {
        Self {
            exclude: default_excludes(),
            include: Vec::new(),
        }
    }
}

const fn default_save_interval() -> u64 {
    120
}

fn default_session_dir() -> String {
    "~/.local/share/hypr-persist".to_string()
}

const fn default_true() -> bool {
    true
}

const fn default_settle_delay() -> u64 {
    10
}

const fn default_max_preexisting_windows() -> u32 {
    3
}

fn default_excludes() -> Vec<String> {
    vec![
        r"^xdg-desktop-portal.*".to_string(),
        r"^org\.kde\.polkit.*".to_string(),
    ]
}

impl Config {
    fn default_general() -> GeneralConfig {
        GeneralConfig {
            save_interval: default_save_interval(),
            session_dir: default_session_dir(),
            restore_on_start: true,
            per_window_launch: true,
            restore_geometry: true,
            restore_layout: true,
            settle_delay: default_settle_delay(),
            max_preexisting_windows: default_max_preexisting_windows(),
            notify: true,
        }
    }

    pub fn load(path: Option<&str>) -> Result<Self> {
        let config_path = path.map_or_else(Self::default_path, PathBuf::from);

        if !config_path.exists() {
            tracing::info!("no config at {}, using defaults", config_path.display());
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(&config_path)
            .with_context(|| format!("reading config from {}", config_path.display()))?;

        toml::from_str(&contents)
            .with_context(|| format!("parsing config from {}", config_path.display()))
    }

    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("hypr")
            .join("hypr-persist.toml")
    }

    /// Configured state directory, before the `sessions` subdirectory.
    pub fn state_root(&self) -> PathBuf {
        let raw = &self.general.session_dir;
        raw.strip_prefix('~').map_or_else(
            || PathBuf::from(raw),
            |rest| {
                let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
                home.join(rest.strip_prefix('/').unwrap_or(rest))
            },
        )
    }

    pub fn session_dir(&self) -> PathBuf {
        self.state_root().join("sessions")
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: Self::default_general(),
            rules: RulesConfig::default(),
            overrides: HashMap::new(),
            experimental: ExperimentalConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let cfg = Config::default();
        assert_eq!(cfg.general.save_interval, 120);
        assert!(cfg.general.restore_on_start);
        assert!(cfg.general.per_window_launch);
        assert!(cfg.general.restore_geometry);
        assert!(cfg.general.restore_layout);
        assert_eq!(cfg.general.settle_delay, 10);
        assert_eq!(cfg.general.max_preexisting_windows, 3);
        assert!(cfg.general.notify);
        assert!(!cfg.rules.exclude.is_empty());
        assert!(cfg.rules.include.is_empty());
        assert!(cfg.overrides.is_empty());
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let cfg = Config::load(Some("/tmp/nonexistent-hypr-persist-test-config.toml")).unwrap();
        assert_eq!(cfg.general.save_interval, 120);
    }

    #[test]
    fn load_empty_toml_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.toml");
        std::fs::write(&path, "").unwrap();

        let cfg = Config::load(Some(path.to_str().unwrap())).unwrap();
        assert_eq!(cfg.general.save_interval, 120);
        assert!(cfg.general.restore_on_start);
    }

    #[test]
    fn load_partial_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.toml");
        std::fs::write(
            &path,
            r"
[general]
save_interval = 60
restore_on_start = false
",
        )
        .unwrap();

        let cfg = Config::load(Some(path.to_str().unwrap())).unwrap();
        assert_eq!(cfg.general.save_interval, 60);
        assert!(!cfg.general.restore_on_start);
        assert!(cfg.general.restore_geometry);
    }

    #[test]
    fn load_full_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.toml");
        std::fs::write(
            &path,
            r#"
[general]
save_interval = 30
session_dir = "/tmp/my-sessions"
restore_on_start = false
per_window_launch = true
restore_geometry = false
restore_layout = true

[rules]
exclude = ["^steam.*", "^lutris.*"]
include = ["^firefox$"]

[overrides]
"app.zen_browser.zen" = "flatpak run app.zen_browser.zen"
"steam_app_.*" = ""
"#,
        )
        .unwrap();

        let cfg = Config::load(Some(path.to_str().unwrap())).unwrap();
        assert_eq!(cfg.general.save_interval, 30);
        assert_eq!(cfg.general.session_dir, "/tmp/my-sessions");
        assert!(!cfg.general.restore_on_start);
        assert!(cfg.general.per_window_launch);
        assert!(!cfg.general.restore_geometry);
        assert!(cfg.general.restore_layout);
        assert_eq!(cfg.rules.exclude.len(), 2);
        assert_eq!(cfg.rules.include.len(), 1);
        assert_eq!(cfg.overrides.len(), 2);
        assert_eq!(
            cfg.overrides.get("app.zen_browser.zen").unwrap(),
            "flatpak run app.zen_browser.zen"
        );
        assert_eq!(cfg.overrides.get("steam_app_.*").unwrap(), "");
    }

    #[test]
    fn load_invalid_toml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is {{{{ not valid toml").unwrap();

        let result = Config::load(Some(path.to_str().unwrap()));
        assert!(result.is_err());
    }

    #[test]
    fn session_dir_expands_tilde() {
        let cfg = Config::default();
        let dir = cfg.session_dir();
        let dir_str = dir.to_string_lossy();
        // Should not contain tilde
        assert!(!dir_str.contains('~'));
        // Should end with /sessions
        assert!(dir_str.ends_with("sessions"));
    }
}
