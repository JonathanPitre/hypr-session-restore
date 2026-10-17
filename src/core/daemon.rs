use anyhow::{Context, Result};
use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1};
use signal_hook_tokio::Signals;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

use crate::config::Config;
use crate::core::restore::{RestoreEngine, validate_session};
use crate::core::scheduler::{
    Delta, SaveReason, SettleState, Tick, WindowStamp, classify, on_tick,
};
use crate::core::snapshot::SnapshotEngine;
use crate::core::stamp::{AutoRestore, AutoRestoreInput, decide_auto_restore};
use crate::core::state::StateManager;
use crate::ipc::client::HyprCtl;
use crate::ipc::event_listener;
use crate::models::HyprEvent;
use crate::models::TrackedWindow;
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

    maybe_auto_restore(&config, &snapshot, &state, &ctl, &mut event_rx).await?;

    let mut signals =
        Signals::new([SIGINT, SIGTERM, SIGUSR1]).context("registering signal handlers")?;

    let save_interval = Duration::from_secs(config.general.save_interval);
    let mut settle = SettleState {
        delay: Duration::from_secs(config.general.settle_delay),
        deadline: None,
    };
    let mut prev_stamps: Option<Vec<WindowStamp>> = None;
    let mut next_wake = std::time::Instant::now() + save_interval;

    tracing::info!(
        "daemon running, tracking {} windows, saving every {}s",
        state.lock().await.window_count(),
        config.general.save_interval
    );

    loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                let changed = handle_event(event, &state, &resolver, &ctl).await;
                if changed && settle.deadline.is_some() {
                    let count = state.lock().await.window_count();
                    let tick = on_tick(
                        Delta::Material,
                        SaveReason::Periodic,
                        &mut settle,
                        std::time::Instant::now(),
                        count,
                    );
                    if tick == Tick::Save {
                        save_with_refresh(&state, &snapshot, &ctl, "event").await;
                        prev_stamps = Some(current_stamps(&state).await);
                        next_wake = std::time::Instant::now() + save_interval;
                    }
                }
            }

            () = tokio::time::sleep_until(tokio::time::Instant::from_std(next_wake)) => {
                next_wake = periodic_wake(
                    &state,
                    &snapshot,
                    &ctl,
                    &mut prev_stamps,
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
                if save_for_signal(&state, &snapshot, &ctl, &mut prev_stamps, &mut settle).await {
                    next_wake = std::time::Instant::now() + save_interval;
                }
                if exiting {
                    break;
                }
            }

            else => {
                tracing::info!("event stream ended, saving final session...");
                let saved =
                    save_for_signal(&state, &snapshot, &ctl, &mut prev_stamps, &mut settle).await;
                tracing::debug!("final save wrote={saved}");
                break;
            }
        }
    }

    event_handle.abort();
    tracing::info!("daemon stopped");
    Ok(())
}

async fn maybe_auto_restore(
    config: &Config,
    snapshot: &SnapshotEngine,
    state: &Arc<Mutex<StateManager>>,
    ctl: &HyprCtl,
    event_rx: &mut mpsc::Receiver<HyprEvent>,
) -> Result<()> {
    let preexisting = if config.general.restore_on_start {
        ctl.get_clients()
            .await
            .map(|clients| clients.len())
            .unwrap_or(0)
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
    ctl: &HyprCtl,
    prev_stamps: &mut Option<Vec<WindowStamp>>,
    settle: &mut SettleState,
    period: Duration,
) -> std::time::Instant {
    let stamps = current_stamps(state).await;
    let delta = classify(prev_stamps.as_deref(), &stamps);
    let tick = on_tick(
        delta,
        SaveReason::Periodic,
        settle,
        std::time::Instant::now(),
        stamps.len(),
    );
    match tick {
        Tick::Wait(deadline) => {
            *prev_stamps = Some(stamps);
            deadline
        }
        Tick::Save => {
            save_with_refresh(state, snapshot, ctl, "periodic").await;
            *prev_stamps = Some(current_stamps(state).await);
            std::time::Instant::now() + period
        }
        Tick::Idle => {
            *prev_stamps = Some(stamps);
            std::time::Instant::now() + period
        }
    }
}

async fn save_for_signal(
    state: &Arc<Mutex<StateManager>>,
    snapshot: &SnapshotEngine,
    ctl: &HyprCtl,
    prev_stamps: &mut Option<Vec<WindowStamp>>,
    settle: &mut SettleState,
) -> bool {
    let count = state.lock().await.window_count();
    let tick = on_tick(
        Delta::Unchanged,
        SaveReason::Signal,
        settle,
        std::time::Instant::now(),
        count,
    );
    if tick != Tick::Save {
        return false;
    }
    save_with_refresh(state, snapshot, ctl, "signal").await;
    *prev_stamps = Some(current_stamps(state).await);
    true
}

async fn current_stamps(state: &Arc<Mutex<StateManager>>) -> Vec<WindowStamp> {
    state
        .lock()
        .await
        .windows()
        .into_iter()
        .map(stamp_window)
        .collect()
}

fn stamp_window(window: &TrackedWindow) -> WindowStamp {
    WindowStamp {
        address: window.address.clone(),
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
    ctl: &HyprCtl,
    label: &str,
) {
    let mut state = state.lock().await;
    if state.window_count() == 0 {
        return;
    }
    let monitor_map = ctl.get_monitor_map().await.unwrap_or_default();
    match ctl.get_clients().await {
        Ok(clients) => state.refresh_geometry(&clients, &monitor_map),
        Err(e) => tracing::warn!("{label} save: failed to refresh geometry: {e}"),
    }
    let target = match snapshot.save_target(None) {
        Ok(name) => name,
        Err(e) => {
            tracing::error!("{label} save failed: {e}");
            return;
        }
    };
    let save_result = snapshot.save(&state, &target);
    drop(state);
    if let Err(e) = save_result {
        tracing::error!("{label} save failed: {e}");
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
    let monitor_map = ctl.get_monitor_map().await.unwrap_or_default();
    let mut state = state.lock().await;

    for c in clients {
        if !state.should_track(&c.class) {
            continue;
        }

        let launch_cmd = resolver.resolve(&c.class, c.pid).unwrap_or_default();
        let profile = crate::resolver::profile::detect_browser_profile(c.pid);
        let monitor = monitor_map.get(&c.monitor).cloned().unwrap_or_default();

        state.add(crate::models::TrackedWindow {
            address: c.address,
            app_id: c.class,
            launch_cmd,
            workspace: c.workspace.name,
            monitor,
            position: c.at,
            size: c.size,
            floating: c.floating,
            fullscreen: c.fullscreen_mode > 0,
            pid: c.pid,
            profile,
        });
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

            let (pid, position, size, floating, fullscreen) =
                match ctl.get_client_by_address(&address).await {
                    Ok(Some(c)) => (c.pid, c.at, c.size, c.floating, c.fullscreen_mode > 0),
                    _ => (-1, (0, 0), (0, 0), false, false),
                };

            let launch_cmd = resolver.resolve(&class, pid).unwrap_or_default();
            let profile = crate::resolver::profile::detect_browser_profile(pid);

            let mut state = state.lock().await;
            state.add(crate::models::TrackedWindow {
                address,
                app_id: class,
                launch_cmd,
                workspace,
                monitor: String::new(),
                position,
                size,
                floating,
                fullscreen,
                pid,
                profile,
            });
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
    use super::handle_event;
    use crate::config::Config;
    use crate::core::state::StateManager;
    use crate::ipc::client::{HyprCtl, HyprSocketPaths};
    use crate::models::{HyprEvent, TrackedWindow};
    use crate::resolver::AppResolver;
    use std::path::PathBuf;
    use std::sync::Arc;
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
