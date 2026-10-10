use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use eyre::WrapErr;
use serde::{Deserialize, Serialize};

use crate::config::DEFAULT_STOP_TIMEOUT_MS;
use crate::dirs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessState {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub port: Option<u16>,
    /// Start time of the group leader as `ps -o lstart` reports it. It tells
    /// our process apart from an unrelated one that later reused its pid.
    #[serde(default)]
    pub leader_started: Option<String>,
    /// False while `devhub start` is still waiting for readiness.
    #[serde(default = "ready_by_default")]
    pub ready: bool,
    /// The project's `stop_timeout_ms`, recorded at start because `stop`
    /// does not read the config.
    #[serde(default)]
    pub stop_timeout_ms: Option<u64>,
}

/// Entries written before readiness was tracked were only ever recorded
/// once ready.
fn ready_by_default() -> bool {
    true
}

impl ProcessState {
    /// A just-spawned process group that has not passed readiness yet.
    pub fn starting(pid: u32, port: Option<u16>, stop_timeout_ms: u64) -> Self {
        Self {
            pid,
            started_at: Utc::now(),
            port,
            leader_started: process_start_time(pid),
            ready: false,
            stop_timeout_ms: Some(stop_timeout_ms),
        }
    }

    /// How long `stop` lets the group shut down after SIGTERM.
    pub fn stop_grace(&self) -> Duration {
        Duration::from_millis(self.stop_timeout_ms.unwrap_or(DEFAULT_STOP_TIMEOUT_MS))
    }

    /// Whether this entry still refers to a live process group of ours.
    pub fn is_alive(&self) -> bool {
        if !is_process_group_alive(self.pid) {
            return false;
        }

        // A live leader with a different start time is an unrelated process
        // that reused the pid after ours died. A missing leader is fine: the
        // kernel never hands out a pid that is still in use as a process
        // group id, so a surviving group is still ours.
        match (&self.leader_started, process_start_time(self.pid)) {
            (Some(recorded), Some(current)) => *recorded == current,
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    #[serde(flatten)]
    pub processes: BTreeMap<String, ProcessState>,
}

impl AppState {
    pub fn load() -> eyre::Result<Self> {
        let path = dirs::state_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read state from {}", path.display()))?;
        let state: AppState = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse state from {}", path.display()))?;
        Ok(state)
    }

    /// Apply `f` as one locked read-modify-write of the on-disk state. `self`
    /// is reloaded first, so entries written by concurrent devhub commands
    /// since our own load are kept instead of overwritten.
    pub fn update<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> eyre::Result<R> {
        let _lock = StateLock::acquire()?;
        *self = Self::load()?;
        let result = f(self);
        self.save()?;
        Ok(result)
    }

    /// Write via a temporary file and rename, so a concurrent reader or a
    /// crash mid-write never sees a truncated state file.
    fn save(&self) -> eyre::Result<()> {
        let path = dirs::state_path()?;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp, content)
            .with_context(|| format!("failed to write state to {}", tmp.display()))?;
        if let Err(err) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(err)
                .with_context(|| format!("failed to write state to {}", path.display()));
        }
        Ok(())
    }

    pub fn add(&mut self, name: String, process: ProcessState) {
        self.processes.insert(name, process);
    }

    #[cfg(test)]
    pub fn remove(&mut self, name: &str) -> Option<ProcessState> {
        self.processes.remove(name)
    }

    /// Remove `name` only while it still refers to `pid`; a concurrent devhub
    /// may have replaced the entry in the meantime.
    pub fn remove_if_pid(&mut self, name: &str, pid: u32) -> Option<ProcessState> {
        if self.processes.get(name)?.pid == pid {
            self.processes.remove(name)
        } else {
            None
        }
    }

    pub fn mark_ready(&mut self, name: &str, pid: u32) {
        if let Some(ps) = self.processes.get_mut(name).filter(|ps| ps.pid == pid) {
            ps.ready = true;
        }
    }

    /// The entry for `name`, if its process group is still alive.
    pub fn live(&self, name: &str) -> Option<&ProcessState> {
        self.processes.get(name).filter(|ps| ps.is_alive())
    }

    #[cfg(test)]
    pub fn is_running(&self, name: &str) -> bool {
        self.live(name).is_some()
    }

    /// Remove entries whose processes are no longer alive and return them.
    pub fn prune_dead(&mut self) -> Vec<(String, ProcessState)> {
        let dead_names: Vec<String> = self
            .processes
            .iter()
            .filter(|(_, ps)| !ps.is_alive())
            .map(|(name, _)| name.clone())
            .collect();

        let mut dead = Vec::with_capacity(dead_names.len());
        for name in dead_names {
            if let Some(process) = self.processes.remove(&name) {
                dead.push((name, process));
            }
        }

        dead
    }
}

/// Exclusive advisory lock that serializes state read-modify-write cycles
/// across devhub processes. Released when dropped.
struct StateLock {
    _file: File,
}

impl StateLock {
    fn acquire() -> eyre::Result<Self> {
        let path = dirs::state_lock_path()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open state lock {}", path.display()))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to lock {}", path.display()));
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
pub fn is_pid_alive(pid: u32) -> bool {
    signal_target_exists(pid as i32)
}

pub fn is_process_group_alive(group_id: u32) -> bool {
    signal_target_exists(-(group_id as i32))
}

/// Start time of `pid`, or `None` if no such process exists. `LC_ALL=C`
/// keeps the format stable between the recording and the comparison.
pub fn process_start_time(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let started = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !started.is_empty()).then_some(started)
}

fn signal_target_exists(target: i32) -> bool {
    unsafe {
        if libc::kill(target, 0) == 0 {
            true
        } else {
            matches!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    fn entry(pid: u32, port: Option<u16>) -> ProcessState {
        ProcessState {
            pid,
            started_at: Utc::now(),
            port,
            leader_started: None,
            ready: true,
            stop_timeout_ms: None,
        }
    }

    /// A `sleep` leading its own process group, as devhub's projects do.
    fn spawn_group_leader() -> std::process::Child {
        Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn state_serialize_roundtrip() {
        let mut state = AppState::default();
        state.add("worth".to_string(), entry(12345, Some(3000)));
        state.add("blog".to_string(), entry(12346, None));

        let content = serde_json::to_string_pretty(&state).unwrap();
        let loaded: AppState = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.processes.len(), 2);
        assert_eq!(loaded.processes["worth"].pid, 12345);
        assert_eq!(loaded.processes["worth"].port, Some(3000));
        assert_eq!(loaded.processes["blog"].port, None);
    }

    #[test]
    fn state_written_before_readiness_tracking_loads_as_ready() {
        let loaded: AppState = serde_json::from_str(
            r#"{"worth": {"pid": 12345, "started_at": "2026-01-01T00:00:00Z", "port": 3000}}"#,
        )
        .unwrap();
        assert!(loaded.processes["worth"].ready);
        assert_eq!(loaded.processes["worth"].leader_started, None);
        assert_eq!(
            loaded.processes["worth"].stop_grace(),
            Duration::from_millis(DEFAULT_STOP_TIMEOUT_MS)
        );
    }

    #[test]
    fn state_add_and_remove() {
        let mut state = AppState::default();
        state.add("test".to_string(), entry(9999, Some(8080)));
        assert!(state.processes.contains_key("test"));

        let removed = state.remove("test");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().pid, 9999);
        assert!(state.processes.is_empty());
    }

    #[test]
    fn remove_if_pid_keeps_a_replaced_entry() {
        let mut state = AppState::default();
        state.add("app".to_string(), entry(200, None));

        assert!(state.remove_if_pid("app", 100).is_none());
        assert!(state.processes.contains_key("app"));
        assert!(state.remove_if_pid("app", 200).is_some());
    }

    #[test]
    fn is_pid_alive_current_process() {
        // Current process should be alive
        let pid = std::process::id();
        assert!(is_pid_alive(pid));
    }

    #[test]
    fn is_pid_alive_nonexistent() {
        // PID 99999999 should not exist
        assert!(!is_pid_alive(99999999));
    }

    #[test]
    fn is_process_group_alive_nonexistent() {
        assert!(!is_process_group_alive(99999999));
    }

    #[test]
    fn prune_dead_processes() {
        let mut state = AppState::default();
        let current_group_id = unsafe { libc::getpgrp() as u32 };
        state.add("alive".to_string(), entry(current_group_id, None));
        state.add("dead".to_string(), entry(99999999, Some(3000)));

        let dead = state.prune_dead();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].0, "dead");
        assert_eq!(dead[0].1.port, Some(3000));
        assert_eq!(state.processes.len(), 1);
        assert!(state.processes.contains_key("alive"));
    }

    #[test]
    fn entry_with_matching_start_time_is_alive() {
        let mut leader = spawn_group_leader();
        let process = ProcessState::starting(leader.id(), None, DEFAULT_STOP_TIMEOUT_MS);
        assert!(process.leader_started.is_some());
        assert!(process.is_alive());

        leader.kill().unwrap();
        leader.wait().unwrap();
        assert!(!process.is_alive());
    }

    #[test]
    fn reused_pid_is_not_mistaken_for_the_recorded_process() {
        let mut leader = spawn_group_leader();
        let mut state = AppState::default();
        // As if the recorded project died and the OS gave its pid to this
        // unrelated group leader.
        state.add(
            "stale".to_string(),
            ProcessState {
                leader_started: Some("Mon Jan  1 00:00:00 2001".to_string()),
                ..entry(leader.id(), None)
            },
        );

        assert!(is_process_group_alive(leader.id()));
        assert!(!state.is_running("stale"));
        assert_eq!(state.prune_dead().len(), 1);

        leader.kill().unwrap();
        leader.wait().unwrap();
    }

    #[test]
    fn update_keeps_entries_written_since_load() {
        let tempdir = tempfile::tempdir().unwrap();
        crate::dirs::with_test_devhub_dir(tempdir.path().join(".devhub"), || {
            let mut first = AppState::load().unwrap();
            let mut second = AppState::load().unwrap();

            first
                .update(|s| s.add("a".to_string(), entry(1, None)))
                .unwrap();
            // `second` loaded before `a` was written; a blind save would drop it.
            second
                .update(|s| s.add("b".to_string(), entry(2, None)))
                .unwrap();

            let on_disk = AppState::load().unwrap();
            assert!(on_disk.processes.contains_key("a"));
            assert!(on_disk.processes.contains_key("b"));
        });
    }
}
