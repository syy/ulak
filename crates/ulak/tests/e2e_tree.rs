//! Docker's whole tree through the catalog dispatcher, end to end.
//!
//! What this file is for. Until the routing table existed, `ulak docker`
//! was a hand-written clap enum with eleven branches, so 316 of Docker's
//! 327 command paths died at the argument parser with "not a root
//! command" — `container ls`, `system df`, `logs`, `exec`, every one of
//! them. The unit tests in `catalog` prove that argv now RESOLVES; they
//! cannot prove a resolved command reaches a daemon, comes back with
//! bytes, or carries its exit code home. That is what runs here.
//!
//! Two groups, split by what they need:
//!
//!   - the tree and the refusals are answered on THIS machine, before
//!     any ssh, so they are tested without a server and would still be
//!     tested on a laptop that has no docker at all;
//!   - the daemon and stream routes are only true if a real remote
//!     daemon answers, so they are tested against one.
//!
//! Measured, because "it passed" and "it never ran" look identical from
//! the outside: with the fixture up this file takes ~31 s, and under
//! `ULAK_TEST_E2E=skip` the same eleven tests report ok in about a second. If
//! a CI run of this file finishes that fast, the remote half did not
//! happen.
//!
//! The nine remote scenarios were one `#[test]` calling them in
//! sequence, which reported a single verdict for nine claims and stopped
//! at the first break. They are nine tests now, sharing one fixture
//! (`TestServer::shared`) and running in parallel: 28 s for three
//! verdicts became 31 s for eleven.
//!
//! Every remote assertion looks at CONTENT. A daemon command that
//! returned an empty table would pass an exit-status check, and an empty
//! table is exactly what a mis-routed command produces when it lands on
//! the wrong machine — that is the whole failure mode Ulak exists to
//! prevent, so it must not be the one the tests wave through.
//!
//! Not covered here: `cp`, `save`, `load`, `export`, `import`,
//! `secret create`, `config create`, `run` and `create`. What makes
//! those interesting is a LOCAL file crossing the wire, which is a
//! different claim from the one made here, so they have suites of their
//! own — `e2e_bridge` for the `Bridge` route and `e2e_footprint` for
//! `Footprint`. Everything below is a command whose inputs and effects
//! already live in the daemon.

mod common;

use std::time::{Duration, Instant};

use common::{TestServer, Workspace};

/// Printed by the test container on startup and read back through
/// `ulak docker logs`. Distinctive so it cannot match anything the
/// daemon says on its own.
const LOG_MARKER: &str = "ULAK-TREE-LOGS-REACHED-STDOUT";

/// A lone CR, a NUL, and two bytes that are not valid UTF-8.
///
/// This is the `ssh -t` tripwire. A pty on the far side rewrites every
/// LF as CRLF and can mangle the rest, so `ulak docker exec -i db
/// psql < dump.sql` silently corrupts the dump instead of failing —
/// which is the kind of bug a user diagnoses as "the database is
/// broken". Comparing the bytes is the only thing that notices.
const BINARY_PAYLOAD: &[u8] = b"first\nsecond\rthird\n\x00\xfe\xff\n";

// ─── local: no server, no workspace ─────────────────────────────────

/// `--help` is a question about Ulak, not about Docker: the tree is
/// rendered from the same table the router reads, on this machine.
///
/// It has to answer before `ulak init` has ever run — that is precisely
/// when somebody asks what this thing can carry. The workspace below is
/// deliberately never given a host, so a help path that tried to reach a
/// server would fail with "no Ulak workspace" instead of exiting 0.
#[test]
fn the_command_tree_prints_itself_with_no_server_and_no_workspace() {
    let ws = Workspace::new();

    let out = ws.ulak_alone().args(["docker", "--help"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "`ulak docker --help` must exit 0 without a server, got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    for want in [
        "Usage:  ulak docker COMMAND",
        "Management Commands:",
        "container",
        "compose",
        // The alias annotation can only come from the catalog's
        // `alias_of`, so its presence proves the help and the router are
        // reading one table rather than two that can drift apart.
        "(= docker container ls)",
    ] {
        assert!(
            text.contains(want),
            "`ulak docker --help` must mention {want:?}, got:\n{text}"
        );
    }

    // A family's own page, one level down. `container --help` is not a
    // help FLAG to Ulak — it resolves to the family and the dispatcher
    // prints its children — so this covers a different code path than
    // the root call above.
    let out = ws
        .ulak_alone()
        .args(["docker", "container", "--help"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "`ulak docker container --help` must exit 0 without a server, got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    for want in [
        "Manage containers",
        "Usage:  ulak docker container COMMAND",
        // Three children the eleven-branch enum never had.
        "prune",
        "exec",
        "logs",
    ] {
        assert!(
            text.contains(want),
            "`ulak docker container --help` must list {want:?}, got:\n{text}"
        );
    }

    // The two markers for "this command is real and will not travel",
    // checked on the pages that actually carry them. Both are rendered
    // from the entry's own `Route`, so the help cannot advertise a tree
    // the router does not have — and a user who reads the page before
    // typing the command is not surprised by the refusal.
    //
    // Neither marker lives at the root any more: `bake` was the last
    // `Planned` root command and it is implemented now, so every
    // remaining one sits a level down. That is why this asserts per
    // family rather than on the front page.
    for (family, leaf, marker) in [
        ("context", "ls", "[local only]"),
        ("plugin", "create", "[not yet]"),
    ] {
        let out = ws
            .ulak_alone()
            .args(["docker", family, "--help"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            out.status.success(),
            "`ulak docker {family} --help` must exit 0 — it is a page, not an attempt"
        );
        let line = text
            .lines()
            .find(|l| l.split_whitespace().next() == Some(leaf))
            .unwrap_or_else(|| panic!("`ulak docker {family} --help` has no {leaf} line:\n{text}"));
        assert!(
            line.contains(marker),
            "`{family} {leaf}` must be marked {marker} on its family page, got: {line:?}"
        );
    }

    // A family whose tail is nothing but help flags prints the tree; the
    // other spellings have to keep working too. This regressed once
    // already, when the fix for a flag between a family and its command
    // took the plain `-h` path with it.
    for tail in [&["container", "-h"][..], &["help", "container"][..]] {
        let argv = [&["docker"][..], tail].concat();
        let out = ws.ulak_alone().args(&argv).output().unwrap();
        assert!(
            out.status.success(),
            "`ulak docker {}` must print the tree locally, got {:?}\nstderr: {}",
            tail.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("Manage containers"),
            "`ulak docker {}` must print the container family's page",
            tail.join(" ")
        );
    }
}

/// A refusal is a product surface. One that does not say WHY leaves the
/// user's next move as "work around Ulak", which is how a wrapper stops
/// being trusted — so each of these has to name the reason AND a way
/// forward.
///
/// All three are decided before any ssh: a rejected global and an
/// `Excluded` route never open a connection, and an unknown command
/// never gets past the table. So they answer identically on a laptop
/// with no server, which is where somebody typing a typo usually is.
#[test]
fn every_refusal_names_the_reason_and_the_way_forward() {
    let ws = Workspace::new();

    // Contexts are this machine's client state. Forwarded, the command
    // would edit the SERVER's context list — a real effect, on the wrong
    // machine, reported as success.
    let said = refusal(&ws, &["docker", "context", "ls"]);
    for want in [
        "not carried to the server",
        "client state",
        "workspace config",
    ] {
        assert!(
            said.contains(want),
            "the context refusal must explain {want:?}, got:\n{said}"
        );
    }

    // `-H` and `--host` are one flag with two spellings; a refusal that
    // only knew the long one would be a hole big enough to drive a
    // production daemon through.
    for spelling in ["-H", "--host"] {
        let said = refusal(&ws, &["docker", spelling, "tcp://x", "ps"]);
        assert!(
            said.contains(&format!("`{spelling}` cannot be forwarded")),
            "`{spelling}` must be refused by name, got:\n{said}"
        );
        assert!(
            said.contains("workspace config"),
            "`{spelling}` must say where the daemon actually comes from, got:\n{said}"
        );
    }

    // A typo is the common case, and the fix is one word away. Being
    // handed the whole tree instead of a suggestion is what makes people
    // stop reading errors.
    let said = refusal(&ws, &["docker", "contaner", "ls"]);
    assert!(
        said.contains("no command `contaner`"),
        "the typo must be named, got:\n{said}"
    );
    assert!(
        said.contains("did you mean `ulak docker container`?"),
        "the typo must be offered `container`, got:\n{said}"
    );

    // A typo one level down has to resolve against that family's
    // children, not the root's — `creat` must find `network create`.
    let said = refusal(&ws, &["docker", "network", "creat", "x"]);
    assert!(
        said.contains("`docker network` has no command `creat`")
            && said.contains("did you mean `ulak docker network create`?"),
        "a nested typo must be scoped to its family, got:\n{said}"
    );

    // A password on the command line is refused rather than redacted.
    // Redaction only ever covered Ulak's own audit trail; `Remote::spell`
    // builds ONE remote shell string, so the value was visible in `ps` to
    // every other user on that server for the life of the call. All four
    // spellings pflag accepts, `-phunter2` included — that is the one
    // that hides the value inside the flag's own word and slips past
    // anything keyed on `=` or on an exact match.
    for spelling in [
        &["-p", "hunter2"][..],
        &["-phunter2"][..],
        &["--password", "hunter2"][..],
        &["--password=hunter2"][..],
    ] {
        let argv = [&["docker", "login", "-u", "someone"][..], spelling].concat();
        let said = refusal(&ws, &argv);
        assert!(
            said.contains("would put your password in an argument list on the server"),
            "`login {}` must be refused, got:\n{said}",
            spelling.join(" ")
        );
        assert!(
            said.contains("--password-stdin"),
            "the refusal must name the safe form, got:\n{said}"
        );
        assert!(
            !said.contains("hunter2"),
            "the refusal must not echo the password back, got:\n{said}"
        );
    }

    // A flag between a family and its command hides the command from
    // Ulak entirely: `buildx --builder mine build .` resolves to the
    // `buildx` family with no verb, and forwarding that on a guess is how
    // a build lands somewhere nobody asked for.
    let said = refusal(
        &ws,
        &["docker", "buildx", "--builder", "mine", "build", "."],
    );
    assert!(
        said.contains("is a group of commands, and `--builder` is not one of them"),
        "a flag before the command must be named, got:\n{said}"
    );
    assert!(
        said.contains("put it after"),
        "the refusal must say where the flag belongs, got:\n{said}"
    );

    // Compose builds its own `docker compose` prefix on the far side, so
    // a Docker global typed before the word `compose` has no seat on
    // that command line. Refused rather than dropped: a
    // `--log-level debug` that silently vanishes is an afternoon spent
    // wondering why nothing ever got more verbose. Both spellings,
    // because `-l` is the one somebody actually types.
    for global in [&["--log-level", "debug"][..], &["-l", "debug"][..]] {
        let argv = [&["docker"][..], global, &["compose", "up"][..]].concat();
        let said = refusal(&ws, &argv);
        assert!(
            said.contains(&format!(
                "`{}` cannot come before `compose`",
                global.join(" ")
            )),
            "`{}` before compose must be refused by name, got:\n{said}",
            global.join(" ")
        );
        assert!(
            said.contains("Compose has its own globals"),
            "the refusal must point at where the flag does belong, got:\n{said}"
        );
    }
}

/// The catalog's stated policy applied to the one spelling that has two
/// possible meanings and no way to choose between them.
///
/// Its own `#[test]` rather than a line in the sweep above, because it
/// is the only refusal here that needs a host to be NAMED. It still
/// needs no server: `runspec::run` calls `Workspace::locate` and
/// `Remote::to` before `RunSpec::parse`, and neither of them opens a
/// connection — `Remote::to` builds an `Ssh` and stops. It used to sit
/// in e2e_footprint, where it took a share of the fixture and never sent
/// a byte over it.
#[test]
fn a_bind_source_above_the_workspace_is_refused_rather_than_guessed() {
    let ws = Workspace::new();
    // Registered and never reached: the refusal happens before anything
    // would dial it.
    ws.set_host("ulak-e2e-never-reached");

    // Ulak carries the workspace and only the workspace, so a source
    // above it has no remote spelling at all. Refusing costs a minute;
    // picking one of the two possible meanings costs an afternoon of
    // wondering which machine the container read.
    let said = refusal(
        &ws,
        &[
            "docker",
            "run",
            "--rm",
            "-v",
            "../elsewhere:/app",
            "alpine:3.20",
            "true",
        ],
    );
    assert!(
        said.contains("outside the workspace"),
        "the refusal must name the reason, got:\n{said}"
    );
    assert!(
        said.contains("name it absolutely or as ~/…"),
        "the refusal must name the way forward, got:\n{said}"
    );
}

/// stderr of one `ulak docker …` that MUST fail, with the project's
/// no-dead-end-errors contract checked in passing.
///
/// A refusal that quietly succeeded is the bug this returns nothing for,
/// so stdout goes into the panic message: "exit 0" alone would not say
/// what the command did instead.
///
/// The contract check used to be `said.contains("now")`, and that was a
/// check no refusal on any code path could fail. `ui::render_error`
/// prints a `now` line for EVERY error — an error carrying no steps of
/// its own falls through to `now run: ulak doctor` — so the word is
/// always there whatever the error said; and "now" is a substring of
/// "know", "unknown" and "nowhere" besides, so even deleting that line
/// would not have made it red. A test that cannot fail reads as coverage
/// and is worse than none.
///
/// What a refusal actually owes the user is a step of its OWN. So the
/// check is structural: find the rendered `now` line, and reject the
/// catch-all by name. Verified red — `ulak docker ps` from a deleted
/// working directory fails with `cannot read current directory` and
/// nothing but the catch-all, and this rejects it while the old
/// `contains("now")` waved it through.
fn refusal(ws: &Workspace, args: &[&str]) -> String {
    let out = ws.ulak_alone().args(args).output().unwrap();
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "`ulak {}` must be refused, but it exited {:?}\nstdout: {}",
        args.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout)
    );
    names_a_way_forward(&said, &format!("ulak {}", args.join(" ")));
    said
}

/// The rendered shape of the project's promise, asserted rather than
/// assumed: `error <what>` then `  now <do this>`. Stderr is a pipe
/// here, so `ui::style()` is empty and the line arrives unstyled.
fn names_a_way_forward(said: &str, what: &str) {
    let step = said
        .lines()
        .find_map(|l| l.strip_prefix("  now "))
        .unwrap_or_else(|| panic!("`{what}` printed no next step at all:\n{said}"));
    assert!(
        !step.starts_with("run: ulak doctor"),
        "`{what}` fell through to the catch-all next step, which means this refusal carries no \
         guidance of its own:\n{said}"
    );
    assert!(
        !step.trim().is_empty(),
        "`{what}` printed an empty next step:\n{said}"
    );
}

// ─── remote: a real daemon answers ──────────────────────────────────

/// One scenario's own corner of the shared remote daemon: a label
/// nothing else can match, and whatever containers, volumes and tags it
/// asked for — all removed however the test ends.
///
/// These fixtures used to be made ONCE and passed to nine scenarios
/// called in sequence from a single `#[test]`. That is what made this
/// file report one verdict for nine claims: a break in the third
/// scenario aborted the six after it and printed a single failure, so a
/// regression sweep could not see how much else was broken. A container
/// costs about a second, which is what makes nine independent verdicts
/// affordable — the DinD boot, which is the expensive part, is still
/// paid once for the whole binary (`TestServer::shared`).
///
/// The label is what keeps that safe now that the scenarios run in
/// parallel against one daemon: every `--filter` below is scoped to it,
/// and `prune` without it would reach another scenario's containers —
/// or, on a real host, somebody else's.
struct Probe<'a> {
    server: &'a TestServer,
    /// `ulak.e2e.tree=<pid>-<scenario>`, unique on this daemon.
    label: String,
    containers: Vec<String>,
    volumes: Vec<String>,
    images: Vec<String>,
}

impl Probe<'_> {
    /// The base image is asked for here rather than inside each
    /// scenario: a missing one otherwise surfaces as "no such container"
    /// further down, which sends the reader looking at the routing
    /// instead of at the network. `needs_image` pulls it once for the
    /// whole binary — nine scenarios pulling it at once is a Docker Hub
    /// rate limit, not a test.
    fn new<'a>(server: &'a TestServer, what: &str) -> Probe<'a> {
        server.needs_image("alpine:3.20");
        Probe {
            server,
            label: format!("ulak.e2e.tree={}-{what}", std::process::id()),
            containers: Vec::new(),
            volumes: Vec::new(),
            images: Vec::new(),
        }
    }

    /// The `--filter` every listing in this scenario is scoped by.
    fn by_label(&self) -> String {
        format!("label={}", self.label)
    }

    /// A long-lived container that has ALREADY said something: `logs`
    /// needs output to fetch and `exec` needs somewhere to run, and a
    /// container that exits immediately gives neither.
    fn talker(&mut self) -> String {
        let name = self.name("talker");
        self.containers.push(name.clone());
        let made = self.server.ssh(&format!(
            "docker run -d --name {name} --label {label} alpine:3.20 \
             sh -c 'echo {LOG_MARKER}; sleep 300'",
            label = self.label,
        ));
        assert!(
            made.status.success(),
            "could not create the remote test container: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        name
    }

    /// A container that has already exited, with its id — which is the
    /// only kind `prune` will take, and the id is what the prune has to
    /// name.
    fn stopped(&mut self) -> (String, String) {
        let name = self.name("goner");
        self.containers.push(name.clone());
        let made = self.server.ssh(&format!(
            "docker run -d --name {name} --label {label} alpine:3.20 true",
            label = self.label,
        ));
        assert!(
            made.status.success(),
            "could not create the container to be pruned: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        let id = String::from_utf8_lossy(&made.stdout).trim().to_string();
        assert!(!id.is_empty(), "docker run -d printed no container id");
        // `docker run -d` returns once the container is CREATED. Prune
        // skips anything still running, so waiting for the exit is what
        // makes this a test rather than a coin flip.
        self.server.ssh(&format!("docker wait {id} >/dev/null"));
        (name, id)
    }

    fn volume(&mut self) -> String {
        let name = self.name("volume");
        self.volumes.push(name.clone());
        let made = self.server.ssh(&format!(
            "docker volume create --label {label} {name} >/dev/null",
            label = self.label,
        ));
        assert!(
            made.status.success(),
            "could not create the remote test volume: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        name
    }

    /// A tag this scenario may destroy. Recorded before anything makes
    /// it, so a panic between the two still gets it cleaned up.
    fn tag(&mut self, what: &str) -> String {
        let tag = format!("{}:test", self.name(what));
        self.images.push(tag.clone());
        tag
    }

    /// Names carry the pid so two checkouts on one server cannot
    /// collide, and the scenario's own word so two scenarios in one
    /// process cannot either.
    fn name(&self, what: &str) -> String {
        let scope = self.label.split_once('=').map_or("", |(_, v)| v);
        format!("ulak-tree-{what}-{scope}")
    }
}

impl Drop for Probe<'_> {
    fn drop(&mut self) {
        let mut removals: Vec<String> = Vec::new();
        removals.extend(self.containers.iter().map(|c| format!("docker rm -f {c}")));
        removals.extend(
            self.volumes
                .iter()
                .map(|v| format!("docker volume rm -f {v}")),
        );
        removals.extend(
            self.images
                .iter()
                .map(|i| format!("docker image rm -f {i}")),
        );
        // Each one silenced and forgiven separately: most of these are
        // already gone by the time this runs (the scenarios remove them
        // as their assertions), and one `|| true` at the end would let a
        // real failure hide behind an expected one.
        let script = removals
            .iter()
            .map(|r| format!("{r} >/dev/null 2>&1 || true"))
            .collect::<Vec<_>>()
            .join("; ");
        self.server.ssh(&script);
    }
}

/// The workspace a remote scenario drives ulak from. It never gets a
/// compose file's worth of meaning — a `Daemon` route syncs nothing —
/// but it is what points ulak at this server.
fn wired(server: &TestServer) -> Workspace {
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws
}

/// The daemon-only listings, every one of which used to be unreachable.
///
/// `version` and `info` are compared against what the SERVER's own
/// docker says rather than against a hard-coded string: that is the one
/// assertion that distinguishes "the remote daemon answered" from "some
/// daemon answered", and it holds on the dockerized fixture and on a
/// real host alike, whatever versions they run.
#[test]
fn every_daemon_listing_answers_from_the_remote_daemon() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "listings");
    let talker = probe.talker();
    let volume = probe.volume();
    let by_label = probe.by_label();

    let theirs = server_says(&server, "docker version --format '{{.Server.Version}}'");
    let ours = docker_stdout(
        &ws,
        &server,
        &["version", "--format", "{{.Server.Version}}"],
    );
    assert_eq!(
        ours.trim(),
        theirs,
        "`ulak docker version` must report the REMOTE daemon; the server says \
         {theirs:?} and Ulak said {:?}",
        ours.trim()
    );

    let theirs = server_says(&server, "docker info --format '{{.ServerVersion}}'");
    let ours = docker_stdout(&ws, &server, &["info", "--format", "{{.ServerVersion}}"]);
    assert_eq!(
        ours.trim(),
        theirs,
        "`ulak docker info` must report the REMOTE daemon; the server says \
         {theirs:?} and Ulak said {:?}",
        ours.trim()
    );

    let listed = docker_stdout(
        &ws,
        &server,
        &[
            "container",
            "ls",
            "-a",
            "--filter",
            &by_label,
            "--format",
            "{{.Names}}",
        ],
    );
    assert!(
        listed.lines().any(|l| l == talker),
        "`ulak docker container ls` must list {talker:?}, got:\n{listed}"
    );

    let images = docker_stdout(
        &ws,
        &server,
        &[
            "image",
            "ls",
            "--format",
            "{{.Repository}}:{{.Tag}}",
            "alpine",
        ],
    );
    assert!(
        images.lines().any(|l| l == "alpine:3.20"),
        "`ulak docker image ls` must list alpine:3.20, got:\n{images}"
    );

    let volumes = docker_stdout(&ws, &server, &["volume", "ls", "-q", "--filter", &by_label]);
    assert_eq!(
        volumes.trim(),
        volume,
        "`ulak docker volume ls` must list exactly {volume:?}, got:\n{volumes}"
    );

    // The three networks every daemon is born with. Asserted because
    // they need no setup at all: if this table is empty, the command did
    // not reach a daemon, full stop.
    let networks = docker_stdout(&ws, &server, &["network", "ls", "--format", "{{.Name}}"]);
    for want in ["bridge", "host", "none"] {
        assert!(
            networks.lines().any(|l| l == want),
            "`ulak docker network ls` must include the built-in {want:?} network, got:\n{networks}"
        );
    }

    let usage = docker_stdout_after_storage_settles(&ws, &server, &["system", "df"]);
    for want in ["RECLAIMABLE", "Images", "Containers", "Local Volumes"] {
        assert!(
            usage.contains(want),
            "`ulak docker system df` must report {want:?}, got:\n{usage}"
        );
    }

    ws.forget_on_server(&server);
}

/// Docker's root shorthands and their nested spellings are ONE command —
/// that is what the catalog's `alias_of` records, and the only proof of
/// it that cannot rot is identical output for identical flags.
///
/// The pairs here are the read-only ones, so both spellings can run
/// against the same state; the destructive pair gets its own test below
/// because the first removal leaves the second nothing to do.
#[test]
fn a_root_shorthand_and_its_nested_spelling_are_one_command() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "aliases");
    let talker = probe.talker();
    let by_label = probe.by_label();

    let listing = same_command(
        &ws,
        &server,
        &["ps"],
        &["container", "ls"],
        &["-a", "--filter", &by_label, "--format", "{{.Names}}"],
    );
    assert!(
        listing.lines().any(|l| l == talker),
        "both spellings must list {talker:?}, got:\n{listing}"
    );

    let images = same_command(
        &ws,
        &server,
        &["images"],
        &["image", "ls"],
        &["--format", "{{.Repository}}:{{.Tag}}", "alpine"],
    );
    assert!(
        images.lines().any(|l| l == "alpine:3.20"),
        "both spellings must list alpine:3.20, got:\n{images}"
    );

    let history = same_command(
        &ws,
        &server,
        &["history"],
        &["image", "history"],
        &["--format", "{{.CreatedBy}}", "alpine:3.20"],
    );
    assert!(
        !history.trim().is_empty(),
        "both spellings must return alpine's build history, got nothing"
    );

    // `info` is the pair whose value can be checked against a third
    // source, so this one proves more than agreement: both spellings
    // reach the same daemon, and it is the remote one.
    let version = same_command(
        &ws,
        &server,
        &["info"],
        &["system", "info"],
        &["--format", "{{.ServerVersion}}"],
    );
    assert_eq!(
        version.trim(),
        server_says(&server, "docker info --format '{{.ServerVersion}}'"),
        "both spellings of info must report the remote daemon's version"
    );

    ws.forget_on_server(&server);
}

/// Docker's UNPRINTED long forms — the ones no `--help` output mentions
/// and no reader of the docs would know to try.
///
/// `docker volume list` and `docker container ps` are real commands that
/// Docker itself never advertises; they were found by measuring the tree
/// with `docker __complete` rather than by reading it. That is exactly
/// why they are worth a test: a hand-maintained table has no way to
/// notice it is missing a spelling nobody prints, so the only thing that
/// keeps them routed is a test that names them.
#[test]
fn an_unprinted_long_form_resolves_to_the_same_command() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "unprinted");
    let talker = probe.talker();
    let volume = probe.volume();
    let by_label = probe.by_label();

    let volumes = same_command(
        &ws,
        &server,
        &["volume", "list"],
        &["volume", "ls"],
        &["-q", "--filter", &by_label],
    );
    assert_eq!(
        volumes.trim(),
        volume,
        "both spellings must list exactly {volume:?}, got:\n{volumes}"
    );

    let listing = same_command(
        &ws,
        &server,
        &["container", "ps"],
        &["container", "ls"],
        &["-a", "--filter", &by_label, "--format", "{{.Names}}"],
    );
    assert!(
        listing.lines().any(|l| l == talker),
        "both spellings must list {talker:?}, got:\n{listing}"
    );

    ws.forget_on_server(&server);
}

/// The destructive half of the alias claim.
///
/// `rmi` and `image rm` cannot both be run against one target — the
/// first removal leaves the second nothing to do, and "no such image"
/// would look exactly like a routing failure. So each spelling gets its
/// own identically-made tag and has to remove it, which also exercises
/// `tag` on the way in.
#[test]
fn both_spellings_of_image_removal_actually_remove() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "removal");
    let alias_a = probe.tag("alias-a");
    let alias_b = probe.tag("alias-b");

    for tag in [&alias_a, &alias_b] {
        ws.ulak(&server)
            .args(["docker", "tag", "alpine:3.20", tag])
            .assert()
            .success();
        assert!(
            server
                .ssh(&format!("docker image inspect {tag} >/dev/null"))
                .status
                .success(),
            "`ulak docker tag` did not create {tag} on the remote daemon"
        );
    }

    ws.ulak(&server)
        .args(["docker", "rmi", &alias_a])
        .assert()
        .success();
    ws.ulak(&server)
        .args(["docker", "image", "rm", &alias_b])
        .assert()
        .success();

    for tag in [&alias_a, &alias_b] {
        assert!(
            !server
                .ssh(&format!("docker image inspect {tag} >/dev/null 2>&1"))
                .status
                .success(),
            "{tag} survived its removal — both spellings must really remove"
        );
    }

    ws.forget_on_server(&server);
}

/// `logs` and `exec` are `Route::Stream`: the same ssh transport as a
/// daemon call, but they hold the terminal.
///
/// Every invocation here is deliberately terminal-free — no `-t`, and
/// stdin is a pipe — which is what a script and CI do. It is also the
/// only shape a test can assert on: with a pty in the way the bytes
/// below stop being comparable.
#[test]
fn logs_and_exec_stream_back_without_a_terminal() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "streams");
    let talker = probe.talker();

    // The container's `echo` races this command's first attempt, so the
    // marker is polled rather than assumed. `docker run -d` returns as
    // soon as the container is created, not once it has said anything.
    let deadline = Instant::now() + Duration::from_secs(30);
    let logs = loop {
        let text = docker_stdout(&ws, &server, &["logs", &talker]);
        if text.contains(LOG_MARKER) {
            break text;
        }
        assert!(
            Instant::now() < deadline,
            "`ulak docker logs {talker}` never carried {LOG_MARKER:?} home; last output:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    assert!(
        !logs.contains('\r'),
        "`ulak docker logs` output must be LF-only over a pipe, got:\n{logs:?}"
    );

    // `-t` here is Docker's `--timestamps`, NOT a TTY request. Reading it
    // as one across the whole tree is what put CR LF into every
    // redirected log file, so the flag has to survive its own spelling.
    let stamped = docker_stdout(&ws, &server, &["logs", "-t", &talker]);
    assert!(
        stamped.contains(LOG_MARKER),
        "`ulak docker logs -t` must still carry the output, got:\n{stamped}"
    );
    assert!(
        !stamped.contains('\r'),
        "`ulak docker logs -t` must not allocate a pty — CR found in:\n{stamped:?}"
    );

    // The same claim in the shape the bug was actually reported in:
    // `ulak docker logs -t <name> > out.log`. A real file, not a pipe —
    // the TTY decision reads `stdout().is_terminal()`, and a redirect is
    // the case a user hits while their terminal is still attached to
    // stdin, which is the half a captured-pipe test cannot reach.
    let redirected = ws.home.join("../logs-t.log");
    let sink = std::fs::File::create(&redirected).expect("create the redirect target");
    let status = ws
        .ulak_raw(&server)
        .args(["docker", "logs", "-t", &talker])
        .stdout(std::process::Stdio::from(sink))
        .status()
        .expect("spawn ulak docker logs");
    assert!(status.success(), "a redirected `logs -t` must succeed");
    let written = std::fs::read(&redirected).expect("read the redirect target");
    assert!(
        !written.contains(&b'\r'),
        "`ulak docker logs -t > file` must write LF-only, got:\n{:?}",
        String::from_utf8_lossy(&written)
    );
    assert!(
        String::from_utf8_lossy(&written).contains(LOG_MARKER),
        "the redirected file must hold the container's output, got:\n{:?}",
        String::from_utf8_lossy(&written)
    );

    // `exec` with no `-t`: the plain-docker equivalent of compose's
    // `exec -T`, and the command the old enum could not reach at all.
    let said = docker_stdout(&ws, &server, &["exec", &talker, "echo", "hi"]);
    assert_eq!(
        said, "hi\n",
        "`ulak docker exec {talker} echo hi` must print exactly \"hi\\n\", got {said:?}"
    );

    ws.forget_on_server(&server);
}

/// The classic `ssh -t` trap, measured on the bytes.
///
/// `exec -i` is the shape a piped input takes — `ulak docker exec -i db
/// psql < dump.sql`. A pty on the far side rewrites LF as CRLF, so the
/// dump arrives corrupted and nothing errors; the failure is reported
/// days later as a broken database. Comparing raw bytes is what catches
/// it, and a NUL plus two non-UTF-8 bytes are in the payload so a
/// lossy-string round trip cannot pass either.
#[test]
fn a_piped_stdin_arrives_byte_for_byte() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "stdin");
    let talker = probe.talker();

    let out = ws
        .ulak(&server)
        .args(["docker", "exec", "-i", &talker, "cat"])
        .write_stdin(BINARY_PAYLOAD.to_vec())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ulak docker exec -i {talker} cat` failed with {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout, BINARY_PAYLOAD,
        "piped stdin must arrive byte-for-byte\n  expected: {BINARY_PAYLOAD:?}\n  got:      {:?}",
        out.stdout
    );

    ws.forget_on_server(&server);
}

/// The exit code is the whole answer for a script: `&&` chains and CI
/// gates read nothing else.
///
/// Before the catalog, `exec` never reached a server — clap answered 1,
/// which is indistinguishable from the command itself having failed. So
/// a code that is neither 0 nor 1 is the one worth asserting: 7 can only
/// have come from the far side.
#[test]
fn an_exec_exit_code_arrives_verbatim() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "exitcode");
    let talker = probe.talker();

    let out = ws
        .ulak(&server)
        .args(["docker", "exec", &talker, "sh", "-c", "exit 7"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(7),
        "`ulak docker exec … 'exit 7'` must exit 7, got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    // The other end of the same claim: a command that worked must not
    // inherit a stray non-zero from the transport.
    let out = ws
        .ulak(&server)
        .args(["docker", "exec", &talker, "true"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "`ulak docker exec … true` must exit 0, got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    ws.forget_on_server(&server);
}

/// The other half of the Compose-globals refusal, and the half a
/// refusal can quietly overreach into.
///
/// `--log-level` before `compose` is an error because Compose builds its
/// own prefix on the far side. Everywhere else the global genuinely
/// rides along in argv, so the same flag before `ps` must WORK — and
/// still return the same listing it returns without it. A refusal that
/// spread to every route would be a regression that the Compose test
/// alone cannot see.
#[test]
fn a_docker_global_rides_along_on_every_route_but_compose() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "globals");
    let talker = probe.talker();
    let by_label = probe.by_label();
    let tail = ["-a", "--filter", &by_label, "--format", "{{.Names}}"];

    let plain = docker_stdout(&ws, &server, &[&["ps"][..], &tail[..]].concat());
    let with_global = docker_stdout(
        &ws,
        &server,
        &[&["--log-level", "debug", "ps"][..], &tail[..]].concat(),
    );
    assert!(
        with_global.lines().any(|l| l == talker),
        "`ulak docker --log-level debug ps` must still list {talker:?}, got:\n{with_global}"
    );
    // Docker writes its own debug chatter to stderr, so a global that
    // reached the far side changes nothing on stdout. That is the point:
    // the flag travels without disturbing the answer.
    assert_eq!(
        with_global, plain,
        "a Docker global must not change what `ps` reports on stdout\n  without: \
         {plain:?}\n  with:    {with_global:?}"
    );

    ws.forget_on_server(&server);
}

/// `container prune` is the daemon command whose own output names what
/// it destroyed, so the assertion can be the container id rather than
/// "the exit status was zero".
///
/// Scoped by label on purpose: an unfiltered prune reaches every stopped
/// container on that daemon, which now includes the other scenarios in
/// this file, and on a real host somebody else's besides. The running
/// container is left as the control — it shares the label and must
/// survive, which proves the prune was selective rather than merely
/// quiet.
#[test]
fn container_prune_names_the_container_it_removed() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "prune");
    let talker = probe.talker();
    let (goner, id) = probe.stopped();
    let by_label = probe.by_label();

    let pruned = docker_stdout(
        &ws,
        &server,
        &["container", "prune", "-f", "--filter", &by_label],
    );
    assert!(
        pruned.contains("Total reclaimed space"),
        "`ulak docker container prune` must report what it reclaimed, got:\n{pruned}"
    );
    assert!(
        pruned.contains(&id),
        "the prune must name the container it removed ({id}), got:\n{pruned}"
    );
    assert!(
        !server
            .ssh(&format!("docker container inspect {id} >/dev/null 2>&1"))
            .status
            .success(),
        "{goner} survived a prune that claimed to have removed it"
    );
    assert!(
        server
            .ssh(&format!("docker container inspect {talker} >/dev/null"))
            .status
            .success(),
        "the prune took the RUNNING {talker} with it — the label filter did not hold"
    );

    ws.forget_on_server(&server);
}

// ─── helpers ────────────────────────────────────────────────────────

/// stdout of one `ulak docker …`, with both streams in the panic
/// message.
///
/// A remote failure otherwise arrives as an empty string, and the
/// assertion that follows then blames the content instead of the
/// connection — which is the wrong end of the problem to start reading
/// from.
fn docker_stdout(ws: &Workspace, server: &TestServer, args: &[&str]) -> String {
    let out = ws.ulak(server).arg("docker").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "`ulak docker {}` failed with {:?}\nstdout: {}\nstderr: {}",
        args.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Docker 29's global storage report is not atomic with container removal.
/// The scenarios in this binary deliberately share one daemon and run in
/// parallel, so another scenario's `Probe::drop` can remove its own container
/// between `system df` reading the container and its rw-layer snapshot. The
/// daemon then returns `rw layer snapshot not found` until that removal
/// settles. Retry only that measured daemon race; every transport, routing or
/// other Docker error remains an immediate failure.
fn docker_stdout_after_storage_settles(
    ws: &Workspace,
    server: &TestServer,
    args: &[&str],
) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = ws.ulak(server).arg("docker").args(args).output().unwrap();
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).into_owned();
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.contains("rw layer snapshot not found") || Instant::now() >= deadline {
            panic!(
                "`ulak docker {}` failed with {:?}\nstdout: {}\nstderr: {}",
                args.join(" "),
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                stderr
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Two spellings of one command, run with the same flags, asserted equal
/// — and the shared output handed back so the caller can also say what
/// it should CONTAIN. Agreement alone would be satisfied by two empty
/// tables.
fn same_command(
    ws: &Workspace,
    server: &TestServer,
    short: &[&str],
    nested: &[&str],
    tail: &[&str],
) -> String {
    let short_name = short.join(" ");
    let nested_name = nested.join(" ");
    let from_short = docker_stdout(ws, server, &[short, tail].concat());
    let from_nested = docker_stdout(ws, server, &[nested, tail].concat());
    assert_eq!(
        from_short, from_nested,
        "`docker {short_name}` is an alias of `docker {nested_name}` in the catalog, so \
         identical flags must give identical output\n  {short_name}: {from_short:?}\n  \
         {nested_name}: {from_nested:?}"
    );
    from_short
}

/// What the server's own docker says, through the harness's ssh rather
/// than through Ulak — the independent second opinion the version
/// assertions compare against. Asserted non-empty so two blanks can
/// never agree with each other.
fn server_says(server: &TestServer, remote_cmd: &str) -> String {
    let out = server.ssh(remote_cmd);
    let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        out.status.success() && !said.is_empty(),
        "the harness could not read `{remote_cmd}` from the server: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    said
}
