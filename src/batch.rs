//! Starting and stopping several projects at once. Each project runs on its
//! own thread and is independent of the others: one failing never stops,
//! delays or rolls back another, and the command fails at the end if any did.

use std::sync::{Mutex, PoisonError};
use std::thread;

use eyre::{Result, eyre};

use crate::config::Config;
use crate::state::AppState;
use crate::{caddy, dirs, logs, process};

/// What a batch did with one project.
enum Outcome {
    Done(String),
    Skipped(String),
    Failed(eyre::Report),
}

pub fn start(names: Vec<String>, config: &Config, state: &AppState) -> Result<()> {
    if names.is_empty() {
        println!("No projects configured.");
        println!("Add projects to ~/.devhub/proj.json");
        return Ok(());
    }

    let progress = Progress::new(&names);
    let mut settled = Vec::new();
    let mut launch = Vec::new();
    for name in names {
        if !config.projects.contains_key(&name) {
            let err = eyre!("project '{name}' not found in config");
            settled.push((name, Outcome::Failed(err)));
        } else if let Some(ps) = state.live(&name) {
            let phase = if ps.ready { "running" } else { "starting" };
            let reason = format!("already {phase} (pid {})", ps.pid);
            settled.push((name, Outcome::Skipped(reason)));
        } else {
            launch.push(name);
        }
    }

    if !launch.is_empty() {
        println!("Starting {}...", quoted_list(&launch));
        println!(
            "  logs:  {}",
            dirs::logs_dir()?.join("<name>.log").display()
        );
    }
    for (name, outcome) in &settled {
        progress.report(name, outcome);
    }

    // Held across the whole batch so a Ctrl-C reaches every start still
    // waiting for readiness and devhub survives to report the outcome.
    let _interrupts = process::InterruptGuard::install();
    let routes = Routes::default();
    settled.extend(run_parallel(&launch, &progress, |name| {
        if process::interrupt_received() {
            return Outcome::Failed(eyre!("startup interrupted"));
        }
        let proj = &config.projects[name];
        let mut state = state.clone();
        match process::start_project(name, proj, &mut state) {
            Ok(pid) => match proj.port {
                Some(port) => {
                    routes.sync();
                    Outcome::Done(format!(
                        "started (pid {pid}), {} (→ localhost:{port})",
                        caddy::project_url(name)
                    ))
                }
                None => Outcome::Done(format!("started (pid {pid})")),
            },
            // Flattened so a MaybeRunning failure keeps its pid in the text.
            Err(failure) => Outcome::Failed(eyre!("{failure:#}")),
        }
    }));

    finish(settled, Verb::Start)
}

pub fn stop(names: Vec<String>, state: &AppState) -> Result<()> {
    if names.is_empty() {
        println!("No projects running.");
        return Ok(());
    }

    let progress = Progress::new(&names);
    let mut settled = Vec::new();
    let mut launch = Vec::new();
    for name in names {
        if state.live(&name).is_some() {
            launch.push(name);
        } else {
            settled.push((name, Outcome::Skipped("not running".to_string())));
        }
    }

    if !launch.is_empty() {
        println!("Stopping {}...", quoted_list(&launch));
    }
    for (name, outcome) in &settled {
        progress.report(name, outcome);
    }

    settled.extend(run_parallel(&launch, &progress, |name| {
        let mut state = state.clone();
        match process::stop_project_if_running(name, &mut state) {
            Ok(process::StopOutcome::Killed { pid }) => {
                if let Err(e) = logs::remove_project_log(name) {
                    eprintln!("  warning: log cleanup for '{name}' failed: {e:#}");
                }
                Outcome::Done(format!("stopped (pid {pid})"))
            }
            // It died on its own since the check above; like a single stop,
            // leave its log, the only record of why.
            Ok(process::StopOutcome::AlreadyDead) => Outcome::Skipped("not running".to_string()),
            Err(err) => Outcome::Failed(err),
        }
    }));

    // Stops are quick, so one route update after all of them is enough.
    Routes::default().sync();

    finish(settled, Verb::Stop)
}

/// Run `work` for every name on its own thread, reporting each outcome as
/// it arrives.
fn run_parallel(
    names: &[String],
    progress: &Progress,
    work: impl Fn(&str) -> Outcome + Sync,
) -> Vec<(String, Outcome)> {
    let work = &work;
    thread::scope(|scope| {
        let workers: Vec<_> = names
            .iter()
            .map(|name| {
                let worker = thread::Builder::new().spawn_scoped(scope, move || {
                    let outcome = work(name);
                    progress.report(name, &outcome);
                    outcome
                });
                (name, worker)
            })
            .collect();

        workers
            .into_iter()
            .map(|(name, worker)| {
                let outcome = match worker {
                    Ok(handle) => handle.join().unwrap_or_else(|_| {
                        let outcome = Outcome::Failed(eyre!("internal error (thread panicked)"));
                        progress.report(name, &outcome);
                        outcome
                    }),
                    Err(err) => {
                        let outcome = Outcome::Failed(eyre!("failed to spawn a thread: {err}"));
                        progress.report(name, &outcome);
                        outcome
                    }
                };
                (name.clone(), outcome)
            })
            .collect()
    })
}

/// Applies proxy routes during a batch one update at a time, each from a
/// fresh read of the state: per-thread snapshots would let an update
/// carrying fewer routes land after one carrying more. Serialized, the last
/// update includes every project ready by then. A failing caddy is reported
/// once rather than once per project.
#[derive(Default)]
struct Routes {
    warned: Mutex<bool>,
}

impl Routes {
    fn sync(&self) {
        let mut warned = self.warned.lock().unwrap_or_else(PoisonError::into_inner);
        let result = AppState::load().and_then(|state| caddy::reconcile_caddy(&state));
        if let Err(e) = result {
            if !*warned {
                eprintln!("  warning: caddy reload failed: {e:#}");
                *warned = true;
            }
        }
    }
}

/// One aligned line per project, printed as soon as its outcome is known.
struct Progress {
    width: usize,
}

impl Progress {
    fn new(names: &[String]) -> Self {
        let width = names.iter().map(|name| name.len() + 1).max().unwrap_or(0);
        Self { width }
    }

    fn report(&self, name: &str, outcome: &Outcome) {
        let text = match outcome {
            Outcome::Done(text) => text.clone(),
            Outcome::Skipped(reason) => format!("skipped, {reason}"),
            Outcome::Failed(err) => {
                let message = format!("{err:#}");
                format!("FAILED, {}", message.lines().next().unwrap_or_default())
            }
        };
        // One println per line: stdout's lock keeps parallel reports whole.
        println!(
            "  {:<width$}  {text}",
            format!("{name}:"),
            width = self.width
        );
    }
}

#[derive(Clone, Copy)]
enum Verb {
    Start,
    Stop,
}

impl Verb {
    fn base(self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
        }
    }

    fn past(self) -> &'static str {
        match self {
            Verb::Start => "Started",
            Verb::Stop => "Stopped",
        }
    }
}

/// Summarize the batch; failures are detailed last, so log tails never
/// interleave with the progress of projects still running.
fn finish(mut settled: Vec<(String, Outcome)>, verb: Verb) -> Result<()> {
    settled.sort_by(|a, b| a.0.cmp(&b.0));
    let total = settled.len();
    let (mut done, mut skipped) = (0, 0);
    let mut failed = Vec::new();
    for (name, outcome) in settled {
        match outcome {
            Outcome::Done(_) => done += 1,
            Outcome::Skipped(_) => skipped += 1,
            Outcome::Failed(err) => failed.push((name, err)),
        }
    }

    if failed.is_empty() {
        if done == 0 {
            println!("Nothing to {}.", verb.base());
        } else if skipped == 0 {
            println!("{} {}.", verb.past(), projects(done));
        } else {
            println!("{} {}, skipped {skipped}.", verb.past(), projects(done));
        }
        return Ok(());
    }

    for (name, err) in &failed {
        eprintln!();
        eprintln!("'{name}' failed to {}: {err:#}", verb.base());
    }
    eprintln!();

    let names: Vec<&str> = failed.iter().map(|(name, _)| name.as_str()).collect();
    Err(eyre!(
        "{} of {} failed to {}: {}",
        failed.len(),
        projects(total),
        verb.base(),
        names.join(", ")
    ))
}

fn projects(count: usize) -> String {
    match count {
        1 => "1 project".to_string(),
        n => format!("{n} projects"),
    }
}

fn quoted_list(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|name| format!("'{name}'")).collect();
    quoted.join(", ")
}
