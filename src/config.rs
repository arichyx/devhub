use std::collections::HashMap;

use eyre::{WrapErr, bail};
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
        config.validate()?;
        Ok(config)
    }

    /// Readiness probing and Caddy routing both target `port`, so two projects
    /// declaring the same port would let one project's listener satisfy the
    /// other's readiness probe and claim its proxy route.
    pub fn validate(&self) -> eyre::Result<()> {
        let mut ports: HashMap<u16, Vec<&str>> = HashMap::new();
        for (name, project) in &self.projects {
            if let Some(port) = project.port {
                ports.entry(port).or_default().push(name.as_str());
            }
        }

        let mut duplicates: Vec<String> = ports
            .into_iter()
            .filter(|(_, names)| names.len() > 1)
            .map(|(port, mut names)| {
                names.sort_unstable();
                format!(
                    "duplicate port {port} declared by projects {}",
                    names
                        .iter()
                        .map(|name| format!("'{name}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect();
        duplicates.sort_unstable();

        if duplicates.is_empty() {
            Ok(())
        } else {
            bail!("{}", duplicates.join("; "))
        }
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
    fn validate_rejects_duplicate_ports() {
        let config: Config = serde_json::from_str(
            r#"{
            "first": {"path": "/tmp", "cmd": "true", "port": 3000},
            "second": {"path": "/tmp", "cmd": "true", "port": 3000},
            "third": {"path": "/tmp", "cmd": "true", "port": 4000}
        }"#,
        )
        .unwrap();

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("duplicate port 3000"), "error was: {error}");
        assert!(error.contains("'first'"), "error was: {error}");
        assert!(error.contains("'second'"), "error was: {error}");
        assert!(!error.contains("'third'"), "error was: {error}");
    }

    #[test]
    fn validate_lists_all_duplicated_ports() {
        let config: Config = serde_json::from_str(
            r#"{
            "a": {"path": "/tmp", "cmd": "true", "port": 3000},
            "b": {"path": "/tmp", "cmd": "true", "port": 3000},
            "c": {"path": "/tmp", "cmd": "true", "port": 4000},
            "d": {"path": "/tmp", "cmd": "true", "port": 4000}
        }"#,
        )
        .unwrap();

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("port 3000"), "error was: {error}");
        assert!(error.contains("port 4000"), "error was: {error}");
    }

    #[test]
    fn validate_accepts_unique_ports() {
        let config: Config = serde_json::from_str(
            r#"{
            "a": {"path": "/tmp", "cmd": "true", "port": 3000},
            "b": {"path": "/tmp", "cmd": "true", "port": 4000},
            "c": {"path": "/tmp", "cmd": "true"}
        }"#,
        )
        .unwrap();

        assert!(config.validate().is_ok());
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
