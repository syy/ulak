//! The explicit Docker tree without any Compose file: direct build owns
//! its context, while daemon-only commands need only the configured host.
//!
//! What used to stand here as well was an eighteen-command carousel —
//! volume create/inspect/ls/rm, network create/inspect/rm,
//! ps/inspect/stop/rm/rmi — proving over and over that argv reaches the
//! far side. `docker info` proves that in one round trip, and
//! `catalog.rs` pins every one of those routings statically, so the
//! carousel cost a third of this binary's runtime to re-state what a
//! unit test already owns. Its place is taken by the sequence nothing
//! anywhere covered: a build and a narrow `run` sharing one workspace,
//! and a locally deleted file that has to leave the server without being
//! carried home again.

mod common;

use common::{TestServer, Workspace};

struct RemoteImage<'a> {
    server: &'a TestServer,
    image: String,
}

impl Drop for RemoteImage<'_> {
    fn drop(&mut self) {
        self.server.ssh(&format!(
            "docker image rm -f {} >/dev/null 2>&1 || true",
            self.image
        ));
    }
}

/// A workspace with no Compose file in it, wired to the server through
/// `ulak init` — which is itself the first claim: a Compose-free project
/// is a workspace ulak accepts.
fn compose_free_workspace(server: &TestServer) -> Workspace {
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
    let init = ws
        .ulak(server)
        .args(["init", &server.alias])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "Compose-free init failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    ws
}

#[test]
fn a_compose_free_project_is_a_workspace_and_its_build_lands_remotely() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();

    // Before init there is no host, and the guidance has to say so
    // rather than stack-trace at a daemon nobody named.
    ws.ulak(&server)
        .args(["docker", "network", "ls"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("ulak init"));

    let init = ws
        .ulak(&server)
        .args(["init", &server.alias])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "Compose-free init failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(
        String::from_utf8_lossy(&init.stderr).contains("no Compose file found"),
        "init should explain the valid Compose-free workspace"
    );
    ws.ulak(&server)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicates::str::contains("Compose is optional"));

    // One forwarding proof, and the cheapest one there is: the answer
    // can only have come from a daemon, and it is not this machine's.
    let info = ws
        .ulak(&server)
        .args(["docker", "info", "--format", "{{.ServerVersion}}"])
        .output()
        .unwrap();
    assert!(
        info.status.success() && !info.stdout.is_empty(),
        "remote docker info failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&info.stdout),
        String::from_utf8_lossy(&info.stderr)
    );

    let image = format!("ulak-direct-build-{}:test", std::process::id());
    let _cleanup = RemoteImage {
        server: &server,
        image: image.clone(),
    };
    ws.write(
        "direct-build/Dockerfile",
        "FROM scratch\nCOPY marker /marker\n",
    );
    ws.write("direct-build/marker", "DIRECT-DOCKER-BUILD\n");

    let out = ws
        .ulak(&server)
        .args(["docker", "build", "-t", &image, "direct-build"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "remote docker build failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        server
            .ssh(&format!("docker image inspect {image} >/dev/null"))
            .status
            .success(),
        "the image was not built on the remote daemon"
    );
    // The other half of that claim: the build left this machine. The
    // fixture's daemon lives in its own container, so the image being
    // here would mean it never travelled.
    assert!(
        !std::process::Command::new("docker")
            .args(["image", "inspect", &image])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        "{image} was built on the LOCAL daemon, so the build never reached the server"
    );

    ws.forget_on_server(&server);
}

/// The sequence the ledger rewrite exists for, and the one no test drove
/// end to end: build the whole context, then run something that names a
/// SLIVER of it, then delete a file and build again.
///
/// Two silent failures live in that sequence, and only real hashing and
/// a real rsync can reach either. If the build and the run do not land
/// on the same workspace_id, the run's narrow footprint is a workspace
/// of its own and the build's tree is stranded on the server with
/// nothing left that can ever delete it. And if the narrow run's walk —
/// which sees `app/` and nothing else — is allowed to decide what is
/// doomed, everything outside `app/` is deleted from under a build that
/// is still using it. Both end with the user's files in the wrong state
/// and the command exiting 0.
///
/// The last leg is the other direction of the same hole: `pull_back`
/// runs with `--ignore-existing`, so a file the user has just deleted is
/// missing here and present there — exactly the shape rsync CREATES.
/// A deletion that comes back is a deletion that cannot be repeated,
/// because the next walk sees the file and claims it all over again.
#[test]
fn a_build_and_a_narrow_run_keep_one_ledger_over_one_workspace() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = compose_free_workspace(&server);

    let image = format!("ulak-ledger-{}:test", std::process::id());
    let _cleanup = RemoteImage {
        server: &server,
        image: image.clone(),
    };

    ws.write(
        "Dockerfile",
        "FROM alpine:3.20\nCOPY app/keep.txt /keep.txt\n",
    );
    ws.write("app/keep.txt", "APP-KEEP\n");
    // Outside the run's footprint on purpose: this is the file the two
    // commands have to agree about.
    ws.write("doomed.txt", "DOOMED\n");

    let out = ws
        .ulak(&server)
        .args(["docker", "build", "-t", &image, "."])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the first build failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let workspace = the_one_workspace(&ws);
    let remote_root = ws.remote_workspace_root(&workspace);
    assert!(
        on_the_server(&ws, &server, &workspace, "doomed.txt"),
        "the build context never reached the workspace, so nothing below can mean anything"
    );

    // What a container writes into a bind mount looks exactly like a
    // file the user deleted, and only the ledger tells them apart. This
    // one is written straight into the workspace so that nothing here
    // ever claimed it — it must outlive every deletion below, or the
    // "gone" assertions are just proof that ulak deletes everything.
    server.ssh(&format!(
        "printf 'MADE-ON-THE-SERVER\\n' > {remote_root}/proj/server-only.txt"
    ));

    // Written after the build and outside `app/`: if the run below is
    // really the narrow route, this never travels. Without that the
    // "doomed.txt survived the run" assertion would be satisfied by a
    // run that simply syncs the whole tree.
    ws.write("outside-the-run.txt", "NOT-THE-RUNS-BUSINESS\n");

    // A run that names a sliver of the same tree. `--rm` and a read-only
    // mount: what is under test is which workspace it lands in, not what
    // the container does.
    let out = ws
        .ulak(&server)
        .args(["docker", "run", "--rm", "-v", "./app:/app:ro", &image])
        .args(["cat", "/app/keep.txt"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the narrow run failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "APP-KEEP\n",
        "the container read the wrong machine's app/"
    );
    assert_eq!(
        ws.workspace_ids(),
        vec![workspace.clone()],
        "the build and the run took different workspaces, so neither one's ledger \
         can ever retire the other's files"
    );
    assert!(
        on_the_server(&ws, &server, &workspace, "doomed.txt"),
        "the run's narrow footprint deleted a file it had simply never heard of; \
         the build that is still using it exited 0 and said nothing"
    );
    assert!(
        !on_the_server(&ws, &server, &workspace, "outside-the-run.txt"),
        "`run -v ./app:/app` carried the whole tree, so it is not the narrow route \
         this scenario is about and the assertion above proves nothing"
    );

    // The deletion, made the way a user makes one.
    std::fs::remove_file(ws.project.join("doomed.txt")).unwrap();
    let out = ws
        .ulak(&server)
        .args(["docker", "build", "-t", &image, "."])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the second build failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !on_the_server(&ws, &server, &workspace, "doomed.txt"),
        "a file the user deleted is still on the server; the workspace now holds:\n{}",
        String::from_utf8_lossy(&server.ssh(&format!("ls -a {remote_root}/proj")).stdout)
    );
    assert!(
        !ws.project.join("doomed.txt").exists(),
        "the deleted file came home again on the pull, which makes the deletion \
         unrepeatable: the next walk claims it all over again"
    );
    assert!(
        on_the_server(&ws, &server, &workspace, "server-only.txt"),
        "a file ulak never put there was deleted along with the one it did — the \
         ledger is not deciding this, the walk is"
    );

    // And once more through the narrow route, because that is the leg
    // that pulls with `--ignore-existing` and the one where a
    // resurrection would land.
    ws.ulak(&server)
        .args(["docker", "run", "--rm", "-v", "./app:/app:ro", &image])
        .args(["cat", "/app/keep.txt"])
        .assert()
        .success();
    assert!(
        !ws.project.join("doomed.txt").exists(),
        "the run's pull brought the deleted file back"
    );

    ws.forget_on_server(&server);
}

/// The same workspace, driven by two commands that anchor differently.
///
/// `build_anchor` climbs to the common ancestor of the workspace root,
/// the context and the Dockerfile, so a Dockerfile kept beside the
/// project — the shape every monorepo has — anchors the build ABOVE the
/// root. `run`, `create` and `bake` always anchor AT the root. Nothing
/// separates the two: `into_project` leaves `compose_files` empty, so
/// both hash the root and land on one workspace id, one remote
/// directory, one manifest and one ledger.
///
/// `ensure_workspace` then reads the manifest, sees an anchor that is
/// not the one it is holding, and answers the only way it knows: `rm
/// -rf` the workspace and forget the ledger. The next command changes it
/// back. Two commands a developer alternates all day therefore destroy
/// and re-push each other's tree forever — and each wipe takes the
/// container's own output and every `protect` path with it, because the
/// `rm -rf` is aimed at the workspace directory rather than at anything
/// the ledger claims.
///
/// The last leg is what makes this more than churn: with no terminal to
/// confirm in, `relayout` does not wipe — it fails. So in CI, or with
/// output piped anywhere, the second command simply stops working.
#[test]
fn a_build_anchored_outside_the_root_does_not_wipe_the_run_that_follows_it() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = compose_free_workspace(&server);

    let image = format!("ulak-anchor-{}:test", std::process::id());
    let _cleanup = RemoteImage {
        server: &server,
        image: image.clone(),
    };

    // A Dockerfile shared between sibling projects — outside this one,
    // which is the whole point: it is what pulls the anchor up.
    let shared = ws.project.parent().expect("a parent").join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(
        shared.join("Dockerfile"),
        "FROM alpine:3.20\nCOPY app/keep.txt /keep.txt\n",
    )
    .unwrap();
    ws.write("app/keep.txt", "APP-KEEP\n");
    // Outside the run's footprint, so it can only survive if the run
    // leaves the build's tree alone.
    ws.write("build-only.txt", "BUILD-ONLY\n");

    let out = ws
        .ulak(&server)
        .args(["docker", "build", "-f", "../shared/Dockerfile"])
        .args(["-t", &image, "."])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the build with an outside Dockerfile failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let workspace = the_one_workspace(&ws);
    let remote_root = ws.remote_workspace_root(&workspace);

    // The narrow run, with no terminal anywhere — the shape a script or
    // a CI job has, and the one where `relayout` cannot ask.
    let out = ws
        .ulak(&server)
        .args(["docker", "run", "--rm", "-v", "./app:/app:ro", &image])
        .args(["cat", "/app/keep.txt"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the run after a build anchored outside the root failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "APP-KEEP\n",
        "the container read the wrong machine's app/"
    );

    // And the build's own tree is still there: the run must not have
    // been given a reason to empty it. Asked without naming a layout —
    // the anchor decides how deep the file sits, and what is under test
    // is whether it SURVIVES, not where it landed.
    assert!(
        anywhere_on_the_server(&ws, &server, &workspace, "build-only.txt"),
        "the run wiped the workspace the build had filled; it now holds:\n{}",
        String::from_utf8_lossy(&server.ssh(&format!("ls -aR {remote_root}")).stdout)
    );

    // Back the other way, because a relayout that fires on every
    // alternation costs a full re-push each time even when it succeeds.
    ws.ulak(&server)
        .args(["docker", "build", "-f", "../shared/Dockerfile"])
        .args(["-t", &image, "."])
        .assert()
        .success();
    assert!(
        anywhere_on_the_server(&ws, &server, &workspace, "build-only.txt"),
        "the second build wiped the workspace again"
    );

    ws.forget_on_server(&server);
}

/// The other way a command can meet an anchor it did not expect: the
/// server holds a WIDER one than this invocation worked out for itself.
///
/// Two roads lead here and they differ only in how the local record went
/// missing. A state directory that was cleaned never had one to widen
/// against; and two commands started together both read the record
/// before either takes the workspace lock, so the one that loses the
/// race carries the anchor from before the winner settled a wider one.
/// Removing the record is the second case made deterministic.
///
/// What must NOT happen either way is the relayout: it empties the whole
/// remote workspace — `protect` included, which lives nowhere else — to
/// rebuild a layout that was already correct.
#[test]
fn a_workspace_the_server_anchored_wider_is_written_down_rather_than_wiped() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = compose_free_workspace(&server);

    let image = format!("ulak-anchor-stale-{}:test", std::process::id());
    let _cleanup = RemoteImage {
        server: &server,
        image: image.clone(),
    };

    let shared = ws.project.parent().expect("a parent").join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(
        shared.join("Dockerfile"),
        "FROM alpine:3.20\nCOPY app/keep.txt /keep.txt\n",
    )
    .unwrap();
    ws.write("app/keep.txt", "APP-KEEP\n");
    // Outside the run's footprint, so it survives only if nothing was
    // re-laid out.
    ws.write("build-only.txt", "BUILD-ONLY\n");

    ws.ulak(&server)
        .args(["docker", "build", "-f", "../shared/Dockerfile"])
        .args(["-t", &image, "."])
        .assert()
        .success();
    let workspace = the_one_workspace(&ws);
    let remote_root = ws.remote_workspace_root(&workspace);
    let anchor_record = ws.workspaces_dir().join(&workspace).join("anchor");
    assert!(
        anchor_record.is_file(),
        "the build must have written the base down for this to be about losing it"
    );
    std::fs::remove_file(&anchor_record).unwrap();

    // No terminal anywhere, which is the shape a script or a CI job has
    // — and the shape in which `relayout` cannot even ask.
    let run = |ws: &common::Workspace| {
        ws.ulak(&server)
            .args(["docker", "run", "--rm", "-v", "./app:/app:ro", &image])
            .args(["cat", "/app/keep.txt"])
            .output()
            .unwrap()
    };

    let out = run(&ws);
    assert!(
        !out.status.success(),
        "a command whose paths are computed against the wrong base must not proceed"
    );
    let said = String::from_utf8_lossy(&out.stderr);
    let flat = said.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("run the same command again"),
        "the refusal has to name the way out, since there is nothing wrong to repair:\n{said}"
    );
    assert!(
        anywhere_on_the_server(&ws, &server, &workspace, "build-only.txt"),
        "the workspace was emptied for a layout that was already right; it now holds:\n{}",
        String::from_utf8_lossy(&server.ssh(&format!("ls -aR {remote_root}")).stdout)
    );

    // And it repairs itself: the base is written down now, so the same
    // command widens to it and agrees.
    let out = run(&ws);
    assert!(
        out.status.success(),
        "the second run still disagreed with the server:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "APP-KEEP\n",
        "the container read the wrong machine's app/"
    );
    assert!(
        anywhere_on_the_server(&ws, &server, &workspace, "build-only.txt"),
        "the run that succeeded emptied the workspace instead"
    );

    ws.forget_on_server(&server);
}

/// The workspace this project registered, and the assertion that there
/// is exactly one of them: "the build and the run agree" is a claim
/// about a count before it is a claim about a name.
fn the_one_workspace(ws: &Workspace) -> String {
    let ids = ws.workspace_ids();
    assert_eq!(
        ids.len(),
        1,
        "one project directory must be one workspace, and this machine recorded {ids:?}"
    );
    ids.into_iter().next().expect("a workspace was registered")
}

fn on_the_server(ws: &Workspace, server: &TestServer, workspace: &str, rel: &str) -> bool {
    let root = ws.remote_workspace_root(workspace);
    server
        .ssh(&format!("test -e {root}/proj/{rel}"))
        .status
        .success()
}

/// The same question with the layout left out: is this file anywhere
/// under the workspace? An anchor above the project root nests the tree
/// one level deeper, so a scenario about anchors CHANGING cannot spell
/// the path it expects without assuming the answer.
fn anywhere_on_the_server(
    ws: &Workspace,
    server: &TestServer,
    workspace: &str,
    name: &str,
) -> bool {
    let root = ws.remote_workspace_root(workspace);
    server
        .ssh(&format!("find {root} -name {name} | grep -q ."))
        .status
        .success()
}
