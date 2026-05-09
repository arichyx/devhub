#![cfg(unix)]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use tempfile::TempDir;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

struct CommandOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

struct TestEnv {
    home: TempDir,
    project_dir: TempDir,
    path: String,
}

impl TestEnv {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let bin_dir = home.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        write_executable(
            &bin_dir,
            "caddy",
            r#"#!/bin/sh
exit 0
"#,
        );

        let path = match std::env::var("PATH") {
            Ok(current) if !current.is_empty() => format!("{}:{current}", bin_dir.display()),
            _ => bin_dir.display().to_string(),
        };

        Self {
            home,
            project_dir,
            path,
        }
    }

    fn project_path(&self) -> &Path {
        self.project_dir.path()
    }

    fn devhub_dir(&self) -> PathBuf {
        self.home.path().join(".devhub")
    }

    fn proj_path(&self) -> PathBuf {
        self.devhub_dir().join("proj.json")
    }

    fn state_path(&self) -> PathBuf {
        self.devhub_dir().join("state.json")
    }

    fn log_path(&self, name: &str) -> PathBuf {
        self.devhub_dir().join("logs").join(format!("{name}.log"))
    }

    fn caddyfile_path(&self) -> PathBuf {
        self.devhub_dir().join("Caddyfile")
    }

    fn write_config(&self, config: Value) {
        fs::create_dir_all(self.devhub_dir()).unwrap();
        fs::write(
            self.proj_path(),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .unwrap();
    }

    fn read_state(&self) -> Option<Value> {
        let content = fs::read_to_string(self.state_path()).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn run(&self, args: &[&str]) -> CommandOutput {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devhub"));
        command
            .args(args)
            .env("HOME", self.home.path())
            .env("PATH", &self.path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        run_command(command)
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let Some(state) = self.read_state() else {
            return;
        };

        let Some(processes) = state.as_object() else {
            return;
        };

        for process in processes.values() {
            let Some(pid) = process.get("pid").and_then(Value::as_u64) else {
                continue;
            };
            let pid = pid as i32;
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

#[test]
fn start_and_stop_project_with_ready_cmd() {
    let env = TestEnv::new();
    write_executable(
        env.project_path(),
        "ready-service.sh",
        r#"#!/bin/sh
echo "booting"
sleep 1
touch ready
echo "ready"
exec sleep 30
"#,
    );

    env.write_config(json!({
        "worth": {
            "path": env.project_path().display().to_string(),
            "cmd": "./ready-service.sh",
            "startup_timeout_ms": 5000,
            "ready_cmd": "test -f ready"
        }
    }));

    let start = env.run(&["start", "worth"]);
    assert!(
        start.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        start.stdout,
        start.stderr
    );
    assert!(start.stdout.contains("Starting 'worth'..."));
    assert!(start.stdout.contains("readiness:"));
    assert!(start.stdout.contains("test -f ready"));
    assert!(start.stdout.contains("Started."));

    let state = env.read_state().unwrap();
    assert!(state.get("worth").is_some(), "state: {state}");

    let log = fs::read_to_string(env.log_path("worth")).unwrap();
    assert!(log.contains("booting"));
    assert!(log.contains("ready"));

    let status = env.run(&["status"]);
    assert!(
        status.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        status.stdout,
        status.stderr
    );
    assert!(status.stdout.contains("worth"));
    assert!(status.stdout.contains("running"));

    let stop = env.run(&["stop", "worth"]);
    assert!(
        stop.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        stop.stdout,
        stop.stderr
    );
    assert!(!env.log_path("worth").exists());

    let state_after = env.read_state().unwrap();
    assert_eq!(state_after, json!({}));
}

#[test]
fn failed_start_prints_log_tail_and_removes_log() {
    let env = TestEnv::new();
    write_executable(
        env.project_path(),
        "fail-service.sh",
        r#"#!/bin/sh
echo "booting"
echo "fatal: boom" >&2
exit 7
"#,
    );

    env.write_config(json!({
        "broken": {
            "path": env.project_path().display().to_string(),
            "cmd": "./fail-service.sh",
            "startup_timeout_ms": 3000,
            "ready_cmd": "test -f ready"
        }
    }));

    let start = env.run(&["start", "broken"]);
    assert!(
        !start.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        start.stdout,
        start.stderr
    );
    assert!(
        start
            .stderr
            .contains("project exited before becoming ready")
    );
    assert!(start.stderr.contains("Last log lines:"));
    assert!(start.stderr.contains("booting"));
    assert!(start.stderr.contains("fatal: boom"));
    assert!(!env.log_path("broken").exists());

    let state = env.read_state().unwrap_or_else(|| json!({}));
    assert_eq!(state, json!({}));
}

#[test]
fn start_uses_tcp_probe_when_only_port_is_configured() {
    let env = TestEnv::new();
    let port = reserve_port();

    fs::write(
        env.project_path().join("tcp_service.py"),
        format!(
            r#"import socket
import sys
import time

port = int(sys.argv[1])
print("booting", flush=True)
time.sleep(1)
sock = socket.socket()
sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
sock.bind(("127.0.0.1", port))
sock.listen()
print("listening", flush=True)
time.sleep(30)
"#
        ),
    )
    .unwrap();

    env.write_config(json!({
        "tcp-app": {
            "path": env.project_path().display().to_string(),
            "cmd": format!("python3 -u ./tcp_service.py {port}"),
            "port": port,
            "startup_timeout_ms": 8000
        }
    }));

    let start = env.run(&["start", "tcp-app"]);
    assert!(
        start.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        start.stdout,
        start.stderr
    );
    assert!(start.stdout.contains(&format!("tcp 127.0.0.1:{port}")));
    assert!(start.stdout.contains("Started."));
    assert!(env.caddyfile_path().exists());

    let status = env.run(&["status"]);
    assert!(
        status.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        status.stdout,
        status.stderr
    );
    assert!(status.stdout.contains("tcp-app"));
    assert!(status.stdout.contains("running"));
    assert!(status.stdout.contains("http://tcp-app.localhost:1300"));

    let stop = env.run(&["stop", "tcp-app"]);
    assert!(
        stop.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        stop.stdout,
        stop.stderr
    );
    assert!(!env.log_path("tcp-app").exists());
    assert!(!env.caddyfile_path().exists());
}

#[test]
fn stale_inactive_logs_are_cleaned_on_next_command() {
    let env = TestEnv::new();
    env.write_config(json!({}));

    let logs_dir = env.devhub_dir().join("logs");
    fs::create_dir_all(&logs_dir).unwrap();

    let stale = logs_dir.join("stale.log");
    let fresh = logs_dir.join("fresh.log");
    fs::write(&stale, "old").unwrap();
    fs::write(&fresh, "new").unwrap();

    let now = SystemTime::now();
    set_file_mtime(&stale, now - Duration::from_secs(24 * 60 * 60 + 5));
    set_file_mtime(&fresh, now - Duration::from_secs(60));

    let list = env.run(&["list"]);
    assert!(
        list.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        list.stdout,
        list.stderr
    );
    assert!(!stale.exists());
    assert!(fresh.exists());
}

fn write_executable(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, contents).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn reserve_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

fn run_command(mut command: Command) -> CommandOutput {
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + COMMAND_TIMEOUT;

    loop {
        if let Some(_status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            return CommandOutput {
                status: output.status,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            };
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "command timed out\nstdout:\n{}\n\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

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
