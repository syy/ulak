//! `docker run` and `docker create` against a real server: the route
//! whose flags quietly name directories on THIS machine.
//!
//! `docker run -v .:/app node npm test` is the most common thing anybody
//! types and the one that cannot be forwarded. On the server `.` is
//! wherever the ssh session landed, so the container mounts the wrong
//! tree — or an empty new one docker helpfully creates — and nothing
//! errors. The test suite inside it just runs against no source code.
//!
//! So no scenario here asserts an exit status. Every one of them makes
//! the two machines DISAGREE first: a decoy of the same name is planted
//! in the server's login directory, holding different bytes, and the
//! container is asked to read the file. A forwarded command reads the
//! decoy and exits 0. Only the bytes tell them apart.
//!
//! The `.env` scenario is the one that already went wrong. Classifying a
//! source by `./` alone sent every dotfile down the named-volume arm,
//! where nothing is synced, nothing is respelled and nothing is said —
//! and Docker, which reads any leading dot as a host path, then mounted
//! the SERVER's `.env` into the container. That is this route's whole
//! failure mode reached through a spelling, and until now it had no
//! end-to-end test at all: `Route::Footprint` was the only transport in
//! the catalog with zero coverage.
//!
//! Cost: these scenarios share ONE fixture (`TestServer::shared`) and
//! run in parallel, so everything each of them makes — container names,
//! image tags, volumes, decoys — carries the scenario's own name.

mod common;

use common::{TestServer, Workspace};

/// The image every scenario runs.
const IMAGE: &str = "alpine:3.20";

/// A workspace pointed at the shared server, with the base image
/// present. `needs_image` pulls it once for the whole binary: ten
/// scenarios pulling it at the same moment is a Docker Hub rate limit,
/// not a test.
fn footprint_workspace(server: &TestServer) -> Workspace {
    server.needs_image(IMAGE);
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws
}

/// A file of the same name in the server's LOGIN directory, holding
/// different bytes.
///
/// This is the whole method. An ssh session lands in `$HOME`, so a
/// forwarded `-v ./app:/app` is resolved by the server's own docker
/// against `$HOME` — and if nothing is there, docker CREATES it, empty,
/// and the container reads an empty directory while exiting 0. Planting
/// something readable and wrong turns that silence into a comparison.
fn plant_a_decoy(server: &TestServer, rel: &str, body: &str) {
    let out = server.ssh(&format!(
        "set -e; mkdir -p \"$(dirname ~/{rel})\"; printf %s {body} > ~/{rel}",
        body = q(body),
    ));
    assert!(
        out.status.success(),
        "could not plant the decoy ~/{rel} on the server: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Read a decoy back. A scenario asserts on this as well as on the
/// container's output: the local file must reach the container AND the
/// server's own copy must be left exactly as it was, because a sync that
/// overwrote it would pass the first assertion for the wrong reason.
fn decoy_says(server: &TestServer, rel: &str) -> String {
    let out = server.ssh(&format!("cat ~/{rel} 2>/dev/null"));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Everything a scenario leaves on the server, removed however it ends.
///
/// The backend may be somebody's real host, so the decoys go too — and
/// `rmdir` rather than `rm -rf` for the ones docker may have created:
/// it removes an empty directory and refuses everything else, which is
/// exactly the difference between clearing up after a regression and
/// deleting a file that was already there.
struct RemoteCleanup<'a> {
    server: &'a TestServer,
    containers: Vec<String>,
    volumes: Vec<String>,
    /// Paths under the server's home, removed with `rm -f`.
    decoys: Vec<String>,
    /// Paths under the server's home removed only if they are empty
    /// directories docker itself made.
    docker_made: Vec<String>,
}

impl RemoteCleanup<'_> {
    fn new(server: &TestServer) -> RemoteCleanup<'_> {
        RemoteCleanup {
            server,
            containers: Vec::new(),
            volumes: Vec::new(),
            decoys: Vec::new(),
            docker_made: Vec::new(),
        }
    }
}

impl Drop for RemoteCleanup<'_> {
    fn drop(&mut self) {
        let mut removals: Vec<String> = Vec::new();
        removals.extend(self.containers.iter().map(|c| format!("docker rm -f {c}")));
        removals.extend(
            self.volumes
                .iter()
                .map(|v| format!("docker volume rm -f {v}")),
        );
        removals.extend(self.decoys.iter().map(|d| format!("rm -rf ~/{d}")));
        removals.extend(self.docker_made.iter().map(|d| format!("rmdir ~/{d}")));
        // Silenced and forgiven one at a time: most of these are already
        // gone, and a single `|| true` at the end would let a real
        // failure hide behind an expected one.
        let script = removals
            .iter()
            .map(|r| format!("{r} >/dev/null 2>&1 || true"))
            .collect::<Vec<_>>()
            .join("; ");
        self.server.ssh(&script);
    }
}

// ─── bind sources ───────────────────────────────────────────────────

/// The headline claim, and the only one that needs no flag at all to be
/// interesting: `-v ./app:/app` mounts THIS machine's directory.
///
/// The decoy is what makes it a test rather than a smoke check. Without
/// one, a forwarded command would mount an empty directory docker
/// created in the server's home and the container would print nothing —
/// which an "is not empty" assertion catches but a careless one does
/// not. With it, the two readings produce two different strings.
#[test]
fn a_relative_bind_source_is_this_machines_directory_and_not_the_servers() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = footprint_workspace(&server);
    let dir = unique("app");
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.decoys.push(dir.clone());

    ws.write(&format!("{dir}/marker.txt"), "LOCAL-APP\n");
    plant_a_decoy(&server, &format!("{dir}/marker.txt"), "SERVER-DECOY\n");

    let out = ws
        .ulak(&server)
        .args(["docker", "run", "--rm", "-v"])
        .arg(format!("./{dir}:/app:ro"))
        .args([IMAGE, "cat", "/app/marker.txt"])
        .output()
        .unwrap();
    assert_run(&out, "run -v ./<dir>:/app");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "LOCAL-APP\n",
        "the container read the wrong machine's file; the server's own copy of \
         ~/{dir}/marker.txt holds {:?}",
        decoy_says(&server, &format!("{dir}/marker.txt"))
    );
    assert_eq!(
        decoy_says(&server, &format!("{dir}/marker.txt")),
        "SERVER-DECOY\n",
        "the sync wrote over the server's login directory instead of the workspace"
    );

    ws.forget_on_server(&server);
}

/// The regression this whole file was written for.
///
/// A `-v` source with no leading slash or dot is a NAMED VOLUME, which
/// is daemon state and nothing of ours to carry. Reading that rule as
/// "starts with `./` or `../`" put every dotfile on the wrong side of
/// it — and Docker reads ANY leading dot as a host path (a volume name
/// is `[a-zA-Z0-9][a-zA-Z0-9_.-]*` and cannot begin with one), so
/// `-v .env:/x` was forwarded verbatim and the container was handed the
/// SERVER's `.env`. Nothing was synced, nothing was respelled, nothing
/// was said, and the command exited 0.
///
/// Two dotfiles here for two different reasons: one this scenario owns,
/// so a decoy can be planted and removed even when the backend is
/// somebody's real host — that is the one that proves the pre-fix
/// behaviour was silently WRONG rather than merely broken — and `.env`
/// itself, because it is the name people actually type. Nothing is
/// planted for `.env`; a regression would have docker create it as an
/// empty directory in the server's home, which `RemoteCleanup` takes
/// back with `rmdir` and nothing else.
#[test]
fn a_dotfile_bind_source_is_a_path_and_not_a_volume_name() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = footprint_workspace(&server);
    // A dotfile whose name is this test's alone, so the decoy can be
    // planted and removed on a real host without touching anything of
    // the user's. `.env` itself is covered below, where nothing is
    // planted for exactly that reason.
    let dotfile = format!(".{}", unique("decoyrc"));
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.decoys.push(dotfile.clone());
    // Only reachable if the classification regresses: docker creates a
    // missing bind source, so a forwarded `-v .env:/x` leaves an empty
    // directory named `.env` in the server's home.
    cleanup.docker_made.push(".env".into());

    ws.write(&dotfile, "LOCAL-DOTFILE\n");
    ws.write(".env", "PROBE=LOCAL-ENV\n");
    plant_a_decoy(&server, &dotfile, "SERVER-DOTFILE-DECOY\n");

    let out = ws
        .ulak(&server)
        .args(["docker", "run", "--rm", "-v"])
        .arg(format!("{dotfile}:/probe/dotfile:ro"))
        .args(["-v", ".env:/probe/dotenv:ro"])
        .args([IMAGE, "cat", "/probe/dotfile", "/probe/dotenv"])
        .output()
        .unwrap();
    assert_run(
        &out,
        "run -v <dotfile>:/probe/dotfile -v .env:/probe/dotenv",
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "LOCAL-DOTFILE\nPROBE=LOCAL-ENV\n",
        "a dotfile bind source was read as a named volume and the command forwarded blind; \
         the server's own {dotfile} holds {:?}",
        decoy_says(&server, &dotfile)
    );
    assert_eq!(
        decoy_says(&server, &dotfile),
        "SERVER-DOTFILE-DECOY\n",
        "the sync wrote over the server's login directory instead of the workspace"
    );

    // The other half of the same claim, in the place the transport can
    // be seen rather than inferred: both dotfiles reached the workspace.
    // A named volume would have left the workspace without them, and
    // `.env` is the name that made this worth a test.
    for (name, body) in [
        (dotfile.as_str(), "LOCAL-DOTFILE\n"),
        (".env", "PROBE=LOCAL-ENV\n"),
    ] {
        assert_eq!(
            remote_read(&ws, &server, name),
            body,
            "{name} never travelled — it was taken for daemon state, not a path"
        );
    }

    ws.forget_on_server(&server);
}

/// `--mount` is the same claim through a different parser: a CSV record
/// whose `source=` field is the only part that moves. Splitting it the
/// obvious way rather than as a record is what made a path vanish from
/// the spec entirely — never synced, never respelled, never mentioned —
/// so the flag earns a scenario of its own rather than a unit test's
/// word that the two agree.
#[test]
fn a_mount_flag_bind_source_travels_like_a_dash_v_one() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = footprint_workspace(&server);
    let dir = unique("mounted");
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.decoys.push(dir.clone());

    ws.write(&format!("{dir}/marker.txt"), "LOCAL-MOUNT\n");
    plant_a_decoy(&server, &format!("{dir}/marker.txt"), "SERVER-DECOY\n");

    let out = ws
        .ulak(&server)
        .args(["docker", "run", "--rm", "--mount"])
        .arg(format!("type=bind,source=./{dir},target=/app,readonly"))
        .args([IMAGE, "cat", "/app/marker.txt"])
        .output()
        .unwrap();
    assert_run(&out, "run --mount type=bind,source=./<dir>");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "LOCAL-MOUNT\n",
        "`--mount type=bind` read the wrong machine; the server's copy holds {:?}",
        decoy_says(&server, &format!("{dir}/marker.txt"))
    );

    ws.forget_on_server(&server);
}

/// The one place a bare word means the opposite of what it means after
/// `-v`, so it is the one place a shared rule would be wrong in both
/// directions at once.
#[test]
fn an_env_file_is_read_here_and_its_values_reach_the_container() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = footprint_workspace(&server);
    // A bare word is a NAMED VOLUME after `-v` and a file in the cwd
    // after `--env-file`, so this is the one flag where the same
    // spelling has to be read the other way.
    let file = format!("{}.env", unique("probe"));
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.decoys.push(file.clone());

    ws.write(&file, "PROBE=LOCAL-ENV-FILE\n");
    plant_a_decoy(&server, &file, "PROBE=SERVER-DECOY\n");

    let out = ws
        .ulak(&server)
        .args(["docker", "run", "--rm", "--env-file", &file])
        .args([IMAGE, "printenv", "PROBE"])
        .output()
        .unwrap();
    assert_run(&out, "run --env-file <file>");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "LOCAL-ENV-FILE\n",
        "the env file was read on the server; its copy there holds {:?}",
        decoy_says(&server, &file)
    );

    ws.forget_on_server(&server);
}

/// A bind mount is two directions, and only one of them is the sync.
/// `docker run -v ./out:/out … build` that leaves its artefacts on the
/// server is a build the user cannot see, which is the same result as a
/// build that never ran.
#[test]
fn what_the_container_wrote_into_a_bind_mount_comes_home() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = footprint_workspace(&server);
    let dir = unique("out");

    // The directory has to exist here first. A bind source that is not
    // here yet earns no filter rule in either direction — deliberately,
    // upstream — so nothing would be pushed and nothing could be pulled.
    ws.write(&format!("{dir}/.keep"), "");

    let out = ws
        .ulak(&server)
        .args(["docker", "run", "--rm", "-v"])
        .arg(format!("./{dir}:/out"))
        .args([IMAGE, "sh", "-c", "echo MADE-ON-THE-SERVER > /out/made.txt"])
        .output()
        .unwrap();
    assert_run(&out, "run -v ./<dir>:/out");

    let landed = ws.project.join(format!("{dir}/made.txt"));
    let got = std::fs::read_to_string(&landed).unwrap_or_else(|e| {
        panic!(
            "what the container wrote never came home ({e}); {} holds {:?}",
            landed.display(),
            siblings(&landed)
        )
    });
    assert_eq!(got, "MADE-ON-THE-SERVER\n");

    ws.forget_on_server(&server);
}

/// The boundary from the other side: not everything that looks like a
/// path is ours, and carrying the ones that are not would be its own
/// kind of wrong machine.
#[test]
fn an_absolute_path_outside_the_workspace_stays_the_servers_own() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = footprint_workspace(&server);
    // `-v /var/run/docker.sock:/var/run/docker.sock` is the shape this
    // protects: an absolute path outside the workspace can only usefully
    // mean the server's own filesystem, and carrying it would be
    // nonsense. Said out loud rather than done silently, because a user
    // who meant their own machine has to be able to tell.
    let outside = format!("/tmp/{}", unique("server-owned"));
    let made = server.ssh(&format!(
        "set -e; mkdir -p {d}; printf 'SERVER-OWNED\\n' > {d}/marker.txt",
        d = q(&outside),
    ));
    assert!(
        made.status.success(),
        "could not make the server-side directory: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    struct ServerDir<'a>(&'a TestServer, String);
    impl Drop for ServerDir<'_> {
        fn drop(&mut self) {
            self.0.ssh(&format!("rm -rf {}", q(&self.1)));
        }
    }
    let _made = ServerDir(&server, outside.clone());

    let out = ws
        .ulak(&server)
        .args(["docker", "run", "--rm", "-v"])
        .arg(format!("{outside}:/srv:ro"))
        .args([IMAGE, "cat", "/srv/marker.txt"])
        .output()
        .unwrap();
    assert_run(&out, "run -v /abs/outside:/srv");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "SERVER-OWNED\n",
        "an absolute path outside the workspace must stay the server's own"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is resolved on the server, not here"),
        "the user has to be told which machine answered, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !std::path::Path::new(&outside).exists(),
        "{outside} was created on THIS machine, so the path was read as local after all"
    );

    ws.forget_on_server(&server);
}

/// The mirror image of every other scenario here, and the reason the
/// dotfile rule has to be a rule rather than a widening: a name that is
/// NOT a path must not become one. Turning `pgdata:/var/lib/postgresql`
/// into a synced directory would trade a database for an empty mount.
#[test]
fn a_named_volume_is_daemon_state_and_nothing_local() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = footprint_workspace(&server);
    let volume = unique("vol");
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.volumes.push(volume.clone());

    let out = ws
        .ulak(&server)
        .args(["docker", "run", "--rm", "-v"])
        .arg(format!("{volume}:/data"))
        .args([IMAGE, "sh", "-c", "echo IN-VOLUME > /data/v.txt"])
        .output()
        .unwrap();
    assert_run(&out, "run -v <volume>:/data");

    assert!(
        server
            .ssh(&format!("docker volume inspect {} >/dev/null", q(&volume)))
            .status
            .success(),
        "a bare source is a named volume on the remote daemon, and there is none called {volume}"
    );
    assert!(
        !ws.project.join(&volume).exists(),
        "a named volume became a directory in the project"
    );
    let listing = remote_listing(&ws, &server);
    assert!(
        !listing.lines().any(|l| l == volume),
        "a named volume became a directory in the remote workspace:\n{listing}"
    );

    ws.forget_on_server(&server);
}

// ─── docker create ──────────────────────────────────────────────────

/// `docker create` is the route's other half, and `--cidfile` is its one
/// OUTPUT: docker writes it on the server, so it has to be cleared there
/// first and fetched back after. Left alone, the user is told the file
/// was written and finds nothing where they were told to look.
#[test]
fn a_created_container_leaves_its_id_file_on_this_machine() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = footprint_workspace(&server);
    let name = unique("created");
    let dir = unique("ctx");
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.containers.push(name.clone());

    ws.write(&format!("{dir}/marker.txt"), "LOCAL-CREATE\n");

    let out = ws
        .ulak(&server)
        .args([
            "docker",
            "create",
            "--name",
            &name,
            "--cidfile",
            "cid.txt",
            "-v",
        ])
        .arg(format!("./{dir}:/app:ro"))
        .args([IMAGE, "cat", "/app/marker.txt"])
        .output()
        .unwrap();
    assert_run(&out, "create --cidfile cid.txt");

    let here = std::fs::read_to_string(ws.project.join("cid.txt")).unwrap_or_else(|e| {
        panic!(
            "the container id file never came home ({e}); the project holds {:?}",
            siblings(&ws.project.join("cid.txt"))
        )
    });
    let theirs = server.ssh(&format!(
        "docker container inspect --format '{{{{.Id}}}}' {}",
        q(&name)
    ));
    assert_eq!(
        here.trim(),
        String::from_utf8_lossy(&theirs.stdout).trim(),
        "the id file names a container the remote daemon does not have"
    );

    // `create` writes the container down and stops. A container that is
    // already running would mean `run` and `create` had been collapsed
    // into one, which is the sort of thing an exit-status test waves
    // through.
    let state = server.ssh(&format!(
        "docker container inspect --format '{{{{.State.Status}}}}' {}",
        q(&name)
    ));
    assert_eq!(
        String::from_utf8_lossy(&state.stdout).trim(),
        "created",
        "`docker create` started the container"
    );

    ws.forget_on_server(&server);
}

/// A `--cidfile` and nothing else: no bind, no context, nothing to
/// push. The whole prepare-the-workspace block was gated on there being
/// something to sync, so this shape reached `clear_remote_cidfile`'s
/// `mkdir -p` and `rm -f` with no lock taken and no manifest read —
/// `ensure_workspace`, which is the one place that proves the directory
/// is ours, only ran from inside the sync.
#[test]
fn a_run_that_only_names_a_cidfile_still_claims_the_workspace() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = footprint_workspace(&server);
    let name = unique("cidonly");
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.containers.push(name.clone());

    let out = ws
        .ulak(&server)
        .args(["docker", "create", "--name", &name, "--cidfile", "only.cid"])
        .args([IMAGE, "true"])
        .output()
        .unwrap();
    assert_run(&out, "create --cidfile with nothing to sync");

    let here = std::fs::read_to_string(ws.project.join("only.cid")).unwrap_or_else(|e| {
        panic!(
            "the container id file never came home ({e}); the project holds {:?}",
            siblings(&ws.project.join("only.cid"))
        )
    });
    let theirs = server.ssh(&format!(
        "docker container inspect --format '{{{{.Id}}}}' {}",
        q(&name)
    ));
    assert_eq!(here.trim(), String::from_utf8_lossy(&theirs.stdout).trim());

    // The manifest is what `ensure_workspace` writes and reads back
    // before anything is allowed to mutate the tree. Without it, the
    // `rm -f` above went into a directory nobody had checked belonged
    // to this checkout.
    let ids = ws.workspace_ids();
    assert!(
        !ids.is_empty(),
        "no workspace state to check, so the assertion below would pass on nothing"
    );
    for id in ids {
        let root = ws.remote_workspace_root(&id);
        let seen = server.ssh(&format!(
            "test -f {root}/manifest.json && echo yes || echo no"
        ));
        assert_eq!(
            String::from_utf8_lossy(&seen.stdout).trim(),
            "yes",
            "the workspace was written into without its manifest ever being read"
        );
    }

    ws.forget_on_server(&server);
}

/// Docker opens the id file before it creates the container and makes
/// no directory for it. Measured on 29.4.0: `docker run --cidfile
/// nosuchdir/id.cid alpine:3.20 true` answers "failed to create the
/// container ID file: open nosuchdir/id.cid: no such file or directory",
/// exits 127, and leaves no container behind.
///
/// Here the file is written on the SERVER, where `clear_remote_cidfile`
/// makes the parent with `mkdir -p` — so the missing directory stopped
/// nothing. The container ran, only the write home failed, and it failed
/// after the fact: reported to stderr while docker's own 0 went out as
/// the exit code. A run docker would have refused came back saying it
/// had worked, which is the one answer a script reads.
#[test]
fn a_cidfile_with_no_directory_here_refuses_before_a_container_exists() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = footprint_workspace(&server);
    let name = unique("nocidpath");
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.containers.push(name.clone());

    let out = ws
        .ulak(&server)
        .args(["docker", "run", "--rm", "--name", &name])
        .args(["--cidfile", "nosuchdir/id.cid", IMAGE, "true"])
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "the run reported success for a file it could not write:\n{said}"
    );
    assert!(
        said.contains("nosuchdir"),
        "the refusal has to name the directory:\n{said}"
    );

    let seen = server.ssh(&format!(
        "docker ps -a --filter name={} --format '{{{{.Names}}}}'",
        q(&name)
    ));
    assert!(
        String::from_utf8_lossy(&seen.stdout).trim().is_empty(),
        "a container docker would never have created is on the server"
    );

    ws.forget_on_server(&server);
}

// ─── the shapes that must NOT change ────────────────────────────────

/// The shape that must not have been slowed down or bent by any of the
/// above: `docker run --rm alpine ls` names nothing here and should cost
/// exactly one ssh — no workspace, no sync, no rewrite, and still the
/// remote daemon's answer with the remote daemon's exit code.
#[test]
fn a_run_that_names_nothing_local_still_lands_on_the_remote_daemon() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = footprint_workspace(&server);
    let name = unique("plain");
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.containers.push(name.clone());

    // `docker run --rm alpine ls` names nothing on this machine and must
    // cost exactly one ssh — no sync, no workspace, no rewrite. The
    // proof that it still went SOMEWHERE is a container the remote
    // daemon has and this machine does not.
    ws.ulak(&server)
        .args(["docker", "run", "--name", &name, IMAGE, "true"])
        .assert()
        .success();
    assert!(
        server
            .ssh(&format!("docker container inspect {} >/dev/null", q(&name)))
            .status
            .success(),
        "the remote daemon has no container called {name}"
    );
    assert!(
        !std::process::Command::new("docker")
            .args(["container", "inspect", &name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        "{name} was created on the LOCAL daemon, so the run never left this machine"
    );

    // The exit code is the whole answer for a script, and a route that
    // syncs, rewrites and pulls has three places to lose it.
    let out = ws
        .ulak(&server)
        .args(["docker", "run", "--rm", IMAGE, "sh", "-c", "exit 7"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(7),
        "`ulak docker run … 'exit 7'` must exit 7, got {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    ws.forget_on_server(&server);
}

// A bind source ABOVE the workspace is refused rather than guessed, and
// that scenario now lives in `e2e_tree.rs` beside the other refusals.
// It belongs there because `RunSpec::parse` raises it before anything
// opens a connection: here it took a share of the fixture and never
// sent a byte over it.

// ─── helpers ────────────────────────────────────────────────────────

/// A name no other scenario on this shared server can be using: the
/// suite's own prefix, the process id (two checkouts, one host) and a
/// counter (two scenarios in one process).
fn unique(what: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "ulak-fp-{what}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Fail with BOTH streams. A run that never reached the daemon comes
/// back with empty stdout, and the content assertion that follows would
/// then blame the bytes instead of the connection.
fn assert_run(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "`ulak docker {what}` failed with {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A file in this workspace as it exists ON THE SERVER — which separates
/// "the sync never carried it" from "the mount read the wrong thing".
fn remote_read(ws: &Workspace, server: &TestServer, rel: &str) -> String {
    let ids = ws.workspace_ids();
    let id = ids.first().expect("a workspace was registered");
    let root = ws.remote_workspace_root(id);
    let out = server.ssh(&format!("cat {root}/proj/{rel}"));
    assert!(
        out.status.success(),
        "{rel} is not in the remote workspace either; it holds:\n{}",
        remote_listing(ws, server)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn remote_listing(ws: &Workspace, server: &TestServer) -> String {
    let Some(id) = ws.workspace_ids().first().cloned() else {
        return "(no workspace was registered)".into();
    };
    let root = ws.remote_workspace_root(&id);
    let out = server.ssh(&format!("ls -a {root}/proj 2>&1"));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn siblings(path: &std::path::Path) -> Vec<String> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    std::fs::read_dir(parent)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// One level of shell quoting for the scripts the harness hands to the
/// server's own shell.
fn q(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}
