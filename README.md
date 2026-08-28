# devhub

A CLI tool for managing local development projects. Start, stop, and monitor your projects from a single config file, with automatic Caddy reverse proxy so you get clean URLs like `http://worth.localhost` instead of memorizing `localhost:3000`.

## Prerequisites

- **[Caddy](https://caddyserver.com/docs/install)** — required for the reverse proxy feature. If Caddy is not installed, devhub will still start your projects, but the proxy setup will be skipped with a warning.
- **Rust 1.85+** — only needed if building from source.

## Installation

```bash
cargo install devhub
```

Or build from source:

```bash
git clone https://github.com/arichyx/devhub
cd devhub
cargo install --path .
```

## Configuration

Create `~/.devhub/proj.json`:

```json
{
  "worth": {
    "path": "~/Projects/worth_meter",
    "cmd": "PORT=13001 pnpm dev",
    "port": 13001,
    "startup_timeout_ms": 120000,
    "ready_cmd": "curl -fsS http://127.0.0.1:13001/healthz"
  },
  "blog": {
    "path": "~/Code/blogs",
    "cmd": "PORT=13002 pnpm dev",
    "port": 13002
  },
  "tauri-demo": {
    "path": "~/playground/tauri-demo",
    "cmd": "pnpm tauri dev"
  }
}
```

Each project supports these fields:

| Field | Required | Description |
|-------|----------|-------------|
| `path` | yes | Project directory. Supports `~` for home dir. |
| `cmd` | yes | Shell command to start the project. |
| `port` | no | If set, devhub sets up a Caddy reverse proxy at `<name>.localhost` and, unless `ready_cmd` is set, uses a TCP readiness probe against `127.0.0.1:<port>`. Must be unique across projects. |
| `startup_timeout_ms` | no | How long `devhub start` waits for readiness before failing. Defaults to `60000`. |
| `ready_cmd` | no | Custom readiness probe command. If set, `devhub start` runs this command until it exits `0`, the project exits first, or the timeout is reached. |

### Port conventions

Ports are user-assigned, and `cmd` can carry the assignment to the app (`PORT=13001 pnpm dev`, `pnpm dev --port 13001`), so most projects need no code changes. Pick a dedicated block such as `13001, 13002, …` and stay out of the trouble zones: ports below 1024, popular dev defaults (3000, 5173, 8080), and the ephemeral ranges the OS hands to outgoing connections (32768+ on Linux, 49152+ on macOS). Ports must be unique across projects — a config that repeats one is rejected at load time.

## Usage

```
$ devhub list

PROJECT              PATH                                     PORT       COMMAND
worth                 ~/Projects/worth_meter                   13001      PORT=13001 pnpm dev
blog                  ~/Code/blogs                             13002      PORT=13002 pnpm dev
tauri-demo            ~/playground/tauri-demo                  -          pnpm tauri dev
```

```
$ devhub start worth

Starting 'worth'...
  log:  /Users/you/.devhub/logs/worth.log
  readiness:  exec `curl -fsS http://127.0.0.1:13001/healthz`
  pid:  93128
  url:  http://worth.localhost (→ localhost:13001)
Started.
```

```
$ devhub status

PROJECT              STATUS       PID      URL
worth                 running      93128    http://worth.localhost (→ :13001)
blog                  stopped      -        -
tauri-demo            stopped      -        -
```

```
$ devhub stop worth

Stopping 'worth'...
Stopped.
```

```
$ devhub restart worth

Restarting 'worth'...
  stopped previous run (pid 93128)
  log:  /Users/you/.devhub/logs/worth.log
  readiness:  exec `curl -fsS http://127.0.0.1:13001/healthz`
  pid:  93131
  url:  http://worth.localhost (→ localhost:13001)
Restarted.
```

Restarting a project that is not running just starts it, so `restart` also works as "pull my crashed project back up". If the previous run ended on its own, the tail of its log is printed before the fresh start replaces it.

## How it works

- **Config & state** live in `~/.devhub/`:

  ```
  ~/.devhub/
    proj.json      # your project config
    state.json     # runtime state (PIDs, auto-managed)
    Caddyfile      # generated reverse proxy config (auto-managed)
    logs/          # per-project stdout/stderr logs
  ```

- Processes are spawned in a **new process group** (`setpgid`), fully detached from the devhub CLI. They survive after devhub exits.
- `devhub start` waits for readiness instead of treating `spawn()` as success. If `ready_cmd` is set, it is polled until it exits `0`; otherwise, a project with `port` uses a TCP readiness probe against `127.0.0.1:<port>`.
- The TCP probe verifies the listener on `port` belongs to the project's own process group. If an unrelated process holds the port (a stale server, a manually started dev server), the start fails immediately and names the offending PID instead of letting that process answer the probe and receive the project's proxy route. Ports must also be unique across projects — a config that declares the same port twice is rejected at load time.
- During startup and runtime, project `stdout`/`stderr` are written to `~/.devhub/logs/<name>.log`.
- On `stop`, the entire process group receives `SIGTERM` (then `SIGKILL` if it doesn't exit within 100ms); the stop returns only after the group is confirmed gone. Stopping a project that is not running is an error, but a group that dies mid-stop is still removed from state first.
- `restart` preflights the config before stopping anything, then stops the project (if running) and starts it again through the same readiness-checked path. The proxy route is kept across the restart — the URL briefly 502s instead of disappearing — and if the new start fails, the route is dropped and the error states whether the project is stopped or may still be running.
- Stale entries (PIDs that are no longer alive) are **automatically pruned** on every command.
- Failed starts print the tail of the startup log and then delete that log. Successful project logs are deleted on `stop`, and inactive logs older than one day are cleaned up automatically on later commands.
- Projects with a `port` field get a Caddy reverse proxy entry at `http://<name>.localhost → localhost:<port>`. The gateway listens on the standard HTTP port 80 (macOS allows unprivileged binding), so project URLs carry no port. Caddy is reloaded (or started) automatically; if port 80 is already held by another process, the error names the offending PID.

For a detailed explanation of process groups, status/stop semantics, Caddy reconciliation, and a worked `worth` example, see [docs/process-management.md](docs/process-management.md).

## Case Studies

- [docs/process-management.md](docs/process-management.md) — operational walkthrough of the full lifecycle and command flow
- [docs/case-studies/process-group-based-project-management.md](docs/case-studies/process-group-based-project-management.md) — why the architecture centers on process groups, persisted runtime ownership, and derived proxy state
- [docs/case-studies/startup-readiness-and-failure-diagnostics.md](docs/case-studies/startup-readiness-and-failure-diagnostics.md) — why startup success means readiness, and how logs/readiness replaced silent failures

## Development

```bash
cargo build
cargo nextest run   # or: cargo test
```
