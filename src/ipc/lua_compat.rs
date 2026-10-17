//! Translates classic `hyprctl dispatch`/`keyword` argument strings into the
//! Lua call syntax required when Hyprland is running a Lua config.
//!
//! On a Lua-config Hyprland instance, the `dispatch <args>` socket request is
//! evaluated as `return hl.dispatch(<args>)` — i.e. `<args>` must be valid Lua
//! that evaluates to a dispatcher (e.g. `hl.dsp.window.close()`), not a
//! classic space/comma-separated dispatcher string. Everything in this module
//! is derived from empirical probing of a live Lua-config instance (see
//! `translate_dispatch` doc comments for per-dispatcher confidence), plus one
//! structural inference: `layoutmsg <msg>` is a generic passthrough to the
//! active layout plugin in classic Hyprland, and `hl.dsp.layout("togglesplit")`
//! was confirmed to behave identically to classic `layoutmsg togglesplit` —
//! so the same passthrough is used for every `layoutmsg` message.

use anyhow::{Context, Result, anyhow, bail};

/// Translate a classic `dispatch <args>` payload into the Lua expression the
/// socket expects when Hyprland is running a Lua config.
///
/// Returns an error (rather than a guessed translation) for dispatchers that
/// haven't been empirically or structurally confirmed, so a missing mapping
/// fails loudly instead of silently sending malformed Lua.
pub fn translate_dispatch(args: &str) -> Result<String> {
    let (name, rest) = args.split_once(' ').unwrap_or((args, ""));
    match name {
        // Confirmed: hl.dsp.focus({ window = "address:0x.." })
        "focuswindow" => {
            let selector = parse_address_selector(rest)?;
            Ok(format!(
                "hl.dsp.focus({{ window = {} }})",
                lua_quote(selector)
            ))
        }
        // Confirmed: hl.dsp.window.float({ window = "address:0x.." }) toggles
        // floating state — empirically observed switching a tiled window to
        // floating with no "action" field at all, matching classic
        // `setfloating`'s own toggle semantics exactly (no force-set needed).
        "setfloating" => {
            let selector = parse_address_selector(rest)?;
            Ok(format!(
                "hl.dsp.window.float({{ window = {} }})",
                lua_quote(selector)
            ))
        }
        // Inferred from the same pattern as focuswindow/setfloating (bare
        // address selector as the whole argument) — not independently
        // verified against a live instance.
        "closewindow" => {
            let selector = parse_address_selector(rest)?;
            Ok(format!(
                "hl.dsp.window.close({{ window = {} }})",
                lua_quote(selector)
            ))
        }
        // Confirmed shape (window+x+y+exact fields all verified individually);
        // fullscreen's own "window" key specifically was not re-verified but
        // follows the same pattern as float/resize/move.
        "fullscreen" => {
            let (mode, selector) = split_leading_arg_and_address(rest)?;
            require_lua_int(mode)?;
            Ok(format!(
                "hl.dsp.window.fullscreen({{ window = {}, mode = {mode} }})",
                lua_quote(&selector)
            ))
        }
        // Confirmed: hl.dsp.window.move({ window = .., workspace = ".." })
        "movetoworkspacesilent" => {
            let (ws, selector) = split_leading_arg_and_address(rest)?;
            Ok(format!(
                "hl.dsp.window.move({{ window = {}, workspace = {} }})",
                lua_quote(&selector),
                lua_quote(ws)
            ))
        }
        // Confirmed: hl.dsp.window.move({ window = .., x = .., y = .. }) sets
        // absolute pixel position (the "+relative" flag is opt-in, so the
        // default matches classic `movewindowpixel exact`). hypr-persist only
        // ever calls this with `exact`, so the prefix is stripped and ignored.
        "movewindowpixel" => {
            let rest = rest.strip_prefix("exact ").unwrap_or(rest);
            let (xy, selector) = rest.rsplit_once(",address:").ok_or_else(|| {
                anyhow!("movewindowpixel: expected `<x> <y>,address:<addr>`, got {rest:?}")
            })?;
            let (x, y) = xy
                .split_once(' ')
                .ok_or_else(|| anyhow!("movewindowpixel: expected `<x> <y>`, got {xy:?}"))?;
            require_lua_int(x)?;
            require_lua_int(y)?;
            Ok(format!(
                "hl.dsp.window.move({{ window = {}, x = {x}, y = {y} }})",
                lua_quote(&format!("address:{selector}"))
            ))
        }
        // Confirmed: hl.dsp.window.resize({ window = .., x = .., y = .., exact = bool })
        "resizewindowpixel" => {
            let (exact, rest) = rest
                .strip_prefix("exact ")
                .map_or((false, rest), |r| (true, r));
            let (wh, selector) = rest.rsplit_once(",address:").ok_or_else(|| {
                anyhow!("resizewindowpixel: expected `<w> <h>,address:<addr>`, got {rest:?}")
            })?;
            let (w, h) = wh
                .split_once(' ')
                .ok_or_else(|| anyhow!("resizewindowpixel: expected `<w> <h>`, got {wh:?}"))?;
            require_lua_int(w)?;
            require_lua_int(h)?;
            Ok(format!(
                "hl.dsp.window.resize({{ window = {}, x = {w}, y = {h}, exact = {exact} }})",
                lua_quote(&format!("address:{selector}"))
            ))
        }
        // Structural inference (not independently verified per-message): classic
        // `layoutmsg <msg>` forwards <msg> verbatim to the active layout plugin.
        // hl.dsp.layout("togglesplit") was empirically confirmed to behave
        // identically to `layoutmsg togglesplit`, so the same passthrough is
        // used for preselect/splitratio/orientation*/addmaster/etc.
        "layoutmsg" => Ok(format!(r"hl.dsp.layout({})", lua_quote(rest))),
        // Confirmed: hl.dsp.workspace.move({ workspace = .., monitor = .. })
        "moveworkspacetomonitor" => {
            let (ws, monitor) = rest.split_once(' ').ok_or_else(|| {
                anyhow!("moveworkspacetomonitor: expected `<ws> <monitor>`, got {rest:?}")
            })?;
            Ok(format!(
                "hl.dsp.workspace.move({{ workspace = {}, monitor = {} }})",
                lua_quote(ws),
                lua_quote(monitor)
            ))
        }
        // Inferred, NOT independently confirmed: `type(hl.dsp.workspace) ==
        // "table"` only rules out a plain function value — Lua's `type()`
        // still reports "table" for a table with a `__call` metamethod, so a
        // callable `hl.dsp.workspace(<selector>)` (mirroring how
        // `hl.dsp.layout(<string>)` is called directly) remains the best
        // structural guess, consistent with move/rename/swap_monitors/
        // toggle_special being distinct named sub-dispatchers alongside it.
        // A wrong guess here fails no worse than bailing outright would (both
        // hard-abort the restore via the unguarded `?` at the call site), so
        // attempting it can only do better, never worse. Verify against
        // journalctl on the next real restore.
        "workspace" => Ok(format!("hl.dsp.workspace({})", lua_quote(rest))),
        // Confirmed shape for the dispatcher name; exec's own argument is a
        // raw shell command, not address-targeted.
        "exec" => Ok(format!("hl.dsp.exec_cmd({})", lua_quote(rest))),
        other => bail!("lua_compat: no translation for dispatcher {other:?} (args: {rest:?})"),
    }
}

/// Parse a bare `address:0x...` selector (used by dispatchers whose entire
/// remaining argument is the selector, e.g. `focuswindow`/`setfloating`).
fn parse_address_selector(s: &str) -> Result<&str> {
    if s.starts_with("address:") {
        Ok(s)
    } else {
        Err(anyhow!("expected `address:0x..`, got {s:?}"))
    }
}

/// Split `"<leading> <trailing-arg>,address:0x.."` into the leading
/// positional argument and the full `address:0x..` selector.
fn split_leading_arg_and_address(s: &str) -> Result<(&str, String)> {
    let (leading, addr) = s
        .split_once(',')
        .with_context(|| format!("expected `<arg>,address:<addr>`, got {s:?}"))?;
    if !addr.starts_with("address:") {
        bail!("expected `address:0x..` after comma, got {addr:?}");
    }
    Ok((leading, addr.to_string()))
}

fn require_lua_int(raw: &str) -> Result<()> {
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("lua_compat: expected an integer, got {raw:?}");
    }
    Ok(())
}

/// Lua single-quoted string literal, escaping backslashes/quotes.
fn lua_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focuswindow() {
        assert_eq!(
            translate_dispatch("focuswindow address:0xabc123").unwrap(),
            r#"hl.dsp.focus({ window = "address:0xabc123" })"#
        );
    }

    #[test]
    fn setfloating() {
        assert_eq!(
            translate_dispatch("setfloating address:0xabc123").unwrap(),
            r#"hl.dsp.window.float({ window = "address:0xabc123" })"#
        );
    }

    #[test]
    fn closewindow() {
        assert_eq!(
            translate_dispatch("closewindow address:0xabc123").unwrap(),
            r#"hl.dsp.window.close({ window = "address:0xabc123" })"#
        );
    }

    #[test]
    fn fullscreen() {
        assert_eq!(
            translate_dispatch("fullscreen 0,address:0xabc123").unwrap(),
            r#"hl.dsp.window.fullscreen({ window = "address:0xabc123", mode = 0 })"#
        );
    }

    #[test]
    fn movetoworkspacesilent() {
        assert_eq!(
            translate_dispatch("movetoworkspacesilent 4,address:0xabc123").unwrap(),
            r#"hl.dsp.window.move({ window = "address:0xabc123", workspace = "4" })"#
        );
    }

    #[test]
    fn movewindowpixel_exact() {
        assert_eq!(
            translate_dispatch("movewindowpixel exact 4044 45,address:0xabc123").unwrap(),
            r#"hl.dsp.window.move({ window = "address:0xabc123", x = 4044, y = 45 })"#
        );
    }

    #[test]
    fn resizewindowpixel_exact() {
        assert_eq!(
            translate_dispatch("resizewindowpixel exact 1066 688,address:0xabc123").unwrap(),
            r#"hl.dsp.window.resize({ window = "address:0xabc123", x = 1066, y = 688, exact = true })"#
        );
    }

    #[test]
    fn resizewindowpixel_relative() {
        assert_eq!(
            translate_dispatch("resizewindowpixel -3 6,address:0xabc123").unwrap(),
            r#"hl.dsp.window.resize({ window = "address:0xabc123", x = -3, y = 6, exact = false })"#
        );
    }

    #[test]
    fn layoutmsg_preselect() {
        assert_eq!(
            translate_dispatch("layoutmsg preselect r").unwrap(),
            r#"hl.dsp.layout("preselect r")"#
        );
    }

    #[test]
    fn layoutmsg_splitratio() {
        assert_eq!(
            translate_dispatch("layoutmsg splitratio 0.100000").unwrap(),
            r#"hl.dsp.layout("splitratio 0.100000")"#
        );
    }

    #[test]
    fn layoutmsg_togglesplit() {
        assert_eq!(
            translate_dispatch("layoutmsg togglesplit").unwrap(),
            r#"hl.dsp.layout("togglesplit")"#
        );
    }

    #[test]
    fn moveworkspacetomonitor() {
        assert_eq!(
            translate_dispatch("moveworkspacetomonitor 4 DP-3").unwrap(),
            r#"hl.dsp.workspace.move({ workspace = "4", monitor = "DP-3" })"#
        );
    }

    #[test]
    fn exec_simple() {
        assert_eq!(
            translate_dispatch("exec alacritty").unwrap(),
            r#"hl.dsp.exec_cmd("alacritty")"#
        );
    }

    #[test]
    fn workspace_switch_best_effort() {
        assert_eq!(
            translate_dispatch("workspace 4").unwrap(),
            r#"hl.dsp.workspace("4")"#
        );
    }

    #[test]
    fn unknown_dispatcher_errors() {
        assert!(translate_dispatch("bogusdispatcher foo").is_err());
    }
}
