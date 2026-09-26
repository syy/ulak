# Environment variables

Use environment variables to configure Ulak without editing project files.
This also works for agents running in temporary or read-only checkouts.

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
never touched. Non-interactive runs do not create it. `ULAK_CONFIG` suppresses
it because the global layer has been relocated, and
`ULAK_GLOBAL_CONFIG_AUTO=false` stops it outright.

## Settings

| variable | setting | value |
|---|---|---|
| `ULAK_HOST` | `host` | SSH destination: an alias, a hostname, or `user@host` |
| `ULAK_WORKSPACE_NAMESPACE` | `workspace.namespace` | client namespace; otherwise one is minted locally |
| `ULAK_SYNC_EXCLUDE` | `sync.exclude` | gitignore patterns, **one per line** |
| `ULAK_SYNC_INCLUDE` | `sync.include` | force-included paths, one per line |
| `ULAK_SYNC_PROTECT` | `sync.protect` | server-owned paths, one per line |
| `ULAK_SYNC_MAX_DELETE` | `sync.max_delete` | a number, or `unlimited` |
| `ULAK_SYNC_NO_HISTORY` | `sync.no_history` | `send` or `hold` |
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
your own `ssh` would make, `ssh-agent` and `~/.ssh/config` included.

Setting both `ULAK_SSH_KEY` and `ULAK_SSH_KEY_FILE` is refused — picking
one would authenticate as an identity you did not choose. A key with a
passphrase is refused too: load it into `ssh-agent` instead. Host key
checking is **never** relaxed, in any mode; pin the key with
`ULAK_KNOWN_HOSTS` rather than accepting whatever answers.

`ULAK_SSH_KEY` takes either spelling a secret store produces — the
ordinary multi-line value, or a single line with literal `\n`. A missing
final newline is restored.

## Using Ulak from an agent

Give the agent a stable workspace namespace and UUID, and keep both values
for the life of its workspace:

```bash
export ULAK_HOST="user@server"
export ULAK_WORKSPACE_NAMESPACE="my-agent"
export ULAK_WORKSPACE_UUID="my-project"
ulak docker compose up -d
```

Normal SSH configuration still applies. If the environment has none, supply
`ULAK_SSH_KEY_FILE` or `ULAK_SSH_KEY`, and verified host-key contents through
`ULAK_KNOWN_HOSTS`.

Collect the server's host key before configuring access:

```bash
ssh-keyscan -t ed25519 server > known_hosts.candidate
ssh-keygen -lf known_hosts.candidate
```

Compare the fingerprint with the host key shown through the server's trusted
console or another previously verified channel. For example, on that console:

```bash
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
```

Only after the fingerprints match, put the contents of `known_hosts.candidate`
in `ULAK_KNOWN_HOSTS`. Use the key type your server offers if it does not use
Ed25519. [`ssh-keyscan` collects keys but does not authenticate them](https://man.openbsd.org/ssh-keyscan);
running it on a trusted computer alone does not verify the server's identity.

The two workspace settings serve different purposes:

- `ULAK_WORKSPACE_NAMESPACE` selects the client's remote location. Without a
  stable value or persisted local state, each fresh environment gets another
  workspace and leaves the previous one behind.
- `ULAK_WORKSPACE_UUID` declares ownership. If local state was lost, Ulak
  otherwise asks before reusing an existing workspace and refuses without a
  terminal. A namespace alone does not establish ownership.

Any stable one-word value works for either. Use distinct identities for
independent clients; sharing a checkout path does not mean sharing ownership.

**Persist Ulak's state directory between runs.** Under `$XDG_STATE_HOME/ulak`
(default `~/.local/state/ulak`), it holds the deletion ledger and fingerprints
of the content both ends last agreed on. Namespace and UUID do not replace it:

- Without the ledger, files removed from the checkout remain on the server.
  `ulak doctor` reports this when it sees a declared identity with no local record.
- Without common-content history, a differing server copy follows
  `sync.no_history`: `send` replaces it with the local copy and warns; `hold`
  keeps both copies until the conflict is resolved.

Non-interactive runs do not install the background service automatically.
Run `ulak service run --foreground` when the agent needs continuous sync and
port forwarding.

## Test hooks

`ULAK_TEST_*` belongs to Ulak's own test suite, not to the product:
`ULAK_TEST_E2E`, `ULAK_TEST_E2E_HUB`, `ULAK_TEST_E2E_SWARM`,
`ULAK_TEST_WRITE_MAP`. `ulak config` labels them as such.

Ulak says nothing about a `ULAK_*` name it does not recognise — the
prefix is not its property, and your own wrapper may own
`ULAK_DEPLOY_TARGET`. That means a typo is silent, so `ulak config` lists
every `ULAK_*` that is set: it is where a misspelling becomes visible.
