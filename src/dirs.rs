#[cfg(test)]
use std::cell::RefCell;
use std::path::PathBuf;

use eyre::{ContextCompat, WrapErr};

const DEVHUB_DIR: &str = ".devhub";

#[cfg(test)]
thread_local! {
    static TEST_DEVHUB_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub fn devhub_dir() -> eyre::Result<PathBuf> {
    let dir = resolved_devhub_dir()?;
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    Ok(dir)
}

fn resolved_devhub_dir() -> eyre::Result<PathBuf> {
    #[cfg(test)]
    if let Some(dir) = TEST_DEVHUB_DIR.with(|slot| slot.borrow().clone()) {
        return Ok(dir);
    }

    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(DEVHUB_DIR))
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

pub fn logs_dir() -> eyre::Result<PathBuf> {
    let dir = devhub_dir()?.join("logs");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    Ok(dir)
}

#[cfg(test)]
pub(crate) fn with_test_devhub_dir<T>(dir: PathBuf, f: impl FnOnce() -> T) -> T {
    TEST_DEVHUB_DIR.with(|slot| {
        let previous = slot.replace(Some(dir));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        slot.replace(previous);
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    })
}
