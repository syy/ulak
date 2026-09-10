# Changelog

Notable changes to Ulak. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/spec/v2.0.0.html).

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
