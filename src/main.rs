mod caddy;
mod config;
mod dirs;
mod logs;
mod process;
mod state;

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
    let config = config::Config::load()?;
    let mut state = state::AppState::load()?;

    // Clean up any stale entries
    let dead = state.prune_dead();
    if !dead.is_empty() {
        state.save()?;
    }

    if let Err(e) = logs::cleanup_outdated_logs(&state) {
        eprintln!("warning: log cleanup failed: {e:#}");
    }

    if let Err(e) = caddy::reconcile_caddy(&state) {
        eprintln!("warning: caddy reload failed: {e:#}");
    }

    match cli.command {
        Commands::List => cmd_list(&config),
        Commands::Start { name } => cmd_start(&name, &config, &mut state),
        Commands::Stop { name } => cmd_stop(&name, &mut state),
        Commands::Status => cmd_status(&config, &mut state),
    }
}

fn cmd_list(config: &config::Config) -> Result<()> {
    if config.projects.is_empty() {
        println!("No projects configured.");
        println!("Add projects to ~/.devhub/proj.json");
        return Ok(());
    }

    println!(
        "{:<20} {:<40} {:<10} {}",
        "PROJECT", "PATH", "PORT", "COMMAND"
    );
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
    println!("  log:  {}", logs::project_log_path(name)?.display());
    println!("  readiness:  {}", process::describe_readiness(proj));
    let pid = process::start_project(name, proj, state)?;

    if let Some(port) = proj.port {
        println!("  pid:  {pid}");
        println!("  url:  http://{name}.localhost:1300 (→ localhost:{port})");

        // Attempt Caddy reload
        if let Err(e) = caddy::reload_caddy(state) {
            eprintln!("  warning: caddy reload failed: {e:#}");
        }
    } else {
        println!("  pid:  {pid}");
    }

    println!("Started.");
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

fn cmd_status(config: &config::Config, state: &mut state::AppState) -> Result<()> {
    if config.projects.is_empty() {
        println!("No projects configured.");
        return Ok(());
    }

    println!("{:<20} {:<12} {:<8} {}", "PROJECT", "STATUS", "PID", "URL");
    for (name, _proj) in &config.projects {
        if state.is_running(name) {
            let ps = &state.processes[name];
            let status = "running";
            let url = ps
                .port
                .map(|p| format!("http://{}.localhost:1300 (→ :{})", name, p))
                .unwrap_or("-".to_string());
            println!("{:<20} {:<12} {:<8} {}", name, status, ps.pid, url);
        } else {
            println!("{:<20} {:<12} {:<8} {}", name, "stopped", "-", "-");
        }
    }

    Ok(())
}
