# Process-Group-Based Project Management in devhub

> We built `devhub` around Unix process groups so a small non-daemon CLI can still manage real multi-process development services safely.

## Problem

We wanted `devhub` to be a lightweight CLI, not a long-lived background daemon, but still give us a reliable way to:

- start multiple local development projects
- stop them cleanly later
- show trustworthy status
- expose stable local URLs such as `http://worth.localhost:1300`

The catch is that modern dev commands rarely map to one stable PID. A single `pnpm dev` can fan out into a shell, a package manager wrapper, a framework command, the actual HTTP server, and worker processes. If we chose the wrong ownership model, `devhub` could easily report the wrong status or stop the wrong thing.

## Why It's Hard

There are three competing constraints in this design.

First, dev servers are usually process trees, not single processes. The PID that `devhub` starts is often just the top layer.

Second, `devhub` is intentionally not a daemon. Once `devhub start worth` exits, there is no resident supervisor keeping an in-memory model up to date. That means every later command has to reconstruct reality from persisted state plus what the OS says is still alive.

Third, local proxy state can drift from runtime ownership if they are maintained separately. If a service is gone but the proxy route remains, users can still reach stale traffic. If the service is alive but state was pruned incorrectly, `stop` loses control over it.

## Alternatives Considered

### Option A: Track only one PID

- How it works: store the PID returned by `spawn()` and treat that one process as the service.
- Pros: very simple mental model and minimal implementation.
- Cons: wrong for multi-process dev servers; if the leader exits but descendants continue, `status` can report `stopped` while the actual service is still reachable.

### Option B: Manage by parent/child assumptions

- How it works: treat the original process as the permanent parent and infer liveness from parent-child relationships.
- Pros: sounds intuitive if we think of the service as one tree with one obvious root.
- Cons: Unix does not guarantee that the parent process is the one we care about. Wrappers can `exec`, parents can exit, and real servers can outlive their launcher.

### Option C: Run a long-lived supervisor daemon

- How it works: keep a background service that owns every child process and reconciles state continuously.
- Pros: strongest continuous control, easiest place to keep logs, metrics, and richer health state.
- Cons: much heavier operational model, more moving parts, and directly against the simplicity goal of this project.

### Final Choice: One Process Group per Project + Persisted State + Derived Proxy Config

- How it works: create a dedicated Unix process group for each project, persist that ownership in `state.json`, and derive `Caddyfile` from current managed state.
- Pros: strong enough for real dev servers while keeping `devhub` itself simple and short-lived.
- Cons: requires understanding process groups, reconciliation on every command, and careful treatment of “PID” as a process-group identifier in practice.

## Solution

We centered the architecture on a simple rule: **the managed unit is the process group, not the original leader process**.

At the top level, every `devhub` command performs the same housekeeping loop before running its subcommand:

```rust
fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = config::Config::load()?;
    let mut state = state::AppState::load()?;

    let dead = state.prune_dead();
    if !dead.is_empty() {
        state.save()?;
    }

    logs::cleanup_outdated_logs(&state)?;
    caddy::reconcile_caddy(&state)?;
    // then dispatch list/start/stop/status
}
```

This loop is what lets a non-daemon tool stay consistent. We reload config, prune stale process ownership, clean old logs, reconcile Caddy, and only then answer the user’s command.

When we start a project, we create a new process group immediately:

```rust
let mut child = Command::new("sh")
    .arg("-c")
    .arg(&config.cmd)
    .current_dir(project_dir)
    .stdout(Stdio::from(stdout_log))
    .stderr(Stdio::from(stderr_log))
    .process_group(0)
    .spawn()?;
```

Calling `.process_group(0)` means the spawned process begins a new group whose initial PGID matches its PID. From that point on, we manage the whole group as one unit, even if the original leader later exits.

That is why `state.json` stores a field still named `pid`, but lifecycle-wise we treat it as the managed process-group identifier:

```rust
pub fn is_running(&self, name: &str) -> bool {
    if let Some(ps) = self.processes.get(name) {
        is_process_group_alive(ps.pid)
    } else {
        false
    }
}

pub fn is_process_group_alive(group_id: u32) -> bool {
    signal_target_exists(-(group_id as i32))
}
```

The negative target is the important detail. `kill(-pgid, 0)` probes whether the process group still exists, and `kill(-pgid, SIGTERM)` or `kill(-pgid, SIGKILL)` signals the whole managed service, not just one wrapper process.

That same model shapes `stop`:

- we look up the stored group identifier from `state.json`
- we confirm the group is still alive
- we terminate the whole group
- we remove the entry from state

The same model also shapes Caddy. We do not treat `Caddyfile` as a source of truth. Instead, it is a derived artifact generated from the projects that `devhub` still considers managed and alive. That keeps routing aligned with ownership, which is especially important in a short-lived CLI where consistency must be re-established on later commands.

Taken together, the system has four layers of state with clear responsibilities:

- `proj.json`: user intent
- `state.json`: current managed ownership
- `Caddyfile`: derived proxy view of managed services
- `logs/`: operational diagnostics for active and recent starts

That separation keeps the core model understandable while still giving us enough leverage to manage real dev workflows.

## Key Takeaways

- The core abstraction is the process group, not the leader PID.
- For a non-daemon tool, every command is a reconciliation point.
- Proxy config should be derived from runtime ownership state.
- Persisted state is useful, but it must always be cross-checked against actual OS liveness.

## References

- `src/main.rs` — top-level command loop and reconciliation sequence
- `src/process.rs` — process-group spawning and whole-group termination
- `src/state.rs` — persisted ownership model and process-group liveness checks
- `src/caddy.rs` — derived proxy generation and reconciliation behavior
- `docs/process-management.md` — operational walkthrough of the same architecture
