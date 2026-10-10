use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use eyre::{WrapErr, eyre};

use crate::dirs;
use crate::state::AppState;

const CADDY_TIMEOUT: Duration = Duration::from_secs(5);

/// The gateway listens on the standard HTTP port so project URLs stay
/// port-free (`http://<name>.localhost`).
const PROXY_PORT: u16 = 80;

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

    entries.join("\n\n")
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

    if caddyfile_is_current(&desired, current.as_deref()) {
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
            let _ = run_caddy(["stop", "--config", &path.display().to_string()]);
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }

    let path = dirs::caddyfile_path()?;
    std::fs::write(&path, &content)
        .with_context(|| format!("failed to write Caddyfile to {}", path.display()))?;

    let applied = apply_caddyfile(&path);
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
    if let Ok(true) = run_caddy(["reload", "--config", &config_arg]) {
        return Ok(());
    }

    // Caddy not running — start it
    match run_caddy(["start", "--config", &config_arg]) {
        Ok(true) => Ok(()),
        Ok(false) => Err(eyre!(
            "caddy failed to start; {}",
            describe_port_conflict()
        )),
        // Spawn failures mean caddy itself is missing; nothing to diagnose.
        Err(e) => Err(e),
    }
}

/// Name the process holding the gateway port when caddy cannot bind it.
fn describe_port_conflict() -> String {
    if let Some(pids) = crate::process::listening_pids(PROXY_PORT) {
        if let Some(pid) = pids.first() {
            if let Some((_, command)) = crate::process::listener_details(*pid) {
                if command.contains("caddy") {
                    return format!(
                        "an existing caddy instance (pid {pid}) already holds port {PROXY_PORT} — try `caddy stop`"
                    );
                }
                return format!("port {PROXY_PORT} is held by pid {pid} (`{command}`)");
            }
        }
    }
    format!("check whether port {PROXY_PORT} is already in use")
}

fn caddyfile_is_current(desired: &str, current: Option<&str>) -> bool {
    match (desired.is_empty(), current) {
        (true, None) => true,
        (_, Some(current)) => desired == current,
        (false, None) => false,
    }
}

fn run_caddy(args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> eyre::Result<bool> {
    let mut child = match Command::new("caddy")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CaddyNotInstalled.into());
        }
        Err(err) => return Err(err).wrap_err("failed to run caddy"),
    };

    match child.wait_timeout(CADDY_TIMEOUT)? {
        Some(status) => Ok(status.success()),
        None => {
            child.kill()?;
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
        let expected = "http://app.localhost {\n    reverse_proxy localhost:8080\n}";
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
    fn port_conflict_hint_mentions_gateway_port() {
        let hint = describe_port_conflict();
        assert!(hint.contains("80"), "hint was: {hint}");
    }
}
