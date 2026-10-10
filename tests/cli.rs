#![cfg(unix)]

use std::cell::RefCell;
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
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
    /// Stands in for the running caddy's admin endpoint: devhub treats
    /// caddy as up while this accepts connections.
    caddy_admin: RefCell<Option<TcpListener>>,
    caddy_admin_port: u16,
    /// Gateway port for these tests, so they never look at the machine's
    /// real port 80.
    http_port: u16,
}

impl TestEnv {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let bin_dir = home.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        // Records every invocation and fails while `$HOME/caddy.fail` exists,
        // printing what a real caddy would when port 80 is taken. Like a real
        // caddy, `reload` fails while no server is running (`caddy.down`).
        write_executable(
            &bin_dir,
            "caddy",
            r#"#!/bin/sh
echo "$*" >> "$HOME/caddy.calls"
if [ "$1" = reload ] && [ -f "$HOME/caddy.down" ]; then
    echo "Error: sending configuration to instance: connect: connection refused" >&2
    exit 1
fi
if [ -f "$HOME/caddy.fail" ]; then
    echo "Error: loading new config: listen tcp :80: bind: address already in use" >&2
    exit 1
fi
echo "fake caddy: $1 ok"
exit 0
"#,
        );

        let path = match std::env::var("PATH") {
            Ok(current) if !current.is_empty() => format!("{}:{current}", bin_dir.display()),
            _ => bin_dir.display().to_string(),
        };

        let caddy_admin = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let caddy_admin_port = caddy_admin.local_addr().unwrap().port();

        Self {
            home,
            project_dir,
            path,
            caddy_admin: RefCell::new(Some(caddy_admin)),
            caddy_admin_port,
            http_port: reserve_port(),
        }
    }

    /// Make caddy look stopped, as after a crash: its admin endpoint stops
    /// answering and `reload` fails until something starts it again.
    fn crash_caddy(&self) {
        self.caddy_admin.borrow_mut().take();
        fs::write(self.home.path().join("caddy.down"), "").unwrap();
    }

    fn caddy_log(&self) -> String {
        fs::read_to_string(self.devhub_dir().join("caddy.log")).unwrap_or_default()
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

    fn caddy_calls(&self) -> Vec<String> {
        fs::read_to_string(self.home.path().join("caddy.calls"))
            .map(|calls| calls.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn clear_caddy_calls(&self) {
        let _ = fs::remove_file(self.home.path().join("caddy.calls"));
    }

    fn set_caddy_failing(&self, failing: bool) {
        let flag = self.home.path().join("caddy.fail");
        if failing {
            fs::write(flag, "").unwrap();
        } else {
            let _ = fs::remove_file(flag);
        }
    }

    fn read_state(&self) -> Option<Value> {
        let content = fs::read_to_string(self.state_path()).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devhub"));
        command
            .args(args)
            .env("HOME", self.home.path())
            .env("PATH", &self.path)
            .env("DEVHUB_CADDY_ADMIN_PORT", self.caddy_admin_port.to_string())
            .env("DEVHUB_HTTP_PORT", self.http_port.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> CommandOutput {
        run_command(self.command(args))
    }

    /// Run devhub in the background; collect it with `wait_child`.
    fn spawn(&self, args: &[&str]) -> Child {
        self.command(args).spawn().unwrap()
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
"#,
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
    assert!(start.stdout.contains(&format!("tcp localhost:{port}")));
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
    assert!(status
        .stdout
        .contains(&format!("http://tcp-app.localhost:{} (", env.http_port)));

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
fn duplicate_ports_in_config_are_rejected() {
    let env = TestEnv::new();

    env.write_config(json!({
        "first": {
            "path": env.project_path().display().to_string(),
            "cmd": "sleep 30",
            "port": 4500
        },
        "second": {
            "path": env.project_path().display().to_string(),
            "cmd": "sleep 30",
            "port": 4500
        }
    }));

    let list = env.run(&["list"]);
    assert!(
        !list.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        list.stdout,
        list.stderr
    );
    assert!(list.stderr.contains("duplicate port 4500"), "{}", list.stderr);
    assert!(list.stderr.contains("'first'"), "{}", list.stderr);
    assert!(list.stderr.contains("'second'"), "{}", list.stderr);
}

#[test]
fn start_fails_when_foreign_process_holds_the_port() {
    let env = TestEnv::new();
    // Listener owned by the test process, i.e. outside any project group
    // devhub spawns.
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    env.write_config(json!({
        "victim": {
            "path": env.project_path().display().to_string(),
            "cmd": "sleep 30",
            "port": port,
            "startup_timeout_ms": 5000
        }
    }));

    let start = env.run(&["start", "victim"]);
    assert!(
        !start.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        start.stdout,
        start.stderr
    );
    assert!(
        start.stderr.contains(&format!("port {port} is held by pid")),
        "{}",
        start.stderr
    );

    // The squatted port must not be recorded as a running project, and no
    // proxy route may be created for it.
    let state = env.read_state().unwrap_or_else(|| json!({}));
    assert_eq!(state, json!({}));
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

#[test]
fn restart_running_project() {
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
    let old_pid = state_pid(&env, "worth");

    // Reset the readiness marker so the restart exercises a full boot.
    fs::remove_file(env.project_path().join("ready")).unwrap();

    let restart = env.run(&["restart", "worth"]);
    assert!(
        restart.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        restart.stdout,
        restart.stderr
    );
    assert!(restart.stdout.contains("Restarting 'worth'..."));
    assert!(
        restart
            .stdout
            .contains(&format!("stopped previous run (pid {old_pid})"))
    );
    assert!(restart.stdout.contains("readiness:"));
    assert!(restart.stdout.contains("Restarted."));

    let new_pid = state_pid(&env, "worth");
    assert_ne!(new_pid, old_pid);

    // The old process group is really gone, not just forgotten in state.
    unsafe {
        assert_eq!(
            libc::kill(-(old_pid as i32), 0),
            -1,
            "old process group should be gone"
        );
    }

    let status = env.run(&["status"]);
    assert!(
        status.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        status.stdout,
        status.stderr
    );
    assert!(status.stdout.contains("running"));
}

#[test]
fn restart_stopped_project_starts_it() {
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

    let restart = env.run(&["restart", "worth"]);
    assert!(
        restart.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        restart.stdout,
        restart.stderr
    );
    assert!(restart.stdout.contains("not running — starting fresh"));
    assert!(restart.stdout.contains("Restarted."));

    let state = env.read_state().unwrap();
    assert!(state.get("worth").is_some(), "state: {state}");
}

#[test]
fn restart_shows_leftover_log_from_crashed_run() {
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

    // What devhub sees the morning after an overnight crash: a state entry
    // whose process is long gone, and the log the crash left behind.
    fs::create_dir_all(env.devhub_dir().join("logs")).unwrap();
    fs::write(
        env.state_path(),
        json!({
            "worth": {
                "pid": 99999999,
                "started_at": "2026-01-01T00:00:00Z",
                "port": null
            }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(env.log_path("worth"), "booting\nfatal: database locked\n").unwrap();

    let restart = env.run(&["restart", "worth"]);
    assert!(
        restart.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        restart.stdout,
        restart.stderr
    );
    assert!(restart.stdout.contains("previous run ended on its own"));
    assert!(restart.stdout.contains("fatal: database locked"));
    assert!(restart.stdout.contains("Restarted."));

    // The leftover log was replaced by the new run's log.
    let log = fs::read_to_string(env.log_path("worth")).unwrap();
    assert!(log.contains("booting"));
    assert!(!log.contains("database locked"));
}

#[test]
fn restart_with_missing_directory_spares_running_process() {
    let env = TestEnv::new();
    write_executable(
        env.project_path(),
        "sleeper.sh",
        r#"#!/bin/sh
exec sleep 30
"#,
    );

    env.write_config(json!({
        "worth": {
            "path": env.project_path().display().to_string(),
            "cmd": "./sleeper.sh"
        }
    }));

    let start = env.run(&["start", "worth"]);
    assert!(
        start.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        start.stdout,
        start.stderr
    );
    let pid = state_pid(&env, "worth");

    // The project directory is moved away while the project runs; the
    // restart must fail before stopping anything.
    env.write_config(json!({
        "worth": {
            "path": format!("{}/gone", env.project_path().display()),
            "cmd": "./sleeper.sh"
        }
    }));

    let restart = env.run(&["restart", "worth"]);
    assert!(
        !restart.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        restart.stdout,
        restart.stderr
    );
    assert!(
        restart.stderr.contains("does not exist"),
        "stderr: {}",
        restart.stderr
    );

    // The running process was not collateral damage.
    let state = env.read_state().unwrap();
    assert_eq!(state.get("worth").unwrap().get("pid").unwrap(), pid);
    unsafe {
        assert_eq!(libc::kill(pid as i32, 0), 0, "process should be alive");
    }
}

#[test]
fn failed_restart_leaves_project_stopped_and_drops_route() {
    let env = TestEnv::new();
    let port = reserve_port();

    fs::write(
        env.project_path().join("tcp_service.py"),
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
"#,
    )
    .unwrap();

    env.write_config(json!({
        "api": {
            "path": env.project_path().display().to_string(),
            "cmd": format!("python3 -u ./tcp_service.py {port}"),
            "port": port,
            "startup_timeout_ms": 8000
        }
    }));

    let start = env.run(&["start", "api"]);
    assert!(
        start.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        start.stdout,
        start.stderr
    );
    assert!(env.caddyfile_path().exists());

    // The project now crashes on boot.
    write_executable(
        env.project_path(),
        "broken.sh",
        r#"#!/bin/sh
echo "booting"
echo "fatal: boom" >&2
exit 7
"#,
    );
    env.write_config(json!({
        "api": {
            "path": env.project_path().display().to_string(),
            "cmd": "./broken.sh",
            "port": port,
            "startup_timeout_ms": 3000
        }
    }));

    let restart = env.run(&["restart", "api"]);
    assert!(
        !restart.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        restart.stdout,
        restart.stderr
    );
    assert!(
        restart.stderr.contains("restart failed"),
        "stderr: {}",
        restart.stderr
    );
    assert!(
        restart.stderr.contains("now stopped"),
        "stderr: {}",
        restart.stderr
    );
    assert!(
        restart.stderr.contains("fatal: boom"),
        "stderr: {}",
        restart.stderr
    );

    let state = env.read_state().unwrap_or_else(|| json!({}));
    assert_eq!(state, json!({}));
    assert!(!env.log_path("api").exists());
    assert!(
        !env.caddyfile_path().exists(),
        "route must be dropped when a restart leaves the project stopped"
    );
}

#[test]
fn concurrent_starts_keep_both_projects_tracked() {
    let env = TestEnv::new();
    env.write_config(json!({
        "first": {
            "path": env.project_path().display().to_string(),
            "cmd": "exec sleep 30",
            "ready_cmd": "sleep 0.5"
        },
        "second": {
            "path": env.project_path().display().to_string(),
            "cmd": "exec sleep 30",
            "ready_cmd": "sleep 0.8"
        }
    }));

    // Both commands load the state before either writes it back.
    let first = env.spawn(&["start", "first"]);
    let second = env.spawn(&["start", "second"]);
    assert_success(&wait_child(first));
    assert_success(&wait_child(second));

    let state = env.read_state().unwrap();
    assert!(state.get("first").is_some(), "state: {state}");
    assert!(state.get("second").is_some(), "state: {state}");
}

#[test]
fn interrupted_start_stops_the_project_it_spawned() {
    let env = TestEnv::new();
    let pidfile = env.project_path().join("pid");
    env.write_config(json!({
        "slow": {
            "path": env.project_path().display().to_string(),
            "cmd": "echo $$ > pid; exec sleep 30",
            "startup_timeout_ms": 20000,
            "ready_cmd": "false"
        }
    }));

    let start = env.spawn(&["start", "slow"]);
    wait_until("the project to be recorded", || {
        pidfile.exists() && env.read_state().is_some_and(|state| state.get("slow").is_some())
    });

    // Tracked while still waiting for readiness, so nothing is lost if
    // devhub dies here.
    let status = env.run(&["status"]);
    assert!(status.stdout.contains("starting"), "stdout: {}", status.stdout);

    unsafe {
        libc::kill(start.id() as i32, libc::SIGINT);
    }
    let start = wait_child(start);
    assert!(!start.status.success());
    assert!(
        start.stderr.contains("startup interrupted"),
        "stderr: {}",
        start.stderr
    );
    assert_eq!(env.read_state().unwrap(), json!({}));

    let group: i32 = fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    wait_until("the project group to disappear", || unsafe {
        libc::kill(-group, 0) != 0
    });
}

#[test]
fn project_does_not_inherit_devhub_stdin() {
    let env = TestEnv::new();
    let log = env.log_path("reader");
    env.write_config(json!({
        "reader": {
            "path": env.project_path().display().to_string(),
            "cmd": "read line; echo \"read-exit=$?\"; exec sleep 30",
            "startup_timeout_ms": 5000,
            "ready_cmd": format!("grep -q read-exit= {}", log.display())
        }
    }));

    // devhub's stdin stays open without data, like an idle terminal; a
    // project reading an inherited stdin would block forever.
    let mut command = env.command(&["start", "reader"]);
    command.stdin(Stdio::piped());
    assert_success(&run_command(command));
}

#[test]
fn start_accepts_ipv6_only_listener() {
    if TcpListener::bind(("::1", 0)).is_err() {
        return; // no IPv6 loopback on this host
    }
    let env = TestEnv::new();
    let port = reserve_port();
    env.write_config(json!({
        "v6": {
            "path": env.project_path().display().to_string(),
            "cmd": http_server_cmd(port, "::1"),
            "port": port,
            "startup_timeout_ms": 8000
        }
    }));

    let start = env.run(&["start", "v6"]);
    assert_success(&start);
    assert!(start.stdout.contains("Started."));
}

#[test]
fn unchanged_routes_do_not_reload_caddy() {
    let env = TestEnv::new();
    let (beta_port, alpha_port) = (reserve_port(), reserve_port());
    env.write_config(json!({
        "beta": {
            "path": env.project_path().display().to_string(),
            "cmd": http_server_cmd(beta_port, "127.0.0.1"),
            "port": beta_port,
            "startup_timeout_ms": 8000
        },
        "alpha": {
            "path": env.project_path().display().to_string(),
            "cmd": http_server_cmd(alpha_port, "127.0.0.1"),
            "port": alpha_port,
            "startup_timeout_ms": 8000
        }
    }));
    assert_success(&env.run(&["start", "beta"]));
    assert_success(&env.run(&["start", "alpha"]));

    env.clear_caddy_calls();
    for _ in 0..5 {
        let status = env.run(&["status"]);
        assert_success(&status);
        let alpha = status.stdout.find("alpha").unwrap();
        let beta = status.stdout.find("beta").unwrap();
        assert!(alpha < beta, "projects should be listed by name:\n{}", status.stdout);
    }
    assert_eq!(env.caddy_calls(), Vec::<String>::new());
}

#[test]
fn failed_caddy_apply_is_retried_by_the_next_command() {
    let env = TestEnv::new();
    let port = reserve_port();
    env.write_config(json!({
        "web": {
            "path": env.project_path().display().to_string(),
            "cmd": http_server_cmd(port, "127.0.0.1"),
            "port": port,
            "startup_timeout_ms": 8000
        }
    }));

    env.set_caddy_failing(true);
    let start = env.run(&["start", "web"]);
    assert_success(&start);
    assert!(start.stderr.contains("caddy reload failed"), "stderr: {}", start.stderr);

    env.set_caddy_failing(false);
    env.clear_caddy_calls();
    assert_success(&env.run(&["status"]));
    assert!(
        !env.caddy_calls().is_empty(),
        "the route that failed to apply must be retried"
    );
    assert!(env.caddyfile_path().exists());
}

#[test]
fn stop_works_while_config_is_broken() {
    let env = TestEnv::new();
    env.write_config(json!({
        "app": {
            "path": env.project_path().display().to_string(),
            "cmd": "exec sleep 30"
        }
    }));
    assert_success(&env.run(&["start", "app"]));

    fs::write(env.proj_path(), "{ \"app\": { mid-edit").unwrap();
    assert_success(&env.run(&["stop", "app"]));
    assert_eq!(env.read_state().unwrap(), json!({}));
}

#[test]
fn list_without_config_explains_setup() {
    let env = TestEnv::new();
    let list = env.run(&["list"]);
    assert_success(&list);
    assert!(list.stdout.contains("No projects configured."));
    assert!(list.stdout.contains("proj.json"));
}

#[test]
fn crashed_caddy_is_restarted_by_the_next_command() {
    let env = TestEnv::new();
    let port = reserve_port();
    env.write_config(json!({
        "web": {
            "path": env.project_path().display().to_string(),
            "cmd": http_server_cmd(port, "127.0.0.1"),
            "port": port,
            "startup_timeout_ms": 8000
        }
    }));
    assert_success(&env.run(&["start", "web"]));

    // The Caddyfile on disk is still current, but nothing serves it.
    env.crash_caddy();
    env.clear_caddy_calls();
    assert_success(&env.run(&["status"]));
    assert!(
        env.caddy_calls().iter().any(|call| call.starts_with("start")),
        "caddy calls: {:?}",
        env.caddy_calls()
    );
}

#[test]
fn caddy_output_is_logged_and_quoted_when_it_fails_to_start() {
    let env = TestEnv::new();
    let (first, second) = (reserve_port(), reserve_port());
    env.write_config(json!({
        "first": {
            "path": env.project_path().display().to_string(),
            "cmd": http_server_cmd(first, "127.0.0.1"),
            "port": first,
            "startup_timeout_ms": 8000
        },
        "second": {
            "path": env.project_path().display().to_string(),
            "cmd": http_server_cmd(second, "127.0.0.1"),
            "port": second,
            "startup_timeout_ms": 8000
        }
    }));

    env.crash_caddy(); // force `caddy start` rather than a reload
    assert_success(&env.run(&["start", "first"]));
    assert!(env.caddy_log().contains("fake caddy: start ok"), "caddy.log: {}", env.caddy_log());

    env.set_caddy_failing(true);
    let start = env.run(&["start", "second"]);
    assert_success(&start);
    assert!(
        start.stderr.contains("address already in use"),
        "stderr: {}",
        start.stderr
    );
    assert!(
        start.stderr.contains(&format!("port {}", env.http_port)),
        "stderr: {}",
        start.stderr
    );
}

#[test]
fn stop_lets_a_project_finish_its_shutdown() {
    let env = TestEnv::new();
    env.write_config(json!({
        "app": {
            "path": env.project_path().display().to_string(),
            // Cleanup takes 300ms after SIGTERM, longer than the old fixed
            // 100ms before SIGKILL.
            "cmd": "trap 'sleep 0.3; touch stopped-cleanly; exit 0' TERM; while :; do sleep 0.05; done"
        }
    }));
    assert_success(&env.run(&["start", "app"]));

    let started = Instant::now();
    assert_success(&env.run(&["stop", "app"]));
    assert!(env.project_path().join("stopped-cleanly").exists());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "stop should return once the project exits, not after the full grace period"
    );
}

#[test]
fn stop_kills_a_project_that_ignores_sigterm_after_its_stop_timeout() {
    let env = TestEnv::new();
    env.write_config(json!({
        "stubborn": {
            "path": env.project_path().display().to_string(),
            "cmd": "trap '' TERM; exec sleep 30",
            "stop_timeout_ms": 500
        }
    }));
    assert_success(&env.run(&["start", "stubborn"]));
    let group = state_pid(&env, "stubborn") as i32;

    let started = Instant::now();
    assert_success(&env.run(&["stop", "stubborn"]));
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(500), "SIGKILL came early: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "stop took {elapsed:?}");
    wait_until("the group to disappear", || unsafe { libc::kill(-group, 0) != 0 });
}

#[test]
fn port_owner_check_can_be_turned_off() {
    let env = TestEnv::new();
    // Held outside the project's process group, like Docker's port forwarder.
    let forwarder = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = forwarder.local_addr().unwrap().port();
    env.write_config(json!({
        "dockerized": {
            "path": env.project_path().display().to_string(),
            "cmd": "exec sleep 30",
            "port": port,
            "startup_timeout_ms": 5000,
            "verify_port_owner": false
        }
    }));

    let start = env.run(&["start", "dockerized"]);
    assert_success(&start);
    assert!(
        start.stdout.contains("listener ownership not verified"),
        "stdout: {}",
        start.stdout
    );
}

/// Something besides devhub's caddy listening on the gateway port, whose
/// command line carries `marker`.
fn occupy_gateway(env: &TestEnv, marker: &str) -> Child {
    let squatter = Command::new("python3")
        .args([
            "-c",
            "import socket, sys, time\n\
             s = socket.socket()\n\
             s.bind(('127.0.0.1', int(sys.argv[1])))\n\
             s.listen()\n\
             time.sleep(60)",
            &env.http_port.to_string(),
            marker,
        ])
        .spawn()
        .unwrap();
    wait_until("the gateway port to be taken", || {
        std::net::TcpStream::connect(("127.0.0.1", env.http_port)).is_ok()
    });
    squatter
}

#[test]
fn another_listener_on_the_gateway_port_is_reported() {
    let env = TestEnv::new();
    let port = reserve_port();
    env.write_config(json!({
        "web": {
            "path": env.project_path().display().to_string(),
            "cmd": http_server_cmd(port, "127.0.0.1"),
            "port": port,
            "startup_timeout_ms": 8000
        }
    }));
    // The admin stand-in belongs to this test process, the squatter does not.
    let mut squatter = occupy_gateway(&env, "some-other-server");

    let start = env.run(&["start", "web"]);
    squatter.kill().unwrap();
    squatter.wait().unwrap();

    assert_success(&start);
    assert!(
        start.stderr.contains(&format!("port {} is also held by pid {}", env.http_port, squatter.id())),
        "stderr: {}",
        start.stderr
    );
    // Not applied, so the next command retries.
    assert!(!env.caddyfile_path().exists());
}

#[test]
fn caddy_left_by_an_earlier_devhub_is_named_with_the_fix() {
    let env = TestEnv::new();
    let port = reserve_port();
    env.write_config(json!({
        "web": {
            "path": env.project_path().display().to_string(),
            "cmd": http_server_cmd(port, "127.0.0.1"),
            "port": port,
            "startup_timeout_ms": 8000
        }
    }));
    // Started from devhub's own Caddyfile, but not on devhub's admin port.
    let caddyfile = env.caddyfile_path().display().to_string();
    let mut legacy = occupy_gateway(&env, &caddyfile);

    let start = env.run(&["start", "web"]);
    legacy.kill().unwrap();
    legacy.wait().unwrap();

    assert!(
        start.stderr.contains("a caddy that an earlier devhub version started")
            && start.stderr.contains("caddy stop"),
        "stderr: {}",
        start.stderr
    );
}

fn state_pid(env: &TestEnv, name: &str) -> u64 {
    let state = env.read_state().unwrap_or_else(|| json!({}));
    state
        .get(name)
        .unwrap_or_else(|| panic!("no state entry for '{name}': {state}"))
        .get("pid")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("no pid in state entry for '{name}': {state}"))
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
    wait_child(command.spawn().unwrap())
}

fn wait_child(mut child: Child) -> CommandOutput {
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

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn http_server_cmd(port: u16, bind: &str) -> String {
    format!("exec python3 -m http.server {port} --bind {bind}")
}

fn assert_success(output: &CommandOutput) {
    assert!(
        output.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        output.stdout,
        output.stderr
    );
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
