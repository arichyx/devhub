use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use eyre::{WrapErr, bail};

use crate::config::{ProjectConfig, expand_path};
use crate::state::AppState;

pub fn start_project(name: &str, config: &ProjectConfig, state: &mut AppState) -> eyre::Result<u32> {
    if state.is_running(name) {
        bail!("project '{}' is already running (pid {})", name, state.processes[name].pid);
    }

    let expanded = expand_path(&config.path);
    let project_dir = Path::new(&expanded);
    if !project_dir.exists() {
        bail!("project directory '{}' does not exist", expanded);
    }

    // Spawn the command in a new process group so we can kill the whole group
    let child = Command::new("sh")
        .arg("-c")
        .arg(&config.cmd)
        .current_dir(project_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("failed to start command '{}' in {}", config.cmd, expanded))?;

    let pid = child.id();
    state.add(name.to_string(), pid, config.port);
    state.save()?;

    Ok(pid)
}

pub fn stop_project(name: &str, state: &mut AppState) -> eyre::Result<()> {
    if !state.processes.contains_key(name) {
        bail!("project '{}' is not running", name);
    }

    let pid = state.processes[name].pid;

    if !state.is_running(name) {
        state.remove(name);
        state.save()?;
        bail!("project '{}' process (pid {}) is no longer alive", name, pid);
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
                bail!("failed to send SIGTERM to process {}: {}", pid, std::io::Error::last_os_error());
            }
        }
    }

    // Give the process a moment to shut down gracefully
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Check if still alive, force kill if necessary
    if crate::state::is_pid_alive(pid) {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    #[test]
    fn start_and_stop_process() {
        let mut state = AppState::default();
        let config = ProjectConfig {
            path: "/tmp".to_string(),
            cmd: "sleep 10".to_string(),
            port: None,
        };

        let pid = start_project("test-sleep", &config, &mut state).unwrap();
        assert!(pid > 0);
        assert!(state.is_running("test-sleep"));

        // Stop the project
        stop_project("test-sleep", &mut state).unwrap();
        assert!(!state.is_running("test-sleep"));
    }

    #[test]
    fn start_nonexistent_dir() {
        let mut state = AppState::default();
        let config = ProjectConfig {
            path: "/nonexistent/path".to_string(),
            cmd: "echo hello".to_string(),
            port: None,
        };

        let result = start_project("bad-path", &config, &mut state);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn start_already_running() {
        let mut state = AppState::default();
        let config = ProjectConfig {
            path: "/tmp".to_string(),
            cmd: "sleep 30".to_string(),
            port: None,
        };

        let pid = start_project("dup", &config, &mut state).unwrap();

        // Try to start again
        let result = start_project("dup", &config, &mut state);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already running"));

        // Cleanup
        stop_project("dup", &mut state).unwrap();
    }

    #[test]
    fn stop_not_running() {
        let mut state = AppState::default();
        let result = stop_project("nonexistent", &mut state);
        assert!(result.is_err());
    }
}
