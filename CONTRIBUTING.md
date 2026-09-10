# Contributing

Open an issue before a large change. For a vulnerability, see
[`SECURITY.md`](SECURITY.md) instead of opening an issue.

## Read first

[`AGENTS.md`](AGENTS.md) is the project guide. It names the module that owns
each decision and lists the rules a patch is reviewed against. Read the owning
module's `//!` header before changing what it decides.

## Development setup

Install [Ulak's local prerequisites](README.md#on-your-computer), plus the Docker
CLI and Buildx. The Rust toolchain in `rust-toolchain.toml` includes rustfmt and
Clippy. A local Docker daemon is needed only for the dockerized e2e fixture.

On macOS, the Docker CLI and Buildx can be installed without Docker Desktop:

```bash
brew install rsync docker docker-buildx
mkdir -p ~/.docker/cli-plugins
ln -sfn "$(brew --prefix)/opt/docker-buildx/bin/docker-buildx" \
  ~/.docker/cli-plugins/docker-buildx
```

Check Buildx and start an ssh-agent before running unit tests. No key needs to
be loaded; Buildx uses the agent socket when evaluating the test definitions.

```bash
docker buildx version
eval "$(ssh-agent -s)"
```

## Checks

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --bins
```

Unit tests are `--bins`, not `--lib`; the crate has no `lib.rs`.

For e2e tests, start a Docker daemon and pull the images listed in the
[CI image seed step](.github/workflows/ci.yml). Tests copy those images from
your local daemon to the fixture and refuse to pull a missing image themselves.
Then run:

```bash
ULAK_TEST_E2E_HUB=block cargo test --workspace
```

`ULAK_TEST_E2E=host:<name>` uses a real server instead of the dockerized fixture.
Read the [test harness header](crates/ulak/tests/common/mod.rs) before using it.
`ULAK_TEST_E2E=skip cargo test --workspace` runs without a daemon, but the e2e
targets then report `ok` without testing anything.

Generated files are regenerated, never edited by hand; `AGENTS.md` lists them.
`README.md` is hand-maintained and must follow a behaviour change.

## Commits

Lowercase conventional commits: `feat`, `fix`, `refactor`, `perf`, `test`,
`docs`, `ci`, `chore`. The subject is an imperative sentence about observable
behaviour, since it feeds the changelog:

```
fix: read `-papi` as the project it names, not as a word to pass on
```

Reference an issue with `refs #<n>`, not with a closing keyword: `main` carries
unreleased work, and an issue should stay open until the release that fixes it.

## License

Contributions are licensed under MIT or Apache-2.0, at the user's option, like
the rest of the project. The root license files are authoritative; the links
under `crates/ulak/` include those same texts in the Cargo package.
