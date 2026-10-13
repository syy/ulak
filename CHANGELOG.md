# Changelog

Notable changes to Ulak. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/spec/v2.0.0.html).

## 0.2.0

The first four changes below can change what an existing project sends or
requires; read them before upgrading.

### Changed

- **The server needs Python 3.7 or later.** Ulak sends its helpers over SSH
  without installing anything. A command that synchronizes stops before
  sending anything if the server lacks `python3`; `ulak doctor` reports it.
- **A file edited on both ends is held as a conflict instead of last write
  winning.** Sync compares both copies with the content the two ends last
  agreed on. A file changed on both is kept on both and named in a warning,
  and a file deleted here is not deleted there if the server changed it.
  Before there is agreed content, `[sync] no_history` decides: `send` (the
  default) sends this machine's copy and names each file it sent; `hold`
  keeps both. `ULAK_SYNC_NO_HISTORY` sets it too.
- **`.gitignore` is no longer read**, because Docker does not read it either.
  Mounted directories sync whole except `.git`, `.DS_Store` and Ulak's
  temporary files; build contexts are still filtered by `.dockerignore`. Use
  `[sync] exclude` to keep paths local; `[sync] include` overrides both.
- **`[sync]` paths are relative to the Compose project directory**, not to
  the workspace anchor; `../` reaches a sibling. Projects whose anchor is the
  project directory are unaffected.
- Saves, deletions and small server output travel over one persistent SSH
  channel; large files and new directories still use rsync. A full sync
  compares both ends by directory digests and hands rsync only what differs.
  On Linux servers with Python 3.9 or later, files the stack writes come back
  shortly after they change.
- Pulled files land through directories held open from the project root, so
  a directory swapped for a symlink cannot redirect them.
- The background service picks up `[sync]` changes without a restart.

### Added

- `ulak ui` (beta): a full-screen terminal dashboard for the server's Docker.
  It shows containers grouped by Compose project with live CPU and memory,
  merged stack logs with search, inspect, stats, images, volumes, networks
  and this machine's live stacks. Keys and layout may still change; see
  `docs/dashboard.md`.
- `ulak status`, `sync`, `doctor` and `clean` take a stack's Compose project
  name and work from any directory; `ulak status` alone shows every stack
  declared in the checkout. Write `name@server` when two servers have a stack
  of that name; see `docs/stacks.md`.
- `--raw` on those four commands prints suggested commands in full instead of
  by stack name.

### Fixed

- A file saved during a sync no longer becomes a permanent conflict: it is
  sent as it is then, or, if it changes while being read, waits for the next
  sync.
- A directory a pull brings home keeps the server's mode, so later pushes no
  longer fail on directories the SSH user does not own.
- A profile that stays inside the project keeps the wider layout the
  workspace already uses, instead of asking to run again on every run.
- After a reinstall, the service warns that it is still running the build it
  started from.

## 0.1.0

First public release. macOS and Linux.

- `ulak docker <command>` runs a supported Docker or Compose command on a
  server over SSH after rsync has placed the files it needs. No local daemon,
  no agent on the server.
- The Compose model, resolved on the server, decides which local paths travel:
  bind mounts, `env_file`, `configs`, build contexts, and siblings such as
  `../shared`.
- Commands are routed by a table; one the table lacks is refused by name.
  `docs/docker-commands.md` is generated from it.
- rsync never runs with `--delete`. A remote file is deleted only if Ulak
  recorded sending it and it is gone locally. More than 25 deletions ask first.
  `[sync] protect` paths never travel.
- Files written by containers come back to the local checkout. Last write
  wins.
- Published ports are forwarded to `localhost` over SSH tunnels.
- A user-level background service (launchd or systemd) keeps sync and tunnels
  alive for running stacks. It installs only from an interactive terminal;
  `[service] auto = false` turns it off.
- `ulak shim` puts a stand-in `docker` on `PATH` so project scripts reach the
  server unchanged.
- `ulak doctor` checks local tools, the server, and every path in the Compose
  model.
- A running stack keeps its declared server after a `host` edit;
  `ulak clean --forget-destination` retires a declaration for a dead server
  without contacting it.
- Configuration in three TOML layers, `ULAK_*` variables, and flags;
  `ulak config` shows the effective value and its source.
- `ulak init`, `ulak sync --dry-run`, `ulak status`, `ulak completions`.
- Prebuilt binaries for macOS (arm64, x86_64) and Linux (x86_64, aarch64),
  and a Homebrew tap: `brew install syy/tap/ulak`.
