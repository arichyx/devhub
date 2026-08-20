# Startup Readiness and Failure Diagnostics in devhub

> We redefined startup success as “the service became ready,” then paired readiness probes with lifecycle-managed logs so failures stop being silent without turning `devhub` into a heavy supervisor.

## Problem

Originally, `devhub` treated `spawn()` success as startup success. That was too weak.

For long-running services, a successful `spawn()` only tells us that the OS launched a process. It does not tell us that:

- the service finished booting
- the service is actually accepting traffic
- the process did not exit immediately with an error

At the same time, startup failures were hard to understand because stdout and stderr were discarded. When a project failed during boot, `devhub` had very little to show beyond “it did not work.”

We needed a startup model where “success” means “the project became usable,” not “the launcher process existed briefly.”

## Why It's Hard

Startup correctness sounds simple until we look at real services.

Different projects have very different startup times. Some become ready in a few hundred milliseconds, while others spend tens of seconds recompiling, warming caches, or starting dependencies. That makes any fixed “observation window” brittle.

We also did not want to hard-code HTTP assumptions. Some projects expose HTTP endpoints, but others are Redis, Postgres, gRPC, or custom TCP services. A single health-check mechanism would not cover all of them.

At the same time, we wanted startup diagnostics without introducing a helper/shim architecture or permanent unbounded logs. `devhub` is still meant to be a simple CLI, not a resident supervisor with a complex buffering model.

Finally, process timing can race during early failure. If the launcher process exits quickly, the process group can briefly look different before the failure is fully visible. Startup logic has to treat those transitions carefully or it can misreport a fast failure as a timeout.

## Alternatives Considered

### Option A: Treat `spawn()` as success

- How it works: return success as soon as the child process is launched.
- Pros: simplest possible implementation and very fast CLI return.
- Cons: produces false positives, misses immediate startup crashes, and gives no meaningful guarantee that the service is usable.

### Option B: Fixed observation window

- How it works: wait a hardcoded 2s, 5s, or 10s after spawn and assume success if nothing obviously died.
- Pros: still simple and slightly better than raw `spawn()`.
- Cons: too brittle for slow compiles, large services, or first-boot workflows; the “right” duration is workload-specific and constantly wrong for somebody.

### Option C: Helper/Shim Process with In-Memory Buffers

- How it works: keep a long-lived helper attached to stdout/stderr, buffer output in memory, and continue supervising after `devhub start` exits.
- Pros: strong diagnostics and clean output handling without writing persistent logs.
- Cons: adds architectural complexity, creates another long-lived moving part, and pushes the project closer to a supervisor design we explicitly did not want.

### Option D: Readiness Probes + Per-Project Logs with Lifecycle Cleanup

- How it works: block `devhub start` until readiness succeeds, the project exits, or a timeout is hit; capture stdout/stderr in a project log and clean those logs by lifecycle rules.
- Pros: good startup guarantees, protocol-flexible readiness, and practical diagnostics with a simple operational model.
- Cons: startup now waits synchronously for readiness, and logs can still grow during one long-running session until the service stops.

## Solution

We adopted a simple precedence rule for readiness:

1. If `ready_cmd` is configured, use that.
2. Otherwise, if `port` is configured, use a TCP probe against `127.0.0.1:<port>`.
3. Otherwise, fall back to spawn-only behavior because we have no strong readiness signal.

One correctness rule sits on top of the port-based paths: a successful connect only proves that *something* is listening, not that it is *our* project. A stale server, a manually started dev server, or another project configured with the same port can satisfy the probe and silently receive the project's proxy route. So whenever `port` is configured, `devhub` also asks `lsof`/`ps` for the listener's process group and requires it to match the group it spawned — descendants inherit the group, so the real HTTP server matches even when the launcher already exited.

That logic lives directly in the startup path:

```rust
fn readiness_probe_passed(
    config: &ProjectConfig,
    project_dir: &Path,
    probe_timeout: Duration,
    group_id: u32,
) -> eyre::Result<ProbeOutcome> {
    if let Some(cmd) = &config.ready_cmd {
        if !exec_readiness_probe(cmd, project_dir, probe_timeout)? {
            return Ok(ProbeOutcome::NotYet);
        }
        // A ready_cmd cannot tell *who* answered; when a port is also
        // configured, require the listener to be ours before declaring ready.
        return Ok(match config.port {
            Some(port) => port_readiness(port, group_id),
            None => ProbeOutcome::Ready,
        });
    }

    if let Some(port) = config.port {
        return Ok(port_readiness(port, group_id));
    }

    Ok(ProbeOutcome::Ready)
}
```

When the listener turns out to be foreign, waiting is pointless — the project can never bind that port — so the start fails immediately, names the offending PID, and kills the group it just spawned:

```rust
match readiness_probe_passed(config, project_dir, probe_timeout, group_id)? {
    ProbeOutcome::Ready => return Ok(()),
    ProbeOutcome::NotYet => {}
    ProbeOutcome::ForeignListener { port, pid, command } => {
        if crate::state::is_process_group_alive(group_id) {
            let _ = kill_process_group(group_id);
        }
        return Err(startup_failure(
            name,
            format!(
                "port {port} is held by pid {pid} (`{command}`), which is not part of \
                 this project — another project or a manually started server may \
                 already be using it"
            ),
        ));
    }
}
```

`startup_timeout_ms` is not a success window. It is an upper bound on how long we are willing to wait for readiness before declaring the attempt failed. The main startup loop re-checks group liveness, the deadline, and the probe outcome on every iteration, and it does not report success until readiness passes — a child process that merely exists is not enough.

We also fixed the early-exit race by checking `child.try_wait()` before relying only on process-group liveness. That makes fast failures show up as “exited before becoming ready” instead of being misclassified as timeouts while the leader is still in transition.

Diagnostics come from simple per-project logs rather than a helper process:

```rust
pub fn create_project_log(name: &str) -> eyre::Result<(PathBuf, File)> {
    let path = project_log_path(name)?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    Ok((path, file))
}
```

When startup fails, we read the tail of that log, include it in the error, and immediately delete the file:

```rust
fn startup_failure(name: &str, message: String) -> eyre::Report {
    let tail = logs::read_project_log_tail(name).ok().flatten();
    let _ = logs::remove_project_log(name);

    if let Some(tail) = tail {
        eyre!("{message}\n\nLast log lines:\n{tail}")
    } else {
        eyre!("{message}")
    }
}
```

The rest of the log lifecycle is intentionally simple:

- create or truncate on `start`
- keep while the service is running
- delete on `stop`
- delete inactive old logs on later commands

That cleanup policy gives us useful failure evidence without accumulating historical logs forever:

```rust
pub fn cleanup_outdated_logs(state: &AppState) -> eyre::Result<usize> {
    let active_logs: HashSet<String> = state
        .processes
        .keys()
        .map(|name| log_file_name(name))
        .collect();

    cleanup_outdated_logs_in_dir(&dirs::logs_dir()?, &active_logs, SystemTime::now())
}
```

The result is a startup contract that is much closer to what users actually mean:

- success means the service became ready, and — when a `port` is configured — that the listener belongs to the project's own process group
- failure means the service exited early, timed out before readiness, or found its port already held by another process
- errors come with enough recent output to debug the failure quickly

## Key Takeaways

- Startup success should mean ready, not merely spawned.
- A successful connect proves someone is listening, not that your service is; verify listener ownership when a port is part of the contract.
- Readiness probing is more general than HTTP health checks.
- `startup_timeout_ms` should be treated as an upper bound, not a guessed success window.
- Log retention should be lifecycle-based if we want simple diagnostics without permanent accumulation.

## References

- `src/process.rs` — readiness precedence, startup wait loop, and failure reporting
- `src/logs.rs` — per-project log creation, tail reading, deletion, and stale-log cleanup
- `src/main.rs` — command-level cleanup and how readiness-aware startup is surfaced to the CLI
- `docs/process-management.md` — broader runtime flow around state, logs, and reconciliation
