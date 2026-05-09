# devhub

A CLI tool for managing local development projects. Start, stop, and monitor your projects from a single config file, with automatic Caddy reverse proxy so you get clean URLs like `http://worth.localhost:1300` instead of memorizing `localhost:3000`.

## Prerequisites

- **[Caddy](https://caddyserver.com/docs/install)** — required for the reverse proxy feature. If Caddy is not installed, devhub will still start your projects, but the proxy setup will be skipped with a warning.
- **Rust 1.85+** — only needed if building from source.

## Installation

```bash
git clone <repo-url> devhub
cd devhub
cargo install --path .
```

## Configuration

Create `~/.devhub/proj.json`:

```json
{
  "worth": {
    "path": "~/Projects/worth_meter",
    "cmd": "pnpm dev",
    "port": 3000,
    "startup_timeout_ms": 120000,
    "ready_cmd": "curl -fsS http://127.0.0.1:3000/healthz"
  },
  "blog": {
    "path": "~/Code/blogs",
    "cmd": "pnpm dev",
    "port": 4000
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
| `port` | no | If set, devhub sets up a Caddy reverse proxy at `<name>.localhost:1300` and, unless `ready_cmd` is set, uses a TCP readiness probe against `127.0.0.1:<port>`. |
| `startup_timeout_ms` | no | How long `devhub start` waits for readiness before failing. Defaults to `60000`. |
| `ready_cmd` | no | Custom readiness probe command. If set, `devhub start` runs this command until it exits `0`, the project exits first, or the timeout is reached. |

## Usage

```
$ devhub list

PROJECT              PATH                                     PORT       COMMAND
worth                 ~/Projects/worth_meter                   3000       pnpm dev
blog                  ~/Code/blogs                             4000       pnpm dev
tauri-demo            ~/playground/tauri-demo                  -          pnpm tauri dev
```

```
$ devhub start worth

Starting 'worth'...
  log:  /Users/you/.devhub/logs/worth.log
  readiness:  exec `curl -fsS http://127.0.0.1:3000/healthz`
  pid:  93128
  url:  http://worth.localhost:1300 (→ localhost:3000)
Started.
```

```
$ devhub status

PROJECT              STATUS       PID      URL
worth                 running      93128    http://worth.localhost:1300 (→ :3000)
blog                  stopped      -        -
tauri-demo            stopped      -        -
```

```
$ devhub stop worth

Stopping 'worth'...
Stopped.
```

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
- During startup and runtime, project `stdout`/`stderr` are written to `~/.devhub/logs/<name>.log`.
- On `stop`, the entire process group receives `SIGTERM` (then `SIGKILL` if it doesn't exit within 100ms).
- Stale entries (PIDs that are no longer alive) are **automatically pruned** on every command.
- Failed starts print the tail of the startup log and then delete that log. Successful project logs are deleted on `stop`, and inactive logs older than one day are cleaned up automatically on later commands.
- Projects with a `port` field get a Caddy reverse proxy entry at `http://<name>.localhost:1300 → localhost:<port>`. Caddy is reloaded (or started) automatically.

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
