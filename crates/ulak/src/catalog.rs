//! The map: every command path Docker's CLI exposes, and how Ulak
//! carries it to the server.
//!
//! Why a table and not a rule. Before this file, `ulak docker` was a
//! hand-written clap enum with eleven branches, so 316 of Docker's 327
//! command paths were answered with "not a root command" — including
//! `exec`, `logs` and `cp`, which is most of what anybody types. The
//! obvious fix, "forward whatever we do not recognise", is the wrong
//! one: `docker cp ./x web:/x` and `docker run -v .:/app` LOOK like
//! plain daemon calls and are not — forwarded blindly they read paths
//! that exist on the wrong machine, silently. So the routing decision is
//! written down once, per command path, and everything else derives from
//! it: the dispatcher, `ulak docker --help`, shell completions and the
//! support tracker.
//!
//! A command Docker adds tomorrow is NOT forwarded on a guess. It gets
//! an error that names it, because being told "Ulak does not know this
//! one yet" costs a minute and being handed the wrong machine's
//! filesystem costs an afternoon.
//!
//! The 327 paths were measured, not recalled: every `--help` walked
//! recursively, unioned with `docker __complete`, then every known verb
//! probed against every family. That last pass is what found the 40-odd
//! paths Docker ships but never prints — `docker volume list`, `docker
//! image remove`, `docker buildx b`, `docker system dial-stdio`, the
//! whole `docker compose alpha` subtree, and the Swarm families, which
//! are hidden until a daemon joins a swarm and work regardless. A
//! router built from help output alone rejects all of them.

use crate::ui::fail;

/// How one command path reaches the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Not a command: a namespace whose children are.
    Family,

    /// `ssh -> docker …`. Inputs and effects live entirely in the
    /// daemon, so there is no workspace to sync and no path to rewrite:
    /// object ids, labels and Go templates mean the same thing from any
    /// client.
    Daemon,

    /// Daemon, plus a stream that outlives the call: a TTY, a follow, a
    /// live stat feed.
    ///
    /// The transport is the same one `Daemon` uses and the dispatcher
    /// treats them alike today — the distinction is a claim about the
    /// COMMAND, not yet a difference in what happens to it: these hold
    /// the user's terminal for as long as they run, so nothing that
    /// waits on a lock may ever be put in front of them. Kept separate
    /// so that when something is, the list already exists rather than
    /// having to be re-derived from what each verb happens to do.
    Stream,

    /// Compose's own dispatcher: workspace sync, `up`/`down` intent,
    /// tunnel lifecycle. Ulak stops parsing here and hands the rest of
    /// argv to `passthrough`.
    Compose,

    /// A build whose context, Dockerfile and inputs are local: the
    /// footprint is synced and the argv rewritten before the daemon
    /// sees it.
    Build,

    /// `stack config`/`deploy`: a Compose model resolved for Swarm.
    /// The compose files and everything they reference are local, so
    /// they travel and the `-c` paths are respelled.
    Stack,

    /// `bake`: many builds from one file. Buildx is asked what the file
    /// resolves to, every local path in the answer is synced, and the
    /// argv travels unchanged — bake resolves relative paths against the
    /// working directory, and the remote one mirrors this one.
    Bake,

    /// A local file crosses the wire as a stream — never as a remote
    /// path the server would resolve against the wrong filesystem.
    Bridge(Bridge),

    /// `run`/`create`: bind sources, env and label files are local, so
    /// the smallest safe footprint is synced and argv rewritten.
    Footprint,

    /// Deliberately not proxied. The string says what to do instead.
    Excluded(&'static str),

    /// Recognised and mapped, transport not built yet. The string names
    /// what is missing, so the answer is never a puzzle.
    Planned(&'static str),
}

/// Which direction a local file travels, and how it is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bridge {
    /// `docker cp` — a tar stream, either direction, container or host.
    Cp,
    /// Remote stdout lands in a local file (`save`, `export`).
    Pull,
    /// A local file feeds remote stdin (`load`, `import`).
    Push,
    /// A local file feeds remote stdin AND must never be written to
    /// disk on the way (`secret create`, `config create`).
    Confidential,
}

/// A flag whose value names a path on the machine that TYPED it, on a
/// command that is otherwise pure daemon state.
///
/// Most of the daemon-routed tree forwards safely because its arguments
/// are object ids, labels and Go templates, which mean the same thing
/// from any client. A handful of flags are not: `docker exec --env-file
/// .env`, `docker swarm ca --ca-cert ca.pem` and `docker buildx
/// imagetools create -f desc.json` all open the file on the CLIENT,
/// before a daemon is dialled. Each was settled by pointing it at a path
/// that does not exist on Docker 29.4.0 and watching which error came
/// back: `docker exec --env-file /nonexistent/x NOCTR true` says "open
/// /nonexistent/x: no such file or directory", while the same line
/// without the flag says "No such container" — the file loses the race
/// to the daemon, so the daemon never sees the command at all.
///
/// Forwarded, these read the SERVER's filesystem. That is the failure
/// this whole file exists to prevent, and it is the quiet kind: a server
/// that happens to have a file by that name answers with the wrong
/// contents and no complaint.
///
/// Ulak does not sync them. A command with no workspace has nowhere to
/// put a copy and nobody to delete it afterwards, so the answer is the
/// one the catalog gives an unknown command: refuse, name the flag, and
/// say what to do instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientPath {
    /// Every spelling of the flag. The error names the one that was
    /// actually typed, because that is the one on screen.
    pub flags: &'static [&'static str],
    /// Set when the path is one field of a csv value rather than the
    /// whole of it: `--external-ca protocol=cfssl,cacert=./ca.pem`. The
    /// key is matched case-insensitively because Docker matches it that
    /// way — `CACERT=` loads the same file.
    pub field: Option<&'static str>,
    /// True when Docker stops reading its own flags at the first
    /// positional, because everything after it belongs to a command
    /// running inside the container. `docker exec web myprog --env-file
    /// conf` is myprog's flag, and refusing that would be a lie.
    pub before_the_command: bool,
    /// What to do instead, in the imperative. Never empty: an error that
    /// only says no is a dead end.
    pub instead: &'static str,
}

impl ClientPath {
    /// The part of a flag's value that is a path here, or `None` when
    /// this particular value carries none — `--external-ca` without a
    /// `cacert=` field names nothing local, and Docker accepts it.
    fn path_in(&self, value: &str) -> Option<String> {
        match self.field {
            None => (!value.is_empty()).then(|| value.to_string()),
            Some(field) => csv_fields(value)?.iter().find_map(|part| {
                let (key, path) = part.split_once('=')?;
                key.trim()
                    .eq_ignore_ascii_case(field)
                    .then(|| path.to_string())
            }),
        }
    }
}

/// The fields of a csv flag value, read the way Docker reads them.
///
/// Docker parses `--external-ca` as one CSV RECORD, not as a string
/// split on every comma: a field that BEGINS with a quote runs to its
/// matching one and may hold commas, and `""` inside it is one literal
/// quote. Measured on 29.4.0 — `swarm ca --external-ca
/// '"cacert=/nonexistent,x.pem",protocol=cfssl,url=https://x'` answers
/// `open /nonexistent,x.pem`, and `'"cacert=/no""pe.pem",protocol=cfssl'`
/// answers `open /no"pe.pem`.
///
/// Splitting on every comma instead produced a field keyed `"cacert`,
/// which is not `cacert` at all: `path_in` found nothing, the refusal
/// below never fired, and the command was forwarded for the SERVER's PEM
/// to be read in its place. That is this file's own failure mode
/// arriving through a quoting rule, and it is why `runspec` reads
/// `--mount` the same way.
///
/// `None` is the answer for a record Docker itself refuses — a bare
/// quote in an unquoted field, or a quote that never closes, both
/// measured as parse errors before any file is opened. There is no path
/// here to be sure of, and the client that receives the value stops on
/// the same error, out loud.
fn csv_fields(value: &str) -> Option<Vec<String>> {
    fn quoted(body: &str) -> Option<(String, &str)> {
        let mut text = String::new();
        let mut rest = body;
        loop {
            let close = rest.find('"')?;
            text.push_str(&rest[..close]);
            rest = &rest[close + 1..];
            match rest.strip_prefix('"') {
                Some(after) => {
                    text.push('"');
                    rest = after;
                }
                None => return Some((text, rest)),
            }
        }
    }

    let mut fields = Vec::new();
    let mut rest = value;
    loop {
        let (text, tail) = match rest.strip_prefix('"') {
            Some(body) => quoted(body)?,
            None => {
                let end = rest.find(',').unwrap_or(rest.len());
                let (field, tail) = rest.split_at(end);
                // A quote that starts nothing is the record Docker
                // rejects as "bare \" in non-quoted-field".
                if field.contains('"') {
                    return None;
                }
                (field.to_string(), tail)
            }
        };
        fields.push(text);
        match tail.strip_prefix(',') {
            Some(next) => rest = next,
            // Anything but a comma after a closing quote is the record
            // Docker rejects as "extraneous or missing \" in
            // quoted-field", and reading past it would be a guess.
            None => return tail.is_empty().then_some(fields),
        }
    }
}

#[derive(Debug)]
pub struct Entry {
    /// The command path, e.g. `["container", "run"]`.
    pub path: &'static [&'static str],
    pub route: Route,
    /// Docker's own one-line description.
    pub about: &'static str,
    /// Set when this path is another spelling of a command that also
    /// lives elsewhere on this list — Docker's root-level shorthands
    /// (`ps` for `container ls`) and its unprinted long forms (`volume
    /// list` for `volume ls`). Both spellings are real and both work;
    /// recording the link is what stops the map counting one command
    /// twice, and what lets a test insist the two never drift onto
    /// different routes.
    pub alias_of: Option<&'static [&'static str]>,
    /// Flags whose VALUE must never reach the audit trail. Command
    /// scoped on purpose: `-p` is a password to `login` and a published
    /// port to `run`, and a redactor that cannot tell them apart either
    /// leaks the first or corrupts the second.
    pub secret_flags: &'static [&'static str],
    /// Flags of THIS command whose value is a path on this machine.
    /// Command scoped for the same reason `secret_flags` is: `-f` is a
    /// source descriptor to `imagetools create`, a Dockerfile to
    /// `build`, and a compose file to everything under `compose`.
    pub client_paths: &'static [ClientPath],
    /// The letters of THIS command's short flags that take no value, so
    /// a bundle can be read the way pflag reads it: `-Df/desc.json` is
    /// `-D -f /desc.json`, and without knowing that `-D` is a boolean
    /// the `-f` inside it is invisible and its path is forwarded.
    ///
    /// Only the commands with a `ClientPath` on a SHORT flag need one,
    /// which today is `imagetools create` alone. Empty everywhere else,
    /// and empty is the safe reading: nothing is peeled, so a bundle is
    /// simply not recognised rather than misread.
    pub short_booleans: &'static [char],
}

impl Entry {
    pub fn is_family(&self) -> bool {
        matches!(self.route, Route::Family)
    }

    /// The command as a human writes it, without the `docker` prefix.
    pub fn name(&self) -> String {
        self.path.join(" ")
    }
}

const fn c(path: &'static [&'static str], route: Route, about: &'static str) -> Entry {
    Entry {
        path,
        route,
        about,
        alias_of: None,
        secret_flags: &[],
        client_paths: &[],
        short_booleans: &[],
    }
}

/// Same command, root-level spelling.
const fn a(
    path: &'static [&'static str],
    route: Route,
    about: &'static str,
    of: &'static [&'static str],
) -> Entry {
    Entry {
        path,
        route,
        about,
        alias_of: Some(of),
        secret_flags: &[],
        client_paths: &[],
        short_booleans: &[],
    }
}

const fn s(
    path: &'static [&'static str],
    route: Route,
    about: &'static str,
    secret_flags: &'static [&'static str],
) -> Entry {
    Entry {
        path,
        route,
        about,
        alias_of: None,
        secret_flags,
        client_paths: &[],
        short_booleans: &[],
    }
}

/// A command one of whose flags names a file on this machine.
const fn f(
    path: &'static [&'static str],
    route: Route,
    about: &'static str,
    client_paths: &'static [ClientPath],
) -> Entry {
    Entry {
        path,
        route,
        about,
        alias_of: None,
        secret_flags: &[],
        client_paths,
        short_booleans: &[],
    }
}

/// The root-level spelling of one. `docker exec` and `docker container
/// exec` are the same command, so the flag list has to travel with both
/// spellings or the refusal is only half there.
const fn af(
    path: &'static [&'static str],
    route: Route,
    about: &'static str,
    of: &'static [&'static str],
    client_paths: &'static [ClientPath],
) -> Entry {
    Entry {
        path,
        route,
        about,
        alias_of: Some(of),
        secret_flags: &[],
        client_paths,
        short_booleans: &[],
    }
}

/// One of those whose flag has a SHORT spelling, so a boolean can bundle
/// in front of it and hide it. The table is what lets that be seen.
const fn fb(
    path: &'static [&'static str],
    route: Route,
    about: &'static str,
    client_paths: &'static [ClientPath],
    short_booleans: &'static [char],
) -> Entry {
    Entry {
        path,
        route,
        about,
        alias_of: None,
        secret_flags: &[],
        client_paths,
        short_booleans,
    }
}

use Bridge::{Confidential, Cp, Pull, Push};
use Route::{
    Bake, Bridge as Br, Build, Compose, Daemon, Excluded, Family, Footprint, Planned, Stack, Stream,
};

/// Why the client's own state is not something a remote runner should
/// be editing on your behalf.
const CONTEXT_IS_LOCAL: &str = "Docker contexts are this machine's client state; Ulak picks the server from the \
     workspace config instead";

const DESKTOP_IS_LOCAL: &str = "this reaches into Docker Desktop, which runs on THIS machine and not on the server; \
     run it with plain `docker` here";

/// The `docker builder` alias is a key in `~/.docker/config.json`, which
/// is client state in exactly the way a context is.
const ALIAS_IS_LOCAL: &str = "install and uninstall rewrite the `docker builder` alias in ~/.docker/config.json on \
     whichever machine runs them, so forwarded they would edit the SERVER's client config and \
     leave this one alone — which is nobody's intent. Buildx 0.33.0 calls both deprecated in \
     any case: type `docker buildx` directly";

/// Measured, not guessed: `docker manifest inspect alpine` answers in
/// full with `DOCKER_HOST` pointed at a socket that does not exist, and
/// `manifest push nosuchrepo/nosuch:v1` answers "No such manifest" —
/// a lookup in a file store, not a daemon call.
const MANIFEST_STORE_IS_LOCAL: &str = "a manifest list is assembled in ~/.docker/manifests on the machine that runs the \
     command and pushed straight to the registry — no daemon takes part, which is why \
     forwarding these looked harmless. Forwarded, the list is built in the SERVER's store, \
     where the `docker manifest push` you type next cannot find it; run them here with plain \
     `docker`";

/// Measured on Buildx 0.33.0, which is what the rest of this map was
/// measured against: the policy commands are not newer than it and not
/// missing from it, they simply read a tree that is here and not there.
const POLICY_READS_A_LOCAL_TREE: &str = "`policy eval` takes a local source and reads the `.rego` policy file beside its \
     Dockerfile, and `policy test` takes a local test path — the same footprint `build` already \
     syncs, not yet wired to these two";

/// `--addr` defaults to `127.0.0.1:0`, so the viewer is served on the
/// loopback of whichever machine runs the command.
const TRACE_SERVES_A_UI: &str = "trace serves the trace viewer on 127.0.0.1 of whichever machine runs it, so forwarded it \
     binds a port on the server and prints a URL no browser here can open — it wants the tunnel \
     Ulak already builds for Compose, which is not wired to it yet";

const PLUGIN_ROOTFS: &str = "plugin create reads a local rootfs directory that Ulak does not sync yet — build the \
     plugin on the server, or push it to a registry and `plugin install` from there";

const DEBUG_ADAPTER: &str =
    "the build debugger speaks a local protocol over stdio that Ulak does not bridge yet";

// ─── flags that name a file on this machine ─────────────────────────

/// `--env-file` on `exec` and `service create`.
///
/// Both open it on the client: `docker exec --env-file /nonexistent/x
/// NOCTR true` answers "open /nonexistent/x: no such file or directory"
/// where the same line without the flag answers "No such container".
/// `service update` is absent from this list because it has no
/// `--env-file` at all — the client says "unknown flag".
const ENV_FILE: &[ClientPath] = &[ClientPath {
    flags: &["--env-file"],
    field: None,
    before_the_command: true,
    instead: "pass the variables themselves — `-e NAME=value` travels on the command line, and \
              the file stays where you can still read it",
}];

/// What to do with CA material that only this machine holds. There is no
/// syncing it: a root CA copied into a synced workspace is a private key
/// in a directory rsync deletes from.
const CA_ON_THE_SERVER: &str = "put the PEM on the server and run it there over ssh — `ulak status` names the host \
     this workspace is bound to";

/// The path hidden inside `--external-ca`'s csv value. pflag parses the
/// whole spec at PARSE time and opens `cacert=` while doing it, so
/// `docker swarm init --external-ca protocol=cfssl,url=…,cacert=/nope`
/// fails with `invalid argument … for "--external-ca" flag` and never
/// reaches a daemon. `swarm ca`, `swarm init` and `swarm update` all
/// take it; `swarm join` does not.
const EXTERNAL_CA: ClientPath = ClientPath {
    flags: &["--external-ca"],
    field: Some("cacert"),
    before_the_command: false,
    instead: CA_ON_THE_SERVER,
};

const EXTERNAL_CA_ONLY: &[ClientPath] = &[EXTERNAL_CA];

/// `swarm ca`'s own PEM flags are pflag Values that open the file while
/// parsing: `--ca-cert /nonexistent/ca.pem` answers `invalid argument
/// "/nonexistent/ca.pem" for "--ca-cert" flag: open …`. The daemon is
/// never asked, which is why this one reads as a plain daemon call and
/// is not.
const SWARM_CA: &[ClientPath] = &[
    ClientPath {
        flags: &["--ca-cert"],
        field: None,
        before_the_command: false,
        instead: CA_ON_THE_SERVER,
    },
    ClientPath {
        flags: &["--ca-key"],
        field: None,
        before_the_command: false,
        instead: CA_ON_THE_SERVER,
    },
    EXTERNAL_CA,
];

/// `buildx create` reads its BuildKit config on the CLIENT and keeps
/// only what it read.
///
/// Measured twice, because the flag reads as daemon configuration and
/// is not. A path that is not there stops the command before a builder
/// exists at all: `--buildkitd-config /nonexistent.toml` answers
/// "buildkit configuration file not found: … stat …: no such file or
/// directory" and `buildx ls` is unchanged. A path that IS there gets
/// its CONTENTS embedded, base64, in `~/.docker/buildx/instances/<name>`
/// under `"Files"`, and the path itself appears nowhere in the record —
/// the TOML is parsed and re-serialized on the way in, which is
/// something only the reader can do.
///
/// So forwarded, this reads the server's disk for a file the user meant
/// from theirs. Refused for now; carrying it needs only a sync and an
/// argv rewrite, because the record the server would end up with is
/// byte-identical either way and no path survives to keep consistent.
///
/// `--driver-opt cacert=` is deliberately NOT here: `buildx create`
/// performs no read for it, storing the literal path string and nothing
/// else. It resolves later, on whichever machine builds against the
/// builder — a different failure, at a different time, and not one this
/// refusal would describe honestly.
const BUILDKITD_CONFIG: &[ClientPath] = &[ClientPath {
    flags: &["--buildkitd-config", "--config"],
    field: None,
    before_the_command: false,
    instead: "point it at a config already on the server, or create the builder there",
}];

/// `imagetools create` reads its source descriptors and writes its
/// result metadata on the CLIENT, in both directions: `--dry-run -f
/// /nonexistent/desc.json` answers "ERROR: open /nonexistent/desc.json:
/// no such file or directory" without touching a registry, and
/// `--metadata-file` puts the answer on the machine that ran the
/// command. Everything else it takes is a registry reference, which is
/// why the rest of it forwards perfectly well.
const IMAGETOOLS_FILES: &[ClientPath] = &[
    ClientPath {
        flags: &["--file", "-f"],
        field: None,
        before_the_command: false,
        instead: "name the source images on the command line instead — a registry reference means \
                  the same thing from either machine",
    },
    ClientPath {
        flags: &["--metadata-file"],
        field: None,
        before_the_command: false,
        instead: "drop it: the file would be written on the server, out of reach here — \
                  `imagetools inspect` on the new tag afterwards gives you the digest",
    },
];

/// Which of `imagetools create`'s short flags take no value, and so can
/// sit in front of its `-f` in one word.
///
/// Asked of the client one flag at a time on Buildx 0.33.0, because
/// arity is the one thing a parser answers without being given a valid
/// command: `imagetools create -D` runs on to "no sources specified",
/// while `-f`, `-p` and `-t` each stop at "flag needs an argument: 'f'
/// in -f". So `-D` is the whole table, and the other three are what
/// makes it matter that the table is not just "every letter": `-tf/x`
/// is a tag, measured, and `-Df/x` is a descriptor.
const IMAGETOOLS_BOOLEANS: &[char] = &['D'];

/// Every command path, in Docker's own order: root commands first, then
/// each family's tree. Sorted within a family so a reader can find a
/// verb without searching, and so the tracker generator produces a
/// stable document.
#[rustfmt::skip]
pub static CATALOG: &[Entry] = &[
    // ── root: the shorthands people actually type ────────────────────
    a(&["attach"], Stream, "Attach local standard input, output, and error streams to a running container", &["container", "attach"]),
    c(&["bake"], Bake, "Build from a file"),
    c(&["build"], Build, "Build an image from a Dockerfile"),
    a(&["commit"], Daemon, "Create a new image from a container's changes", &["container", "commit"]),
    a(&["cp"], Br(Cp), "Copy files/folders between a container and the local filesystem", &["container", "cp"]),
    a(&["create"], Footprint, "Create a new container", &["container", "create"]),
    a(&["diff"], Daemon, "Inspect changes to files or directories on a container's filesystem", &["container", "diff"]),
    a(&["events"], Stream, "Get real time events from the server", &["system", "events"]),
    af(&["exec"], Stream, "Execute a command in a running container", &["container", "exec"], ENV_FILE),
    a(&["export"], Br(Pull), "Export a container's filesystem as a tar archive", &["container", "export"]),
    a(&["history"], Daemon, "Show the history of an image", &["image", "history"]),
    a(&["images"], Daemon, "List images", &["image", "ls"]),
    a(&["import"], Br(Push), "Import the contents from a tarball to create a filesystem image", &["image", "import"]),
    a(&["info"], Daemon, "Display system-wide information", &["system", "info"]),
    c(&["inspect"], Daemon, "Return low-level information on Docker objects"),
    a(&["kill"], Daemon, "Kill one or more running containers", &["container", "kill"]),
    a(&["load"], Br(Push), "Load an image from a tar archive or STDIN", &["image", "load"]),
    s(&["login"], Stream, "Authenticate to a registry", &["-p", "--password"]),
    c(&["logout"], Daemon, "Log out from a registry"),
    a(&["logs"], Stream, "Fetch the logs of a container", &["container", "logs"]),
    a(&["pause"], Daemon, "Pause all processes within one or more containers", &["container", "pause"]),
    a(&["port"], Daemon, "List port mappings or a specific mapping for the container", &["container", "port"]),
    a(&["ps"], Daemon, "List containers", &["container", "ls"]),
    a(&["pull"], Daemon, "Download an image from a registry", &["image", "pull"]),
    a(&["push"], Daemon, "Upload an image to a registry", &["image", "push"]),
    a(&["rename"], Daemon, "Rename a container", &["container", "rename"]),
    a(&["restart"], Daemon, "Restart one or more containers", &["container", "restart"]),
    a(&["rm"], Daemon, "Remove one or more containers", &["container", "rm"]),
    a(&["rmi"], Daemon, "Remove one or more images", &["image", "rm"]),
    a(&["run"], Footprint, "Create and run a new container from an image", &["container", "run"]),
    a(&["save"], Br(Pull), "Save one or more images to a tar archive", &["image", "save"]),
    c(&["search"], Daemon, "Search Docker Hub for images"),
    a(&["start"], Stream, "Start one or more stopped containers", &["container", "start"]),
    a(&["stats"], Stream, "Display a live stream of container(s) resource usage statistics", &["container", "stats"]),
    a(&["stop"], Daemon, "Stop one or more running containers", &["container", "stop"]),
    a(&["tag"], Daemon, "Create a tag TARGET_IMAGE that refers to SOURCE_IMAGE", &["image", "tag"]),
    a(&["top"], Daemon, "Display the running processes of a container", &["container", "top"]),
    a(&["unpause"], Daemon, "Unpause all processes within one or more containers", &["container", "unpause"]),
    a(&["update"], Daemon, "Update configuration of one or more containers", &["container", "update"]),
    c(&["version"], Daemon, "Show the Docker version information"),
    a(&["wait"], Stream, "Block until one or more containers stop, then print their exit codes", &["container", "wait"]),

    // ── builder ──────────────────────────────────────────────────────
    c(&["builder"], Family, "Manage builds"),
    a(&["builder", "b"], Build, "Start a build", &["builder", "build"]),
    c(&["builder", "bake"], Bake, "Build from a file"),
    c(&["builder", "build"], Build, "Start a build"),
    f(&["builder", "create"], Daemon, "Create a new builder instance", BUILDKITD_CONFIG),
    c(&["builder", "dap"], Family, "Start debug adapter protocol compatible debugger"),
    c(&["builder", "dap", "attach"], Planned(DEBUG_ADAPTER), "Attach to a debug session"),
    c(&["builder", "dap", "build"], Planned(DEBUG_ADAPTER), "Start a build"),
    c(&["builder", "debug"], Family, "Start debug adapter protocol compatible debugger"),
    a(&["builder", "debug", "b"], Planned(DEBUG_ADAPTER), "Start a build", &["builder", "debug", "build"]),
    c(&["builder", "debug", "build"], Planned(DEBUG_ADAPTER), "Start a build"),
    c(&["builder", "dial-stdio"], Stream, "Proxy current stdio streams to builder instance"),
    c(&["builder", "du"], Daemon, "Disk usage"),
    a(&["builder", "f"], Bake, "Build from a file", &["builder", "bake"]),
    c(&["builder", "history"], Family, "Commands to work on build records"),
    c(&["builder", "history", "export"], Br(Pull), "Export build records into Docker Desktop bundle"),
    c(&["builder", "history", "import"], Excluded(DESKTOP_IS_LOCAL), "Import build records into Docker Desktop"),
    // Not a namespace, however much its help page looks like one.
    // Invoked bare on Buildx 0.33.0 it prints the LATEST build record;
    // with a REF it prints that one; with `--format json` it prints it
    // as JSON — and it still owns `attachment`. Every other family in
    // this catalog was invoked bare against the same Buildx and answered
    // with its own usage line, so this node, in its two spellings, is
    // the only one in Docker's tree that is both. Routed `Family`, the
    // REF was rejected as an unknown child, a flag as a stray word, and
    // the bare form printed Ulak's own listing and exited 0 — which a
    // script reads as a record inspected and found empty.
    c(&["builder", "history", "inspect"], Daemon, "Inspect a build"),
    c(&["builder", "history", "inspect", "attachment"], Daemon, "Inspect a build record attachment"),
    c(&["builder", "history", "logs"], Stream, "Print the logs of a build record"),
    // `history ls --local` filters by "current repository", which Buildx
    // resolves from the git remote of the working directory — the
    // server's, once forwarded, and a daemon call runs in the ssh
    // session's home. It stays Daemon because it fails closed rather
    // than quietly: outside a repository Buildx answers "could not get
    // remote URL for local filter", which names its own problem.
    c(&["builder", "history", "ls"], Daemon, "List build records"),
    c(&["builder", "history", "open"], Excluded(DESKTOP_IS_LOCAL), "Open a build record in Docker Desktop"),
    c(&["builder", "history", "rm"], Daemon, "Remove build records"),
    c(&["builder", "history", "trace"], Planned(TRACE_SERVES_A_UI), "Show the OpenTelemetry trace of a build record"),
    c(&["builder", "imagetools"], Family, "Commands to work on images in registry"),
    fb(&["builder", "imagetools", "create"], Daemon, "Create a new image based on source images", IMAGETOOLS_FILES, IMAGETOOLS_BOOLEANS),
    c(&["builder", "imagetools", "inspect"], Daemon, "Show details of an image in the registry"),
    c(&["builder", "inspect"], Daemon, "Inspect current builder instance"),
    c(&["builder", "install"], Excluded(ALIAS_IS_LOCAL), "Install buildx as a 'docker builder' alias"),
    c(&["builder", "ls"], Daemon, "List builder instances"),
    c(&["builder", "policy"], Family, "Commands to work on build policies"),
    c(&["builder", "policy", "eval"], Planned(POLICY_READS_A_LOCAL_TREE), "Evaluate a policy"),
    c(&["builder", "policy", "test"], Planned(POLICY_READS_A_LOCAL_TREE), "Test a policy"),
    c(&["builder", "prune"], Daemon, "Remove build cache"),
    c(&["builder", "rm"], Daemon, "Remove one or more builder instances"),
    c(&["builder", "stop"], Daemon, "Stop builder instance"),
    c(&["builder", "uninstall"], Excluded(ALIAS_IS_LOCAL), "Uninstall the 'docker builder' alias"),
    c(&["builder", "use"], Daemon, "Set the current builder instance"),
    c(&["builder", "version"], Daemon, "Show buildx version information"),

    // ── buildx (the same plugin under its own name) ──────────────────
    c(&["buildx"], Family, "Docker Buildx"),
    a(&["buildx", "b"], Build, "Start a build", &["buildx", "build"]),
    c(&["buildx", "bake"], Bake, "Build from a file"),
    c(&["buildx", "build"], Build, "Start a build"),
    f(&["buildx", "create"], Daemon, "Create a new builder instance", BUILDKITD_CONFIG),
    c(&["buildx", "dap"], Family, "Start debug adapter protocol compatible debugger"),
    c(&["buildx", "dap", "attach"], Planned(DEBUG_ADAPTER), "Attach to a debug session"),
    c(&["buildx", "dap", "build"], Planned(DEBUG_ADAPTER), "Start a build"),
    c(&["buildx", "debug"], Family, "Start debug adapter protocol compatible debugger"),
    a(&["buildx", "debug", "b"], Planned(DEBUG_ADAPTER), "Start a build", &["buildx", "debug", "build"]),
    c(&["buildx", "debug", "build"], Planned(DEBUG_ADAPTER), "Start a build"),
    c(&["buildx", "dial-stdio"], Stream, "Proxy current stdio streams to builder instance"),
    c(&["buildx", "du"], Daemon, "Disk usage"),
    a(&["buildx", "f"], Bake, "Build from a file", &["buildx", "bake"]),
    c(&["buildx", "history"], Family, "Commands to work on build records"),
    c(&["buildx", "history", "export"], Br(Pull), "Export build records into Docker Desktop bundle"),
    c(&["buildx", "history", "import"], Excluded(DESKTOP_IS_LOCAL), "Import build records into Docker Desktop"),
    c(&["buildx", "history", "inspect"], Daemon, "Inspect a build"),
    c(&["buildx", "history", "inspect", "attachment"], Daemon, "Inspect a build record attachment"),
    c(&["buildx", "history", "logs"], Stream, "Print the logs of a build record"),
    c(&["buildx", "history", "ls"], Daemon, "List build records"),
    c(&["buildx", "history", "open"], Excluded(DESKTOP_IS_LOCAL), "Open a build record in Docker Desktop"),
    c(&["buildx", "history", "rm"], Daemon, "Remove build records"),
    c(&["buildx", "history", "trace"], Planned(TRACE_SERVES_A_UI), "Show the OpenTelemetry trace of a build record"),
    c(&["buildx", "imagetools"], Family, "Commands to work on images in registry"),
    fb(&["buildx", "imagetools", "create"], Daemon, "Create a new image based on source images", IMAGETOOLS_FILES, IMAGETOOLS_BOOLEANS),
    c(&["buildx", "imagetools", "inspect"], Daemon, "Show details of an image in the registry"),
    c(&["buildx", "inspect"], Daemon, "Inspect current builder instance"),
    c(&["buildx", "install"], Excluded(ALIAS_IS_LOCAL), "Install buildx as a 'docker builder' alias"),
    c(&["buildx", "ls"], Daemon, "List builder instances"),
    c(&["buildx", "policy"], Family, "Commands to work on build policies"),
    c(&["buildx", "policy", "eval"], Planned(POLICY_READS_A_LOCAL_TREE), "Evaluate a policy"),
    c(&["buildx", "policy", "test"], Planned(POLICY_READS_A_LOCAL_TREE), "Test a policy"),
    c(&["buildx", "prune"], Daemon, "Remove build cache"),
    c(&["buildx", "rm"], Daemon, "Remove one or more builder instances"),
    c(&["buildx", "stop"], Daemon, "Stop builder instance"),
    c(&["buildx", "uninstall"], Excluded(ALIAS_IS_LOCAL), "Uninstall the 'docker builder' alias"),
    c(&["buildx", "use"], Daemon, "Set the current builder instance"),
    c(&["buildx", "version"], Daemon, "Show buildx version information"),

    // ── checkpoint ───────────────────────────────────────────────────
    c(&["checkpoint"], Family, "Manage checkpoints"),
    c(&["checkpoint", "create"], Daemon, "Create a checkpoint from a running container"),
    a(&["checkpoint", "list"], Daemon, "List checkpoints for a container", &["checkpoint", "ls"]),
    c(&["checkpoint", "ls"], Daemon, "List checkpoints for a container"),
    a(&["checkpoint", "remove"], Daemon, "Remove a checkpoint", &["checkpoint", "rm"]),
    c(&["checkpoint", "rm"], Daemon, "Remove a checkpoint"),

    // ── compose: its own dispatcher owns everything below this ───────
    c(&["compose"], Compose, "Docker Compose"),
    c(&["compose", "alpha"], Compose, ""),
    c(&["compose", "alpha", "generate"], Compose, "Generate a Compose file from existing containers"),
    c(&["compose", "alpha", "publish"], Compose, "Publish compose application"),
    c(&["compose", "alpha", "viz"], Compose, "Generate a graphviz graph from your compose file"),
    c(&["compose", "attach"], Compose, "Attach local standard input, output, and error streams to a service's running container"),
    c(&["compose", "bridge"], Compose, "Convert compose files into another model"),
    c(&["compose", "bridge", "convert"], Compose, "Convert compose files to Kubernetes manifests"),
    c(&["compose", "bridge", "transformations"], Compose, "Manage transformation images"),
    c(&["compose", "bridge", "transformations", "create"], Compose, "Create a new transformation"),
    c(&["compose", "bridge", "transformations", "list"], Compose, "List available transformations"),
    c(&["compose", "bridge", "transformations", "ls"], Compose, "List available transformations"),
    c(&["compose", "build"], Compose, "Build or rebuild services"),
    c(&["compose", "commit"], Compose, "Create a new image from a service container's changes"),
    c(&["compose", "config"], Compose, "Parse, resolve and render compose file in canonical format"),
    c(&["compose", "cp"], Compose, "Copy files/folders between a service container and the local filesystem"),
    c(&["compose", "create"], Compose, "Creates containers for a service"),
    c(&["compose", "down"], Compose, "Stop and remove containers, networks"),
    c(&["compose", "events"], Compose, "Receive real time events from containers"),
    c(&["compose", "exec"], Compose, "Execute a command in a running container"),
    c(&["compose", "export"], Compose, "Export a service container's filesystem as a tar archive"),
    c(&["compose", "images"], Compose, "List images used by the created containers"),
    c(&["compose", "kill"], Compose, "Force stop service containers"),
    c(&["compose", "logs"], Compose, "View output from containers"),
    c(&["compose", "ls"], Compose, "List running compose projects"),
    c(&["compose", "pause"], Compose, "Pause services"),
    c(&["compose", "port"], Compose, "Print the public port for a port binding"),
    c(&["compose", "ps"], Compose, "List containers"),
    c(&["compose", "publish"], Compose, "Publish compose application"),
    c(&["compose", "pull"], Compose, "Pull service images"),
    c(&["compose", "push"], Compose, "Push service images"),
    c(&["compose", "restart"], Compose, "Restart service containers"),
    c(&["compose", "rm"], Compose, "Removes stopped service containers"),
    c(&["compose", "run"], Compose, "Run a one-off command on a service"),
    c(&["compose", "scale"], Compose, "Scale services"),
    c(&["compose", "start"], Compose, "Start services"),
    c(&["compose", "stats"], Compose, "Display a live stream of container(s) resource usage statistics"),
    c(&["compose", "stop"], Compose, "Stop services"),
    c(&["compose", "top"], Compose, "Display the running processes"),
    c(&["compose", "unpause"], Compose, "Unpause services"),
    c(&["compose", "up"], Compose, "Create and start containers"),
    c(&["compose", "version"], Compose, "Show the Docker Compose version information"),
    c(&["compose", "volumes"], Compose, "List volumes used by the created containers"),
    c(&["compose", "wait"], Compose, "Block until containers of all (or specified) services stop"),
    c(&["compose", "watch"], Compose, "Watch build context for service and rebuild/refresh containers when files are updated"),

    // ── config (Swarm) ───────────────────────────────────────────────
    c(&["config"], Family, "Manage Swarm configs"),
    c(&["config", "create"], Br(Confidential), "Create a config from a file or STDIN"),
    c(&["config", "inspect"], Daemon, "Display detailed information on one or more configs"),
    a(&["config", "list"], Daemon, "List configs", &["config", "ls"]),
    c(&["config", "ls"], Daemon, "List configs"),
    a(&["config", "remove"], Daemon, "Remove one or more configs", &["config", "rm"]),
    c(&["config", "rm"], Daemon, "Remove one or more configs"),

    // ── container ────────────────────────────────────────────────────
    c(&["container"], Family, "Manage containers"),
    c(&["container", "attach"], Stream, "Attach local standard input, output, and error streams to a running container"),
    c(&["container", "commit"], Daemon, "Create a new image from a container's changes"),
    c(&["container", "cp"], Br(Cp), "Copy files/folders between a container and the local filesystem"),
    c(&["container", "create"], Footprint, "Create a new container"),
    c(&["container", "diff"], Daemon, "Inspect changes to files or directories on a container's filesystem"),
    f(&["container", "exec"], Stream, "Execute a command in a running container", ENV_FILE),
    c(&["container", "export"], Br(Pull), "Export a container's filesystem as a tar archive"),
    c(&["container", "inspect"], Daemon, "Display detailed information on one or more containers"),
    c(&["container", "kill"], Daemon, "Kill one or more running containers"),
    a(&["container", "list"], Daemon, "List containers", &["container", "ls"]),
    c(&["container", "logs"], Stream, "Fetch the logs of a container"),
    c(&["container", "ls"], Daemon, "List containers"),
    c(&["container", "pause"], Daemon, "Pause all processes within one or more containers"),
    c(&["container", "port"], Daemon, "List port mappings or a specific mapping for the container"),
    c(&["container", "prune"], Daemon, "Remove all stopped containers"),
    a(&["container", "ps"], Daemon, "List containers", &["container", "ls"]),
    a(&["container", "remove"], Daemon, "Remove one or more containers", &["container", "rm"]),
    c(&["container", "rename"], Daemon, "Rename a container"),
    c(&["container", "restart"], Daemon, "Restart one or more containers"),
    c(&["container", "rm"], Daemon, "Remove one or more containers"),
    c(&["container", "run"], Footprint, "Create and run a new container from an image"),
    c(&["container", "start"], Stream, "Start one or more stopped containers"),
    c(&["container", "stats"], Stream, "Display a live stream of container(s) resource usage statistics"),
    c(&["container", "stop"], Daemon, "Stop one or more running containers"),
    c(&["container", "top"], Daemon, "Display the running processes of a container"),
    c(&["container", "unpause"], Daemon, "Unpause all processes within one or more containers"),
    c(&["container", "update"], Daemon, "Update configuration of one or more containers"),
    c(&["container", "wait"], Stream, "Block until one or more containers stop, then print their exit codes"),

    // ── context: this machine's client state, deliberately untouched ─
    c(&["context"], Family, "Manage contexts"),
    c(&["context", "create"], Excluded(CONTEXT_IS_LOCAL), "Create a context"),
    c(&["context", "export"], Excluded(CONTEXT_IS_LOCAL), "Export a context to a tar archive"),
    c(&["context", "import"], Excluded(CONTEXT_IS_LOCAL), "Import a context from a tar archive"),
    c(&["context", "inspect"], Excluded(CONTEXT_IS_LOCAL), "Display detailed information on one or more contexts"),
    a(&["context", "list"], Excluded(CONTEXT_IS_LOCAL), "List contexts", &["context", "ls"]),
    c(&["context", "ls"], Excluded(CONTEXT_IS_LOCAL), "List contexts"),
    a(&["context", "remove"], Excluded(CONTEXT_IS_LOCAL), "Remove one or more contexts", &["context", "rm"]),
    c(&["context", "rm"], Excluded(CONTEXT_IS_LOCAL), "Remove one or more contexts"),
    c(&["context", "show"], Excluded(CONTEXT_IS_LOCAL), "Print the name of the current context"),
    c(&["context", "update"], Excluded(CONTEXT_IS_LOCAL), "Update a context"),
    c(&["context", "use"], Excluded(CONTEXT_IS_LOCAL), "Set the current docker context"),

    // ── image ────────────────────────────────────────────────────────
    c(&["image"], Family, "Manage images"),
    c(&["image", "build"], Build, "Build an image from a Dockerfile"),
    c(&["image", "history"], Daemon, "Show the history of an image"),
    c(&["image", "import"], Br(Push), "Import the contents from a tarball to create a filesystem image"),
    c(&["image", "inspect"], Daemon, "Display detailed information on one or more images"),
    a(&["image", "list"], Daemon, "List images", &["image", "ls"]),
    c(&["image", "load"], Br(Push), "Load an image from a tar archive or STDIN"),
    c(&["image", "ls"], Daemon, "List images"),
    c(&["image", "prune"], Daemon, "Remove unused images"),
    c(&["image", "pull"], Daemon, "Download an image from a registry"),
    c(&["image", "push"], Daemon, "Upload an image to a registry"),
    a(&["image", "remove"], Daemon, "Remove one or more images", &["image", "rm"]),
    c(&["image", "rm"], Daemon, "Remove one or more images"),
    a(&["image", "rmi"], Daemon, "Remove one or more images", &["image", "rm"]),
    c(&["image", "save"], Br(Pull), "Save one or more images to a tar archive"),
    c(&["image", "tag"], Daemon, "Create a tag TARGET_IMAGE that refers to SOURCE_IMAGE"),

    // ── manifest: a file store here, not daemon state anywhere ───────
    //
    // `inspect` is the exception and stays: it asks the REGISTRY rather
    // than the store, so running it on the server is a real choice —
    // the server's credentials and network are the ones that can reach
    // a registry this machine cannot.
    c(&["manifest"], Family, "Manage Docker image manifests and manifest lists"),
    c(&["manifest", "annotate"], Excluded(MANIFEST_STORE_IS_LOCAL), "Add additional information to a local image manifest"),
    c(&["manifest", "create"], Excluded(MANIFEST_STORE_IS_LOCAL), "Create a local manifest list for annotating and pushing to a registry"),
    c(&["manifest", "inspect"], Daemon, "Display an image manifest, or manifest list"),
    c(&["manifest", "push"], Excluded(MANIFEST_STORE_IS_LOCAL), "Push a manifest list to a repository"),
    c(&["manifest", "rm"], Excluded(MANIFEST_STORE_IS_LOCAL), "Delete one or more manifest lists from local storage"),

    // ── network ──────────────────────────────────────────────────────
    c(&["network"], Family, "Manage networks"),
    c(&["network", "connect"], Daemon, "Connect a container to a network"),
    c(&["network", "create"], Daemon, "Create a network"),
    c(&["network", "disconnect"], Daemon, "Disconnect a container from a network"),
    c(&["network", "inspect"], Daemon, "Display detailed information on one or more networks"),
    a(&["network", "list"], Daemon, "List networks", &["network", "ls"]),
    c(&["network", "ls"], Daemon, "List networks"),
    c(&["network", "prune"], Daemon, "Remove all unused networks"),
    a(&["network", "remove"], Daemon, "Remove one or more networks", &["network", "rm"]),
    c(&["network", "rm"], Daemon, "Remove one or more networks"),

    // ── node (Swarm) ─────────────────────────────────────────────────
    c(&["node"], Family, "Manage Swarm nodes"),
    c(&["node", "demote"], Daemon, "Demote one or more nodes from manager in the swarm"),
    c(&["node", "inspect"], Daemon, "Display detailed information on one or more nodes"),
    a(&["node", "list"], Daemon, "List nodes in the swarm", &["node", "ls"]),
    c(&["node", "ls"], Daemon, "List nodes in the swarm"),
    c(&["node", "promote"], Daemon, "Promote one or more nodes to manager in the swarm"),
    c(&["node", "ps"], Daemon, "List tasks running on one or more nodes, defaults to current node"),
    a(&["node", "remove"], Daemon, "Remove one or more nodes from the swarm", &["node", "rm"]),
    c(&["node", "rm"], Daemon, "Remove one or more nodes from the swarm"),
    c(&["node", "update"], Daemon, "Update a node"),

    // ── plugin ───────────────────────────────────────────────────────
    c(&["plugin"], Family, "Manage plugins"),
    c(&["plugin", "create"], Planned(PLUGIN_ROOTFS), "Create a plugin from a rootfs and configuration"),
    c(&["plugin", "disable"], Daemon, "Disable a plugin"),
    c(&["plugin", "enable"], Daemon, "Enable a plugin"),
    c(&["plugin", "inspect"], Daemon, "Display detailed information on one or more plugins"),
    c(&["plugin", "install"], Stream, "Install a plugin"),
    a(&["plugin", "list"], Daemon, "List plugins", &["plugin", "ls"]),
    c(&["plugin", "ls"], Daemon, "List plugins"),
    c(&["plugin", "push"], Daemon, "Push a plugin to a registry"),
    a(&["plugin", "remove"], Daemon, "Remove one or more plugins", &["plugin", "rm"]),
    c(&["plugin", "rm"], Daemon, "Remove one or more plugins"),
    c(&["plugin", "set"], Daemon, "Change settings for a plugin"),
    c(&["plugin", "upgrade"], Stream, "Upgrade an existing plugin"),

    // ── secret (Swarm) ───────────────────────────────────────────────
    c(&["secret"], Family, "Manage Swarm secrets"),
    c(&["secret", "create"], Br(Confidential), "Create a secret from a file or STDIN"),
    c(&["secret", "inspect"], Daemon, "Display detailed information on one or more secrets"),
    a(&["secret", "list"], Daemon, "List secrets", &["secret", "ls"]),
    c(&["secret", "ls"], Daemon, "List secrets"),
    a(&["secret", "remove"], Daemon, "Remove one or more secrets", &["secret", "rm"]),
    c(&["secret", "rm"], Daemon, "Remove one or more secrets"),

    // ── service (Swarm) ──────────────────────────────────────────────
    c(&["service"], Family, "Manage Swarm services"),
    f(&["service", "create"], Daemon, "Create a new service", ENV_FILE),
    c(&["service", "inspect"], Daemon, "Display detailed information on one or more services"),
    a(&["service", "list"], Daemon, "List services", &["service", "ls"]),
    c(&["service", "logs"], Stream, "Fetch the logs of a service or task"),
    c(&["service", "ls"], Daemon, "List services"),
    c(&["service", "ps"], Daemon, "List the tasks of one or more services"),
    a(&["service", "remove"], Daemon, "Remove one or more services", &["service", "rm"]),
    c(&["service", "rm"], Daemon, "Remove one or more services"),
    c(&["service", "rollback"], Daemon, "Revert changes to a service's configuration"),
    c(&["service", "scale"], Daemon, "Scale one or multiple replicated services"),
    c(&["service", "update"], Daemon, "Update a service"),

    // ── stack (Swarm) ────────────────────────────────────────────────
    c(&["stack"], Family, "Manage Swarm stacks"),
    c(&["stack", "config"], Stack, "Outputs the final config file, after doing merges and interpolations"),
    c(&["stack", "deploy"], Stack, "Deploy a new stack or update an existing stack"),
    a(&["stack", "down"], Daemon, "Remove one or more stacks", &["stack", "rm"]),
    a(&["stack", "list"], Daemon, "List stacks", &["stack", "ls"]),
    c(&["stack", "ls"], Daemon, "List stacks"),
    c(&["stack", "ps"], Daemon, "List the tasks in the stack"),
    a(&["stack", "remove"], Daemon, "Remove one or more stacks", &["stack", "rm"]),
    c(&["stack", "rm"], Daemon, "Remove one or more stacks"),
    c(&["stack", "services"], Daemon, "List the services in the stack"),
    a(&["stack", "up"], Stack, "Deploy a new stack or update an existing stack", &["stack", "deploy"]),

    // ── swarm ────────────────────────────────────────────────────────
    c(&["swarm"], Family, "Manage Swarm"),
    f(&["swarm", "ca"], Daemon, "Display and rotate the root CA", SWARM_CA),
    f(&["swarm", "init"], Daemon, "Initialize a swarm", EXTERNAL_CA_ONLY),
    s(&["swarm", "join"], Daemon, "Join a swarm as a node and/or manager", &["--token"]),
    c(&["swarm", "join-token"], Daemon, "Manage join tokens"),
    c(&["swarm", "leave"], Daemon, "Leave the swarm"),
    c(&["swarm", "unlock"], Stream, "Unlock swarm"),
    c(&["swarm", "unlock-key"], Daemon, "Manage the unlock key"),
    f(&["swarm", "update"], Daemon, "Update the swarm", EXTERNAL_CA_ONLY),

    // ── system ───────────────────────────────────────────────────────
    c(&["system"], Family, "Manage Docker"),
    c(&["system", "df"], Daemon, "Show docker disk usage"),
    c(&["system", "dial-stdio"], Stream, "Proxy the daemon socket to stdio"),
    c(&["system", "events"], Stream, "Get real time events from the server"),
    c(&["system", "info"], Daemon, "Display system-wide information"),
    c(&["system", "prune"], Daemon, "Remove unused data"),

    // ── volume ───────────────────────────────────────────────────────
    c(&["volume"], Family, "Manage volumes"),
    c(&["volume", "create"], Daemon, "Create a volume"),
    c(&["volume", "inspect"], Daemon, "Display detailed information on one or more volumes"),
    a(&["volume", "list"], Daemon, "List volumes", &["volume", "ls"]),
    c(&["volume", "ls"], Daemon, "List volumes"),
    c(&["volume", "prune"], Daemon, "Remove unused local volumes"),
    a(&["volume", "remove"], Daemon, "Remove one or more volumes", &["volume", "rm"]),
    c(&["volume", "rm"], Daemon, "Remove one or more volumes"),
    c(&["volume", "update"], Daemon, "Update a volume (cluster volumes only)"),

];

/// Docker's own global flags that carry a value. Skipped when looking
/// for the command word, so `docker --log-level debug ps` still finds
/// `ps` and not `debug`.
const GLOBAL_VALUE_FLAGS: &[&str] = &["-l", "--log-level"];

/// Global flags Ulak refuses rather than forwards, each with the reason
/// it cannot mean here what it means locally.
const REJECTED_GLOBALS: &[(&str, &str)] = &[
    (
        "-H",
        "the daemon is chosen by this workspace config, not by a flag",
    ),
    (
        "--host",
        "the daemon is chosen by this workspace config, not by a flag",
    ),
    (
        "-c",
        "Docker contexts are local client state; Ulak selects the server from your workspace config",
    ),
    (
        "--context",
        "Docker contexts are local client state; Ulak selects the server from your workspace config",
    ),
    (
        "--config",
        "this points at a client config directory on THIS machine, which the server cannot read",
    ),
    (
        "--tlscacert",
        "TLS material named here lives on this machine, not on the server that would use it",
    ),
    (
        "--tlscert",
        "TLS material named here lives on this machine, not on the server that would use it",
    ),
    (
        "--tlskey",
        "TLS material named here lives on this machine, not on the server that would use it",
    ),
];

/// One resolved command: which entry it is, and the argv to carry —
/// path words included, because Docker on the far side needs them.
#[derive(Debug)]
pub struct Resolved {
    pub entry: &'static Entry,
    /// The full remote argv: the command path followed by everything
    /// the user typed after it, with Docker's own globals preserved in
    /// front. This is what a `docker` on the server would receive.
    pub argv: Vec<String>,
    /// Just the part after the command path — what a parser for this
    /// specific command (build inputs, bind mounts) has to read.
    pub tail_start: usize,
    /// How many of Docker's own globals came before the command. Every
    /// route carries them in `argv`; `compose` is the one that cannot,
    /// because it hands the tail to a dispatcher that builds its own
    /// `docker compose` prefix — so it has to REFUSE them rather than
    /// drop them quietly.
    pub globals: usize,
}

impl Resolved {
    /// The command's own arguments — argv with the command path and
    /// Docker's globals cut off the front. A parser that reads these
    /// cannot mistake `run` for a container name or `cp` for a file.
    pub fn tail(&self) -> &[String] {
        &self.argv[self.tail_start..]
    }
}

pub fn find(path: &[&str]) -> Option<&'static Entry> {
    CATALOG.iter().find(|e| e.path == path)
}

/// Direct children of `path`, in catalog order.
pub fn children(path: &[&str]) -> Vec<&'static Entry> {
    CATALOG
        .iter()
        .filter(|e| e.path.len() == path.len() + 1 && e.path.starts_with(path))
        .collect()
}

/// Turn what the user typed after `ulak docker` into one catalog entry.
///
/// The scan is deliberately shallow: it walks command words only while
/// the catalog says the current node is a family, and stops at the
/// first real command. That is what keeps `docker exec -it web sh` from
/// reading `sh` as a subcommand, and `docker network create ls` from
/// reading `ls` as one.
/// The nodes that are a working command AND own subcommands.
///
/// Listed rather than derived from "has children", because a family
/// quietly given a body has exactly that shape too, and the structure
/// tests below should catch that rather than wave it through. `compose`
/// is deliberately not here: it parses its own tree, so the walk stops
/// dead at the word `compose` rather than stepping one further.
const BOTH_A_COMMAND_AND_A_NAMESPACE: &[&[&str]] = &[
    &["builder", "history", "inspect"],
    &["buildx", "history", "inspect"],
];

/// Whether the command-path walk steps past a node that is already a
/// command.
///
/// Only for the nodes that are both, and only when the next word really
/// names one of their children — so `inspect attachment x` reaches the
/// `attachment` entry while `inspect <ref>` and `inspect --format json`
/// stop at `inspect` and hand the rest over as its arguments. A REF
/// spelled exactly `attachment` walks on, which is not a bug to fix
/// here: Docker's own parser reads that word the same way.
fn walks_on(path: &[&str], next: Option<&String>) -> bool {
    let Some(next) = next else { return false };
    if !BOTH_A_COMMAND_AND_A_NAMESPACE.contains(&path) {
        return false;
    }
    let mut probe = path.to_vec();
    probe.push(next.as_str());
    find(&probe).is_some()
}

pub fn resolve(argv: &[String]) -> anyhow::Result<Resolved> {
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        if let Some((flag, why)) = rejected_global(arg) {
            return Err(fail!("`{flag}` cannot be forwarded: {why}")
                .now("drop the flag; `ulak init <host>` is how the server is chosen")
                .into_err());
        }
        let flag = arg.split('=').next().unwrap_or(arg);
        if GLOBAL_VALUE_FLAGS.contains(&flag) && !arg.contains('=') {
            i += 1;
        }
        i += 1;
    }

    let mut path: Vec<&str> = Vec::new();
    let mut entry: Option<&'static Entry> = None;
    let mut j = i;
    while j < argv.len() {
        let word = argv[j].as_str();
        // A flag ends the command path: everything from here belongs to
        // the command itself.
        if word.starts_with('-') && word != "-" {
            break;
        }
        path.push(word);
        match find(&path) {
            Some(found) => {
                entry = Some(found);
                j += 1;
                if !found.is_family() && !walks_on(&path, argv.get(j)) {
                    break;
                }
            }
            None => {
                let bad = path.pop().unwrap_or_default();
                return Err(unknown(&path, bad));
            }
        }
    }

    let Some(entry) = entry else {
        return Err(fail!("`ulak docker` needs a Docker command")
            .now("for example: ulak docker ps  ·  ulak docker compose up -d")
            .now("`ulak docker --help` lists the whole tree")
            .into_err());
    };

    // A family with a word after it that is not one of its children was
    // already rejected above; a family with nothing after it is a help
    // request, which the dispatcher answers.
    let mut out: Vec<String> = argv[..i].to_vec();
    let tail_start = out.len() + entry.path.len();
    out.extend(entry.path.iter().map(|s| s.to_string()));
    out.extend(argv[j..].iter().cloned());
    let resolved = Resolved {
        entry,
        argv: out,
        tail_start,
        globals: i,
    };
    refuse_client_paths(&resolved)?;
    Ok(resolved)
}

/// The rejected global this argument spells, in any spelling pflag
/// accepts for it.
///
/// `--host=x` and `-H x` were already caught by an exact match on the
/// part before the `=`. `-Hunix:///var/run/docker.sock` was not, and an
/// attached value does not make it a different flag: the client takes it
/// (measured), so the refusal that was meant to stop Ulak being pointed
/// at another daemon simply let it through. Booleans bundle in front of
/// it too — `-Dcprod` is `-D -c prod` — so the whole short cluster is
/// walked, stopping at the first short that takes a value, because from
/// there on the characters are that value and not flags.
fn rejected_global(arg: &str) -> Option<(&'static str, &'static str)> {
    let name = arg.split('=').next().unwrap_or(arg);
    if let Some(hit) = REJECTED_GLOBALS.iter().copied().find(|(f, _)| *f == name) {
        return Some(hit);
    }
    let cluster = arg.strip_prefix('-').filter(|c| !c.starts_with('-'))?;
    for ch in cluster.chars() {
        let short = format!("-{ch}");
        if let Some(hit) = REJECTED_GLOBALS.iter().copied().find(|(f, _)| *f == short) {
            return Some(hit);
        }
        if GLOBAL_VALUE_FLAGS.contains(&short.as_str()) {
            break;
        }
    }
    None
}

// ─── flags whose value is a path on this machine ────────────────────

/// Refuse a command whose own flags name a path here.
///
/// This sits in `resolve` beside the global refusal because it is the
/// same kind of answer: a command line that cannot be carried is refused
/// before anything is opened, rather than forwarded and discovered on
/// the far side. `ClientPath` says why each flag is on the list.
pub fn refuse_client_paths(resolved: &Resolved) -> anyhow::Result<()> {
    let tail = resolved.tail();
    let dockers_own = where_dockers_flags_stop(tail);
    for cp in resolved.entry.client_paths {
        let window = if cp.before_the_command {
            dockers_own
        } else {
            tail.len()
        };
        for k in 0..window {
            let next = tail.get(k + 1).map(String::as_str);
            let Some((flag, Some(value))) =
                spelled(&tail[k], next, cp.flags, resolved.entry.short_booleans)
            else {
                continue;
            };
            let Some(named) = cp.path_in(value) else {
                continue;
            };
            return Err(fail!(
                "`{flag}` names `{named}`, and that is a path on THIS machine — forwarded, \
                 `docker {}` would open the server's instead",
                resolved.entry.name()
            )
            .now(cp.instead)
            .into_err());
        }
    }
    Ok(())
}

/// The flag this argument spells and the value it carries, in every
/// spelling pflag accepts: `--file x`, `--file=x`, `-f x`, `-fx`, and a
/// bundle that hides one of those behind booleans — `-Df/desc.json`.
///
/// The bundle used to be the one it could not read, and it is the one
/// that mattered: Buildx takes `-Df/desc.json` as `-D -f /desc.json`
/// (measured), so an unseen `-f` meant a local descriptor forwarded and
/// the SERVER's file opened in its place. Reading it needs the command's
/// short booleans, which is why `short_booleans` travels with the entry;
/// guessing instead of knowing would refuse `-tapp/frontend:1`, whose
/// `f` is four characters into a tag.
///
/// So the cluster is walked the way pflag walks it, one character at a
/// time, and the walk stops at the first character that is not a known
/// boolean. That character is either the carrier — in which case the
/// rest of the word is its value, `=` and all if it wrote one — or a
/// flag that takes a value, or not a flag at all; in the last two cases
/// the rest of the word belongs to it and no `-f` can be hiding there.
fn spelled<'a>(
    arg: &'a str,
    next: Option<&'a str>,
    flags: &[&'static str],
    short_booleans: &[char],
) -> Option<(&'static str, Option<&'a str>)> {
    if let Some((name, value)) = arg.split_once('=')
        && let Some(flag) = flags.iter().copied().find(|f| *f == name)
    {
        return Some((flag, Some(value)));
    }
    if let Some(flag) = flags.iter().copied().find(|f| *f == arg) {
        return Some((flag, next));
    }
    let cluster = arg.strip_prefix('-').filter(|c| !c.starts_with('-'))?;
    for (at, ch) in cluster.char_indices() {
        let short = format!("-{ch}");
        if let Some(flag) = flags.iter().copied().find(|f| *f == short) {
            // `-Df=x` is `-f`'s `x`, not its `=x`: pflag drops an `=`
            // written straight after a shorthand, and a refusal that
            // quoted the `=` back would be naming a path nobody typed.
            let rest = &cluster[at + ch.len_utf8()..];
            let rest = rest.strip_prefix('=').unwrap_or(rest);
            return Some((flag, if rest.is_empty() { next } else { Some(rest) }));
        }
        if !short_booleans.contains(&ch) {
            return None;
        }
    }
    None
}

/// How far into a command's own arguments Docker is still reading flags
/// of its own.
///
/// `exec` and `service create` hand everything after their positional to
/// a program inside the container, so `docker exec web myprog --env-file
/// conf` is myprog's flag and refusing it would be a lie. Telling the
/// positional from a flag's value normally needs that command's whole
/// value-flag table — 76 entries on `service create` alone, one more
/// list to drift out of true.
///
/// It does not need one. No Docker flag takes two values, so two bare
/// words in a row can only be a value followed by the positional, or the
/// positional followed by the command's first argument. Either way the
/// second of them is at or past the positional, and so is every flag
/// after it — which makes scanning up to and including it enough to
/// cover everything Docker itself could still have read, at the cost of
/// reaching one harmless bare word too far.
fn where_dockers_flags_stop(tail: &[String]) -> usize {
    let bare = |a: &str| !a.starts_with('-') || a == "-";
    for (k, arg) in tail.iter().enumerate() {
        if bare(arg) && (k == 0 || bare(&tail[k - 1])) {
            return k + 1;
        }
    }
    tail.len()
}

// ─── help, from the same table the router uses ──────────────────────

/// Docker's own front page order: the verbs people type all day first,
/// then the namespaces, then the rest. Worth reproducing — somebody
/// looking for `exec` should not have to read past `checkpoint`.
const COMMON: &[&str] = &[
    "run", "exec", "ps", "logs", "build", "pull", "push", "images", "cp", "login", "version",
    "info",
];

/// `ulak docker [PATH…] --help`, rendered from the catalog so it can
/// never describe a tree the router does not have.
pub fn help(path: &[&str]) -> anyhow::Result<String> {
    use std::fmt::Write as _;

    if !path.is_empty() && find(path).is_none() {
        let (parent, bad) = path.split_at(path.len() - 1);
        return Err(unknown(parent, bad[0]));
    }
    let mut out = String::new();
    let spelled = if path.is_empty() {
        String::new()
    } else {
        format!("{} ", path.join(" "))
    };

    match find(path) {
        None => {
            out.push_str("Docker's command tree, executed on this workspace's server.\n\n");
            out.push_str("Usage:  ulak docker COMMAND [ARGS...]\n");
        }
        Some(e) => {
            let _ = writeln!(out, "{}\n", e.about);
            let _ = writeln!(out, "Usage:  ulak docker {spelled}COMMAND [ARGS...]");
        }
    }

    let kids = children(path);
    let width = kids
        .iter()
        .filter_map(|e| e.path.last())
        .map(|n| n.len())
        .max()
        .unwrap_or(0)
        .max(10);

    let section = |title: &str, list: &[&'static Entry], out: &mut String| {
        if list.is_empty() {
            return;
        }
        let _ = writeln!(out, "\n{title}:");
        for e in list {
            let leaf = e.path.last().copied().unwrap_or_default();
            let _ = write!(out, "  {leaf:width$}  {}", e.about);
            match e.route {
                Route::Excluded(_) => out.push_str("  [local only]"),
                Route::Planned(_) => out.push_str("  [not yet]"),
                Route::Family => out.push_str("  …"),
                _ => {}
            }
            if let Some(of) = e.alias_of {
                let _ = write!(out, "  (= docker {})", of.join(" "));
            }
            out.push('\n');
        }
    };

    if path.is_empty() {
        let common: Vec<_> = COMMON.iter().filter_map(|n| find(&[n])).collect();
        // "Has children" and not "is a namespace": `compose` is its own
        // dispatcher rather than a family, and listing it down among the
        // one-word verbs is where nobody looks for it.
        let deep = |e: &&Entry| !children(e.path).is_empty();
        let families: Vec<_> = kids.iter().copied().filter(deep).collect();
        let rest: Vec<_> = kids
            .iter()
            .copied()
            .filter(|e| !deep(e) && !COMMON.contains(&e.path.last().copied().unwrap_or_default()))
            .collect();
        section("Common Commands", &common, &mut out);
        section("Management Commands", &families, &mut out);
        section("Commands", &rest, &mut out);
        out.push_str(
            "\nUlak's own commands (init, doctor, sync, status, config, clean, service, shim, \
             completions) live at the root:\n  ulak --help\n",
        );
    } else {
        section("Commands", &kids, &mut out);
    }

    if let Some(e) = find(path) {
        match e.route {
            Route::Excluded(why) => {
                let _ = write!(out, "\nNot carried to the server: {why}\n");
            }
            Route::Planned(missing) => {
                let _ = write!(out, "\nNot through Ulak yet: {missing}\n");
            }
            _ if kids.is_empty() => {
                let _ = write!(
                    out,
                    "\nFlags are Docker's own — `ulak docker {spelled}--help` asks the server.\n"
                );
            }
            _ => {}
        }
    }
    Ok(out)
}

// ─── the map, as a document ─────────────────────────────────────────
//
// Only the generator lives here; the document it writes is checked into
// docs/ and a test fails when the two disagree. It is `cfg(test)`
// because nothing at runtime asks for it — a user reads the file, and a
// developer regenerates it.

#[cfg(test)]
impl Route {
    /// The word the generated map uses for this route.
    ///
    /// Spelled as a constant — `BUILD_SYNC`, not "build sync" — because
    /// that is what the Route column holds: one routing DECISION per
    /// command path, taken from a fixed set, not prose describing one.
    /// Lowercased with a space in it, `file bridge` and `local only` read
    /// as sentence fragments, and in a 300-row table the eye cannot tell
    /// them from the `note` column beside them. The shape says "this is
    /// an enum value" before the word is even read.
    pub fn label(self) -> &'static str {
        match self {
            Route::Family => "NAMESPACE",
            Route::Daemon => "DAEMON",
            Route::Stream => "STREAM",
            Route::Compose => "COMPOSE",
            Route::Build => "BUILD_SYNC",
            Route::Bake => "BAKE_SYNC",
            Route::Stack => "STACK_SYNC",
            Route::Bridge(_) => "FILE_BRIDGE",
            Route::Footprint => "FOOTPRINT_SYNC",
            Route::Excluded(_) => "LOCAL_ONLY",
            Route::Planned(_) => "NOT_YET",
        }
    }

    /// The sentence the legend puts under that word.
    ///
    /// Here rather than in the legend's own loop because it USED to be
    /// there: a hand-written list of ten `(label, meaning)` pairs, beside
    /// a `label()` that answered eleven. Two spellings of the same set,
    /// with nothing comparing them — so `namespace` reached ten rows of
    /// the map while the legend never mentioned it, and a rename of any
    /// one word would have left the legend describing a route no row
    /// uses. One source now, and `every_route_the_map_prints_is_in_the_
    /// legend` holds it to the catalog.
    pub fn meaning(self) -> &'static str {
        match self {
            Route::Daemon => {
                "Runs on the server. Nothing is synced: its inputs are already in Docker."
            }
            Route::Stream => {
                "Same, with a stream that outlives the call — a TTY, a follow, a live feed."
            }
            Route::Compose => {
                "Compose's own dispatcher: workspace sync, up/down intent, tunnel lifecycle."
            }
            Route::Build => {
                "The build context and Dockerfile are yours, so they travel and the argv is rewritten."
            }
            Route::Stack => {
                "The Compose model a stack deploys is yours: the files travel and the -c paths are respelled."
            }
            Route::Bake => {
                "The server's Buildx resolves the bootstrapped bake definition; every local path in the resulting plan travels."
            }
            Route::Bridge(_) => {
                "An argument names a file on THIS machine. The bytes are streamed; the path is never forwarded."
            }
            Route::Footprint => {
                "Bind sources, env and label files are yours. The smallest safe set travels; the argv is rewritten."
            }
            Route::Excluded(_) => "Deliberately not carried. The row says what to do instead.",
            Route::Planned(_) => "Mapped, transport not built. The row says what is missing.",
            Route::Family => {
                "Not a command at all: a group whose children are. Its own row carries nothing."
            }
        }
    }

    /// Whether a user typing this command gets their work done.
    pub fn carried(self) -> bool {
        !matches!(self, Route::Family | Route::Excluded(_) | Route::Planned(_))
    }
}

/// Every route the map can print, in the order the legend lists them:
/// the ones that carry a command first, then the three that do not.
///
/// The payload variants carry a placeholder — the legend asks a route
/// what KIND it is, never what its particular reason says.
#[cfg(test)]
const LEGEND: &[Route] = &[
    Route::Daemon,
    Route::Stream,
    Route::Compose,
    Route::Build,
    Route::Stack,
    Route::Bake,
    Route::Bridge(Bridge::Cp),
    Route::Footprint,
    Route::Family,
    Route::Excluded(""),
    Route::Planned(""),
];

/// The whole catalog as Markdown.
///
/// Generated rather than written, and checked by a test, because the
/// last support table in this repo was maintained by hand and had drifted
/// from the code by 263 command paths before anybody noticed.
#[cfg(test)]
pub fn map_markdown() -> String {
    use std::fmt::Write as _;

    let leaves: Vec<&Entry> = CATALOG.iter().filter(|e| !e.is_family()).collect();
    let carried = leaves.iter().filter(|e| e.route.carried()).count();
    let aliases = leaves.iter().filter(|e| e.alias_of.is_some()).count();
    let local_only = leaves
        .iter()
        .filter(|e| matches!(e.route, Route::Excluded(_)))
        .count();
    let not_yet = leaves
        .iter()
        .filter(|e| matches!(e.route, Route::Planned(_)))
        .count();

    let mut out = String::new();
    out.push_str("# Docker commands through Ulak\n\n");
    out.push_str(
        "<!-- Generated from crates/ulak/src/catalog.rs. A test fails when this file and\n     \
         the routing table disagree, so edit the table and regenerate:\n     \
         ULAK_TEST_WRITE_MAP=1 cargo test --bins the_map_on_disk -->\n\n",
    );
    let _ = writeln!(
        out,
        "Measured against Docker CLI 29.4.0, Compose 5.1.2 and Buildx 0.33.0.\n"
    );
    let _ = writeln!(
        out,
        "{} command paths. {carried} are carried to your server, of which {aliases} are \
         Docker's own shorthands for another spelling on this list. {local_only} are \
         deliberately left on this machine and {not_yet} are mapped but not built yet — \
         each says which, in its own row.\n",
        leaves.len()
    );

    out.push_str("## How a command travels\n\n");
    out.push_str("| Route | What happens |\n| --- | --- |\n");
    for route in LEGEND {
        let _ = writeln!(out, "| `{}` | {} |", route.label(), route.meaning());
    }

    out.push_str("\n## One command, two spellings\n\n");
    out.push_str(
        "Docker gives many commands a second name: a root-level shorthand for the ones people \
         type all day (`ps` for `container ls`), and an unprinted long form on nearly every \
         family (`volume list` for `volume ls`). Both spellings are real and both route the \
         same way here — recorded so the map cannot count one command twice, and so a change \
         to one can never miss the other.\n\n",
    );
    out.push_str("| Shorthand | Same command as |\n| --- | --- |\n");
    for e in CATALOG {
        if let Some(of) = e.alias_of {
            let _ = writeln!(out, "| `docker {}` | `docker {}` |", e.name(), of.join(" "));
        }
    }

    out.push_str("\n## Flags that name a file on this machine\n\n");
    out.push_str(
        "These commands run in the daemon and forward cleanly, all but a flag or two each: the \
         client opens those before it dials the daemon at all, so forwarded they read the \
         SERVER's filesystem — quietly, and with the wrong contents. Ulak refuses them and says \
         what to do instead.\n\n",
    );
    out.push_str("| Command | Flag | Instead |\n| --- | --- | --- |\n");
    for e in CATALOG {
        for cp in e.client_paths {
            let spellings: Vec<String> = cp.flags.iter().map(|f| format!("`{f}`")).collect();
            let named = match cp.field {
                Some(field) => format!("{} (the `{field}=` field)", spellings.join(" / ")),
                None => spellings.join(" / "),
            };
            let _ = writeln!(out, "| `docker {}` | {named} | {} |", e.name(), cp.instead);
        }
    }

    out.push_str("\n## The tree\n\n");
    let mut families: Vec<&'static str> = Vec::new();
    for e in CATALOG {
        if e.path.len() == 1 && !families.contains(&e.path[0]) {
            families.push(e.path[0]);
        }
    }

    let roots: Vec<&Entry> = CATALOG
        .iter()
        .filter(|e| e.path.len() == 1 && !e.is_family())
        .collect();
    out.push_str("### Root commands\n\n");
    table(&mut out, &roots, 0);

    for family in families {
        let Some(head) = find(&[family]) else {
            continue;
        };
        if !head.is_family() && head.route != Route::Compose {
            continue;
        }
        let under: Vec<&Entry> = CATALOG
            .iter()
            .filter(|e| e.path.len() > 1 && e.path[0] == family)
            .collect();
        if under.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n### docker {family} — {}\n", head.about);
        table(&mut out, &under, 1);
    }
    out
}

#[cfg(test)]
fn table(out: &mut String, rows: &[&Entry], skip: usize) {
    use std::fmt::Write as _;
    out.push_str("| Command | Route | Notes |\n| --- | --- | --- |\n");
    for e in rows {
        let name = e.path[skip..].join(" ");
        let note = match e.route {
            Route::Excluded(why) | Route::Planned(why) => why.replace('\n', " "),
            Route::Family => "namespace only".into(),
            _ => match e.alias_of {
                Some(of) => format!("same as `docker {}`", of.join(" ")),
                None => e.about.to_string(),
            },
        };
        let _ = writeln!(out, "| `{name}` | {} | {note} |", e.route.label());
    }
}

/// The catalog as a clap tree, for shell completions only. Leaves take
/// a trailing bucket so a completion script stops guessing at Docker's
/// own flags rather than offering the wrong ones.
pub fn completion_tree(base: clap::Command) -> clap::Command {
    fn walk(mut node: clap::Command, path: &mut Vec<&'static str>) -> clap::Command {
        for child in children(path) {
            let leaf = child.path.last().copied().unwrap_or_default();
            let mut sub = clap::Command::new(leaf).about(child.about);
            let grandchildren = !children(child.path).is_empty();
            if grandchildren {
                path.push(leaf);
                sub = walk(sub, path);
                path.pop();
            }
            sub = sub.arg(
                clap::Arg::new("argv")
                    .value_name("ARGV")
                    .num_args(0..)
                    .trailing_var_arg(true)
                    .allow_hyphen_values(true),
            );
            node = node.subcommand(sub);
        }
        node
    }
    walk(base, &mut Vec::new())
}

fn unknown(parent: &[&str], bad: &str) -> anyhow::Error {
    let scope = if parent.is_empty() {
        "docker".to_string()
    } else {
        format!("docker {}", parent.join(" "))
    };
    let mut err = fail!("`{scope}` has no command `{bad}`");
    let siblings = children(parent);
    if let Some(near) = nearest(bad, &siblings) {
        err = err.now(format!("did you mean `ulak docker {}`?", near.name()));
    }
    // A family's whole list is worth printing; the root's is 58 names
    // and reads as noise exactly where somebody is already lost.
    let names: Vec<&str> = siblings
        .iter()
        .filter_map(|e| e.path.last().copied())
        .collect();
    err = if names.is_empty() || names.len() > 16 {
        err.now(format!(
            "`ulak docker{}--help` lists what there is",
            if parent.is_empty() {
                " ".to_string()
            } else {
                format!(" {} ", parent.join(" "))
            }
        ))
    } else {
        err.now(format!("{scope} has: {}", names.join(", ")))
    };
    err.into_err()
}

/// A one-edit-away neighbour, which is what a typo usually is. Nothing
/// cleverer: a suggestion that is wrong is worse than none.
fn nearest(bad: &str, among: &[&'static Entry]) -> Option<&'static Entry> {
    among.iter().copied().find(|e| {
        e.path
            .last()
            .is_some_and(|leaf| edit_distance_at_most_one(bad, leaf))
    })
}

fn edit_distance_at_most_one(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (long, short) = if a.len() >= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    if long.len() - short.len() > 1 {
        return false;
    }
    let mut i = 0;
    let mut j = 0;
    let mut slack = 1usize;
    while i < long.len() && j < short.len() {
        if long[i] == short[j] {
            i += 1;
            j += 1;
            continue;
        }
        if slack == 0 {
            return false;
        }
        slack -= 1;
        if long.len() == short.len() {
            i += 1;
            j += 1;
        } else {
            i += 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn every_path_is_unique_and_has_a_parent() {
        let mut seen = std::collections::BTreeSet::new();
        for e in CATALOG {
            assert!(!e.path.is_empty(), "a command needs a name");
            assert!(
                seen.insert(e.path),
                "{} appears twice in the catalog",
                e.name()
            );
        }
        for e in CATALOG {
            if e.path.len() < 2 {
                continue;
            }
            let parent = &e.path[..e.path.len() - 1];
            let found = find(parent).unwrap_or_else(|| panic!("{} has no parent entry", e.name()));
            // Compose owns its own tree, so its children hang off a
            // node that is a dispatcher rather than a namespace; the
            // nodes that are both a command and a namespace hang theirs
            // off a node that is a command.
            assert!(
                found.is_family()
                    || found.route == Compose
                    || BOTH_A_COMMAND_AND_A_NAMESPACE.contains(&found.path),
                "{} is a parent, so it must be a family",
                found.name()
            );
        }
    }

    #[test]
    fn every_alias_points_at_a_real_command_with_the_same_route() {
        for e in CATALOG {
            let Some(of) = e.alias_of else { continue };
            let target = find(of).unwrap_or_else(|| panic!("{} aliases nothing", e.name()));
            assert_eq!(
                e.route,
                target.route,
                "{} and {} are one command and must route the same way",
                e.name(),
                target.name()
            );
            // `docker exec` and `docker container exec` are one command,
            // so a flag refused under one spelling has to be refused
            // under the other — otherwise the fix is only half applied
            // and the half that is missing is the one people type.
            assert_eq!(
                e.client_paths,
                target.client_paths,
                "{} and {} are one command and must refuse the same flags",
                e.name(),
                target.name()
            );
            // Same reasoning one level down: the bundle `-Df/desc.json`
            // is only seen where the booleans are known, so a table on
            // one spelling and not the other refuses under one name and
            // forwards under the other.
            assert_eq!(
                e.short_booleans,
                target.short_booleans,
                "{} and {} are one command and must read a bundle the same way",
                e.name(),
                target.name()
            );
        }
    }

    #[test]
    fn a_family_is_never_a_leaf_and_a_leaf_is_never_empty() {
        for e in CATALOG {
            let kids = children(e.path);
            if e.is_family() {
                assert!(
                    !kids.is_empty(),
                    "{} is a family with no children",
                    e.name()
                );
            } else if e.route != Compose && !BOTH_A_COMMAND_AND_A_NAMESPACE.contains(&e.path) {
                assert!(
                    kids.is_empty(),
                    "{} has children but is neither a family nor one of the nodes \
                     that is deliberately both",
                    e.name()
                );
            }
        }
    }

    #[test]
    fn the_command_path_stops_at_the_first_real_command() {
        // `sh` must not be read as a subcommand of exec.
        let r = resolve(&v(&["exec", "-it", "web", "sh"])).unwrap();
        assert_eq!(r.entry.path, ["exec"]);
        assert_eq!(r.argv, v(&["exec", "-it", "web", "sh"]));
        assert_eq!(r.tail_start, 1);

        // A family walks exactly one level further.
        let r = resolve(&v(&["container", "ls", "-a"])).unwrap();
        assert_eq!(r.entry.path, ["container", "ls"]);
        assert_eq!(r.tail_start, 2);

        // Four levels deep, because buildx really is that deep.
        let r = resolve(&v(&["buildx", "history", "inspect", "attachment", "x"])).unwrap();
        assert_eq!(r.entry.path, ["buildx", "history", "inspect", "attachment"]);

        // `ls` here is a positional argument to create, not a command.
        let r = resolve(&v(&["network", "create", "ls"])).unwrap();
        assert_eq!(r.entry.path, ["network", "create"]);
        assert_eq!(r.argv, v(&["network", "create", "ls"]));
    }

    /// Every route the map prints has a legend row explaining it.
    ///
    /// The legend used to be a hand-written list of ten `(label,
    /// meaning)` pairs standing beside a `label()` that answered eleven,
    /// with nothing comparing the two. `namespace` was the eleventh: it
    /// reached ten rows of the generated map while the legend never once
    /// mentioned it, so a reader met a word the page did not define. Both
    /// come from `Route` now, and this is what stops a route added
    /// tomorrow from printing a label nobody explains.
    #[test]
    fn every_route_the_map_prints_is_in_the_legend() {
        let explained: std::collections::BTreeSet<&str> =
            LEGEND.iter().map(|r| r.label()).collect();
        for e in CATALOG {
            assert!(
                explained.contains(e.route.label()),
                "{} prints `{}`, which the legend does not explain",
                e.name(),
                e.route.label()
            );
        }
        // And the other direction: a legend row for a route no command
        // path takes is the same drift, pointing the other way.
        let printed: std::collections::BTreeSet<&str> =
            CATALOG.iter().map(|e| e.route.label()).collect();
        for route in LEGEND {
            assert!(
                printed.contains(route.label()),
                "the legend explains `{}`, which no command path takes",
                route.label()
            );
        }
    }

    /// A node that owns a subcommand can still be a command itself.
    ///
    /// Measured on Buildx 0.33.0: `docker buildx history inspect` bare
    /// prints the LATEST build record, with a REF prints that one, and
    /// with `--format json` prints it as JSON — while `attachment`
    /// remains a subcommand underneath it. Read as a namespace, all
    /// three broke in different ways: the REF became an unknown child,
    /// the flag became a stray word, and the bare form printed Ulak's
    /// own listing and exited 0 — which in a script is a record that was
    /// inspected and found empty.
    #[test]
    fn a_command_that_owns_a_subcommand_still_takes_its_own_arguments() {
        let r = resolve(&v(&["buildx", "history", "inspect", "l2abl7px0x8653"])).unwrap();
        assert_eq!(r.entry.path, ["buildx", "history", "inspect"]);
        assert_eq!(r.tail(), v(&["l2abl7px0x8653"]));

        // The `builder` spelling is the same plugin and must not drift.
        let r = resolve(&v(&["builder", "history", "inspect", "--format", "json"])).unwrap();
        assert_eq!(r.entry.path, ["builder", "history", "inspect"]);
        assert_eq!(r.tail(), v(&["--format", "json"]));

        // Bare: a command with an empty tail. It reaches the server —
        // what it must NOT do is reach the `Route::Family` arm, which
        // prints a listing and exits 0.
        let r = resolve(&v(&["buildx", "history", "inspect"])).unwrap();
        assert_eq!(r.entry.path, ["buildx", "history", "inspect"]);
        assert!(r.tail().is_empty());
        assert_ne!(r.entry.route, Family, "bare, this would print a listing");

        // And the child is still reachable, or this fix would have cost
        // the subcommand to save the command.
        let r = resolve(&v(&["buildx", "history", "inspect", "attachment", "x"])).unwrap();
        assert_eq!(r.entry.path, ["buildx", "history", "inspect", "attachment"]);
        assert_eq!(r.tail(), v(&["x"]));
    }

    #[test]
    fn compose_keeps_its_whole_argv() {
        let r = resolve(&v(&["compose", "up", "-d"])).unwrap();
        assert_eq!(r.entry.path, ["compose"], "compose parses its own tree");
        assert_eq!(r.argv, v(&["compose", "up", "-d"]));
        assert_eq!(r.tail_start, 1);
        assert_eq!(r.globals, 0);
    }

    #[test]
    fn a_global_before_the_command_is_counted_so_it_cannot_vanish() {
        // Every route but compose carries these through in `argv`.
        // Compose cannot, so the dispatcher needs to know they were
        // there — a flag that simply disappears is worse than one that
        // is refused.
        let r = resolve(&v(&["--log-level", "debug", "compose", "up"])).unwrap();
        assert_eq!(r.globals, 2);
        assert_eq!(r.tail(), v(&["up"]));

        let r = resolve(&v(&["-D", "ps", "-a"])).unwrap();
        assert_eq!(r.globals, 1);
        assert_eq!(r.argv, v(&["-D", "ps", "-a"]));
    }

    #[test]
    fn docker_globals_survive_and_the_dangerous_ones_do_not() {
        let r = resolve(&v(&["--log-level", "debug", "ps"])).unwrap();
        assert_eq!(r.entry.path, ["ps"]);
        assert_eq!(r.argv, v(&["--log-level", "debug", "ps"]));
        assert_eq!(r.tail_start, 3);

        for bad in [
            v(&["-H", "tcp://x", "ps"]),
            v(&["--context", "other", "ps"]),
            v(&["--config", "/tmp/x", "ps"]),
        ] {
            let err = resolve(&bad).unwrap_err();
            assert!(
                err.to_string().contains("cannot be forwarded"),
                "{bad:?} must be refused: {err}"
            );
        }
    }

    #[test]
    fn an_unknown_command_names_itself_and_its_neighbours() {
        // The way forward is half the answer: somebody who typed
        // `contaner` needs the spelling handed back, not just told no.
        let text = refusal(&["contaner", "ls"]);
        assert!(text.contains("no command `contaner`"), "{text}");
        assert!(
            text.contains("did you mean `ulak docker container`?"),
            "{text}"
        );

        // One level down the suggestion has to be scoped to the family,
        // because `create` exists under a dozen parents and naming the
        // wrong one sends the reader somewhere that will not work.
        let text = refusal(&["network", "creat", "x"]);
        assert!(text.contains("no command `creat`"), "{text}");
        assert!(
            text.contains("did you mean `ulak docker network create`?"),
            "{text}"
        );
        assert!(
            text.contains("network has: "),
            "the family lists itself: {text}"
        );

        // Two edits away there is no neighbour worth offering, and a
        // guess would be worse than the list — but the list is still
        // owed. The root's list is far past the sixteen names that read
        // as help rather than noise, so it points at `--help` instead.
        let text = refusal(&["ontanr", "ls"]);
        assert!(text.contains("no command `ontanr`"), "{text}");
        assert!(!text.contains("did you mean"), "a two-edit guess: {text}");
        assert!(
            text.contains("`ulak docker --help` lists what there is"),
            "{text}"
        );
    }

    #[test]
    fn an_empty_docker_command_asks_for_one() {
        let err = resolve(&[]).unwrap_err();
        assert!(err.to_string().contains("needs a Docker command"), "{err}");
    }

    /// What counts as a typo. The suggestion this feeds is asserted in
    /// `an_unknown_command_names_itself_and_its_neighbours`; this one
    /// measures only the ruler, because the ruler is where "cerate" gets
    /// offered `create` by a distance that was never one edit.
    #[test]
    fn edit_distance_counts_one_edit_and_not_two() {
        assert!(edit_distance_at_most_one("ls", "ls"));
        assert!(edit_distance_at_most_one("creat", "create"));
        assert!(edit_distance_at_most_one("crete", "create"));
        assert!(!edit_distance_at_most_one("cerate", "create"));
        assert!(!edit_distance_at_most_one("run", "create"));
    }

    #[test]
    fn the_commands_people_type_all_day_are_all_routed() {
        // The regression this whole file exists to prevent: of the 38
        // below, the old eleven-branch enum reached six — `ps`, `build`,
        // `stop`, `rm`, `inspect`, `info` — and answered the other 32
        // with "not a root command".
        for cmd in [
            "run", "exec", "ps", "logs", "build", "pull", "push", "images", "cp", "stop", "rm",
            "start", "restart", "kill", "inspect", "stats", "top", "port", "attach", "wait",
            "commit", "diff", "rename", "update", "pause", "unpause", "save", "load", "export",
            "import", "tag", "history", "info", "version", "events", "search", "login", "logout",
        ] {
            let r = resolve(&v(&[cmd])).unwrap_or_else(|e| panic!("{cmd} unroutable: {e}"));
            assert!(
                !matches!(r.entry.route, Family),
                "{cmd} resolved to a namespace"
            );
        }
    }

    /// The map on disk is generated, and this is what stops it from
    /// becoming decoration. The previous support table was maintained by
    /// hand and had drifted from the code by 263 command paths.
    ///
    /// Regenerate with: ULAK_TEST_WRITE_MAP=1 cargo test --bins the_map_on_disk
    #[test]
    fn the_map_on_disk_says_what_the_router_does() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/docker-commands.md");
        let fresh = map_markdown();
        if std::env::var_os("ULAK_TEST_WRITE_MAP").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &fresh).unwrap();
            return;
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{} cannot be read ({e}) — regenerate it with \
                 ULAK_TEST_WRITE_MAP=1 cargo test --bins the_map_on_disk",
                path.display()
            )
        });
        assert_eq!(
            on_disk,
            fresh,
            "{} is out of date — regenerate it with \
             ULAK_TEST_WRITE_MAP=1 cargo test --bins the_map_on_disk",
            path.display()
        );
    }

    /// Refused, message and "now do this" flattened the way a user sees
    /// them — the way forward is half of what is being asserted.
    fn refusal(args: &[&str]) -> String {
        let err = resolve(&v(args))
            .err()
            .unwrap_or_else(|| panic!("{args:?} must be refused"));
        crate::ui::flatten(&err)
    }

    #[test]
    fn a_client_side_path_is_refused_in_every_spelling_pflag_accepts() {
        // The failure this prevents: forwarded, each of these opens the
        // SERVER's copy of the file and never says so.
        for args in [
            vec!["exec", "--env-file", "/etc/app.env", "web", "true"],
            vec!["exec", "--env-file=/etc/app.env", "web", "true"],
            vec![
                "container",
                "exec",
                "--env-file",
                "/etc/app.env",
                "web",
                "true",
            ],
            vec!["service", "create", "--env-file", "/etc/app.env", "nginx"],
        ] {
            let text = refusal(&args);
            assert!(text.contains("--env-file"), "{args:?}: {text}");
            assert!(text.contains("/etc/app.env"), "{args:?}: {text}");
            assert!(text.contains("-e NAME=value"), "{args:?}: {text}");
        }

        for args in [
            vec!["swarm", "ca", "--rotate", "--ca-cert", "./ca.pem"],
            vec!["swarm", "ca", "--ca-key=./ca.key"],
        ] {
            let text = refusal(&args);
            assert!(text.contains("ulak status"), "{args:?}: {text}");
        }

        // `-f value`, `-fvalue` and `--file=value` are one flag.
        for args in [
            vec!["buildx", "imagetools", "create", "-f", "./desc.json"],
            vec!["buildx", "imagetools", "create", "-f./desc.json"],
            vec!["builder", "imagetools", "create", "--file=./desc.json"],
        ] {
            let text = refusal(&args);
            assert!(text.contains("./desc.json"), "{args:?}: {text}");
        }

        // The write direction counts too: the file lands on the server.
        let text = refusal(&[
            "buildx",
            "imagetools",
            "create",
            "--metadata-file",
            "out.json",
            "app:1",
        ]);
        assert!(text.contains("--metadata-file"), "{text}");
    }

    /// A carrier hiding behind a bundled boolean, in both directions.
    ///
    /// Buildx 0.33.0 reads every line below the same way — `-D` is a
    /// boolean, so it slides off and leaves the `-f`. Measured one by
    /// one against the real client with `--dry-run` and a path that is
    /// not there: each answers "open /nonexistent/desc.json: no such
    /// file or directory", which is the client having read the flag and
    /// opened the file HERE. Forwarded, the server's `/desc.json` would
    /// be opened in its place and the answer would look no different.
    #[test]
    fn a_carrier_behind_a_bundled_boolean_is_refused_like_a_spaced_one() {
        for args in [
            vec!["buildx", "imagetools", "create", "-D", "-f", "./desc.json"],
            vec!["buildx", "imagetools", "create", "-Df./desc.json"],
            vec!["buildx", "imagetools", "create", "-Df", "./desc.json"],
            vec!["buildx", "imagetools", "create", "-Df=./desc.json"],
            vec!["buildx", "imagetools", "create", "-DDf./desc.json"],
            vec!["builder", "imagetools", "create", "-Df./desc.json"],
        ] {
            let text = refusal(&args);
            // The flag as typed and the path as typed: a refusal that
            // named `--file` or quoted the `=` back would be describing
            // a command line the user did not write.
            assert!(text.contains("`-f`"), "{args:?}: {text}");
            assert!(
                text.contains("`./desc.json`"),
                "{args:?} must name the path it would have opened: {text}"
            );
        }
    }

    /// The other direction, which is the one that makes the table worth
    /// keeping: a letter inside somebody else's VALUE is not a flag.
    ///
    /// `-t`, `-p` and `-f` all take values on `imagetools create`
    /// (measured: each alone answers "flag needs an argument"), so the
    /// walk has to stop at the first of them. `-tf/desc.json` is a tag
    /// — measured, it parses and the command runs on to open the `-f`
    /// that follows it — and refusing a tag for containing an `f` would
    /// be the catalog inventing a local path out of a registry
    /// reference.
    #[test]
    fn a_value_that_merely_contains_a_carriers_letter_is_left_alone() {
        for args in [
            // A tag whose text starts with the carrier's own letter.
            vec!["buildx", "imagetools", "create", "-tf/desc.json", "app:1"],
            // And one with an `f` four characters in, which is what a
            // search for the letter rather than a walk of the flags
            // would trip over.
            vec![
                "buildx",
                "imagetools",
                "create",
                "-tapp/frontend:1",
                "app:1",
            ],
            // A boolean in front of it changes nothing: `-D -t app/…`.
            vec![
                "buildx",
                "imagetools",
                "create",
                "-Dtapp/frontend:1",
                "app:1",
            ],
            // An unknown short is not a boolean, so it must not be
            // peeled: read as one, the `f` behind it would become a
            // carrier and a tag would be refused as a file. Buildx
            // answers "unknown shorthand flag: 'X'" and that is its
            // answer to give.
            vec!["buildx", "imagetools", "create", "-Xf./desc.json"],
        ] {
            resolve(&v(&args)).unwrap_or_else(|e| {
                panic!(
                    "{args:?} names no path on this machine: {}",
                    crate::ui::flatten(&e)
                )
            });
        }
    }

    /// A short carrier added without its command's boolean table would
    /// reopen the hole above silently: the bundle simply stops being
    /// recognised, and the path is forwarded with nothing said. So the
    /// commands that have one are named here, and a new one has to come
    /// past this test to be added.
    #[test]
    fn every_short_flag_that_names_a_path_has_its_commands_booleans_measured() {
        // A short is what `spelled` treats as one: two characters, one
        // dash, the shape a bundle can swallow.
        let short = |cp: &ClientPath| cp.flags.iter().any(|f| f.len() == 2 && f.starts_with('-'));
        let guarded: Vec<String> = CATALOG
            .iter()
            .filter(|e| e.client_paths.iter().any(short))
            .map(Entry::name)
            .collect();
        assert_eq!(
            guarded,
            ["builder imagetools create", "buildx imagetools create"],
            "a command with a short client-path flag needs `short_booleans` measured \
             against the real client — `--help` names them, and `docker … -X` alone \
             says whether it takes a value"
        );
        for path in [
            ["builder", "imagetools", "create"],
            ["buildx", "imagetools", "create"],
        ] {
            let e = find(&path).unwrap();
            assert_eq!(
                e.short_booleans,
                IMAGETOOLS_BOOLEANS,
                "{} is the same command under two names, and a table on one of them \
                 only refuses under the name that has it",
                e.name()
            );
        }
    }

    /// The prose counts the table, so the table should be what says so.
    ///
    /// `cli.rs` claimed "263 of Docker's 274 command paths" for as long
    /// as it took someone to count: the table had grown to 327 and the
    /// generated map had followed it, but a sentence in a doc comment is
    /// the one kind of documentation nothing reads back. Three places
    /// gave three different answers, and the wrong two read exactly as
    /// confidently as the right one.
    #[test]
    fn the_counts_in_the_prose_are_the_counts_in_the_table() {
        let total = CATALOG.len();
        let leaves = CATALOG.iter().filter(|e| !e.is_family()).count();

        // The eleven variants `ulak docker` used to spell out by hand.
        let claim = format!("{} of Docker's {total}", total - 11);
        for (file, text) in [
            ("catalog.rs", include_str!("catalog.rs")),
            ("cli.rs", include_str!("cli.rs")),
        ] {
            assert!(text.contains(&claim), "{file} no longer says \"{claim}\"");
        }

        // The map counts only the leaves, and says so in those words.
        let map = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/docker-commands.md"),
        )
        .expect("the generated map");
        assert!(
            map.contains(&format!("{leaves} command paths")),
            "the map does not count {leaves} leaves"
        );
    }

    /// The builder config, including the spelling Docker no longer
    /// prints. `--help` lists only `--buildkitd-config`, but the older
    /// `--config` is still accepted — measured, it answers the same
    /// "buildkit configuration file not found" — and a guard that knows
    /// only the documented name is a guard the deprecated alias walks
    /// straight past.
    #[test]
    fn the_builder_config_is_refused_under_both_its_names() {
        for args in [
            vec!["buildx", "create", "--buildkitd-config", "./buildkitd.toml"],
            vec!["buildx", "create", "--buildkitd-config=./buildkitd.toml"],
            vec!["buildx", "create", "--config", "./buildkitd.toml"],
            vec![
                "builder",
                "create",
                "--buildkitd-config",
                "./buildkitd.toml",
            ],
        ] {
            let text = refusal(&args);
            assert!(text.contains("./buildkitd.toml"), "{args:?}: {text}");
        }

        // `--driver-opt cacert=` is NOT a read at create time — buildx
        // stores the string and nothing else — so refusing it would be
        // describing a failure that does not happen here.
        assert!(
            resolve(&[
                "buildx".into(),
                "create".into(),
                "--driver-opt".into(),
                "cacert=./ca.pem".into(),
            ])
            .is_ok(),
            "a driver option is carried to the builder record, not read"
        );
    }

    #[test]
    fn a_path_hidden_in_a_csv_field_is_found_and_an_absent_one_is_not() {
        // pflag opens `cacert=` while PARSING the spec, so the file
        // loses the race to the daemon exactly as `--ca-cert` does.
        for spec in [
            "protocol=cfssl,url=https://ca.example,cacert=./ca.pem",
            // Docker matches the key case-insensitively; so must this.
            "protocol=cfssl,CACERT=./ca.pem",
            // The value is a CSV record, so a defensively quoted field
            // is one field: split on every comma instead and the key
            // reads `"cacert`, which matches nothing and forwards a
            // local PEM without a word.
            "\"cacert=./ca.pem\",protocol=cfssl",
        ] {
            for cmd in [["swarm", "init"], ["swarm", "update"], ["swarm", "ca"]] {
                let text = refusal(&[cmd[0], cmd[1], "--external-ca", spec]);
                assert!(text.contains("./ca.pem"), "{cmd:?} {spec}: {text}");
            }
        }

        // The reason the quoting rule matters at all: a comma inside a
        // quoted field belongs to the path. Measured — `swarm ca
        // --external-ca '"cacert=/nonexistent,x.pem",protocol=cfssl'`
        // answers `open /nonexistent,x.pem`, so the whole field is the
        // path and the refusal must name all of it.
        let text = refusal(&[
            "swarm",
            "ca",
            "--external-ca",
            "\"cacert=./ca,backup.pem\",protocol=cfssl",
        ]);
        assert!(text.contains("./ca,backup.pem"), "{text}");

        // `""` inside a quoted field is one literal quote, and the
        // refusal names the file Docker would open, not the spelling.
        let text = refusal(&[
            "swarm",
            "ca",
            "--external-ca",
            "\"cacert=./ca\"\".pem\",protocol=cfssl",
        ]);
        assert!(text.contains("./ca\".pem"), "{text}");

        // A record Docker cannot parse at all is forwarded rather than
        // refused: `'\"cacert=./ca.pem,url=x'` answers "extraneous or
        // missing \" in quoted-field" before it opens anything, so
        // there is no local read to prevent and the client's own
        // message is the honest one.
        resolve(&v(&[
            "swarm",
            "ca",
            "--external-ca",
            "\"cacert=./ca.pem,url=https://ca.example",
        ]))
        .expect("a malformed record names no path Ulak can be sure of");

        // Without a cacert field the flag names nothing here, and Docker
        // accepts it — refusing it would be inventing a problem.
        resolve(&v(&[
            "swarm",
            "init",
            "--external-ca",
            "protocol=cfssl,url=https://ca.example",
        ]))
        .expect("an external CA with no local file is fine");
    }

    #[test]
    fn a_flag_belonging_to_the_container_command_is_not_refused() {
        // `docker exec web myprog --env-file conf` runs myprog with its
        // own flag — verified against the client, which passes it
        // through untouched. Refusing that would be a lie, and there
        // would be no way to spell what the user meant.
        for args in [
            vec!["exec", "web", "myprog", "--env-file", "conf"],
            vec!["exec", "-it", "web", "myprog", "--env-file", "conf"],
            vec!["exec", "-u", "root", "web", "myprog", "--env-file", "conf"],
        ] {
            resolve(&v(&args)).unwrap_or_else(|e| panic!("{args:?} is myprog's flag: {e:#}"));
        }

        // But Docker's own `--env-file` still is Docker's, however many
        // value-taking flags come before it.
        for args in [
            vec!["exec", "-u", "root", "--env-file", "/x.env", "web", "sh"],
            vec!["exec", "-e", "A=1", "--env-file", "/x.env", "web", "sh"],
        ] {
            assert!(refusal(&args).contains("/x.env"), "{args:?}");
        }
    }

    #[test]
    fn a_command_with_no_client_side_flag_is_left_alone() {
        // The refusal must not leak onto the rest of the tree: `-f` is a
        // format to `ps` and a force to `rm`, and neither names a file.
        for args in [
            vec!["ps", "-f", "status=running"],
            vec!["service", "update", "--image", "nginx:1", "web"],
            vec!["swarm", "join", "--token", "SWMTKN-1-x", "10.0.0.1:2377"],
        ] {
            resolve(&v(&args)).unwrap_or_else(|e| panic!("{args:?} carries no local path: {e:#}"));
        }
    }

    #[test]
    fn a_short_global_with_its_value_attached_is_refused_like_the_spaced_one() {
        // pflag lets a short flag carry its value with no space, and
        // lets booleans bundle in front of it. Both spellings reach the
        // real client (measured), so a refusal that only matches
        // `-H tcp://x` leaves `-Htcp://x` pointing Ulak at another
        // daemon with nothing said.
        for bad in [
            v(&["-Hunix:///var/run/docker.sock", "ps"]),
            v(&["-cprod", "ps"]),
            v(&["-c=prod", "ps"]),
            v(&["-Dcprod", "ps"]),
        ] {
            let err = resolve(&bad).unwrap_err();
            assert!(
                err.to_string().contains("cannot be forwarded"),
                "{bad:?} must be refused: {err}"
            );
        }

        // A cluster whose value merely contains one of those letters is
        // that flag's value, not a flag: `-l` takes the rest of it.
        resolve(&v(&["-lHc", "ps"])).expect("`-lHc` is a log level, however unwise");
    }

    #[test]
    fn every_flag_that_names_a_local_path_sits_on_a_command_that_would_forward_it() {
        // A `ClientPath` on a route that already syncs would be dead
        // weight and, worse, would read as a promise the transport is
        // making and is not.
        for e in CATALOG {
            if e.client_paths.is_empty() {
                continue;
            }
            assert!(
                matches!(e.route, Daemon | Stream),
                "{} syncs already; a refusal there is the wrong answer",
                e.name()
            );
            for cp in e.client_paths {
                assert!(!cp.flags.is_empty(), "{} has a flag with no name", e.name());
                assert!(
                    !cp.instead.is_empty(),
                    "{} refuses {:?} with no way forward",
                    e.name(),
                    cp.flags
                );
            }
        }
    }

    #[test]
    fn the_local_only_families_are_the_ones_this_machine_owns() {
        // Each of these edits or reads state that belongs to the client,
        // and each was routed to the daemon before it was measured.
        for path in [
            ["manifest", "create"],
            ["manifest", "annotate"],
            ["manifest", "push"],
            ["manifest", "rm"],
            ["buildx", "install"],
            ["buildx", "uninstall"],
            ["builder", "install"],
            ["builder", "uninstall"],
        ] {
            let e = find(&path).unwrap();
            assert!(
                matches!(e.route, Excluded(_)),
                "{} edits this machine's own state",
                e.name()
            );
        }
        // `manifest inspect` asks the registry, not the store, so the
        // server's credentials and network are a reason to run it there.
        assert_eq!(find(&["manifest", "inspect"]).unwrap().route, Daemon);
    }

    #[test]
    fn a_command_that_serves_a_ui_says_which_machine_it_would_serve_it_on() {
        for path in [
            ["buildx", "history", "trace"],
            ["builder", "history", "trace"],
        ] {
            let Planned(why) = find(&path).unwrap().route else {
                panic!("{path:?} binds a port on whichever machine runs it");
            };
            assert!(why.contains("127.0.0.1"), "{why}");
        }
    }

    #[test]
    fn the_policy_commands_are_described_by_what_was_measured() {
        // The old wording said build policies were newer than the Buildx
        // this map was measured against. They are not: `buildx policy
        // eval` and `test` both ship in 0.33.0, and both read a tree on
        // this machine.
        let Planned(why) = find(&["buildx", "policy", "eval"]).unwrap().route else {
            panic!("policy eval has no transport yet");
        };
        assert!(!why.contains("newer than"), "{why}");
        assert!(why.contains("local"), "{why}");
    }

    #[test]
    fn passwords_are_marked_where_they_actually_are() {
        // `-p` is a password to login and a published port to run. A
        // redactor that cannot tell them apart either leaks the first
        // or corrupts the second.
        assert!(find(&["login"]).unwrap().secret_flags.contains(&"-p"));
        assert!(find(&["run"]).unwrap().secret_flags.is_empty());
    }
}
