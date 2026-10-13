# Ulak

[![CI](https://github.com/syy/ulak/actions/workflows/ci.yml/badge.svg)](https://github.com/syy/ulak/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ulak.svg)](https://crates.io/crates/ulak)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Run the Docker workloads for a local project on a server you control.

Ulak keeps your editor and source checkout on your computer. It synchronizes
the files that Docker needs, runs Docker commands over SSH, and forwards
published ports back to `localhost`. Builds and containers use the CPU, memory,
disk, and architecture of the server.

You do not need a local Docker daemon or Docker Desktop. Ulak itself installs
no agent and opens no additional service port on the server.

```text
local checkout  ⇄  sync over SSH   ⇄  remote workspace
                                             │
                                             └── Docker / Compose
localhost       ◀──── SSH tunnels ───────────┘
```

Ulak supports macOS and Linux.

## Quick start

After you [install Ulak](#install), open a project that has a Compose file:

```bash
cd ~/projects/my-app
ulak init my-server
ulak doctor
ulak docker compose up -d
```

`my-server` can be an SSH alias, a hostname, or `user@hostname`.

By default, the first ordinary command in a terminal also installs a local
background service for live sync and port forwarding. See
[Background service](#background-service) to configure or disable it.

- `init` creates `ulak.toml`. It is optional; you can write the file yourself.
- `doctor` checks your computer, the server, and the paths in the Compose model.
- `up -d` synchronizes the project and starts the stack on the server.

Once the stack is running, Ulak keeps the remote workspace in sync. Files made
by a container, such as generated code or migrations, come back to your local
checkout. Published ports are available on `localhost`.

Use the Docker commands you already know:

```bash
ulak status
ulak docker compose logs -f
ulak docker compose exec app sh
ulak docker compose down
```

## Install

### On your computer

You need macOS or Linux, OpenSSH and rsync 3.2 or later. Apple's built-in
openrsync is not supported; install `rsync` with Homebrew on macOS.

Install a prebuilt release:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/syy/ulak/releases/latest/download/ulak-installer.sh | sh
```

Or use Homebrew (also installs rsync) or Cargo (requires Rust 1.89 or later):

```bash
brew install syy/tap/ulak
# Or build from source:
cargo install ulak
```

### On the server

The server needs key-based SSH access, Docker Engine, rsync 3.2 or later,
and Python 3.7 or later. Compose projects also need Docker Compose v2.
The SSH user must be able to run Docker without `sudo`.

Run `ulak doctor` to check the requirements and get commands to fix missing
tools. From a cloned repository, you can check a server before configuration:

```bash
ssh my-server 'bash -s' < scripts/check-server.sh
```

Ulak sends Python helpers over SSH without installing them. Linux servers
with Python 3.9 or later support prompt detection of server-side file changes;
other supported servers use periodic comparisons.

## Why not `DOCKER_HOST=ssh://`?

An SSH Docker context moves the Docker API connection, but bind mounts still
refer to the server's filesystem. Ulak first synchronizes the required files,
then runs Compose in the remote workspace. Relative bind mounts, `env_file`,
`configs`, build contexts and sibling paths such as `../shared` keep their meaning.

## How synchronization works

Ulak asks the server's `docker compose config` which local paths the project
needs. It copies those paths, including sibling directories, rather than an
entire repository by default.

Local edits go to the server; files created or changed by containers come back.
If both sides changed a file differently since their last agreed content, Ulak
keeps both copies and reports a conflict. It does not merge files.

**Without recorded common content, `[sync] no_history = "send"` is the default:
the local copy replaces a differing server copy, with a warning.** Set it to
`"hold"` to keep both copies instead. See the
[synchronization guide](https://github.com/syy/ulak/blob/main/docs/synchronization.md)
for conflict resolution, deletion rules and transfer limits.

## Docker commands

Put `ulak` before a supported Docker command:

```bash
ulak docker build -t api .
ulak docker run --rm image
ulak docker cp app:/tmp/out.txt .
ulak docker compose -f compose.yaml -f compose.dev.yaml -p my-app up -d
```

Run `ulak docker --help` or see the
[Docker command map](https://github.com/syy/ulak/blob/main/docs/docker-commands.md).
Commands that Ulak cannot route safely are refused.

### Project scripts that call `docker`

Run an existing script through a temporary shim:

```bash
ulak shim run -- ./scripts/start.sh
```

For a persistent shim, use `ulak shim install`, `ulak shim status` and
`ulak shim uninstall`. Installation asks before changing a shell profile;
uninstall removes only Ulak's block.

## Dashboard

`ulak ui` opens a full-screen view of the server's Docker in your terminal:
containers by Compose project with live CPU and memory, logs, stats, images,
volumes and networks, with start, stop, restart and delete one key away.

```bash
cd ~/projects/my-app
ulak ui
```

![ulak ui: containers grouped by Compose project with live CPU and memory](https://raw.githubusercontent.com/syy/ulak/main/docs/images/dashboard-containers.svg)

The dashboard is in beta: keys and layout may still change. Destructive
actions ask first, with No as the default. See the
[dashboard guide](https://github.com/syy/ulak/blob/main/docs/dashboard.md)
for every key and what it runs on the server.

## Main commands

| Command | Purpose |
|---|---|
| `ulak init [host]` | Create an optional project configuration. |
| `ulak doctor [stack]` | Check local tools, the server, and Compose references. |
| `ulak docker <command>` | Run a supported Docker command on the server. |
| `ulak sync [stack]` | Synchronize the current project once. |
| `ulak status [stack]` | Show the workspace, stack, service, and forwarded ports. |
| `ulak ui` | Open the dashboard (beta): containers, logs, images, volumes and networks. |
| `ulak config` | Show the effective settings and the layer that supplied each one. |
| `ulak shim ...` | Let project scripts use the remote Docker daemon. |
| `ulak clean [stack]` | Remove the selected remote workspace when no stack uses it. |
| `ulak clean --forget-destination <ssh-host>` | Retire what this checkout declared on a server that no longer exists, without contacting it. |
| `ulak service ...` | Install, remove, or run the local background service. |
| `ulak completions <shell>` | Generate Bash, Zsh, or Fish completions. |

`[stack]` is a Compose project name; see [several stacks in one checkout](https://github.com/syy/ulak/blob/main/docs/stacks.md).

`ulak sync --dry-run` previews uploads and remote deletions, not downloads.
It still contacts the server; its upload list can differ from the real sync.
See [preview details](https://github.com/syy/ulak/blob/main/docs/synchronization.md#previewing-a-sync).

When you change `host`, Ulak's management commands can keep addressing the
server recorded for a running stack. See
[Changing a stack's server](https://github.com/syy/ulak/blob/main/docs/destinations.md) for destination selection and
retiring a server that no longer exists.

## Configuration

Settings apply in this order; later layers override earlier ones:

1. Global: `~/.config/ulak/config.toml`
2. Project: `ulak.toml`
3. Checkout: `ulak.local.toml`
4. Environment variables
5. CLI flags

Project and checkout files sit next to the Compose file. Lists replace earlier
lists. Run `ulak config` to inspect the result, or `ulak config --json` for scripts.
The first ordinary interactive run creates a fully commented global config.
The old `.config/ulak.toml` project location is ignored.

```toml
host = "my-server"

[sync]
exclude = ["node_modules", ".venv", "target"]
protect = ["data/", "uploads/"]
max_delete = 25
```

Paths in `[sync]` are relative to the Compose project directory, even when
the stack mounts something outside it; use `../web/uploads/` for a sibling
folder. A pattern without a slash, such as `node_modules`, matches at any
depth in every folder that travels.

`[sync] include` sends paths an exclude or a `.dockerignore` would drop. It
defaults to `.env`; setting it replaces that default. To keep `.env` local,
exclude it and leave it out of `include`.

### Protect server-owned data

`[sync] protect` paths are never uploaded, downloaded or deleted. Use it for
database files and uploads, not source code or generated files you want back.
`ulak doctor` suggests writable mounts that may need protection.

See [Environment variables](https://github.com/syy/ulak/blob/main/docs/env.md)
for all settings, SSH keys and agent setup.

## Background service

The first ordinary interactive command installs a local user service using
launchd on macOS or systemd on Linux. For stacks left running, it synchronizes
files, maintains localhost ports and reconnects after interruptions. Nothing
is installed automatically in non-interactive runs or on the server.

To disable automatic installation, set this in the **global** configuration:

```toml
[service]
auto = false
```

| Command | Purpose |
|---|---|
| `ulak service run --foreground` | Run continuous sync and tunnels manually. |
| `ulak service install` | Install or refresh the service; run after upgrading. |
| `ulak service uninstall` | Stop and remove the installed service. |

Keep `auto = false` to prevent a later terminal command from reinstalling it.
Small changes use a persistent SSH connection; larger transfers use rsync.
See the [synchronization guide](https://github.com/syy/ulak/blob/main/docs/synchronization.md)
for server-change detection and full comparisons.

## Ports and access

Publish ports on server loopback:

```yaml
services:
  web:
    ports:
      - "127.0.0.1:8080:80"
```

With the stack and background service running, this is available at
`http://localhost:8080`. A busy local port is reported; other ports still open.

**`"8080:80"` publishes on all server interfaces.** A localhost tunnel does not
make that server port private, and Docker's port rules can bypass UFW.
`ulak doctor` warns about public bindings.

Forwarding is TCP-only and runs from server to computer. UDP and automatic
forwarding of `network_mode: host` ports are unsupported. Containers cannot
use these tunnels to reach services on your computer.

## Data safety

- Ulak never uses `rsync --delete` and never deletes local files during a pull.
- Remote deletion requires a tracked file that is absent locally. A server
  edit since the last agreed content is kept and brought back instead.
- With no agreed content, a recorded send can authorize deletion even if the
  server changed the file. Sends known never to have arrived are excepted;
  see [deletion rules](https://github.com/syy/ulak/blob/main/docs/synchronization.md#deletion-safety).
- Protected paths and unclaimed server-created files are not deleted.
- More than 25 remote deletions require confirmation by default; configure
  `sync.max_delete` or `--max-delete` to change the limit.

Workspaces are isolated per client and checkout, locked during changes, and
checked for identity conflicts. SSH host-key checking stays enabled.

## Known limits

- Absolute and `~` paths in Compose refer to the server; `ulak doctor` checks them.
- Conflicts require manual resolution. Ulak does not merge or create conflict files.
- Rsync compares files before writing them: a concurrent server edit can be
  overwritten. Avoid editing locally a file that a container is rewriting.
- Digest caching assumes consistent filesystem clocks. Network filesystems
  with clock skew can hide a change.

See [Synchronization](https://github.com/syy/ulak/blob/main/docs/synchronization.md)
for resolving conflicts, finding warnings and clearing digest caches.

## Using Ulak from an agent

Set `ULAK_HOST` and provide SSH credentials through environment variables.
Keep a stable workspace namespace and UUID; persist the state directory for
deletion tracking and common-content history. Without history, the default
`sync.no_history = "send"` can replace a differing server copy.

Follow the [agent setup](https://github.com/syy/ulak/blob/main/docs/env.md#using-ulak-from-an-agent)
for identities, credentials and host-key verification.

## Demo

From a cloned repository:

```bash
cd examples/demo
ulak init my-server
ulak doctor
ulak docker compose up -d --build
ulak docker compose logs prober
# Open http://localhost:8080
ulak docker compose down
ulak clean
```

## Contributing

See [`CONTRIBUTING.md`](https://github.com/syy/ulak/blob/main/CONTRIBUTING.md) for the checks and commit format,
[`AGENTS.md`](https://github.com/syy/ulak/blob/main/AGENTS.md) for the design guide, and [`SECURITY.md`](https://github.com/syy/ulak/blob/main/SECURITY.md)
to report a vulnerability privately. Releases are listed in
[`CHANGELOG.md`](https://github.com/syy/ulak/blob/main/CHANGELOG.md).

## License

Licensed under either [Apache-2.0](https://github.com/syy/ulak/blob/main/LICENSE-APACHE) or [MIT](https://github.com/syy/ulak/blob/main/LICENSE-MIT), at
your option.
