use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use eyre::WrapErr;

use crate::dirs;
use crate::state::AppState;

const LOG_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const LOG_TAIL_MAX_BYTES: u64 = 8 * 1024;
const LOG_TAIL_MAX_LINES: usize = 40;

pub fn project_log_path(name: &str) -> eyre::Result<PathBuf> {
    Ok(dirs::logs_dir()?.join(log_file_name(name)))
}

pub fn create_project_log(name: &str) -> eyre::Result<(PathBuf, File)> {
    let path = project_log_path(name)?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .with_context(|| format!("failed to open log file {}", path.display()))?;
    Ok((path, file))
}

pub fn remove_project_log(name: &str) -> eyre::Result<()> {
    remove_if_exists(&project_log_path(name)?)
}

pub fn read_project_log_tail(name: &str) -> eyre::Result<Option<String>> {
    read_log_tail(&project_log_path(name)?)
}

pub fn cleanup_outdated_logs(state: &AppState) -> eyre::Result<usize> {
    let active_logs: HashSet<String> = state
        .processes
        .keys()
        .map(|name| log_file_name(name))
        .collect();
    cleanup_outdated_logs_in_dir(&dirs::logs_dir()?, &active_logs, SystemTime::now())
}

fn cleanup_outdated_logs_in_dir(
    log_dir: &Path,
    active_logs: &HashSet<String>,
    now: SystemTime,
) -> eyre::Result<usize> {
    if !log_dir.exists() {
        return Ok(0);
    }

    let mut removed = 0;
    for entry in std::fs::read_dir(log_dir)
        .with_context(|| format!("failed to read log directory {}", log_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            continue;
        }

        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.ends_with(".log") || active_logs.contains(file_name.as_ref()) {
            continue;
        }

        let modified = match entry.metadata().and_then(|metadata| metadata.modified()) {
            Ok(modified) => modified,
            Err(_) => continue,
        };

        let age = match now.duration_since(modified) {
            Ok(age) => age,
            Err(_) => continue,
        };

        if age >= LOG_RETENTION {
            remove_if_exists(&path)?;
            removed += 1;
        }
    }

    Ok(removed)
}

fn read_log_tail(path: &Path) -> eyre::Result<Option<String>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to open log file {}", path.display()));
        }
    };

    let size = file.metadata()?.len();
    if size == 0 {
        return Ok(None);
    }

    let start = size.saturating_sub(LOG_TAIL_MAX_BYTES);
    file.seek(SeekFrom::Start(start))?;

    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    if start > 0 {
        if let Some(pos) = buf.iter().position(|byte| *byte == b'\n') {
            buf.drain(..=pos);
        }
    }

    let content = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = content.lines().collect();
    let start_line = lines.len().saturating_sub(LOG_TAIL_MAX_LINES);
    let tail = lines[start_line..].join("\n");

    if tail.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(tail))
    }
}

fn remove_if_exists(path: &Path) -> eyre::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("failed to remove log file {}", path.display()))
        }
    }
}

fn log_file_name(name: &str) -> String {
    let mut sanitized = String::with_capacity(name.len() + 4);
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    sanitized.push_str(".log");
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_outdated_logs_removes_only_inactive_old_logs() {
        let tempdir = tempfile::tempdir().unwrap();
        let active = tempdir.path().join("active.log");
        let stale = tempdir.path().join("stale.log");
        let fresh = tempdir.path().join("fresh.log");

        fs::write(&active, "active").unwrap();
        fs::write(&stale, "stale").unwrap();
        fs::write(&fresh, "fresh").unwrap();

        let now = SystemTime::now();
        let stale_time = now - (LOG_RETENTION + Duration::from_secs(1));
        let fresh_time = now - Duration::from_secs(60);

        set_file_mtime(&active, stale_time);
        set_file_mtime(&stale, stale_time);
        set_file_mtime(&fresh, fresh_time);

        let removed = cleanup_outdated_logs_in_dir(
            tempdir.path(),
            &HashSet::from([String::from("active.log")]),
            now,
        )
        .unwrap();

        assert_eq!(removed, 1);
        assert!(active.exists());
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn read_log_tail_returns_last_lines() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("service.log");
        let content = (0..60)
            .map(|idx| format!("line-{idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, content).unwrap();

        let tail = read_log_tail(&path).unwrap().unwrap();
        assert!(tail.contains("line-59"));
        assert!(!tail.contains("line-0"));
    }

    #[cfg(target_os = "macos")]
    fn set_file_mtime(path: &Path, time: SystemTime) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let duration = time.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let times = [
            libc::timespec {
                tv_sec: duration.as_secs() as i64,
                tv_nsec: duration.subsec_nanos() as i64,
            },
            libc::timespec {
                tv_sec: duration.as_secs() as i64,
                tv_nsec: duration.subsec_nanos() as i64,
            },
        ];
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let ret = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(ret, 0, "failed to update mtime");
    }

    #[cfg(not(target_os = "macos"))]
    fn set_file_mtime(path: &Path, time: SystemTime) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let duration = time.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let times = [
            libc::timespec {
                tv_sec: duration.as_secs() as i64,
                tv_nsec: duration.subsec_nanos() as i64,
            },
            libc::timespec {
                tv_sec: duration.as_secs() as i64,
                tv_nsec: duration.subsec_nanos() as i64,
            },
        ];
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let ret = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(ret, 0, "failed to update mtime");
    }
}
