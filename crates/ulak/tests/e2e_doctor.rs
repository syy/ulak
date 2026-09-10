//! Phase 2 e2e: doctor against a real server, driven by the
//! examples/demo fixture (the 5-mechanism prober project).
//!
//! Seven claims, seven verdicts. They used to be one `#[test]` calling
//! seven scenarios in sequence, which reported "1 passed" for all of
//! them and aborted the rest at the first failure — so a broken
//! classification of out-of-root paths hid whether external resources,
//! public ports, protect candidates and compose blame still worked at
//! all. Nothing here justified that: every scenario below rewrites the
//! compose file and asks doctor one question, and the only reason they
//! were threaded together was the cost of the fixture, which
//! `TestServer::shared` now pays once for the whole binary.
//!
//! What that costs each of them: cargo runs these in PARALLEL, so each
//! takes its OWN workspace (the compose file being the thing under test,
//! a shared one would have them overwriting each other's question), and
//! anything made on the server carries the scenario's name.

mod common;

use common::{TestServer, Workspace};

/// The demo project, pointed at the shared server, in a workspace of
/// this scenario's own.
fn demo_workspace(server: &TestServer) -> Workspace {
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
    ws.use_demo_fixture();
    ws.set_host(&server.alias);
    ws
}

fn doctor_output(ws: &Workspace, server: &TestServer) -> (bool, String) {
    let out = ws.ulak(server).arg("doctor").output().unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// The Compose-free doctor branch used to print the workspace root twice,
/// once under the misleading `config` label. Verify its real output while
/// also making the server round trip.
#[test]
fn a_compose_free_doctor_names_the_workspace_once() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
    ws.set_host(&server.alias);

    let (ok, text) = doctor_output(&ws, &server);
    assert!(ok, "workspace doctor must pass:\n{text}");
    let root = ws.project.display().to_string();
    let root_lines = text.lines().filter(|line| line.contains(&root)).count();
    assert_eq!(
        root_lines, 1,
        "workspace root must be reported once:\n{text}"
    );
    assert!(text.contains("workspace"));
    assert!(text.contains("not configured — skipped"));
}

/// The demo project: everything SYNC, data/ writable → protect hint.
#[test]
fn the_demo_project_classifies_clean_and_offers_the_protect_line() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = demo_workspace(&server);

    let (ok, text) = doctor_output(&ws, &server);
    assert!(ok, "doctor must pass on the demo project:\n{text}");

    for expected in ["SYNC", "site", "bind", "app", "configs/app.conf"] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }
    assert!(
        text.contains("writable"),
        "data/ must be flagged writable:\n{text}"
    );
    assert!(
        text.contains("protect = [\"data/\"]") || text.contains("protect = [\"data\"]"),
        "must suggest a ready-to-paste protect line:\n{text}"
    );
    // The whole selected invocation, not a bare `up`: since root commands
    // recover a declared context, the next step carries `-f`, `-p` and
    // `--project-directory` so a paste addresses exactly this stack.
    assert!(
        text.contains("ulak docker compose -f ") && text.contains(" up -d"),
        "success must point at an exact up:\n{text}"
    );

    ws.forget_on_server(&server);
}

/// Adding the suggested protect line removes the warning.
#[test]
fn the_suggested_protect_line_silences_the_warning() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = demo_workspace(&server);

    ws.write("ulak.toml", "[sync]\nprotect = [\"data/\"]\n");
    let (ok, text) = doctor_output(&ws, &server);
    assert!(ok, "doctor must still pass:\n{text}");
    assert!(
        !text.contains("without protect"),
        "protect warning must disappear once configured:\n{text}"
    );
    assert!(
        text.contains("protected"),
        "the data mount should now show as protected:\n{text}"
    );

    ws.forget_on_server(&server);
}

/// /etc/hostname exists on the server (SERVER); a bogus path does not
/// (MISSING) and must fail doctor with a create-it suggestion.
#[test]
fn an_out_of_root_path_is_the_servers_own_and_a_missing_one_fails() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = demo_workspace(&server);

    let compose = std::fs::read_to_string(ws.project.join("compose.yaml")).unwrap();
    let with_outside = compose.replace(
        "    volumes:\n      - ./site:/usr/share/nginx/html:ro",
        "    volumes:\n      - ./site:/usr/share/nginx/html:ro\n      - /etc/hostname:/mnt/host-name:ro",
    );
    assert_ne!(with_outside, compose, "compose fixture changed shape");
    std::fs::write(ws.project.join("compose.yaml"), &with_outside).unwrap();

    let (ok, text) = doctor_output(&ws, &server);
    assert!(ok, "an existing server path must not fail doctor:\n{text}");
    assert!(
        text.contains("SERVER") && text.contains("/etc/hostname"),
        "localtime must classify as SERVER:\n{text}"
    );

    let with_missing = with_outside.replace(
        "      - /etc/hostname:/mnt/host-name:ro",
        "      - /etc/hostname:/mnt/host-name:ro\n      - /ulak-e2e-definitely-missing:/x:ro",
    );
    std::fs::write(ws.project.join("compose.yaml"), &with_missing).unwrap();

    let (ok, text) = doctor_output(&ws, &server);
    assert!(!ok, "a missing server path must fail doctor:\n{text}");
    assert!(
        text.contains("MISSING") && text.contains("/ulak-e2e-definitely-missing"),
        "the missing path must be named:\n{text}"
    );
    assert!(
        text.contains("mkdir -p"),
        "must suggest creating it on the server:\n{text}"
    );

    ws.forget_on_server(&server);
}

/// Compose declares `external: true` and then fails at `up` with a
/// message that names neither the resource nor the fix.
///
/// Its own compose file, because compose PRUNES external resources no
/// service uses (measured) — an unused declaration is invisible to the
/// model, and rightly so.
#[test]
fn a_missing_external_resource_is_named_with_the_command_that_makes_it() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = demo_workspace(&server);

    let net = format!("ulak-e2e-net-{}", std::process::id());
    let vol = format!("ulak-e2e-vol-{}", std::process::id());
    ws.write(
        "compose.yaml",
        &format!(
            "services:\n  app:\n    image: alpine:3.20\n    command: [\"true\"]\n    \
             networks: [shared]\n    volumes:\n      - keep:/data\n\
             networks:\n  shared:\n    external: true\n    name: {net}\n\
             volumes:\n  keep:\n    external: true\n    name: {vol}\n"
        ),
    );

    let (ok, text) = doctor_output(&ws, &server);
    assert!(!ok, "missing external resources must fail doctor:\n{text}");
    for name in [&net, &vol] {
        assert!(
            text.contains(name.as_str()),
            "the external resource {name} must be named:\n{text}"
        );
    }
    assert!(
        text.contains("ulak docker network create") && text.contains("docker volume create"),
        "must say exactly how to create each:\n{text}"
    );

    // Create them and the complaint disappears — doctor never creates
    // them itself, because docker would not either (scope guard).
    server.ssh(&format!("docker network create {net} >/dev/null 2>&1"));
    server.ssh(&format!("docker volume create {vol} >/dev/null 2>&1"));
    let (ok, text) = doctor_output(&ws, &server);
    assert!(ok, "existing external resources must pass:\n{text}");
    assert!(
        text.contains("already on the server"),
        "must show them as present:\n{text}"
    );
    server.ssh(&format!("docker network rm {net} >/dev/null 2>&1"));
    server.ssh(&format!("docker volume rm {vol} >/dev/null 2>&1"));

    ws.forget_on_server(&server);
}

/// The service brings the ports to localhost, which makes a port feel
/// private. A port published without a host IP is open on the server's
/// public interface.
#[test]
fn a_port_published_on_every_interface_is_called_out_and_a_loopback_one_is_not() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = demo_workspace(&server);

    let compose = std::fs::read_to_string(ws.project.join("compose.yaml")).unwrap();
    // The demo binds 127.0.0.1:8080 — a clean doctor must stay quiet.
    let (_, quiet) = doctor_output(&ws, &server);
    assert!(
        !quiet.contains("on EVERY interface"),
        "a loopback bind must not be reported as exposure:\n{quiet}"
    );

    let exposed = compose.replace("127.0.0.1:8080:80", "18081:80");
    assert_ne!(exposed, compose, "compose fixture changed shape");
    std::fs::write(ws.project.join("compose.yaml"), &exposed).unwrap();
    let (_, text) = doctor_output(&ws, &server);
    assert!(
        text.contains("on EVERY interface") && text.contains("18081"),
        "a port published on all interfaces must be called out.\ncompose:\n{exposed}\ndoctor:\n{text}"
    );

    ws.forget_on_server(&server);
}

/// A writable bind mount is not the same thing as data the container
/// owns. A tracked FILE must never get a protect line — protecting it
/// would stop it being pushed and break the stack.
#[test]
fn a_writable_file_gets_the_caveat_and_never_a_protect_line() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = demo_workspace(&server);

    let compose = std::fs::read_to_string(ws.project.join("compose.yaml")).unwrap();
    ws.write("seed.sql", "-- init\n");
    std::fs::write(
        ws.project.join("compose.yaml"),
        compose.replace(
            "      - ./bind:/mnt/bind:ro",
            "      - ./bind:/mnt/bind:ro\n      - ./seed.sql:/docker-entrypoint-initdb.d/seed.sql",
        ),
    )
    .unwrap();

    let (_, text) = doctor_output(&ws, &server);
    assert!(
        !text.contains("\"seed.sql\""),
        "a tracked writable FILE must never be suggested for protect:\n{text}"
    );
    assert!(
        text.contains("also hold YOUR files") && text.contains("seed.sql"),
        "it should get the caveat instead:\n{text}"
    );

    ws.forget_on_server(&server);
}

/// Broken compose syntax: the error must clearly blame the compose file
/// (resolved on the server), not ulak's transport.
#[test]
fn broken_compose_blames_the_compose_file_and_not_the_transport() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = demo_workspace(&server);

    std::fs::write(
        ws.project.join("compose.yaml"),
        "services:\n  web:\n    image: [this is not\n  valid yaml at all\n",
    )
    .unwrap();

    let (ok, text) = doctor_output(&ws, &server);
    assert!(!ok, "broken compose must fail doctor:\n{text}");
    assert!(
        text.contains("docker compose config") && text.contains("rejected them"),
        "blame must land on the compose model, not the transport:\n{text}"
    );
    assert!(
        text.contains("fix the compose file first"),
        "must tell the user how to attribute the failure:\n{text}"
    );

    ws.forget_on_server(&server);
}

/// Doctor puts files on the server before it can answer anything, and
/// for a long time it CLAIMED none of them.
///
/// The pre-flight push carries the compose files and whatever they name,
/// which is a real transfer into the real workspace. A file that arrives
/// unclaimed is one the ledger has never heard of, and the ledger is the
/// only thing that can ever mark it doomed — so deleting it here left it
/// on the server for good, and the next pull carried it home again.
///
/// The CONSEQUENCE is what this asserts, because that is the half a unit
/// test cannot reach: doctor, then delete, then sync, and the file has to
/// be gone from the server without reappearing in the repo. `push_no_delete`
/// needs a real rsync destination, so nothing short of this proves it.
#[test]
fn a_file_doctor_pushed_can_still_be_retired() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = demo_workspace(&server);
    // Under `site/`, which the demo bind-mounts, so it is part of the
    // footprint doctor pushes rather than something a later sync
    // discovers on its own.
    ws.write("site/only-doctor-sent-this.html", "PUSHED-BY-DOCTOR\n");

    let (ok, text) = doctor_output(&ws, &server);
    assert!(ok, "doctor must pass on the demo project:\n{text}");

    let ids = ws.workspace_ids();
    let id = ids.first().expect("doctor must register the workspace");
    let root = ws.remote_workspace_root(id);
    let landed = format!("{root}/proj/site/only-doctor-sent-this.html");
    assert!(
        server.ssh(&format!("test -e {landed}")).status.success(),
        "doctor's pre-flight push never carried the file, so there is nothing here \
         that could be stranded"
    );

    // No sync in between: whatever claim exists is the one doctor made.
    std::fs::remove_file(ws.project.join("site/only-doctor-sent-this.html")).unwrap();
    ws.ulak(&server).arg("sync").assert().success();

    assert!(
        server.ssh(&format!("test ! -e {landed}")).status.success(),
        "a file doctor put on the server can never be removed — the ledger never \
         heard of it. The workspace holds:\n{}",
        String::from_utf8_lossy(&server.ssh(&format!("ls -a {root}/proj/site")).stdout)
    );
    assert!(
        !ws.project.join("site/only-doctor-sent-this.html").exists(),
        "the deletion was undone by the pull, so it can never be repeated"
    );

    ws.forget_on_server(&server);
}
