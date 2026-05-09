use std::collections::HashMap;

use eyre::WrapErr;
use serde::{Deserialize, Serialize};

use crate::dirs;

fn default_startup_timeout_ms() -> u64 {
    60_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub path: String,
    pub cmd: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default = "default_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    #[serde(default)]
    pub ready_cmd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub projects: HashMap<String, ProjectConfig>,
}

impl Config {
    pub fn load() -> eyre::Result<Self> {
        let path = dirs::proj_path()?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config from {}", path.display()))?;
        let config: Config = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse config from {}", path.display()))?;
        Ok(config)
    }
}

pub fn expand_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = ::dirs::home_dir() {
            return format!("{}/{}", home.display(), rest);
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_with_ports() {
        let json = r#"{
            "worth": {
                "path": "~/Projects/worth_meter",
                "cmd": "pnpm dev",
                "port": 3000,
                "startup_timeout_ms": 120000,
                "ready_cmd": "curl -fsS http://127.0.0.1:3000/healthz"
            },
            "blog": {
                "path": "~/Code/blogs",
                "cmd": "pnpm dev"
            }
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.projects.len(), 2);
        assert_eq!(config.projects["worth"].port, Some(3000));
        assert_eq!(config.projects["worth"].startup_timeout_ms, 120_000);
        assert_eq!(
            config.projects["worth"].ready_cmd.as_deref(),
            Some("curl -fsS http://127.0.0.1:3000/healthz")
        );
        assert_eq!(config.projects["blog"].port, None);
        assert_eq!(
            config.projects["blog"].startup_timeout_ms,
            default_startup_timeout_ms()
        );
    }

    #[test]
    fn parse_config_without_ports() {
        let json = r#"{
            "worth": {
                "path": "~/Projects/worth_meter",
                "cmd": "pnpm dev"
            }
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.projects["worth"].port, None);
        assert_eq!(config.projects["worth"].cmd, "pnpm dev");
        assert_eq!(
            config.projects["worth"].startup_timeout_ms,
            default_startup_timeout_ms()
        );
        assert_eq!(config.projects["worth"].ready_cmd, None);
    }

    #[test]
    fn expand_tilde_in_path() {
        let home = ::dirs::home_dir().unwrap();
        let expanded = expand_path("~/Projects/test");
        assert_eq!(expanded, format!("{}/Projects/test", home.display()));
    }

    #[test]
    fn no_expand_for_absolute_path() {
        let expanded = expand_path("/absolute/path");
        assert_eq!(expanded, "/absolute/path");
    }

    #[test]
    fn roundtrip_serialize() {
        let json = r#"{"app":{"path":"/tmp/app","cmd":"npm start","port":8080,"startup_timeout_ms":45000,"ready_cmd":"curl -f http://127.0.0.1:8080/healthz"}}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        let output = serde_json::to_string(&config).unwrap();
        let reparsed: Config = serde_json::from_str(&output).unwrap();
        assert_eq!(reparsed.projects["app"].port, Some(8080));
        assert_eq!(reparsed.projects["app"].startup_timeout_ms, 45_000);
        assert_eq!(
            reparsed.projects["app"].ready_cmd.as_deref(),
            Some("curl -f http://127.0.0.1:8080/healthz")
        );
    }
}
