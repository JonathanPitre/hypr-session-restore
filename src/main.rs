mod config;
mod core;
mod ipc;
mod models;
mod resolver;

#[cfg(test)]
mod tests;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "hypr-persist",
    version,
    about = "Session persistence daemon for Hyprland"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to config file
    #[arg(short, long)]
    config: Option<String>,

    /// Log verbosity: -v (info), -vv (debug), -vvv (trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Suppress all output
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Save current session to disk
    Save {
        /// Session name (default: "last")
        name: Option<String>,
    },
    /// Restore a saved session
    Restore {
        /// Session name (default: "last")
        name: Option<String>,
    },
    /// List saved sessions
    List,
    /// Delete a saved session
    Delete {
        /// Session name to delete
        name: String,
    },
    /// Show what command a window class resolves to
    Resolve {
        /// Window class to resolve
        class: String,
    },
    /// Show current daemon status
    Status,
    /// Pin a session, or print the current pin
    Pin {
        /// Session name to pin. Omit to print the current pin.
        name: Option<String>,
        /// Clear the current pin
        #[arg(long)]
        clear: bool,
    },
}

fn init_logging(verbose: u8, quiet: bool) {
    let level = if quiet {
        "error"
    } else {
        match verbose {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };

    let filter = format!("hypr_persist={level}");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(false)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose, cli.quiet);

    let cfg = config::Config::load(cli.config.as_deref())?;

    match cli.command {
        None => {
            tracing::info!(
                "starting hypr-persist daemon v{}",
                env!("CARGO_PKG_VERSION")
            );
            core::daemon::run(cfg).await?;
        }

        Some(Command::Save { name }) => {
            cmd_save(&cfg, name.as_deref()).await?;
        }

        Some(Command::Restore { name }) => {
            cmd_restore(&cfg, name.as_deref()).await?;
        }

        Some(Command::List) => {
            cmd_list(&cfg)?;
        }

        Some(Command::Delete { name }) => {
            let snapshot = core::snapshot::SnapshotEngine::new(&cfg)?;
            snapshot.delete(&name)?;
            println!("Deleted session '{name}'.");
        }

        Some(Command::Resolve { class }) => {
            cmd_resolve(&cfg, &class).await?;
        }

        Some(Command::Status) => {
            cmd_status(&cfg).await?;
        }

        Some(Command::Pin { name, clear }) => {
            cmd_pin(&cfg, name.as_deref(), clear)?;
        }
    }

    Ok(())
}

async fn cmd_save(cfg: &config::Config, name: Option<&str>) -> Result<()> {
    let snapshot = core::snapshot::SnapshotEngine::new(cfg)?;
    let name = snapshot.save_target(name)?;
    let ctl = ipc::client::HyprCtl::from_env()?;
    let resolver = resolver::AppResolver::new(cfg);
    let mut state = core::state::StateManager::new(cfg);

    let clients = ctl.get_clients().await?;
    let monitor_map = ctl.get_monitor_map().await.unwrap_or_default();
    for c in clients {
        if !state.should_track(&c.class) {
            continue;
        }
        let launch_cmd = resolver.resolve(&c.class, c.pid).unwrap_or_default();
        let profile = resolver::profile::detect_browser_profile(c.pid);
        let monitor = monitor_map.get(&c.monitor).cloned().unwrap_or_default();
        state.add(models::TrackedWindow {
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

    let path = snapshot.save(&state, &name)?;
    println!("Session '{name}' saved to {}", path.display());
    Ok(())
}

async fn cmd_restore(cfg: &config::Config, name: Option<&str>) -> Result<()> {
    let snapshot = core::snapshot::SnapshotEngine::new(cfg)?;
    let name = snapshot.resolve_restore_target(name)?;
    let ctl = ipc::client::HyprCtl::from_env()?;
    let session = snapshot.load(&name)?;
    let engine =
        core::restore::RestoreEngine::new(cfg.general.restore_geometry, cfg.general.restore_layout);
    // No live-window inventory here: this is a one-shot manual restore with
    // no daemon-tracked state, unlike the daemon's restore-on-start path.
    let (report, watcher) = engine.restore(&session, &ctl, Vec::new()).await?;

    println!(
        "Restored {}/{} apps{}",
        report.restored,
        report.restored + report.failed,
        if report.failed > 0 {
            format!(" ({} failed)", report.failed)
        } else {
            String::new()
        }
    );
    for (app, err) in &report.errors {
        eprintln!("  {app}: {err}");
    }

    if let Some(handle) = watcher {
        println!("Waiting for slow-starting apps (up to 60s)...");
        drop(handle.await);
    }
    Ok(())
}

fn cmd_list(cfg: &config::Config) -> Result<()> {
    let snapshot = core::snapshot::SnapshotEngine::new(cfg)?;
    let sessions = snapshot.list()?;

    if sessions.is_empty() {
        println!("No saved sessions.");
    } else {
        println!("{:<20} {:<24} ", "NAME", "SAVED AT");
        for (name, ts) in sessions {
            let dt = chrono::DateTime::from_timestamp(ts, 0).map_or_else(
                || "unknown".to_string(),
                |d| d.format("%Y-%m-%d %H:%M:%S").to_string(),
            );
            println!("{name:<20} {dt:<24}");
        }
    }
    Ok(())
}

fn cmd_pin(cfg: &config::Config, name: Option<&str>, clear: bool) -> Result<()> {
    if clear && name.is_some() {
        bail!("pass either a session name or --clear");
    }
    let snapshot = core::snapshot::SnapshotEngine::new(cfg)?;
    if clear {
        snapshot.clear_pin()?;
        println!("Pin cleared.");
        return Ok(());
    }
    if let Some(name) = name {
        snapshot.pin(name)?;
        println!("Pinned session '{name}'.");
        return Ok(());
    }
    match snapshot.pinned_name()? {
        Some(name) => println!("{name}"),
        None => println!("No pinned session."),
    }
    Ok(())
}

async fn cmd_resolve(cfg: &config::Config, class: &str) -> Result<()> {
    let resolver = resolver::AppResolver::new(cfg);

    let pid = match ipc::client::HyprCtl::from_env() {
        Ok(ctl) => ctl.get_clients().await.map_or(-1, |clients| {
            clients
                .iter()
                .find(|c| c.class == class)
                .map_or(-1, |c| c.pid)
        }),
        Err(_) => -1,
    };

    if let Some(cmd) = resolver.resolve(class, pid) {
        println!("{class} → {cmd}");
    } else {
        eprintln!("Could not resolve '{class}' to a launch command");
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_status(cfg: &config::Config) -> Result<()> {
    let ctl = ipc::client::HyprCtl::from_env()?;
    match ctl.get_clients().await {
        Ok(clients) => {
            println!("Hyprland is running ({} windows)", clients.len());
        }
        Err(e) => {
            eprintln!("Cannot connect to Hyprland: {e}");
            std::process::exit(1);
        }
    }

    let snapshot = core::snapshot::SnapshotEngine::new(cfg)?;
    if snapshot.exists("last") {
        match snapshot.load("last") {
            Ok(session) => {
                let dt = chrono::DateTime::from_timestamp(session.session.timestamp, 0)
                    .map_or_else(
                        || "unknown".to_string(),
                        |d| d.format("%Y-%m-%d %H:%M:%S").to_string(),
                    );
                println!(
                    "Last session: {} apps, saved at {dt}",
                    session.windows.len()
                );
            }
            Err(e) => println!("Last session: error reading ({e})"),
        }
    } else {
        println!("No saved session.");
    }

    println!("Session dir: {}", snapshot.session_dir().display());
    println!("Config: {}", config::Config::default_path().display());
    Ok(())
}
