//! The compose flags real projects actually type, end to end.
//!
//! Every one of these is forwarded mechanically, and that is exactly why
//! they were untested: there is no ulak code to point at. What these
//! scenarios protect is that the forwarding STAYS mechanical — a flag
//! silently dropped, reordered or answered locally looks like nothing at
//! all from this machine, which is this product's whole failure class.
//!
//! The regressions name the concrete shapes they were measured in. The
//! original three were:
//!
//!   * a wrapper that only ever spells `-f`, because the project is
//!     named in the file
//!     (`a_wrapper_that_only_ever_names_files_still_addresses_one_stack`);
//!   * `up --wait`, which is DEFINED to exit nonzero on a partial start
//!     (`an_up_that_only_half_succeeded_keeps_its_stack_live`);
//!   * a container writing into a bind mount that is not here yet
//!     (`a_bind_source_that_is_not_here_is_named_before_the_command_runs`).
//!
//! Every scenario names its server-side resources after itself: these
//! share one server with every other suite.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{Service, TestServer, Workspace, desired, desired_for};

/// A service that does nothing but stay alive, and asks the server for
/// as little as possible while doing it.
///
/// `network_mode: "none"` is the frugal half and it is load-bearing on a
/// SHARED server: compose creates a `<project>_default` bridge per
/// project unless no service wants one, and docker's predefined address
/// pools are finite. Measured — a full-workspace run against a real host
/// died on `all predefined address pools have been fully subnetted`,
/// with nineteen `<project>_default` networks left behind by earlier
/// runs. Nothing in this suite is about networking, so nothing here
/// spends a subnet on it. The same reasoning as `needs_image`: a suite
/// whose colour depends on a shared resource it did not have to consume
/// is a suite people learn to re-run instead of read.
const IDLE: &str =
    "image: alpine:3.20\n    network_mode: \"none\"\n    command: [\"sleep\", \"600\"]\n";

/// A server-side name no other scenario can collide with. The pid alone
/// is not enough — these run in parallel inside ONE process.
fn scenario_project(what: &str) -> String {
    format!("ulak-e2e-{what}-{}", std::process::id())
}

/// Container ids compose has for this project, running or not.
fn containers(server: &TestServer, project: &str) -> Vec<String> {
    ids(
        server,
        &format!("docker ps -aq --filter label=com.docker.compose.project={project}"),
    )
}

fn container_labels(server: &TestServer, project: &str) -> String {
    let ids = containers(server, project).join(" ");
    let out = server.ssh(&format!(
        "docker inspect --format '{{{{json .Config.Labels}}}}' {ids}"
    ));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The same, narrowed to one service — which is how `--no-deps` is
/// checked without parsing a table.
fn containers_of(server: &TestServer, project: &str, service: &str) -> Vec<String> {
    ids(
        server,
        &format!(
            "docker ps -aq --filter label=com.docker.compose.project={project} \
         --filter label=com.docker.compose.service={service}"
        ),
    )
}

fn ids(server: &TestServer, cmd: &str) -> Vec<String> {
    let out = server.ssh(cmd);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn said(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn second_checkout(ws: &Workspace, server: &TestServer, name: &str, compose: &str) -> PathBuf {
    let checkout = ws.project.parent().unwrap().join(name);
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::write(
        checkout.join("ulak.local.toml"),
        format!("host = {:?}\n", server.alias),
    )
    .unwrap();
    std::fs::write(checkout.join("compose.yaml"), compose).unwrap();
    checkout
}

/// `down` the stack and take its workspace off the server. Runs on the
/// failure path too, so nothing here may panic.
///
/// `--profile '*'` is not decoration, and it was measured here before it
/// was written: a plain `down` leaves every profile-GATED container
/// running, prints nothing and exits 0 — the caveat `compose.rs`'s
/// `remote_prefix` already records, met from the other side. The first
/// run of this suite left two containers on the shared server for
/// exactly that reason. It is also the reset line real projects write,
/// which is how the caveat gets found in the first place.
fn tear_down(server: &TestServer, ws: &Workspace, project: &str) {
    let _ = ws
        .ulak(server)
        .args(["docker", "compose", "-p", project, "--profile", "*"])
        .args(["down", "-v", "--remove-orphans"])
        .output();
    ws.forget_on_server(server);
}

// ─── regressions ───────────────────────────────────────────────────

/// The implicit `.env` is absent when the first footprint cache entry is
/// written, so there is no file stamp that can notice its later creation.
/// Both appearance and removal must force a fresh server-side Compose model;
/// otherwise the stale cached project name steers the next command at the
/// wrong Docker stack.
#[test]
fn creating_or_removing_implicit_dot_env_invalidates_the_footprint_cache() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = Workspace::new();
    ws.set_host(&server.alias);

    let read_name = || {
        let out = ws
            .ulak(&server)
            .env_remove("COMPOSE_PROJECT_NAME")
            .args(["docker", "compose", "config", "--format", "json"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "compose model failed:\n{}",
            said(&out)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["name"]
            .as_str()
            .unwrap()
            .to_string()
    };

    assert_eq!(read_name(), "proj", "the cache precondition is no .env");
    let from_env = scenario_project("implicit-env-cache");
    ws.write(".env", &format!("COMPOSE_PROJECT_NAME={from_env}\n"));
    assert_eq!(
        read_name(),
        from_env,
        "the missing-file cache entry survived `.env` creation"
    );

    std::fs::remove_file(ws.project.join(".env")).unwrap();
    assert_eq!(
        read_name(),
        "proj",
        "the `.env` model survived removal of the file"
    );
    ws.forget_on_server(&server);
}

/// A wrapper script spells its `-f` list on every line and never spells
/// `-p` at all:
///
///     compose -f base.yaml -f extra.yaml up -d
///     compose -f base.yaml --profile '*' down --remove-orphans
///
/// Both lines must address one stack, and under docker they do — because
/// the project is named in the FILE, with a top-level `name:`. That is
/// docker's rung 4, and ulak walked straight past it: nothing read the
/// YAML, and every call carried a `-p <dir>-<12 hex>` of ulak's own
/// making that overruled whatever the file said. So a project that had
/// named itself was renamed behind its back, and the two lines above
/// could only agree by accident.
///
/// Nothing local computes this name — it can only have come off the
/// server's own `docker compose config`, which is the point.
#[test]
fn a_wrapper_that_only_ever_names_files_still_addresses_one_stack() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
    ws.set_host(&server.alias);
    let project = scenario_project("wrapper");
    ws.write(
        "base.yaml",
        &format!("name: {project}\nservices:\n  api:\n    {IDLE}"),
    );
    ws.write(
        "extra.yaml",
        "services:\n  api:\n    environment:\n      EXTRA: \"1\"\n",
    );

    let up = ws
        .ulak(&server)
        .args(["docker", "compose"])
        .args(["-f", "base.yaml", "-f", "extra.yaml", "up", "-d"])
        .output()
        .unwrap();
    assert!(up.status.success(), "up failed:\n{}", said(&up));
    assert!(
        !containers(&server, &project).is_empty(),
        "the compose file named this project and something overruled it:\n{}",
        said(&up)
    );

    // The reset line: one `-f`, still no `-p`. Measured on Compose
    // v5.3.1, a file that declares no name does not clear one, so
    // dropping `extra.yaml` keeps `base.yaml`'s.
    let down = ws
        .ulak(&server)
        .args(["docker", "compose", "-f", "base.yaml", "down"])
        .output()
        .unwrap();
    assert!(down.status.success(), "down failed:\n{}", said(&down));
    assert!(
        containers(&server, &project).is_empty(),
        "down reported success while the containers kept running — it addressed \
         a project nobody had started:\n{}",
        said(&down)
    );

    // And the other half of being a drop-in: Docker does not remember
    // the `-f` list either. There is deliberately no compose.yaml in this
    // directory, so a bare command must fail instead of silently reviving
    // the stack from Ulak's last invocation.
    let bare = ws
        .ulak(&server)
        .args(["docker", "compose", "up", "-d"])
        .output()
        .unwrap();
    assert!(
        !bare.status.success(),
        "a bare up reused the earlier -f list even though plain Docker would find no compose file:\n{}",
        said(&bare)
    );
    assert!(
        containers(&server, &project).is_empty(),
        "a hidden -f memory revived {project}:\n{}",
        said(&bare)
    );

    tear_down(&server, &ws, &project);
}

/// The other half of that faithfulness, and the one a memory would
/// break: a `-p` is NOT remembered for the next command.
///
/// Measured on Compose v5.3.1 with no ulak involved: `compose -p chosen
/// up -d` followed by a plain `compose up -d` in the same directory
/// starts a SECOND stack named after the directory, and the `down` after
/// it removes only that one while `chosen-web-1` keeps running. Docker
/// resolves the name from scratch every time. Anything else is ulak
/// addressing containers docker would have left alone, which is this
/// product's own stated failure class pointed the other way.
#[test]
fn a_p_is_not_remembered_for_the_command_after_it() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let project = scenario_project("noremember");
    ws.write("compose.yaml", &format!("services:\n  api:\n    {IDLE}"));

    ws.ulak(&server)
        .args(["docker", "compose", "-p", &project, "up", "-d"])
        .assert()
        .success();
    assert!(!containers(&server, &project).is_empty());

    let down = ws
        .ulak(&server)
        .args(["docker", "compose", "down"])
        .output()
        .unwrap();
    assert!(down.status.success(), "bare down failed:\n{}", said(&down));
    assert!(
        !containers(&server, &project).is_empty(),
        "a bare `down` stopped a project it was never told the name of — ulak has \
         started remembering a -p again:\n{}",
        said(&down)
    );
    let chosen = desired_for(&ws, &project).expect("the explicit stack keeps its own intent");
    assert_eq!(
        chosen["live"], true,
        "bare down retired the service and tunnels for an explicit stack Docker left running: {chosen}"
    );

    tear_down(&server, &ws, &project);
}

/// Docker allows two Compose project names from one checkout at once.
/// Their files are one sync workspace, but their lifecycle, service
/// status and tunnels are two independent things. The service used to
/// make that last distinction impossible: retiring either logical stack
/// unsubscribed notify's path-wide watcher and silently blinded the one
/// that survived. A compose edit must still cross inside the idle
/// reconcile interval after the first stack goes down.
#[test]
fn two_p_names_from_one_checkout_keep_independent_lifecycles() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let first = scenario_project("first-name");
    let second = scenario_project("second-name");
    ws.write("compose.yaml", &format!("services:\n  api:\n    {IDLE}"));

    for project in [&first, &second] {
        ws.ulak(&server)
            .args(["docker", "compose", "-p", project, "up", "-d"])
            .assert()
            .success();
        assert!(
            !containers(&server, project).is_empty(),
            "Docker did not create {project}"
        );
    }
    assert_eq!(desired_for(&ws, &first).unwrap()["live"], true);
    assert_eq!(desired_for(&ws, &second).unwrap()["live"], true);

    let service = Service::start(&ws, &server);
    for project in [&first, &second] {
        service.wait_status_for(&ws, project, 90, "its first reconcile", |st| {
            st["connection"] == "up" && st["last_sync_unix"].as_u64().is_some_and(|stamp| stamp > 0)
        });
    }

    ws.ulak(&server)
        .args(["docker", "compose", "-p", &first, "down"])
        .assert()
        .success();
    assert!(containers(&server, &first).is_empty());
    assert!(
        !containers(&server, &second).is_empty(),
        "down for one -p name stopped the other"
    );
    assert_eq!(desired_for(&ws, &first).unwrap()["live"], false);
    assert_eq!(
        desired_for(&ws, &second).unwrap()["live"],
        true,
        "retiring one stack retired the other stack's service and tunnels"
    );

    // Catalog retirement runs every five seconds. Once it has removed
    // the first logical watcher, the second must still own the shared
    // physical subscription.
    std::thread::sleep(Duration::from_secs(7));
    let changed =
        format!("services:\n  api:\n    {IDLE}# the surviving stack still watches this file\n");
    ws.write("compose.yaml", &changed);
    let workspace = ws
        .workspace_ids()
        .into_iter()
        .next()
        .expect("up synced one workspace");
    let remote_path = format!("{}/proj/compose.yaml", ws.remote_workspace_root(&workspace));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let remote = server.ssh(&format!("cat {remote_path}"));
        if remote.stdout == changed.as_bytes() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "retiring {first} blinded {second}'s shared file watcher; remote bytes: {:?}\nservice:\n{}",
            String::from_utf8_lossy(&remote.stdout),
            service.said()
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    drop(service);
    tear_down(&server, &ws, &second);
}

/// `clean` removes the shared sync workspace, not merely the currently
/// addressed Docker project. An explicit stack may still use those bytes
/// even though a bare identity has no containers, so every live intent
/// over the workspace must veto deletion. Docker's labels are the
/// independent answer after local state is reset, including for a
/// stopped container that can be started over the same bind mounts.
#[test]
fn clean_refuses_while_an_explicit_p_stack_uses_the_workspace() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let project = scenario_project("clean-explicit");
    let compose = format!("services:\n  api:\n    {IDLE}");
    ws.write("compose.yaml", &compose);

    ws.ulak(&server)
        .args(["docker", "compose", "-p", &project, "up", "-d"])
        .assert()
        .success();
    let out = ws.ulak(&server).arg("clean").output().unwrap();
    assert!(
        !out.status.success(),
        "clean removed a workspace still used by {project}:\n{}",
        said(&out)
    );
    assert!(
        said(&out).contains("still used") && said(&out).contains(&project),
        "the refusal did not name the live stack and the way out:\n{}",
        said(&out)
    );
    assert!(
        !containers(&server, &project).is_empty(),
        "clean implicitly tore the stack down"
    );
    let workspace = ws
        .workspace_ids()
        .into_iter()
        .next()
        .expect("up synced one workspace");
    let remote_root = ws.remote_workspace_root(&workspace);
    let remote = server.ssh(&format!("cat {remote_root}/proj/compose.yaml"));
    assert_eq!(
        String::from_utf8_lossy(&remote.stdout),
        compose,
        "clean deleted or changed the bytes the explicit stack still uses"
    );

    // Simulate the exact case a separate server-side Ulak ledger was
    // proposed for: local lifecycle state is gone, while Docker still
    // owns the authoritative paths in its Compose labels. Stop rather
    // than down so the container remains able to start over those bytes.
    std::fs::remove_dir_all(ws.stacks_dir()).unwrap();
    ws.ulak(&server)
        .args(["docker", "compose", "-p", &project, "stop"])
        .assert()
        .success();
    assert!(
        !containers(&server, &project).is_empty(),
        "the stopped container disappeared, so Docker's fallback guard was not exercised"
    );
    let without_local_state = ws.ulak(&server).arg("clean").output().unwrap();
    assert!(
        !without_local_state.status.success(),
        "clean trusted missing local state over Docker's own labels:\n{}",
        said(&without_local_state)
    );
    assert!(
        said(&without_local_state).contains("existing Docker stack(s)"),
        "the refusal did not come from Docker's independent ownership check:\n{}",
        said(&without_local_state)
    );
    let remote = server.ssh(&format!("cat {remote_root}/proj/compose.yaml"));
    assert_eq!(
        String::from_utf8_lossy(&remote.stdout),
        compose,
        "clean deleted the stopped stack's bind source after local state was reset"
    );

    tear_down(&server, &ws, &project);
}

/// `clean` owns an already-transported directory, not the current
/// Compose model. Re-resolving that model first made a syntax error a
/// permanent doorstop: the stack could be down and the workspace safe to
/// remove, yet cleanup failed before it reached either ownership check.
#[test]
fn clean_can_remove_an_idle_workspace_after_the_compose_file_breaks() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws.write(
        "compose.yaml",
        "services:\n  idle:\n    image: alpine:3.20\n    network_mode: \"none\"\n",
    );
    ws.ulak(&server).arg("sync").assert().success();
    let workspace = ws
        .workspace_ids()
        .into_iter()
        .next()
        .expect("sync created one workspace");
    let remote_root = ws.remote_workspace_root(&workspace);

    ws.write("compose.yaml", "services: [this is not valid compose\n");
    let cleaned = ws.ulak(&server).arg("clean").output().unwrap();
    let removed = server
        .ssh(&format!("test ! -e {remote_root}"))
        .status
        .success();
    // Failure-path hygiene without making the assertion green: record
    // the answer first, then remove whatever the scenario left behind.
    ws.forget_on_server(&server);
    assert!(
        cleaned.status.success(),
        "clean tried to resolve the broken model before checking ownership:\n{}",
        said(&cleaned)
    );
    assert!(
        removed,
        "clean reported success but left the idle workspace behind"
    );
}

/// Docker deliberately treats the same `-p` on one daemon as one stack,
/// even when two checkouts address it. Once checkout B has recreated
/// that stack over B's workspace, cleaning the unused workspace from A
/// must not run a project-wide `down` and destroy B's containers.
#[test]
fn cleaning_an_old_checkout_does_not_down_the_same_project_from_a_new_checkout() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let old = Workspace::new();
    let current = Workspace::new();
    old.set_host(&server.alias);
    current.set_host(&server.alias);
    let project = scenario_project("clean-old-checkout");
    let compose = format!("services:\n  api:\n    {IDLE}");
    old.write("compose.yaml", &compose);
    current.write("compose.yaml", &compose);

    old.ulak(&server)
        .args(["docker", "compose", "-p", &project, "up", "-d"])
        .assert()
        .success();
    current
        .ulak(&server)
        .args([
            "docker",
            "compose",
            "-p",
            &project,
            "up",
            "-d",
            "--force-recreate",
        ])
        .assert()
        .success();

    let current_workspace = current
        .workspace_ids()
        .into_iter()
        .next()
        .expect("the current checkout synced a workspace");
    let current_remote_root = current.remote_workspace_root(&current_workspace);
    let container = containers(&server, &project)
        .into_iter()
        .next()
        .expect("the current checkout left its stack running");
    let labels = server.ssh(&format!(
        "docker inspect --format '{{{{json .Config.Labels}}}}' {container}"
    ));
    assert!(
        String::from_utf8_lossy(&labels.stdout).contains(&current_workspace),
        "the replacement container still names the old checkout, so cleaning it would rightly refuse: {}",
        String::from_utf8_lossy(&labels.stdout)
    );

    // The old checkout no longer has a lifecycle claim. Docker's labels
    // now point at the current workspace, so only its obsolete bytes are
    // eligible for removal.
    std::fs::remove_dir_all(old.stacks_dir()).unwrap();
    old.ulak(&server)
        .args(["-p", &project, "clean"])
        .assert()
        .success();
    assert!(
        !containers(&server, &project).is_empty(),
        "cleaning the old checkout ran down against the shared Docker project"
    );
    let running = ids(
        &server,
        &format!("docker ps -q --filter label=com.docker.compose.project={project}"),
    );
    assert!(
        !running.is_empty(),
        "cleaning the old checkout stopped the current checkout's container"
    );
    let remote = server.ssh(&format!("cat {current_remote_root}/proj/compose.yaml"));
    assert_eq!(String::from_utf8_lossy(&remote.stdout), compose);

    tear_down(&server, &current, &project);
    old.forget_on_server(&server);
}

/// A Docker project may already be live from checkout A when checkout B
/// tries the same `-p`. If B's `up` fails before recreating anything,
/// the containers found after the failure are A's — their Compose path
/// labels prove it. Merely seeing a container must not rebind the
/// service from A's bytes to B's failed invocation.
#[test]
fn a_failed_up_from_a_new_checkout_restores_the_old_checkout_lifecycle() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let project = scenario_project("failed-checkout-switch");
    ws.write("compose.yaml", &format!("services:\n  api:\n    {IDLE}"));
    ws.ulak(&server)
        .args(["docker", "compose", "-p", &project, "up", "-d"])
        .assert()
        .success();
    let old_workspace = ws
        .workspace_ids()
        .into_iter()
        .next()
        .expect("checkout A synced its workspace");

    // A second checkout under the same HOME, so both commands write the
    // same per-stack lifecycle record just as two real repositories do.
    let replacement = ws.project.parent().unwrap().join("replacement");
    std::fs::create_dir_all(&replacement).unwrap();
    std::fs::write(
        replacement.join("ulak.local.toml"),
        format!("host = {:?}\n", server.alias),
    )
    .unwrap();
    std::fs::write(
        replacement.join("compose.yaml"),
        "services:\n  ghost:\n    image: ulak-no-such-image-v0:missing\n",
    )
    .unwrap();
    let out = ws
        .ulak(&server)
        .current_dir(&replacement)
        .args(["docker", "compose", "-p", &project, "up", "-d"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "checkout B's missing image unexpectedly started:\n{}",
        said(&out)
    );
    assert!(
        said(&out).contains("different workspace"),
        "Ulak did not explain why the old lifecycle record won:\n{}",
        said(&out)
    );

    let restored = desired_for(&ws, &project).expect("checkout A's declaration must survive");
    assert_eq!(restored["live"], true);
    assert_eq!(restored["workspace_id"], old_workspace);
    let old_cwd = ws.project.canonicalize().unwrap();
    assert_eq!(
        restored["cwd"].as_str(),
        Some(old_cwd.to_string_lossy().as_ref()),
        "the rollback kept B's invocation under A's live bit: {restored}"
    );
    assert!(
        !containers(&server, &project).is_empty(),
        "the failed up removed checkout A's running stack"
    );

    tear_down(&server, &ws, &project);
}

/// Compose documents `--no-recreate` as leaving existing containers alone,
/// and it still exits zero. A successful process therefore cannot prove that
/// the new checkout owns the running stack; Docker's path labels must keep
/// checkout A's declaration in place.
#[test]
fn a_successful_no_recreate_keeps_the_workspace_the_containers_still_use() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let project = scenario_project("no-recreate-workspace");
    let compose = format!("services:\n  api:\n    {IDLE}");
    ws.write("compose.yaml", &compose);
    ws.ulak(&server)
        .args(["docker", "compose", "-p", &project, "up", "-d"])
        .assert()
        .success();
    let old_workspace = desired_for(&ws, &project).unwrap()["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let replacement = second_checkout(&ws, &server, "no-recreate-b", &compose);

    let out = ws
        .ulak(&server)
        .current_dir(&replacement)
        .args([
            "docker",
            "compose",
            "-p",
            &project,
            "up",
            "-d",
            "--no-recreate",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "compose must succeed:\n{}",
        said(&out)
    );
    assert!(
        said(&out).contains("different workspace"),
        "Ulak did not explain why success kept A's declaration:\n{}",
        said(&out)
    );
    let restored = desired_for(&ws, &project).unwrap();
    assert_eq!(restored["workspace_id"], old_workspace);
    let labels = container_labels(&server, &project);
    assert!(
        labels.contains(&old_workspace),
        "--no-recreate unexpectedly moved the container, so the fixture proves nothing:\n{labels}"
    );

    tear_down(&server, &ws, &project);
}

/// An orphan from checkout A and a newly-created service from checkout B
/// share one Docker project name but not one workspace. A successful `up`
/// must report that split and must not assign the whole stack to B.
#[test]
fn a_successful_up_never_assigns_a_mixed_orphan_stack_to_the_new_workspace() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let project = scenario_project("mixed-orphan-success");
    ws.write("compose.yaml", &format!("services:\n  old:\n    {IDLE}"));
    ws.ulak(&server)
        .args(["docker", "compose", "-p", &project, "up", "-d"])
        .assert()
        .success();
    let old_workspace = desired_for(&ws, &project).unwrap()["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let replacement = second_checkout(
        &ws,
        &server,
        "mixed-orphan-success-b",
        &format!("services:\n  new:\n    {IDLE}"),
    );

    let out = ws
        .ulak(&server)
        .current_dir(&replacement)
        .args(["docker", "compose", "-p", &project, "up", "-d"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "compose must succeed:\n{}",
        said(&out)
    );
    assert!(
        said(&out).contains("split across old and new workspaces"),
        "the mixed ownership was not reported:\n{}",
        said(&out)
    );
    assert_eq!(
        desired_for(&ws, &project).unwrap()["workspace_id"],
        old_workspace
    );
    let labels = container_labels(&server, &project);
    let new_workspace = ws
        .workspace_ids()
        .into_iter()
        .find(|id| id != &old_workspace)
        .expect("checkout B synced its own workspace");
    assert!(
        labels.contains(&old_workspace) && labels.contains(&new_workspace),
        "the fixture did not leave one old and one new container:\n{labels}"
    );

    tear_down(&server, &ws, &project);
}

/// The failed branch used `any(new_workspace)` and therefore handed a mixed
/// project to B as soon as one service started. The old orphan and the new
/// unhealthy services must instead preserve A's declaration.
#[test]
fn a_failed_up_with_only_some_new_containers_never_claims_the_mixed_stack() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let project = scenario_project("mixed-orphan-failure");
    ws.write("compose.yaml", &format!("services:\n  old:\n    {IDLE}"));
    ws.ulak(&server)
        .args(["docker", "compose", "-p", &project, "up", "-d"])
        .assert()
        .success();
    let old_workspace = desired_for(&ws, &project).unwrap()["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let replacement = second_checkout(
        &ws,
        &server,
        "mixed-orphan-failure-b",
        &format!(
            "services:\n  new:\n    {IDLE}\n  sickly:\n    {IDLE}    healthcheck:\n      test: [\"CMD-SHELL\", \"exit 1\"]\n      interval: 1s\n      timeout: 1s\n      retries: 1\n      start_period: 0s\n"
        ),
    );

    let out = ws
        .ulak(&server)
        .current_dir(&replacement)
        .args(["docker", "compose", "-p", &project])
        .args(["up", "-d", "--wait", "--wait-timeout", "30"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "the unhealthy service must make compose fail:\n{}",
        said(&out)
    );
    assert!(
        said(&out).contains("split across old and new workspaces"),
        "the failed mixed ownership was not reported:\n{}",
        said(&out)
    );
    assert_eq!(
        desired_for(&ws, &project).unwrap()["workspace_id"],
        old_workspace
    );
    let labels = container_labels(&server, &project);
    let new_workspace = ws
        .workspace_ids()
        .into_iter()
        .find(|id| id != &old_workspace)
        .expect("checkout B synced its own workspace");
    assert!(
        labels.contains(&old_workspace) && labels.contains(&new_workspace),
        "the fixture did not leave a genuinely mixed failed stack:\n{labels}"
    );

    tear_down(&server, &ws, &project);
}

/// `up --wait` is DEFINED to exit nonzero when any container fails to
/// become healthy, and hardened install scripts put it on every `up`.
/// Rolling the intent back on that exit code treated "twenty-four of
/// twenty-five services are running" as "the stack never came up": the
/// service dropped the workspace from its job list, and dropping a
/// `Live` closes its tunnels. The ports went away while the containers
/// ran on — and silently, because `agent::stack_complaint` says
/// nothing about a workspace that is not declared live.
#[test]
fn an_up_that_only_half_succeeded_keeps_its_stack_live() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let project = scenario_project("halfway");
    ws.write(
        "compose.yaml",
        &format!(
            "services:\n  steady:\n    {IDLE}\
             \n  sickly:\n    {IDLE}    healthcheck:\n      \
             test: [\"CMD-SHELL\", \"exit 1\"]\n      interval: 1s\n      \
             timeout: 1s\n      retries: 2\n      start_period: 0s\n"
        ),
    );

    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "-p", &project])
        .args(["up", "-d", "--wait", "--wait-timeout", "60"])
        .output()
        .unwrap();
    let text = said(&out);
    assert!(
        !out.status.success(),
        "the fixture's healthcheck must fail, or this scenario proves nothing:\n{text}"
    );
    // Compose's own verdict still reaches the user untouched.
    let running = containers(&server, &project);
    assert!(
        !running.is_empty(),
        "compose left no containers behind, so there is no partial success \
         to be wrong about:\n{text}"
    );

    let d = desired(&ws).expect("up declares an intent before it runs");
    assert_eq!(
        d["live"],
        true,
        "a partial start was filed as 'never came up': the service would drop this \
         workspace and close its tunnels while {} container(s) run on. {d}",
        running.len()
    );
    assert!(
        text.contains("keeping it live"),
        "the stack stayed live and nothing said so:\n{text}"
    );

    // …and `down` still ends it, which is the only thing that may.
    ws.ulak(&server)
        .args(["docker", "compose", "-p", &project, "down"])
        .assert()
        .success();
    assert_eq!(desired(&ws).unwrap()["live"], false);
    tear_down(&server, &ws, &project);
}

/// The ordinary containerized backup, and the direction that makes it
/// one: the CONTAINER writes the file, into a bind mount whose source
/// is on this machine.
///
/// Asserted on the bytes rather than on the exit status, because an exit
/// status of 0 is exactly what this used to report while the dump sat on
/// the server.
#[test]
fn a_bind_source_the_container_writes_into_comes_home() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    // A NARROW footprint on purpose: a project that mounts `.` gets
    // everything home anyway, so it could not tell this apart.
    ws.write("compose.yaml", &format!("services:\n  box:\n    {IDLE}"));
    std::fs::create_dir_all(ws.project.join("backup")).unwrap();

    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "run", "--rm", "-T"])
        .args(["-v", "./backup:/backup", "box"])
        .args(["sh", "-c", "echo DUMP-BODY-42 > /backup/dump.json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "run failed:\n{}", said(&out));

    let landed = ws.project.join("backup/dump.json");
    let body = std::fs::read_to_string(&landed).unwrap_or_else(|e| {
        panic!(
            "{} never came home ({e}) — the container wrote it on the SERVER:\n{}",
            landed.display(),
            said(&out)
        )
    });
    assert_eq!(body.trim(), "DUMP-BODY-42");
    ws.forget_on_server(&server);
}

/// The same shape one step earlier: the bind source is not here yet.
///
/// Docker creates a missing bind source itself, on the SERVER, and the
/// container then fills a directory this machine never hears about. The
/// warning for it existed but was unreachable — it was keyed on
/// `Direction::Write`, and `composepaths` fixes a `-v` at
/// `Direction::Read` because there the direction answers "may I
/// canonicalize the last component", not "who writes into it".
#[test]
fn a_bind_source_that_is_not_here_is_named_before_the_command_runs() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws.write("compose.yaml", &format!("services:\n  box:\n    {IDLE}"));

    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "run", "--rm", "-T"])
        .args(["-v", "./not-here-yet:/out", "box"])
        .args(["sh", "-c", "echo LOST > /out/dump.json"])
        .output()
        .unwrap();
    let text = said(&out);
    assert!(
        text.contains("not-here-yet") && text.contains("on the SERVER"),
        "docker made the bind source on the server and nothing said so:\n{text}"
    );
    assert!(
        !ws.project.join("not-here-yet/dump.json").exists(),
        "the rule itself changed: a path that is not here gets no rsync rule, \
         deliberately — see passthrough::carry"
    );
    ws.forget_on_server(&server);
}

// ─── the flags themselves ───────────────────────────────────────────

/// Repeated `--profile` is how a developer stack turns its optional
/// halves on, and a profile silently dropped looks exactly like a
/// service that failed to start.
#[test]
fn a_profile_gated_service_starts_only_when_its_profile_is_named() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let project = scenario_project("profiles");
    ws.write(
        "compose.yaml",
        &format!(
            "services:\n  always:\n    {IDLE}\
             \n  tools:\n    {IDLE}    profiles: [\"dev_tools\"]\n\
             \n  extra:\n    {IDLE}    profiles: [\"etcd\"]\n"
        ),
    );

    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "-p", &project, "up", "-d"])
        .output()
        .unwrap();
    assert!(out.status.success(), "up failed:\n{}", said(&out));
    assert!(
        containers_of(&server, &project, "tools").is_empty(),
        "a profile nobody named started anyway — the gate is not being forwarded"
    );

    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "-p", &project])
        .args(["--profile", "dev_tools", "--profile", "etcd"])
        .args(["up", "-d"])
        .output()
        .unwrap();
    assert!(out.status.success(), "profiled up failed:\n{}", said(&out));
    for service in ["always", "tools", "extra"] {
        assert!(
            !containers_of(&server, &project, service).is_empty(),
            "{service} is missing, so one of the two --profile flags never arrived:\n{}",
            said(&out)
        );
    }

    // The other half of the same faithfulness, and the reason projects
    // spell their reset line `--profile '*'`: a plain `down` walks past
    // every gated container, prints nothing and exits 0. Ulak is
    // faithful there — `compose.rs`'s `remote_prefix` records the same
    // measurement — and a command that reports success while stopping
    // nothing is worth an assertion wherever it is legitimate, so that
    // the day it stops being compose's behaviour this says so.
    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "-p", &project, "down"])
        .output()
        .unwrap();
    assert!(out.status.success(), "plain down failed:\n{}", said(&out));
    assert!(
        !containers_of(&server, &project, "tools").is_empty(),
        "compose stopped a profile-gated service without being told its profile — \
         if that is now compose's own behaviour, the note in compose::remote_prefix \
         and this suite's tear_down both need re-measuring:\n{}",
        said(&out)
    );

    // …and the reset line that does end it.
    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "-p", &project, "--profile", "*"])
        .args(["down", "--remove-orphans"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "profiled down failed:\n{}",
        said(&out)
    );
    assert!(
        containers(&server, &project).is_empty(),
        "`--profile '*' down` left containers behind:\n{}",
        said(&out)
    );

    tear_down(&server, &ws, &project);
}

/// Two `--env-file` flags on one command line, which is how a hardened
/// install script layers its own defaults under the operator's
/// overrides. Both have to travel, and the LAST one has to win — read
/// off the rendered model rather than off a container, so the assertion
/// is about interpolation and nothing else.
#[test]
fn the_last_env_file_wins_and_both_reach_the_server() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws.write(
        "compose.yaml",
        "services:\n  box:\n    image: alpine:3.20\n    network_mode: \"none\"\n    \
         command: [\"echo\", \"${GREETING}\", \"${ONLY_IN_BASE}\"]\n",
    );
    ws.write(".env-base", "GREETING=from-base\nONLY_IN_BASE=base-only\n");
    ws.write(".env-over", "GREETING=from-override\n");

    let out = ws
        .ulak(&server)
        .args(["docker", "compose"])
        .args(["--env-file", ".env-base", "--env-file", ".env-over"])
        .args(["config"])
        .output()
        .unwrap();
    let text = said(&out);
    assert!(out.status.success(), "config failed:\n{text}");
    let rendered = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        rendered.contains("from-override"),
        "the second --env-file did not win:\n{rendered}"
    );
    assert!(
        !rendered.contains("from-base"),
        "the first --env-file overruled the second:\n{rendered}"
    );
    assert!(
        rendered.contains("base-only"),
        "only the last --env-file travelled — a layered install loses its \
         defaults that way:\n{rendered}"
    );
    ws.forget_on_server(&server);
}

/// `restart` and `up --force-recreate` are the two ways to give a
/// service a fresh start, and they differ in exactly one observable:
/// whether the container survives. Asserted together so the difference
/// itself is what is pinned.
///
/// `--no-deps` rides along because it belongs to the same command and
/// answers the same way: the dependency must NOT be started.
#[test]
fn restart_keeps_the_container_force_recreate_replaces_it_and_no_deps_leaves_the_rest() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let project = scenario_project("recreate");
    ws.write(
        "compose.yaml",
        &format!(
            "services:\n  db:\n    {IDLE}\
             \n  web:\n    {IDLE}    depends_on: [db]\n"
        ),
    );

    // `--no-deps`: web alone, db untouched.
    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "-p", &project])
        .args(["up", "-d", "--no-deps", "web"])
        .output()
        .unwrap();
    assert!(out.status.success(), "up --no-deps failed:\n{}", said(&out));
    let web = containers_of(&server, &project, "web");
    assert_eq!(web.len(), 1, "web did not come up:\n{}", said(&out));
    assert!(
        containers_of(&server, &project, "db").is_empty(),
        "--no-deps was dropped: compose started the dependency too:\n{}",
        said(&out)
    );

    // `restart`: same container, new start time.
    let started_before = inspect(&server, &web[0], "{{.State.StartedAt}}");
    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "-p", &project, "restart", "web"])
        .output()
        .unwrap();
    assert!(out.status.success(), "restart failed:\n{}", said(&out));
    assert_eq!(
        containers_of(&server, &project, "web"),
        web,
        "restart replaced the container — that is force-recreate's job:\n{}",
        said(&out)
    );
    assert_ne!(
        inspect(&server, &web[0], "{{.State.StartedAt}}"),
        started_before,
        "restart left the container exactly as it was, so the verb never \
         reached compose:\n{}",
        said(&out)
    );

    // `--force-recreate`: a different container.
    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "-p", &project])
        .args(["up", "-d", "--no-deps", "--force-recreate", "web"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "force-recreate failed:\n{}",
        said(&out)
    );
    assert_ne!(
        containers_of(&server, &project, "web"),
        web,
        "--force-recreate was dropped: the old container is still there:\n{}",
        said(&out)
    );
    assert!(
        containers_of(&server, &project, "db").is_empty(),
        "--no-deps was dropped on the recreate:\n{}",
        said(&out)
    );

    tear_down(&server, &ws, &project);
}

fn inspect(server: &TestServer, id: &str, template: &str) -> String {
    let out = server.ssh(&format!("docker inspect -f '{template}' {id}"));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `compose pull` on a stack whose images are already on the daemon.
///
/// `--policy missing` is not decoration here: this suite may not reach a
/// registry (`ULAK_TEST_E2E_HUB=block` audits exactly that), and the image
/// was handed over from this machine by `needs_image`. So what is being
/// asserted is the routing — the verb reaches the server's daemon and
/// answers about the server's images — not a download.
#[test]
fn compose_pull_reaches_the_servers_daemon_and_asks_only_for_what_is_missing() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws.write("compose.yaml", &format!("services:\n  box:\n    {IDLE}"));

    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "pull", "--policy", "missing"])
        .output()
        .unwrap();
    let text = said(&out);
    assert!(
        out.status.success(),
        "compose pull --policy missing had nothing to fetch and still failed \
         — either the verb did not reach the server, or it went to a registry \
         this suite cannot reach:\n{text}"
    );
    ws.forget_on_server(&server);
}

/// `logs --tail=N` is the default shape of everyday debugging, and the
/// only one whose answer can be counted exactly.
#[test]
fn logs_tail_returns_exactly_the_lines_it_was_asked_for() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let project = scenario_project("tail");
    ws.write(
        "compose.yaml",
        "services:\n  talker:\n    image: alpine:3.20\n    network_mode: \"none\"\n    \
         command:\n      - sh\n      - -c\n      - 'i=1; while [ $$i -le 10 ]; do echo line-$$i; \
         i=$$((i+1)); done; sleep 600'\n",
    );

    ws.ulak(&server)
        .args(["docker", "compose", "-p", &project, "up", "-d"])
        .assert()
        .success();

    // The container writes its ten lines and stays up; poll for the last
    // one rather than sleeping a guessed amount.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let out = ws
            .ulak(&server)
            .args(["docker", "compose", "-p", &project, "logs", "talker"])
            .output()
            .unwrap();
        if said(&out).contains("line-10") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fixture never printed its ten lines:\n{}",
            said(&out)
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "-p", &project])
        .args(["logs", "--tail", "3", "talker"])
        .output()
        .unwrap();
    let text = said(&out);
    let lines: Vec<&str> = text.lines().filter(|l| l.contains("line-")).collect();
    assert_eq!(
        lines.len(),
        3,
        "--tail 3 was dropped or rewritten — got {} log lines:\n{text}",
        lines.len()
    );
    assert!(
        lines.iter().any(|l| l.contains("line-10")),
        "--tail took the FIRST lines, not the last:\n{text}"
    );

    tear_down(&server, &ws, &project);
}

/// `exec -it` is the interactive shell, and the flags it carries are the
/// CONTAINER's, not Ulak's.
///
/// Ulak decides its own `ssh -t` from the terminals it was handed —
/// `stdin.is_terminal() && stdout.is_terminal()` — and a test's stdio
/// are pipes, so it must add none. What the user typed still has to
/// reach compose verbatim: rewriting `-it` here would either take away
/// the shell they asked for or, worse, put the shared terminal into raw
/// mode behind a backgrounded child.
///
/// Read off the audit trail rather than a terminal, because the trail is
/// the product's own record of what it ran — no test seam, and it says
/// both halves in one line.
#[test]
fn an_interactive_exec_forwards_its_own_flags_and_adds_no_tty_of_its_own() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let project = scenario_project("interactive");
    ws.write("compose.yaml", &format!("services:\n  box:\n    {IDLE}"));

    ws.ulak(&server)
        .args(["docker", "compose", "-p", &project, "up", "-d"])
        .assert()
        .success();

    // Not asserted successful: compose refuses `-t` without a tty, and
    // being faithful about that IS the behaviour. What must not happen
    // is a hang, or a rewrite.
    let _ = ws
        .ulak(&server)
        .args(["docker", "compose", "-p", &project])
        .args(["exec", "-it", "box", "true"])
        .output()
        .unwrap();

    let line = last_ssh_argv(&ws).expect("the compose route audits every ssh it runs");
    let (flags, remote) = line.split_at(line.len() - 1);
    assert!(
        remote[0].contains("exec -it"),
        "the container's own flags were rewritten on the way out: {remote:?}"
    );
    assert!(
        !flags.iter().any(|a| a == "-t"),
        "Ulak added an ssh -t for a command whose stdio are pipes — a \
         backgrounded `ssh -t` puts the shared terminal in raw mode and steals \
         Ctrl-C from the whole process group: {flags:?}"
    );

    tear_down(&server, &ws, &project);
}

/// The ssh argv of the last compose command this workspace ran, exactly
/// as the audit trail recorded it (the final element is the redacted
/// remote command).
fn last_ssh_argv(ws: &Workspace) -> Option<Vec<String>> {
    let state = ws.home.join(".local/state/ulak");
    let mut newest: Option<(u64, Vec<String>)> = None;
    for entry in std::fs::read_dir(&state).ok()?.flatten() {
        let trail = entry.path().join("commands.jsonl");
        let Ok(text) = std::fs::read_to_string(&trail) else {
            continue;
        };
        for value in text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["kind"] == "ssh")
        {
            let argv: Vec<String> = value["argv"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let ts = value["ts_unix"].as_u64().unwrap_or(0);
            if argv.last().is_some_and(|r| r.contains("docker compose"))
                && newest.as_ref().is_none_or(|(seen, _)| ts >= *seen)
            {
                newest = Some((ts, argv));
            }
        }
    }
    newest.map(|(_, argv)| argv)
}

/// A real `up --wait` waits for a whole stack to become healthy, which
/// on a hardened install is minutes. Nothing in Ulak may put a clock on
/// it: the two sync legs each carry their own `proc::Budget`, on purpose
/// two rather than one, and the compose command that runs between them
/// is charged for neither.
///
/// The fixture waits about twenty seconds — past `proc::PROBE` and past
/// every per-leg budget except the two long ones — so a refactor that
/// "adds the missing budget" to the compose child goes red here instead
/// of cutting somebody's install off in its third minute with the stack
/// half up. `passthrough.rs`'s
/// `the_compose_child_runs_under_no_clock_of_ulaks_own` pins the same
/// rule in source, where it costs nothing.
#[test]
fn an_up_that_waits_longer_than_a_budget_is_not_cut_off() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let project = scenario_project("slowwait");
    ws.write(
        "compose.yaml",
        "services:\n  slowly:\n    image: alpine:3.20\n    network_mode: \"none\"\n    \
         command:\n      - sh\n      - -c\n      - 'sleep 20; touch /tmp/ready; sleep 600'\n    \
         healthcheck:\n      test: [\"CMD-SHELL\", \"test -f /tmp/ready\"]\n      \
         interval: 2s\n      timeout: 2s\n      retries: 30\n      \
         start_period: 0s\n",
    );

    let began = std::time::Instant::now();
    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "-p", &project])
        .args(["up", "-d", "--wait", "--wait-timeout", "120"])
        .output()
        .unwrap();
    let waited = began.elapsed();
    let text = said(&out);
    assert!(
        out.status.success(),
        "a --wait that outlasted a budget was cut off after {waited:?}:\n{text}"
    );
    assert!(
        waited >= std::time::Duration::from_secs(15),
        "the fixture became healthy in {waited:?}, so nothing about a long wait \
         was exercised"
    );
    assert_eq!(desired(&ws).unwrap()["live"], true);

    tear_down(&server, &ws, &project);
}
