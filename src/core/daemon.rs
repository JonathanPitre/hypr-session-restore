use anyhow::{Context, Result};
use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1};
use signal_hook_tokio::Signals;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

use crate::config::Config;
use crate::core::restore::{RestoreEngine, validate_session};
use crate::core::scheduler::{
    Delta, SaveHistory, SaveReason, SettleState, Tick, WindowStamp, classify, on_tick,
};
use crate::core::snapshot::SnapshotEngine;
use crate::core::stamp::{AutoRestore, AutoRestoreInput, decide_auto_restore};
use crate::core::state::{StateManager, normalize_address};
use crate::ipc::client::HyprCtl;
use crate::ipc::event_listener;
use crate::models::{HyprClient, HyprEvent, TrackedWindow};
use crate::resolver::AppResolver;

pub async fn run(config: Config) -> Result<()> {
    let ctl = Arc::new(HyprCtl::from_env()?);
    let resolver = Arc::new(AppResolver::new(&config));
    let state = Arc::new(Mutex::new(StateManager::new(&config)));
    let snapshot = Arc::new(SnapshotEngine::new(&config)?);

    let (event_tx, mut event_rx) = mpsc::channel::<HyprEvent>(256);
    let paths = ctl.socket_paths().clone();
    let event_handle = tokio::spawn(async move {
        if let Err(e) = event_listener::listen(&paths, event_tx).await {
            tracing::error!("event listener error: {e}");
        }
    });

    populate_initial_state(&state, &resolver, &ctl).await?;

    maybe_auto_restore(&config, &snapshot, &state, &resolver, &ctl, &mut event_rx).await?;

    let mut signals =
        Signals::new([SIGINT, SIGTERM, SIGUSR1]).context("registering signal handlers")?;

    let save_interval = Duration::from_secs(config.general.save_interval);
    let mut settle = SettleState {
        delay: Duration::from_secs(config.general.settle_delay),
        deadline: None,
    };
    let target = snapshot.save_target(None)?;
    let startup_saved_count = startup_saved_count(&snapshot, &target);
    let mut history =
        SaveHistory::new(current_stamps(&state, &snapshot).await, startup_saved_count);
    let mut next_wake = std::time::Instant::now() + save_interval;

    tracing::info!(
        "daemon running, tracking {} windows, saving every {}s",
        state.lock().await.window_count(),
        config.general.save_interval
    );

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                let Some(event) = event else {
                    tracing::info!("event stream ended, saving final session...");
                    let saved = save_for_signal(
                        &state, &snapshot, &resolver, &ctl, &mut history, &mut settle, SaveReason::Shutdown,
                    ).await;
                    tracing::debug!("final save wrote={saved}");
                    break;
                };
                let changed = handle_event(event, &state, &resolver, &ctl).await;
                if changed {
                    next_wake = periodic_wake(
                        &state,
                        &snapshot,
                        &resolver,
                        &ctl,
                        &mut history,
                        &mut settle,
                        save_interval,
                    ).await;
                }
            }

            () = tokio::time::sleep_until(tokio::time::Instant::from_std(next_wake)) => {
                next_wake = periodic_wake(
                    &state,
                    &snapshot,
                    &resolver,
                    &ctl,
                    &mut history,
                    &mut settle,
                    save_interval,
                )
                .await;
            }

            Some(sig) = futures::StreamExt::next(&mut signals) => {
                let exiting = matches!(sig, SIGTERM | SIGINT);
                if exiting {
                    tracing::info!("received signal {sig}, saving and exiting...");
                } else if sig == SIGUSR1 {
                    tracing::info!("received SIGUSR1, saving session...");
                }
                let reason = if exiting { SaveReason::Shutdown } else { SaveReason::Explicit };
                if save_for_signal(&state, &snapshot, &resolver, &ctl, &mut history, &mut settle, reason).await {
                    next_wake = std::time::Instant::now() + save_interval;
                }
                if exiting {
                    break;
                }
            }

        }
    }

    event_handle.abort();
    tracing::info!("daemon stopped");
    Ok(())
}

fn startup_saved_count(snapshot: &SnapshotEngine, target: &str) -> Option<usize> {
    if !snapshot.exists(target) {
        return None;
    }
    match snapshot.load(target) {
        Ok(session) => Some(session.windows.len()),
        Err(err) => {
            tracing::warn!("failed to load startup save target '{target}': {err}");
            None
        }
    }
}

async fn maybe_auto_restore(
    config: &Config,
    snapshot: &SnapshotEngine,
    state: &Arc<Mutex<StateManager>>,
    resolver: &Arc<AppResolver>,
    ctl: &HyprCtl,
    event_rx: &mut mpsc::Receiver<HyprEvent>,
) -> Result<()> {
    let preexisting = if config.general.restore_on_start {
        let clients = ctl
            .get_clients()
            .await
            .context("checking preexisting windows")?;
        let state = state.lock().await;
        clients
            .iter()
            .filter(|client| state.should_track(&client.class))
            .count()
    } else {
        0
    };
    let loaded = if config.general.restore_on_start {
        match snapshot.resolve_restore_target(None) {
            Ok(name) => match snapshot.load(&name) {
                Ok(session) => Some((name, session)),
                Err(err) => {
                    tracing::warn!("failed to load restore target: {err}");
                    None
                }
            },
            Err(err) => {
                tracing::debug!("no restore target: {err}");
                None
            }
        }
    } else {
        None
    };
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok();
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|text| text.trim().to_string());
    let decision = decide_auto_restore(&AutoRestoreInput {
        restore_on_start: config.general.restore_on_start,
        state_root: &config.state_root(),
        runtime_dir: runtime.as_deref(),
        signature: signature.as_deref(),
        boot_id: boot_id.as_deref(),
        preexisting_windows: preexisting,
        max_preexisting_windows: config.general.max_preexisting_windows,
        target: loaded
            .as_ref()
            .map(|(name, session)| (name.as_str(), session)),
    });
    let AutoRestore::Schedule(plan) = decision else {
        let AutoRestore::Skip(reason) = decision else {
            return Ok(());
        };
        tracing::info!("auto-restore skipped: {reason:?}");
        return Ok(());
    };
    if let Err(err) = validate_session(&plan.session) {
        tracing::warn!("auto-restore skipped, session rejected: {err}");
        return Ok(());
    }
    if let Err(reason) = plan.burn() {
        tracing::warn!("auto-restore skipped: {reason:?}");
        return Ok(());
    }
    tracing::info!(
        "stamped instance {} at {}",
        plan.instance.as_str(),
        plan.stamp_path().display()
    );
    tracing::info!("waiting for compositor to settle...");
    wait_for_settle(event_rx).await;
    tracing::info!("restoring session '{}'...", plan.name);
    let live_windows: Vec<crate::models::TrackedWindow> =
        state.lock().await.windows().into_iter().cloned().collect();
    let engine = RestoreEngine::new(
        config.general.restore_geometry,
        config.general.restore_layout,
    );
    let (report, _watcher) = match engine.restore(&plan.session, ctl, live_windows).await {
        Ok(result) => result,
        Err(err) => {
            crate::core::notify::notify_outcome(config.general.notify, false, &err.to_string());
            return Err(err);
        }
    };
    populate_initial_state(state, resolver, ctl)
        .await
        .context("reconciling live state after auto-restore")?;
    if report.failed > 0 {
        for (app, err) in &report.errors {
            tracing::warn!("restore failed for {app}: {err}");
        }
    }
    crate::core::notify::notify_outcome(
        config.general.notify,
        report.failed == 0,
        &format!("session '{}'", plan.name),
    );
    Ok(())
}

async fn periodic_wake(
    state: &Arc<Mutex<StateManager>>,
    snapshot: &SnapshotEngine,
    resolver: &AppResolver,
    ctl: &HyprCtl,
    history: &mut SaveHistory,
    settle: &mut SettleState,
    period: Duration,
) -> std::time::Instant {
    {
        let mut state = state.lock().await;
        if state.has_pending_opens()
            && let Err(err) = refresh_state(&mut state, resolver, ctl).await
        {
            tracing::warn!("failed to refresh pending opens: {err}");
            return std::time::Instant::now() + period;
        }
    }
    let stamps = current_stamps(state, snapshot).await;
    let mut delta = classify(history.observed.as_deref(), &stamps);
    // Disk snapshots have no live addresses; defer a smaller startup capture once.
    if history.startup_would_shrink(stamps.len()) {
        delta = if settle.deadline.is_none() || delta == Delta::VanishOnly {
            Delta::VanishOnly
        } else {
            Delta::Unchanged
        };
    }
    let tick = on_tick(
        delta,
        SaveReason::Periodic,
        settle,
        std::time::Instant::now(),
        stamps.len(),
    );
    match tick {
        Tick::Wait(deadline) => {
            tracing::debug!("deferring reduced session capture until {deadline:?}");
            history.observed = Some(stamps);
            deadline
        }
        Tick::Save => {
            save_with_refresh(
                state,
                snapshot,
                resolver,
                ctl,
                history,
                SaveReason::Periodic,
                "periodic",
            )
            .await;
            std::time::Instant::now() + period
        }
        Tick::Idle => {
            history.observed = Some(stamps);
            std::time::Instant::now() + period
        }
    }
}

async fn save_for_signal(
    state: &Arc<Mutex<StateManager>>,
    snapshot: &SnapshotEngine,
    resolver: &AppResolver,
    ctl: &HyprCtl,
    history: &mut SaveHistory,
    settle: &mut SettleState,
    reason: SaveReason,
) -> bool {
    let saved = save_with_refresh(state, snapshot, resolver, ctl, history, reason, "signal").await;
    if saved {
        settle.deadline = None;
    }
    saved
}

async fn current_stamps(
    state: &Arc<Mutex<StateManager>>,
    snapshot: &SnapshotEngine,
) -> Vec<WindowStamp> {
    let state = state.lock().await;
    snapshot
        .capture_windows(&state)
        .into_iter()
        .map(stamp_window)
        .collect()
}

fn stamp_window(window: &TrackedWindow) -> WindowStamp {
    WindowStamp {
        address: normalize_address(&window.address),
        workspace: window.workspace.clone(),
        monitor: window.monitor.clone(),
        floating: window.floating,
        fullscreen: window.fullscreen,
        position: window.position,
        size: window.size,
    }
}

/// Refresh window geometry from Hyprland and save. Hyprland emits no events
/// for tiled resize/position changes, so we must re-query before every save
/// to capture the user's actual layout.
async fn save_with_refresh(
    state: &Arc<Mutex<StateManager>>,
    snapshot: &SnapshotEngine,
    resolver: &AppResolver,
    ctl: &HyprCtl,
    history: &mut SaveHistory,
    reason: SaveReason,
    label: &str,
) -> bool {
    let mut state = state.lock().await;
    if state.window_count() == 0 && !state.has_pending_opens() {
        return false;
    }
    if let Err(err) = refresh_state(&mut state, resolver, ctl).await {
        tracing::warn!("{label} save: failed to refresh state: {err}");
        return false;
    }
    let stamps = snapshot
        .capture_windows(&state)
        .into_iter()
        .map(stamp_window)
        .collect::<Vec<_>>();
    if stamps.is_empty() {
        return false;
    }
    let target = match snapshot.save_target(None) {
        Ok(name) => name,
        Err(e) => {
            tracing::error!("{label} save failed: {e}");
            return false;
        }
    };
    if reason == SaveReason::Shutdown
        && snapshot.exists(&target)
        && history.shutdown_would_shrink(&stamps)
    {
        tracing::info!("shutdown capture lost windows; preserving existing save");
        return false;
    }
    if let Err(e) = snapshot.save(&state, &target) {
        tracing::error!("{label} save failed: {e}");
        return false;
    }
    drop(state);
    history.record_save(stamps);
    true
}

async fn refresh_state(
    state: &mut StateManager,
    resolver: &AppResolver,
    ctl: &HyprCtl,
) -> Result<()> {
    let monitor_map = ctl.get_monitor_map().await?;
    let clients = ctl.get_clients().await?;
    for client in &clients {
        let Some(class) = state.pending_class(&client.address).map(str::to_owned) else {
            continue;
        };
        let Some(monitor) = monitor_map.get(&client.monitor) else {
            continue;
        };
        if client.size.0 <= 0 || client.size.1 <= 0 {
            continue;
        }
        state.add(track_client(client, class, monitor.clone(), resolver));
    }
    state.refresh_geometry(&clients, &monitor_map);
    Ok(())
}

fn track_client(
    client: &HyprClient,
    class: String,
    monitor: String,
    resolver: &AppResolver,
) -> TrackedWindow {
    TrackedWindow {
        address: client.address.clone(),
        launch_cmd: resolver.resolve(&class, client.pid).unwrap_or_default(),
        app_id: class,
        workspace: client.workspace.name.clone(),
        monitor,
        position: client.at,
        size: client.size,
        floating: client.floating,
        fullscreen: client.fullscreen_mode > 0,
        pid: client.pid,
        profile: crate::resolver::profile::detect_browser_profile(client.pid),
    }
}

/// Drain events until there's a 1-second gap, indicating the compositor
/// and its extensions (bars, shells) have finished their startup burst.
async fn wait_for_settle(events: &mut mpsc::Receiver<HyprEvent>) {
    let quiet = Duration::from_secs(1);
    while let Ok(Some(_)) = tokio::time::timeout(quiet, events.recv()).await {}
    tracing::info!("compositor settled");
}

async fn populate_initial_state(
    state: &Arc<Mutex<StateManager>>,
    resolver: &Arc<AppResolver>,
    ctl: &HyprCtl,
) -> Result<()> {
    let clients = ctl.get_clients().await?;
    let monitor_map = ctl.get_monitor_map().await?;
    let mut state = state.lock().await;

    let live_addresses: HashSet<_> = clients
        .iter()
        .map(|client| normalize_address(&client.address))
        .collect();
    let closed_addresses: Vec<_> = state
        .windows()
        .into_iter()
        .filter(|window| !live_addresses.contains(&normalize_address(&window.address)))
        .map(|window| window.address.clone())
        .collect();
    for address in closed_addresses {
        state.remove(&address);
    }

    for c in clients {
        if !state.should_track(&c.class) {
            continue;
        }

        let monitor = monitor_map.get(&c.monitor).cloned().unwrap_or_default();
        state.add(track_client(&c, c.class.clone(), monitor, resolver));
    }

    let count = state.window_count();
    drop(state);
    tracing::info!("populated initial state: {count} windows");
    Ok(())
}

async fn handle_event(
    event: HyprEvent,
    state: &Arc<Mutex<StateManager>>,
    resolver: &Arc<AppResolver>,
    ctl: &HyprCtl,
) -> bool {
    match event {
        HyprEvent::OpenWindow {
            address,
            workspace,
            class,
        } => {
            // Check tracking eligibility without holding mutex across IPC
            let should_track = {
                let state = state.lock().await;
                state.should_track(&class)
            };

            if !should_track {
                return false;
            }
            state.lock().await.defer_open(&address, class.clone());

            let client = match ctl.get_client_by_address(&address).await {
                Ok(Some(client)) => client,
                Ok(None) => return false,
                Err(err) => {
                    tracing::warn!("failed to query opened window {address}: {err}");
                    return false;
                }
            };
            if client.size.0 <= 0 || client.size.1 <= 0 {
                return false;
            }
            let mut window = track_client(&client, class, String::new(), resolver);
            window.workspace = workspace;
            state.lock().await.add(window);
            true
        }
        HyprEvent::CloseWindow { address } => {
            let mut state = state.lock().await;
            state.remove(&address);
            false
        }
        HyprEvent::MoveWindow { address, workspace } => {
            let mut state = state.lock().await;
            state.update_workspace(&address, &workspace)
        }
        HyprEvent::ChangeFloatingMode { address, floating } => {
            let mut state = state.lock().await;
            state.update_floating(&address, floating)
        }
        HyprEvent::Fullscreen { state } => {
            tracing::debug!("fullscreen toggled: {state}");
            false
        }
        HyprEvent::Unknown(ref raw) => {
            tracing::trace!("unhandled event: {raw}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{handle_event, periodic_wake, save_for_signal};
    use crate::config::Config;
    use crate::core::scheduler::{SaveHistory, SaveReason, SettleState};
    use crate::core::snapshot::SnapshotEngine;
    use crate::core::state::StateManager;
    use crate::ipc::client::{HyprCtl, HyprSocketPaths};
    use crate::models::{HyprEvent, TrackedWindow};
    use crate::resolver::AppResolver;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    fn harness() -> (Arc<Mutex<StateManager>>, Arc<AppResolver>, HyprCtl) {
        let config = Config::default();
        (
            Arc::new(Mutex::new(StateManager::new(&config))),
            Arc::new(AppResolver::new(&config)),
            HyprCtl::new(HyprSocketPaths::new(
                PathBuf::from("/tmp/hypr-persist-unused.sock"),
                PathBuf::from("/tmp/hypr-persist-unused.sock2"),
            )),
        )
    }

    fn tracked(address: &str, workspace: &str, floating: bool) -> TrackedWindow {
        TrackedWindow {
            address: address.to_string(),
            app_id: "firefox".to_string(),
            launch_cmd: "firefox".to_string(),
            workspace: workspace.to_string(),
            monitor: String::new(),
            position: (0, 0),
            size: (800, 600),
            floating,
            fullscreen: false,
            pid: 0,
            profile: None,
        }
    }

    #[test]
    fn corrupt_startup_save_has_no_saved_count() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = SnapshotEngine::new_with_dir(dir.path(), true).unwrap();
        std::fs::write(dir.path().join("last.toml"), "[broken").unwrap();

        let target = snapshot.save_target(None).unwrap();
        assert_eq!(super::startup_saved_count(&snapshot, &target), None);
    }

    struct SaveHarness {
        _dir: tempfile::TempDir,
        state: Arc<Mutex<StateManager>>,
        snapshot: SnapshotEngine,
        resolver: AppResolver,
        ctl: HyprCtl,
        history: SaveHistory,
        settle: SettleState,
        server: tokio::task::JoinHandle<()>,
    }

    impl SaveHarness {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("ipc");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = String::new();
                    stream.read_to_string(&mut request).await.unwrap();
                    let response = if request == "j/clients" {
                        "[]"
                    } else {
                        r#"[{"id":0,"name":"TEST"}]"#
                    };
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            });
            let mut state = StateManager::new(&Config::default());
            for address in ["0x1", "0x2", "0x3", "0x4", "0x5", "0x6"] {
                state.add(tracked(address, "1", false));
            }
            let history = SaveHistory::new(
                state
                    .windows()
                    .into_iter()
                    .map(super::stamp_window)
                    .collect(),
                None,
            );
            Self {
                state: Arc::new(Mutex::new(state)),
                snapshot: SnapshotEngine::new_with_dir(&dir.path().join("sessions"), true).unwrap(),
                resolver: AppResolver::new(&Config::default()),
                ctl: HyprCtl::new(HyprSocketPaths::new(socket.clone(), socket)),
                history,
                settle: SettleState {
                    delay: Duration::from_secs(10),
                    deadline: None,
                },
                server,
                _dir: dir,
            }
        }

        async fn save(&mut self, reason: SaveReason) -> bool {
            save_for_signal(
                &self.state,
                &self.snapshot,
                &self.resolver,
                &self.ctl,
                &mut self.history,
                &mut self.settle,
                reason,
            )
            .await
        }

        async fn wake(&mut self) {
            periodic_wake(
                &self.state,
                &self.snapshot,
                &self.resolver,
                &self.ctl,
                &mut self.history,
                &mut self.settle,
                Duration::from_secs(60),
            )
            .await;
        }

        async fn close_to_helper(&self) {
            let mut state = self.state.lock().await;
            for address in ["0x2", "0x3", "0x4", "0x5", "0x6"] {
                state.remove(address);
            }
        }

        async fn reflow_helper(&self) {
            let mut helper = tracked("0x1", "2", true);
            helper.position = (400, 300);
            helper.size = (1600, 900);
            self.state.lock().await.add(helper);
        }
    }

    impl Drop for SaveHarness {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    #[tokio::test]
    async fn shutdown_preserves_saved_capture_after_vanish_wait_and_reflow() {
        let mut h = SaveHarness::new();
        assert!(h.save(SaveReason::Explicit).await);
        let path = h.snapshot.session_dir().join("last.toml");
        let original = std::fs::read(&path).unwrap();
        h.close_to_helper().await;
        h.wake().await;
        assert!(h.settle.deadline.is_some());
        assert_eq!(h.history.observed.as_ref().unwrap().len(), 1);
        h.reflow_helper().await;

        assert!(!h.save(SaveReason::Shutdown).await);
        assert_eq!(
            h.snapshot.load("last").unwrap().windows.len(),
            6,
            "shutdown replaced the complete capture with the leftover helper"
        );
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[tokio::test]
    async fn explicit_save_accepts_a_reduced_capture() {
        let mut h = SaveHarness::new();
        assert!(h.save(SaveReason::Explicit).await);
        h.close_to_helper().await;
        assert!(h.save(SaveReason::Explicit).await);
        assert_eq!(h.snapshot.load("last").unwrap().windows.len(), 1);
        assert!(h.save(SaveReason::Shutdown).await);
    }

    #[tokio::test]
    async fn periodic_reflow_waits_then_accepts_a_settled_reduction() {
        let mut h = SaveHarness::new();
        assert!(h.save(SaveReason::Explicit).await);
        h.close_to_helper().await;
        h.reflow_helper().await;
        h.wake().await;
        assert!(h.settle.deadline.is_some());
        assert_eq!(h.snapshot.load("last").unwrap().windows.len(), 6);
        assert!(!h.save(SaveReason::Shutdown).await);
        h.wake().await;
        assert_eq!(h.snapshot.load("last").unwrap().windows.len(), 6);
        h.settle.deadline = Some(std::time::Instant::now());
        h.wake().await;
        assert_eq!(h.snapshot.load("last").unwrap().windows.len(), 1);
        assert!(h.save(SaveReason::Shutdown).await);
    }

    #[tokio::test]
    async fn failed_save_does_not_advance_shutdown_baseline() {
        let mut h = SaveHarness::new();
        assert!(h.save(SaveReason::Explicit).await);
        h.close_to_helper().await;
        let path = h.snapshot.session_dir().join("last.toml");
        let backup = h.snapshot.session_dir().join("backup");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(!h.save(SaveReason::Explicit).await);
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(backup, path).unwrap();

        assert!(!h.save(SaveReason::Shutdown).await);
        assert_eq!(h.snapshot.load("last").unwrap().windows.len(), 6);
    }

    #[tokio::test]
    async fn shutdown_can_write_the_first_capture() {
        let mut h = SaveHarness::new();
        h.close_to_helper().await;
        assert!(h.save(SaveReason::Shutdown).await);
        assert_eq!(h.snapshot.load("last").unwrap().windows.len(), 1);
    }

    #[tokio::test]
    async fn untracked_move_and_float_are_not_material() {
        let (state, resolver, ctl) = harness();
        assert!(
            !handle_event(
                HyprEvent::MoveWindow {
                    address: "0xabc".to_string(),
                    workspace: "2".to_string(),
                },
                &state,
                &resolver,
                &ctl,
            )
            .await
        );
        assert!(
            !handle_event(
                HyprEvent::ChangeFloatingMode {
                    address: "0xabc".to_string(),
                    floating: true,
                },
                &state,
                &resolver,
                &ctl,
            )
            .await
        );
    }

    #[tokio::test]
    async fn unchanged_tracked_move_and_float_are_not_material() {
        let (state, resolver, ctl) = harness();
        state.lock().await.add(tracked("0xabc", "1", false));

        assert!(
            !handle_event(
                HyprEvent::MoveWindow {
                    address: "abc".to_string(),
                    workspace: "1".to_string(),
                },
                &state,
                &resolver,
                &ctl,
            )
            .await
        );
        assert!(
            !handle_event(
                HyprEvent::ChangeFloatingMode {
                    address: "0xabc".to_string(),
                    floating: false,
                },
                &state,
                &resolver,
                &ctl,
            )
            .await
        );

        assert!(
            handle_event(
                HyprEvent::MoveWindow {
                    address: "0xabc".to_string(),
                    workspace: "2".to_string(),
                },
                &state,
                &resolver,
                &ctl,
            )
            .await
        );
        assert!(
            handle_event(
                HyprEvent::ChangeFloatingMode {
                    address: "0xabc".to_string(),
                    floating: true,
                },
                &state,
                &resolver,
                &ctl,
            )
            .await
        );
        assert_eq!(state.lock().await.windows()[0].workspace, "2");
        assert!(state.lock().await.windows()[0].floating);
    }
}
