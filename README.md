# hypr-persist

Session persistence for [Hyprland](https://hyprland.org). Saves your open applications and puts them back on the right workspaces after a reboot.

- Resolves launch commands from `.desktop` files, Flatpak cgroups or `/proc`
- Saves sessions as human-readable TOML
- Restores apps to their original workspaces with floating window geometry
- Reconstructs tiling layouts from saved window positions (dwindle and master layouts)

Originally forked from [hyprresume](https://github.com/IraSkyx/hyprresume) by Adrien Lenoir;
substantially reworked since (see `LICENSE` for attribution).

## Install

**Arch Linux (AUR):**

```sh
paru -S hypr-persist    # or yay, etc.
```

**From source:**

```sh
cargo install --path .
```

## Usage

Add to your `hyprland.conf`:

```conf
exec-once = hypr-persist
```

This starts the daemon, which restores your previous session, tracks window changes and saves periodically. It also saves on logout/shutdown via SIGTERM.

### Commands

```sh
hypr-persist save [name]         # snapshot current session (unnamed saves the pin, or "last")
hypr-persist restore [name]      # restore the named session, or the pin, or the newest
hypr-persist restore [name] --dry-run
                                 # print the launch plan; no compositor, stamp, or notification
hypr-persist pin [name]          # pin a session; omit the name to print the current pin
hypr-persist pin --clear         # clear the pin
hypr-persist list                # list saved sessions; the pinned row is marked with *
hypr-persist delete <name>       # delete a session (also clears the pin when it matches)
hypr-persist resolve <class>     # show what command a window class maps to
hypr-persist status              # show daemon/session info
hypr-persist status --json       # the same facts as one JSON object (schemaVersion 1)
```

Use `-v` for info logs, `-vv` for debug, `-vvv` for trace.

## Configuration

Place a config file at `~/.config/hypr/hypr-persist.toml`. All fields are optional; defaults are sane.

```toml
[general]
save_interval = 120           # seconds between auto-saves
settle_delay = 10             # extra seconds to wait when windows are only closing
session_dir = "~/.local/share/hypr-persist"
restore_on_start = true       # pinned session, otherwise the newest
max_preexisting_windows = 3   # skip auto-restore when more windows are already open
notify = true                 # notify-send after a restore attempt
per_window_launch = true      # one process per window, not per unique app
restore_geometry = true       # restore floating window position/size
restore_layout = true         # reconstruct tiling layout (dwindle + master)

[rules]
exclude = [
    "^xdg-desktop-portal.*",
    "^org\\.kde\\.polkit.*",
]
# include = ["^firefox$", "^code$"]  # allowlist mode

[overrides]
# "app.zen_browser.zen" = "flatpak run app.zen_browser.zen"
# "steam_app_.*" = ""  # empty = skip
```

A full example config is in [`assets/hypr-persist.toml`](assets/hypr-persist.toml).

### Tiling layout restoration

When `restore_layout` is enabled (the default), hypr-persist saves every tiled window's position and size and reconstructs the layout on restore. The active layout is auto-detected via `general:layout`.

**Dwindle** — infers a BSP tree from saved geometry, replays it via `layoutmsg preselect`, and corrects split ratios with `layoutmsg splitratio` where reachable and targeted pixel resizes for splits it can't reach directly. Your Hyprland config should include `preserve_split = true` so that split directions are stable:

```conf
dwindle {
    preserve_split = true
}
```

**Master** — infers orientation, master/stack partition and `mfact` from saved geometry. Windows are opened in the correct order (masters first, then stack) so the layout engine places them naturally.

Unsupported layouts fall back to simple restore: windows land on the correct workspaces but use Hyprland's default placement.


## Session files

The session directory is private (`0700`) and each session file is `0600`. Writes are atomic (a hidden temporary in the same directory, then rename). The daemon refuses to follow a symlink at the directory or at a session file, and it refuses non-regular files. A hidden `.pin` file stores the pinned session name. Replacing a session leaves the previous contents in a hidden `.name.toml.prev` file, which `list` ignores.

A regular file named `disabled` in the session state directory (or a symlink in that place, which is not followed) turns off restore-on-start. Automatic restore also burns a per-instance stamp under `$XDG_RUNTIME_DIR/hypr-persist` and will not restore twice for the same Hyprland instance.

## Contributing

Requires Rust 1.94 (pinned in `rust-toolchain.toml`).

```sh
git clone https://github.com/ngamber/hypr-persist.git
cd hypr-persist
```

Build and test:

```sh
make build      # dev build
make test       # run all tests
make test-events # event/save regression harness with fake IPC
make lint       # clippy (pedantic)
make format     # rustfmt
```

The test suite runs entirely without a live Hyprland instance. It uses mock IPC sockets and simulated sessions.

## License

BSD-3-Clause
