use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eyre::{WrapErr, bail, eyre};

use crate::config::{ProjectConfig, expand_path};
use crate::logs;
use crate::state::{AppState, ProcessState};

const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(250);
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const READY_CMD_TIMEOUT_CAP: Duration = Duration::from_secs(1);
const DEATH_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);
const DEATH_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// `ps -A` is far costlier than the kill(0) probe, so the zombie check
/// behind it runs at a slower cadence during long waits.
const ZOMBIE_CHECK_INTERVAL: Duration = Duration::from_millis(200);

/// What a stop request found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// A live process group was terminated.
    Killed { pid: u32 },
    /// There was nothing alive to stop; any stale state entry was removed.
    AlreadyDead,
}

/// How a failed start left the spawned project.
#[derive(Debug)]
pub enum StartFailure {
    /// Nothing from this start is left running.
    NothingRunning(eyre::Report),
    /// Termination of the spawned group could not be confirmed; it may
    /// still be running, trackable only through the pid carried here.
    MaybeRunning { pid: u32, err: eyre::Report },
}

impl std::fmt::Display for StartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let report = match self {
            StartFailure::NothingRunning(report) => report,
            StartFailure::MaybeRunning { err: report, .. } => report,
        };
        if f.alternate() {
            write!(f, "{report:#}")
        } else {
            write!(f, "{report}")
        }
    }
}

impl std::error::Error for StartFailure {}

pub fn start_project(
    name: &str,
    config: &ProjectConfig,
    state: &mut AppState,
) -> Result<u32, StartFailure> {
    if let Some(ps) = state.live(name) {
        return Err(StartFailure::NothingRunning(already_started(name, ps)));
    }

    let expanded = expand_path(&config.path);
    let project_dir = Path::new(&expanded);
    if !project_dir.exists() {
        return Err(StartFailure::NothingRunning(eyre!(
            "project directory '{}' does not exist",
            expanded
        )));
    }

    // Flattened via {err:#} so the full context chain survives being carried
    // inside StartFailure (its Error impl cannot delegate source()).
    let (_log_path, stdout_log) = logs::create_project_log(name)
        .map_err(|err| StartFailure::NothingRunning(eyre!("{err:#}")))?;
    let stderr_log = stdout_log.try_clone().map_err(|err| {
        StartFailure::NothingRunning(eyre!(
            "failed to clone log handle for project '{name}': {err}"
        ))
    })?;

    // Installed before the spawn so an interrupt at any point after it is
    // turned into a cleanup instead of killing devhub mid-start.
    let _interrupts = InterruptGuard::install();

    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&config.cmd)
        .current_dir(project_dir)
        // A project that reads from or reconfigures an inherited terminal
        // while in a background group is stopped by SIGTTIN/SIGTTOU.
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log));
    // A new session makes the child a process group leader (pgid == pid), so
    // the whole tree can be signaled as one group, and detaches it from the
    // controlling terminal so job control can never stop or hang it up.
    // SAFETY: setsid is async-signal-safe, as pre_exec requires.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = logs::remove_project_log(name);
            return Err(StartFailure::NothingRunning(eyre!(
                "failed to start command '{}' in {}: {err}",
                config.cmd,
                expanded
            )));
        }
    };

    let pid = child.id();
    let grace = Duration::from_millis(config.stop_timeout_ms);

    // Record the group before waiting for readiness: an entry written only
    // afterwards is lost if devhub dies mid-wait, leaving the project
    // running with nothing able to stop it.
    let claimed = state.update(|s| match s.live(name) {
        Some(other) => Err(already_started(name, other)),
        None => {
            s.add(
                name.to_string(),
                ProcessState::starting(pid, config.port, config.stop_timeout_ms),
            );
            Ok(())
        }
    });
    match claimed {
        Ok(Ok(())) => {}
        // A concurrent `devhub start` of the same project claimed it first;
        // its log is the one now in use, so only our group is discarded.
        Ok(Err(conflict)) => {
            return Err(match kill_process_group(pid, grace) {
                Ok(_) => StartFailure::NothingRunning(conflict),
                Err(stop_err) => StartFailure::MaybeRunning {
                    pid,
                    err: eyre!("{conflict}; could not stop the duplicate process: {stop_err:#}"),
                },
            });
        }
        Err(err) => {
            state.remove_if_pid(name, pid);
            return Err(rollback_untracked_start(name, pid, grace, err));
        }
    }

    if let Err(failure) = wait_for_startup(name, config, project_dir, pid, &mut child) {
        if let StartFailure::NothingRunning(_) = failure {
            // Best effort: an entry left behind for a dead group is pruned by
            // the next command anyway.
            let _ = state.update(|s| s.remove_if_pid(name, pid));
        }
        // On MaybeRunning the entry stays, so `devhub stop` can still reach
        // the group.
        return Err(failure);
    }

    if let Err(err) = state.update(|s| s.mark_ready(name, pid)) {
        state.remove_if_pid(name, pid);
        return Err(rollback_untracked_start(name, pid, grace, err));
    }

    Ok(pid)
}

fn already_started(name: &str, ps: &ProcessState) -> eyre::Report {
    let phase = if ps.ready { "running" } else { "starting" };
    eyre!("project '{name}' is already {phase} (pid {})", ps.pid)
}

/// Undo a start whose state could not be persisted: the project is running
/// but nothing could find it later, so it must not be leaked.
fn rollback_untracked_start(
    name: &str,
    pid: u32,
    grace: Duration,
    err: eyre::Report,
) -> StartFailure {
    match kill_process_group(pid, grace) {
        Ok(_) => {
            let _ = logs::remove_project_log(name);
            StartFailure::NothingRunning(eyre!(
                "failed to persist state for '{name}'; \
                 stopped the just-started process (pid {pid}): {err:#}"
            ))
        }
        Err(stop_err) => {
            // The process may still be running; keep the log, the only
            // record of what it was doing, and say so honestly.
            let log_note = match logs::project_log_path(name) {
                Ok(path) => format!("log kept at {}", path.display()),
                Err(_) => "log kept".to_string(),
            };
            StartFailure::MaybeRunning {
                pid,
                err: eyre!(
                    "could not stop the just-started process (pid {pid}): {stop_err:#} \
                     — it may still be running; {log_note}. \
                     Additionally, persisting state failed: {err}"
                ),
            }
        }
    }
}

/// Stop the project if anything is still running. Err means a live process
/// group could not be cleared (no permission, refusal to die, or a state
/// write failure); every timing in which the process dies on its own is Ok.
pub fn stop_project_if_running(name: &str, state: &mut AppState) -> eyre::Result<StopOutcome> {
    let Some(ps) = state.processes.get(name) else {
        return Ok(StopOutcome::AlreadyDead);
    };
    let pid = ps.pid;

    // Never signal a group whose pid now belongs to an unrelated process.
    let outcome = if ps.is_alive() {
        kill_process_group(pid, ps.stop_grace())?
    } else {
        StopOutcome::AlreadyDead
    };

    state.update(|s| s.remove_if_pid(name, pid))?;
    Ok(outcome)
}

pub fn stop_project(name: &str, state: &mut AppState) -> eyre::Result<()> {
    match stop_project_if_running(name, state)? {
        StopOutcome::Killed { .. } => Ok(()),
        StopOutcome::AlreadyDead => bail!("project '{name}' is not running"),
    }
}

/// SIGTERM the group, give it up to `grace` to shut down, and SIGKILL it
/// only if it is still there. Returns as soon as the group is gone, so a
/// generous grace period costs nothing for projects that exit promptly.
fn kill_process_group(pid: u32, grace: Duration) -> eyre::Result<StopOutcome> {
    if !signal_delivered(pid, libc::SIGTERM)? {
        // ESRCH: the group is already gone, which is a completed stop.
        return Ok(StopOutcome::AlreadyDead);
    }

    if wait_until_group_gone(pid, grace) {
        return Ok(StopOutcome::Killed { pid });
    }

    // It may exit between the wait and the SIGKILL; that still counts as
    // stopped by us. Only a group that survives SIGKILL is an error.
    let _ = signal_delivered(pid, libc::SIGKILL);
    if !wait_until_group_gone(pid, DEATH_CONFIRM_TIMEOUT) {
        bail!(
            "process group {pid} did not exit within {DEATH_CONFIRM_TIMEOUT:?} of \
             SIGKILL — it may be stuck in uninterruptible I/O"
        );
    }

    Ok(StopOutcome::Killed { pid })
}

/// Send `sig` to the process group, falling back to the leader alone.
/// Ok(false) means no signalable target exists — both the group and the
/// leader are gone (ESRCH) — which is nothing left to stop, not a failure.
fn signal_delivered(pid: u32, sig: i32) -> eyre::Result<bool> {
    let (group_ret, group_err) = unsafe {
        let ret = libc::kill(-(pid as i32), sig);
        (ret, std::io::Error::last_os_error())
    };
    if group_ret == 0 {
        return Ok(true);
    }

    let leader_ret = unsafe { libc::kill(pid as i32, sig) };
    if leader_ret == 0 {
        return Ok(true);
    }

    // The leader is a group member, so when the group itself is gone (ESRCH)
    // the leader's errno decides. Any other group failure means the group
    // exists but cannot be signaled, and must not be mistaken for absence.
    let leader_err = std::io::Error::last_os_error();
    let decisive = if group_err.raw_os_error() == Some(libc::ESRCH) {
        leader_err
    } else {
        group_err
    };
    match decisive.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        _ => Err(decisive).wrap_err_with(|| format!("failed to signal process {pid}")),
    }
}

/// Wait up to `timeout` for the process group to be gone; true once it is.
/// Returning from a stop before death is confirmed lets a follow-up start's
/// readiness probe attribute the old listener to a foreign process group
/// and kill the new one.
fn wait_until_group_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut next_zombie_check = Instant::now();
    loop {
        std::thread::sleep(DEATH_POLL_INTERVAL);

        if !crate::state::is_process_group_alive(pid) {
            return true;
        }

        // Zombies cannot execute or hold any resource, and only their
        // parent can reap them; if nothing else lives in the group, it is
        // dead enough.
        let now = Instant::now();
        if now >= next_zombie_check {
            if group_has_only_zombies(pid) {
                return true;
            }
            next_zombie_check = now + ZOMBIE_CHECK_INTERVAL;
        }

        if now >= deadline {
            return false;
        }
    }
}

/// Whether every remaining member of the process group is a zombie. Unreaped
/// zombies keep kill(-pgid, 0) succeeding (macOS even answers EPERM) while
/// holding no resources, but live members can remain alongside a zombie
/// leader, so the whole group is inspected. Conservative on any doubt: a
/// failed, empty, or unparseable listing reports `false` so the caller keeps
/// waiting for its timeout instead of vacuously confirming death.
fn group_has_only_zombies(group_id: u32) -> bool {
    let Ok(output) = Command::new("ps")
        .args(["-A", "-o", "pgid=,stat="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    // kill(-pgid, 0) said the group exists, so a listing without a single
    // matching row is not trustworthy either.
    let mut saw_member = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.split_whitespace();
        let (Some(pgid), Some(stat)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !pgid.parse::<u32>().is_ok_and(|pgid| pgid == group_id) {
            continue;
        }
        saw_member = true;
        if !stat.starts_with('Z') {
            return false;
        }
    }
    saw_member
}

pub fn describe_readiness(config: &ProjectConfig) -> String {
    if let Some(cmd) = &config.ready_cmd {
        format!("exec `{cmd}`")
    } else if let Some(port) = config.port {
        let ownership = if config.verify_port_owner {
            "verified"
        } else {
            "not verified"
        };
        format!("tcp localhost:{port} (127.0.0.1 or ::1, listener ownership {ownership})")
    } else {
        "none (spawn only)".to_string()
    }
}

/// Kill a just-started group whose startup is being abandoned, and classify
/// the failure by whether its termination could be confirmed.
fn abort_startup(name: &str, group_id: u32, grace: Duration, message: String) -> StartFailure {
    let base = startup_failure(name, message);
    match kill_process_group(group_id, grace) {
        Ok(_) => StartFailure::NothingRunning(base),
        Err(stop_err) => StartFailure::MaybeRunning {
            pid: group_id,
            err: eyre!(
                "could not confirm termination of the just-started process: {stop_err:#} \
                 — it may still be running. {base:#}"
            ),
        },
    }
}

fn wait_for_startup(
    name: &str,
    config: &ProjectConfig,
    project_dir: &Path,
    group_id: u32,
    child: &mut Child,
) -> Result<(), StartFailure> {
    let grace = Duration::from_millis(config.stop_timeout_ms);
    let interrupted = || abort_startup(name, group_id, grace, "startup interrupted".to_string());

    if config.ready_cmd.is_none() && config.port.is_none() {
        if interrupt_received() {
            return Err(interrupted());
        }
        return Ok(());
    }

    let timeout = Duration::from_millis(config.startup_timeout_ms);
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 24 * 60 * 60));

    loop {
        if interrupt_received() {
            return Err(interrupted());
        }

        // Reaping the leader first matters: an unreaped zombie leader keeps
        // the group looking alive.
        let status = child.try_wait().map_err(|err| {
            abort_startup(
                name,
                group_id,
                grace,
                format!("failed to poll project status: {err}"),
            )
        })?;
        if !crate::state::is_process_group_alive(group_id) {
            return Err(StartFailure::NothingRunning(startup_failure(
                name,
                format!(
                    "project exited before becoming ready{}",
                    format_exit_status(status.as_ref())
                ),
            )));
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(abort_startup(
                name,
                group_id,
                grace,
                format!("startup timed out after {}ms", config.startup_timeout_ms),
            ));
        }

        let remaining = deadline.saturating_duration_since(now);
        let probe_timeout = remaining.min(READY_CMD_TIMEOUT_CAP);
        let probe = readiness_probe_passed(config, project_dir, probe_timeout, group_id).map_err(
            |err| abort_startup(name, group_id, grace, format!("readiness probe failed: {err:#}")),
        )?;
        // Ctrl-C also reaches the probe's own helpers (ready_cmd, lsof), and a
        // killed lsof reads as "listener not attributable"; never let an
        // interrupted round count as ready.
        if interrupt_received() {
            return Err(interrupted());
        }
        match probe {
            ProbeOutcome::Ready => return Ok(()),
            ProbeOutcome::NotYet => {}
            ProbeOutcome::ForeignListener { port, pid, command } => {
                return Err(abort_startup(
                    name,
                    group_id,
                    grace,
                    format!(
                        "port {port} is held by pid {pid} (`{command}`), which is not part of \
                         this project — another project or a manually started server may \
                         already be using it"
                    ),
                ));
            }
        }

        if Instant::now() >= deadline {
            return Err(abort_startup(
                name,
                group_id,
                grace,
                format!("startup timed out after {}ms", config.startup_timeout_ms),
            ));
        }

        std::thread::sleep(READINESS_POLL_INTERVAL);
    }
}

enum ProbeOutcome {
    Ready,
    NotYet,
    ForeignListener {
        port: u16,
        pid: u32,
        command: String,
    },
}

fn readiness_probe_passed(
    config: &ProjectConfig,
    project_dir: &Path,
    probe_timeout: Duration,
    group_id: u32,
) -> eyre::Result<ProbeOutcome> {
    if let Some(cmd) = &config.ready_cmd {
        if !exec_readiness_probe(cmd, project_dir, probe_timeout)? {
            return Ok(ProbeOutcome::NotYet);
        }
        // A ready_cmd cannot tell *who* answered; when a port is also
        // configured, require the listener to be ours before declaring ready.
        return Ok(match config.port {
            Some(port) => port_readiness(port, group_id, config.verify_port_owner),
            None => ProbeOutcome::Ready,
        });
    }

    if let Some(port) = config.port {
        return Ok(port_readiness(port, group_id, config.verify_port_owner));
    }

    Ok(ProbeOutcome::Ready)
}

/// Probe `port` and, once something is listening, verify the listener belongs
/// to the project's process group. Without the ownership check any unrelated
/// listener (a stale server, another project on the same port) would satisfy
/// the probe and silently receive this project's Caddy route. With
/// `verify_owner` off (the port is held by e.g. Docker's forwarder, outside
/// the group), any listener counts.
fn port_readiness(port: u16, group_id: u32, verify_owner: bool) -> ProbeOutcome {
    if !loopback_accepts(port) {
        return ProbeOutcome::NotYet;
    }
    if !verify_owner {
        return ProbeOutcome::Ready;
    }

    let Some(pids) = listening_pids(port) else {
        // lsof could not attribute the listener; fall back to connect-only semantics.
        return ProbeOutcome::Ready;
    };

    let mut foreign: Option<(u32, String)> = None;
    for pid in pids {
        if let Some((pgid, command)) = listener_details(pid) {
            if pgid == group_id {
                return ProbeOutcome::Ready;
            }
            foreign = foreign.or(Some((pid, command)));
        }
    }

    match foreign {
        Some((pid, command)) => ProbeOutcome::ForeignListener { port, pid, command },
        // Connect succeeded but no listener could be identified (it likely
        // exited in between); treat it as ready, like a connect-only probe.
        None => ProbeOutcome::Ready,
    }
}

/// PIDs with a TCP listener on `port`, or `None` when lsof cannot attribute
/// the listener (not installed, or no match found).
pub(crate) fn listening_pids(port: u16) -> Option<Vec<u32>> {
    let output = Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    let pids: Vec<u32> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect();

    if pids.is_empty() {
        None
    } else {
        Some(pids)
    }
}

/// Full command line of `pid`, or `None` if it is already gone.
pub(crate) fn process_command_line(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !command.is_empty()).then_some(command)
}

/// Process group id and command name of `pid`, or `None` if it is already gone.
pub(crate) fn listener_details(pid: u32) -> Option<(u32, String)> {
    let output = Command::new("ps")
        .args(["-o", "pgid=,comm=", "-p", &pid.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let pgid: u32 = parts.next()?.parse().ok()?;
    let command = parts.collect::<Vec<_>>().join(" ");
    Some((pgid, command))
}

fn exec_readiness_probe(cmd: &str, project_dir: &Path, timeout: Duration) -> eyre::Result<bool> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(project_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to run readiness command '{cmd}'"))?;

    match child.wait_timeout(timeout)? {
        Some(status) => Ok(status.success()),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Ok(false)
        }
    }
}

/// Whether anything accepts connections on `port` on either loopback
/// address. Dev servers bound to `localhost` often listen on one family
/// only (Node 17+ resolves localhost to ::1 on macOS), and the proxy's
/// `localhost` upstream reaches either.
pub(crate) fn loopback_accepts(port: u16) -> bool {
    [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)]
        .into_iter()
        .any(|ip| TcpStream::connect_timeout(&SocketAddr::new(ip, port), TCP_CONNECT_TIMEOUT).is_ok())
}

fn startup_failure(name: &str, message: String) -> eyre::Report {
    let tail = logs::read_project_log_tail(name).ok().flatten();
    let _ = logs::remove_project_log(name);

    if let Some(tail) = tail {
        eyre!("{message}\n\nLast log lines:\n{tail}")
    } else {
        eyre!("{message}")
    }
}

fn format_exit_status(status: Option<&ExitStatus>) -> String {
    match status.and_then(ExitStatus::code) {
        Some(code) => format!(" (code {code})"),
        None => String::new(),
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
const INTERRUPT_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

extern "C" fn note_interrupt(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

fn interrupt_received() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// While alive, turns SIGINT/SIGTERM/SIGHUP into a flag that the startup
/// wait polls, so an interrupted start can kill the group it spawned rather
/// than die and leave it running. Signals that were ignored on entry (e.g.
/// under `nohup`) stay ignored; previous handlers return on drop.
struct InterruptGuard {
    previous: Vec<(libc::c_int, libc::sighandler_t)>,
}

impl InterruptGuard {
    fn install() -> Self {
        INTERRUPTED.store(false, Ordering::SeqCst);
        let handler = note_interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t;
        let mut previous = Vec::with_capacity(INTERRUPT_SIGNALS.len());
        for signal in INTERRUPT_SIGNALS {
            let old = unsafe { libc::signal(signal, handler) };
            if old == libc::SIG_IGN {
                unsafe { libc::signal(signal, libc::SIG_IGN) };
            } else {
                previous.push((signal, old));
            }
        }
        Self { previous }
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        for &(signal, handler) in &self.previous {
            unsafe { libc::signal(signal, handler) };
        }
    }
}

trait ChildExt {
    fn wait_timeout(&mut self, timeout: Duration) -> eyre::Result<Option<ExitStatus>>;
}

impl ChildExt for Child {
    fn wait_timeout(&mut self, timeout: Duration) -> eyre::Result<Option<ExitStatus>> {
        let start = Instant::now();
        loop {
            match self.try_wait()? {
                Some(status) => return Ok(Some(status)),
                None if start.elapsed() >= timeout => return Ok(None),
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    fn with_temp_devhub_dir<T>(f: impl FnOnce() -> T) -> T {
        let tempdir = tempfile::tempdir().unwrap();
        crate::dirs::with_test_devhub_dir(tempdir.path().join(".devhub"), f)
    }

    #[test]
    fn start_and_stop_process() {
        with_temp_devhub_dir(|| {
            let mut state = AppState::default();
            let config = ProjectConfig {
                path: "/tmp".to_string(),
                cmd: "sleep 10".to_string(),
                port: None,
                startup_timeout_ms: 60_000,
                ready_cmd: None,
                stop_timeout_ms: 5_000,
                verify_port_owner: true,
            };

            let pid = start_project("test-sleep", &config, &mut state).unwrap();
            assert!(pid > 0);
            assert!(state.is_running("test-sleep"));

            // Stop the project
            stop_project("test-sleep", &mut state).unwrap();
            assert!(!state.is_running("test-sleep"));
            let _ = crate::logs::remove_project_log("test-sleep");
        });
    }

    #[test]
    fn start_nonexistent_dir() {
        let mut state = AppState::default();
        let config = ProjectConfig {
            path: "/nonexistent/path".to_string(),
            cmd: "echo hello".to_string(),
            port: None,
            startup_timeout_ms: 60_000,
            ready_cmd: None,
            stop_timeout_ms: 5_000,
            verify_port_owner: true,
        };

        let result = start_project("bad-path", &config, &mut state);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn start_already_running() {
        with_temp_devhub_dir(|| {
            let mut state = AppState::default();
            let config = ProjectConfig {
                path: "/tmp".to_string(),
                cmd: "sleep 30".to_string(),
                port: None,
                startup_timeout_ms: 60_000,
                ready_cmd: None,
                stop_timeout_ms: 5_000,
                verify_port_owner: true,
            };

            let _pid = start_project("dup", &config, &mut state).unwrap();

            // Try to start again
            let result = start_project("dup", &config, &mut state);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("already running"));

            // Cleanup
            stop_project("dup", &mut state).unwrap();
            let _ = crate::logs::remove_project_log("dup");
        });
    }

    #[test]
    fn stop_not_running() {
        let mut state = AppState::default();
        let result = stop_project("nonexistent", &mut state);
        assert!(result.is_err());
    }

    #[test]
    fn stop_if_running_without_entry_reports_already_dead() {
        let mut state = AppState::default();
        assert_eq!(
            stop_project_if_running("ghost", &mut state).unwrap(),
            StopOutcome::AlreadyDead
        );
    }

    #[test]
    fn stop_if_running_cleans_stale_entry() {
        with_temp_devhub_dir(|| {
            let mut state = AppState::default();
            state.add(
                "crashed".to_string(),
                ProcessState::starting(99999999, Some(3000), 5_000),
            );

            assert_eq!(
                stop_project_if_running("crashed", &mut state).unwrap(),
                StopOutcome::AlreadyDead
            );
            assert!(!state.processes.contains_key("crashed"));
        });
    }

    #[test]
    fn stop_if_running_kills_live_group() {
        with_temp_devhub_dir(|| {
            let mut state = AppState::default();
            let config = ProjectConfig {
                path: "/tmp".to_string(),
                cmd: "sleep 30".to_string(),
                port: None,
                startup_timeout_ms: 60_000,
                ready_cmd: None,
                stop_timeout_ms: 5_000,
                verify_port_owner: true,
            };
            let pid = start_project("live", &config, &mut state).unwrap();

            match stop_project_if_running("live", &mut state).unwrap() {
                StopOutcome::Killed { pid: stopped_pid } => assert_eq!(stopped_pid, pid),
                outcome => panic!("expected Killed, got {outcome:?}"),
            }
            assert!(!state.is_running("live"));
            let _ = crate::logs::remove_project_log("live");
        });
    }

    #[test]
    fn signal_delivered_reports_missing_target_as_gone() {
        assert!(!signal_delivered(99999999, libc::SIGTERM).unwrap());
    }

    #[test]
    fn zombie_leader_with_live_member_is_not_gone() {
        let mut leader = Command::new("sh")
            .arg("-c")
            .arg("sleep 30 & exec true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();

        let group_id = leader.id();

        // The leader exits immediately; unreaped, it stays a zombie while
        // the `sleep 30` member keeps the group alive.
        std::thread::sleep(Duration::from_millis(300));

        assert!(crate::state::is_process_group_alive(group_id));
        assert!(!group_has_only_zombies(group_id));

        let _ = leader.wait();
        kill_process_group(group_id, Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!crate::state::is_process_group_alive(group_id));
    }

    #[test]
    fn start_rolls_back_when_state_cannot_be_saved() {
        use std::os::unix::fs::PermissionsExt;

        with_temp_devhub_dir(|| {
            let devhub_dir = crate::dirs::devhub_dir().unwrap();
            let logs_dir = crate::dirs::logs_dir().unwrap();
            // The lock can still be taken; only writing the state fails.
            std::fs::File::create(crate::dirs::state_lock_path().unwrap()).unwrap();
            std::fs::set_permissions(&devhub_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

            let mut state = AppState::default();
            let config = ProjectConfig {
                path: "/tmp".to_string(),
                cmd: "sleep 30".to_string(),
                port: None,
                startup_timeout_ms: 60_000,
                ready_cmd: None,
                stop_timeout_ms: 5_000,
                verify_port_owner: true,
            };

            let result = start_project("leaky", &config, &mut state);
            std::fs::set_permissions(&devhub_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

            let error = format!("{:#}", result.unwrap_err());
            assert!(
                error.contains("failed to persist state"),
                "error was: {error}"
            );
            assert!(
                error.contains("stopped the just-started process"),
                "error was: {error}"
            );
            assert!(state.processes.is_empty());

            let pid: u32 = error
                .split("(pid ")
                .nth(1)
                .and_then(|rest| rest.split(')').next())
                .and_then(|pid| pid.parse().ok())
                .unwrap_or_else(|| panic!("no pid in error: {error}"));
            assert!(
                !crate::state::is_process_group_alive(pid) || group_has_only_zombies(pid),
                "the just-started process must be rolled back"
            );
            assert!(!logs_dir.join("leaky.log").exists());
        });
    }

    #[test]
    fn project_runs_in_its_own_session() {
        with_temp_devhub_dir(|| {
            let mut state = AppState::default();
            let config = ProjectConfig {
                path: "/tmp".to_string(),
                cmd: "sleep 30".to_string(),
                port: None,
                startup_timeout_ms: 60_000,
                ready_cmd: None,
                stop_timeout_ms: 5_000,
                verify_port_owner: true,
            };

            let pid = start_project("session", &config, &mut state).unwrap();
            // Session leader, hence also group leader, and so without a
            // controlling terminal.
            assert_eq!(unsafe { libc::getsid(pid as i32) }, pid as i32);
            assert_eq!(unsafe { libc::getpgid(pid as i32) }, pid as i32);

            stop_project("session", &mut state).unwrap();
            let _ = crate::logs::remove_project_log("session");
        });
    }

    #[test]
    fn started_project_is_recorded_as_ready() {
        with_temp_devhub_dir(|| {
            let mut state = AppState::default();
            let config = ProjectConfig {
                path: "/tmp".to_string(),
                cmd: "sleep 30".to_string(),
                port: None,
                startup_timeout_ms: 60_000,
                ready_cmd: Some("true".to_string()),
                stop_timeout_ms: 5_000,
                verify_port_owner: true,
            };

            let pid = start_project("recorded", &config, &mut state).unwrap();
            let on_disk = AppState::load().unwrap();
            let entry = &on_disk.processes["recorded"];
            assert_eq!(entry.pid, pid);
            assert!(entry.ready);
            assert!(entry.leader_started.is_some());

            stop_project("recorded", &mut state).unwrap();
            let _ = crate::logs::remove_project_log("recorded");
        });
    }

    #[test]
    fn stop_leaves_a_reused_pid_alone() {
        with_temp_devhub_dir(|| {
            let mut bystander = Command::new("sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .unwrap();
            let mut state = AppState::default();
            state.add(
                "gone".to_string(),
                ProcessState {
                    leader_started: Some("Mon Jan  1 00:00:00 2001".to_string()),
                    ..ProcessState::starting(bystander.id(), None, 5_000)
                },
            );

            assert_eq!(
                stop_project_if_running("gone", &mut state).unwrap(),
                StopOutcome::AlreadyDead
            );
            assert!(
                bystander.try_wait().unwrap().is_none(),
                "an unrelated process that reused the pid must not be signaled"
            );

            bystander.kill().unwrap();
            bystander.wait().unwrap();
        });
    }

    #[test]
    fn kill_group_when_leader_has_exited() {
        let mut leader = Command::new("sh")
            .arg("-c")
            .arg("sleep 30 & exec true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();

        let group_id = leader.id();
        let status = leader.wait().unwrap();
        assert!(status.success());
        assert!(!crate::state::is_pid_alive(group_id));
        assert!(crate::state::is_process_group_alive(group_id));

        kill_process_group(group_id, Duration::from_secs(5)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        assert!(!crate::state::is_process_group_alive(group_id));
    }

    #[test]
    fn tcp_probe_detects_open_listener() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(loopback_accepts(port));
    }

    #[test]
    fn tcp_probe_detects_ipv6_only_listener() {
        // Skip on hosts without IPv6 loopback.
        let Ok(listener) = std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        assert!(loopback_accepts(port));
    }

    #[test]
    fn start_fails_when_port_is_held_by_foreign_listener() {
        with_temp_devhub_dir(|| {
            // Listener owned by this test process, i.e. outside the spawned
            // project's process group.
            let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();

            let mut state = AppState::default();
            let config = ProjectConfig {
                path: "/tmp".to_string(),
                cmd: "sleep 30".to_string(),
                port: Some(port),
                startup_timeout_ms: 5_000,
                ready_cmd: None,
                stop_timeout_ms: 5_000,
                verify_port_owner: true,
            };

            let error = start_project("foreign-port", &config, &mut state).unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains(&format!("port {port} is held by pid")),
                "error was: {message}"
            );
            assert!(!state.is_running("foreign-port"));
            let _ = logs::remove_project_log("foreign-port");
        });
    }

    #[test]
    fn listener_details_reports_process_group() {
        let (pgid, _) = listener_details(std::process::id()).unwrap();
        assert_eq!(pgid, unsafe { libc::getpgrp() as u32 });
    }

    #[test]
    fn readiness_description_prefers_ready_cmd() {
        let config = ProjectConfig {
            path: "/tmp".to_string(),
            cmd: "sleep 1".to_string(),
            port: Some(3000),
            startup_timeout_ms: 60_000,
            ready_cmd: Some("curl -f http://127.0.0.1:3000/healthz".to_string()),
            stop_timeout_ms: 5_000,
            verify_port_owner: true,
        };

        assert_eq!(
            describe_readiness(&config),
            "exec `curl -f http://127.0.0.1:3000/healthz`"
        );
    }

    #[test]
    fn hanging_ready_cmd_respects_startup_timeout() {
        with_temp_devhub_dir(|| {
            let mut state = AppState::default();
            let config = ProjectConfig {
                path: "/tmp".to_string(),
                cmd: "sleep 5".to_string(),
                port: None,
                startup_timeout_ms: 300,
                ready_cmd: Some("sleep 5".to_string()),
                stop_timeout_ms: 5_000,
                verify_port_owner: true,
            };

            let started = Instant::now();
            let error = start_project("hang-probe", &config, &mut state).unwrap_err();

            assert!(error.to_string().contains("startup timed out after 300ms"));
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "startup timeout should not be blocked by a hanging readiness command"
            );
            let _ = crate::logs::remove_project_log("hang-probe");
        });
    }
}
