# Ulak Development Guidelines

Ulak synchronizes a local project's files over SSH, runs Docker on the server,
and forwards published ports to localhost. This is the shared guide for coding
agents; tool-specific files should reference it.

## Commands

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --bins                          # unit tests, no server
cargo test --workspace                     # includes e2e
cargo test --bins catalog::tests           # one module
cargo test --test e2e_sync                  # one e2e target
```

- Unit tests use `--bins`, not `--lib`; there is no `lib.rs`.
- Unit tests need Docker Buildx, an ssh-agent (`eval "$(ssh-agent -s)"`) and Python 3.
  Without Buildx, use `cargo test --bins -- --skip with_a_real_buildx` and report the skip.
- `ULAK_TEST_E2E=host:<name>` exercises e2e on a real server.
- `ULAK_TEST_E2E=skip cargo test --workspace` is the only sanctioned daemon-free
  workspace run. E2e targets then report `ok` without running; never call that e2e validation.
- Do not use `--nocapture` with doctor tests; it breaks their output snapshots.
- See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and CI image seeding.

## Design and ownership

A project's **footprint is the set of paths its Compose model references**, not
its directory tree. Read `footprint.rs` before changing which bytes travel.
Compose is resolved on the server; never parse its YAML locally.

Read the owning module's `//!` header before changing its decision, and update
that header when its rationale changes. Modules below are in `crates/ulak/src/`.

| Decision | Owner |
|---|---|
| Invocation and project identity | `invocation.rs` |
| Docker command routes | `catalog.rs` (table), `docker.rs` (dispatch) |
| Compose model, footprint, bound configuration | `compose.rs`, `footprint.rs`, `config.rs` |
| Local paths in argv | `composepaths.rs`, `buildflags.rs`, `runspec.rs` |
| File selection and ignores | `walk.rs`, `dockerignore.rs`, `selection.rs` |
| Transfers, deletion and claims | `sync.rs`, `ledger.rs` |
| Common content and digest validity | `baseline.rs` / `baseline.py`, `hashcache.rs` |
| Small-file transport and safe local landing | `stream.rs` / `stream.py`, `dirfd.rs` |
| Remote change hints | `observer.rs` / `observer.py` |
| Shell quoting and local-file commands | `ssh.rs`, `bridge.rs` |
| Bounded subprocesses | `proc.rs` |
| Service, tunnels and persisted intent | `service.rs`, `agent.rs`, `forward.rs`, `intent.rs` |
| Project scripts' Docker shim | `shim.rs` |
| Audit records and user output | `audit.rs`, `ui.rs` |
| Dashboard terminal and effects; state and input; drawing | `tui.rs`, `app.rs`, `view.rs` |
| Dashboard server reads, streams, tasks and actions | `engine.rs`, `feed.rs`, `verbs.rs` |
| ANSI in container output | `ansi.rs` |

Compute each answer once, in its owner:

| Instead of | Use |
|---|---|
| Re-deriving identity or re-parsing argv | `Invocation` |
| Adding Docker clap variants or dispatch branches | `CATALOG` |
| Raw interpolation into a remote shell | `ssh::sh_quote` per argv element |
| `wait_with_output()` | `proc::run_bounded` with a shared `proc::Budget` |
| Exit-code arithmetic | `docker::status_code` |
| Hard-coded state directories | `invocation::state_dir()` |
| `anyhow!`, `bail!`, `eprintln!` outside `ui.rs` | `ui::fail!(…).now(…).into_err()` and `ui::*` output |

## Data safety

These are review invariants. The owning headers describe the algorithms and
edge cases; read them before changing sync, including performance changes.

- Never use `rsync --delete`. Remote deletion needs a ledger claim and local
  absence; use explicit removal or the channel's guarded unlink with receipts.
- Preserve server edits since agreed content. Unreadable or restructured paths
  are not deleted. No-history deletion, return marks and uncertain sends follow
  the rules in `sync.rs`, `baseline.rs` and `ledger.rs`.
- A refused push must not bypass its content guard through rsync; only
  `sync::RSYNC_CAN` reasons permit fallback. Preserve claim/receipt ordering and
  refusal rollback; a lost receipt is not proof of either success or failure.
- Common content advances only after both ends are observed equal. Pushes with
  no history follow `[sync] no_history`; recorded conflicts keep their guards.
  Local and server digest caches must obey the same `hashcache.rs` stamp rule.
- `[sync] protect` paths never travel or enter the ledger; previously claimed
  paths become ineligible for deletion when protected. Pulls never delete local files.
- Ledger updates merge and retire only confirmed losses under their anchor.
  Receipts and ledger rows are scoped to workspace **and destination**; locks
  remain workspace-wide because different destinations can pull into one checkout.
- Hold a bound `WorkspaceLock` for ledger writes, never `let _ = …`.
  Interactive work acquires it; the service uses `try_acquire` and steps aside.
- `clean` affects only the selected destination. `clean --forget-destination`
  removes its declarations without contacting the server or discarding receipts;
  `intent.rs` owns the state and orphan-tunnel rules.
- Channel pulls land through `stream::land` and `dirfd.rs`, holding directories
  open and refusing symlink traversal. Do not replace this with check-then-write paths.
- `ui::confirm` defaults to NO without a terminal and on Esc/Ctrl-C.

## Errors and output

Errors need a concrete next step: `ui::fail!(…).now(…).into_err()`. Report caught
errors with `ui::flatten`, not `{e:#}`, which loses those steps.
Reports use stdout with `ui::style_stdout()`; other output uses stderr through
`ui::info/ok/warn/dim`. Refusals preserve flags and values as typed (`-f`, not
`--file`). Use `ulak` for the binary and `Ulak` for the product.

## Docker commands and generated files

- Change `CATALOG`; refuse unknown commands by name, never forward on a guess.
- Keep `builder` and `buildx` routes and client paths identical. Inspect with
  `cargo run -p ulak -- docker --help`.
- Register local-file flags in `client_paths` and secrets in `secret_flags`.
  Refuse secrets that would remain visible in remote argv even after audit redaction.
- When the catalog size changes, update the count in both `catalog.rs` and
  `cli.rs`, and regenerate the command map.

| Generated file | Regenerate with |
|---|---|
| `docs/docker-commands.md` | `ULAK_TEST_WRITE_MAP=1 cargo test --bins the_map_on_disk` |
| `crates/ulak/src/snapshots/*.snap` | `cargo insta review` |
| `docs/images/dashboard-*.svg` | `ULAK_TEST_WRITE_PICTURES=1 cargo test --bins the_pictures_on_disk` |
| `.github/workflows/release.yml` | edit `dist-workspace.toml`, regenerate with dist |

Never hand-edit generated files. Keep user-visible behaviour documented in
`README.md`; link detailed usage from there instead of duplicating it.

## Tests

Unit tests live at the bottom of their module; e2e targets are in
`crates/ulak/tests/`. Read `tests/common/mod.rs`'s header before writing e2e tests.

- Missing required tools must fail with the tool name and fix command, not silently skip.
  Assert a filtered test set is non-empty before iterating; test accepting and refusing cases.
- Tests must never pull images: call `server.needs_image(...)` and add images to
  CI's seed loop. Use `ULAK_TEST_E2E_HUB=block` to audit the dockerized fixture.
- Shared-server tests need scenario-specific names for every server resource.
- Assert file bytes, not just exit status. Name tests as full rules without `test_`.
- Preserve source-pinning call-order assertions when moving code.
- `invocation::state_dir()` uses a per-pid test directory; never touch developer state.

## Rust and code style

CI denies Clippy warnings, including in tests. Also follow these project rules:

- No new production `unwrap()`/`expect()` and no `process::exit`; return errors or
  `docker::status_code` so `Drop` guards run.
- Write state with `invocation::write_private` (0600); use temporary-file plus
  rename when other processes read it (`ledger::write_atomic`, `intent::write_atomic`).
- `#[serde(deny_unknown_fields)]` belongs only on user TOML structs in `config.rs`.
  Docker JSON and internal state use `#[serde(default)]`. Preserve parsed-but-unused config fields.
- Never decode path bytes with `from_utf8_lossy`; validate UTF-8 or report `WalkReport::bad_names`.
- Walk argv by index using a value-flag table. Rewrite `{index, span}` right to
  left (`runspec::Slot`), never global string replacement.
- Parse Docker CSV values through `csv_fields` / `fields`, never `split(',')`.
- Keep test seams out of the binary: use parameterized `_in` helpers, not test-only
  `ULAK_*` switches. Read product environment settings once, in `config.rs`.
- Use `cfg!(target_os = …)` when both arms must compile everywhere; use `#[cfg]`
  only for genuinely platform-specific code.
- Production `unsafe` belongs only in checked `dirfd.rs` wrappers.
- New modules are flat, one-word lowercase files, each owning one decision.
- Separate and test pure output/terminal decisions; printing stays in thin wrappers.
- Extend existing tables before adding parsers. Name functions whose rules must agree.
- Sort command-line and audit output (`BTreeMap` or explicit sort).
  Directory ledger rows end in `/`; use `ledger::dir_row`.

## Comments and evidence

Keep cross-module rationale in the owner's `//!` header and regression details
in tests. Inline comments explain local ordering, ownership and safety traps.
Avoid repeating algorithms or bug histories in this guide or beside each caller.

External parsing/routing claims need a reproducible command, observed result
and tool version; re-measure when changing them. Keep exact timings and byte
counts only to justify a bound or threshold. Preserve past failures when they
explain a dangerous alternative; otherwise Git history suffices. Match the
module's density: core safety needs rationale, plumbing should stay sparse.

## Commits and environment

Use lowercase conventional commits (`feat`, `fix`, `refactor`, `perf`, `test`,
`docs`, `ci`, `chore`), an imperative subject describing observable behaviour,
and no emoji or AI co-author lines. Breaking changes use `!` and a
`BREAKING CHANGE:` body. Use `refs #<n>` for issues, not closing keywords;
`main` contains unreleased work. Propose the message and get alignment before committing.

Unix only; use threads and bounded subprocesses, no async runtime. `Cargo.toml`
owns the MSRV and `sync.rs` the rsync floor; keep README requirements aligned.
CI checks stable and MSRV. Apple's built-in openrsync is deliberately refused.
