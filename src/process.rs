use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use eyre::{WrapErr, bail, eyre};

use crate::config::{ProjectConfig, expand_path};
use crate::logs;
use crate::state::AppState;

const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(250);
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const READY_CMD_TIMEOUT_CAP: Duration = Duration::from_secs(1);

pub fn start_project(
    name: &str,
    config: &ProjectConfig,
    state: &mut AppState,
) -> eyre::Result<u32> {
    if state.is_running(name) {
        bail!(
            "project '{}' is already running (pid {})",
            name,
            state.processes[name].pid
        );
    }

    let expanded = expand_path(&config.path);
    let project_dir = Path::new(&expanded);
    if !project_dir.exists() {
        bail!("project directory '{}' does not exist", expanded);
    }

    let (_log_path, stdout_log) = logs::create_project_log(name)?;
    let stderr_log = stdout_log
        .try_clone()
        .with_context(|| format!("failed to clone log handle for project '{name}'"))?;

    // Spawn the command in a new process group so we can kill the whole group
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(&config.cmd)
        .current_dir(project_dir)
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log))
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            let _ = logs::remove_project_log(name);
            return Err(err).with_context(|| {
                format!("failed to start command '{}' in {}", config.cmd, expanded)
            });
        }
    };

    let pid = child.id();
    wait_for_startup(name, config, project_dir, pid, &mut child)?;

    state.add(name.to_string(), pid, config.port);
    state.save()?;

    Ok(pid)
}

pub fn stop_project(name: &str, state: &mut AppState) -> eyre::Result<()> {
    if !state.processes.contains_key(name) {
        bail!("project '{}' is not running", name);
    }

    let pid = state.processes[name].pid;

    if !crate::state::is_process_group_alive(pid) {
        state.remove(name);
        state.save()?;
        bail!(
            "project '{}' process group (pid {}) is no longer alive",
            name,
            pid
        );
    }

    // Kill the entire process group (negative PID sends to group)
    kill_process_group(pid)?;

    state.remove(name);
    state.save()?;
    Ok(())
}

fn kill_process_group(pid: u32) -> eyre::Result<()> {
    // First try SIGTERM to the process group
    unsafe {
        let ret = libc::kill(-(pid as i32), libc::SIGTERM);
        if ret != 0 {
            // If killing the group fails, try killing just the process
            let ret = libc::kill(pid as i32, libc::SIGTERM);
            if ret != 0 {
                bail!(
                    "failed to send SIGTERM to process {}: {}",
                    pid,
                    std::io::Error::last_os_error()
                );
            }
        }
    }

    // Give the process a moment to shut down gracefully
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Check if still alive, force kill if necessary
    if crate::state::is_process_group_alive(pid) {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }

    Ok(())
}

pub fn describe_readiness(config: &ProjectConfig) -> String {
    if let Some(cmd) = &config.ready_cmd {
        format!("exec `{cmd}`")
    } else if let Some(port) = config.port {
        format!("tcp 127.0.0.1:{port}")
    } else {
        "none (spawn only)".to_string()
    }
}

fn wait_for_startup(
    name: &str,
    config: &ProjectConfig,
    project_dir: &Path,
    group_id: u32,
    child: &mut Child,
) -> eyre::Result<()> {
    if config.ready_cmd.is_none() && config.port.is_none() {
        return Ok(());
    }

    let timeout = Duration::from_millis(config.startup_timeout_ms);
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(status) = child.try_wait()? {
            if !crate::state::is_process_group_alive(group_id) {
                return Err(startup_failure(
                    name,
                    format!(
                        "project exited before becoming ready{}",
                        format_exit_status(Some(&status))
                    ),
                ));
            }
        }

        if !crate::state::is_process_group_alive(group_id) {
            return Err(startup_failure(
                name,
                format!(
                    "project exited before becoming ready{}",
                    format_exit_status(None)
                ),
            ));
        }

        let now = Instant::now();
        if now >= deadline {
            if crate::state::is_process_group_alive(group_id) {
                let _ = kill_process_group(group_id);
            }
            return Err(startup_failure(
                name,
                format!("startup timed out after {}ms", config.startup_timeout_ms),
            ));
        }

        let remaining = deadline.saturating_duration_since(now);
        let probe_timeout = remaining.min(READY_CMD_TIMEOUT_CAP);
        if readiness_probe_passed(config, project_dir, probe_timeout)? {
            return Ok(());
        }

        if Instant::now() >= deadline {
            if crate::state::is_process_group_alive(group_id) {
                let _ = kill_process_group(group_id);
            }
            return Err(startup_failure(
                name,
                format!("startup timed out after {}ms", config.startup_timeout_ms),
            ));
        }

        std::thread::sleep(READINESS_POLL_INTERVAL);
    }
}

fn readiness_probe_passed(
    config: &ProjectConfig,
    project_dir: &Path,
    probe_timeout: Duration,
) -> eyre::Result<bool> {
    if let Some(cmd) = &config.ready_cmd {
        return exec_readiness_probe(cmd, project_dir, probe_timeout);
    }

    if let Some(port) = config.port {
        return Ok(tcp_readiness_probe(port));
    }

    Ok(false)
}

fn exec_readiness_probe(cmd: &str, project_dir: &Path, timeout: Duration) -> eyre::Result<bool> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(project_dir)
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

fn tcp_readiness_probe(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, TCP_CONNECT_TIMEOUT).is_ok()
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

        kill_process_group(group_id).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        assert!(!crate::state::is_process_group_alive(group_id));
    }

    #[test]
    fn tcp_probe_detects_open_listener() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(tcp_readiness_probe(port));
    }

    #[test]
    fn readiness_description_prefers_ready_cmd() {
        let config = ProjectConfig {
            path: "/tmp".to_string(),
            cmd: "sleep 1".to_string(),
            port: Some(3000),
            startup_timeout_ms: 60_000,
            ready_cmd: Some("curl -f http://127.0.0.1:3000/healthz".to_string()),
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
