use std::collections::{BTreeMap, HashMap};

use eyre::{WrapErr, bail};
use serde::{Deserialize, Serialize};

use crate::dirs;

fn default_startup_timeout_ms() -> u64 {
    60_000
}

/// Grace period between SIGTERM and SIGKILL when stopping a project.
pub const DEFAULT_STOP_TIMEOUT_MS: u64 = 5_000;

fn default_stop_timeout_ms() -> u64 {
    DEFAULT_STOP_TIMEOUT_MS
}

fn verify_by_default() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// A misspelled field (`ready_command`) would otherwise be silently ignored.
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub path: String,
    pub cmd: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default = "default_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    #[serde(default)]
    pub ready_cmd: Option<String>,
    #[serde(default = "default_stop_timeout_ms")]
    pub stop_timeout_ms: u64,
    /// Whether port readiness requires the listener to belong to the
    /// project's own process group. Projects whose port is held by another
    /// process tree, such as Docker's port forwarder, turn this off.
    #[serde(default = "verify_by_default")]
    pub verify_port_owner: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub projects: BTreeMap<String, ProjectConfig>,
}

impl Config {
    pub fn load() -> eyre::Result<Self> {
        let path = dirs::proj_path()?;
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            // No config yet means no projects, not a broken setup.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to read config from {}", path.display()));
            }
        };
        let config: Config = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse config from {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> eyre::Result<()> {
        let mut problems = self.invalid_hostnames();
        problems.extend(self.duplicate_ports());

        if problems.is_empty() {
            Ok(())
        } else {
            bail!("{}", problems.join("; "))
        }
    }

    /// A project with a port is served at `http://<name>.localhost`, so its
    /// name must be a valid DNS label; anything else breaks the generated
    /// Caddyfile (a space even splits it into two site addresses).
    fn invalid_hostnames(&self) -> Vec<String> {
        self.projects
            .iter()
            .filter(|(name, project)| project.port.is_some() && !is_dns_label(name))
            .map(|(name, _)| {
                format!(
                    "project '{name}' has a port, so its name becomes the hostname \
                     '{name}.localhost'; use only lowercase letters, digits and inner '-'"
                )
            })
            .collect()
    }

    /// Readiness probing and Caddy routing both target `port`, so two projects
    /// declaring the same port would let one project's listener satisfy the
    /// other's readiness probe and claim its proxy route.
    fn duplicate_ports(&self) -> Vec<String> {
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
        duplicates
    }
}

fn is_dns_label(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
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
    fn unknown_fields_are_rejected() {
        let error = serde_json::from_str::<Config>(
            r#"{"app": {"path": "/tmp", "cmd": "true", "ready_command": "true"}}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown field `ready_command`"), "error was: {error}");
    }

    #[test]
    fn stop_and_owner_settings_have_defaults() {
        let config: Config =
            serde_json::from_str(r#"{"app": {"path": "/tmp", "cmd": "true"}}"#).unwrap();
        assert_eq!(config.projects["app"].stop_timeout_ms, DEFAULT_STOP_TIMEOUT_MS);
        assert!(config.projects["app"].verify_port_owner);
    }

    #[test]
    fn validate_rejects_names_that_are_not_hostnames() {
        let config: Config = serde_json::from_str(
            r#"{
            "My App": {"path": "/tmp", "cmd": "true", "port": 3000},
            "my_app": {"path": "/tmp", "cmd": "true", "port": 3001},
            "-app": {"path": "/tmp", "cmd": "true", "port": 3002}
        }"#,
        )
        .unwrap();

        let error = config.validate().unwrap_err().to_string();
        for name in ["My App", "my_app", "-app"] {
            assert!(error.contains(&format!("'{name}'")), "error was: {error}");
        }
    }

    #[test]
    fn validate_allows_any_name_without_a_port() {
        let config: Config = serde_json::from_str(
            r#"{
            "Tauri Demo": {"path": "/tmp", "cmd": "true"},
            "worth-2": {"path": "/tmp", "cmd": "true", "port": 3000}
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
