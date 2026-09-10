//! Phase 3 e2e: passthrough, project identity, manifest, lock, audit —
//! and the 5-mechanism prober actually RUNNING on the server, which is
//! the product's entire reason to exist (DoD: 5/5 in CI).

mod common;

use std::time::{Duration, Instant};

use common::{Chain, TestServer, Workspace, extract_remote_dir};

const ULAK_RESERVED: &[&str] = &[
    "init", "doctor", "sync", "service", "status", "clean", "docker",
];

/// The scenarios, in order and by name, so a break says what fell over
/// and what never got the chance to. They share one running stack and
/// one workspace identity, so splitting them would either race or
/// re-impose this order through the back door.
const CHAIN: &[&str] = &[
    "the five mechanisms all answer from the server",
    "the project identity is deterministic",
    "the manifest holds that identity",
    "exit codes and stdio stream through",
    "a reserved name still has an escape hatch",
    "the audit trail is written",
    "a user's project name reaches every command",
    "an attached project name reaches every command",
    "two projects of the same name do not collide",
];

#[test]
fn phase3_passthrough_identity_audit() {
    let Some(server) = TestServer::start() else {
        return;
    };
    // The demo fixture's two bases: nginx serves the bind-mounted site,
    // and the prober's Dockerfile is `FROM alpine:3.20`. The scenarios
    // further down add their own one-service stacks on alpine too.
    server.needs_image("nginx:alpine");
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
    ws.use_demo_fixture();
    ws.set_host(&server.alias);
    ws.write("ulak.toml", "[sync]\nprotect = [\"data/\"]\n");

    let mut chain = Chain::new(CHAIN);
    let remote_dir = chain.link(|| up_and_probe_all_five_mechanisms(&server, &ws));
    let identity = chain.link(|| identity_is_deterministic(&server, &ws));
    chain.link(|| manifest_holds_identity(&server, &ws, &remote_dir, &identity));
    chain.link(|| exit_codes_and_stdio_stream(&server, &ws));
    chain.link(|| reserved_names_have_escape_hatch(&server, &ws));
    chain.link(|| audit_trail_written(&ws));
    chain.link(|| user_project_name_reaches_every_command(&server));
    chain.link(|| an_attached_project_name_reaches_every_command(&server));
    let ws2 = chain.link(|| same_name_projects_do_not_collide(&server, &identity));
    chain.finish();

    // Teardown: both stacks down, images gone, workspaces gone.
    ws.ulak(&server)
        .args(["docker", "compose", "down", "-v", "--rmi", "local"])
        .assert()
        .success();
    ws2.ulak(&server)
        .args(["docker", "compose", "down", "-v", "--rmi", "local"])
        .assert()
        .success();
    for w in [&ws, &ws2] {
        let out = w.ulak(&server).arg("sync").output().unwrap();
        if let Some(dir) = extract_remote_dir(&String::from_utf8_lossy(&out.stderr))
            && let Some((hash_dir, _)) = dir.rsplit_once('/')
        {
            server.ssh(&format!("rm -rf {hash_dir}"));
        }
    }
    if server.is_real_host() {
        // Leave the expendable server as we found it.
        server.ssh("docker rmi nginx:alpine alpine:3.20 >/dev/null 2>&1; true");
    }
}

/// `ulak docker compose up -d --build` then read the prober's own report: all five
/// local-reference mechanisms must be alive on the server.
fn up_and_probe_all_five_mechanisms(server: &TestServer, ws: &Workspace) -> String {
    let out = ws
        .ulak(server)
        .args(["docker", "compose", "up", "-d", "--build"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "up -d --build failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The prober reports once and then idles; poll its logs until the
    // report is in.
    let deadline = Instant::now() + Duration::from_secs(60);
    let logs = loop {
        let out = ws
            .ulak(server)
            .args(["docker", "compose", "logs", "prober"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if text.contains("5. writable") {
            break text;
        }
        assert!(
            Instant::now() < deadline,
            "prober report never appeared; last logs:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(500));
    };

    for mechanism in [
        "1. env_file (./.env)",
        "2. build context (./app)",
        "3. bind mount (./bind)",
        "4. configs file (./configs)",
    ] {
        let line = logs
            .lines()
            .find(|l| l.contains(mechanism))
            .unwrap_or_else(|| panic!("no line for {mechanism:?} in:\n{logs}"));
        assert!(line.contains("OK"), "mechanism broken: {line}");
    }
    assert!(
        logs.contains("lines"),
        "writable-mount mechanism (5) missing:\n{logs}"
    );
    assert!(
        !logs.contains("BROKEN") && !logs.contains("MISSING "),
        "a mechanism reported broken:\n{logs}"
    );

    // Mechanism 5 from the web side: the bind-mounted site is really
    // what nginx serves.
    let html = ws
        .ulak(server)
        .args([
            "docker",
            "compose",
            "exec",
            "-T",
            "web",
            "cat",
            "/usr/share/nginx/html/index.html",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&html.stdout).contains("BIND-MOUNT-OK"),
        "bind-mounted site content not visible in the container"
    );

    let out = ws.ulak(server).arg("sync").output().unwrap();
    extract_remote_dir(&String::from_utf8_lossy(&out.stderr)).expect("remote dir")
}

/// The project on the server is the one COMPOSE resolves — here, the
/// `name: workspace-demo` the fixture's own compose.yaml declares.
///
/// That line has been in the fixture the whole time and ulak used to
/// walk straight past it: docker's rung 4 was never implemented, and
/// every call carried a `-p <dir>-<12 hex>` of ulak's own making that
/// overruled it. Both are gone. What proves the rung works is that this
/// name appears at all — nothing local computes `workspace-demo`, so it
/// can only have come off `docker compose config` on the server.
fn identity_is_deterministic(server: &TestServer, _ws: &Workspace) -> String {
    let ls = server.ssh("docker compose ls --all");
    let text = String::from_utf8_lossy(&ls.stdout).to_string();
    let found = text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .find(|name| *name == "workspace-demo")
        .unwrap_or_else(|| {
            panic!(
                "the fixture declares `name: workspace-demo`; compose ls shows no such \
                 project, so a name of ulak's own overruled the file's:\n{text}"
            )
        });
    found.to_string()
}

fn manifest_holds_identity(server: &TestServer, ws: &Workspace, remote_dir: &str, identity: &str) {
    let hash_dir = remote_dir.rsplit_once('/').unwrap().0;
    let cat = server.ssh(&format!("cat {hash_dir}/manifest.json"));
    let manifest: serde_json::Value =
        serde_json::from_slice(&cat.stdout).expect("manifest is JSON");
    assert_eq!(manifest["identity"].as_str().unwrap(), identity);
    let uuid1 = manifest["uuid"].as_str().unwrap().to_string();
    assert_eq!(uuid1.len(), 36, "uuid must be a v4 uuid");

    // The uuid is the permanent identity: a second sync must keep it.
    ws.ulak(server).arg("sync").assert().success();
    let cat = server.ssh(&format!("cat {hash_dir}/manifest.json"));
    let manifest: serde_json::Value =
        serde_json::from_slice(&cat.stdout).expect("manifest is JSON");
    assert_eq!(manifest["uuid"].as_str().unwrap(), uuid1);
}

/// A user `-p` is CAPTURED — it reaches the server as the project, and
/// every ulak-side answer about that stack is the same one docker would
/// give.
///
/// The original bug was that ulak's own follow-ups recomputed an
/// identity of their own making, so `down` printed success while the
/// stack kept running. What replaced it is not a memory: the invocation
/// is captured once and docker's cascade is followed, which means the
/// flag has to be on the follow-up too — exactly as with plain docker.
///
/// The second half pins that faithfulness the only way it can be
/// pinned, by asserting the thing a memory would break. Measured on
/// Compose v5.3.1: `compose -p chosen up -d` followed by a bare
/// `compose down` in the same directory removes nothing of `chosen`,
/// because the bare command resolves the directory's own name instead.
/// If that assertion ever goes red, someone has taught ulak to remember
/// a `-p` again and it has stopped being docker.
fn user_project_name_reaches_every_command(server: &TestServer) {
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws.write(
        "compose.yaml",
        "services:\n  idle:\n    image: alpine:3.20\n    network_mode: \"none\"\n    \
         command: [\"sleep\", \"600\"]\n",
    );
    let chosen = format!("ulak-e2e-chosen-{}", std::process::id());

    ws.ulak(server)
        .args(["docker", "compose", "-p", &chosen, "up", "-d"])
        .assert()
        .success();
    let up = server.ssh(&format!("docker compose -p {chosen} ps -q"));
    assert!(
        !String::from_utf8_lossy(&up.stdout).trim().is_empty(),
        "the user's -p never reached the server"
    );

    // Told the name again, `status` answers about that stack.
    let status = ws
        .ulak(server)
        .args(["docker", "compose", "-p", &chosen])
        .arg("ps")
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&status.stdout).to_string();
    assert!(
        shown.contains("idle"),
        "the -p did not reach a read-only command:\n{shown}"
    );

    // Docker's own answer, and now ulak's: a command that does not carry
    // the name does not address that project.
    ws.ulak(server)
        .args(["docker", "compose", "down"])
        .assert()
        .success();
    let after = server.ssh(&format!("docker compose -p {chosen} ps -q"));
    assert!(
        !String::from_utf8_lossy(&after.stdout).trim().is_empty(),
        "a bare `down` stopped a project it was never told the name of — ulak has \
         started remembering a -p again, which docker does not do"
    );

    // …and the same command carrying the flag ends it, as it must.
    ws.ulak(server)
        .args(["docker", "compose", "-p", &chosen, "down"])
        .assert()
        .success();
    let after = server.ssh(&format!("docker compose -p {chosen} ps -q"));
    assert!(
        String::from_utf8_lossy(&after.stdout).trim().is_empty(),
        "down reported success while the containers kept running"
    );

    clean_up_workspace(server, &ws);
}

/// The same bug, one spelling over. pflag lets `-p` swallow its value —
/// `-papi` is `-p api` — and that word matched neither ulak's flag table
/// nor the check that catches an uncapturable global, so it travelled to
/// the server unread. Compose took the last `-p` and ran the user's
/// project while ulak kept its state under a name derived from the
/// directory: `up` started containers that ulak's own `down`, `status`
/// and background service could not then see.
///
/// Its own workspace, for the same reason the spaced spelling has one:
/// the project name decides the workspace identity, so a second name in
/// one directory is a different workspace and the server refuses it.
fn an_attached_project_name_reaches_every_command(server: &TestServer) {
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws.write(
        "compose.yaml",
        "services:\n  idle:\n    image: alpine:3.20\n    command: [\"sleep\", \"600\"]\n",
    );
    let chosen = format!("ulak-e2e-attached-{}", std::process::id());

    ws.ulak(server)
        .args(["docker", "compose", &format!("-p{chosen}"), "up", "-d"])
        .assert()
        .success();
    let up = server.ssh(&format!("docker compose -p {chosen} ps -q"));
    assert!(
        !String::from_utf8_lossy(&up.stdout).trim().is_empty(),
        "the attached -p never reached the server"
    );

    // Uncaptured, this word travelled to the server unread while ulak
    // keyed its own state to the directory's name — so compose ran the
    // user's project and ulak's `down` addressed a different one. Every
    // command that carries the flag, in either spelling, has to reach
    // the same place.
    let ls = ws
        .ulak(server)
        .args(["docker", "compose", &format!("-p{chosen}"), "ps"])
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&ls.stdout).to_string();
    assert!(
        shown.contains("idle"),
        "the attached -p was not captured on a read-only command:\n{shown}"
    );

    ws.ulak(server)
        .args(["docker", "compose", &format!("-p{chosen}"), "down"])
        .assert()
        .success();
    let after = server.ssh(&format!("docker compose -p {chosen} ps -q"));
    assert!(
        String::from_utf8_lossy(&after.stdout).trim().is_empty(),
        "down reported success while the containers kept running"
    );

    clean_up_workspace(server, &ws);
}

/// Take the workspace this scenario made back off the server, so the
/// next run of the suite meets a clean one.
fn clean_up_workspace(server: &TestServer, ws: &Workspace) {
    let out = ws.ulak(server).arg("sync").output().unwrap();
    if let Some(dir) = extract_remote_dir(&String::from_utf8_lossy(&out.stderr))
        && let Some((hash_dir, _)) = dir.rsplit_once('/')
    {
        server.ssh(&format!("rm -rf {hash_dir}"));
    }
}

fn exit_codes_and_stdio_stream(server: &TestServer, ws: &Workspace) {
    // Exit code propagation, verbatim.
    let out = ws
        .ulak(server)
        .args([
            "docker", "compose", "exec", "-T", "web", "sh", "-c", "exit 7",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(7),
        "compose exit code must pass through"
    );

    // A `-p` that belongs to the CONTAINER command must not hijack the
    // project identity (a confirmed trap: `psql -p 5432`).
    ws.ulak(server)
        .args(["docker", "compose", "exec", "-T", "web", "ls", "-p", "/tmp"])
        .assert()
        .success();

    // stdin flows over ssh into the container; stdout flows back.
    ws.ulak(server)
        .args([
            "docker",
            "compose",
            "exec",
            "-T",
            "web",
            "sh",
            "-c",
            "cat > /tmp/e2e-stdin",
        ])
        .write_stdin("hello-through-the-pipe")
        .assert()
        .success();
    let out = ws
        .ulak(server)
        .args([
            "docker",
            "compose",
            "exec",
            "-T",
            "web",
            "cat",
            "/tmp/e2e-stdin",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello-through-the-pipe"
    );
}

/// A compose command with the same name as a ulak root command remains
/// reachable because the two trees are explicitly namespaced.
fn reserved_names_have_escape_hatch(server: &TestServer, ws: &Workspace) {
    let help = server.ssh("docker compose --help");
    let text = String::from_utf8_lossy(&help.stdout).to_string();
    // Collect entries under EVERY "…Commands:" header ("Management
    // Commands:" and "Commands:" both hold invocable names).
    let mut compose_commands: Vec<&str> = Vec::new();
    let mut in_section = false;
    for line in text.lines() {
        if line.trim_end().ends_with("Commands:") {
            in_section = true;
        } else if line.trim().is_empty() {
            in_section = false;
        } else if in_section
            && line.starts_with(' ')
            && let Some(word) = line.split_whitespace().next()
        {
            compose_commands.push(word);
        }
    }
    assert!(
        compose_commands.len() > 10,
        "could not parse compose command list from:\n{text}"
    );

    // Measured against Compose v5.1.2: this intersection is EMPTY — not
    // one of the eight reserved words is a Compose subcommand. So the
    // loop that used to stand here ran zero times, and the scenario
    // reported green having never once evaluated the claim its own name
    // makes. An assertion instead of a loop: the day Compose ships a
    // `status` or a `clean`, this goes red and asks for the round trip
    // to be written, rather than quietly continuing to prove nothing.
    let collisions: Vec<&&str> = compose_commands
        .iter()
        .filter(|c| ULAK_RESERVED.contains(*c))
        .collect();
    assert!(
        collisions.is_empty(),
        "Compose now ships {collisions:?}, which Ulak also uses at its root — add the \
         round trip here and assert that `ulak docker compose <name>` still reaches \
         Compose:\n{text}"
    );

    // The collision that IS live, and the only reason this scenario has
    // anything to run: `service` is both `ulak service` (the root
    // command that manages the background daemon) and Docker's own Swarm
    // family. Below `ulak docker` it must be Docker's, or the namespace
    // buys nothing.
    //
    // A non-manager daemon refuses with a Swarm diagnostic; a manager
    // answers with Docker's service table. Both prove the word reached
    // Docker's family. What must NOT come back is Ulak's own `service`
    // command, which would mean the word was read at the root and never
    // left this machine.
    let out = ws
        .ulak(server)
        .args(["docker", "service", "ls"])
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let docker_answered = said.contains("swarm")
        || said.contains("Swarm")
        || (out.status.success() && said.contains("REPLICAS") && said.contains("IMAGE"));
    assert!(
        docker_answered,
        "`ulak docker service ls` did not reach Docker's Swarm family — the reserved \
         root word swallowed it:\n{said}"
    );
    assert!(
        !said.contains("service run") && !said.contains("service install"),
        "`ulak docker service ls` was answered by Ulak's OWN service command:\n{said}"
    );
}

/// EVERY trail this sandbox wrote, not the first one `read_dir` happens
/// to hand back.
///
/// One project directory is more than one workspace id: a compose
/// invocation hashes its compose FILE, while `ulak docker <anything>`
/// hashes the project root. A daemon-routed command syncs nothing, so
/// its trail holds `ssh` and no `rsync` — and with `find()` picking by
/// directory order, whether this scenario passed depended on which of
/// the two the filesystem listed first. It was a coin flip that had been
/// landing the same way.
///
/// Every line of every trail is still parsed and stamped: what is
/// relaxed is WHICH trail the two kinds have to appear in, not whether
/// the trail is well-formed.
fn audit_trail_written(ws: &Workspace) {
    let state = ws.home.join(".local/state/ulak");
    let trails: Vec<std::path::PathBuf> = std::fs::read_dir(&state)
        .expect("state dir exists")
        .flatten()
        .map(|e| e.path().join("commands.jsonl"))
        .filter(|p| p.is_file())
        .collect();
    assert!(
        !trails.is_empty(),
        "no workspace under {} wrote a trail at all",
        state.display()
    );
    let mut kinds = std::collections::BTreeSet::new();
    for trail in &trails {
        for line in std::fs::read_to_string(trail).unwrap().lines() {
            let entry: serde_json::Value = serde_json::from_str(line).expect("valid jsonl");
            assert!(entry["ts_unix"].as_u64().is_some());
            kinds.insert(entry["kind"].as_str().unwrap().to_string());
        }
    }
    assert!(
        kinds.contains("ssh"),
        "audit must record ssh commands; {trails:?} hold {kinds:?}"
    );
    assert!(
        kinds.contains("rsync"),
        "audit must record rsync commands; {trails:?} hold {kinds:?}"
    );
}

/// Two different checkouts with the SAME basename on the same server —
/// and the two halves of what that means, which are not the same half.
///
/// The FILES stay apart, and that is ulak's own doing: a workspace is
/// located by the hash of the first compose file's absolute path, so
/// two checkouts called `proj` get two workspaces and neither can see
/// the other's tree.
///
/// The CONTAINERS do not, and that is docker's. Measured on Compose
/// v5.3.1 with no ulak involved: `up -d` in `a/api` and then in `b/api`
/// leaves ONE project called `api` holding the first checkout's
/// container, `docker compose ls` still naming the first checkout's
/// file — and a `down` in `b/api` removes it. Docker's answer to that
/// is to name the project, and ulak honours every way docker offers of
/// doing so (`-p`, `COMPOSE_PROJECT_NAME`, `name:`).
///
/// Ulak used to answer it instead, by appending a hash to every derived
/// name. That bought this isolation and cost the case where a project's
/// directory IS its `-p`: those two lines agree under plain docker and
/// could never agree under a hashed name. The trade was reversed
/// deliberately — this scenario is what records it, so the next person
/// to wonder why finds the measurement rather than the hash.
fn same_name_projects_do_not_collide(server: &TestServer, first_identity: &str) -> Workspace {
    let ws2 = Workspace::new(); // also named "proj"
    ws2.set_host(&server.alias);
    ws2.write(
        "compose.yaml",
        "services:\n  sleeper:\n    image: alpine:3.20\n    network_mode: \"none\"\n    \
         command: [\"sleep\", \"60\"]\n",
    );
    ws2.ulak(server)
        .args(["docker", "compose", "up", "-d"])
        .assert()
        .success();

    // The first checkout named itself in its compose file, so nothing of
    // this touches it.
    let ls = server.ssh("docker compose ls --all");
    let text = String::from_utf8_lossy(&ls.stdout).to_string();
    assert!(
        text.lines()
            .filter_map(|l| l.split_whitespace().next())
            .any(|n| n == first_identity),
        "the second checkout took over the first one's project:\n{text}"
    );

    // Two workspaces on the server, one per checkout: what travels is
    // still keyed on the path, whatever the containers are called.
    let ids: std::collections::BTreeSet<String> = ws2.workspace_ids().into_iter().collect();
    assert!(
        !ids.is_empty(),
        "the second checkout got no workspace of its own"
    );
    ws2
}
