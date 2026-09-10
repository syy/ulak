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
local checkout  ⇄  rsync over SSH  ⇄  remote workspace
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

You need:

- macOS or Linux;
- OpenSSH (`ssh`);
- rsync 3.2 or later; and
- Rust 1.89 or later to build Ulak from source.

Apple's built-in openrsync is not supported. On macOS, install a current
rsync with Homebrew first (the Homebrew formula below does this for you):

```bash
brew install rsync
```

Install the latest release (prebuilt for macOS and Linux):

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/syy/ulak/releases/latest/download/ulak-installer.sh | sh
```

Alternatives:

```bash
# Homebrew, on macOS or Linux; also installs rsync
brew install syy/tap/ulak

# crates.io, built from source
cargo install ulak

# a checkout; also gives you scripts/check-server.sh and the demo
git clone https://github.com/syy/ulak.git
cd ulak
cargo install --path crates/ulak
```

### On the server

The server needs:

| Requirement | Check |
|---|---|
| SSH with key authentication | `ssh my-server true` |
| Docker Engine | `docker --version` |
| Docker Compose v2 (for Compose projects) | `docker compose version` |
| rsync 3.2 or later | `rsync --version` |

The SSH user must be able to run Docker without `sudo`. Ulak never runs `sudo`
for you.

Compose is optional if you only run Docker commands that do not use a Compose
model, such as `ulak docker ps` or `ulak docker build`.

Run `ulak doctor` after installation. It reports a missing requirement and
shows the next command to run. To check a new server before you configure a
project, run this from the cloned Ulak repository:

```bash
ssh my-server 'bash -s' < scripts/check-server.sh
```

## Why not `DOCKER_HOST=ssh://`?

An SSH Docker context moves the Docker API connection. It does not move the
filesystem.

This difference matters for bind mounts. A path such as `./site` exists on your
computer, but a remote Docker daemon looks for it on the server. A missing file
causes an error. A missing directory can become an empty directory, so the
container can start with no source files and no clear failure.

Ulak synchronizes the required files first and then runs Compose inside the
remote workspace. Relative bind mounts, `env_file`, `configs`, build contexts,
and paths such as `../shared` keep their normal meaning.

## How synchronization works

Ulak uses the Compose model as the boundary of a project. It asks the server's
`docker compose config` for the resolved model and synchronizes the local paths
that model references. It does not copy an entire repository unless the model
needs the entire repository.

This has two useful effects:

- A service in a large monorepo can synchronize only its own files.
- A Compose file can reference a sibling directory without moving to a new
  project root.

The synchronization runs in both directions. Local edits go to the server, and
files created or changed by the stack come back. Ulak does not provide a merge
engine: if both sides change the same file, the last write wins.

Ulak follows `.gitignore` by default. Use `[sync] include` for a required
gitignored path and `[sync] protect` for data that must stay on the server.

## Docker commands

Docker's command tree stays below `ulak docker`:

| Docker | Ulak |
|---|---|
| `docker compose up -d` | `ulak docker compose up -d` |
| `docker build -t api .` | `ulak docker build -t api .` |
| `docker run --rm image` | `ulak docker run --rm image` |
| `docker exec -it app sh` | `ulak docker exec -it app sh` |
| `docker logs -f app` | `ulak docker logs -f app` |
| `docker cp app:/tmp/out.txt .` | `ulak docker cp app:/tmp/out.txt .` |
| `docker compose cp ./seed.txt app:/tmp/seed.txt` | `ulak docker compose cp ./seed.txt app:/tmp/seed.txt` |

Compose flags keep Docker's order:

```bash
ulak docker compose -f compose.yaml -f compose.dev.yaml -p my-app up -d
```

Run `ulak docker --help` to inspect the supported tree. Ulak refuses a command
that it cannot route safely instead of forwarding it on a guess. The generated
[Docker command map](https://github.com/syy/ulak/blob/main/docs/docker-commands.md) lists every command and how Ulak
handles it.

### Project scripts that call `docker`

A project script usually calls `docker`, not `ulak docker`. Run it through the
temporary shim without installing anything:

```bash
ulak shim run -- ./scripts/start.sh
```

For a persistent shim:

```bash
ulak shim install
ulak shim status
ulak shim uninstall
```

`shim install` asks before it changes a shell profile. `shim uninstall` removes
only the block that Ulak added.

## Main commands

| Command | Purpose |
|---|---|
| `ulak init [host]` | Create an optional project configuration. |
| `ulak doctor` | Check local tools, the server, and Compose references. |
| `ulak docker <command>` | Run a supported Docker command on the server. |
| `ulak sync` | Synchronize the current project once. |
| `ulak status` | Show the workspace, stack, service, and forwarded ports. |
| `ulak config` | Show the effective settings and the layer that supplied each one. |
| `ulak shim ...` | Let project scripts use the remote Docker daemon. |
| `ulak clean` | Remove the selected remote workspace when no stack uses it. |
| `ulak clean --forget-destination <ssh-host>` | Retire what this checkout declared on a server that no longer exists, without contacting it. |
| `ulak service ...` | Install, remove, or run the local background service. |
| `ulak completions <shell>` | Generate Bash, Zsh, or Fish completions. |

`ulak sync --dry-run` previews uploads and remote deletions without applying
them. It does not preview downloads. Resolving the Compose model still requires
a server connection and may upload temporary Compose and environment files.
Use `ulak config --json` when a script needs the effective configuration.

When you change `host`, Ulak's management commands can keep addressing the
server recorded for a running stack. See
[Changing a stack's server](https://github.com/syy/ulak/blob/main/docs/destinations.md) for destination selection and
retiring a server that no longer exists.

## Configuration

Ulak reads three TOML layers. Later layers override earlier ones, field by
field:

| Layer | Default path | Use |
|---|---|---|
| Global | `~/.config/ulak/config.toml` | Defaults for this computer. |
| Project | `ulak.toml` | Settings shared by the project. |
| Local | `ulak.local.toml` | Settings for one checkout. |

Environment variables override the files, and CLI flags override everything.
Lists replace earlier lists; they are not appended. Run `ulak config` to see the
result. The project and local files sit in the project directory itself, next
to the compose file; a `.config/ulak.toml` left over from an earlier version is
not read, and `ulak config` lists it as ignored.

On its first ordinary interactive run, Ulak creates a fully commented global
configuration file. The file changes no setting until you edit it.

A typical project configuration is small:

```toml
host = "my-server"

[sync]
exclude = ["node_modules", ".venv", "target"]
protect = ["data/", "uploads/"]
max_delete = 25

[forward]
auto = true
```

`.env` is already included by default. You only need to set `include` when you
want to replace that default or add other gitignored paths.

### Protect server-owned data

Use `[sync] protect` for writable data that belongs only to the server, such as
database files or uploads:

```toml
[sync]
protect = ["data/", "uploads/"]
```

A protected path is not pushed, pulled, or deleted. Do not protect source code
or generated files that you want in the local checkout. `ulak doctor` reports
writable mounts and suggests paths that may need protection.

For all environment variables, SSH key settings, and CI examples, see
[Environment variables](https://github.com/syy/ulak/blob/main/docs/env.md).

## Background service

The first Ulak command run in an interactive terminal installs one user-level
background service on your computer. Ulak uses launchd on macOS and a systemd
user unit on Linux. It prints a message when it does this.

The service only maintains stacks that you left running. It:

- watches local files and synchronizes changes;
- brings container-generated files back;
- keeps published ports on `localhost`; and
- reconnects after sleep, a network change, or a server restart.

Nothing is installed automatically in a non-interactive script, CI job, or
test. Nothing is installed on the Docker server.

To disable automatic background work, put this in the global configuration:

```toml
[service]
auto = false
```

Every command still works. Run the service manually when you need continuous
sync and tunnels:

```bash
ulak service run --foreground
```

Install or refresh the service explicitly with `ulak service install`.
To stop and remove the installed service, run:

```bash
ulak service uninstall
```

Keep `[service] auto = false` in the global configuration to prevent the next
ordinary terminal command from installing it again. After an upgrade, run
`ulak service install` to restart the service with the installed binary.

## Ports and access

Bind a published port to loopback in Compose:

```yaml
services:
  web:
    ports:
      - "127.0.0.1:8080:80"
```

While the stack and Ulak's background service are running, the server's TCP
port is available at `http://localhost:8080`. If a local port is busy, Ulak
names it and still opens the other available ports.

Be careful with this shorter form:

```yaml
ports:
  - "8080:80"
```

It binds the port on all server interfaces. Forwarding it to `localhost` does
not make the server-side port private, and Docker's published-port rules can
bypass host firewall policies such as UFW. `ulak doctor` warns about public
bindings.

Ulak forwards server ports to your computer. It does not provide the reverse
path from a container to a service on your computer. Use a private routed
network when a container must reach a local service.

## Data safety

Ulak treats deletion as a separate operation from synchronization.

- It never runs `rsync --delete`.
- It never deletes a local file during a pull.
- A remote file is eligible for deletion only if Ulak recorded that it sent
  the file and the file is now absent locally.
- Ulak does not delete an unclaimed server-created file merely because no local
  copy exists.
- Protected paths never enter the deletion ledger.
- More than 25 remote deletions require confirmation by default. Change the
  limit with `sync.max_delete` or `--max-delete`.

Each client and checkout gets its own remote workspace. Ulak locks the
workspace during changes and refuses if two clients claim the same workspace.
SSH host-key checking is never disabled.

## Known limits

- Ulak supports Unix systems only.
- For Compose, only referenced local paths are synchronized. Add a mount or
  another Compose reference when the stack needs another path.
- `.gitignore` applies to synchronization. Use `[sync] include` for a required
  ignored path.
- Absolute paths and paths under `~` in a Compose file belong to the server.
  `ulak doctor` checks whether they exist there.
- SSH tunnels forward TCP ports only. UDP ports are reported and skipped.
- Ports used through `network_mode: host` do not appear in the Compose port
  model, so Ulak cannot forward them automatically.
- Ulak has no three-way merge or conflict files. Avoid changing the same file
  on both machines at the same time.

## CI and temporary runners

You can configure Ulak without writing files. Set `ULAK_HOST` and, when
needed, provide key and `known_hosts` contents through environment variables.
Host-key checking stays enabled.

A fresh runner should also keep a stable workspace namespace and UUID. Persist
the Ulak state directory if server-side deletions must reconcile between jobs.
See [Environment variables](https://github.com/syy/ulak/blob/main/docs/env.md#ulak-in-ci-or-an-agent) before using
Ulak from a temporary runner.

## Demo

The included demo checks bind mounts, `configs`, `env_file`, build contexts,
and port forwarding:

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
