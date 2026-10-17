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

/// Leave `text` unquoted when it is non-empty and every byte is in
/// `A-Za-z0-9_./@=:+-`. The empty word is `''`, so a quoted empty argument
/// survives a round-trip. Every other word is wrapped in POSIX single quotes.
pub fn shell_quote(text: &str) -> String {
    if !text.is_empty() && text.bytes().all(is_bare_byte) {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Split `cmd` on unquoted whitespace.
///
/// This is a quote remover, not a shell. Single quotes are literal. Double
/// quotes honor backslash escapes for `$`, `` ` ``, `"`, `\`, and newline.
/// A bare backslash escapes the next character. `|`, `;`, and `&` stay
/// ordinary characters.
pub fn shell_words(cmd: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = cmd.chars();
    let mut quoting = Quote::None;
    let mut started = false;

    while let Some(ch) = chars.next() {
        quoting = feed_char(
            ch,
            quoting,
            &mut chars,
            &mut current,
            &mut started,
            &mut words,
        )?;
    }
    if quoting != Quote::None {
        bail!("unclosed quote in launch command");
    }
    if started {
        words.push(current);
    }
    Ok(words)
}

fn feed_char(
    ch: char,
    quoting: Quote,
    chars: &mut impl Iterator<Item = char>,
    current: &mut String,
    started: &mut bool,
    words: &mut Vec<String>,
) -> Result<Quote> {
    match quoting {
        Quote::None => feed_unquoted(ch, chars, current, started, words),
        Quote::Single => Ok(feed_single(ch, current)),
        Quote::Double => feed_double(ch, chars, current),
    }
}

fn feed_unquoted(
    ch: char,
    chars: &mut impl Iterator<Item = char>,
    current: &mut String,
    started: &mut bool,
    words: &mut Vec<String>,
) -> Result<Quote> {
    match ch {
        ' ' | '\t' | '\n' => {
            if *started {
                words.push(std::mem::take(current));
                *started = false;
            }
            Ok(Quote::None)
        }
        '\\' => {
            if push_bare_escape(chars, current)? {
                *started = true;
            }
            Ok(Quote::None)
        }
        '\'' => {
            *started = true;
            Ok(Quote::Single)
        }
        '"' => {
            *started = true;
            Ok(Quote::Double)
        }
        _ => {
            current.push(ch);
            *started = true;
            Ok(Quote::None)
        }
    }
}

fn feed_single(ch: char, current: &mut String) -> Quote {
    if ch == '\'' {
        Quote::None
    } else {
        current.push(ch);
        Quote::Single
    }
}

fn feed_double(
    ch: char,
    chars: &mut impl Iterator<Item = char>,
    current: &mut String,
) -> Result<Quote> {
    if ch == '"' {
        return Ok(Quote::None);
    }
    if ch == '\\' {
        push_double_escape(chars, current)?;
    } else {
        current.push(ch);
    }
    Ok(Quote::Double)
}

fn push_bare_escape(chars: &mut impl Iterator<Item = char>, current: &mut String) -> Result<bool> {
    match chars.next() {
        Some('\n') => Ok(false),
        Some(next) => {
            current.push(next);
            Ok(true)
        }
        None => bail!("trailing backslash in launch command"),
    }
}

fn push_double_escape(chars: &mut impl Iterator<Item = char>, current: &mut String) -> Result<()> {
    match chars.next() {
        Some('\n') => Ok(()),
        Some(next) if matches!(next, '$' | '`' | '"' | '\\') => {
            current.push(next);
            Ok(())
        }
        Some(next) => {
            current.push('\\');
            current.push(next);
            Ok(())
        }
        None => bail!("trailing backslash in launch command"),
    }
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
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_words(&shell_quote("")).unwrap(), vec![String::new()]);
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

    #[test]
    fn shell_words_preserves_quoted_and_escaped_argv() {
        assert_eq!(
            shell_words(r#""/opt/My App/bin/app" --flag"#).unwrap(),
            vec!["/opt/My App/bin/app".to_string(), "--flag".to_string()]
        );
        assert_eq!(
            shell_words("'/opt/My App/bin/app' --flag").unwrap(),
            vec!["/opt/My App/bin/app".to_string(), "--flag".to_string()]
        );
        assert_eq!(
            shell_words(r"foo\ bar").unwrap(),
            vec!["foo bar".to_string()]
        );
        assert_eq!(
            shell_words(r#""a\"b" c"#).unwrap(),
            vec!["a\"b".to_string(), "c".to_string()]
        );
        assert_eq!(
            shell_words(r#""a\b" \c"#).unwrap(),
            vec!["a\\b".to_string(), "c".to_string()]
        );
        assert_eq!(
            shell_words("echo a && echo b").unwrap(),
            vec![
                "echo".to_string(),
                "a".to_string(),
                "&&".to_string(),
                "echo".to_string(),
                "b".to_string()
            ]
        );
        assert!(shell_words("\"abc").is_err());
        assert!(shell_words("abc\\").is_err());
    }
}
