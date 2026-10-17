//! Validated dispatcher tokens and POSIX shell quoting.
//!
//! Workspace, monitor, and address values are parsed before they are
//! interpolated into a Hyprland dispatch. Shell words are left bare only when
//! every byte is in a fixed safe set; everything else is single-quoted.

use anyhow::{Result, bail};

use crate::core::state::normalize_address;

const MAX_WORKSPACE_LEN: usize = 128;
const MAX_MONITOR_LEN: usize = 64;

pub struct WorkspaceName(String);

impl WorkspaceName {
    pub fn parse(raw: &str) -> Result<Self> {
        validate_token("workspace", raw, MAX_WORKSPACE_LEN)?;
        Ok(Self(raw.to_string()))
    }

    pub const fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

pub struct MonitorName(String);

impl MonitorName {
    pub fn parse(raw: &str) -> Result<Self> {
        validate_token("monitor", raw, MAX_MONITOR_LEN)?;
        Ok(Self(raw.to_string()))
    }

    pub const fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

pub struct WindowAddress(String);

impl WindowAddress {
    /// Hex address, after [`normalize_address`].
    pub fn parse(raw: &str) -> Result<Self> {
        let hex = normalize_address(raw);
        if hex.is_empty() || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid window address '{raw}'");
        }
        Ok(Self(hex))
    }

    pub const fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn selector(&self) -> String {
        format!("address:0x{}", self.as_str())
    }
}

/// Leave `s` unquoted when every byte is in `A-Za-z0-9_./@=:+-`.
/// Otherwise wrap it in POSIX single quotes.
pub fn shell_quote(text: &str) -> String {
    if text.bytes().all(is_bare_byte) {
        text.to_string()
    } else {
        posix_single_quote(text)
    }
}

pub fn posix_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub fn shell_join(cmd: &str) -> String {
    cmd.split_whitespace()
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn exec_on_workspace(workspace: &str, cmd: &str) -> Result<String> {
    let workspace = WorkspaceName::parse(workspace)?;
    Ok(format!(
        "exec [workspace {} silent] {}",
        workspace.as_str(),
        prepared_exec(cmd)
    ))
}

pub fn exec_here(cmd: &str) -> String {
    format!("exec {}", prepared_exec(cmd))
}

pub fn move_to_workspace_silent(workspace: &str, address: &str) -> Result<String> {
    let workspace = WorkspaceName::parse(workspace)?;
    Ok(format!(
        "movetoworkspacesilent {},{}",
        workspace.as_str(),
        address_selector(address)?
    ))
}

pub fn focus_window(address: &str) -> Result<String> {
    Ok(format!("focuswindow {}", address_selector(address)?))
}

pub fn workspace_switch(workspace: &str) -> Result<String> {
    let workspace = WorkspaceName::parse(workspace)?;
    Ok(format!("workspace {}", workspace.as_str()))
}

pub fn move_workspace_to_monitor(workspace: &str, monitor: &str) -> Result<String> {
    let workspace = WorkspaceName::parse(workspace)?;
    let monitor = MonitorName::parse(monitor)?;
    Ok(format!(
        "moveworkspacetomonitor {} {}",
        workspace.as_str(),
        monitor.as_str()
    ))
}

pub fn reject_control_bytes(text: &str) -> Result<()> {
    if text.bytes().any(|byte| byte < 0x20) {
        bail!("control byte rejected");
    }
    Ok(())
}

/// Map a window class onto `A-Za-z0-9_-` for a Hyprland window-rule name.
pub fn normalize_windowrule_name(app_id: &str) -> Result<String> {
    reject_control_bytes(app_id)?;
    Ok(app_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect())
}

fn prepared_exec(cmd: &str) -> String {
    // `build_launch_cmd` already joined the argv and, for a saved cwd, wrapped
    // it in an `sh -c` script or appended a quoted directory flag. Joining
    // that result again would split the quotes.
    if cmd.starts_with("sh -c ") || cmd.contains('\'') {
        cmd.to_string()
    } else {
        shell_join(cmd)
    }
}

fn address_selector(address: &str) -> Result<String> {
    if let Ok(parsed) = WindowAddress::parse(address) {
        return Ok(parsed.selector());
    }
    // Restore fixtures use non-hex tokens (`newwin`, `splash1`). Accept only
    // an alphanumeric token so the historical `address:0x{token}` spelling
    // stays intact, and reject anything that could break out of the dispatch.
    let normalized = normalize_address(address);
    if !normalized.is_empty() && normalized.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Ok(format!("address:0x{normalized}"));
    }
    bail!("invalid window address '{address}'");
}

fn validate_token(kind: &str, raw: &str, max_len: usize) -> Result<()> {
    if raw.is_empty() || raw.len() > max_len {
        bail!("invalid {kind} name '{raw}'");
    }
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        bail!("invalid {kind} name '{raw}'");
    };
    if !first.is_ascii_alphanumeric() {
        bail!("invalid {kind} name '{raw}'");
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '.' | '-')) {
        bail!("invalid {kind} name '{raw}'");
    }
    Ok(())
}

const fn is_bare_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'_'
            | b'.'
            | b'/'
            | b'@'
            | b'='
            | b':'
            | b'+'
            | b'-'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_space_apostrophe_semicolon_dollar_and_subshell() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("a;b"), "'a;b'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(shell_quote("$(id)"), "'$(id)'");
        assert_eq!(shell_quote("`id`"), "'`id`'");
        assert_eq!(shell_quote("foot"), "foot");
    }

    #[test]
    fn exec_on_workspace_exact_string() {
        assert_eq!(
            exec_on_workspace("1", "foot").unwrap(),
            "exec [workspace 1 silent] foot"
        );
    }

    #[test]
    fn invalid_workspace_monitor_and_address() {
        assert!(WorkspaceName::parse("").is_err());
        assert!(WorkspaceName::parse("-bad").is_err());
        assert!(WorkspaceName::parse("has space").is_err());
        assert!(WorkspaceName::parse(&"a".repeat(MAX_WORKSPACE_LEN + 1)).is_err());
        assert!(WorkspaceName::parse("work:1").is_ok());

        assert!(MonitorName::parse("").is_err());
        assert!(MonitorName::parse("bad monitor").is_err());
        assert!(MonitorName::parse(&"m".repeat(MAX_MONITOR_LEN + 1)).is_err());
        assert!(MonitorName::parse("DP-1").is_ok());

        assert!(WindowAddress::parse("").is_err());
        assert!(WindowAddress::parse("zz").is_err());
        assert!(WindowAddress::parse("not hex").is_err());
        let addr = WindowAddress::parse("0xABC123").unwrap();
        assert_eq!(addr.as_str(), "abc123");
        assert_eq!(addr.selector(), "address:0xabc123");
    }

    #[test]
    fn control_byte_rejected() {
        assert!(reject_control_bytes("foot\n").is_err());
        assert!(normalize_windowrule_name("app\tid").is_err());
        assert_eq!(
            normalize_windowrule_name("org.gnome.Nautilus").unwrap(),
            "org-gnome-Nautilus"
        );
    }
}
