use std::collections::HashMap;

use eyre::WrapErr;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::dirs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessState {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    #[serde(flatten)]
    pub processes: HashMap<String, ProcessState>,
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

    pub fn save(&self) -> eyre::Result<()> {
        let path = dirs::state_path()?;
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write state to {}", path.display()))?;
        Ok(())
    }

    pub fn add(&mut self, name: String, pid: u32, port: Option<u16>) {
        self.processes.insert(name, ProcessState {
            pid,
            started_at: Utc::now(),
            port,
        });
    }

    pub fn remove(&mut self, name: &str) -> Option<ProcessState> {
        self.processes.remove(name)
    }

    pub fn is_running(&self, name: &str) -> bool {
        if let Some(ps) = self.processes.get(name) {
            is_pid_alive(ps.pid)
        } else {
            false
        }
    }

    /// Remove entries whose processes are no longer alive.
    pub fn prune_dead(&mut self) -> Vec<String> {
        let dead: Vec<String> = self.processes.iter()
            .filter(|(_, ps)| !is_pid_alive(ps.pid))
            .map(|(name, _)| name.clone())
            .collect();
        for name in &dead {
            self.processes.remove(name);
        }
        dead
    }
}

pub fn is_pid_alive(pid: u32) -> bool {
    // Send signal 0 to check if process exists
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_serialize_roundtrip() {
        let mut state = AppState::default();
        state.add("worth".to_string(), 12345, Some(3000));
        state.add("blog".to_string(), 12346, None);

        let content = serde_json::to_string_pretty(&state).unwrap();
        let loaded: AppState = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.processes.len(), 2);
        assert_eq!(loaded.processes["worth"].pid, 12345);
        assert_eq!(loaded.processes["worth"].port, Some(3000));
        assert_eq!(loaded.processes["blog"].port, None);
    }

    #[test]
    fn state_add_and_remove() {
        let mut state = AppState::default();
        state.add("test".to_string(), 9999, Some(8080));
        assert!(state.processes.contains_key("test"));

        let removed = state.remove("test");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().pid, 9999);
        assert!(state.processes.is_empty());
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
    fn prune_dead_processes() {
        let mut state = AppState::default();
        let current_pid = std::process::id();
        state.add("alive".to_string(), current_pid, None);
        state.add("dead".to_string(), 99999999, None);

        let dead = state.prune_dead();
        assert_eq!(dead, vec!["dead".to_string()]);
        assert_eq!(state.processes.len(), 1);
        assert!(state.processes.contains_key("alive"));
    }
}
