//! Desktop notifications via `notify-send` argv. Never a shell.

use std::path::Path;
use std::process::Command;

pub fn notify(bin: &Path, summary: &str, body: &str) {
    match Command::new(bin)
        .arg("-a")
        .arg("hypr-persist")
        .arg(summary)
        .arg(body)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::debug!("notify-send exited {status}"),
        Err(err) => tracing::debug!("notify-send failed to start: {err}"),
    }
}

pub fn notify_outcome(enabled: bool, success: bool, detail: &str) {
    if !enabled {
        return;
    }
    let summary = if success {
        "Session restored"
    } else {
        "Session restore failed"
    };
    notify(Path::new("notify-send"), summary, detail);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_binary_is_ignored() {
        notify(
            Path::new("/no/such/hypr-persist-notify-send"),
            "summary",
            "body",
        );
    }
}
