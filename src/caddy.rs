use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use eyre::{WrapErr, eyre};

use crate::dirs;
use crate::logs;
use crate::state::AppState;

const CADDY_TIMEOUT: Duration = Duration::from_secs(5);

/// The gateway listens on the standard HTTP port so project URLs stay
/// port-free (`http://<name>.localhost`).
const DEFAULT_HTTP_PORT: u16 = 80;

/// devhub's caddy gets its own admin endpoint instead of caddy's default
/// `localhost:2019`, so `caddy reload`/`stop` can never reconfigure or stop
/// a caddy the user runs for something else.
const DEFAULT_ADMIN_PORT: u16 = 12019;

/// Lines of caddy's own output quoted in a start failure.
const CADDY_OUTPUT_LINES: usize = 5;

/// The `caddy` binary is not on PATH. Kept distinct so routine reconciles can
/// stay quiet about it; `start` and `stop` still warn.
#[derive(Debug)]
struct CaddyNotInstalled;

impl std::fmt::Display for CaddyNotInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to run caddy - is it installed?")
    }
}

impl std::error::Error for CaddyNotInstalled {}

pub fn is_not_installed(err: &eyre::Report) -> bool {
    err.downcast_ref::<CaddyNotInstalled>().is_some()
}

/// `DEVHUB_CADDY_ADMIN_PORT` overrides the admin port, e.g. when 12019 is
/// taken or to point tests at a stand-in.
fn admin_port() -> u16 {
    std::env::var("DEVHUB_CADDY_ADMIN_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(DEFAULT_ADMIN_PORT)
}

fn admin_address() -> String {
    format!("localhost:{}", admin_port())
}

/// `DEVHUB_HTTP_PORT` moves the gateway off port 80, e.g. where binding it
/// needs privileges (Linux) or to keep tests off the machine's real gateway.
fn http_port() -> u16 {
    std::env::var("DEVHUB_HTTP_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(DEFAULT_HTTP_PORT)
}

pub fn project_url(name: &str) -> String {
    match http_port() {
        DEFAULT_HTTP_PORT => format!("http://{name}.localhost"),
        port => format!("http://{name}.localhost:{port}"),
    }
}

/// Whether devhub's caddy is up, judged by its admin endpoint accepting
/// connections.
fn caddy_running() -> bool {
    crate::process::loopback_accepts(admin_port())
}

pub fn generate_caddyfile(state: &AppState) -> String {
    let mut entries = Vec::new();

    // Projects still waiting for readiness get no route yet. Iteration order
    // is the state's sorted order, so equal states give byte-equal files.
    for (name, ps) in state.processes.iter().filter(|(_, ps)| ps.ready) {
        if let Some(port) = ps.port {
            entries.push(format!(
                "http://{name}.localhost {{\n    reverse_proxy localhost:{port}\n}}"
            ));
        }
    }

    if entries.is_empty() {
        return String::new();
    }

    let mut global = format!("    admin {}\n", admin_address());
    if http_port() != DEFAULT_HTTP_PORT {
        global.push_str(&format!("    http_port {}\n", http_port()));
    }
    format!("{{\n{global}}}\n\n{}", entries.join("\n\n"))
}

pub fn reconcile_caddy(state: &AppState) -> eyre::Result<bool> {
    let desired = generate_caddyfile(state);
    let path = dirs::caddyfile_path()?;
    let current = match std::fs::read_to_string(&path) {
        Ok(content) => Some(content),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read Caddyfile from {}", path.display()));
        }
    };

    // A matching file only records what was last applied: a caddy that has
    // since crashed or been stopped leaves every route down with the file
    // intact.
    let routes_down = !desired.is_empty() && !caddy_running();
    if caddyfile_is_current(&desired, current.as_deref()) && !routes_down {
        return Ok(false);
    }

    reload_caddy(state)?;
    Ok(true)
}

pub fn reload_caddy(state: &AppState) -> eyre::Result<()> {
    let content = generate_caddyfile(state);

    if content.is_empty() {
        let path = dirs::caddyfile_path()?;
        if path.exists() {
            let _ = run_caddy(&["stop", "--address", &admin_address()], None);
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }

    let path = dirs::caddyfile_path()?;
    std::fs::write(&path, &content)
        .with_context(|| format!("failed to write Caddyfile to {}", path.display()))?;

    let applied = apply_caddyfile(&path).and_then(|()| check_gateway_is_ours());
    if applied.is_err() {
        // reconcile_caddy treats the on-disk file as the applied config; left
        // in place after a failure, it would look current and never be retried.
        let _ = std::fs::remove_file(&path);
    }
    applied
}

fn apply_caddyfile(path: &Path) -> eyre::Result<()> {
    let config_arg = path.display().to_string();

    // Try reload first (works if caddy is already running)
    let reload = ["reload", "--config", &config_arg, "--address", &admin_address()];
    if let Ok(true) = run_caddy(&reload, None) {
        return Ok(());
    }

    // Caddy not running — start it. The server it launches outlives devhub
    // and keeps writing to caddy.log.
    let log_path = dirs::caddy_log_path()?;
    if run_caddy(&["start", "--config", &config_arg], Some(&log_path))? {
        return Ok(());
    }

    let output = logs::read_log_tail(&log_path).ok().flatten().unwrap_or_default();
    let mut message = String::from("caddy failed to start");
    if output.is_empty() || output.contains("address already in use") {
        message.push_str(&format!("; {}", describe_port_conflict()));
    }
    if !output.is_empty() {
        message.push_str(&format!(
            "\n\nlast caddy output ({}):\n{}",
            log_path.display(),
            last_lines(&output, CADDY_OUTPUT_LINES)
        ));
    }
    Err(eyre!(message))
}

/// Fail if anything besides devhub's caddy listens on the gateway port.
/// Caddy binds with SO_REUSEPORT, so starting it next to another caddy does
/// not fail: both listen, and on macOS the instance that bound first keeps
/// receiving the connections — devhub's routes would silently never be
/// served.
fn check_gateway_is_ours() -> eyre::Result<()> {
    let port = http_port();
    let (Some(holders), Some(ours)) = (
        crate::process::listening_pids(port),
        crate::process::listening_pids(admin_port()),
    ) else {
        // Without lsof (or without our admin listener) the owner is unknown.
        return Ok(());
    };

    let others: Vec<String> = holders
        .into_iter()
        .filter(|pid| !ours.contains(pid))
        .map(|pid| describe_port_holder(pid, &ours))
        .collect();
    if others.is_empty() {
        return Ok(());
    }
    Err(eyre!(
        "devhub's caddy is running, but port {port} is also held by {}, which may \
         receive the traffic for *.localhost instead",
        others.join(" and ")
    ))
}

fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// Name what holds the gateway port when caddy cannot bind it.
fn describe_port_conflict() -> String {
    let port = http_port();
    let Some(holders) = crate::process::listening_pids(port) else {
        return format!("check whether port {port} is already in use");
    };
    let ours = crate::process::listening_pids(admin_port()).unwrap_or_default();
    let holders: Vec<String> = holders
        .into_iter()
        .map(|pid| describe_port_holder(pid, &ours))
        .collect();
    format!("port {port} is held by {}", holders.join(" and "))
}

/// `ours` are the pids serving devhub's admin endpoint, i.e. its own caddy.
fn describe_port_holder(pid: u32, ours: &[u32]) -> String {
    if ours.contains(&pid) {
        return format!("devhub's own caddy (pid {pid})");
    }
    let command = crate::process::process_command_line(pid).unwrap_or_default();
    let our_caddyfile = dirs::caddyfile_path()
        .is_ok_and(|caddyfile| command.contains(caddyfile.to_string_lossy().as_ref()));
    if our_caddyfile {
        // Started from devhub's Caddyfile, yet not on devhub's admin
        // endpoint: left behind by a devhub version that used caddy's
        // default endpoint, which `caddy stop` still reaches.
        format!("a caddy that an earlier devhub version started (pid {pid}); stop it once with `caddy stop`")
    } else if command.contains("caddy") {
        format!("a caddy instance devhub does not manage (pid {pid}: `{command}`)")
    } else {
        format!("pid {pid} (`{command}`)")
    }
}

fn caddyfile_is_current(desired: &str, current: Option<&str>) -> bool {
    match (desired.is_empty(), current) {
        (true, None) => true,
        (_, Some(current)) => desired == current,
        (false, None) => false,
    }
}

/// Run one caddy CLI command and report whether it succeeded. With `log`,
/// its output replaces that file; otherwise it is discarded.
fn run_caddy(args: &[&str], log: Option<&Path>) -> eyre::Result<bool> {
    let mut command = Command::new("caddy");
    command.args(args).stdin(Stdio::null());
    // `caddy start` leaves a server running that would hold any descriptor
    // it inherits for as long as it lives.
    crate::process::inherit_only_stdio(&mut command);
    match log {
        Some(path) => {
            // `caddy start` hands its stdout/stderr to the server it launches.
            // Unlink rather than truncate: a server from an earlier start may
            // still hold the old file open.
            let _ = std::fs::remove_file(path);
            let file = File::create(path)
                .with_context(|| format!("failed to create {}", path.display()))?;
            command.stdout(file.try_clone()?).stderr(file);
        }
        None => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CaddyNotInstalled.into());
        }
        Err(err) => return Err(err).wrap_err("failed to run caddy"),
    };

    match child.wait_timeout(CADDY_TIMEOUT)? {
        Some(status) => Ok(status.success()),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Ok(false)
        }
    }
}

trait ChildExt {
    fn wait_timeout(&mut self, timeout: Duration)
    -> eyre::Result<Option<std::process::ExitStatus>>;
}

impl ChildExt for std::process::Child {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> eyre::Result<Option<std::process::ExitStatus>> {
        let start = std::time::Instant::now();
        loop {
            match self.try_wait()? {
                Some(status) => return Ok(Some(status)),
                None if start.elapsed() >= timeout => return Ok(None),
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ProcessState;
    use chrono::Utc;

    fn make_state(entries: Vec<(&str, u32, Option<u16>)>) -> AppState {
        let mut processes = std::collections::BTreeMap::new();
        for (name, pid, port) in entries {
            processes.insert(
                name.to_string(),
                ProcessState {
                    pid,
                    started_at: Utc::now(),
                    port,
                    leader_started: None,
                    ready: true,
                    stop_timeout_ms: None,
                },
            );
        }
        AppState { processes }
    }

    #[test]
    fn generate_empty_caddyfile() {
        let state = AppState::default();
        let content = generate_caddyfile(&state);
        assert!(content.is_empty());
    }

    #[test]
    fn generate_caddyfile_with_ports() {
        let state = make_state(vec![
            ("worth", 100, Some(3000)),
            ("blog", 101, Some(4000)),
            ("noport", 102, None),
        ]);

        let content = generate_caddyfile(&state);

        assert!(content.contains("http://worth.localhost {\n"));
        assert!(content.contains("reverse_proxy localhost:3000"));
        assert!(content.contains("http://blog.localhost {\n"));
        assert!(content.contains("reverse_proxy localhost:4000"));
        assert!(!content.contains("noport.local"));
    }

    #[test]
    fn caddyfile_format() {
        let state = make_state(vec![("app", 100, Some(8080))]);

        let content = generate_caddyfile(&state);
        let expected = "{\n    admin localhost:12019\n}\n\n\
                        http://app.localhost {\n    reverse_proxy localhost:8080\n}";
        assert_eq!(content, expected);
    }

    #[test]
    fn caddyfile_lists_routes_in_name_order() {
        let state = make_state(vec![
            ("worth", 100, Some(3000)),
            ("blog", 101, Some(4000)),
            ("api", 102, Some(5000)),
        ]);

        let content = generate_caddyfile(&state);
        let positions: Vec<usize> = ["api", "blog", "worth"]
            .iter()
            .map(|name| content.find(&format!("http://{name}.localhost")).unwrap())
            .collect();
        assert!(positions.is_sorted(), "content was:\n{content}");
    }

    #[test]
    fn starting_projects_get_no_route() {
        let mut state = make_state(vec![("ready", 100, Some(3000)), ("booting", 101, Some(4000))]);
        state.processes.get_mut("booting").unwrap().ready = false;

        let content = generate_caddyfile(&state);
        assert!(content.contains("ready.localhost"));
        assert!(!content.contains("booting.localhost"));
    }

    #[test]
    fn current_caddyfile_matches_desired_content() {
        let state = make_state(vec![("app", 100, Some(8080))]);
        let desired = generate_caddyfile(&state);

        assert!(caddyfile_is_current(&desired, Some(&desired)));
    }

    #[test]
    fn missing_caddyfile_is_stale_when_routes_are_expected() {
        let state = make_state(vec![("app", 100, Some(8080))]);
        let desired = generate_caddyfile(&state);

        assert!(!caddyfile_is_current(&desired, None));
    }

    #[test]
    fn leftover_caddyfile_is_stale_when_no_routes_are_expected() {
        let desired = String::new();
        let current = "http://app.localhost {\n    reverse_proxy localhost:8080\n}";

        assert!(!caddyfile_is_current(&desired, Some(current)));
    }

    #[test]
    fn last_lines_keeps_the_tail() {
        assert_eq!(last_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(last_lines("a", 5), "a");
    }

    #[test]
    fn port_conflict_hint_mentions_gateway_port() {
        let hint = describe_port_conflict();
        assert!(hint.contains("80"), "hint was: {hint}");
    }
}
