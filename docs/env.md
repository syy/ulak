# Environment variables

Every setting has one. A CI job or a coding agent gets a fresh machine and
a read-only checkout, so "edit `ulak.toml`" is advice it cannot take.

`ulak config` prints what is actually in effect and which layer won it —
reach for that before reading this table.

## The cascade

```
defaults
  → global:    ~/.config/ulak/config.toml   OR   $ULAK_CONFIG
  → project:   ulak.toml
  → local:     ulak.local.toml
  → ULAK_* variables
  → CLI flags
```

Later wins, field by field. **Lists replace, they do not merge** — on
every layer, the environment included. Setting `ULAK_SYNC_INCLUDE` means
you carry `.env` yourself if you still want it.

An **empty variable is an unset one**: `ULAK_HOST=""` leaves whatever the
files decided, so `ULAK_HOST=${MAYBE_UNSET}` in a pipeline degrades
predictably instead of failing on an empty destination.

`ULAK_CONFIG` **relocates** the global layer rather than adding one on
top of it — the shape `KUBECONFIG` and `RCLONE_CONFIG` use. The project's
own `ulak.toml` still wins over it; individual variables still win over
everything. A file it names that does not exist is refused, unlike a
discovered `ulak.toml`, which may simply be absent.

The global file is **written for you** the first time ulak runs in a
terminal: a fully commented `~/.config/ulak/config.toml` that changes no
setting by existing. It is where `[service]` has to live, since that is
the one setting no project may decide for a machine. An existing file is
never touched, nothing is written where stderr is not a terminal (so no
CI job or container grows one), `ULAK_CONFIG` suppresses it because the
global layer has been relocated, and `ULAK_GLOBAL_CONFIG_AUTO=false`
stops it outright.

## Settings

| variable | setting | value |
|---|---|---|
| `ULAK_HOST` | `host` | SSH destination: an alias, a hostname, or `user@host` |
| `ULAK_WORKSPACE_NAMESPACE` | `workspace.namespace` | client namespace; otherwise one is minted locally |
| `ULAK_SYNC_EXCLUDE` | `sync.exclude` | gitignore patterns, **one per line** |
| `ULAK_SYNC_INCLUDE` | `sync.include` | force-included paths, one per line |
| `ULAK_SYNC_PROTECT` | `sync.protect` | server-owned paths, one per line |
| `ULAK_SYNC_MAX_DELETE` | `sync.max_delete` | a number, or `unlimited` |
| `ULAK_FORWARD_AUTO` | `forward.auto` | `true` or `false` |
| `ULAK_SERVICE_AUTO` | `service.auto` | `true` or `false`; machine-wide, like the file it mirrors |

Booleans take **only** `true` and `false` — the spellings TOML already
accepts. Anything else is refused by name rather than read as `false`.

Lists split on newlines because a gitignore pattern may legitimately hold
a comma, a space or a colon; newline is the one byte it cannot. In YAML
use a block scalar, in a shell use `$'…'`:

```yaml
ULAK_SYNC_EXCLUDE: |
  *.log
  tmp/
```

```bash
ULAK_SYNC_EXCLUDE=$'*.log\ntmp/'
```

A single pattern needs neither: `ULAK_SYNC_EXCLUDE='*.log'`.

## Files, identity and the server

| variable | what it does |
|---|---|
| `ULAK_CONFIG` | TOML file to read instead of the global config |
| `ULAK_GLOBAL_CONFIG_AUTO` | `false` stops ulak writing the global config it offers on a first run |
| `ULAK_SSH_KEY` | private key **contents** — never printed, written 0600 |
| `ULAK_SSH_KEY_FILE` | private key **path** |
| `ULAK_KNOWN_HOSTS` | `known_hosts` contents to pin the server against |
| `ULAK_WORKSPACE_UUID` | this run's workspace identity (see below) |

With none of these set, nothing changes: connections are exactly the ones
your own `ssh` would make, agent and `~/.ssh/config` included.

Setting both `ULAK_SSH_KEY` and `ULAK_SSH_KEY_FILE` is refused — picking
one would authenticate as an identity you did not choose. A key with a
passphrase is refused too: load it into an agent instead. Host key
checking is **never** relaxed, in any mode; pin the key with
`ULAK_KNOWN_HOSTS` rather than accepting whatever answers.

`ULAK_SSH_KEY` takes either spelling a secret store produces — the
ordinary multi-line value, or a single line with literal `\n`. A missing
final newline is restored.

## Ulak in CI or an agent

```yaml
- run: ulak docker compose up -d
  env:
    ULAK_HOST:           deploy@server
    ULAK_SSH_KEY:        ${{ secrets.SSH_KEY }}
    ULAK_KNOWN_HOSTS:    ${{ secrets.KNOWN_HOSTS }}
    ULAK_WORKSPACE_NAMESPACE: ${{ vars.ULAK_WORKSPACE_NAMESPACE }}
    ULAK_WORKSPACE_UUID:      ${{ vars.ULAK_WORKSPACE_UUID }}
```

Collect the server's host key before configuring the job:

```bash
ssh-keyscan -t ed25519 server > known_hosts.candidate
ssh-keygen -lf known_hosts.candidate
```

Compare the fingerprint with the host key shown through the server's trusted
console or another previously verified channel. For example, on that console:

```bash
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
```

Only after the fingerprints match, store the contents of `known_hosts.candidate`
in `KNOWN_HOSTS`. Use the key type your server offers if it does not use
Ed25519. [`ssh-keyscan` collects keys but does not authenticate them](https://man.openbsd.org/ssh-keyscan);
running it on a trusted computer alone does not verify the server's identity.

The last two are the ones that are not obvious, and a wiped runner needs
**both** — they fix different halves of the same problem.

`ULAK_WORKSPACE_NAMESPACE` fixes **where**. It is half the remote
locator, and Ulak mints one into the state directory when you do not
supply it — so a fresh runner lands in a brand-new workspace every job.
Nothing fails; the server just grows another copy and orphans the last
one.

`ULAK_WORKSPACE_UUID` fixes **whose**. The workspace on the server
carries a random uuid, and a machine that has lost its own copy cannot
tell "mine, before the runner was wiped" from "another machine using the
same checkout path". Ulak asks a human, and with no terminal it refuses —
so with the namespace pinned but not this, the second job is refused
outright. Declaring the identity is that answer, given in advance.

Any stable one-word value works for either; keep both for the life of the
workspace.

**Deletions do not reconcile on a wiped runner.** The ledger — the record
of what this machine put on the server — lives in the state directory, so
a fresh one claims nothing and nothing is retired: files you delete from
the checkout stay on the server, and the workspace grows. `doctor` says
so. To reconcile, persist `$XDG_STATE_HOME` between runs, which also
restores the deletion budget's usual meaning.

Nothing here detects CI and changes its own defaults. An agent's microVM
may legitimately want the background service, and a rule that guessed
would be wrong for exactly that case.

## Test hooks

`ULAK_TEST_*` belongs to Ulak's own test suite, not to the product:
`ULAK_TEST_E2E`, `ULAK_TEST_E2E_HUB`, `ULAK_TEST_E2E_SWARM`,
`ULAK_TEST_WRITE_MAP`. `ulak config` labels them as such.

Ulak says nothing about a `ULAK_*` name it does not recognise — the
prefix is not its property, and your own wrapper may own
`ULAK_DEPLOY_TARGET`. That means a typo is silent, so `ulak config` lists
every `ULAK_*` that is set: it is where a misspelling becomes visible.
