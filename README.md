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
    "port": 3000
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

Each project has three fields:

| Field  | Required | Description |
|--------|----------|-------------|
| `path` | yes      | Project directory. Supports `~` for home dir. |
| `cmd`  | yes      | Shell command to start the project. |
| `port` | no       | If set, devhub sets up a Caddy reverse proxy at `<name>.localhost:1300`. |

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
  ```

- Processes are spawned in a **new process group** (`setpgid`), fully detached from the devhub CLI. They survive after devhub exits.
- On `stop`, the entire process group receives `SIGTERM` (then `SIGKILL` if it doesn't exit within 100ms).
- Stale entries (PIDs that are no longer alive) are **automatically pruned** on every command.
- Projects with a `port` field get a Caddy reverse proxy entry at `http://<name>.localhost:1300 → localhost:<port>`. Caddy is reloaded (or started) automatically.

For a detailed explanation of process groups, status/stop semantics, Caddy reconciliation, and a worked `worth` example, see [docs/process-management.md](docs/process-management.md).

## Development

```bash
cargo build
cargo nextest run   # or: cargo test
```
