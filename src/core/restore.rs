use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::core::layout::dwindle::{self, DwindlePlan};
use crate::core::layout::master::{self, MasterPlan};
use crate::core::quote::posix_single_quote;
use crate::core::quote::{
    WorkspaceName, exec_here, exec_on_workspace, focus_window, move_to_workspace_silent,
    move_workspace_to_monitor, normalize_windowrule_name, shell_join, shell_quote,
    workspace_switch,
};
use crate::core::state::normalize_address;
use crate::ipc::client::HyprCtl;
use crate::ipc::event_listener::parse_event;
use crate::models::{HyprEvent, SessionFile, TrackedWindow, WindowEntry};

const WINDOW_APPEAR_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the background watcher keeps listening for slow-starting apps
/// after the main restore loop finishes.
const LATE_WINDOW_GRACE_PERIOD: Duration = Duration::from_secs(60);

/// How long to wait, after a launched app's first window appears, for a
/// second window of the same class to show up (splash-window supersede).
const RETILE_GRACE_PERIOD: Duration = Duration::from_millis(800);

/// Known terminal working-directory flags, keyed by binary name.
/// Known terminal working-directory flags, keyed by binary name.
const TERMINAL_CWD_FLAGS: &[(&str, &str)] = &[
    ("ghostty", "--working-directory="),
    ("kitty", "--directory="),
    ("alacritty", "--working-directory="),
    ("wezterm", "--cwd="),
    ("foot", "--working-directory="),
    ("tilix", "--working-directory="),
    ("terminator", "--working-directory="),
];

/// Flags that force single-instance behavior via D-Bus, which prevents
/// each launched process from being independent (breaking CWD).
const SINGLE_INSTANCE_FLAGS: &[&str] = &["--gtk-single-instance=true", "--single-instance"];

/// A window that was launched but didn't appear within the per-window timeout.
/// Handed off to the background watcher for deferred placement.
struct PendingWindow {
    app_id: String,
    workspace: String,
    floating: bool,
    fullscreen: bool,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    rule_name: String,
    /// How to finalize this window's placement once it actually appears,
    /// known at plan time. Owned (not borrowed like the params used
    /// elsewhere) because it's moved into the spawned `watch_late_windows`
    /// task.
    placement: PlacementHint,
}

/// How a late-appearing window should be finalized once it's handed off
/// from the main restore loop to the background watcher. Dwindle and
/// master placement are structurally different (BSP anchor/preselect vs.
/// master/stack promotion), so this is a variant per layout rather than
/// overloading one field with a shape that only makes sense for one of them.
#[derive(Debug, Clone)]
enum PlacementHint {
    /// No special finalization beyond landing on the workspace — stack
    /// windows, the first master window, floating windows, and dwindle
    /// windows with no anchor.
    None,
    /// BSP-tree anchor sibling and preselect direction known at plan time.
    DwindleAnchor(String, dwindle::PreselDir),
    /// Promote to the master slot once placed (an "extra master" window
    /// under the master layout, beyond the first).
    MasterPromote,
}

/// Which role a window plays in a master-layout restore, if any. Governs
/// both whether the splash-supersede grace-period wait applies and whether
/// the window gets promoted to master once placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MasterRole {
    /// Not part of a master-layout restore (floating, fallback) — no
    /// splash-supersede check, no promotion.
    None,
    /// Master layout, but no promotion needed (the first master window, or
    /// a stack window) — still gets the splash-supersede check.
    Plain,
    /// Master layout, needs `layoutmsg addmaster` once placed.
    Promote,
}

/// Result of attempting to get a window onto the target workspace, one way
/// or another.
#[derive(Debug)]
enum LaunchOutcome {
    /// An already-live window (racing autostart) was claimed instead of
    /// launching a duplicate.
    Adopted(String),
    /// The window was launched and its `OpenWindow` event observed normally.
    Opened(String),
    /// Nothing appeared in time; handed off to the late-window watcher.
    Deferred,
}

/// Windows already live at daemon startup, available to be adopted in place
/// of launching a duplicate. Only matters when the daemon restarts
/// mid-session — Hyprland itself always starts with zero windows, so a real
/// login/reboot leaves this pool empty and restore behaves as before.
struct LiveWindowPool {
    windows: Vec<TrackedWindow>,
    /// Addresses present in the daemon-startup snapshot, kept separately from
    /// `windows` (which is drained by `take_match`) so a live re-query can
    /// always tell "existed before this restore" apart from "appeared since".
    known_at_startup: HashSet<String>,
    /// Addresses already handed out by `take_match` or
    /// `find_unclaimed_racing_window`, so the same live window is never
    /// adopted twice.
    claimed: HashSet<String>,
}

impl LiveWindowPool {
    fn new(windows: Vec<TrackedWindow>) -> Self {
        let known_at_startup = windows
            .iter()
            .map(|w| normalize_address(&w.address))
            .collect();
        Self {
            windows,
            known_at_startup,
            claimed: HashSet::new(),
        }
    }

    /// Find and remove the best-matching live window for a plan entry: same
    /// `app_id` and workspace, breaking ties by nearest saved position. This
    /// only helps apps that always fork a fresh process (e.g. terminals) —
    /// single-instance apps already avoid duplication via window-rule
    /// silent-activation.
    fn take_match(&mut self, entry: &WindowEntry) -> Option<TrackedWindow> {
        let best = self
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.app_id == entry.app_id && w.workspace == entry.workspace)
            .min_by_key(|(_, w)| position_distance_sq(w.position, entry.position))
            .map(|(i, _)| i)?;
        let window = self.windows.remove(best);
        self.claimed.insert(normalize_address(&window.address));
        Some(window)
    }

    /// Re-query Hyprland's live clients right now for a window of `app_id`
    /// that didn't exist at daemon startup and hasn't already been claimed.
    /// Catches an app that opened independently sometime after the startup
    /// snapshot was taken — an autostart entry can race ahead of, or during,
    /// hypr-persist's own restore sequence. Two failure modes this fixes:
    /// the class's `OpenWindow` event arrived while a different window's
    /// `wait_for_open_event` call was still draining events (and silently
    /// discarded it, since that loop only keeps events matching its own
    /// class), or the app is effectively single-instance and hypr-persist's
    /// own `exec` for it just activates the existing window without
    /// emitting a fresh event at all — either way, no timeout duration would
    /// let `wait_for_open_event` ever succeed for it.
    ///
    /// Deliberately does not filter by workspace like `take_match` does: the
    /// whole point is to catch a window that hasn't been moved to its target
    /// workspace yet (that's still the caller's job). This assumes any
    /// matching live window that appeared since startup is this entry's
    /// instance — the same heuristic `take_match` already relies on.
    async fn find_unclaimed_racing_window(
        &mut self,
        ctl: &HyprCtl,
        app_id: &str,
    ) -> Option<String> {
        let clients = ctl.get_clients().await.ok()?;
        let matched = clients.into_iter().find(|c| {
            c.class == app_id && {
                let addr = normalize_address(&c.address);
                !self.known_at_startup.contains(&addr) && !self.claimed.contains(&addr)
            }
        })?;
        let addr = normalize_address(&matched.address);
        self.claimed.insert(addr.clone());
        Some(addr)
    }

    /// Marks a window's address as claimed so a later plan entry for the
    /// same `app_id` never mistakes it for a racing autostart window via
    /// `find_unclaimed_racing_window`. Must be called for every window a
    /// plan entry ends up genuinely owning (not just adopted ones, which
    /// `find_unclaimed_racing_window` already claims) — otherwise a second
    /// same-class entry (e.g. two terminal windows) steals the first
    /// entry's freshly-opened window instead of getting its own.
    fn mark_claimed(&mut self, addr: &str) {
        self.claimed.insert(normalize_address(addr));
    }
}

fn position_distance_sq(live: (i32, i32), saved: Option<(i32, i32)>) -> i64 {
    let Some((sx, sy)) = saved else {
        return 0;
    };
    let dx = i64::from(live.0 - sx);
    let dy = i64::from(live.1 - sy);
    dx * dx + dy * dy
}

pub struct RestoreEngine {
    restore_geometry: bool,
    restore_layout: bool,
    window_appear_timeout: Duration,
}

impl RestoreEngine {
    pub const fn new(restore_geometry: bool, restore_layout: bool) -> Self {
        Self {
            restore_geometry,
            restore_layout,
            window_appear_timeout: WINDOW_APPEAR_TIMEOUT,
        }
    }

    /// Shorten the window-appear timeout so tests can exercise the
    /// timeout-recheck path without a real 15s wait.
    #[cfg(test)]
    const fn with_window_appear_timeout(mut self, timeout: Duration) -> Self {
        self.window_appear_timeout = timeout;
        self
    }

    pub async fn restore(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        live_windows: Vec<TrackedWindow>,
    ) -> Result<(RestoreReport, Option<JoinHandle<()>>)> {
        validate_session(session)?;
        let mut live_pool = LiveWindowPool::new(live_windows);
        let mut report = RestoreReport::default();
        let total = session.windows.len();
        tracing::info!(
            "restoring session '{}' ({total} apps)",
            session.session.name
        );

        let (event_tx, mut event_rx) = mpsc::channel::<HyprEvent>(256);
        let socket2 = ctl.socket_paths().socket2.clone();
        let listener = tokio::spawn(async move {
            let Ok(stream) = tokio::net::UnixStream::connect(&socket2).await else {
                tracing::error!("failed to connect to socket2 for restore events");
                return;
            };
            let reader = tokio::io::BufReader::new(stream);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let event = parse_event(line.trim());
                if matches!(&event, HyprEvent::OpenWindow { .. })
                    && event_tx.send(event).await.is_err()
                {
                    break;
                }
            }
        });

        bind_workspaces_to_monitors(session, ctl).await;

        let had_focus_on_activate = ctl
            .get_option("misc:focus_on_activate")
            .await
            .unwrap_or(true);
        if had_focus_on_activate {
            drop(ctl.keyword("misc:focus_on_activate false").await);
        }

        let mut active_rules = Vec::new();
        let mut pending = Vec::new();

        if self.restore_layout {
            self.restore_with_layout(
                session,
                ctl,
                &mut event_rx,
                &mut report,
                &mut active_rules,
                &mut pending,
                &mut live_pool,
            )
            .await?;
        } else {
            self.restore_simple(
                session,
                ctl,
                &mut event_rx,
                &mut report,
                &mut active_rules,
                &mut pending,
                &mut live_pool,
            )
            .await?;
        }

        if had_focus_on_activate {
            drop(ctl.keyword("misc:focus_on_activate true").await);
        }
        listener.abort();

        tracing::info!(
            "restore complete: {}/{total} apps ({} failed, {} pending)",
            report.restored,
            report.failed,
            pending.len()
        );

        let watcher_handle = if pending.is_empty() {
            disable_all_rules(ctl, &active_rules).await;
            None
        } else {
            tracing::info!(
                "spawning late-window watcher for {} app(s): {}",
                pending.len(),
                pending
                    .iter()
                    .map(|p| p.app_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let socket_paths = ctl.socket_paths().clone();
            let restore_geometry = self.restore_geometry;
            Some(tokio::spawn(async move {
                watch_late_windows(
                    socket_paths,
                    pending,
                    active_rules,
                    restore_geometry,
                    LATE_WINDOW_GRACE_PERIOD,
                )
                .await;
            }))
        };

        Ok((report, watcher_handle))
    }

    #[allow(clippy::too_many_arguments)]
    async fn restore_simple(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
        live_pool: &mut LiveWindowPool,
    ) -> Result<()> {
        let total = session.windows.len();
        for (i, window) in session.windows.iter().enumerate() {
            tracing::info!(
                "[{}/{}] restoring {} on workspace {}",
                i + 1,
                total,
                window.app_id,
                window.workspace
            );

            match self
                .restore_window(
                    window,
                    ctl,
                    events,
                    active_rules,
                    pending,
                    live_pool,
                    MasterRole::None,
                )
                .await
            {
                Ok(_) => {
                    report.restored += 1;
                    tracing::info!("  restored {}", window.app_id);
                }
                Err(e) => {
                    report.failed += 1;
                    report.errors.push((window.app_id.clone(), e.to_string()));
                    tracing::warn!("  failed to restore {}: {e}", window.app_id);
                }
            }
        }
        Ok(())
    }

    /// Restore windows using layout-aware strategies.
    ///
    /// Auto-detects the active Hyprland layout (dwindle, master, ...) and
    /// dispatches to the appropriate strategy. Falls back to simple restore
    /// for unknown layouts.
    /// Restore windows using layout-aware strategies.
    ///
    /// Auto-detects the active Hyprland layout (dwindle, master, ...) and
    /// dispatches to the appropriate strategy. Falls back to simple restore
    /// for unknown layouts.
    #[allow(clippy::too_many_arguments)]
    async fn restore_with_layout(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
        live_pool: &mut LiveWindowPool,
    ) -> Result<()> {
        let layout = ctl.get_layout().await.unwrap_or_default();
        tracing::info!("detected layout: {layout:?}");

        match layout.as_str() {
            "dwindle" => {
                self.restore_dwindle(
                    session,
                    ctl,
                    events,
                    report,
                    active_rules,
                    pending,
                    live_pool,
                )
                .await
            }
            "master" => {
                self.restore_master(
                    session,
                    ctl,
                    events,
                    report,
                    active_rules,
                    pending,
                    live_pool,
                )
                .await
            }
            other => {
                tracing::warn!(
                    "layout {other:?} has no layout-aware restore, falling back to simple"
                );
                self.restore_simple(
                    session,
                    ctl,
                    events,
                    report,
                    active_rules,
                    pending,
                    live_pool,
                )
                .await
            }
        }
    }

    /// Dwindle restore: BSP inference, preselect-based placement, then
    /// splitratio application and convergence.
    #[allow(clippy::too_many_arguments)]
    async fn restore_dwindle(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
        live_pool: &mut LiveWindowPool,
    ) -> Result<()> {
        let (floating, ws_plans, fallback_windows) = Self::build_dwindle_plans(session);

        let (addresses, adopted) = self
            .execute_bsp_plans(
                session,
                ctl,
                events,
                report,
                &ws_plans,
                active_rules,
                pending,
                live_pool,
            )
            .await?;

        self.apply_split_ratios(ctl, &ws_plans, &addresses, &adopted)
            .await;
        self.converge_split_ratios(&ws_plans, ctl, &addresses, &adopted)
            .await;
        self.converge_tiled_sizes(session, ctl, &addresses, &adopted)
            .await;
        self.apply_fullscreen(session, ctl, &addresses, &adopted)
            .await?;

        self.restore_indexed(
            session,
            ctl,
            events,
            report,
            &fallback_windows,
            "fallback",
            active_rules,
            pending,
            live_pool,
        )
        .await?;
        self.restore_indexed(
            session,
            ctl,
            events,
            report,
            &floating,
            "float",
            active_rules,
            pending,
            live_pool,
        )
        .await?;

        Ok(())
    }

    /// Master layout restore: infer master/stack split, set orientation and
    /// mfact, open master windows first then stack windows.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn restore_master(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
        live_pool: &mut LiveWindowPool,
    ) -> Result<()> {
        let (floating, master_plans, fallback_windows) = Self::build_master_plans(session);

        let mut sorted_ws: Vec<&String> = master_plans.keys().collect();
        sorted_ws.sort();

        for ws in sorted_ws {
            let plan = &master_plans[ws];
            tracing::info!(
                "[master] workspace {ws}: orientation={}, mfact={:.3}, {} master + {} stack",
                plan.orientation,
                plan.mfact,
                plan.master_indices.len(),
                plan.stack_indices.len()
            );

            // Set the default mfact before opening windows so the layout
            // engine uses it for initial placement on this workspace.
            drop(
                ctl.keyword(&format!("master:mfact {:.6}", plan.mfact))
                    .await,
            );

            ctl.dispatch(&workspace_switch(ws)?).await?;
            drop(
                ctl.dispatch(&format!("layoutmsg orientation{}", plan.orientation))
                    .await,
            );

            // Open the first master window.
            if let Some(&first_idx) = plan.master_indices.first() {
                let window = &session.windows[first_idx];
                tracing::info!("[master] opening master: {}", window.app_id);
                match self
                    .restore_window(
                        window,
                        ctl,
                        events,
                        active_rules,
                        pending,
                        live_pool,
                        MasterRole::Plain,
                    )
                    .await
                {
                    Ok(_) => report.restored += 1,
                    Err(e) => {
                        report.failed += 1;
                        report.errors.push((window.app_id.clone(), e.to_string()));
                    }
                }
            }

            // Open additional master windows and promote them. Promotion is
            // handled inside restore_window/launch_and_track (gated on
            // MasterRole::Promote), which skips it for adopted windows: an
            // already-live window is already in its correct slot, and
            // addmaster would instead promote whatever happens to be
            // focused. A window that times out gets the promotion deferred
            // to the late-window watcher instead of firing immediately here
            // against whatever's currently focused.
            for &idx in plan.master_indices.iter().skip(1) {
                let window = &session.windows[idx];
                tracing::info!("[master] opening extra master: {}", window.app_id);
                match self
                    .restore_window(
                        window,
                        ctl,
                        events,
                        active_rules,
                        pending,
                        live_pool,
                        MasterRole::Promote,
                    )
                    .await
                {
                    Ok(_) => report.restored += 1,
                    Err(e) => {
                        report.failed += 1;
                        report.errors.push((window.app_id.clone(), e.to_string()));
                    }
                }
            }

            // Open stack windows in order.
            for &idx in &plan.stack_indices {
                let window = &session.windows[idx];
                tracing::info!("[master] opening stack: {}", window.app_id);
                match self
                    .restore_window(
                        window,
                        ctl,
                        events,
                        active_rules,
                        pending,
                        live_pool,
                        MasterRole::Plain,
                    )
                    .await
                {
                    Ok(_) => report.restored += 1,
                    Err(e) => {
                        report.failed += 1;
                        report.errors.push((window.app_id.clone(), e.to_string()));
                    }
                }
            }

            // Nothing else needed: the `master:mfact` keyword set before
            // window placement is used by the layout engine for this workspace.
        }

        self.restore_indexed(
            session,
            ctl,
            events,
            report,
            &fallback_windows,
            "fallback",
            active_rules,
            pending,
            live_pool,
        )
        .await?;
        self.restore_indexed(
            session,
            ctl,
            events,
            report,
            &floating,
            "float",
            active_rules,
            pending,
            live_pool,
        )
        .await?;

        Ok(())
    }

    fn build_dwindle_plans(
        session: &SessionFile,
    ) -> (Vec<usize>, HashMap<String, DwindlePlan>, Vec<usize>) {
        let floating: Vec<usize> = session
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.floating)
            .map(|(i, _)| i)
            .collect();

        let mut ws_groups: HashMap<&str, (Vec<&WindowEntry>, Vec<usize>)> = HashMap::new();
        for (i, w) in session.windows.iter().enumerate() {
            if !w.floating {
                let entry = ws_groups.entry(&w.workspace).or_default();
                entry.0.push(w);
                entry.1.push(i);
            }
        }

        let mut ws_plans: HashMap<String, DwindlePlan> = HashMap::new();
        let mut fallback_windows: Vec<usize> = Vec::new();

        for (ws, (wins, indices)) in &ws_groups {
            if let Some(plan) = dwindle::build_workspace_plan(wins, indices) {
                tracing::info!(
                    "workspace {ws}: inferred BSP layout for {} windows ({} ratio steps)",
                    wins.len(),
                    plan.ratio_steps.len(),
                );
                ws_plans.insert((*ws).to_string(), plan);
            } else {
                tracing::warn!(
                    "workspace {ws}: could not infer BSP layout, falling back to simple restore"
                );
                fallback_windows.extend_from_slice(indices);
            }
        }

        (floating, ws_plans, fallback_windows)
    }

    fn build_master_plans(
        session: &SessionFile,
    ) -> (Vec<usize>, HashMap<String, MasterPlan>, Vec<usize>) {
        let floating: Vec<usize> = session
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.floating)
            .map(|(i, _)| i)
            .collect();

        let mut ws_groups: HashMap<&str, (Vec<&WindowEntry>, Vec<usize>)> = HashMap::new();
        for (i, w) in session.windows.iter().enumerate() {
            if !w.floating {
                let entry = ws_groups.entry(&w.workspace).or_default();
                entry.0.push(w);
                entry.1.push(i);
            }
        }

        let mut master_plans: HashMap<String, MasterPlan> = HashMap::new();
        let mut fallback_windows: Vec<usize> = Vec::new();

        for (ws, (wins, indices)) in &ws_groups {
            if let Some(plan) = master::build_workspace_plan(wins, indices) {
                tracing::info!(
                    "workspace {ws}: inferred master layout ({} master, {} stack, orientation={})",
                    plan.master_indices.len(),
                    plan.stack_indices.len(),
                    plan.orientation,
                );
                master_plans.insert((*ws).to_string(), plan);
            } else {
                tracing::warn!(
                    "workspace {ws}: could not infer master layout, falling back to simple restore"
                );
                fallback_windows.extend_from_slice(indices);
            }
        }

        (floating, master_plans, fallback_windows)
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_bsp_plans(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        ws_plans: &HashMap<String, DwindlePlan>,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
        live_pool: &mut LiveWindowPool,
    ) -> Result<(HashMap<usize, String>, HashSet<usize>)> {
        let mut addresses: HashMap<usize, String> = HashMap::new();
        let mut adopted: HashSet<usize> = HashSet::new();
        let mut sorted_ws: Vec<&String> = ws_plans.keys().collect();
        sorted_ws.sort();
        let mut rule_counter = 0usize;

        for ws in sorted_ws {
            let plan = &ws_plans[ws];
            for (i, step) in plan.steps.iter().enumerate() {
                let window = &session.windows[step.window_idx];
                tracing::info!(
                    "[layout] restoring {} on workspace {} (focus={:?}, presel={:?})",
                    window.app_id,
                    window.workspace,
                    step.focus_idx,
                    step.preselect,
                );

                if let Some(live) = live_pool.take_match(window) {
                    let addr = normalize_address(&live.address);
                    tracing::info!("  adopted already-open {} (0x{addr})", window.app_id);
                    addresses.insert(step.window_idx, addr);
                    adopted.insert(step.window_idx);
                    report.restored += 1;
                    continue;
                }

                // Deliberately does NOT arm focuswindow/preselect here: a
                // binding set up before `exec` has to survive an
                // indeterminate wait for the window to actually appear
                // (`bsp_launch_and_track` -> `wait_for_open_event`), and
                // Hyprland's pending preselect is tied to whatever window is
                // focused at the moment it's consumed — any intervening
                // focus change (e.g. an unrelated window closing while a
                // slow-launching app is still starting) silently invalidates
                // it. `anchor` is instead handed to `place_window_in_bsp_slot`
                // (via `bsp_launch_and_track`/`retile_superseding_window`),
                // which issues focus+preselect fresh immediately before the
                // settle, once the window's address is already confirmed to
                // exist.
                let anchor: Option<(&str, dwindle::PreselDir)> =
                    if let (Some(focus_idx), Some(presel)) = (step.focus_idx, step.preselect)
                        && let Some(focus_addr) = addresses.get(&focus_idx)
                    {
                        Some((focus_addr.as_str(), presel))
                    } else {
                        if i == 0 {
                            ctl.dispatch(&workspace_switch(ws)?).await?;
                        }
                        None
                    };

                match self
                    .bsp_launch_and_track(
                        window,
                        ctl,
                        events,
                        active_rules,
                        pending,
                        &mut rule_counter,
                        anchor,
                        live_pool,
                    )
                    .await
                {
                    Ok(LaunchOutcome::Adopted(addr)) => {
                        addresses.insert(step.window_idx, addr);
                        adopted.insert(step.window_idx);
                        report.restored += 1;
                        tracing::info!("  adopted {} (racing autostart)", window.app_id);
                    }
                    Ok(LaunchOutcome::Opened(addr)) => {
                        addresses.insert(step.window_idx, addr);
                        report.restored += 1;
                        tracing::info!("  restored {}", window.app_id);
                    }
                    Ok(LaunchOutcome::Deferred) => {
                        report.restored += 1;
                        tracing::info!("  launched {} (no window event)", window.app_id);
                    }
                    Err(e) => {
                        report.failed += 1;
                        report.errors.push((window.app_id.clone(), e.to_string()));
                        tracing::warn!("  failed to restore {}: {e}", window.app_id);
                    }
                }
            }
        }

        Ok((addresses, adopted))
    }

    /// BSP-specific launch: switches to the workspace first (so preselect
    /// works even for single-instance apps), strips single-instance flags,
    /// and does NOT use `[workspace N silent]` since we are already on the
    /// target workspace.
    ///
    /// The rule is disabled immediately once the window appears so that a
    /// subsequent window of the same class never sees a stale rule. Only
    /// rules for timed-out windows are kept alive for the background watcher.
    #[allow(clippy::too_many_arguments)]
    async fn bsp_launch_and_track(
        &self,
        window: &WindowEntry,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
        rule_counter: &mut usize,
        anchor: Option<(&str, dwindle::PreselDir)>,
        live_pool: &mut LiveWindowPool,
    ) -> Result<LaunchOutcome> {
        if let Some(addr) = live_pool
            .find_unclaimed_racing_window(ctl, &window.app_id)
            .await
        {
            tracing::info!(
                "  {} already live before launch (racing autostart), adopting 0x{addr}",
                window.app_id
            );
            place_window_in_bsp_slot(ctl, &addr, &window.workspace, anchor).await;
            return Ok(LaunchOutcome::Adopted(addr));
        }

        let rule_name = format!(
            "hypr-persist-{}-{rule_counter}",
            normalize_windowrule_name(&window.app_id)?,
        );
        *rule_counter += 1;
        let class_escaped = regex::escape(&window.app_id);
        let workspace = WorkspaceName::parse(&window.workspace)?;

        ctl.keyword(&format!(
            "windowrule[{rule_name}]:match:class ^({class_escaped})$"
        ))
        .await?;
        ctl.keyword(&format!(
            "windowrule[{rule_name}]:workspace {} silent",
            workspace.as_str()
        ))
        .await?;
        let launch_cmd = build_bsp_launch_cmd(window);
        ctl.dispatch(&exec_here(&launch_cmd))
            .await
            .with_context(|| format!("launching {}", window.launch_cmd))?;

        if let Some(ref addr) = self.wait_for_open_event(events, &window.app_id).await {
            tracing::debug!("  {} appeared at 0x{addr}", window.app_id);
            disable_all_rules(ctl, &[rule_name]).await;
            drop(
                ctl.dispatch(&move_to_workspace_silent(&window.workspace, addr)?)
                    .await,
            );
            let final_addr = self
                .retile_superseding_window(
                    ctl,
                    events,
                    addr,
                    &window.app_id,
                    &window.workspace,
                    anchor,
                )
                .await;
            live_pool.mark_claimed(&final_addr);
            Ok(LaunchOutcome::Opened(final_addr))
        } else if let Some(addr) = live_pool
            .find_unclaimed_racing_window(ctl, &window.app_id)
            .await
        {
            tracing::info!(
                "  {} appeared via racing autostart while waiting, adopting 0x{addr} instead of deferring",
                window.app_id
            );
            disable_all_rules(ctl, &[rule_name]).await;
            place_window_in_bsp_slot(ctl, &addr, &window.workspace, anchor).await;
            Ok(LaunchOutcome::Adopted(addr))
        } else {
            tracing::warn!(
                "{} did not appear within {}s, deferring to late-window watcher",
                window.app_id,
                self.window_appear_timeout.as_secs()
            );
            active_rules.push(rule_name.clone());
            pending.push(PendingWindow {
                app_id: window.app_id.clone(),
                workspace: window.workspace.clone(),
                floating: window.floating,
                fullscreen: window.fullscreen,
                position: window.position,
                size: window.size,
                rule_name,
                placement: match anchor {
                    Some((addr, presel)) => PlacementHint::DwindleAnchor(addr.to_string(), presel),
                    None => PlacementHint::None,
                },
            });
            Ok(LaunchOutcome::Deferred)
        }
    }

    /// After a launched app's first window appears, wait briefly to see if a
    /// second window of the same class shows up too — some apps (observed:
    /// Discord) open a transient splash window before their real main
    /// window, both sharing the same class. If a second window appears, the
    /// first is a stale splash: close it and finalize placement against the
    /// real one instead.
    ///
    /// Either way — splash superseded or not — the window that ends up
    /// tracked always gets its BSP placement finalized here via
    /// `place_window_in_bsp_slot`, rather than trusting a preselect binding
    /// armed before `exec` to survive an indeterminate wait for `OpenWindow`.
    /// Live-tested regression: an anchored window (Slack, anchored to
    /// Discord) took ~7s to actually open; an unrelated window closing
    /// during that wait (ordinary session-startup churn, which shifts
    /// Hyprland's focused window as a side effect of closing) silently
    /// invalidated the pre-armed preselect, so Slack landed grouped with
    /// unrelated windows instead of next to its anchor despite every earlier
    /// dispatch reporting success. Finalizing the anchor/preselect dance
    /// here — after the window's address is already confirmed to exist —
    /// mirrors the same already-proven-robust pattern used for
    /// racing-adopted and late-appearing windows.
    ///
    /// Returns the address that should be tracked as this window's final
    /// address — either `first_addr` unchanged if no second window showed
    /// up, or the real window's address.
    async fn retile_superseding_window(
        &self,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        first_addr: &str,
        class: &str,
        workspace: &str,
        anchor: Option<(&str, dwindle::PreselDir)>,
    ) -> String {
        let final_addr = self
            .resolve_splash_supersede(ctl, events, first_addr, class)
            .await;

        place_window_in_bsp_slot(ctl, &final_addr, workspace, anchor).await;

        final_addr
    }

    /// Wait briefly to see if a second window of the same class shows up
    /// after the first — some apps (observed: Discord) open a transient
    /// splash window before their real main window, both sharing the same
    /// class. If a second window appears within the grace period, the first
    /// is a stale splash: close it and report the real window's address
    /// instead. Shared between the dwindle and master paths, which each
    /// finalize placement differently afterwards (`place_window_in_bsp_slot`
    /// vs. `layoutmsg addmaster`/stack ordering) — this only resolves which
    /// address is the real one to place.
    async fn resolve_splash_supersede(
        &self,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        first_addr: &str,
        class: &str,
    ) -> String {
        if let Some(real_addr) = self
            .wait_for_open_event_within(events, class, RETILE_GRACE_PERIOD)
            .await
        {
            tracing::info!(
                "retile: {class} splash (0x{first_addr}) superseded by real window (0x{real_addr})"
            );
            if ctl
                .dispatch(&format!("closewindow address:0x{first_addr}"))
                .await
                .is_ok()
            {
                tracing::debug!("retile: close stale ok");
            }
            real_addr
        } else {
            first_addr.to_string()
        }
    }

    /// Apply `layoutmsg splitratio <delta>` for each split node in the BSP tree
    /// that has a direct leaf child. The delta is computed from the default 0.5
    /// ratio since freshly-created windows always start at the default — this
    /// does not hold for adopted windows (already live before this restore, at
    /// whatever ratio they already have), so those are skipped entirely.
    async fn apply_split_ratios(
        &self,
        ctl: &HyprCtl,
        ws_plans: &HashMap<String, DwindlePlan>,
        addresses: &HashMap<usize, String>,
        adopted: &HashSet<usize>,
    ) {
        let mut applied = 0usize;
        let mut sorted_ws: Vec<&String> = ws_plans.keys().collect();
        sorted_ws.sort();

        for ws in sorted_ws {
            let plan = &ws_plans[ws];
            for step in &plan.ratio_steps {
                if adopted.contains(&step.focus_window_idx) {
                    continue;
                }
                let Some(addr) = addresses.get(&step.focus_window_idx) else {
                    continue;
                };
                let Ok(cmd) = focus_window(addr) else {
                    continue;
                };
                if let Err(e) = ctl.dispatch(&cmd).await {
                    tracing::warn!("splitratio: focus failed: {e}");
                    continue;
                }
                let delta = step.ratio - 0.5;
                match ctl
                    .dispatch(&format!("layoutmsg splitratio {delta:.6}"))
                    .await
                {
                    Ok(resp) if resp.trim() != "ok" => {
                        tracing::warn!("splitratio: unexpected response: {resp}");
                    }
                    Err(e) => tracing::warn!("splitratio: ipc error: {e}"),
                    _ => applied += 1,
                }
            }
        }

        if applied > 0 {
            tracing::info!("applied {applied} split ratios");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    /// Correct BSP split nodes that `apply_split_ratios` cannot reach: those
    /// whose two children are both internal sub-trees, so no single window
    /// is directly parented by the split (see `dwindle::PixelRatioStep`).
    /// `layoutmsg splitratio` only ever adjusts the currently focused
    /// window's own immediate parent split, so these are unreachable that
    /// way — but `resizewindowpixel` walks from the resized window up to
    /// the nearest ancestor split matching the resize axis, so resizing the
    /// leftmost leaf of one side by the delta needed to hit the split's
    /// saved ratio reaches exactly this split. Only the split's own axis is
    /// touched (the other delta is always 0) so this never also perturbs an
    /// orthogonal ancestor split.
    ///
    /// Adopted windows are skipped on either side of the split, same reason
    /// as `converge_tiled_sizes`: an adopted window's tree shape is already
    /// live and shouldn't be nudged.
    ///
    /// Same one-candidate-per-pass and stuck-delta dedup invariants as
    /// `converge_tiled_sizes`, for the same reason: resizing one split can
    /// change the live geometry a later step measures, so issuing more than
    /// one per pass risks the same shared-edge fight.
    async fn converge_split_ratios(
        &self,
        ws_plans: &HashMap<String, DwindlePlan>,
        ctl: &HyprCtl,
        addresses: &HashMap<usize, String>,
        adopted: &HashSet<usize>,
    ) {
        const TOLERANCE: i32 = 6;

        let mut sorted_ws: Vec<&String> = ws_plans.keys().collect();
        sorted_ws.sort();

        let mut candidates: Vec<&dwindle::PixelRatioStep> = Vec::new();
        for ws in sorted_ws {
            for step in &ws_plans[ws].pixel_ratio_steps {
                if adopted.contains(&step.first_leaf_idx) || adopted.contains(&step.second_leaf_idx)
                {
                    continue;
                }
                candidates.push(step);
            }
        }

        if candidates.is_empty() {
            return;
        }

        let max_passes = candidates.len() + 2;
        let mut last_attempted: HashMap<(usize, usize), i32> = HashMap::new();

        for pass in 0..max_passes {
            let mut dispatched = false;
            let mut any_out_of_tolerance = false;

            for step in &candidates {
                let key = (step.first_leaf_idx, step.second_leaf_idx);
                let Some(first_addr) = addresses.get(&step.first_leaf_idx) else {
                    continue;
                };
                let Some(second_addr) = addresses.get(&step.second_leaf_idx) else {
                    continue;
                };
                let Ok(Some(first_client)) = ctl.get_client_by_address(first_addr).await else {
                    continue;
                };
                let Ok(Some(second_client)) = ctl.get_client_by_address(second_addr).await else {
                    continue;
                };

                let (first_extent, second_extent) = match step.dir {
                    dwindle::SplitDir::Horizontal => (first_client.size.0, second_client.size.0),
                    dwindle::SplitDir::Vertical => (first_client.size.1, second_client.size.1),
                };
                let total = first_extent + second_extent;
                // Window pixel extents never approach i32::MAX, so the rounded
                // ratio product can't truncate in practice.
                #[allow(clippy::cast_possible_truncation)]
                let target_first = (step.ratio * f64::from(total)).round() as i32;
                let delta = target_first - first_extent;

                if delta.abs() > TOLERANCE {
                    any_out_of_tolerance = true;

                    if last_attempted.get(&key) == Some(&delta) {
                        tracing::debug!(
                            "  pass {}: split ratio for ({}, {}) still off by {delta}, matching \
                             the previous attempt — not re-dispatching, checking other \
                             candidates",
                            pass + 1,
                            step.first_leaf_idx,
                            step.second_leaf_idx,
                        );
                        continue;
                    }

                    tracing::debug!(
                        "  pass {}: correct split ratio via ({}, {}) by {delta}",
                        pass + 1,
                        step.first_leaf_idx,
                        step.second_leaf_idx,
                    );
                    last_attempted.insert(key, delta);

                    let (dw, dh) = match step.dir {
                        dwindle::SplitDir::Horizontal => (delta, 0),
                        dwindle::SplitDir::Vertical => (0, delta),
                    };
                    match ctl
                        .dispatch(&format!(
                            "resizewindowpixel {dw} {dh},address:0x{first_addr}"
                        ))
                        .await
                    {
                        Ok(resp) if resp.trim() != "ok" => {
                            tracing::warn!("  split-ratio resize failed: {resp}");
                        }
                        Err(e) => tracing::warn!("  split-ratio resize ipc error: {e}"),
                        _ => {}
                    }
                    dispatched = true;
                    break;
                }
            }

            if !any_out_of_tolerance {
                tracing::debug!("  split ratios converged after {} pass(es)", pass + 1);
                return;
            }

            if !dispatched {
                tracing::debug!(
                    "  split ratios settled after {} pass(es); remaining candidates unchanged \
                     since their last attempt",
                    pass + 1
                );
                return;
            }

            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        tracing::debug!("  split ratios settled after {max_passes} passes");
    }

    /// Adopted windows are skipped: they were already live before this
    /// restore, in whatever shape the current tree gives them, so nudging
    /// them toward the saved size fights the tree's actual layout instead of
    /// correcting anything (observed: a window driven to near-zero size).
    ///
    /// Resizes dispatch the saved *absolute* size (`resizewindowpixel exact`)
    /// rather than a delta computed from the current live size. A relative
    /// delta was live-tested and observed to get rejected as "Invalid size"
    /// for small, entirely reasonable-looking deltas. An absolute target
    /// sidesteps that rejection.
    ///
    /// A candidate is only re-dispatched across passes if its mismatch
    /// *strictly improved* since its last dispatch. Live-tested and observed:
    /// simply re-issuing the identical `exact` resize to the same
    /// already-near-its-floor tiled window repeatedly (nothing else competing
    /// for the one-per-pass slot, so it kept winning every pass) made the
    /// mismatch **worse** each time — doubling every pass — until the window
    /// was squeezed to a sliver, with no error ever reported. Retrying only
    /// on strict improvement means a window whose resize genuinely had no
    /// effect (unchanged) or made things worse gets exactly one attempt and
    /// is then left alone for the rest of this convergence run, while a
    /// window that's gradually settling toward its target across passes
    /// keeps getting corrective dispatches until it's within tolerance.
    ///
    /// At most one window is resized per pass. `resizewindowpixel` walks up
    /// the BSP tree to the nearest ancestor split matching each axis, so
    /// correcting one window's width or height can silently correct a
    /// sibling sharing that same ancestor split too. Serializing to one
    /// resize per pass means a sibling whose mismatch was resolved as a side
    /// effect simply reads as already converged next pass, and only
    /// genuinely independent windows consume their own pass.
    async fn converge_tiled_sizes(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        addresses: &HashMap<usize, String>,
        adopted: &HashSet<usize>,
    ) {
        const TOLERANCE: i32 = 6;

        let mut candidates: Vec<(usize, &String)> = addresses
            .iter()
            .filter(|(idx, _)| !adopted.contains(idx))
            .map(|(idx, addr)| (*idx, addr))
            .collect();
        candidates.sort_by_key(|(idx, _)| *idx);

        if candidates.is_empty() {
            return;
        }

        // Generous margin over one-resize-per-pass: every candidate may need
        // its own pass, plus room for a couple of settling passes.
        let max_passes = candidates.len() + 2;

        // Tracks the best (smallest) mismatch magnitude seen at each
        // candidate's last dispatch. A candidate is only dispatched again if
        // its current mismatch is strictly smaller than that — i.e. it's
        // provably making progress, not just producing a different (equal or
        // worse) number than last time.
        let mut best_seen: HashMap<usize, i32> = HashMap::new();

        for pass in 0..max_passes {
            let mut dispatched = false;
            let mut any_out_of_tolerance = false;

            for (idx, addr) in &candidates {
                let window = &session.windows[*idx];
                let Some((saved_w, saved_h)) = window.size else {
                    continue;
                };
                let Ok(Some(client)) = ctl.get_client_by_address(addr).await else {
                    continue;
                };

                let dw = saved_w - client.size.0;
                let dh = saved_h - client.size.1;

                if dw.abs() > TOLERANCE || dh.abs() > TOLERANCE {
                    any_out_of_tolerance = true;
                    let magnitude = dw.abs() + dh.abs();

                    if best_seen.get(idx).is_some_and(|&best| magnitude >= best) {
                        tracing::debug!(
                            "  pass {}: {} still off by ({dw}, {dh}), not an improvement \
                             over its last attempt — not re-dispatching, checking other \
                             candidates",
                            pass + 1,
                            window.app_id,
                        );
                        continue;
                    }

                    tracing::debug!(
                        "  pass {}: resizing {} to exact {saved_w}x{saved_h} \
                         (currently off by ({dw}, {dh}))",
                        pass + 1,
                        window.app_id,
                    );
                    best_seen.insert(*idx, magnitude);
                    match ctl
                        .dispatch(&format!(
                            "resizewindowpixel exact {saved_w} {saved_h},address:0x{addr}"
                        ))
                        .await
                    {
                        Ok(resp) if resp.trim() != "ok" => {
                            tracing::warn!("  resize failed: {resp}");
                        }
                        Err(e) => tracing::warn!("  resize ipc error: {e}"),
                        _ => {}
                    }
                    dispatched = true;
                    break;
                }
            }

            if !any_out_of_tolerance {
                tracing::debug!("  tiled sizes converged after {} pass(es)", pass + 1);
                return;
            }

            if !dispatched {
                tracing::debug!(
                    "  tiled sizes settled after {} pass(es); remaining candidates not \
                     improving since their last attempt",
                    pass + 1
                );
                return;
            }

            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        tracing::debug!("  tiled sizes settled after {max_passes} passes");
    }

    /// `fullscreen` is a toggle, not a set — an adopted window that was
    /// already fullscreen before this restore would be un-fullscreened by
    /// re-issuing it, so adopted windows are skipped here too.
    async fn apply_fullscreen(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        addresses: &HashMap<usize, String>,
        adopted: &HashSet<usize>,
    ) -> Result<()> {
        for (idx, addr) in addresses {
            if adopted.contains(idx) {
                continue;
            }
            let window = &session.windows[*idx];
            if window.fullscreen {
                ctl.dispatch(&format!("fullscreen 0,address:0x{addr}"))
                    .await?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn restore_indexed(
        &self,
        session: &SessionFile,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        report: &mut RestoreReport,
        indices: &[usize],
        label: &str,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
        live_pool: &mut LiveWindowPool,
    ) -> Result<()> {
        for &idx in indices {
            let window = &session.windows[idx];
            tracing::info!(
                "[{label}] restoring {} on workspace {}",
                window.app_id,
                window.workspace
            );
            match self
                .restore_window(
                    window,
                    ctl,
                    events,
                    active_rules,
                    pending,
                    live_pool,
                    MasterRole::None,
                )
                .await
            {
                Ok(_) => report.restored += 1,
                Err(e) => {
                    report.failed += 1;
                    report.errors.push((window.app_id.clone(), e.to_string()));
                }
            }
        }
        Ok(())
    }

    /// Launch a window with workspace placement via named rules, wait for it
    /// to appear, then return its address.
    ///
    /// The rule is disabled immediately once the window appears so that a
    /// subsequent window of the same class never sees a stale rule. Only
    /// rules for timed-out windows are kept alive (pushed to `active_rules`)
    /// for the background late-window watcher.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn launch_and_track(
        &self,
        window: &WindowEntry,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
        live_pool: &mut LiveWindowPool,
        master_role: MasterRole,
    ) -> Result<LaunchOutcome> {
        if let Some(addr) = live_pool
            .find_unclaimed_racing_window(ctl, &window.app_id)
            .await
        {
            tracing::info!(
                "  {} already live before launch (racing autostart), adopting 0x{addr}",
                window.app_id
            );
            drop(
                ctl.dispatch(&move_to_workspace_silent(&window.workspace, &addr)?)
                    .await,
            );
            return Ok(LaunchOutcome::Adopted(addr));
        }

        let rule_name = format!(
            "hypr-persist-{}-{}",
            normalize_windowrule_name(&window.app_id)?,
            active_rules.len()
        );
        let class_escaped = regex::escape(&window.app_id);
        let workspace = WorkspaceName::parse(&window.workspace)?;

        ctl.keyword(&format!(
            "windowrule[{rule_name}]:match:class ^({class_escaped})$"
        ))
        .await?;
        ctl.keyword(&format!(
            "windowrule[{rule_name}]:workspace {} silent",
            workspace.as_str()
        ))
        .await?;

        let launch_cmd = build_launch_cmd(window);
        ctl.dispatch(&exec_on_workspace(&window.workspace, &launch_cmd)?)
            .await
            .with_context(|| format!("launching {}", window.launch_cmd))?;

        if let Some(ref addr) = self.wait_for_open_event(events, &window.app_id).await {
            tracing::debug!("  {} appeared at 0x{addr}", window.app_id);
            disable_all_rules(ctl, &[rule_name]).await;

            let final_addr = if master_role == MasterRole::None {
                addr.clone()
            } else {
                self.resolve_splash_supersede(ctl, events, addr, &window.app_id)
                    .await
            };

            drop(
                ctl.dispatch(&move_to_workspace_silent(&window.workspace, &final_addr)?)
                    .await,
            );
            if master_role == MasterRole::Promote {
                drop(ctl.dispatch("layoutmsg addmaster").await);
            }
            live_pool.mark_claimed(&final_addr);
            Ok(LaunchOutcome::Opened(final_addr))
        } else if let Some(addr) = live_pool
            .find_unclaimed_racing_window(ctl, &window.app_id)
            .await
        {
            tracing::info!(
                "  {} appeared via racing autostart while waiting, adopting 0x{addr} instead of deferring",
                window.app_id
            );
            disable_all_rules(ctl, &[rule_name]).await;
            drop(
                ctl.dispatch(&move_to_workspace_silent(&window.workspace, &addr)?)
                    .await,
            );
            Ok(LaunchOutcome::Adopted(addr))
        } else {
            tracing::warn!(
                "{} did not appear within {}s, deferring to late-window watcher",
                window.app_id,
                self.window_appear_timeout.as_secs()
            );
            active_rules.push(rule_name.clone());
            pending.push(PendingWindow {
                app_id: window.app_id.clone(),
                workspace: window.workspace.clone(),
                floating: window.floating,
                fullscreen: window.fullscreen,
                position: window.position,
                size: window.size,
                rule_name,
                placement: if master_role == MasterRole::Promote {
                    PlacementHint::MasterPromote
                } else {
                    PlacementHint::None
                },
            });
            Ok(LaunchOutcome::Deferred)
        }
    }

    /// Restore a single window. Returns `Ok(true)` if an already-live window
    /// was adopted instead of launching a duplicate, `Ok(false)` otherwise
    /// (freshly launched, or deferred to the late-window watcher).
    #[allow(clippy::too_many_arguments)]
    async fn restore_window(
        &self,
        window: &WindowEntry,
        ctl: &HyprCtl,
        events: &mut mpsc::Receiver<HyprEvent>,
        active_rules: &mut Vec<String>,
        pending: &mut Vec<PendingWindow>,
        live_pool: &mut LiveWindowPool,
        master_role: MasterRole,
    ) -> Result<bool> {
        if let Some(live) = live_pool.take_match(window) {
            tracing::info!(
                "  adopted already-open {} (0x{})",
                window.app_id,
                normalize_address(&live.address)
            );
            return Ok(true);
        }

        let outcome = self
            .launch_and_track(
                window,
                ctl,
                events,
                active_rules,
                pending,
                live_pool,
                master_role,
            )
            .await?;

        let addr = match outcome {
            LaunchOutcome::Adopted(addr) => {
                tracing::info!("  adopted {} (racing autostart, 0x{addr})", window.app_id);
                return Ok(true);
            }
            LaunchOutcome::Opened(addr) => addr,
            LaunchOutcome::Deferred => return Ok(false),
        };

        if self.restore_geometry {
            if window.floating
                && let (Some((x, y)), Some((w, h))) = (window.position, window.size)
            {
                ctl.dispatch(&format!("setfloating address:0x{addr}"))
                    .await?;
                ctl.dispatch(&format!("resizewindowpixel exact {w} {h},address:0x{addr}"))
                    .await?;
                ctl.dispatch(&format!("movewindowpixel exact {x} {y},address:0x{addr}"))
                    .await?;
            }

            if window.fullscreen {
                ctl.dispatch(&format!("fullscreen 0,address:0x{addr}"))
                    .await?;
            }
        }

        Ok(false)
    }

    async fn wait_for_open_event(
        &self,
        events: &mut mpsc::Receiver<HyprEvent>,
        app_id: &str,
    ) -> Option<String> {
        self.wait_for_open_event_within(events, app_id, self.window_appear_timeout)
            .await
    }

    async fn wait_for_open_event_within(
        &self,
        events: &mut mpsc::Receiver<HyprEvent>,
        app_id: &str,
        timeout: Duration,
    ) -> Option<String> {
        tokio::time::timeout(timeout, async {
            while let Some(event) = events.recv().await {
                if let HyprEvent::OpenWindow { address, class, .. } = event
                    && class == app_id
                {
                    return Some(address);
                }
            }
            None
        })
        .await
        .unwrap_or(None)
    }
}

/// Background task that watches for windows that didn't appear during the main
/// restore loop. Keeps their Hyprland window rules active and listens on
/// socket2 until every pending window appears or the grace period expires.
async fn watch_late_windows(
    paths: crate::ipc::client::HyprSocketPaths,
    mut pending: Vec<PendingWindow>,
    all_rules: Vec<String>,
    restore_geometry: bool,
    grace_period: Duration,
) {
    let ctl = HyprCtl::new(paths.clone());
    let mut resolved_rules: Vec<String> = Vec::new();

    let Ok(stream) = tokio::net::UnixStream::connect(&paths.socket2).await else {
        tracing::error!("late-window watcher: failed to connect to socket2, disabling rules");
        disable_all_rules(&ctl, &all_rules).await;
        return;
    };

    let reader = tokio::io::BufReader::new(stream);
    let mut lines = reader.lines();
    let deadline = tokio::time::Instant::now() + grace_period;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                let event = parse_event(line.trim());
                if let HyprEvent::OpenWindow { address, class, .. } = event
                    && let Some(idx) = pending.iter().position(|p| p.app_id == class)
                {
                    let pw = pending.remove(idx);
                    tracing::info!(
                        "late-window watcher: {} appeared at 0x{address}, \
                         moving to workspace {}",
                        pw.app_id,
                        pw.workspace
                    );
                    apply_late_window(&ctl, &pw, &address, restore_geometry).await;
                    resolved_rules.push(pw.rule_name);

                    if pending.is_empty() {
                        tracing::info!("late-window watcher: all pending windows resolved");
                        break;
                    }
                }
            }
            Ok(Ok(None) | Err(_)) => {
                tracing::warn!("late-window watcher: socket2 stream ended");
                break;
            }
            Err(_) => break,
        }
    }

    // Eagerly disable rules for windows that were resolved during the watch.
    disable_all_rules(&ctl, &resolved_rules).await;

    if !pending.is_empty() {
        tracing::warn!(
            "late-window watcher: {} window(s) never appeared after {}s: {}",
            pending.len(),
            grace_period.as_secs(),
            pending
                .iter()
                .map(|p| p.app_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Disable all remaining rules (including for windows that appeared in the
    // main loop but whose rules were kept alive for forking-app safety).
    let remaining_rules: Vec<String> = all_rules
        .into_iter()
        .filter(|r| !resolved_rules.contains(r))
        .collect();
    disable_all_rules(&ctl, &remaining_rules).await;
}

/// Move an already-live window into its BSP-tree slot: workspace, then
/// float it out and settle it back in, preselecting against `anchor`
/// (focused and preselected as the very last step before settling)
/// if one is given. Used for a splash window's real successor
/// (`retile_superseding_window`), a window adopted mid-restore because it
/// raced ahead of its own launch step, and a late-appearing window handed
/// off to the background watcher (`apply_late_window`).
///
/// Preselect must be issued immediately before the floating->tiled
/// transition that consumes it — issuing it earlier risks it being
/// cleared by an intervening tiling-state change, which is exactly what
/// caused a live-tested regression (see `retile_superseding_window`'s
/// doc comment for the full story). Deliberately does NOT re-focus
/// `addr` right before the final settle: refocusing there breaks the
/// preselect binding set up above.
async fn place_window_in_bsp_slot(
    ctl: &HyprCtl,
    addr: &str,
    workspace: &str,
    anchor: Option<(&str, dwindle::PreselDir)>,
) {
    if let Ok(cmd) = move_to_workspace_silent(workspace, addr)
        && ctl.dispatch(&cmd).await.is_ok()
    {
        tracing::debug!("place: move to workspace ok");
    }

    if let Ok(cmd) = focus_window(addr)
        && ctl.dispatch(&cmd).await.is_ok()
    {
        tracing::debug!("place: focus ok");
    }
    if ctl
        .dispatch(&format!("setfloating address:0x{addr}"))
        .await
        .is_ok()
    {
        tracing::debug!("place: float ok");
    }

    if let Some((anchor_addr, presel)) = anchor {
        if let Ok(cmd) = focus_window(anchor_addr)
            && ctl.dispatch(&cmd).await.is_ok()
        {
            tracing::debug!("place: focus anchor ok");
        }
        if ctl
            .dispatch(&format!("layoutmsg preselect {presel}"))
            .await
            .is_ok()
        {
            tracing::debug!("place: preselect ok");
        }
    }

    if ctl
        .dispatch(&format!("setfloating address:0x{addr}"))
        .await
        .is_ok()
    {
        tracing::debug!("place: settle ok");
    }
}

async fn apply_late_window(
    ctl: &HyprCtl,
    pw: &PendingWindow,
    address: &str,
    restore_geometry: bool,
) {
    match (&pw.placement, pw.floating) {
        (PlacementHint::DwindleAnchor(anchor_addr, presel), false) => {
            place_window_in_bsp_slot(
                ctl,
                address,
                &pw.workspace,
                Some((anchor_addr.as_str(), *presel)),
            )
            .await;
        }
        (PlacementHint::MasterPromote, false) => {
            if let Ok(cmd) = move_to_workspace_silent(&pw.workspace, address) {
                drop(ctl.dispatch(&cmd).await);
            }
            drop(ctl.dispatch("layoutmsg addmaster").await);
        }
        _ => {
            if let Ok(cmd) = move_to_workspace_silent(&pw.workspace, address) {
                drop(ctl.dispatch(&cmd).await);
            }
        }
    }

    if restore_geometry
        && pw.floating
        && let (Some((x, y)), Some((w, h))) = (pw.position, pw.size)
    {
        drop(
            ctl.dispatch(&format!("setfloating address:0x{address}"))
                .await,
        );
        drop(
            ctl.dispatch(&format!(
                "resizewindowpixel exact {w} {h},address:0x{address}"
            ))
            .await,
        );
        drop(
            ctl.dispatch(&format!(
                "movewindowpixel exact {x} {y},address:0x{address}"
            ))
            .await,
        );
    }

    if pw.fullscreen {
        drop(
            ctl.dispatch(&format!("fullscreen 0,address:0x{address}"))
                .await,
        );
    }
}

fn validate_session(session: &SessionFile) -> Result<()> {
    for window in &session.windows {
        crate::core::quote::reject_control_bytes(&window.app_id)?;
        WorkspaceName::parse(&window.workspace)?;
        if let Some(monitor) = window.monitor.as_deref().filter(|name| !name.is_empty()) {
            crate::core::quote::MonitorName::parse(monitor)?;
        }
        if window.launch_cmd.is_empty() {
            bail!("empty launch command for '{}'", window.app_id);
        }
    }
    Ok(())
}

/// Build the exec command, injecting browser profile flags and/or saved CWD.
///
/// Profile flags (e.g. `-P work`, `--profile-directory=Profile 1`) are
/// appended to the base launch command before CWD handling, since browsers
/// and terminals are mutually exclusive in practice.
///
/// For known terminals: strips single-instance flags (so each launch is
/// its own process) and appends `--working-directory=<path>`.
/// For other apps with CWD: wraps with `cd <path> && exec <cmd>`.
fn build_launch_cmd(window: &WindowEntry) -> String {
    let raw = window.profile.as_ref().map_or_else(
        || window.launch_cmd.clone(),
        |profile| format!("{} {profile}", window.launch_cmd),
    );
    let cmd = shell_join(&raw);

    let Some(cwd) = window.cwd.as_deref() else {
        return cmd;
    };

    terminal_cwd_flag(&raw).map_or_else(
        || {
            let escaped = posix_single_quote(cwd);
            format!("sh -c 'cd {escaped} && exec {cmd}'")
        },
        |flag| {
            let clean = strip_single_instance_flags(&cmd);
            format!("{clean} {flag}{}", shell_quote(cwd))
        },
    )
}

fn strip_single_instance_flags(cmd: &str) -> String {
    cmd.split_whitespace()
        .filter(|arg| !SINGLE_INSTANCE_FLAGS.contains(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build launch command for BSP restore: always strips single-instance flags
/// so each launch creates an independent process (critical for preselect to
/// work, since single-instance apps create windows in the existing process's
/// workspace, bypassing the preselection on the target workspace).
fn build_bsp_launch_cmd(window: &WindowEntry) -> String {
    let base = build_launch_cmd(window);
    strip_single_instance_flags(&base)
}

/// Match the binary name in a launch command against known terminals
/// and return the appropriate `--working-directory=` style flag.
fn terminal_cwd_flag(launch_cmd: &str) -> Option<&'static str> {
    let bin = launch_cmd.split_whitespace().next()?.rsplit('/').next()?;
    TERMINAL_CWD_FLAGS
        .iter()
        .find(|(name, _)| *name == bin)
        .map(|(_, flag)| *flag)
}

/// Before restoring windows, move each workspace to the monitor it was
/// originally saved on. Only binds to monitors that are currently connected;
/// workspaces targeting unavailable monitors get default Hyprland placement
/// (typically the first available monitor).
async fn bind_workspaces_to_monitors(session: &SessionFile, ctl: &HyprCtl) {
    let available: HashSet<String> = match ctl.get_monitors().await {
        Ok(monitors) => monitors.into_iter().map(|m| m.name).collect(),
        Err(e) => {
            tracing::warn!("could not query monitors, skipping workspace-monitor binding: {e}");
            return;
        }
    };

    let mut seen = HashSet::new();
    let mut missing_monitors: HashSet<&str> = HashSet::new();
    let mut bound = 0usize;

    for window in &session.windows {
        let Some(monitor) = window.monitor.as_deref().filter(|m| !m.is_empty()) else {
            continue;
        };
        if !seen.insert((&window.workspace, monitor)) {
            continue;
        }
        if !available.contains(monitor) {
            missing_monitors.insert(monitor);
            continue;
        }
        tracing::info!(
            "binding workspace {} to monitor {monitor}",
            window.workspace
        );
        if let Ok(cmd) = move_workspace_to_monitor(&window.workspace, monitor) {
            drop(ctl.dispatch(&cmd).await);
        }
        bound += 1;
    }

    if !missing_monitors.is_empty() {
        let names: Vec<&str> = missing_monitors.into_iter().collect();
        tracing::info!(
            "saved monitor(s) no longer connected ({}), \
             affected workspaces will use default placement",
            names.join(", ")
        );
    }
    if bound > 0 {
        tracing::info!("bound {bound} workspace(s) to their saved monitors");
    }
}

async fn disable_all_rules(ctl: &HyprCtl, rules: &[String]) {
    for rule in rules {
        drop(
            ctl.keyword(&format!("windowrule[{rule}]:enable false"))
                .await,
        );
    }
    if !rules.is_empty() {
        tracing::debug!("disabled {} window rules", rules.len());
    }
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: usize,
    pub failed: usize,
    pub errors: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::client::HyprSocketPaths;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    /// Shared handle to a `RecordingSocket1`'s `j/clients` response, mutable
    /// so a test can simulate a window appearing live partway through.
    type ClientsJsonHandle = Arc<Mutex<String>>;

    /// Mock socket1 that records all received IPC commands, responds "ok" to
    /// everything except `j/clients` queries, which get `clients_json`.
    struct RecordingSocket1 {
        listener: UnixListener,
        log: Arc<Mutex<Vec<String>>>,
        clients_json: ClientsJsonHandle,
    }

    impl RecordingSocket1 {
        fn new(path: &std::path::Path) -> (Self, Arc<Mutex<Vec<String>>>) {
            let (me, log, _clients) = Self::with_clients(path, "[]".to_string());
            (me, log)
        }

        fn with_clients(
            path: &std::path::Path,
            clients_json: String,
        ) -> (Self, Arc<Mutex<Vec<String>>>, ClientsJsonHandle) {
            let listener = UnixListener::bind(path).unwrap();
            let log = Arc::new(Mutex::new(Vec::new()));
            let clients_json = Arc::new(Mutex::new(clients_json));
            (
                Self {
                    listener,
                    log: log.clone(),
                    clients_json: clients_json.clone(),
                },
                log,
                clients_json,
            )
        }

        async fn serve(self) {
            loop {
                let Ok((mut stream, _)) = self.listener.accept().await else {
                    break;
                };
                let log = self.log.clone();
                let clients_json = self.clients_json.clone();
                tokio::spawn(async move {
                    let mut buf = String::new();
                    drop(stream.read_to_string(&mut buf).await);
                    log.lock().await.push(buf.clone());
                    let response = if buf.contains("j/clients") {
                        clients_json.lock().await.clone()
                    } else {
                        "ok".to_string()
                    };
                    drop(stream.write_all(response.as_bytes()).await);
                });
            }
        }
    }

    /// Minimal `hyprctl clients -j`-shaped JSON for a single live window.
    fn single_client_json(address: &str, class: &str, workspace: &str) -> String {
        format!(
            r#"[{{"address":"{address}","class":"{class}","pid":1,
            "workspace":{{"id":1,"name":"{workspace}"}},"monitor":0,
            "at":[0,0],"size":[800,600],"floating":false,"fullscreen":0}}]"#
        )
    }

    /// Bind a socket2 listener (synchronously creates the file) and spawn
    /// a task that accepts one connection and emits events with delays.
    fn spawn_delayed_socket2(
        path: &std::path::Path,
        events: Vec<(Duration, String)>,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for (delay, event) in &events {
                tokio::time::sleep(*delay).await;
                let line = format!("{event}\n");
                if stream.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        })
    }

    fn make_entry(
        app_id: &str,
        launch_cmd: &str,
        profile: Option<&str>,
        cwd: Option<&str>,
    ) -> WindowEntry {
        WindowEntry {
            app_id: app_id.to_string(),
            launch_cmd: launch_cmd.to_string(),
            workspace: "1".to_string(),
            monitor: None,
            floating: false,
            fullscreen: false,
            position: None,
            size: None,
            cwd: cwd.map(String::from),
            profile: profile.map(String::from),
        }
    }

    fn make_tracked(
        address: &str,
        app_id: &str,
        workspace: &str,
        position: (i32, i32),
    ) -> TrackedWindow {
        TrackedWindow {
            address: address.to_string(),
            app_id: app_id.to_string(),
            launch_cmd: format!("{app_id}-cmd"),
            workspace: workspace.to_string(),
            monitor: String::new(),
            position,
            size: (800, 600),
            floating: false,
            fullscreen: false,
            pid: 0,
            profile: None,
        }
    }

    fn entry_with_position(app_id: &str, workspace: &str, position: (i32, i32)) -> WindowEntry {
        WindowEntry {
            app_id: app_id.to_string(),
            launch_cmd: app_id.to_string(),
            workspace: workspace.to_string(),
            monitor: None,
            floating: false,
            fullscreen: false,
            position: Some(position),
            size: None,
            cwd: None,
            profile: None,
        }
    }

    #[test]
    fn build_cmd_no_profile_no_cwd() {
        let entry = make_entry("firefox", "firefox", None, None);
        assert_eq!(build_launch_cmd(&entry), "firefox");
    }

    #[test]
    fn build_cmd_with_profile() {
        let entry = make_entry("firefox", "firefox", Some("-P work"), None);
        assert_eq!(build_launch_cmd(&entry), "firefox -P work");
    }

    #[test]
    fn build_cmd_with_no_remote_profile() {
        let entry = make_entry("firefox", "firefox", Some("-no-remote -P dev"), None);
        assert_eq!(build_launch_cmd(&entry), "firefox -no-remote -P dev");
    }

    #[test]
    fn build_cmd_chromium_profile() {
        let entry = make_entry(
            "chromium",
            "chromium",
            Some("--profile-directory=Profile 1"),
            None,
        );
        assert_eq!(
            build_launch_cmd(&entry),
            "chromium --profile-directory=Profile 1"
        );
    }

    #[test]
    fn build_cmd_flatpak_profile() {
        let entry = make_entry(
            "org.mozilla.firefox",
            "flatpak run org.mozilla.firefox",
            Some("-P work"),
            None,
        );
        assert_eq!(
            build_launch_cmd(&entry),
            "flatpak run org.mozilla.firefox -P work"
        );
    }

    #[test]
    fn build_cmd_with_cwd_no_profile() {
        let entry = make_entry("ghostty", "ghostty", None, Some("/home/user/project"));
        assert_eq!(
            build_launch_cmd(&entry),
            "ghostty --working-directory=/home/user/project"
        );
    }

    #[test]
    fn build_cmd_profile_does_not_affect_cwd() {
        let entry = make_entry("ghostty", "ghostty", None, Some("/tmp"));
        let cmd = build_launch_cmd(&entry);
        assert!(cmd.contains("--working-directory=/tmp"));
    }

    #[tokio::test]
    async fn late_watcher_catches_delayed_window() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![(
                Duration::from_millis(100),
                "openwindow>>abc123,4,slow-app,Slow App Title".to_string(),
            )],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec![PendingWindow {
            app_id: "slow-app".to_string(),
            workspace: "3".to_string(),
            floating: false,
            fullscreen: false,
            position: None,
            size: None,
            rule_name: "hypr-persist-slow-app".to_string(),
            placement: PlacementHint::None,
        }];
        let all_rules = vec!["hypr-persist-slow-app".to_string()];

        watch_late_windows(paths, pending, all_rules, false, Duration::from_secs(5)).await;

        let commands = log.lock().await;
        let has_move = commands
            .iter()
            .any(|c| c.contains("movetoworkspacesilent 3,address:0xabc123"));
        assert!(
            has_move,
            "expected movetoworkspacesilent dispatch, got: {commands:?}"
        );

        let has_rule_disable = commands
            .iter()
            .any(|c| c.contains("windowrule[hypr-persist-slow-app]:enable false"));
        assert!(has_rule_disable, "expected rule cleanup, got: {commands:?}");
        drop(commands);

        s1.abort();
        s2.abort();
    }

    /// A late-appearing tiled window that was launched with a BSP anchor
    /// must get the full float/focus-anchor/preselect/settle placement
    /// dance, not just a bare `movetoworkspacesilent` — otherwise it lands
    /// wherever Hyprland's own dwindle insertion defaults to instead of its
    /// saved BSP slot next to its anchor sibling.
    #[tokio::test]
    async fn late_watcher_places_anchored_window_in_bsp_slot() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![(
                Duration::from_millis(100),
                "openwindow>>slack001,4,slack,Slack".to_string(),
            )],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec![PendingWindow {
            app_id: "slack".to_string(),
            workspace: "4".to_string(),
            floating: false,
            fullscreen: false,
            position: None,
            size: None,
            rule_name: "hypr-persist-slack".to_string(),
            placement: PlacementHint::DwindleAnchor(
                "anchor789".to_string(),
                dwindle::PreselDir::Bottom,
            ),
        }];
        let all_rules = vec!["hypr-persist-slack".to_string()];

        watch_late_windows(paths, pending, all_rules, false, Duration::from_secs(5)).await;

        let commands = log.lock().await;
        assert!(
            commands
                .iter()
                .any(|c| c.contains("movetoworkspacesilent 4,address:0xslack001")),
            "expected workspace move, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("focuswindow address:0xanchor789")),
            "expected focus on anchor sibling, got: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c.contains("layoutmsg preselect d")),
            "expected preselect against anchor, got: {commands:?}"
        );
        let float_count = commands
            .iter()
            .filter(|c| c.contains("setfloating address:0xslack001"))
            .count();
        assert_eq!(
            float_count, 2,
            "expected float + settle (2x setfloating), got: {commands:?}"
        );
        drop(commands);

        s1.abort();
        s2.abort();
    }

    /// A late-appearing "extra master" window under the master layout must
    /// get promoted via `layoutmsg addmaster` once it lands, not just a bare
    /// `movetoworkspacesilent` — otherwise it silently ends up stuck in the
    /// stack instead of promoted to master, since master has no
    /// per-window anchor/preselect concept for `place_window_in_bsp_slot`
    /// to fall back on.
    #[tokio::test]
    async fn late_watcher_promotes_master_window_after_placement() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![(
                Duration::from_millis(100),
                "openwindow>>master1,4,thunar,Files".to_string(),
            )],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec![PendingWindow {
            app_id: "thunar".to_string(),
            workspace: "4".to_string(),
            floating: false,
            fullscreen: false,
            position: None,
            size: None,
            rule_name: "hypr-persist-thunar".to_string(),
            placement: PlacementHint::MasterPromote,
        }];
        let all_rules = vec!["hypr-persist-thunar".to_string()];

        watch_late_windows(paths, pending, all_rules, false, Duration::from_secs(5)).await;

        let commands = log.lock().await;
        assert!(
            commands
                .iter()
                .any(|c| c.contains("movetoworkspacesilent 4,address:0xmaster1")),
            "expected workspace move, got: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c.contains("layoutmsg addmaster")),
            "expected the late-appearing extra-master window to be \
             promoted, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
        s2.abort();
    }

    /// A late-appearing stack (or first-master) window must not be
    /// promoted — only extra-master windows carry `PlacementHint::MasterPromote`.
    #[tokio::test]
    async fn late_watcher_does_not_promote_plain_master_window() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![(
                Duration::from_millis(100),
                "openwindow>>stack1,4,kitty,Kitty".to_string(),
            )],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec![PendingWindow {
            app_id: "kitty".to_string(),
            workspace: "4".to_string(),
            floating: false,
            fullscreen: false,
            position: None,
            size: None,
            rule_name: "hypr-persist-kitty".to_string(),
            placement: PlacementHint::None,
        }];
        let all_rules = vec!["hypr-persist-kitty".to_string()];

        watch_late_windows(paths, pending, all_rules, false, Duration::from_secs(5)).await;

        let commands = log.lock().await;
        assert!(
            commands
                .iter()
                .any(|c| c.contains("movetoworkspacesilent 4,address:0xstack1")),
            "expected workspace move, got: {commands:?}"
        );
        assert!(
            !commands.iter().any(|c| c.contains("addmaster")),
            "a stack window must never be promoted, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
        s2.abort();
    }

    /// Verifies that when a pending window never appears, the watcher
    /// times out after the grace period and still cleans up all rules.
    #[tokio::test]
    async fn late_watcher_times_out_and_disables_rules() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![(
                Duration::from_millis(50),
                "openwindow>>xyz,1,other-app,Other".to_string(),
            )],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec![PendingWindow {
            app_id: "missing-app".to_string(),
            workspace: "2".to_string(),
            floating: false,
            fullscreen: false,
            position: None,
            size: None,
            rule_name: "hypr-persist-missing-app".to_string(),
            placement: PlacementHint::None,
        }];
        let all_rules = vec!["hypr-persist-missing-app".to_string()];

        let start = tokio::time::Instant::now();
        watch_late_windows(paths, pending, all_rules, false, Duration::from_millis(300)).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_millis(250),
            "should wait for grace period, only waited {elapsed:?}"
        );

        let commands = log.lock().await;
        let has_rule_disable = commands
            .iter()
            .any(|c| c.contains("windowrule[hypr-persist-missing-app]:enable false"));
        assert!(
            has_rule_disable,
            "rules must be cleaned up even on timeout, got: {commands:?}"
        );

        let has_move = commands.iter().any(|c| c.contains("movetoworkspacesilent"));
        assert!(
            !has_move,
            "no movetoworkspacesilent for a window that never appeared, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
        s2.abort();
    }

    /// Floating window geometry (position + size) is applied when a late
    /// window arrives and `restore_geometry` is enabled.
    #[tokio::test]
    async fn late_watcher_restores_floating_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![(
                Duration::from_millis(50),
                "openwindow>>flt001,9,floater,Floating App".to_string(),
            )],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec![PendingWindow {
            app_id: "floater".to_string(),
            workspace: "5".to_string(),
            floating: true,
            fullscreen: false,
            position: Some((200, 150)),
            size: Some((800, 600)),
            rule_name: "hypr-persist-floater".to_string(),
            placement: PlacementHint::None,
        }];
        let all_rules = vec!["hypr-persist-floater".to_string()];

        watch_late_windows(paths, pending, all_rules, true, Duration::from_secs(5)).await;

        let commands = log.lock().await;
        let has_move = commands
            .iter()
            .any(|c| c.contains("movetoworkspacesilent 5,address:0xflt001"));
        assert!(has_move, "expected workspace move, got: {commands:?}");

        let has_float = commands
            .iter()
            .any(|c| c.contains("setfloating address:0xflt001"));
        assert!(has_float, "expected setfloating, got: {commands:?}");

        let has_resize = commands
            .iter()
            .any(|c| c.contains("resizewindowpixel exact 800 600,address:0xflt001"));
        assert!(has_resize, "expected resize, got: {commands:?}");

        let has_pos = commands
            .iter()
            .any(|c| c.contains("movewindowpixel exact 200 150,address:0xflt001"));
        assert!(has_pos, "expected position, got: {commands:?}");
        drop(commands);

        s1.abort();
        s2.abort();
    }

    /// Multiple slow-starting apps: the watcher resolves all of them as they
    /// arrive and exits early (before the grace period) when the last one appears.
    #[tokio::test]
    async fn late_watcher_handles_multiple_pending_windows() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let s2 = spawn_delayed_socket2(
            &sock2,
            vec![
                (
                    Duration::from_millis(50),
                    "openwindow>>aaa,1,app-a,App A".to_string(),
                ),
                (
                    Duration::from_millis(50),
                    "openwindow>>bbb,2,app-b,App B".to_string(),
                ),
            ],
        );

        let paths = HyprSocketPaths::new(sock1, sock2);
        let pending = vec![
            PendingWindow {
                app_id: "app-a".to_string(),
                workspace: "1".to_string(),
                floating: false,
                fullscreen: false,
                position: None,
                size: None,
                rule_name: "hypr-persist-app-a".to_string(),
                placement: PlacementHint::None,
            },
            PendingWindow {
                app_id: "app-b".to_string(),
                workspace: "4".to_string(),
                floating: false,
                fullscreen: false,
                position: None,
                size: None,
                rule_name: "hypr-persist-app-b".to_string(),
                placement: PlacementHint::None,
            },
        ];
        let all_rules = vec![
            "hypr-persist-app-a".to_string(),
            "hypr-persist-app-b".to_string(),
        ];

        let start = tokio::time::Instant::now();
        watch_late_windows(paths, pending, all_rules, false, Duration::from_secs(10)).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "should exit early when all pending resolved, took {elapsed:?}"
        );

        let commands = log.lock().await;
        let has_move_a = commands
            .iter()
            .any(|c| c.contains("movetoworkspacesilent 1,address:0xaaa"));
        assert!(has_move_a, "expected move for app-a, got: {commands:?}");

        let has_move_b = commands
            .iter()
            .any(|c| c.contains("movetoworkspacesilent 4,address:0xbbb"));
        assert!(has_move_b, "expected move for app-b, got: {commands:?}");
        drop(commands);

        s1.abort();
        s2.abort();
    }

    /// When only one window of the class ever appears (no splash), the
    /// grace period expires with nothing new and the original address is
    /// returned unchanged — but placement must still be finalized via
    /// `place_window_in_bsp_slot` (move/focus/float/settle), since that's
    /// the only point placement is ever explicitly (re)applied for a
    /// normally-opened window; no anchor here means no focus-anchor/preselect
    /// dispatch, and no splash means no `closewindow`.
    #[tokio::test]
    async fn retile_no_supersede_still_finalizes_placement() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);
        let (_tx, mut rx) = mpsc::channel::<HyprEvent>(8);

        let result = engine
            .retile_superseding_window(&ctl, &mut rx, "aaa000", "discord", "4", None)
            .await;

        assert_eq!(result, "aaa000");
        let commands = log.lock().await;
        assert!(
            !commands.iter().any(|c| c.contains("closewindow")),
            "no supersede occurred, so nothing should be closed, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("movetoworkspacesilent 4,address:0xaaa000")),
            "expected workspace move as part of placement, got: {commands:?}"
        );
        let float_count = commands
            .iter()
            .filter(|c| c.contains("setfloating address:0xaaa000"))
            .count();
        assert_eq!(
            float_count, 2,
            "expected float + settle (2x setfloating) even without an anchor, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// When a second window of the same class appears within the grace
    /// period, the stale first window is closed and the anchor/preselect/
    /// focus/float/settle dance is redone against the real window.
    #[tokio::test]
    async fn retile_supersede_redoes_placement() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);
        let (tx, mut rx) = mpsc::channel::<HyprEvent>(8);
        tx.send(HyprEvent::OpenWindow {
            address: "bbb111".to_string(),
            workspace: "4".to_string(),
            class: "discord".to_string(),
        })
        .await
        .unwrap();

        let result = engine
            .retile_superseding_window(
                &ctl,
                &mut rx,
                "aaa000",
                "discord",
                "4",
                Some(("anchor123", dwindle::PreselDir::Bottom)),
            )
            .await;

        assert_eq!(result, "bbb111");

        let commands = log.lock().await;
        assert!(
            commands
                .iter()
                .any(|c| c.contains("closewindow address:0xaaa000")),
            "expected close of stale splash, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("movetoworkspacesilent 4,address:0xbbb111")),
            "expected move of the real window to the target workspace, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("focuswindow address:0xanchor123")),
            "expected focus anchor, got: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c.contains("layoutmsg preselect d")),
            "expected preselect, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("focuswindow address:0xbbb111")),
            "expected focus real window, got: {commands:?}"
        );
        let float_count = commands
            .iter()
            .filter(|c| c.contains("setfloating address:0xbbb111"))
            .count();
        assert_eq!(
            float_count, 2,
            "expected float + settile (2x setfloating), got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// The master-layout path has no BSP placement primitives, so it can't
    /// reuse `retile_superseding_window` directly — but it must still catch
    /// a transient splash window (the exact Discord scenario 314255e fixed
    /// for dwindle) via `launch_and_track`'s `MasterRole`-gated call to the
    /// shared `resolve_splash_supersede` helper. An "extra master" window
    /// (`MasterRole::Promote`) must land and get promoted using the real
    /// window's address, with the stale splash closed and never promoted.
    #[tokio::test]
    async fn launch_and_track_master_promote_supersedes_splash_before_promoting() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log, _clients) = RecordingSocket1::with_clients(&sock1, "[]".to_string());
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);
        let (tx, mut events) = mpsc::channel::<HyprEvent>(8);
        tx.send(HyprEvent::OpenWindow {
            address: "splash1".to_string(),
            workspace: "1".to_string(),
            class: "discord".to_string(),
        })
        .await
        .unwrap();
        tx.send(HyprEvent::OpenWindow {
            address: "realwin1".to_string(),
            workspace: "1".to_string(),
            class: "discord".to_string(),
        })
        .await
        .unwrap();

        let mut active_rules = Vec::new();
        let mut pending = Vec::new();
        let mut live_pool = LiveWindowPool::new(vec![]);
        let window = make_entry("discord", "discord", None, None);

        let outcome = engine
            .launch_and_track(
                &window,
                &ctl,
                &mut events,
                &mut active_rules,
                &mut pending,
                &mut live_pool,
                MasterRole::Promote,
            )
            .await
            .unwrap();

        match outcome {
            LaunchOutcome::Opened(addr) => assert_eq!(addr, "realwin1"),
            other => panic!("expected the real window to be tracked, got {other:?}"),
        }
        assert!(pending.is_empty(), "must not defer once opened");

        let commands = log.lock().await;
        assert!(
            commands
                .iter()
                .any(|c| c.contains("closewindow address:0xsplash1")),
            "expected the stale splash to be closed, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("movetoworkspacesilent 1,address:0xrealwin1")),
            "expected the real window moved to its workspace, got: {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|c| c.contains("address:0xsplash1") && c.contains("movetoworkspacesilent")),
            "the splash window must never be placed, got: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c.contains("layoutmsg addmaster")),
            "expected the real window to be promoted, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// A plain master-layout window (first master, or a stack window) still
    /// gets the splash-supersede check, but must never be promoted — only
    /// `MasterRole::Promote` (an extra master window) triggers `addmaster`.
    #[tokio::test]
    async fn launch_and_track_master_plain_supersedes_splash_without_promoting() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log, _clients) = RecordingSocket1::with_clients(&sock1, "[]".to_string());
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);
        let (tx, mut events) = mpsc::channel::<HyprEvent>(8);
        tx.send(HyprEvent::OpenWindow {
            address: "splash2".to_string(),
            workspace: "1".to_string(),
            class: "discord".to_string(),
        })
        .await
        .unwrap();
        tx.send(HyprEvent::OpenWindow {
            address: "realwin2".to_string(),
            workspace: "1".to_string(),
            class: "discord".to_string(),
        })
        .await
        .unwrap();

        let mut active_rules = Vec::new();
        let mut pending = Vec::new();
        let mut live_pool = LiveWindowPool::new(vec![]);
        let window = make_entry("discord", "discord", None, None);

        let outcome = engine
            .launch_and_track(
                &window,
                &ctl,
                &mut events,
                &mut active_rules,
                &mut pending,
                &mut live_pool,
                MasterRole::Plain,
            )
            .await
            .unwrap();

        match outcome {
            LaunchOutcome::Opened(addr) => assert_eq!(addr, "realwin2"),
            other => panic!("expected the real window to be tracked, got {other:?}"),
        }

        let commands = log.lock().await;
        assert!(
            commands
                .iter()
                .any(|c| c.contains("closewindow address:0xsplash2")),
            "expected the stale splash to be closed, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("movetoworkspacesilent 1,address:0xrealwin2")),
            "expected the real window moved to its workspace, got: {commands:?}"
        );
        assert!(
            !commands.iter().any(|c| c.contains("addmaster")),
            "a plain-role window must never be promoted, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// Deferring a master-layout window to the late-window watcher must not
    /// immediately dispatch `layoutmsg addmaster` against whatever happens
    /// to be focused right now — the window doesn't exist yet. The
    /// promotion must instead be carried on the `PendingWindow` so the
    /// watcher can apply it once the window actually appears (see
    /// `late_watcher_promotes_master_window_after_placement`).
    #[tokio::test]
    async fn launch_and_track_master_promote_defers_without_immediate_addmaster() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log, _clients) = RecordingSocket1::with_clients(&sock1, "[]".to_string());
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine =
            RestoreEngine::new(true, true).with_window_appear_timeout(Duration::from_millis(50));
        let (_tx, mut events) = mpsc::channel::<HyprEvent>(8);

        let mut active_rules = Vec::new();
        let mut pending = Vec::new();
        let mut live_pool = LiveWindowPool::new(vec![]);
        let window = make_entry("slow-master-app", "slow-master-app", None, None);

        let outcome = engine
            .launch_and_track(
                &window,
                &ctl,
                &mut events,
                &mut active_rules,
                &mut pending,
                &mut live_pool,
                MasterRole::Promote,
            )
            .await
            .unwrap();

        assert!(
            matches!(outcome, LaunchOutcome::Deferred),
            "expected a deferred outcome, got {outcome:?}"
        );
        assert_eq!(pending.len(), 1, "expected one pending window");
        assert!(
            matches!(pending[0].placement, PlacementHint::MasterPromote),
            "expected the pending window to carry the promotion hint for \
             the late-window watcher to apply, got {:?}",
            pending[0].placement
        );

        let commands = log.lock().await;
        assert!(
            !commands.iter().any(|c| c.contains("addmaster")),
            "must not dispatch addmaster immediately against whatever is \
             currently focused before the window even exists, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    // --- LiveWindowPool ---

    #[test]
    fn live_pool_matches_by_class_and_workspace() {
        let mut pool = LiveWindowPool::new(vec![make_tracked("0xabc", "foot", "1", (0, 0))]);
        let entry = make_entry("foot", "foot", None, None);

        let matched = pool.take_match(&entry).expect("expected a match");
        assert_eq!(matched.address, "0xabc");
    }

    #[test]
    fn live_pool_no_match_different_class() {
        let mut pool = LiveWindowPool::new(vec![make_tracked("0xabc", "kitty", "1", (0, 0))]);
        let entry = make_entry("foot", "foot", None, None);

        assert!(pool.take_match(&entry).is_none());
    }

    #[test]
    fn live_pool_no_match_different_workspace() {
        let mut pool = LiveWindowPool::new(vec![make_tracked("0xabc", "foot", "2", (0, 0))]);
        let entry = make_entry("foot", "foot", None, None);

        assert!(pool.take_match(&entry).is_none());
    }

    #[test]
    fn live_pool_does_not_reuse_claimed_window() {
        let mut pool = LiveWindowPool::new(vec![make_tracked("0xabc", "foot", "1", (0, 0))]);
        let entry = make_entry("foot", "foot", None, None);

        assert!(pool.take_match(&entry).is_some());
        assert!(
            pool.take_match(&entry).is_none(),
            "the same live window must not be adopted twice"
        );
    }

    #[test]
    fn live_pool_prefers_nearest_saved_position_on_tie() {
        let mut pool = LiveWindowPool::new(vec![
            make_tracked("0xnear", "foot", "1", (100, 100)),
            make_tracked("0xfar", "foot", "1", (900, 900)),
        ]);
        let entry = entry_with_position("foot", "1", (120, 110));

        let matched = pool.take_match(&entry).expect("expected a match");
        assert_eq!(matched.address, "0xnear");
    }

    /// Models the real-world 2x2-grid bug scenario: four same-class terminals
    /// already live on one workspace must each be matched to the plan entry
    /// with the nearest saved position, with no live window claimed twice.
    #[test]
    fn live_pool_matches_2x2_grid_without_double_claiming() {
        let mut pool = LiveWindowPool::new(vec![
            make_tracked("0xa", "foot", "1", (0, 0)),
            make_tracked("0xb", "foot", "1", (960, 0)),
            make_tracked("0xc", "foot", "1", (0, 540)),
            make_tracked("0xd", "foot", "1", (960, 540)),
        ]);

        let expected = [
            ((10, 10), "0xa"),
            ((950, 10), "0xb"),
            ((10, 530), "0xc"),
            ((950, 530), "0xd"),
        ];

        for (position, expected_addr) in expected {
            let entry = entry_with_position("foot", "1", position);
            let matched = pool.take_match(&entry).expect("expected a match");
            assert_eq!(matched.address, expected_addr);
        }

        let entry = entry_with_position("foot", "1", (10, 10));
        assert!(
            pool.take_match(&entry).is_none(),
            "all four live windows should already be claimed"
        );
    }

    // --- restore_window adoption ---

    /// When a live window matches, `restore_window` must adopt it instead of
    /// launching a duplicate: no window rule, no exec, no geometry dispatch.
    #[tokio::test]
    async fn restore_window_adopts_live_window_without_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);
        let (_tx, mut events) = mpsc::channel::<HyprEvent>(8);

        let mut active_rules = Vec::new();
        let mut pending = Vec::new();
        let mut live_pool = LiveWindowPool::new(vec![make_tracked("0xlive1", "foot", "1", (0, 0))]);
        let window = make_entry("foot", "foot", None, None);

        let adopted = engine
            .restore_window(
                &window,
                &ctl,
                &mut events,
                &mut active_rules,
                &mut pending,
                &mut live_pool,
                MasterRole::None,
            )
            .await
            .unwrap();

        assert!(adopted, "expected the live window to be adopted");
        assert!(active_rules.is_empty(), "no rule should be created");
        assert!(pending.is_empty(), "nothing should be deferred");

        let commands = log.lock().await;
        assert!(
            commands.is_empty(),
            "adopting a live window must not issue any hyprctl dispatch, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// With no matching live window, `restore_window` falls back to the
    /// existing launch-and-track behavior.
    #[tokio::test]
    async fn restore_window_launches_when_no_live_match() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);
        let (event_tx, mut events) = mpsc::channel::<HyprEvent>(8);
        event_tx
            .send(HyprEvent::OpenWindow {
                address: "newwin".to_string(),
                workspace: "1".to_string(),
                class: "foot".to_string(),
            })
            .await
            .unwrap();

        let mut active_rules = Vec::new();
        let mut pending = Vec::new();
        let mut live_pool =
            LiveWindowPool::new(vec![make_tracked("0xother", "kitty", "1", (0, 0))]);
        let window = make_entry("foot", "foot", None, None);

        let adopted = engine
            .restore_window(
                &window,
                &ctl,
                &mut events,
                &mut active_rules,
                &mut pending,
                &mut live_pool,
                MasterRole::None,
            )
            .await
            .unwrap();

        assert!(!adopted, "no matching live window, should launch instead");

        let commands = log.lock().await;
        assert!(
            commands
                .iter()
                .any(|c| c.contains("exec [workspace 1 silent] foot")),
            "expected a launch dispatch, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    // --- execute_bsp_plans adoption ---

    /// When the first BSP step's window is already live, `execute_bsp_plans`
    /// must adopt it without touching the workspace-switch dispatch normally
    /// issued for an anchor-less first step, and later steps must anchor
    /// against the adopted window's (normalized) address.
    #[tokio::test]
    async fn execute_bsp_plans_adopts_first_step_and_anchors_second_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(false, true);

        let (event_tx, mut events) = mpsc::channel::<HyprEvent>(8);
        event_tx
            .send(HyprEvent::OpenWindow {
                address: "newwin1".to_string(),
                workspace: "1".to_string(),
                class: "foot".to_string(),
            })
            .await
            .unwrap();

        let session = SessionFile {
            session: crate::models::SessionMeta {
                name: "t".to_string(),
                timestamp: 0,
            },
            windows: vec![
                make_entry("foot", "foot", None, None),
                make_entry("foot", "foot", None, None),
            ],
        };

        let mut ws_plans = HashMap::new();
        ws_plans.insert(
            "1".to_string(),
            DwindlePlan {
                steps: vec![
                    dwindle::RestoreStep {
                        window_idx: 0,
                        focus_idx: None,
                        preselect: None,
                    },
                    dwindle::RestoreStep {
                        window_idx: 1,
                        focus_idx: Some(0),
                        preselect: Some(dwindle::PreselDir::Right),
                    },
                ],
                ratio_steps: vec![],
                pixel_ratio_steps: vec![],
            },
        );

        let mut report = RestoreReport::default();
        let mut active_rules = Vec::new();
        let mut pending = Vec::new();
        let mut live_pool =
            LiveWindowPool::new(vec![make_tracked("0xLIVE01", "foot", "1", (0, 0))]);

        let (addresses, adopted) = engine
            .execute_bsp_plans(
                &session,
                &ctl,
                &mut events,
                &mut report,
                &ws_plans,
                &mut active_rules,
                &mut pending,
                &mut live_pool,
            )
            .await
            .unwrap();

        assert_eq!(report.restored, 2);
        assert_eq!(addresses.get(&0), Some(&"live01".to_string()));
        assert_eq!(addresses.get(&1), Some(&"newwin1".to_string()));
        assert!(adopted.contains(&0), "window 0 was adopted, not launched");
        assert!(!adopted.contains(&1), "window 1 was freshly launched");

        let commands = log.lock().await;
        assert!(
            !commands.iter().any(|c| c.contains("dispatch workspace 1")),
            "adopting the first step must not dispatch a workspace switch, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("focuswindow address:0xlive01")),
            "second step must anchor to the adopted window's address, got: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c.contains("layoutmsg preselect r")),
            "expected preselect for the second step, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    // --- adopted-window convergence skip ---

    /// An adopted window's own splitratio step must not be applied: it never
    /// went through the fresh-insertion default-0.5 dance this delta assumes.
    /// A sibling step for a freshly-launched window in the same call must
    /// still be applied normally.
    #[tokio::test]
    async fn apply_split_ratios_skips_adopted_window() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);

        let mut ws_plans = HashMap::new();
        ws_plans.insert(
            "1".to_string(),
            DwindlePlan {
                steps: vec![],
                ratio_steps: vec![
                    dwindle::SplitRatioStep {
                        focus_window_idx: 0,
                        ratio: 0.7,
                    },
                    dwindle::SplitRatioStep {
                        focus_window_idx: 1,
                        ratio: 0.3,
                    },
                ],
                pixel_ratio_steps: vec![],
            },
        );

        let mut addresses = HashMap::new();
        addresses.insert(0, "adopted01".to_string());
        addresses.insert(1, "fresh01".to_string());
        let mut adopted = HashSet::new();
        adopted.insert(0);

        engine
            .apply_split_ratios(&ctl, &ws_plans, &addresses, &adopted)
            .await;

        let commands = log.lock().await;
        assert!(
            !commands.iter().any(|c| c.contains("address:0xadopted01")),
            "adopted window must not be focused/resplit, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("focuswindow address:0xfresh01")),
            "freshly-launched window must still get its splitratio applied, got: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c.contains("layoutmsg splitratio")),
            "expected a splitratio dispatch for the non-adopted window, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// An adopted window must not be queried or resized toward the saved
    /// session size — it was already live before this restore in whatever
    /// shape the current tree gives it.
    #[tokio::test]
    async fn converge_tiled_sizes_skips_adopted_window() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);

        let mut window = entry_with_position("discord", "4", (0, 0));
        window.size = Some((1200, 800));
        let session = SessionFile {
            session: crate::models::SessionMeta {
                name: "t".to_string(),
                timestamp: 0,
            },
            windows: vec![window],
        };

        let mut addresses = HashMap::new();
        addresses.insert(0usize, "adopted02".to_string());
        let mut adopted = HashSet::new();
        adopted.insert(0);

        engine
            .converge_tiled_sizes(&session, &ctl, &addresses, &adopted)
            .await;

        let commands = log.lock().await;
        assert!(
            commands.is_empty(),
            "adopted window must not be queried or resized at all, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// Two tiled windows sharing a BSP split edge (e.g. left/right column
    /// mates in a 2x2 dwindle grid) must never both be independently resized
    /// in the same pass — that's the shared-edge fight observed on a real
    /// reboot (both sides fighting over the same boundary, each rejected as
    /// "Invalid size"). The mock's `j/clients` response is static, so left's
    /// mismatch never actually resolves: it gets one dispatch, is found
    /// stuck (unchanging) on the next pass, and only then does the pass
    /// move on to right. Right must never be dispatched in the *same* pass
    /// as left (that would be the fight this test guards against), but once
    /// left is confirmed permanently stuck it must not block right forever
    /// either — see `converge_tiled_sizes_lets_other_candidates_through_a_
    /// permanently_stuck_one` for that starvation case.
    #[tokio::test]
    async fn converge_tiled_sizes_never_resizes_two_shared_edge_windows_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let clients_json = r#"[{"address":"0xleft001","class":"discord","pid":1,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[0,0],"size":[1120,563],"floating":false,"fullscreen":0},
            {"address":"0xright01","class":"thunar","pid":2,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[1120,0],"size":[1410,563],"floating":false,"fullscreen":0}]"#
            .to_string();

        let (mock1, log, _clients) = RecordingSocket1::with_clients(&sock1, clients_json);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);

        let mut left = entry_with_position("discord", "4", (0, 0));
        left.size = Some((1265, 563));
        let mut right = entry_with_position("thunar", "4", (1265, 0));
        right.size = Some((1265, 563));

        let session = SessionFile {
            session: crate::models::SessionMeta {
                name: "t".to_string(),
                timestamp: 0,
            },
            windows: vec![left, right],
        };

        let mut addresses = HashMap::new();
        addresses.insert(0usize, "left001".to_string());
        addresses.insert(1usize, "right01".to_string());
        let adopted = HashSet::new();

        engine
            .converge_tiled_sizes(&session, &ctl, &addresses, &adopted)
            .await;

        let commands = log.lock().await;
        let resize_indices: Vec<usize> = commands
            .iter()
            .enumerate()
            .filter(|(_, c)| c.contains("resizewindowpixel"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            resize_indices.len(),
            2,
            "expected exactly one resize attempt each for left and right \
             (left first, then right once left is confirmed stuck), got: {commands:?}"
        );
        let (left_idx, right_idx) = (resize_indices[0], resize_indices[1]);
        assert!(
            commands[left_idx].contains("address:0xleft001"),
            "left must be resized first, got: {commands:?}"
        );
        assert!(
            commands[right_idx].contains("address:0xright01"),
            "right must be resized second, got: {commands:?}"
        );
        assert!(
            commands[left_idx + 1..right_idx]
                .iter()
                .any(|c| c.contains("j/clients")),
            "right must never be resized in the same pass as left's own \
             dispatch — there must be at least one recheck of left (a new \
             pass) confirming it's stuck before right ever gets a turn, \
             got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// A permanently-stuck lowest-index candidate must not consume every
    /// pass for the entire `max_passes` budget and starve every other
    /// out-of-tolerance candidate later in index order. Observed live: a
    /// reboot where one window's resize delta never changed across all
    /// passes — harmless that time because it was the only mismatched
    /// window, but nothing in the pre-fix loop stopped it from silently
    /// preventing any *other* window elsewhere in the session from ever
    /// being resized, since `resized = true; break;` fired unconditionally
    /// for the first out-of-tolerance candidate whether or not a dispatch
    /// was actually sent. Three candidates here: the lowest-index one is
    /// permanently stuck (its delta never changes, unrelated to any shared
    /// edge), a middle one is already in tolerance, and the highest-index
    /// one would actually resolve once given its own dispatch.
    #[tokio::test]
    async fn converge_tiled_sizes_lets_other_candidates_through_a_permanently_stuck_one() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let unresolved_json = r#"[{"address":"0xstuck01","class":"discord","pid":1,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[0,0],"size":[700,500],"floating":false,"fullscreen":0},
            {"address":"0xsteady1","class":"kitty","pid":2,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[700,0],"size":[400,300],"floating":false,"fullscreen":0},
            {"address":"0xresolv3","class":"thunar","pid":3,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[1100,0],"size":[850,650],"floating":false,"fullscreen":0}]"#
            .to_string();
        let resolved_json = r#"[{"address":"0xstuck01","class":"discord","pid":1,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[0,0],"size":[700,500],"floating":false,"fullscreen":0},
            {"address":"0xsteady1","class":"kitty","pid":2,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[700,0],"size":[400,300],"floating":false,"fullscreen":0},
            {"address":"0xresolv3","class":"thunar","pid":3,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[1100,0],"size":[900,700],"floating":false,"fullscreen":0}]"#
            .to_string();

        let (mock1, log, clients) = RecordingSocket1::with_clients(&sock1, unresolved_json);
        let s1 = tokio::spawn(mock1.serve());

        // Once the third candidate's resize is dispatched, simulate Hyprland
        // having applied it by swapping in geometry where that window (and
        // only that window) now matches its saved size.
        let watcher_log = log.clone();
        let watcher = tokio::spawn(async move {
            loop {
                let seen = watcher_log
                    .lock()
                    .await
                    .iter()
                    .any(|c| c.contains("resizewindowpixel") && c.contains("0xresolv3"));
                if seen {
                    *clients.lock().await = resolved_json;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);

        let mut stuck = entry_with_position("discord", "4", (0, 0));
        stuck.size = Some((800, 600));
        let mut steady = entry_with_position("kitty", "4", (700, 0));
        steady.size = Some((400, 300));
        let mut resolvable = entry_with_position("thunar", "4", (1100, 0));
        resolvable.size = Some((900, 700));

        let session = SessionFile {
            session: crate::models::SessionMeta {
                name: "t".to_string(),
                timestamp: 0,
            },
            windows: vec![stuck, steady, resolvable],
        };

        let mut addresses = HashMap::new();
        addresses.insert(0usize, "stuck01".to_string());
        addresses.insert(1usize, "steady1".to_string());
        addresses.insert(2usize, "resolv3".to_string());
        let adopted = HashSet::new();

        engine
            .converge_tiled_sizes(&session, &ctl, &addresses, &adopted)
            .await;

        watcher.abort();

        let commands = log.lock().await;
        assert!(
            commands
                .iter()
                .any(|c| c.contains("resizewindowpixel") && c.contains("0xresolv3")),
            "the resolvable higher-index candidate must get its resize \
             dispatched despite the lowest-index candidate never \
             converging, got: {commands:?}"
        );
        assert!(
            !commands.iter().any(|c| c.contains("0xsteady1")),
            "the already-in-tolerance middle candidate must never be \
             touched, got: {commands:?}"
        );
        let stuck_resize_count = commands
            .iter()
            .filter(|c| c.contains("resizewindowpixel") && c.contains("0xstuck01"))
            .count();
        assert_eq!(
            stuck_resize_count, 1,
            "the permanently-stuck candidate must still only be dispatched \
             once, not re-issued every pass, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// A resize whose computed delta never changes pass over pass (e.g.
    /// because the live client's reported size never moves, meaning
    /// Hyprland rejected the previous attempt) must only be dispatched once.
    /// Re-issuing the identical resize every pass produces a warning on
    /// every restore with no corrective effect — observed live as repeated
    /// "Invalid size" rejections despite the final geometry matching the
    /// saved session exactly.
    #[tokio::test]
    async fn converge_tiled_sizes_does_not_redispatch_identical_stuck_delta() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let clients_json = r#"[{"address":"0xstuck01","class":"discord","pid":1,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[0,0],"size":[700,500],"floating":false,"fullscreen":0}]"#
            .to_string();

        let (mock1, log, _clients) = RecordingSocket1::with_clients(&sock1, clients_json);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);

        let mut window = entry_with_position("discord", "4", (0, 0));
        window.size = Some((800, 600));
        let session = SessionFile {
            session: crate::models::SessionMeta {
                name: "t".to_string(),
                timestamp: 0,
            },
            windows: vec![window],
        };

        let mut addresses = HashMap::new();
        addresses.insert(0usize, "stuck01".to_string());
        let adopted = HashSet::new();

        engine
            .converge_tiled_sizes(&session, &ctl, &addresses, &adopted)
            .await;

        let commands = log.lock().await;
        let resize_count = commands
            .iter()
            .filter(|c| c.contains("resizewindowpixel"))
            .count();
        assert_eq!(
            resize_count, 1,
            "an unchanging delta must only be dispatched once, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// Regression test for the real-reboot failure that motivated switching
    /// from a relative delta to an absolute target: a resize computed from
    /// the live size (`resizewindowpixel {dw} {dh}`) was observed to get
    /// rejected as "Invalid size" for small, ordinary-looking deltas like
    /// (4, -11). The dispatch must instead always be the saved absolute
    /// size via `resizewindowpixel exact`, which is immune to that
    /// rejection and to compounding across passes.
    #[tokio::test]
    async fn converge_tiled_sizes_dispatches_absolute_exact_size_not_relative_delta() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let clients_json = r#"[{"address":"0xdiscor1","class":"discord","pid":1,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[2570,45],"size":[120,62],"floating":false,"fullscreen":0}]"#
            .to_string();

        let (mock1, log, _clients) = RecordingSocket1::with_clients(&sock1, clients_json);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);

        let mut window = entry_with_position("discord", "4", (2570, 45));
        window.size = Some((1269, 660));
        let session = SessionFile {
            session: crate::models::SessionMeta {
                name: "t".to_string(),
                timestamp: 0,
            },
            windows: vec![window],
        };

        let mut addresses = HashMap::new();
        addresses.insert(0usize, "discor1".to_string());
        let adopted = HashSet::new();

        engine
            .converge_tiled_sizes(&session, &ctl, &addresses, &adopted)
            .await;

        let commands = log.lock().await;
        let resize_cmd = commands
            .iter()
            .find(|c| c.contains("resizewindowpixel"))
            .unwrap_or_else(|| panic!("expected a resize dispatch, got: {commands:?}"));
        assert!(
            resize_cmd.contains("resizewindowpixel exact 1269 660,address:0xdiscor1"),
            "resize must request the saved absolute size via `exact`, not a delta \
             computed from the live size, got: {resize_cmd:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// Regression test for the real second-reboot failure: re-dispatching an
    /// identical `exact` resize to the same tiled window on every pass (it
    /// kept winning the one-per-pass slot since nothing else was
    /// out-of-tolerance) made the live size drift *worse* each pass —
    /// doubling the mismatch every time, with every dispatch reporting "ok" —
    /// until the window was squeezed to a sliver. Simulates that drift
    /// directly: after the window's first resize is dispatched, its live
    /// geometry is mutated to a *worse* mismatch than before (standing in for
    /// this self-inflicted drift). A worse mismatch must not be treated as
    /// grounds for another attempt — only exactly one dispatch may occur.
    #[tokio::test]
    async fn converge_tiled_sizes_does_not_retry_after_a_worsening_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let initial_json = r#"[{"address":"0xslack01","class":"slack","pid":1,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[2570,715],"size":[1265,704],"floating":false,"fullscreen":0}]"#
            .to_string();
        // Worse than the initial mismatch, not a step closer — standing in
        // for the observed live doubling-per-pass drift.
        let worse_json = r#"[{"address":"0xslack01","class":"slack","pid":1,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[2570,715],"size":[1265,363],"floating":false,"fullscreen":0}]"#
            .to_string();

        let (mock1, log, clients) = RecordingSocket1::with_clients(&sock1, initial_json);
        let s1 = tokio::spawn(mock1.serve());

        let watcher_log = log.clone();
        let watcher = tokio::spawn(async move {
            loop {
                let seen = watcher_log
                    .lock()
                    .await
                    .iter()
                    .any(|c| c.contains("resizewindowpixel"));
                if seen {
                    *clients.lock().await = worse_json;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);

        let mut window = entry_with_position("slack", "4", (2570, 715));
        window.size = Some((1275, 715));
        let session = SessionFile {
            session: crate::models::SessionMeta {
                name: "t".to_string(),
                timestamp: 0,
            },
            windows: vec![window],
        };

        let mut addresses = HashMap::new();
        addresses.insert(0usize, "slack01".to_string());
        let adopted = HashSet::new();

        engine
            .converge_tiled_sizes(&session, &ctl, &addresses, &adopted)
            .await;

        watcher.abort();

        let commands = log.lock().await;
        let resize_count = commands
            .iter()
            .filter(|c| c.contains("resizewindowpixel"))
            .count();
        assert_eq!(
            resize_count, 1,
            "a worsening mismatch must not trigger a retry — only the \
             initial dispatch should ever go out, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// A window that's genuinely, gradually settling toward its target
    /// across passes (mismatch strictly shrinking pass over pass) must keep
    /// getting corrective dispatches rather than being abandoned after one
    /// attempt — the fix for the worsening case must not overcorrect into
    /// giving up on legitimate incremental progress.
    #[tokio::test]
    async fn converge_tiled_sizes_keeps_retrying_while_mismatch_improves() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let far_json = r#"[{"address":"0xslack01","class":"slack","pid":1,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[2570,715],"size":[1265,600],"floating":false,"fullscreen":0}]"#
            .to_string();
        let closer_json = r#"[{"address":"0xslack01","class":"slack","pid":1,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[2570,715],"size":[1265,660],"floating":false,"fullscreen":0}]"#
            .to_string();
        let converged_json = r#"[{"address":"0xslack01","class":"slack","pid":1,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[2570,715],"size":[1275,715],"floating":false,"fullscreen":0}]"#
            .to_string();

        let (mock1, log, clients) = RecordingSocket1::with_clients(&sock1, far_json);
        let s1 = tokio::spawn(mock1.serve());

        let watcher_log = log.clone();
        let watcher_clients = clients.clone();
        let watcher = tokio::spawn(async move {
            loop {
                let count = watcher_log
                    .lock()
                    .await
                    .iter()
                    .filter(|c| c.contains("resizewindowpixel"))
                    .count();
                match count {
                    1 => *watcher_clients.lock().await = closer_json.clone(),
                    2 => {
                        *watcher_clients.lock().await = converged_json.clone();
                        break;
                    }
                    _ => {}
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);

        let mut window = entry_with_position("slack", "4", (2570, 715));
        window.size = Some((1275, 715));
        let session = SessionFile {
            session: crate::models::SessionMeta {
                name: "t".to_string(),
                timestamp: 0,
            },
            windows: vec![window],
        };

        let mut addresses = HashMap::new();
        addresses.insert(0usize, "slack01".to_string());
        let adopted = HashSet::new();

        engine
            .converge_tiled_sizes(&session, &ctl, &addresses, &adopted)
            .await;

        watcher.abort();

        let commands = log.lock().await;
        let resize_count = commands
            .iter()
            .filter(|c| c.contains("resizewindowpixel"))
            .count();
        assert_eq!(
            resize_count, 2,
            "a strictly improving mismatch must keep getting dispatched \
             until it converges, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    // --- converge_split_ratios: internal-internal split correction ---

    /// Regression test for the real-reboot failure: a 4-leaf tree shaped
    /// like a 2x2 grid (two columns, each an independent split of two
    /// leaves) has a root split whose *both* children are sub-trees, so no
    /// window is directly parented by it and `layoutmsg splitratio` can
    /// never reach it (see `pixel_ratio_steps_capture_2x2_grid_root_split`
    /// in `dwindle.rs`). The old flat per-leaf `resizewindowpixel` diff in
    /// `converge_tiled_sizes` had no way to express this — it only ever
    /// compared one leaf's own saved absolute size against its live size,
    /// with no notion of "your entire column's width comes from an
    /// ancestor ratio that was never set." `converge_split_ratios` instead
    /// derives the delta from the split's saved ratio and the current
    /// combined extent of both sides, then resizes only the matching axis
    /// (dh must stay 0 for a Horizontal split) via the leftmost leaf of one
    /// side — the representative window whose nearest opposite-orientation
    /// ancestor is exactly this split.
    #[tokio::test]
    async fn converge_split_ratios_corrects_root_split_unreachable_by_splitratio() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        // Live: an even 760/760 column split (total 1520). Saved: a 0.6
        // ratio, i.e. the left column should be 912px wide.
        let clients_json = r#"[{"address":"0xa0001","class":"Alacritty","pid":1,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[0,0],"size":[760,900],"floating":false,"fullscreen":0},
            {"address":"0xdiscord1","class":"discord","pid":2,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[770,0],"size":[760,900],"floating":false,"fullscreen":0}]"#
            .to_string();

        let (mock1, log, _clients) = RecordingSocket1::with_clients(&sock1, clients_json);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);

        let mut ws_plans = HashMap::new();
        ws_plans.insert(
            "4".to_string(),
            DwindlePlan {
                steps: vec![],
                ratio_steps: vec![],
                pixel_ratio_steps: vec![dwindle::PixelRatioStep {
                    dir: dwindle::SplitDir::Horizontal,
                    ratio: 0.6,
                    first_leaf_idx: 0,
                    second_leaf_idx: 2,
                }],
            },
        );

        let mut addresses = HashMap::new();
        addresses.insert(0usize, "a0001".to_string());
        addresses.insert(2usize, "discord1".to_string());
        let adopted = HashSet::new();

        engine
            .converge_split_ratios(&ws_plans, &ctl, &addresses, &adopted)
            .await;

        let commands = log.lock().await;
        let resizes: Vec<&String> = commands
            .iter()
            .filter(|c| c.contains("resizewindowpixel"))
            .collect();
        assert_eq!(
            resizes.len(),
            1,
            "expected exactly one resize to correct the root split, got: {commands:?}"
        );
        assert!(
            resizes[0].contains("resizewindowpixel 152 0,address:0xa0001"),
            "expected a width-only (dh=0) correction of +152px on the left \
             column's representative leaf (912 target - 760 live), got: {resizes:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|c| c.contains("address:0xdiscord1") && c.contains("resizewindowpixel")),
            "the second side's representative leaf must never itself be \
             resized, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// An adopted window on either side of an internal-internal split must
    /// not be queried or resized — same reasoning as
    /// `converge_tiled_sizes_skips_adopted_window`.
    #[tokio::test]
    async fn converge_split_ratios_skips_adopted_window() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);

        let mut ws_plans = HashMap::new();
        ws_plans.insert(
            "4".to_string(),
            DwindlePlan {
                steps: vec![],
                ratio_steps: vec![],
                pixel_ratio_steps: vec![dwindle::PixelRatioStep {
                    dir: dwindle::SplitDir::Horizontal,
                    ratio: 0.6,
                    first_leaf_idx: 0,
                    second_leaf_idx: 2,
                }],
            },
        );

        let mut addresses = HashMap::new();
        addresses.insert(0usize, "adopted04".to_string());
        addresses.insert(2usize, "fresh04".to_string());
        let mut adopted = HashSet::new();
        adopted.insert(0);

        engine
            .converge_split_ratios(&ws_plans, &ctl, &addresses, &adopted)
            .await;

        let commands = log.lock().await;
        assert!(
            commands.is_empty(),
            "a split with an adopted window on either side must not be \
             queried or resized at all, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// Two independent internal-internal splits (e.g. in different
    /// workspaces) must never both be resized in the same pass, mirroring
    /// `converge_tiled_sizes_never_resizes_two_shared_edge_windows_in_one_pass`:
    /// resizing one split's representative leaf can change what a later
    /// step measures, so serializing to one resize per pass avoids the same
    /// fight. Both deltas here are permanently stuck (static mock
    /// geometry), so the first candidate gets its one dispatch, is found
    /// unchanged on the next pass, and only then does the second candidate
    /// get its turn.
    #[tokio::test]
    async fn converge_split_ratios_never_resizes_two_splits_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let clients_json = r#"[{"address":"0xa0001","class":"Alacritty","pid":1,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[0,0],"size":[760,900],"floating":false,"fullscreen":0},
            {"address":"0xdiscord1","class":"discord","pid":2,
            "workspace":{"id":4,"name":"4"},"monitor":0,
            "at":[770,0],"size":[760,900],"floating":false,"fullscreen":0},
            {"address":"0xwin4","class":"kitty","pid":3,
            "workspace":{"id":5,"name":"5"},"monitor":0,
            "at":[0,0],"size":[100,300],"floating":false,"fullscreen":0},
            {"address":"0xwin6","class":"kitty","pid":4,
            "workspace":{"id":5,"name":"5"},"monitor":0,
            "at":[0,310],"size":[100,300],"floating":false,"fullscreen":0}]"#
            .to_string();

        let (mock1, log, _clients) = RecordingSocket1::with_clients(&sock1, clients_json);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);

        let mut ws_plans = HashMap::new();
        ws_plans.insert(
            "4".to_string(),
            DwindlePlan {
                steps: vec![],
                ratio_steps: vec![],
                pixel_ratio_steps: vec![dwindle::PixelRatioStep {
                    dir: dwindle::SplitDir::Horizontal,
                    ratio: 0.6,
                    first_leaf_idx: 0,
                    second_leaf_idx: 2,
                }],
            },
        );
        ws_plans.insert(
            "5".to_string(),
            DwindlePlan {
                steps: vec![],
                ratio_steps: vec![],
                pixel_ratio_steps: vec![dwindle::PixelRatioStep {
                    dir: dwindle::SplitDir::Vertical,
                    ratio: 0.4,
                    first_leaf_idx: 4,
                    second_leaf_idx: 6,
                }],
            },
        );

        let mut addresses = HashMap::new();
        addresses.insert(0usize, "a0001".to_string());
        addresses.insert(2usize, "discord1".to_string());
        addresses.insert(4usize, "win4".to_string());
        addresses.insert(6usize, "win6".to_string());
        let adopted = HashSet::new();

        engine
            .converge_split_ratios(&ws_plans, &ctl, &addresses, &adopted)
            .await;

        let commands = log.lock().await;
        let resize_indices: Vec<usize> = commands
            .iter()
            .enumerate()
            .filter(|(_, c)| c.contains("resizewindowpixel"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            resize_indices.len(),
            2,
            "expected exactly one resize each for the two independent \
             splits (workspace 4 first, then workspace 5), got: {commands:?}"
        );
        let (first_idx, second_idx) = (resize_indices[0], resize_indices[1]);
        assert!(
            commands[first_idx].contains("address:0xa0001"),
            "workspace 4's split must be corrected first (sorted before \
             workspace 5), got: {commands:?}"
        );
        assert!(
            commands[second_idx].contains("address:0xwin4"),
            "workspace 5's split must be corrected second, got: {commands:?}"
        );
        assert!(
            commands[first_idx + 1..second_idx]
                .iter()
                .any(|c| c.contains("j/clients")),
            "workspace 5's split must never be resized in the same pass as \
             workspace 4's — there must be a recheck confirming workspace \
             4 is stuck before workspace 5 gets a turn, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// An adopted window that was already fullscreen must not have
    /// `fullscreen` re-toggled — that would exit fullscreen instead of
    /// restoring it, since the dispatch toggles rather than sets state.
    #[tokio::test]
    async fn apply_fullscreen_skips_adopted_window() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);

        let mut adopted_window = entry_with_position("mpv", "2", (0, 0));
        adopted_window.fullscreen = true;
        let mut fresh_window = entry_with_position("mpv", "2", (0, 0));
        fresh_window.fullscreen = true;
        let session = SessionFile {
            session: crate::models::SessionMeta {
                name: "t".to_string(),
                timestamp: 0,
            },
            windows: vec![adopted_window, fresh_window],
        };

        let mut addresses = HashMap::new();
        addresses.insert(0usize, "adopted03".to_string());
        addresses.insert(1usize, "fresh03".to_string());
        let mut adopted = HashSet::new();
        adopted.insert(0);

        engine
            .apply_fullscreen(&session, &ctl, &addresses, &adopted)
            .await
            .unwrap();

        let commands = log.lock().await;
        assert!(
            !commands.iter().any(|c| c.contains("address:0xadopted03")),
            "adopted window's fullscreen state must not be toggled, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("fullscreen 0,address:0xfresh03")),
            "freshly-launched fullscreen window must still be toggled, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    // --- LiveWindowPool::find_unclaimed_racing_window ---

    /// A window that appeared sometime after the daemon-startup snapshot
    /// (i.e. not in `known_at_startup`) is a valid racing-autostart match.
    #[tokio::test]
    async fn find_unclaimed_racing_window_matches_new_live_client() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, _log, _clients) =
            RecordingSocket1::with_clients(&sock1, single_client_json("0xslack1", "slack", "1"));
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let mut pool = LiveWindowPool::new(vec![]);

        let addr = pool.find_unclaimed_racing_window(&ctl, "slack").await;
        assert_eq!(addr, Some("slack1".to_string()));

        s1.abort();
    }

    /// A window already present at daemon startup must be left alone here —
    /// it's `take_match`'s job, keyed on workspace + saved position, not
    /// this method's (which deliberately ignores workspace).
    #[tokio::test]
    async fn find_unclaimed_racing_window_ignores_known_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, _log, _clients) =
            RecordingSocket1::with_clients(&sock1, single_client_json("0xslack1", "slack", "1"));
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let mut pool = LiveWindowPool::new(vec![make_tracked("0xslack1", "slack", "1", (0, 0))]);

        let addr = pool.find_unclaimed_racing_window(&ctl, "slack").await;
        assert!(
            addr.is_none(),
            "a window present at daemon startup must not be claimed by the racing-window check"
        );

        s1.abort();
    }

    /// The same live window must never be handed out twice.
    #[tokio::test]
    async fn find_unclaimed_racing_window_does_not_double_claim() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, _log, _clients) =
            RecordingSocket1::with_clients(&sock1, single_client_json("0xslack1", "slack", "1"));
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let mut pool = LiveWindowPool::new(vec![]);

        assert!(
            pool.find_unclaimed_racing_window(&ctl, "slack")
                .await
                .is_some()
        );
        assert!(
            pool.find_unclaimed_racing_window(&ctl, "slack")
                .await
                .is_none(),
            "the same live window must not be claimed twice"
        );

        s1.abort();
    }

    // --- racing-autostart adoption in bsp_launch_and_track ---

    /// If the app is already live (racing autostart) before hypr-persist even
    /// attempts to launch it, adopt it directly: no window rule, no `exec`.
    #[tokio::test]
    async fn bsp_launch_and_track_adopts_racing_window_before_exec() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log, _clients) =
            RecordingSocket1::with_clients(&sock1, single_client_json("0xslack1", "slack", "1"));
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);
        let (_tx, mut events) = mpsc::channel::<HyprEvent>(8);

        let window = make_entry("slack", "slack", None, None);
        let mut active_rules = Vec::new();
        let mut pending = Vec::new();
        let mut rule_counter = 0usize;
        let mut live_pool = LiveWindowPool::new(vec![]);

        let outcome = engine
            .bsp_launch_and_track(
                &window,
                &ctl,
                &mut events,
                &mut active_rules,
                &mut pending,
                &mut rule_counter,
                None,
                &mut live_pool,
            )
            .await
            .unwrap();

        match outcome {
            LaunchOutcome::Adopted(addr) => assert_eq!(addr, "slack1"),
            other => panic!("expected LaunchOutcome::Adopted, got {other:?}"),
        }
        assert!(
            active_rules.is_empty(),
            "no window rule should be created when adopting before launch"
        );
        assert!(pending.is_empty(), "nothing should be deferred");

        let commands = log.lock().await;
        assert!(
            !commands.iter().any(|c| c.contains("exec")),
            "must not launch a duplicate when a racing window is already live, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("movetoworkspacesilent 1,address:0xslack1")),
            "expected the adopted window to be moved onto its target workspace, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// If the racing window doesn't show up in time for the pre-exec check,
    /// but is live by the time `wait_for_open_event` times out, the timeout
    /// path must re-check and adopt it instead of deferring to the
    /// late-window watcher.
    #[tokio::test]
    async fn bsp_launch_and_track_adopts_racing_window_after_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log, clients_handle) = RecordingSocket1::with_clients(&sock1, "[]".to_string());
        let s1 = tokio::spawn(mock1.serve());

        let handle = clients_handle.clone();
        let updater = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            *handle.lock().await = single_client_json("0xslack1", "slack", "1");
        });

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine =
            RestoreEngine::new(true, true).with_window_appear_timeout(Duration::from_millis(80));
        let (_tx, mut events) = mpsc::channel::<HyprEvent>(8);

        let window = make_entry("slack", "slack", None, None);
        let mut active_rules = Vec::new();
        let mut pending = Vec::new();
        let mut rule_counter = 0usize;
        let mut live_pool = LiveWindowPool::new(vec![]);

        let outcome = engine
            .bsp_launch_and_track(
                &window,
                &ctl,
                &mut events,
                &mut active_rules,
                &mut pending,
                &mut rule_counter,
                None,
                &mut live_pool,
            )
            .await
            .unwrap();

        match outcome {
            LaunchOutcome::Adopted(addr) => assert_eq!(addr, "slack1"),
            other => panic!("expected LaunchOutcome::Adopted after timeout-recheck, got {other:?}"),
        }
        assert!(
            pending.is_empty(),
            "must not defer to the late-window watcher once adopted, got {} pending",
            pending.len()
        );

        let commands = log.lock().await;
        assert!(
            commands.iter().any(|c| c.contains("exec slack")),
            "expected the normal launch attempt to have fired first, got: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c.contains(":enable false")),
            "expected the now-unused window rule to be disabled, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
        updater.abort();
    }

    /// Two session-file entries of the same class (e.g. two terminal
    /// windows) must each get their own live window. Before the window a
    /// genuinely-launched entry opens was marked claimed, a second entry
    /// for the same `app_id` would see it via `find_unclaimed_racing_window`
    /// (it postdates `known_at_startup`, which is empty on a real
    /// login/reboot) and wrongly adopt it instead of launching its own —
    /// the live bug this test guards against.
    #[tokio::test]
    async fn bsp_launch_and_track_does_not_steal_previously_opened_window_of_same_class() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log, clients_handle) = RecordingSocket1::with_clients(&sock1, "[]".to_string());
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);
        let (event_tx, mut events) = mpsc::channel::<HyprEvent>(8);
        let mut active_rules = Vec::new();
        let mut pending = Vec::new();
        let mut rule_counter = 0usize;
        let mut live_pool = LiveWindowPool::new(vec![]);
        let window = make_entry("alacritty", "alacritty", None, None);

        event_tx
            .send(HyprEvent::OpenWindow {
                address: "alac1".to_string(),
                workspace: "1".to_string(),
                class: "alacritty".to_string(),
            })
            .await
            .unwrap();

        let first = engine
            .bsp_launch_and_track(
                &window,
                &ctl,
                &mut events,
                &mut active_rules,
                &mut pending,
                &mut rule_counter,
                None,
                &mut live_pool,
            )
            .await
            .unwrap();
        match first {
            LaunchOutcome::Opened(addr) => assert_eq!(addr, "alac1"),
            other => panic!("expected the first entry to open its own window, got {other:?}"),
        }

        // Hyprland now genuinely reports the first window as live, exactly
        // as it would once `exec` has actually run.
        *clients_handle.lock().await = single_client_json("0xalac1", "alacritty", "1");
        event_tx
            .send(HyprEvent::OpenWindow {
                address: "alac2".to_string(),
                workspace: "1".to_string(),
                class: "alacritty".to_string(),
            })
            .await
            .unwrap();

        let second = engine
            .bsp_launch_and_track(
                &window,
                &ctl,
                &mut events,
                &mut active_rules,
                &mut pending,
                &mut rule_counter,
                None,
                &mut live_pool,
            )
            .await
            .unwrap();
        match second {
            LaunchOutcome::Opened(addr) => assert_eq!(addr, "alac2"),
            other => panic!(
                "second entry must launch and open its own window instead of stealing the \
                 first's, got {other:?}"
            ),
        }

        let commands = log.lock().await;
        assert_eq!(
            commands
                .iter()
                .filter(|c| c.contains("exec alacritty"))
                .count(),
            2,
            "both entries must have issued their own real launch, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    // --- racing-autostart adoption composed with execute_bsp_plans ---

    /// A racing-adopted window must land in `adopted` (so downstream
    /// convergence/splitratio/fullscreen steps skip it, same as a
    /// startup-pool adoption) and later steps must anchor against its
    /// address, exactly like a freshly-launched window would.
    #[tokio::test]
    async fn execute_bsp_plans_adopts_racing_window_and_anchors_next_step() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        // Slack raced onto workspace 9 (Hyprland's default placement for an
        // independent autostart), not workspace 4 where the plan wants it —
        // mirroring the real bug, where the racing window is never on its
        // target workspace yet.
        let (mock1, log, _clients) =
            RecordingSocket1::with_clients(&sock1, single_client_json("0xslack1", "slack", "9"));
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(false, true);

        let (event_tx, mut events) = mpsc::channel::<HyprEvent>(8);
        event_tx
            .send(HyprEvent::OpenWindow {
                address: "discordwin".to_string(),
                workspace: "4".to_string(),
                class: "discord".to_string(),
            })
            .await
            .unwrap();

        let session = SessionFile {
            session: crate::models::SessionMeta {
                name: "t".to_string(),
                timestamp: 0,
            },
            windows: vec![
                entry_with_position("slack", "4", (0, 0)),
                entry_with_position("discord", "4", (500, 0)),
            ],
        };

        let mut ws_plans = HashMap::new();
        ws_plans.insert(
            "4".to_string(),
            DwindlePlan {
                steps: vec![
                    dwindle::RestoreStep {
                        window_idx: 0,
                        focus_idx: None,
                        preselect: None,
                    },
                    dwindle::RestoreStep {
                        window_idx: 1,
                        focus_idx: Some(0),
                        preselect: Some(dwindle::PreselDir::Right),
                    },
                ],
                ratio_steps: vec![],
                pixel_ratio_steps: vec![],
            },
        );

        let mut report = RestoreReport::default();
        let mut active_rules = Vec::new();
        let mut pending = Vec::new();
        let mut live_pool = LiveWindowPool::new(vec![]);

        let (addresses, adopted) = engine
            .execute_bsp_plans(
                &session,
                &ctl,
                &mut events,
                &mut report,
                &ws_plans,
                &mut active_rules,
                &mut pending,
                &mut live_pool,
            )
            .await
            .unwrap();

        assert_eq!(report.restored, 2);
        assert_eq!(addresses.get(&0), Some(&"slack1".to_string()));
        assert_eq!(addresses.get(&1), Some(&"discordwin".to_string()));
        assert!(
            adopted.contains(&0),
            "slack should have been adopted via the racing-autostart check"
        );
        assert!(
            !adopted.contains(&1),
            "discord was freshly launched, not adopted"
        );

        let commands = log.lock().await;
        assert!(
            !commands.iter().any(|c| c.contains("exec slack")),
            "must not launch slack, it was already racing-live, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("movetoworkspacesilent 4,address:0xslack1")),
            "expected slack to be moved onto its target workspace, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("focuswindow address:0xslack1")),
            "expected discord's step to anchor against the adopted slack window, got: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c.contains("layoutmsg preselect r")),
            "expected preselect for discord's step, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }

    /// Regression test for a real-reboot bug: an anchored window (slack,
    /// anchored to discord) whose launch is slow enough that its
    /// `OpenWindow` event arrives well after `exec`, with an unrelated
    /// window's `CloseWindow` event arriving during that wait (ordinary
    /// session-startup churn, observed live: `org.kde.ksecretd` and
    /// `dev.deedles.Trayscale` both closed on their own while slack was
    /// still starting). The old code armed `focuswindow`+`layoutmsg
    /// preselect` against the anchor immediately before `exec`, trusting
    /// that binding to survive until slack's window actually appeared —
    /// live-tested and observed to fail: slack landed grouped with unrelated
    /// windows instead of next to discord. The fix defers the anchor/
    /// preselect dispatch until slack's address is confirmed via its
    /// `OpenWindow` event, issuing it fresh immediately before the final
    /// settle. This asserts that ordering directly: the anchor focus and
    /// preselect dispatches must not appear before slack's `exec`, and must
    /// appear right before its settling `setfloating`.
    ///
    /// Discord (the anchor) is adopted from the live pool rather than
    /// launched, so it needs no `OpenWindow` event of its own — otherwise
    /// its own splash-supersede grace-period wait (`retile_superseding_window`)
    /// would race the shared events channel against slack's delayed event
    /// and could swallow it first, an artifact of the single shared channel
    /// rather than of the bug under test.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn execute_bsp_plans_finalizes_anchor_placement_after_delayed_open_with_intervening_close()
     {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log) = RecordingSocket1::new(&sock1);
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(false, true);

        let (event_tx, mut events) = mpsc::channel::<HyprEvent>(8);

        // Slack's OpenWindow is delayed well past exec; an unrelated
        // window's CloseWindow arrives first, mirroring the live churn that
        // preceded the misplacement.
        let delayer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(
                event_tx
                    .send(HyprEvent::CloseWindow {
                        address: "unrelated1".to_string(),
                    })
                    .await,
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(
                event_tx
                    .send(HyprEvent::OpenWindow {
                        address: "slackwin".to_string(),
                        workspace: "4".to_string(),
                        class: "slack".to_string(),
                    })
                    .await,
            );
        });

        let session = SessionFile {
            session: crate::models::SessionMeta {
                name: "t".to_string(),
                timestamp: 0,
            },
            windows: vec![
                entry_with_position("discord", "4", (0, 0)),
                entry_with_position("slack", "4", (500, 0)),
            ],
        };

        let mut ws_plans = HashMap::new();
        ws_plans.insert(
            "4".to_string(),
            DwindlePlan {
                steps: vec![
                    dwindle::RestoreStep {
                        window_idx: 0,
                        focus_idx: None,
                        preselect: None,
                    },
                    dwindle::RestoreStep {
                        window_idx: 1,
                        focus_idx: Some(0),
                        preselect: Some(dwindle::PreselDir::Bottom),
                    },
                ],
                ratio_steps: vec![],
                pixel_ratio_steps: vec![],
            },
        );

        let mut report = RestoreReport::default();
        let mut active_rules = Vec::new();
        let mut pending = Vec::new();
        let mut live_pool =
            LiveWindowPool::new(vec![make_tracked("0xdiscordwin", "discord", "4", (0, 0))]);

        let (addresses, adopted) = engine
            .execute_bsp_plans(
                &session,
                &ctl,
                &mut events,
                &mut report,
                &ws_plans,
                &mut active_rules,
                &mut pending,
                &mut live_pool,
            )
            .await
            .unwrap();

        assert_eq!(report.restored, 2);
        assert_eq!(addresses.get(&0), Some(&"discordwin".to_string()));
        assert!(adopted.contains(&0), "discord came from the live pool");
        assert_eq!(addresses.get(&1), Some(&"slackwin".to_string()));
        assert!(!adopted.contains(&1), "slack was freshly launched");

        let commands = log.lock().await;

        let exec_idx = commands
            .iter()
            .position(|c| c.contains("exec slack"))
            .expect("expected slack's exec dispatch");
        let anchor_focus_idx = commands
            .iter()
            .position(|c| c.contains("focuswindow address:0xdiscordwin"));
        let preselect_idx = commands
            .iter()
            .position(|c| c.contains("layoutmsg preselect d"));

        assert!(
            anchor_focus_idx.is_none_or(|i| i > exec_idx),
            "anchor focus must not be armed before exec — a binding set up before an \
             indeterminate wait for OpenWindow can be silently invalidated by an \
             intervening focus change, got: {commands:?}"
        );
        assert!(
            preselect_idx.is_some_and(|i| i > exec_idx),
            "preselect must be issued after slack's window is confirmed to exist, not \
             armed before exec, got: {commands:?}"
        );

        let settle_idx = commands
            .iter()
            .rposition(|c| c.contains("setfloating address:0xslackwin"))
            .expect("expected a settle dispatch for slack");
        assert!(
            preselect_idx.unwrap() < settle_idx,
            "preselect must be issued immediately before the final settle, got: {commands:?}"
        );

        drop(commands);
        s1.abort();
        delayer.abort();
    }

    // --- racing-autostart adoption in launch_and_track / restore_window ---

    /// The simple (non-BSP) restore path must also adopt a racing-live
    /// window instead of launching a duplicate.
    #[tokio::test]
    async fn restore_window_adopts_racing_window_before_exec() {
        let dir = tempfile::tempdir().unwrap();
        let sock1 = dir.path().join("s1.sock");
        let sock2 = dir.path().join("s2.sock");

        let (mock1, log, _clients) =
            RecordingSocket1::with_clients(&sock1, single_client_json("0xfoot9", "foot", "1"));
        let s1 = tokio::spawn(mock1.serve());

        let ctl = HyprCtl::new(HyprSocketPaths::new(sock1, sock2));
        let engine = RestoreEngine::new(true, true);
        let (_tx, mut events) = mpsc::channel::<HyprEvent>(8);

        let mut active_rules = Vec::new();
        let mut pending = Vec::new();
        let mut live_pool = LiveWindowPool::new(vec![]);
        let window = make_entry("foot", "foot", None, None);

        let adopted = engine
            .restore_window(
                &window,
                &ctl,
                &mut events,
                &mut active_rules,
                &mut pending,
                &mut live_pool,
                MasterRole::None,
            )
            .await
            .unwrap();

        assert!(adopted, "expected the racing-live window to be adopted");
        assert!(active_rules.is_empty());
        assert!(pending.is_empty());

        let commands = log.lock().await;
        assert!(
            !commands.iter().any(|c| c.contains("exec")),
            "must not launch a duplicate, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("movetoworkspacesilent 1,address:0xfoot9")),
            "expected workspace placement, got: {commands:?}"
        );
        drop(commands);

        s1.abort();
    }
}
