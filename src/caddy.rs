use std::process::Command;
use std::time::Duration;

use eyre::{WrapErr, eyre};

use crate::dirs;
use crate::state::AppState;

const CADDY_TIMEOUT: Duration = Duration::from_secs(5);

/// The gateway listens on the standard HTTP port so project URLs stay
/// port-free (`http://<name>.localhost`).
const PROXY_PORT: u16 = 80;

pub fn generate_caddyfile(state: &AppState) -> String {
    let mut entries = Vec::new();

    for (name, ps) in &state.processes {
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

    let config_arg = path.display().to_string();

    // Try reload first (works if caddy is already running)
    match run_caddy(["reload", "--config", &config_arg]) {
        Ok(true) => return Ok(()),
        _ => {}
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
    let mut child = Command::new("caddy")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .wrap_err("failed to run caddy - is it installed?")?;

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
        let mut processes = std::collections::HashMap::new();
        for (name, pid, port) in entries {
            processes.insert(
                name.to_string(),
                ProcessState {
                    pid,
                    started_at: Utc::now(),
                    port,
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
