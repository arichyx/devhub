# Project Flow and Process Lifecycle in devhub

This document explains how `devhub` works as a whole:

- where project definitions live
- how `start`, `status`, `stop`, and `restart` work
- how runtime state is stored
- how local proxy routes are derived from that state
- why Unix process groups are the core abstraction

It is written for readers who are comfortable with application code, but are not yet comfortable with Unix process management details.

## What devhub is

At a high level, `devhub` is a small CLI that manages local development projects from one config file.

It gives you three things:

1. A single place to define projects.
2. A way to start and stop those projects consistently.
3. Friendly local URLs such as `http://worth.localhost` through Caddy.

It is intentionally simple. It is not a long-lived background daemon. Instead, each `devhub` command runs, does its work, updates a small amount of persisted state, and exits.

That design choice explains almost everything else in the project.

## The three sources of truth

`devhub` revolves around three files under `~/.devhub/`:

```text
~/.devhub/
  proj.json
  state.json
  Caddyfile
  logs/
```

You can think of them like this:

- `proj.json`: what can be run
- `state.json`: what `devhub` currently believes it is managing
- `Caddyfile`: what local HTTP routes should currently exist
- `logs/`: per-project stdout/stderr for active and recent starts

## `proj.json`: desired project config

`proj.json` is the user-owned configuration.

Each project entry defines:

- `path`: where the project lives
- `cmd`: how to start it
- `port`: which local port, if any, should be proxied
- `startup_timeout_ms`: how long `devhub start` waits for readiness
- `ready_cmd`: an optional custom readiness probe command

Example:

```json
{
  "worth": {
    "path": "~/Documents/proj/worth-meter",
    "cmd": "PORT=13001 pnpm dev",
    "port": 13001
  }
}
```

This means:

- when asked to start `worth`, `devhub` should `cd` into that directory
- run `pnpm dev`
- wait until readiness succeeds
- if the project is running, expose it as `http://worth.localhost`

The readiness rules are intentionally simple:

- if `ready_cmd` is set, `devhub` repeatedly runs it until it exits `0`
- otherwise, if `port` is set, `devhub` repeatedly tries a TCP connection to `localhost:<port>`, on both `127.0.0.1` and `::1` — dev servers bound to `localhost` often listen on only one address family (Node 17+ resolves `localhost` to `::1` on macOS)
- otherwise, `devhub` falls back to spawn-only behavior and cannot strongly verify readiness

One correctness rule sits on top of the port-based probes: a connect alone does not prove the *project* is ready, only that *something* is listening. So whenever `port` is configured, `devhub` also asks `lsof`/`ps` for the listener's process group and requires it to match the group it spawned (children inherit the group). Two failure classes fall out of this:

- a stale server or a manually started dev server squatting on the port fails the start immediately, naming the offending PID, instead of silently receiving the project's proxy route
- two projects declaring the same `port` could otherwise let one project's listener satisfy the other's probe — `devhub` rejects such a config at load time

## `state.json`: runtime ownership

`state.json` is owned by `devhub`, not by the user.

It records information about projects that `devhub` has started and still considers alive.

For each running project, it stores:

1. the managed process-group identifier
2. the start time
3. the configured port
4. the group leader's start time as the OS reports it (`leader_started`)
5. whether the project has passed readiness yet (`ready`)

A project is recorded the moment it is spawned, with `ready: false`, and flipped to `ready: true` once readiness succeeds. Recording before the readiness wait means a `devhub` that is killed mid-start never leaves an untracked project behind; `status` shows such a project as `starting`, and it gets no proxy route until it is ready.

Several `devhub` commands can run at once (two terminals starting two projects), so every change to `state.json` is a locked read-modify-write: take an exclusive `flock` on `~/.devhub/state.lock`, re-read the file, apply the change, write a temporary file and `rename` it over `state.json`. Re-reading under the lock keeps entries another command wrote in the meantime; the rename means a reader never sees a half-written file.

`leader_started` guards against PID reuse. Once a project dies, the OS may hand its PID to an unrelated process; if that process happens to lead its own process group, a bare `kill(-pid, 0)` would call it alive and `stop` would signal it. So an entry only counts as alive when its group exists *and*, if the leader process is still around, the leader's start time matches the recorded one.

One subtle detail matters a lot:

the field is still named `pid`, but lifecycle-wise it is treated as the identifier of the managed process group.

That works because when `devhub` first creates a new session (which also creates a new process group):

- the first process PID
- and the new PGID

start out as the same number.

## `Caddyfile`: derived proxy state

`Caddyfile` is generated from `state.json`.

If a running project has a configured `port`, it gets a route like:

```text
http://worth.localhost {
    reverse_proxy localhost:13001
}
```

The site has no port suffix, so Caddy serves it on the standard HTTP port 80. macOS allows unprivileged processes to bind it, which keeps project URLs port-free.

If the project is no longer running, that route should disappear.

So Caddy is not the primary source of truth. It is a derived artifact that should match the current managed state.

## `logs/`: stdout and stderr during startup and runtime

Each project writes its stdout and stderr to:

```text
~/.devhub/logs/<project>.log
```

The log lifecycle is intentionally simple:

- on successful start, the log stays while the service runs
- on `stop`, the log is deleted
- on `restart`, if the previous run ended on its own, its log tail is printed before the fresh start truncates it — otherwise the only record of why it died would be lost
- on startup failure, `devhub` prints the tail of the log and deletes it immediately
- on later commands, inactive logs older than one day are cleaned up automatically

This gives startup diagnostics without keeping unbounded historical logs around forever.

## The top-level command loop

Every `devhub` command follows the same top-level structure:

1. Parse CLI arguments.
2. Load `state.json`.
3. Prune entries whose managed process groups no longer exist (under the state lock).
4. Clean up inactive old log files.
5. Reconcile the generated `Caddyfile` with what is on disk now.
6. Execute the requested subcommand, which loads `proj.json` only if it needs it.

None of the housekeeping reads `proj.json`, and neither does `stop`. A config that is mid-edit or invalid therefore never prevents stopping a running project; a missing `proj.json` simply means no projects are configured.

This means even a read-oriented command such as `status` still does housekeeping:

- it may remove stale state
- it may remove stale proxy routes

That is intentional. Since `devhub` is not a daemon, every command is an opportunity to bring the world back in sync.

## Why process groups are the core abstraction

Modern dev commands often spawn more than one process.

A project command such as `pnpm dev` may end up creating:

- a shell
- `pnpm`
- a framework dev command such as `next dev`
- the actual HTTP server
- worker processes

If `devhub` managed only one PID, it would be easy to stop the wrong layer and accidentally leave the real server running.

So instead, `devhub` creates a dedicated Unix process group for each project and later manages that whole group as one unit.

This gives `devhub` one reliable handle for:

- probing liveness
- terminating the whole project tree

## Process, process group, and group leader

### Process

A process is one running program with its own PID.

Examples:

- a shell running `sh -c "pnpm dev"`
- `pnpm`
- `next dev`
- `next-server`

### Process group

A process group is a set of related processes identified by a PGID.

All members of the same group share the same PGID, even though each still has its own PID.

### Group leader

The first process in a process group is commonly called the group leader.

When `devhub` creates a new group, the first process begins with:

- `PID = X`
- `PGID = X`

Important detail:

the group can stay alive even after the leader exits, as long as some other member in the group still exists.

This is the most important Unix detail to keep in mind when reading the rest of the project.

## How signaling works

At the Unix API level, `devhub` needs to do two kinds of things:

- signal one process
- signal or probe one whole process group

With `kill`, the target determines which one you mean:

- `kill(42000, SIGTERM)`: signal process `42000`
- `kill(-42000, SIGTERM)`: signal process group `42000`
- `kill(42000, 0)`: probe whether process `42000` exists
- `kill(-42000, 0)`: probe whether process group `42000` exists

The `0` signal is a special convention:

- it does not actually terminate anything
- it only checks whether the target exists and is signalable

That is why the same low-level primitive can support both:

- `status`
- `stop`

If you prefer a more explicit mental model, `kill(-pgid, sig)` is conceptually the same thing as a dedicated `killpg(pgid, sig)` helper.

## Why `sh -c` is used

Projects are started with:

```rust
Command::new("sh")
    .arg("-c")
    .arg(&config.cmd)
```

This makes config entries flexible. Commands can be ordinary shell commands such as:

- `pnpm dev`
- `pnpm tauri dev`
- `cargo run`
- `FOO=bar pnpm dev`

It also means the shell is just one layer in the startup chain. It may remain visible as a long-lived process, or it may effectively hand off execution to the launched command. Either way, `devhub` cares about the process group as a whole, not about the shell specifically.

## Code layout

The main pieces of the implementation are:

- `src/main.rs`: parses commands and drives the top-level flow
- `src/config.rs`: loads `proj.json`
- `src/state.rs`: loads, saves, prunes, and probes runtime state
- `src/process.rs`: starts and stops managed process groups
- `src/caddy.rs`: generates and reconciles the Caddy config
- `src/dirs.rs`: resolves paths under `~/.devhub/`

## Flow of `devhub start <name>`

Suppose you run:

```bash
devhub start worth
```

The flow is:

1. Load current state.
2. Remove stale state entries whose process groups no longer exist.
3. Clean up old inactive logs.
4. Reconcile Caddy so old stale routes disappear first.
5. Load config and look up the `worth` config entry.
6. Create or truncate `~/.devhub/logs/worth.log`.
7. Expand `~` in the configured path.
8. Start treating `SIGINT`/`SIGTERM`/`SIGHUP` as "abort this start" instead of letting them kill `devhub`.
9. Spawn `sh -c "pnpm dev"` in that directory, in a new session (`setsid`), with stdin from `/dev/null`.
10. Record the group's identifier, port and leader start time in `state.json` as `ready: false`.
11. Poll readiness until it succeeds, the process group exits, `startup_timeout_ms` is reached, or the user interrupts.
12. On success, mark the entry `ready: true`; on failure, kill the group and remove the entry.
13. If a `port` exists, regenerate the desired Caddy config and reload or start Caddy.
14. Print the project identifier and URL to the user.

A few easy-to-miss implementation details:

- `stdout` and `stderr` are written to the project log file
- the new session leaves the project without a controlling terminal, and stdin is `/dev/null`: a project in a background process group that touched the user's terminal (raw mode for keyboard shortcuts, an interactive prompt) would otherwise be stopped by `SIGTTOU`/`SIGTTIN`, and the start would hang until it timed out
- pressing Ctrl-C while `devhub` waits for readiness stops the just-spawned project and removes its entry, instead of leaving it running untracked
- `devhub` exits after startup completes, so the project continues independently

## Flow of `devhub status`

Suppose you run:

```bash
devhub status
```

The flow is:

1. Load config and state.
2. Prune dead managed groups.
3. Clean up inactive old logs.
4. Reconcile Caddy with the updated state.
5. For each configured project, check whether its recorded managed group is still alive (and still the same process, see PID reuse above).
6. Print `running`, `starting` (spawned, readiness not yet confirmed) or `stopped`, in project-name order.

`status` does not try to discover arbitrary local processes or claim any process that happens to own the configured port.

That is important.

`devhub` reports whether the project is still alive as a managed process group, not whether "something on the machine" is listening on the same port.

## Flow of `devhub stop <name>`

Suppose you run:

```bash
devhub stop worth
```

The flow is:

1. Load state and find the recorded group identifier for `worth`. If that identifier no longer belongs to our process (the group is gone, or the PID was reused), nothing is signaled.
2. Send `SIGTERM` to the whole group. If the group no longer exists, the stop is already complete.
3. Poll for up to `stop_timeout_ms` (default 5 s, recorded in state at start because `stop` does not read the config) and return as soon as the group is gone — most projects exit within milliseconds, while databases or `docker compose` get time to shut down cleanly.
4. If the group still exists, send `SIGKILL` to the whole group.
5. Wait until the group is confirmed gone (a short bounded poll; a group reduced to unreaped zombies counts as gone, since a zombie holds no resources, but any live member keeps the wait going).
6. Remove `worth` from `state.json`.
7. Delete `~/.devhub/logs/worth.log`.
8. Reconcile Caddy so the route disappears.

Why terminate the group instead of just one PID?

Because the top-level command may have spawned several layers. `stop` should bring down the whole project tree, not only one wrapper process.

At the lowest level, a stop only fails when a live group cannot be cleared: a signal is refused, or the group survives `SIGKILL` (for example, stuck in uninterruptible I/O). A group that turns out to be already gone is nothing to clear, so the internal stop primitive reports it like a completed stop — `restart` relies on this to pull a crashed project back up without a spurious error. The `stop` command itself keeps stricter user-facing behavior: stopping a project that is not running reports `project is not running`, and a group that dies mid-stop is removed from state before that error is raised.

## Flow of `devhub restart <name>`

Suppose you run:

```bash
devhub restart worth
```

The flow is:

1. Load config and state, prune dead entries, clean old logs, reconcile Caddy (the usual preamble).
2. Preflight: verify `worth` exists in `proj.json` and that its directory exists — before stopping anything, so a config typo or a moved directory cannot take down a healthy running process.
3. If `worth` is running, stop its group exactly as `stop` does and delete the log.
4. If it is not running, start fresh. When a leftover log from a run that ended on its own is present, its tail is printed first, because the fresh start would otherwise truncate the only record of why it died.
5. Start the project through the same readiness-checked path as `start`.
6. Reconcile Caddy once at the end: with the route content unchanged the reconcile is a no-op, so `worth.localhost` briefly returns 502 while the new process boots instead of the route disappearing and coming back; if the start failed, the route is dropped instead.

A failed start leaves the project stopped — a killed process cannot be resurrected — and the error says so explicitly. In the rare case where even the cleanup kill cannot confirm the process died, the error instead says it may still be running and names the pid, so a live process is never declared stopped.

## Flow of Caddy reconciliation

On every command, `devhub` asks:

- what routes should exist, based on `state.json`?
- what routes currently exist, based on the on-disk `Caddyfile`?

If those differ, `devhub` reloads or stops Caddy as needed.

This keeps proxy state derived from process state, rather than letting proxy state drift on its own.

Two details make that comparison trustworthy:

- the generated `Caddyfile` lists routes in project-name order, so the same state always produces byte-identical content (an unordered map would make almost every command see a "change" and reload Caddy)
- the on-disk `Caddyfile` is only kept once Caddy has accepted it; if a reload/start fails (port 80 busy, Caddy not installed), the file is removed, so the next command sees the route as missing and retries instead of treating the failed config as applied

Only projects marked `ready` get a route.

A matching file only records what was last applied, so each command also checks that devhub's Caddy is still running (its dedicated admin endpoint, `localhost:12019`, accepts connections) and restarts it if not.

After applying, devhub verifies that nothing besides its own Caddy listens on the gateway port. This is needed because Caddy binds with `SO_REUSEPORT`: starting it next to another Caddy that already holds port 80 does not fail, and on macOS the instance that bound first keeps receiving the connections, so devhub's routes would silently never be served. Whatever else holds the port is named in the error — including a Caddy left behind by an earlier devhub version, which `caddy stop` removes.

That prevents situations such as:

- the project is gone from `state.json`
- but `worth.localhost` still points at `localhost:13001`

## End-to-end example: `worth`

Assume this config:

```json
{
  "worth": {
    "path": "~/Documents/proj/worth-meter",
    "cmd": "PORT=13001 pnpm dev",
    "port": 13001
  }
}
```

Now suppose `devhub start worth` creates a new process group whose ID is `42000`.

A plausible runtime tree looks like this:

```text
PID    PPID   PGID   COMMAND
42000  ...    42000  sh -c "pnpm dev"
42000  ...    42000  pnpm dev              (after shell handoff)
42018  42000  42000  next dev
42031  42018  42000  next-server
42044  42031  42000  webpack worker
42045  42031  42000  webpack worker
```

The exact parent-child structure is less important than these two facts:

1. all project processes share `PGID = 42000`
2. the actual HTTP server is `next-server`, not necessarily the original leader

From that point on, the whole project flow makes sense:

1. `start` writes output to `~/.devhub/logs/worth.log`
2. `start` waits until the configured readiness condition passes
3. `start` records `42000` in `state.json`
4. `start` generates a Caddy route to `localhost:13001`
5. `status` probes whether process group `42000` still exists
6. `stop` sends signals to process group `42000`
7. `stop` deletes the log
8. when the group is gone, the route is removed from the generated `Caddyfile`

## One useful edge case

The main project flow above is the normal case.

One edge case is still worth understanding because it explains why `devhub` uses groups instead of one PID.

Suppose this happens:

```text
PID    PPID   PGID   COMMAND
42000  ...    42000  pnpm dev          exited
42018  ...    42000  next dev          alive
42031  ...    42000  next-server       alive
42044  ...    42000  webpack worker    alive
```

In that situation:

- process `42000` is gone
- process group `42000` is still alive

So from `devhub`'s point of view, the project is still running.

This is the core reason the whole design is based on process-group liveness instead of just one PID.

## Current limits

`devhub` is intentionally simple, so a few limits are worth knowing:

- it is not a daemon and does not continuously monitor projects
- long-running projects can still generate large logs during a single run; cleanup is lifecycle-based rather than size-based
- it does not try to infer ownership from arbitrary port listeners
- the stored field is still named `pid`, even though lifecycle logic now treats it as the managed process-group identifier

## Mental model to keep

If you remember only one thing, keep this:

`devhub` starts one detached session and process group per project, records that group's ID in state, derives proxy routes from the set of live managed groups, and later stops projects by signaling the whole recorded group.
