# Project Flow and Process Lifecycle in devhub

This document explains how `devhub` works as a whole:

- where project definitions live
- how `start`, `status`, and `stop` work
- how runtime state is stored
- how local proxy routes are derived from that state
- why Unix process groups are the core abstraction

It is written for readers who are comfortable with application code, but are not yet comfortable with Unix process management details.

## What devhub is

At a high level, `devhub` is a small CLI that manages local development projects from one config file.

It gives you three things:

1. A single place to define projects.
2. A way to start and stop those projects consistently.
3. Friendly local URLs such as `http://worth.localhost:1300` through Caddy.

It is intentionally simple. It is not a long-lived background daemon. Instead, each `devhub` command runs, does its work, updates a small amount of persisted state, and exits.

That design choice explains almost everything else in the project.

## The three sources of truth

`devhub` revolves around three files under `~/.devhub/`:

```text
~/.devhub/
  proj.json
  state.json
  Caddyfile
```

You can think of them like this:

- `proj.json`: what can be run
- `state.json`: what `devhub` currently believes it is managing
- `Caddyfile`: what local HTTP routes should currently exist

## `proj.json`: desired project config

`proj.json` is the user-owned configuration.

Each project entry defines:

- `path`: where the project lives
- `cmd`: how to start it
- `port`: which local port, if any, should be proxied

Example:

```json
{
  "worth": {
    "path": "~/Documents/proj/worth-meter",
    "cmd": "pnpm dev",
    "port": 3000
  }
}
```

This means:

- when asked to start `worth`, `devhub` should `cd` into that directory
- run `pnpm dev`
- if the project is running, expose it as `http://worth.localhost:1300`

## `state.json`: runtime ownership

`state.json` is owned by `devhub`, not by the user.

It records information about projects that `devhub` has started and still considers alive.

For each running project, it stores:

1. the managed process-group identifier
2. the start time
3. the configured port

One subtle detail matters a lot:

the field is still named `pid`, but lifecycle-wise it is treated as the identifier of the managed process group.

That works because when `devhub` first creates a new process group:

- the first process PID
- and the new PGID

start out as the same number.

## `Caddyfile`: derived proxy state

`Caddyfile` is generated from `state.json`.

If a running project has a configured `port`, it gets a route like:

```text
http://worth.localhost:1300 {
    reverse_proxy localhost:3000
}
```

If the project is no longer running, that route should disappear.

So Caddy is not the primary source of truth. It is a derived artifact that should match the current managed state.

## The top-level command loop

Every `devhub` command follows the same top-level structure:

1. Parse CLI arguments.
2. Load `proj.json`.
3. Load `state.json`.
4. Prune entries whose managed process groups no longer exist.
5. Reconcile the generated `Caddyfile` with what is on disk now.
6. Execute the requested subcommand.

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

1. Load config and current state.
2. Remove stale state entries whose process groups no longer exist.
3. Reconcile Caddy so old stale routes disappear first.
4. Look up the `worth` config entry.
5. Expand `~` in the configured path.
6. Spawn `sh -c "pnpm dev"` in that directory.
7. Put that child in a new process group with `.process_group(0)`.
8. Record the group's identifier and port in `state.json`.
9. If a `port` exists, regenerate the desired Caddy config and reload or start Caddy.
10. Print the project identifier and URL to the user.

Two easy-to-miss implementation details:

- `stdout` and `stderr` are currently redirected to `/dev/null`
- `devhub` exits after spawning, so the project continues independently

## Flow of `devhub status`

Suppose you run:

```bash
devhub status
```

The flow is:

1. Load config and state.
2. Prune dead managed groups.
3. Reconcile Caddy with the updated state.
4. For each configured project, check whether its recorded managed group is still alive.
5. Print `running` or `stopped`.

`status` does not try to discover arbitrary local processes or claim any process that happens to own the configured port.

That is important.

`devhub` reports whether the project is still alive as a managed process group, not whether "something on the machine" is listening on the same port.

## Flow of `devhub stop <name>`

Suppose you run:

```bash
devhub stop worth
```

The flow is:

1. Load state and find the recorded group identifier for `worth`.
2. Probe whether that process group still exists.
3. Send `SIGTERM` to the whole group.
4. Wait 100ms.
5. If the group still exists, send `SIGKILL` to the whole group.
6. Remove `worth` from `state.json`.
7. Reconcile Caddy so the route disappears.

Why terminate the group instead of just one PID?

Because the top-level command may have spawned several layers. `stop` should bring down the whole project tree, not only one wrapper process.

## Flow of Caddy reconciliation

On every command, `devhub` asks:

- what routes should exist, based on `state.json`?
- what routes currently exist, based on the on-disk `Caddyfile`?

If those differ, `devhub` reloads or stops Caddy as needed.

This keeps proxy state derived from process state, rather than letting proxy state drift on its own.

That prevents situations such as:

- the project is gone from `state.json`
- but `worth.localhost:1300` still points at `localhost:3000`

## End-to-end example: `worth`

Assume this config:

```json
{
  "worth": {
    "path": "~/Documents/proj/worth-meter",
    "cmd": "pnpm dev",
    "port": 3000
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

1. `start` records `42000` in `state.json`
2. `start` generates a Caddy route to `localhost:3000`
3. `status` probes whether process group `42000` still exists
4. `stop` sends signals to process group `42000`
5. when the group is gone, the route is removed from the generated `Caddyfile`

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
- it does not currently persist stdout or stderr logs
- it does not try to infer ownership from arbitrary port listeners
- the stored field is still named `pid`, even though lifecycle logic now treats it as the managed process-group identifier

## Mental model to keep

If you remember only one thing, keep this:

`devhub` starts one detached process group per project, records that group's ID in state, derives proxy routes from the set of live managed groups, and later stops projects by signaling the whole recorded group.
