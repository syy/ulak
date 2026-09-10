# Ulak Development Guidelines

Ulak runs a local `docker compose` project on a remote server: rsync carries the files,
ssh runs docker there, the published ports come back to `localhost`.

This is the shared project guide for every coding agent. Tool-specific entry points may reference
it; keep project rules here and put only genuinely tool-specific behaviour in their own config.

## Commands

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings   # CI denies warnings; a lint in tests/ fails too
cargo test --bins                           # unit tests, no server
cargo test --workspace                      # unit tests plus e2e suites
cargo test --bins catalog::tests            # one unit-test module
cargo test --test e2e_sync                  # one e2e target
```

- **Unit tests are `--bins`, not `--lib`** — the crate has no `lib.rs`, so `--lib` runs nothing.
- `ULAK_TEST_E2E=skip cargo test --workspace` — the only sanctioned way to run without a docker
  daemon. The e2e targets still report `ok`: they return before doing anything, so a green
  workspace run says nothing about them. `ULAK_TEST_E2E=host:<name>` is what exercises them.
- `ULAK_TEST_E2E=host:<name>` — run e2e against a real server instead of the dockerized fixture.
- `ULAK_TEST_WRITE_MAP=1 cargo test --bins the_map_on_disk` — regenerate `docs/docker-commands.md`.
- `cargo test --bins` needs `docker buildx` and `eval "$(ssh-agent -s)"`; without buildx, use
  `cargo test --bins -- --skip with_a_real_buildx`.
- Do not pass `--nocapture` to the doctor tests — it turns stdout into a tty and fails the snapshot.

## The one idea

A project is not a directory tree. It is the **set of paths its compose files reference** — the
footprint. That is docker's own model, and every module is downstream of it. `src/footprint.rs`
explains it; read that header before touching anything that decides which bytes travel.

## Read the module header first

Design rationale lives in the `//!` headers. Read the owner before changing its decision:

| Question | Owner |
|---|---|
| Which compose call is this? | `invocation.rs` |
| Where does this docker command travel? | `catalog.rs` (the table), `docker.rs` (dispatch only) |
| Which local paths does this stack need? | `footprint.rs` |
| What does the compose model say? | `compose.rs` — resolved on the SERVER, never parsed locally |
| Where do server, footprint and anchor become one answer? | `config.rs` (`bind` returns `Bound`) |
| Which argv words name a local path? | `composepaths.rs`, `buildflags.rs`, `runspec.rs` |
| What may be deleted on the server? | `ledger.rs` |
| How do bytes move? | `sync.rs` (rsync profile + filter rank), `walk.rs`, `dockerignore.rs` |
| How does a word reach a remote shell? | `ssh.rs` (`sh_quote`), `bridge.rs` (local-file commands) |
| How long may a wait last? | `proc.rs` |
| What runs without anyone typing? | `service.rs`, `agent.rs`, `forward.rs`, `intent.rs` |
| How does a project's own script reach the server? | `shim.rs` (the stand-in `docker`) |
| What did ulak actually run? | `audit.rs` |

Update the header whenever its reason changes.

## The One-Answer Rule

Anything that could be computed twice is computed once, in a named place. Most long comments in
`src/` record the bug that happened when it was computed twice. **Adding a second answer is the
easiest way to break ulak silently.**

| Never | Use |
|---|---|
| re-derive a project name or re-parse argv | `Invocation` |
| a clap variant or a `dispatch` branch for a docker command | the `CATALOG` table |
| parse compose YAML locally (no YAML dep — keep it that way) | `docker compose config` on the server |
| interpolate a raw string into a remote command | `ssh::sh_quote`, one argv element at a time |
| `wait_with_output()` | `proc::run_bounded` + a shared `proc::Budget` |
| exit-code arithmetic in a route | `docker::status_code` |
| `rsync --delete` | the ledger + an explicit `rm` |
| compute `~/.local/state/ulak` yourself | `invocation::state_dir()` |
| `anyhow!`, `bail!`, `eprintln!` outside `ui.rs` | `ui::fail!(…).now(…).into_err()` |

## Data safety

Two copies of one tree make deletion dangerous. `rsync --delete` once ate a freshly generated
migration because absence on the local side is not permission to remove the remote side. Therefore:

- **rsync never carries `--delete`.** The doomed set comes from the ledger and is removed by an
  explicit `rm -f` / `rmdir -p` whose output is a receipt.
- A remote file may be deleted only if **the ledger claims it AND it is gone from local disk**.
  Anything else was born on the server and stays.
- The ledger merges; a writer records what it put there and retires only what the server confirmed
  losing under its own anchor.
- Ledger rows and sync receipts are keyed by workspace **and destination**; one server's deletion
  receipt can never retire another server's claim. The `WorkspaceLock` stays workspace-wide because
  both destinations can still pull into the same local checkout.
- `clean` retires only the selected destination's local receipts and caches; another server's
  ledger, stack declarations, UUID claim and anchor remain intact.
- `clean --forget-destination` removes only the `desired.json` declarations for that server and
  never contacts it; the ledger and receipts under `workspaces/` stay, because the bytes they
  describe stay wherever they were sent, and a stack's `tunnels.pid` stays for the orphan sweep.
  `intent.rs` owns the reasoning.
- `[sync] protect` paths neither travel nor enter the ledger.
- A ledger write holds a bound `WorkspaceLock`, never `let _ = …`, which drops immediately. The
  human `acquire`s; the service only `try_acquire`s and steps aside.
- `ui::confirm` answers NO without a terminal and treats Esc/Ctrl-C as NO.

The `sync.rs` and `ledger.rs` headers own the complete reasoning.

## Errors and output

`ui.rs` owns the contract: **no dead-end errors.** Build failures with
`ui::fail!(…).now(…).into_err()` and give the right concrete next step. Callers that report rather
than exit use `ui::flatten`, never `{e:#}`, which drops the now-steps.

Report output goes to stdout with `ui::style_stdout()`, everything else to stderr via
`ui::info/ok/warn/dim` — mixing them makes `ulak doctor > file` embed ANSI escapes. Refusals quote
the flag and value **as the user typed them** (`-f`, not `--file`). `ulak` lowercase is the
binary; `Ulak` capitalised is the product in prose.

## Changing a docker command

- Edit `CATALOG` in `catalog.rs`, never a clap variant or `dispatch` branch. Dispatch, help,
  completions, suggestions and the generated map derive from it.
- **A command the table lacks is refused by name, never forwarded on a guess.** Being told "not
  supported yet" costs a minute; being handed the wrong machine's filesystem costs an afternoon.
- Regenerate the map and update the `"N of Docker's M"` sentence in **both** `catalog.rs` and
  `cli.rs` when the table size changes; a test reads them back.
- Keep `builder` and `buildx` routes and client paths identical. Inspect the generated tree locally
  with `cargo run -p ulak -- docker --help`.
- Put flags that open a local file on an otherwise daemon-side command in the catalog entry's
  `client_paths`; put secret-bearing flags in `secret_flags` so every accepted spelling is redacted.
  If a secret would still appear in the remote process argv, refuse it rather than merely redacting
  Ulak's audit trail.

## Generated files

| File | Regenerate with |
|---|---|
| `docs/docker-commands.md` | `ULAK_TEST_WRITE_MAP=1 cargo test --bins the_map_on_disk` |
| `crates/ulak/src/snapshots/*.snap` | `cargo insta review` |
| `.github/workflows/release.yml` | edit `dist-workspace.toml`, regenerate with dist |

**Never hand-edit generated files.** `README.md` is hand-maintained and must follow behaviour
changes even though no test pins it.

## Tests

Unit tests sit at the bottom of the file they test; e2e tests are separate
`crates/ulak/tests/e2e_*.rs` targets. Read `tests/common/mod.rs`'s header before writing e2e tests.

- **A green tick over work that never ran is worse than a red one.** Never skip a test when a
  required tool is missing; assert with the tool name and exact fix command. A test also skips
  ITSELF when a loop or `filter` over it can match nothing — assert the set is non-empty before
  iterating. Measured: a test looping over "every variable marked secret" stayed green when the
  flag was dropped, while the private key printed in full.
- **Test the accepting case, not only the refusal.** Refusals are the interesting half and get
  written first: `env_bool` had tests for `false` and for garbage, so its `true` arm never ran.
- **No test may pull an image**: call `server.needs_image(...)`, and add new base images to CI's
  seed loop.
- Use `ULAK_TEST_E2E_HUB=block` to audit that the dockerized fixture cannot reach Docker Hub.
- Tests using `TestServer::shared()` run in parallel against one server; give every container, image,
  volume and other server-side resource a scenario-specific name.
- **Assert on bytes, not exit status** for anything about which machine holds a file.
- Test names are full sentences stating the rule, with no `test_` prefix.
- Source-pinning tests deliberately read source and assert call order; satisfy them when moving code.
- `invocation::state_dir()` deliberately returns a per-pid temp path under `cfg!(test)` so tests
  cannot damage developer state.

## Rust

CI runs clippy's default groups on Linux. It does not enforce these rules:

- Do not add `unwrap()` or `expect()` in production code. Existing uses encode proven invariants;
  new code returns `?` or a `fail!`.
- **Never call `process::exit`.** It skips the `Drop` guards that clear staging directories and
  locks. A route returns: a forwarded child's nonzero status is `Ok(docker::status_code(status))`,
  a real failure is an `Err` for `ui::render_error`.
- **Write state through `invocation::write_private` (0600), and tmp-plus-`rename` when another
  process reads the file** (`ledger::write_atomic`, `intent::write_atomic`). A plain `fs::write`
  truncates first, so a reader can see half a file.
- **`#[serde(deny_unknown_fields)]` belongs on the user-authored TOML structs in `config.rs` and
  nowhere else.** Anything parsing docker's JSON or ulak's own state takes `#[serde(default)]`, or
  a newer docker makes every command exit 1. Corollary: never delete a parsed-but-unused config
  field — `ServiceCfg::notify` is unread on purpose.
- **Path bytes never go through `from_utf8_lossy`** — that is for subprocess output. A U+FFFD in a
  ledger row makes `doomed` delete a different file; prove it with `String::from_utf8(…).ok()` or
  divert the name into `WalkReport::bad_names`.
- **Walk argv with an index loop against a value-flag table**, stepping past each flag's value.
  `args.iter().any(|a| a == "-f")` reads the `-f` in `--build-arg -f` as a flag.
- **Rewrite an argv word by `{index, span}`, right to left** (`runspec::Slot`). A
  `args[i].replace(local, remote)` corrupts `--secret id=npm,src=./a` and any word with two paths.
- Keep test seams out of the binary: no test-only `ULAK_*` switch or injected hook. Use an `_in`
  twin taking the root as a parameter (`invocation::capture_in`, `walk::plan_in`). User-facing
  `ULAK_*` configuration is a product surface, not a test seam — it is read once, in `config.rs`.
- **Use `cfg!(target_os = …)` when both platform arms must compile everywhere.** Reserve `#[cfg]`
  for code, such as a platform-only test, that genuinely cannot compile elsewhere.
- New modules are flat, one-word lowercase files (`composepaths.rs`); each owns one decision.

## Code style

- **Never split a docker flag's CSV value on commas** — go through the `csv_fields` / `fields`
  helpers. The three separate readers are deliberate, not debt.
- Split and test the pure half of `is_terminal()` decisions — and of anything whose only output
  is `println!` or `ui::*`: build the lines in a function a test can call, print them in a thin
  one. Cargo gives tests a pipe, not a tty.
- Prefer widening an existing table to writing a second parser, and name the other places that
  must not diverge, by function name.
- `BTreeMap` and explicit `sort` for anything reaching a command line or an audit trail.
- Spell directories with a trailing `/`, added by `ledger::dir_row`.

## Comments and evidence

Code shows what happens; comments preserve why the obvious alternative is wrong.

- Put decisions that cross functions or modules in the owning module's `//!` header, not beside
  every caller.
- Use inline comments for local traps: required ordering, ownership boundaries, data-loss risks,
  or external behaviour the code cannot express. Do not narrate the code.
- A test's doc comment records the concrete failure or regression it prevents. Put reproduction
  detail there instead of repeating the whole history in production code.
- Keep one answer: this shared guide summarizes the high-cost invariant and names its owner; the
  module header owns the full rationale; code, tables and tests own behaviour. Point to the owner
  rather than copying its algorithm.
- Claims about docker, compose, buildx, rsync or pflag that determine parsing or routing carry the
  reproducible command, observed behaviour and probed version. Re-measure when changing them.
- Keep exact timings, byte counts and similar measurements only when they justify a bound or
  threshold. Otherwise preserve the durable conclusion and prove it with a test where possible.
- Keep a past failure when it explains a tempting, dangerous alternative; otherwise Git history
  is enough.
- Match the module: `bridge.rs`, `proc.rs` and `sync.rs` need dense rationale; plumbing such as
  `init.rs` and `main.rs` should remain sparse.

## Commits

Use lowercase conventional commits with no emoji or AI co-author lines. Subjects feed the
changelog: write an imperative sentence about observable behaviour, often "X, not Y":

```
fix: read `-papi` as the project it names, not as a word to pass on
```

Types: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `ci`, `chore`. A breaking change takes
`!` before the colon and a `BREAKING CHANGE:` body line.

When a commit relates to a GitHub issue, add `refs #<n>` as a body line after the subject. Do not
use closing keywords (`fixes #n`, `closes #n`, `resolves #n`) — `main` carries unreleased work,
and an issue should not close before the release that ships the fix.

Propose the commit message and get alignment before committing.

## Environment

Unix only. No async runtime: use threads plus bounded subprocesses. `Cargo.toml` owns the MSRV and
`sync.rs` owns the rsync floor; keep `README.md` aligned with both. CI checks stable and the minimum
Rust version declared in `Cargo.toml`. macOS's built-in openrsync is deliberately refused.
