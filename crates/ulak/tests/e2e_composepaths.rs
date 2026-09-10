//! The local paths a COMPOSE SUBCOMMAND names, end to end.
//!
//! `Route::Compose` syncs the compose MODEL, which is why forwarding
//! argv verbatim is right for thirty of Compose's subcommands. It is
//! wrong for the handful that name a path themselves — `run -v`,
//! `cp`, `build --ssh`, `config -o` — because no model mentions those
//! paths, so nothing carries them.
//!
//! `composepaths.rs` reads them and `passthrough::carry` puts them in
//! the footprint. Both were covered by unit tests that stop at the
//! returned `Vec<LocalPath>` and the rewritten argv: nothing anywhere
//! asserted that the bytes reach the server, that the container sees
//! THIS machine's file, or that the respelled argv still points at what
//! was synced. That whole span — scan → footprint → rsync → respelled
//! argv → remote compose — is what runs here.
//!
//! The failure it exists for is the silent one the module doc names:
//! `compose run -v ./data:/data` mounted an empty directory the server's
//! docker had just created, with no error and no output.

mod common;

use common::{TestServer, Workspace};

/// A compose project whose service does nothing but stay alive, so a
/// `run` can be given any command and a `cp` has somewhere to land.
fn project(server: &TestServer) -> Workspace {
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.write(
        "compose.yaml",
        "services:\n  box:\n    image: alpine:3.20\n    \
         command: [\"sh\", \"-c\", \"sleep 600\"]\n",
    );
    ws.set_host(&server.alias);
    ws
}

/// The headline: a bind source named by `run -v` reaches the container
/// with THIS machine's bytes in it.
///
/// The second half is what makes the first mean something. A compose
/// global Ulak does not own (`--progress`) is re-emitted at the FRONT of
/// `Invocation::args`, ahead of the subcommand — and the scan used to
/// read `args[0]`, so any one of them turned `run` into "one of the
/// other thirty": no path found, nothing synced, and the command still
/// ran against a directory the server's docker created on the spot.
#[test]
fn a_bind_source_named_by_compose_run_carries_this_machines_bytes() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    // A WORKSPACE OF ITS OWN per spelling, which is the whole
    // difference between this test and one that cannot fail. Run in one
    // workspace, the plain spelling syncs `./data` first and leaves it on
    // the server; the `--progress` spelling then finds it already there
    // and passes however badly it read its own argv. Measured — that is
    // exactly what this test did before it was split, and it stayed
    // green with the bug reintroduced by hand.
    for globals in [vec![], vec!["--progress", "plain"]] {
        let ws = project(&server);
        ws.write("data/marker.txt", "FROM-THIS-MACHINE");

        let out = ws
            .ulak(&server)
            .args(["docker", "compose"])
            .args(&globals)
            .args(["run", "--rm", "-v", "./data:/data:ro", "box"])
            .args(["cat", "/data/marker.txt"])
            .output()
            .unwrap();
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.status.success(),
            "`compose {globals:?} run -v` failed:\n{said}"
        );
        assert!(
            said.contains("FROM-THIS-MACHINE"),
            "the container was given a directory the SERVER made, not ours \
             (globals: {globals:?}):\n{said}"
        );
        ws.forget_on_server(&server);
    }
}

/// A repeated `-o` names ONE file, the last one — and the file it
/// overruled has to still be sitting there afterwards.
///
/// Measured on Compose v5.1.2: `compose config -o first.yml -o
/// second.yml` exits 0 having written second.yml, and first.yml is never
/// created. Reading the FIRST occurrence instead left `-o second.yml` in
/// the argv the server ran, so compose rendered the model to second.yml
/// on the SERVER, remote stdout carried nothing, and `bridge::land`
/// renamed that nothing over the user's own first.yml — with ssh exiting
/// 0 through all of it. `bridge::take_flag` had already been measured
/// into shape for the same bug on `docker save`; this route never got it.
#[test]
fn a_repeated_output_flag_writes_the_last_file_and_spares_the_first() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = project(&server);
    ws.write("first.yml", "MINE-AND-STILL-HERE\n");

    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "config", "-o", "first.yml"])
        .args(["-o", "second.yml"])
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "`compose config -o -o` failed:\n{said}"
    );

    let first = std::fs::read_to_string(ws.project.join("first.yml")).unwrap();
    assert_eq!(
        first, "MINE-AND-STILL-HERE\n",
        "the overruled -o was renamed over the user's own file:\n{said}"
    );
    let second = std::fs::read_to_string(ws.project.join("second.yml"))
        .expect("the last -o names the file the answer goes in");
    assert!(
        second.contains("alpine:3.20"),
        "the last -o got no model, so the stream went somewhere else:\n{second}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "the model was printed as well as written, so a -o survived:\n{said}"
    );
    ws.forget_on_server(&server);
}

/// `compose cp` local → service. The local end is read HERE, and the
/// path it is respelled to on the far side has to be the place the sync
/// actually put it.
#[test]
fn compose_cp_reads_the_local_end_on_this_machine() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = project(&server);
    ws.write("seed/payload.txt", "SEED-BODY-7");

    let up = ws
        .ulak(&server)
        .args(["docker", "compose", "up", "-d"])
        .output()
        .unwrap();
    assert!(
        up.status.success(),
        "up failed:\n{}{}",
        String::from_utf8_lossy(&up.stdout),
        String::from_utf8_lossy(&up.stderr)
    );

    let out = ws
        .ulak(&server)
        .args([
            "docker",
            "compose",
            "cp",
            "./seed/payload.txt",
            "box:/tmp/payload.txt",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "compose cp failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let seen = ws
        .ulak(&server)
        .args(["docker", "compose", "exec", "-T", "box"])
        .args(["cat", "/tmp/payload.txt"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&seen.stdout).trim(),
        "SEED-BODY-7",
        "the file that arrived is not the one on this machine:\n{}",
        String::from_utf8_lossy(&seen.stderr)
    );

    ws.ulak(&server)
        .args(["docker", "compose", "down", "--remove-orphans"])
        .output()
        .ok();
    ws.forget_on_server(&server);
}

/// A path the subcommand named survives a build that ignores it.
///
/// `.dockerignore` speaks for a BUILD, and a `-v` bind source is not
/// one. `carry` used to push the entry without PINNING it, so a project
/// building from `.` with the commonplace `*.txt`-style line lost the
/// very file the mount pointed at — and the container was shown nothing,
/// which is the same silent shape as the scenario above.
#[test]
fn a_dockerignore_does_not_narrow_what_a_mount_points_at() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = project(&server);
    // The service must BUILD, not just pull: a `.dockerignore` only ever
    // speaks through a build CONTEXT, so a fixture whose service is a
    // bare `image:` leaves the ignore file with nothing to narrow and
    // the test passes however the pinning goes. Measured — that is what
    // this test did before the fixture was changed, and it stayed green
    // with the pin removed by hand.
    ws.write(
        "compose.yaml",
        "services:\n  box:\n    build: .\n    \
         command: [\"sh\", \"-c\", \"sleep 600\"]\n",
    );
    ws.write("Dockerfile", "FROM alpine:3.20\n");
    ws.write("data/marker.txt", "PINNED-THROUGH");
    // Excludes exactly what the mount below names.
    ws.write(".dockerignore", "data\n");

    let out = ws
        .ulak(&server)
        .args([
            "docker",
            "compose",
            "run",
            "--rm",
            "-v",
            "./data:/data:ro",
            "box",
        ])
        .args(["cat", "/data/marker.txt"])
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the run failed:\n{said}");
    assert!(
        said.contains("PINNED-THROUGH"),
        "a build's ignore file swallowed the file a mount pointed at:\n{said}"
    );

    ws.forget_on_server(&server);
}
