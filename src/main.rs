mod caddy;
mod config;
mod dirs;
mod logs;
mod process;
mod state;

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use eyre::Result;

#[derive(Parser)]
#[command(name = "devhub", version, about = "Manage local development projects")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List all configured projects
    List,
    /// Start a project
    Start {
        /// Project name
        name: String,
    },
    /// Stop a running project
    Stop {
        /// Project name
        name: String,
    },
    /// Restart a project (stop it first if running, then start it)
    Restart {
        /// Project name
        name: String,
    },
    /// Show status of all projects
    Status,
}

fn main() -> ExitCode {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let mut state = state::AppState::load()?;

    // Housekeeping never reads the project config, so a broken proj.json
    // cannot block cleaning up after dead projects or stopping live ones.
    if !state.prune_dead().is_empty() {
        // Prune again under the state lock, against the current file, so
        // entries other devhub commands wrote since our load survive.
        state.update(|s| {
            s.prune_dead();
        })?;
    }

    if let Err(e) = logs::cleanup_outdated_logs(&state) {
        eprintln!("warning: log cleanup failed: {e:#}");
    }

    if let Err(e) = caddy::reconcile_caddy(&state) {
        // Without caddy there is nothing to reconcile; `start` already warned.
        if !caddy::is_not_installed(&e) {
            eprintln!("warning: caddy reload failed: {e:#}");
        }
    }

    match cli.command {
        Commands::List => cmd_list(&config::Config::load()?),
        Commands::Start { name } => cmd_start(&name, &config::Config::load()?, &mut state),
        Commands::Stop { name } => cmd_stop(&name, &mut state),
        Commands::Restart { name } => cmd_restart(&name, &config::Config::load()?, &mut state),
        Commands::Status => cmd_status(&config::Config::load()?, &state),
    }
}

fn cmd_list(config: &config::Config) -> Result<()> {
    if config.projects.is_empty() {
        println!("No projects configured.");
        println!("Add projects to ~/.devhub/proj.json");
        return Ok(());
    }

    println!("{:<20} {:<40} {:<10} COMMAND", "PROJECT", "PATH", "PORT");
    for (name, proj) in &config.projects {
        let port = proj.port.map(|p| p.to_string()).unwrap_or("-".to_string());
        println!("{:<20} {:<40} {:<10} {}", name, proj.path, port, proj.cmd);
    }

    Ok(())
}

fn cmd_start(name: &str, config: &config::Config, state: &mut state::AppState) -> Result<()> {
    let proj = config
        .projects
        .get(name)
        .ok_or_else(|| eyre::eyre!("project '{}' not found in config", name))?;

    println!("Starting '{name}'...");
    start_and_report(name, proj, state)?;

    if proj.port.is_some() {
        // Attempt Caddy reload
        if let Err(e) = caddy::reload_caddy(state) {
            eprintln!("  warning: caddy reload failed: {e:#}");
        }
    }

    println!("Started.");
    Ok(())
}

/// Announce and run a start; the output and readiness handling shared by
/// `start` and `restart`. Caddy handling is left to the caller because the
/// two commands reconcile routes at different points.
fn start_and_report(
    name: &str,
    proj: &config::ProjectConfig,
    state: &mut state::AppState,
) -> Result<u32, process::StartFailure> {
    println!(
        "  log:  {}",
        logs::project_log_path(name)
            .map_err(|err| process::StartFailure::NothingRunning(eyre::eyre!("{err:#}")))?
            .display()
    );
    println!("  readiness:  {}", process::describe_readiness(proj));
    let pid = process::start_project(name, proj, state)?;

    println!("  pid:  {pid}");
    if let Some(port) = proj.port {
        println!("  url:  {} (→ localhost:{port})", caddy::project_url(name));
    }

    Ok(pid)
}

fn cmd_restart(name: &str, config: &config::Config, state: &mut state::AppState) -> Result<()> {
    // Preflight before stopping anything: a typo or a moved project
    // directory must not take down a healthy running process.
    let proj = config
        .projects
        .get(name)
        .ok_or_else(|| eyre::eyre!("project '{}' not found in config", name))?;
    let expanded = config::expand_path(&proj.path);
    if !Path::new(&expanded).exists() {
        eyre::bail!("project directory '{expanded}' does not exist");
    }

    println!("Restarting '{name}'...");
    match process::stop_project_if_running(name, state)? {
        process::StopOutcome::Killed { pid } => {
            println!("  stopped previous run (pid {pid})");
            if let Err(e) = logs::remove_project_log(name) {
                eprintln!("  warning: log cleanup failed: {e:#}");
            }
        }
        process::StopOutcome::AlreadyDead => {
            // The previous run ended without a stop (e.g. it crashed
            // overnight); its log is the only witness, and starting fresh
            // would truncate it silently.
            match logs::read_project_log_tail(name) {
                Ok(Some(tail)) => {
                    println!("  previous run ended on its own; last log lines:");
                    for line in tail.lines() {
                        println!("    {line}");
                    }
                }
                _ => println!("  not running — starting fresh"),
            }
            let _ = logs::remove_project_log(name);
        }
    }

    let started = start_and_report(name, proj, state);

    // Reconcile on both paths: a success that leaves the route content
    // unchanged is a no-op (the URL briefly 502s instead of disappearing),
    // while a failed start must drop the route it no longer serves.
    if let Err(e) = caddy::reconcile_caddy(state) {
        eprintln!("  warning: caddy reload failed: {e:#}");
    }

    // Only claim the project is stopped when the failure proves nothing is
    // left running; an unconfirmed termination must not be papered over.
    match started {
        Ok(_) => {}
        Err(process::StartFailure::NothingRunning(report)) => {
            return Err(eyre::eyre!(
                "restart failed; '{name}' is now stopped: {report:#}"
            ));
        }
        Err(process::StartFailure::MaybeRunning { pid, err }) => {
            return Err(eyre::eyre!(
                "restart failed; '{name}' may still be running as pid {pid}: {err:#}"
            ));
        }
    }

    println!("Restarted.");
    Ok(())
}

fn cmd_stop(name: &str, state: &mut state::AppState) -> Result<()> {
    println!("Stopping '{name}'...");
    let had_port = state.processes.get(name).and_then(|ps| ps.port);

    process::stop_project(name, state)?;
    if let Err(e) = logs::remove_project_log(name) {
        eprintln!("  warning: log cleanup failed: {e:#}");
    }

    if had_port.is_some() {
        if let Err(e) = caddy::reload_caddy(state) {
            eprintln!("  warning: caddy reload failed: {e:#}");
        }
    }

    println!("Stopped.");
    Ok(())
}

fn cmd_status(config: &config::Config, state: &state::AppState) -> Result<()> {
    if config.projects.is_empty() {
        println!("No projects configured.");
        println!("Add projects to ~/.devhub/proj.json");
        return Ok(());
    }

    println!("{:<20} {:<12} {:<8} URL", "PROJECT", "STATUS", "PID");
    for name in config.projects.keys() {
        if let Some(ps) = state.live(name) {
            // A starting project has no proxy route yet.
            let (status, url) = if ps.ready {
                let url = ps
                    .port
                    .map(|p| format!("{} (→ :{})", caddy::project_url(name), p))
                    .unwrap_or("-".to_string());
                ("running", url)
            } else {
                ("starting", "-".to_string())
            };
            println!("{:<20} {:<12} {:<8} {}", name, status, ps.pid, url);
        } else {
            println!("{:<20} {:<12} {:<8} -", name, "stopped", "-");
        }
    }

    Ok(())
}
