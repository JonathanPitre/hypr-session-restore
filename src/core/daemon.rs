use anyhow::{Context, Result};
use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1};
use signal_hook_tokio::Signals;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

use crate::config::Config;
use crate::core::restore::{RestoreEngine, validate_session};
use crate::core::snapshot::SnapshotEngine;
use crate::core::stamp::{AutoRestore, AutoRestoreInput, decide_auto_restore};
use crate::core::state::StateManager;
use crate::ipc::client::HyprCtl;
use crate::ipc::event_listener;
use crate::models::HyprEvent;
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
    let mut save_timer = tokio::time::interval(save_interval);
    save_timer.tick().await;

    tracing::info!(
        "daemon running, tracking {} windows, saving every {}s",
        state.lock().await.window_count(),
        config.general.save_interval
    );

    loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                handle_event(event, &state, &resolver, &ctl).await;
            }

            _ = save_timer.tick() => {
                save_with_refresh(&state, &snapshot, &ctl, "periodic").await;
            }

            Some(sig) = futures::StreamExt::next(&mut signals) => {
                match sig {
                    SIGTERM | SIGINT => {
                        tracing::info!("received signal {sig}, saving and exiting...");
                        save_with_refresh(&state, &snapshot, &ctl, "final").await;
                        break;
                    }
                    SIGUSR1 => {
                        tracing::info!("received SIGUSR1, saving session...");
                        save_with_refresh(&state, &snapshot, &ctl, "manual").await;
                    }
                    _ => {}
                }
            }

            else => {
                tracing::info!("event stream ended, saving final session...");
                save_with_refresh(&state, &snapshot, &ctl, "final").await;
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
    tracing::info!("waiting for compositor to settle...");
    wait_for_settle(event_rx).await;
    if let Err(reason) = plan.burn() {
        tracing::warn!("auto-restore skipped: {reason:?}");
        return Ok(());
    }
    tracing::info!(
        "stamped instance {} at {}",
        plan.instance.as_str(),
        plan.stamp_path().display()
    );
    tracing::info!("restoring session '{}'...", plan.name);
    let live_windows: Vec<crate::models::TrackedWindow> =
        state.lock().await.windows().into_iter().cloned().collect();
    let engine = RestoreEngine::new(
        config.general.restore_geometry,
        config.general.restore_layout,
    );
    let (report, _watcher) = engine.restore(&plan.session, ctl, live_windows).await?;
    if report.failed > 0 {
        for (app, err) in &report.errors {
            tracing::warn!("restore failed for {app}: {err}");
        }
    }
    Ok(())
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
) {
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
                return;
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
        }
        HyprEvent::CloseWindow { address } => {
            let mut state = state.lock().await;
            state.remove(&address);
        }
        HyprEvent::MoveWindow { address, workspace } => {
            let mut state = state.lock().await;
            state.update_workspace(&address, &workspace);
        }
        HyprEvent::ChangeFloatingMode { address, floating } => {
            let mut state = state.lock().await;
            state.update_floating(&address, floating);
        }
        HyprEvent::Fullscreen { state } => {
            tracing::debug!("fullscreen toggled: {state}");
        }
        HyprEvent::Unknown(ref raw) => {
            tracing::trace!("unhandled event: {raw}");
        }
    }
}
