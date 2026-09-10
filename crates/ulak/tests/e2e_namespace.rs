//! Client namespace parity and isolation.
//!
//! These scenarios use one absolute project path and one SSH account from
//! two independent local state roots. That is the ordinary collision shape:
//! two laptops or agents both clone a repository to the same path. The
//! transported bytes must separate without changing the Compose project name
//! Docker itself resolves.

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use std::time::{Duration, Instant};

use common::{Service, TestServer, Workspace, desired, extract_remote_dir};

struct RemoteCleanup<'a> {
    server: &'a TestServer,
    roots: Vec<String>,
    projects: Vec<String>,
}

impl<'a> RemoteCleanup<'a> {
    fn new(server: &'a TestServer) -> RemoteCleanup<'a> {
        RemoteCleanup {
            server,
            roots: Vec::new(),
            projects: Vec::new(),
        }
    }

    fn remember(&mut self, remote_project_dir: &str) -> String {
        let root = remote_project_dir
            .rsplit_once('/')
            .map(|(root, _)| root)
            .expect("the remote project directory sits below a workspace")
            .to_string();
        if !self.roots.contains(&root) {
            self.roots.push(root.clone());
        }
        root
    }

    fn remember_project(&mut self, project: &str) {
        if !self.projects.iter().any(|known| known == project) {
            self.projects.push(project.to_string());
        }
    }
}

impl Drop for RemoteCleanup<'_> {
    fn drop(&mut self) {
        for project in &self.projects {
            let _ = self.server.ssh_quiet(&format!(
                "ids=$(docker ps -aq --filter label=com.docker.compose.project={project}); \
                 if test -n \"$ids\"; then docker rm -f $ids >/dev/null; fi; \
                 docker network rm {project}_default >/dev/null 2>&1; true"
            ));
        }
        for root in &self.roots {
            let _ = self.server.ssh_quiet(&format!("rm -rf {root}"));
            if let Some((namespace, _)) = root.rsplit_once('/') {
                let _ = self.server.ssh_quiet(&format!("rmdir {namespace}"));
            }
        }
    }
}

fn command_as(ws: &Workspace, server: &TestServer, home: &Path) -> Command {
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    let mut command = ws.ulak(server);
    command
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join(".local/state"));
    command
}

fn run_as(ws: &Workspace, server: &TestServer, home: &Path, args: &[&str]) -> Output {
    let mut command = command_as(ws, server, home);
    command.args(args);
    command.output().unwrap()
}

/// The remote workspace path, asked of the command that reports it.
///
/// Only `sync` prints the `~/.ulak/workspaces/…` line; every passthrough
/// route prints `workspace synced (n pushed, m deleted)` and stops there.
/// Reading a `docker compose up` for the path therefore finds nothing at
/// all — which is how three scenarios here failed against code that was
/// right. Keeping the question in one place is what stops the next test
/// from picking a route that does not answer it.
fn remote_dir_of(mut command: Command) -> String {
    command.arg("sync");
    let out = command.output().unwrap();
    assert_success(&out, "the sync that reports the remote workspace");
    remote_dir(&out)
}

fn assert_success(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn remote_dir(out: &Output) -> String {
    extract_remote_dir(&String::from_utf8_lossy(&out.stderr)).unwrap_or_else(|| {
        panic!(
            "no remote workspace in:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn namespace(home: &Path) -> String {
    std::fs::read_to_string(home.join(".local/state/ulak/client/namespace"))
        .expect("a workspace command records the automatic client namespace")
}

fn one_workspace_id(home: &Path) -> String {
    let mut ids: Vec<String> = std::fs::read_dir(home.join(".local/state/ulak/workspaces"))
        .expect("the command registered a workspace")
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort();
    assert_eq!(ids.len(), 1, "one project path must register one workspace");
    ids.pop().unwrap()
}

#[test]
fn independent_clients_with_the_same_checkout_path_never_share_a_workspace() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws.write("proof.txt", "same local bytes\n");
    let other_home = ws.project.parent().unwrap().join("other-home");
    let mut cleanup = RemoteCleanup::new(&server);

    let first = run_as(&ws, &server, &ws.home, &["sync"]);
    assert_success(&first, "the first client's sync");
    let first_dir = remote_dir(&first);
    let first_root = cleanup.remember(&first_dir);
    let first_namespace = namespace(&ws.home);

    // A second command from the same client must address the same place:
    // automatic means no configuration, not a fresh random choice per run.
    ws.write("proof.txt", "same client, next command\n");
    let again = run_as(&ws, &server, &ws.home, &["sync"]);
    assert_success(&again, "the first client's second sync");
    assert_eq!(remote_dir(&again), first_dir);
    assert_eq!(namespace(&ws.home), first_namespace);

    let second = run_as(&ws, &server, &other_home, &["sync"]);
    assert_success(&second, "the independent client's sync");
    let second_dir = remote_dir(&second);
    let second_root = cleanup.remember(&second_dir);
    let second_namespace = namespace(&other_home);

    assert_ne!(first_namespace, second_namespace);
    assert_ne!(first_root, second_root);
    assert!(first_dir.contains(&format!("/workspaces/{first_namespace}/")));
    assert!(second_dir.contains(&format!("/workspaces/{second_namespace}/")));

    // Assert on bytes, not just command status: each locator holds its own
    // server-side file, and cleaning one cannot touch the other's receipt.
    server.ssh(&format!(
        "printf FIRST > {first_dir}/owner.txt; printf SECOND > {second_dir}/owner.txt"
    ));
    let cleaned = run_as(&ws, &server, &ws.home, &["clean"]);
    assert_success(&cleaned, "cleaning the first client");
    let kept = server.ssh(&format!(
        "test ! -e {first_root} && cat {second_dir}/owner.txt"
    ));
    assert!(
        kept.status.success() && kept.stdout == b"SECOND",
        "cleaning one client changed the other's bytes:\n{}",
        String::from_utf8_lossy(&kept.stderr)
    );

    let cleaned = run_as(&ws, &server, &other_home, &["clean"]);
    assert_success(&cleaned, "cleaning the second client");
    assert!(
        server
            .ssh(&format!("test ! -e {second_root}"))
            .status
            .success(),
        "the second workspace survived its own clean"
    );
}

fn write_global_namespace(home: &Path, namespace: &str) {
    let config = home.join(".config/ulak/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, format!("[workspace]\nnamespace = {namespace:?}\n")).unwrap();
}

#[test]
fn configured_client_namespaces_move_files_but_never_rename_the_docker_project() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let other_home: PathBuf = ws.project.parent().unwrap().join("named-client-home");
    write_global_namespace(&ws.home, "alice-laptop");
    write_global_namespace(&other_home, "ci-agent-2");
    let mut cleanup = RemoteCleanup::new(&server);

    let alice = run_as(
        &ws,
        &server,
        &ws.home,
        &["docker", "compose", "config", "--format", "json"],
    );
    assert_success(&alice, "Docker config from alice-laptop");
    let alice_dir = format!(
        ".ulak/workspaces/alice-laptop/{}/proj",
        one_workspace_id(&ws.home)
    );
    cleanup.remember(&alice_dir);

    let agent = run_as(
        &ws,
        &server,
        &other_home,
        &["docker", "compose", "config", "--format", "json"],
    );
    assert_success(&agent, "Docker config from ci-agent-2");
    let agent_dir = format!(
        ".ulak/workspaces/ci-agent-2/{}/proj",
        one_workspace_id(&other_home)
    );
    cleanup.remember(&agent_dir);

    assert!(alice_dir.contains("/workspaces/alice-laptop/"));
    assert!(agent_dir.contains("/workspaces/ci-agent-2/"));
    assert_ne!(alice_dir, agent_dir);

    let alice_model: serde_json::Value = serde_json::from_slice(&alice.stdout).unwrap();
    let agent_model: serde_json::Value = serde_json::from_slice(&agent.stdout).unwrap();
    assert_eq!(alice_model["name"], "proj");
    assert_eq!(agent_model["name"], "proj");
    assert_eq!(
        alice_model["name"], agent_model["name"],
        "the client namespace leaked into Docker's project namespace"
    );

    assert_success(
        &run_as(&ws, &server, &ws.home, &["clean"]),
        "cleaning alice-laptop",
    );
    assert_success(
        &run_as(&ws, &server, &other_home, &["clean"]),
        "cleaning ci-agent-2",
    );
}

/// A project name proves only which Docker stack a container belongs to;
/// its Compose path labels prove which transported workspace it is using.
/// After client B recreates A's project, A's service must drop its tunnel
/// and leave A's now-unused bytes untouched.
#[test]
fn a_service_never_adopts_same_named_containers_from_another_workspace() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("nginx:alpine");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let other_home: PathBuf = ws.project.parent().unwrap().join("probe-owner-b-home");
    write_global_namespace(&ws.home, "probe-owner-a");
    write_global_namespace(&other_home, "probe-owner-b");
    let identity = format!("ulak-probe-owner-{}", std::process::id());
    let local_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    ws.write(
        "compose.yaml",
        &format!(
            "services:\n  web:\n    image: nginx:alpine\n    ports:\n      - \"127.0.0.1:{local_port}:80\"\n    volumes:\n      - .:/app:ro\n"
        ),
    );
    ws.write("owner.txt", "started by A\n");
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.remember_project(&identity);

    let a = run_as(
        &ws,
        &server,
        &ws.home,
        &["docker", "compose", "-p", &identity, "up", "-d"],
    );
    assert_success(&a, "client A's up");
    let a_dir = remote_dir_of(command_as(&ws, &server, &ws.home));
    let a_root = cleanup.remember(&a_dir);
    let service = Service::start(&ws, &server);
    let before = service.wait_status(&ws, 90, "A's live tunnel", |status| {
        status["connection"] == "up"
            && status["tunnels"]
                .as_array()
                .is_some_and(|tunnels| tunnels.iter().any(|t| t["open"] == true))
    });
    assert!(
        before["tunnels"]
            .as_array()
            .is_some_and(|tunnels| !tunnels.is_empty()),
        "the precondition needs a live tunnel: {before}"
    );

    ws.write("owner.txt", "recreated by B\n");
    let b = run_as(
        &ws,
        &server,
        &other_home,
        &[
            "docker",
            "compose",
            "-p",
            &identity,
            "up",
            "-d",
            "--force-recreate",
        ],
    );
    assert_success(&b, "client B's force-recreate");
    let b_dir = remote_dir_of(command_as(&ws, &server, &other_home));
    let b_root = cleanup.remember(&b_dir);
    assert_ne!(a_root, b_root);

    let blocked = service.wait_status(&ws, 90, "the workspace ownership mismatch", |status| {
        status["note"]
            .as_str()
            .is_some_and(|note| note.contains("different transported workspace"))
    });
    assert!(
        blocked["tunnels"].as_array().is_some_and(Vec::is_empty),
        "a mismatched stack kept A's tunnel alive: {blocked}"
    );
    let released = std::net::TcpListener::bind(("127.0.0.1", local_port));
    assert!(
        released.is_ok(),
        "the mismatched stack still owns localhost:{local_port}"
    );
    drop(released);

    // Trigger A's watcher only after the mismatch is established. A probe
    // that checked just the project label would sync this into a workspace
    // no running container uses.
    ws.write("a-service-must-not-sync.txt", "A must leave this local\n");
    std::thread::sleep(Duration::from_secs(3));
    let untouched = server.ssh(&format!(
        "if test -e {a_dir}/a-service-must-not-sync.txt; then cat {a_dir}/a-service-must-not-sync.txt; else printf ABSENT; fi"
    ));
    assert_eq!(
        untouched.stdout, b"ABSENT",
        "A's service synced bytes into an unused workspace"
    );
    let labels = server.ssh(&format!(
        "ids=$(docker ps -q --filter label=com.docker.compose.project={identity}); docker inspect --format '{{{{index .Config.Labels \"com.docker.compose.project.working_dir\"}}}}' $ids"
    ));
    let labels = String::from_utf8_lossy(&labels.stdout);
    assert!(
        labels.contains(&b_dir) && !labels.contains(&a_dir),
        "the fixture did not move the running container to B:\n{labels}"
    );

    drop(service);
    assert_success(
        &run_as(
            &ws,
            &server,
            &other_home,
            &[
                "docker",
                "compose",
                "-p",
                &identity,
                "down",
                "--remove-orphans",
            ],
        ),
        "taking B's stack down",
    );
    assert_success(
        &run_as(
            &ws,
            &server,
            &ws.home,
            &["docker", "compose", "-p", &identity, "down"],
        ),
        "retiring A's declaration",
    );
    assert_success(&run_as(&ws, &server, &ws.home, &["clean"]), "cleaning A");
    assert_success(&run_as(&ws, &server, &other_home, &["clean"]), "cleaning B");
}

#[derive(Clone, Copy)]
enum HistoricalNameSource {
    DotEnv,
    TopLevel,
}

impl HistoricalNameSource {
    fn label(self) -> &'static str {
        match self {
            HistoricalNameSource::DotEnv => "env",
            HistoricalNameSource::TopLevel => "top-level",
        }
    }
}

fn historical_compose(name: Option<&str>) -> String {
    let top = name.map_or_else(String::new, |name| format!("name: {name}\n"));
    format!(
        "{top}services:\n  idle:\n    image: alpine:3.20\n    command: [\"sleep\", \"600\"]\n    network_mode: none\n    volumes:\n      - ./data-${{COMPOSE_PROJECT_NAME}}:/data:ro\n"
    )
}

/// These scenarios are about which name Compose itself resolves, so the
/// ambient `COMPOSE_PROJECT_NAME` must never reach the child: it outranks
/// both `.env` and top-level `name:` in Docker's own cascade and would
/// decide the very thing under test.
fn historical_command(ws: &Workspace, server: &TestServer) -> Command {
    let mut command = ws.ulak(server);
    command.env_remove("COMPOSE_PROJECT_NAME");
    command
}

fn run_historical_compose(ws: &Workspace, server: &TestServer, args: &[&str]) -> Output {
    let mut command = historical_command(ws, server);
    command.args(["docker", "compose"]).args(args);
    command.output().unwrap()
}

fn wait_for_historical_bytes(
    server: &TestServer,
    ws: &Workspace,
    service: &Service,
    remote_path: &str,
    expected: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let out = server.ssh(&format!("cat {remote_path} 2>/dev/null"));
        if out.stdout == expected.as_bytes() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the historical footprint never sent {remote_path}.\nlocal workspaces: {:?}\nremote said: {:?}\nrsync trail:\n{}\nservice log:\n{}",
            ws.workspace_ids(),
            String::from_utf8_lossy(&out.stdout),
            ws.rsync_trail("after-name-change.txt"),
            service.said()
        );
        std::thread::sleep(Duration::from_millis(400));
    }
}

fn historical_name_source_stays_pinned(source: HistoricalNameSource) {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let suffix = source.label();
    let old = format!("ulak-history-{suffix}-old-{}", std::process::id());
    let new = format!("ulak-history-{suffix}-future-{}", std::process::id());
    match source {
        HistoricalNameSource::DotEnv => {
            ws.write("compose.yaml", &historical_compose(None));
            ws.write(".env", &format!("COMPOSE_PROJECT_NAME={old}\n"));
        }
        HistoricalNameSource::TopLevel => {
            ws.write("compose.yaml", &historical_compose(Some(&old)));
        }
    }
    ws.write(&format!("data-{old}/seed.txt"), "old model\n");
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.remember_project(&old);

    let up = run_historical_compose(&ws, &server, &["up", "-d"]);
    assert_success(&up, "starting the historically named stack");
    let remote_dir = remote_dir_of(historical_command(&ws, &server));
    cleanup.remember(&remote_dir);
    assert_eq!(
        desired(&ws)
            .as_ref()
            .and_then(|state| state["identity"].as_str()),
        Some(old.as_str()),
        "the service fixture must record the old Docker identity"
    );

    let service = Service::start(&ws, &server);
    service.wait_status_for(&ws, &old, 90, "the old stack's first reconcile", |status| {
        status["connection"] == "up" && status["last_sync_unix"].as_u64().is_some_and(|v| v > 0)
    });

    match source {
        HistoricalNameSource::DotEnv => {
            ws.write(".env", &format!("COMPOSE_PROJECT_NAME={new}\n"));
        }
        HistoricalNameSource::TopLevel => {
            ws.write("compose.yaml", &historical_compose(Some(&new)));
        }
    }
    let expected = format!("still {old}\n");
    ws.write(&format!("data-{old}/after-name-change.txt"), &expected);
    ws.write(&format!("data-{new}/wrong-model.txt"), "must stay local\n");

    wait_for_historical_bytes(
        &server,
        &ws,
        &service,
        &format!("{remote_dir}/data-{old}/after-name-change.txt"),
        &expected,
    );
    std::thread::sleep(Duration::from_secs(3));
    let wrong = server.ssh(&format!(
        "test ! -e {remote_dir}/data-{new}/wrong-model.txt"
    ));
    assert!(
        wrong.status.success(),
        "the service resolved the changed project name instead of the declared stack:\n{}",
        service.said()
    );
    let running = server.ssh(&format!(
        "docker ps -q --filter label=com.docker.compose.project={old}"
    ));
    assert!(
        !String::from_utf8_lossy(&running.stdout).trim().is_empty(),
        "the old stack stopped running while its historical model was resolved"
    );

    drop(service);
    assert_success(
        &run_historical_compose(&ws, &server, &["-p", &old, "down", "--remove-orphans"]),
        "taking the historically named stack down",
    );
    assert_success(
        &ws.ulak(&server).arg("clean").output().unwrap(),
        "cleaning the historical workspace",
    );
}

/// Changing an implicit `.env` after `up` names a future stack, not the
/// running one. The service must still resolve interpolation and footprint
/// paths with the identity stored in that running stack's declaration.
#[test]
fn a_service_resolves_a_historical_dot_env_name_with_the_declared_identity() {
    historical_name_source_stays_pinned(HistoricalNameSource::DotEnv);
}

/// A changed top-level `name:` has the same historical boundary as `.env`.
/// It must not retarget either the service probe or `${COMPOSE_PROJECT_NAME}`
/// inside the model of the stack that was already declared.
#[test]
fn a_service_resolves_a_historical_top_level_name_with_the_declared_identity() {
    historical_name_source_stays_pinned(HistoricalNameSource::TopLevel);
}
