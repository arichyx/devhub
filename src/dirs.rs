use std::path::PathBuf;

use eyre::{ContextCompat, WrapErr};

const DEVHUB_DIR: &str = ".devhub";

pub fn devhub_dir() -> eyre::Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let dir = home.join(DEVHUB_DIR);
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    Ok(dir)
}

pub fn proj_path() -> eyre::Result<PathBuf> {
    Ok(devhub_dir()?.join("proj.json"))
}

pub fn state_path() -> eyre::Result<PathBuf> {
    Ok(devhub_dir()?.join("state.json"))
}

pub fn caddyfile_path() -> eyre::Result<PathBuf> {
    Ok(devhub_dir()?.join("Caddyfile"))
}
