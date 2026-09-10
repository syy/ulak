//! Phase 4 e2e, rewritten for the service: the intent files, the ports
//! coming home with no command typed, saves reaching the server, the
//! link dying and healing by itself, and clean's resistance rules.
//!
//! What this file replaces is `ulak watch` + `ulak forward` +
//! `ulak dev`. Not one of the scenarios below types a command to make
//! syncing or forwarding happen — that is the whole claim, and the shape
//! of the test is the claim's evidence.
//!
//! It is one `#[test]` because the scenarios genuinely hand each other
//! one running stack, and it reports a MAP rather than one verdict —
//! see `CHAIN` below.

mod common;

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use common::{Chain, Service, TestServer, Workspace, desired, extract_remote_dir, service_status};

/// The scenarios, in the order they run and by name.
///
/// They cannot be split: they hand each other one stack, one service and
/// one workspace, and the ones that take the stack down put it back
/// before they return. What splitting would have bought is bought by
/// `Chain` instead — a break names the scenario it happened in and lists
/// the ones that never got the chance to run. Before that, fourteen
/// claims reported one verdict and everything queued behind a failure
/// left no trace at all, indistinguishable from a scenario that ran and
/// passed.
const CHAIN: &[&str] = &[
    "the intent file follows up and down",
    "a failed up does not claim to be live",
    "a busy port is named, not skipped",
    "the ports come home on their own",
    "saves reach the server",
    "status shows what the service did",
    "a tunnel with nobody behind it says so",
    "the link heals without anyone typing",
    "a key the server will not take is said out loud",
    "a healthy service interrupts nobody",
    "the ports come home right after up",
    "an ordinary command says the service is gone",
    "status outside a project shows the machine",
    "half-sent files never come home",
    "clean resists, then cleans",
];

/// The demo fixture's own page — proof that what answered on localhost
/// is the server's nginx and not something of ours.
const DEMO_PAGE: &str = "BIND-MOUNT-OK";
/// What `site/index.html` says once the save scenarios have run. The
/// tunnel has to serve THAT, or it is pointing at a stale copy.
const LAST_EDIT: &str = "after-a-concurrent-up";

/// How long a save is given to reach the server, or a deletion to leave
/// it. Deliberately DOUBLE the service's own idle reconcile interval
/// (`service::IDLE_RECONCILE`, 60 s), because the two used to be equal:
/// a file event that the watcher misses is then owed to the next idle
/// tick, which lands exactly ON the deadline — a coin flip written into
/// the suite. Measured: it came up tails twice. The claim these
/// scenarios make is "it happens with nothing typed", and that claim
/// does not depend on the number being 60.
const SETTLE: u64 = 120;

/// Held back for a day by an `#[ignore]`, and not because it was flaky:
/// `scenario_saves_reach_the_server` was right and the product was
/// wrong. See `sync::hidden_by_the_quick_check` for the hole and the
/// fix. The line took the thirteen other scenarios on this chain out of
/// CI with it, which is why it is gone rather than annotated.
#[test]
fn phase5_the_service_keeps_the_stack_alive() {
    let Some(server) = TestServer::start() else {
        return;
    };
    // The demo fixture's two bases: nginx is what answers on the
    // forwarded port, and the prober's Dockerfile is `FROM alpine:3.20`.
    server.needs_image("nginx:alpine");
    server.needs_image("alpine:3.20");
    // Not a base for anything in the fixture — this one belongs to the
    // PRODUCT. `commands::clean` falls back to `docker run --rm …
    // busybox sh -c 'rm -rf …'` when a container has written root-owned
    // files into a bind mount, which the demo stack does, so
    // `scenario_clean_resists_then_cleans` reaches for an image no
    // scenario ever names. Unseeded it was a Docker Hub pull in the
    // middle of an assertion — the one dependency left in this suite,
    // and a red run whenever the anonymous quota was spent.
    server.needs_image("busybox");
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
    ws.use_demo_fixture();
    ws.set_host(&server.alias);
    ws.write("ulak.toml", "[sync]\nprotect = [\"data/\"]\n");

    ws.ulak(&server)
        .args(["docker", "compose", "up", "-d", "--build"])
        .assert()
        .success();
    let out = ws.ulak(&server).arg("sync").output().unwrap();
    let remote_dir = extract_remote_dir(&String::from_utf8_lossy(&out.stderr)).expect("remote dir");

    let mut chain = Chain::new(CHAIN);
    chain.link(|| scenario_intent_follows_up_and_down(&server, &ws));
    chain.link(|| scenario_a_failed_up_does_not_claim_to_be_live(&server));

    // Before the real session, because it needs to hold the one port the
    // demo publishes.
    chain.link(|| scenario_a_busy_port_is_named_not_skipped(&server, &ws));

    {
        let service = Service::start(&ws, &server);
        chain.link(|| scenario_the_ports_come_home_on_their_own(&server, &ws, &service));
        chain.link(|| scenario_saves_reach_the_server(&server, &ws, &service, &remote_dir));
        chain.link(|| scenario_status_shows_what_the_service_did(&server, &ws));
        chain.link(|| scenario_a_tunnel_with_nobody_behind_it_says_so(&server, &ws, &service));
        chain.link(|| {
            scenario_the_link_heals_without_anyone_typing(&server, &ws, &service, &remote_dir)
        });
        chain.link(|| {
            scenario_a_key_the_server_will_not_take_is_said_out_loud(&server, &ws, &service)
        });
        chain.link(|| scenario_a_healthy_service_interrupts_nobody(&server, &ws));
        // Last, because it is the only scenario that takes the stack
        // down under the service — and it puts it back up before it
        // returns, which is what the scenarios below still assume.
        chain.link(|| scenario_the_ports_come_home_right_after_up(&server, &ws, &service));
    }
    chain.link(|| scenario_an_ordinary_command_says_the_service_is_gone(&server, &ws));
    chain.link(|| scenario_status_outside_a_project_shows_the_machine(&server, &ws));

    chain.link(|| scenario_half_sent_files_never_come_home(&server, &ws, &remote_dir));
    chain.link(|| scenario_clean_resists_then_cleans(&server, &ws, &remote_dir));
    chain.finish();

    if server.is_real_host() {
        server.ssh("docker rmi nginx:alpine alpine:3.20 >/dev/null 2>&1; true");
    }
}

/// Docker's rule, written to disk: `up` means live until `down`. The
/// service reads nothing else to decide whether a Docker stack is its
/// business, so this file is the whole handover — and the fields have
/// to be the RESOLVED invocation, not the argv someone typed: relative
/// paths have to rebuild into the same project when the service is no
/// longer standing in the declaring shell.
fn scenario_intent_follows_up_and_down(server: &TestServer, ws: &Workspace) {
    let d = desired(ws).expect("`up -d --build` must have declared an intent");
    assert_eq!(d["schema"], 3);
    assert_eq!(d["live"], true, "up means live: {d}");
    assert_eq!(d["destination"], server.alias);
    assert!(d["identity"].as_str().is_some_and(|name| !name.is_empty()));
    assert!(
        d["workspace_id"].as_str().is_some_and(|id| !id.is_empty()),
        "the sync workspace used by this stack must be pinned: {d}"
    );
    assert!(
        d["workspace_namespace"]
            .as_str()
            .is_some_and(|namespace| !namespace.is_empty()),
        "the client namespace used by this stack must be pinned: {d}"
    );
    let globals = d["argv_globals"].as_array().expect("argv_globals");
    let globals: Vec<&str> = globals.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        globals.iter().any(|g| g.ends_with("compose.yaml")),
        "the compose files must be absolute and named: {globals:?}"
    );
    assert!(
        globals
            .iter()
            .all(|g| !g.starts_with("./") && !g.contains("..")),
        "a relative path only means something next to the cwd it was typed in: {globals:?}"
    );
    assert!(
        !globals.contains(&"up") && !globals.contains(&"-d"),
        "the intent is a state, not a command to replay: {globals:?}"
    );

    // `down` ends it, and nothing else does.
    ws.ulak(server)
        .args(["docker", "compose", "down"])
        .assert()
        .success();
    let d = desired(ws).expect("intent must survive down");
    assert_eq!(d["live"], false, "down means not live: {d}");

    // Back up for the scenarios that follow.
    ws.ulak(server)
        .args(["docker", "compose", "up", "-d"])
        .assert()
        .success();
    assert_eq!(desired(ws).unwrap()["live"], true);
}

/// The flag is written BEFORE the command, so a stack that dies halfway
/// is still looked after. The other side of that: a stack that never
/// came up at all must not be left looking live, or the service would
/// maintain a workspace with nothing behind it — forever, since the intent
/// deliberately has no TTL.
fn scenario_a_failed_up_does_not_claim_to_be_live(server: &TestServer) {
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws.write(
        "compose.yaml",
        "services:\n  ghost:\n    image: ulak-no-such-image-v0:missing\n",
    );

    let out = ws
        .ulak(server)
        .args(["docker", "compose", "up", "-d"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "the fixture image must not exist:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let d = desired(&ws).expect("the intent is written before the command, so it must be there");
    assert_eq!(
        d["live"], false,
        "a failed up must put the flag back where it found it: {d}"
    );

    // Cleaned by workspace id rather than by parsing a path out of a
    // command's output: the scenarios that most need cleaning up after
    // are exactly the ones where a command failed.
    for id in ws.workspace_ids() {
        server.ssh(&format!("rm -rf {}", ws.remote_workspace_root(&id)));
    }

    // The same lie, reached before compose ever runs. The flag is
    // written ahead of the sync, and the sync can fail: an unreachable
    // server here, a deletion budget declined at the prompt in real
    // life. Only a compose that got as far as an exit status used to
    // put it back, so this way in left the workspace declared live with
    // nothing behind it — and the intent has no TTL, so it stayed that
    // way.
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws.write(
        "compose.yaml",
        "services:\n  idle:\n    image: alpine:3.20\n    command: [\"sleep\", \"600\"]\n",
    );
    // One command that works first: with a cold footprint cache `bind`
    // does its own round trip and fails before the flag is ever
    // written, which is the case that was already safe.
    ws.ulak(server)
        .args(["docker", "compose", "config"])
        .assert()
        .success();

    server.break_link();
    let out = ws
        .ulak(server)
        .args(["docker", "compose", "up", "-d"])
        .output()
        .unwrap();
    let d = desired(&ws);
    server.heal_link();

    assert!(
        !out.status.success(),
        "the link was down, so the sync had to fail:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let d = d.expect("the intent is written before the sync, so it must be there");
    assert_eq!(
        d["live"], false,
        "an up that died at the sync left the stack looking live: {d}"
    );

    for id in ws.workspace_ids() {
        server.ssh(&format!("rm -rf {}", ws.remote_workspace_root(&id)));
    }
}

/// The measured lie, and this product's own reason to exist turned on
/// itself: without `ExitOnForwardFailure=yes`, ssh SKIPS a forward whose
/// local port is taken and exits 0 — after ulak has announced
/// "localhost:8080 → server:8080". Your browser then talks to whatever
/// is running locally while you believe you are on the server.
///
/// A service cannot answer that with an exit code, so it answers with
/// the two things a service has: it says so, and `status` shows it. What
/// it must NEVER do is stay quiet.
fn scenario_a_busy_port_is_named_not_skipped(server: &TestServer, ws: &Workspace) {
    // The scenario has to BE the thing holding 8080, and when it cannot
    // it says so out loud. It used to print a line and return, which is
    // wrong twice over: libtest captures stderr for a test that PASSES,
    // so the line went nowhere and a scenario named
    // "is_named_not_skipped" skipped itself in silence — and 8080 is the
    // very port the service is asked to bring home three scenarios
    // further down, so a machine that cannot lend it fails this suite
    // anyway, later and wearing a ulak-shaped mask.
    let holder = std::net::TcpListener::bind("127.0.0.1:8080").unwrap_or_else(|e| {
        panic!(
            "this scenario needs to hold local port 8080 itself, and could not: {e}\n\
             Find what has it and stop that, then rerun:  \
             lsof -nP -iTCP:8080 -sTCP:LISTEN"
        )
    });

    let service = Service::start(ws, server);
    let st = service.wait_status(ws, 90, "a tunnel it could not open", |st| {
        st["tunnels"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["port"] == 8080))
    });
    let tunnel = st["tunnels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["port"] == 8080)
        .unwrap()
        .clone();
    assert_eq!(
        tunnel["open"], false,
        "a port we could not take must be reported as not open, not omitted: {st}"
    );
    assert!(
        st["note"].as_str().is_some_and(|n| n.contains("8080")),
        "status must name the port that did not come home: {st}"
    );
    let said = service.said();
    assert!(
        said.contains("8080") && said.contains("NOT tunneled"),
        "the busy port must be named out loud, not skipped in silence:\n{said}"
    );
    assert!(
        said.contains("lsof"),
        "the user must be told how to find the holder:\n{said}"
    );

    // And when the holder goes away, the tunnel has to follow on its
    // own. This is the MIXED set — 18080 open, 8080 taken — and it is
    // the only one where the bug lived: `covers` counts a blocked port
    // as covered, so the plan looked satisfied and the service returned
    // before probing anything, while the child stayed alive and the
    // model never changed. Quitting whatever held the port, which is
    // how this ends every time, left ulak reporting NOT tunneled until
    // the stack cycled or the service was restarted.
    //
    // A single-port stack cannot show it: with every port blocked there
    // is no child, `died()` answers true, and the set is already
    // reopened every tick. That is why the demo publishes two.
    assert!(
        st["tunnels"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["port"] == 18080 && t["open"] == true)),
        "this needs the other port OPEN, or it is not the set that was stuck: {st}"
    );
    drop(holder);
    let st = service.wait_status(ws, 90, "the freed port tunneled", |st| {
        st["tunnels"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["port"] == 8080 && t["open"] == true))
    });
    assert!(
        st["tunnels"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["port"] == 8080 && t["open"] == true)),
        "the port was given back and the tunnel never followed: {st}"
    );

    drop(service);
}

/// `ulak docker compose up -d` was typed once, far above. Nothing since. The ports
/// have to be home anyway — that is the entire product claim, and this
/// is the assertion that carries it.
fn scenario_the_ports_come_home_on_their_own(
    server: &TestServer,
    ws: &Workspace,
    service: &Service,
) {
    service.wait_status(ws, 120, "an open tunnel on 8080", |st| {
        st["tunnels"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["port"] == 8080 && t["open"] == true))
    });
    assert_eq!(
        fetch(DEMO_PAGE, Duration::from_secs(30)),
        Some(true),
        "the tunnel never served the demo page:\n{}",
        service.said()
    );
    // And the link the service is reporting is the one it actually used.
    let st = service_status(ws).unwrap();
    assert_eq!(st["connection"], "up", "{st}");
    let _ = server;
}

/// Everything `ulak watch` used to be asked to do, with nobody
/// starting a watcher: debounced saves, the classic editor save
/// patterns, a directory rename, a deletion, and a `up` running
/// concurrently against the same per-workspace lock.
fn scenario_saves_reach_the_server(
    server: &TestServer,
    ws: &Workspace,
    service: &Service,
    remote_dir: &str,
) {
    // Rapid consecutive saves: the debounce coalesces, the final wins.
    for i in 1..=5 {
        ws.write("site/index.html", &format!("<h1>service-edit-v{i}</h1>"));
        std::thread::sleep(Duration::from_millis(50));
    }
    wait_remote(
        server,
        ws,
        remote_dir,
        "site/index.html",
        "service-edit-v5",
        SETTLE,
        service,
    );

    // vim: write a swap file, then rename it OVER the target.
    ws.write("site/.index.html.swp", "<h1>vim-swap-v6</h1>");
    std::fs::rename(
        ws.project.join("site/.index.html.swp"),
        ws.project.join("site/index.html"),
    )
    .unwrap();
    wait_remote(
        server,
        ws,
        remote_dir,
        "site/index.html",
        "vim-swap-v6",
        SETTLE,
        service,
    );

    // VSCode: write a temp sibling, then rename it over the target.
    ws.write("site/index.html.tmp42", "<h1>vscode-tmp-v7</h1>");
    std::fs::rename(
        ws.project.join("site/index.html.tmp42"),
        ws.project.join("site/index.html"),
    )
    .unwrap();
    wait_remote(
        server,
        ws,
        remote_dir,
        "site/index.html",
        "vscode-tmp-v7",
        SETTLE,
        service,
    );

    // Directory rename: new name appears, old name disappears. Inside
    // site/ because that is what the compose file references — under the
    // footprint model a directory nothing references is not synced.
    ws.write("site/renamed-dir-a/inner.txt", "dir-content");
    wait_remote(
        server,
        ws,
        remote_dir,
        "site/renamed-dir-a/inner.txt",
        "dir-content",
        SETTLE,
        service,
    );
    std::fs::rename(
        ws.project.join("site/renamed-dir-a"),
        ws.project.join("site/renamed-dir-b"),
    )
    .unwrap();
    wait_remote(
        server,
        ws,
        remote_dir,
        "site/renamed-dir-b/inner.txt",
        "dir-content",
        SETTLE,
        service,
    );
    wait_gone(
        server,
        ws,
        &format!("{remote_dir}/site/renamed-dir-a"),
        SETTLE,
        service,
    );

    // `up` while the service runs. The service never waits for the lock
    // — it skips the tick — so both sides come out healthy and the save
    // that triggered the skipped tick is not lost.
    ws.ulak(server)
        .args(["docker", "compose", "up", "-d"])
        .assert()
        .success();
    ws.write("site/index.html", "<h1>after-a-concurrent-up</h1>");
    wait_remote(
        server,
        ws,
        remote_dir,
        "site/index.html",
        "after-a-concurrent-up",
        SETTLE,
        service,
    );

    // A deletion flows through (within budget) with no prompt anywhere.
    ws.write("site/scratch.txt", "temp");
    wait_remote(
        server,
        ws,
        remote_dir,
        "site/scratch.txt",
        "temp",
        SETTLE,
        service,
    );
    std::fs::remove_file(ws.project.join("site/scratch.txt")).unwrap();
    wait_gone(
        server,
        ws,
        &format!("{remote_dir}/site/scratch.txt"),
        SETTLE,
        service,
    );

    // Server-owned data must have survived all that syncing.
    assert!(
        server
            .ssh(&format!("test -d {remote_dir}/data"))
            .status
            .success(),
        "protected data/ vanished while the service was running"
    );
}

/// `status` answers "is everything all right?" without the user having
/// to know a service exists.
fn scenario_status_shows_what_the_service_did(server: &TestServer, ws: &Workspace) {
    let out = ws.ulak(server).arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let all = format!("{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "status failed:\n{all}");
    for needle in ["project", "identity", "web"] {
        assert!(
            text.contains(needle),
            "status must show {needle:?}:\n{text}"
        );
    }
    assert!(
        text.contains("intent") && text.contains("live"),
        "status must say whether this stack is supposed to be up:\n{text}"
    );
    // The service's own line: connection, tunnels, last sync.
    assert!(
        text.contains("service") && text.contains("link up"),
        "status must report what the service sees:\n{text}"
    );
    // The ADDRESS, not a count. "2/3 tunnel(s)" said something was
    // reachable without saying where — which is the one thing a port
    // forward exists to answer, and the first question a user actually
    // asked of this command.
    assert!(
        text.contains("tunnels"),
        "status must list the ports that are home:\n{text}"
    );
    let port = crate::common::service_status(ws)
        .and_then(|st| {
            st["tunnels"]
                .as_array()?
                .iter()
                .find(|t| t["open"] == true)?["port"]
                .as_u64()
        })
        .expect("the service reported an open tunnel");
    assert!(
        text.contains(&format!("localhost:{port}")),
        "status must name the address you can actually connect to:\n{text}"
    );
}

/// The tunnel report's THIRD state, measured on a real server before it
/// existed: with two published ports and only one service started,
/// `ulak status` listed both as `localhost:PORT → svc`. The dead one
/// accepted a TCP connect in 0.28 ms and dropped it (curl exit 56,
/// HTTP 000, empty body) — correct ssh behaviour, `-L` binds eagerly
/// and dials lazily — and the user went to the server to debug a
/// service they had never started, while the `compose ps` a few lines
/// below showed it missing. One screen, contradicting itself.
///
/// Holding the port early stays right (nothing else keeps it safe until
/// the service starts), so what this pins is the REPORT: open with
/// nobody behind it must not read like open and answering. The demo's
/// portless prober keeps the stack alive while `web` — the only
/// port-publishing service — is stopped, which is the smallest way to
/// produce the state.
fn scenario_a_tunnel_with_nobody_behind_it_says_so(
    server: &TestServer,
    ws: &Workspace,
    service: &Service,
) {
    ws.ulak(server)
        .args(["docker", "compose", "stop", "web"])
        .assert()
        .success();
    let st = service.wait_status(ws, 90, "an open tunnel with no container behind it", |st| {
        st["tunnels"].as_array().is_some_and(|t| {
            t.iter()
                .any(|t| t["port"] == 8080 && t["open"] == true && t["service_running"] == false)
        })
    });
    // The engine's own truth must survive the report: both ports stay
    // HELD (open), only the backing is gone. Dropping them instead
    // would hand 8080 to whatever local process asks next.
    for port in [8080, 18080] {
        let t = st["tunnels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["port"] == port)
            .unwrap_or_else(|| panic!("port {port} missing from the report: {st}"));
        assert_eq!(t["open"], true, "{st}");
        assert_eq!(t["service_running"], false, "{st}");
    }

    // And the render the user reads says it, in words, on the same
    // screen that lists the stack's containers.
    let out = ws.ulak(server).arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "status failed:\n{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("no container is running for it"),
        "a dead service's port must not be listed the way a live one is:\n{text}"
    );

    // Start it again and the same report flips back — three seconds was
    // the measured recovery, with no tunnel rebuilt and nothing typed
    // beyond the start itself.
    ws.ulak(server)
        .args(["docker", "compose", "start", "web"])
        .assert()
        .success();
    service.wait_status(ws, 90, "the tunnel answered again", |st| {
        st["tunnels"].as_array().is_some_and(|t| {
            t.iter()
                .any(|t| t["port"] == 8080 && t["service_running"] == true)
        })
    });
    assert_eq!(
        fetch(LAST_EDIT, Duration::from_secs(30)),
        Some(true),
        "web is back, so the tunnel it never lost must serve it:\n{}",
        service.said()
    );
}

/// The scenario the product exists for, minus the laptop: cut ulak's
/// reach to the server, give it back, and type NOTHING. `localhost` has
/// to come back on its own.
///
/// The link is cut without touching the server (the harness swaps the
/// ssh config the binary under test uses), so this runs identically on
/// the dockerized fixture and against a real host — something the
/// connection axis had never done.
fn scenario_the_link_heals_without_anyone_typing(
    server: &TestServer,
    ws: &Workspace,
    service: &Service,
    remote_dir: &str,
) {
    // Asserted, not skipped on. Every scenario above has already had this
    // tunnel open and serving THIS edit, so a localhost that is not
    // serving it is a regression in them — not permission to stand down.
    // As a skip it took the whole connection axis (break it, notice it,
    // heal it) out of the run in silence, under a green tick, which is
    // the one failure a suite must never be able to have.
    assert_eq!(
        fetch(LAST_EDIT, Duration::from_secs(20)),
        Some(true),
        "localhost:8080 is not serving the last edit, so there is no working \
         tunnel here for the link to die under:\n{}",
        service.said()
    );

    server.break_link();
    // The service must NOTICE — silently carrying on with dead tunnels
    // is the failure this replaces.
    service.wait_status(ws, 180, "a link it knows is gone", |st| {
        st["connection"] == "down" || st["connection"] == "blocked"
    });
    assert_eq!(
        fetch(LAST_EDIT, Duration::from_secs(2)),
        None,
        "the tunnels must not survive the link they run over:\n{}",
        service.said()
    );

    let healed = Instant::now();
    server.heal_link();
    service.wait_status(ws, 180, "the link back up", |st| st["connection"] == "up");
    let back = fetch(LAST_EDIT, Duration::from_secs(60));
    assert_eq!(
        back,
        Some(true),
        "localhost never came back on its own:\n{}",
        service.said()
    );
    eprintln!(
        "recovery: localhost was back {:?} after the link healed",
        healed.elapsed()
    );

    // Files must flow again too — a connection that is "up" but no
    // longer syncing would be the old failure wearing a green label.
    ws.write("site/index.html", "<h1>after-the-outage</h1>");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if service_status(ws)
            .and_then(|st| st["last_sync_unix"].as_u64())
            .is_some_and(|t| t > 0)
            && String::from_utf8_lossy(
                &server
                    .ssh(&format!("cat {remote_dir}/site/index.html 2>/dev/null"))
                    .stdout,
            )
            .contains("after-the-outage")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the workspace never caught up after the link healed:\n{}",
            service.said()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The failure a background service is uniquely bad at: a key it cannot
/// use.
///
/// Measured for phase 4, because it never had been: with no terminal and
/// no agent, ssh does not ASK for a passphrase. It skips the key in
/// silence and leaves `Permission denied (publickey,password)` —
/// byte-for-byte what an unknown key leaves. So after a reboot, a user
/// whose keychain is not unlocked yet has a service that fails every
/// probe and climbs the backoff ladder without a word anywhere. The
/// answer is not to guess which of the two it is (nothing can), it is to
/// say both and name the command that fixes either.
fn scenario_a_key_the_server_will_not_take_is_said_out_loud(
    server: &TestServer,
    ws: &Workspace,
    service: &Service,
) {
    server.refuse_keys();
    let st = service.wait_status(ws, 120, "an authentication problem", |st| {
        st["note"].as_str().is_some_and(|n| n.contains("ssh-add"))
    });
    assert_eq!(
        st["connection"], "down",
        "a key the server will not take is a link that is down: {st}"
    );
    let note = st["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("no terminal"),
        "the note has to say WHY the service could not simply ask: {note}"
    );
    // And in the log, not only in a file somebody has to know to read.
    assert!(
        service.said().contains("ssh-add"),
        "the service must say it out loud too:\n{}",
        service.said()
    );

    // Unlocked again: nothing is typed, and it comes back on its own.
    server.heal_link();
    service.wait_status(ws, 180, "the link back up", |st| {
        st["connection"] == "up" && st["note"].is_null()
    });
}

/// An interrupted transfer leaves `.ulak-partial/` on the server —
/// that is `--partial-dir` working as intended, and it is how the next
/// sync resumes. rsync excludes that directory itself, but it appends
/// the rule at the END of the filter list, where the footprint's
/// `+ dir/***` has already matched. Measured: 8.1 MB of a half-sent
/// 30 MB file came back down into the user's repository.
fn scenario_half_sent_files_never_come_home(server: &TestServer, ws: &Workspace, remote_dir: &str) {
    server.ssh(&format!(
        "mkdir -p {remote_dir}/site/.ulak-partial && \
         printf 'half-a-file' > {remote_dir}/site/.ulak-partial/big.bin"
    ));
    assert!(
        server
            .ssh(&format!("test -f {remote_dir}/site/.ulak-partial/big.bin"))
            .status
            .success(),
        "fixture did not plant the partial blob"
    );

    ws.ulak(server).arg("sync").assert().success();

    assert!(
        !ws.project.join("site/.ulak-partial").exists(),
        "a half-sent blob came home into the user's repository"
    );
    // And the push must not have deleted the server's resume state
    // either — that is what makes an interrupted transfer resumable.
    assert!(
        server
            .ssh(&format!("test -f {remote_dir}/site/.ulak-partial/big.bin"))
            .status
            .success(),
        "the partial dir must be left alone on the server, not swept"
    );
    server.ssh(&format!("rm -rf {remote_dir}/site/.ulak-partial"));
}

fn scenario_clean_resists_then_cleans(server: &TestServer, ws: &Workspace, remote_dir: &str) {
    // 1. Running stack → clean refuses, points at down.
    let out = ws.ulak(server).arg("clean").output().unwrap();
    assert!(!out.status.success(), "clean must refuse while running");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        stderr.contains("ulak docker compose") && stderr.contains(" down"),
        "refusal must point at the exact stack's down command:\n{stderr}"
    );

    ws.ulak(server)
        .args(["docker", "compose", "down", "-v", "--rmi", "local"])
        .assert()
        .success();

    // 2. Protect paths + no tty → clean resists with guidance.
    let out = ws.ulak(server).arg("clean").output().unwrap();
    assert!(!out.status.success(), "clean must resist protect paths");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        stderr.contains("protect") || stderr.contains("aborted"),
        "resistance must explain protect:\n{stderr}"
    );

    // 3. Without protect, clean removes the whole hash dir.
    ws.write("ulak.toml", "[sync]\n");
    ws.ulak(server).arg("clean").assert().success();
    let hash_dir = remote_dir.rsplit_once('/').unwrap().0;
    assert!(
        server
            .ssh(&format!("test ! -e {hash_dir}"))
            .status
            .success(),
        "workspace hash dir must be gone after clean"
    );
}

// ─── helpers ────────────────────────────────────────────────────────

/// What `localhost:8080` is serving right now. `Some(true)` the page the
/// server's nginx has, `Some(false)` something answered but not that,
/// `None` nothing was listening at all — three answers, because "the
/// port is open but points nowhere" is exactly the state this product
/// exists to make impossible and must not read as success.
fn fetch(needle: &str, within: Duration) -> Option<bool> {
    let deadline = Instant::now() + within;
    loop {
        if let Ok(mut stream) = std::net::TcpStream::connect("127.0.0.1:8080") {
            // Short on purpose: a tunnel whose far end is gone accepts
            // the connection and then says nothing, and waiting that out
            // would push the service further up its backoff ladder
            // before the test even gets to healing the link.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = stream.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n");
            let mut buf = String::new();
            let _ = stream.read_to_string(&mut buf);
            if buf.contains(needle) && buf.contains("200 OK") {
                return Some(true);
            }
            if !buf.is_empty() {
                return Some(false);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn wait_remote(
    server: &TestServer,
    ws: &Workspace,
    remote_dir: &str,
    rel: &str,
    needle: &str,
    secs: u64,
    service: &Service,
) {
    let began = Instant::now();
    let deadline = began + Duration::from_secs(secs);
    loop {
        let out = server.ssh(&format!("cat {remote_dir}/{rel} 2>/dev/null"));
        if String::from_utf8_lossy(&out.stdout).contains(needle) {
            // `SETTLE` is 120 s and `service::IDLE_RECONCILE` is 60, so a
            // save the watcher lost entirely still lands inside the
            // deadline on the idle tick. Under a second says the watcher
            // carried it; over sixty says it did not.
            eprintln!(
                "  {rel} → {needle:?} after {:.1}s",
                began.elapsed().as_secs_f32()
            );
            return;
        }
        // The local half, because without it the two failures that end
        // up here are indistinguishable: a save that never travelled,
        // and a save that travelled and was then overwritten AT HOME by
        // the copy coming back. Read them side by side and it says
        // which.
        let local = std::fs::read_to_string(ws.project.join(rel))
            .unwrap_or_else(|e| format!("<unreadable: {e}>"));
        assert!(
            Instant::now() < deadline,
            "{rel} never contained {needle:?} on the server.\nlocal:  {local:?}\nremote: {:?}\nremote ls: {}\nevery rsync the service ran:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&server.ssh(&format!("ls -la {remote_dir}/site")).stdout),
            // One `up` here means the service reconciled once and then
            // stopped; several mean it kept trying and rsync decided
            // there was nothing to send. The two have nothing in common
            // except this assertion, and the trail is the only thing
            // that separates them.
            ws.rsync_trail(rel),
            service.said()
        );
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// A deletion that never lands is one of two very different bugs, and
/// the message has to say which. Either the path is still LOCAL — so it
/// is being pushed back up, and something recreated it (the rename race
/// phase 3 fixed once) — or it is gone locally and the server copy is
/// simply not being removed. Measured once with neither fact in hand,
/// which cost a whole run to learn nothing.
fn wait_gone(server: &TestServer, ws: &Workspace, path: &str, secs: u64, service: &Service) {
    let local = path
        .rsplit_once("/site/")
        .map(|(_, rel)| ws.project.join("site").join(rel));
    let name = path.rsplit('/').next().unwrap_or(path);
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !server.ssh(&format!("test ! -e {path}")).status.success() {
        assert!(
            Instant::now() < deadline,
            "{path} never disappeared from the server.\nlocally {}: {}\nlocally holds: {}\nserver has: {}\nledger claims it: {}\nrsync legs (dn = pull home, up = push):\n{}\n{}",
            local
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            local
                .as_ref()
                .map(|p| if p.exists() {
                    "STILL THERE — it is being pushed back up"
                } else {
                    "gone"
                })
                .unwrap_or("?"),
            local
                .as_ref()
                .and_then(|p| std::fs::read_dir(p).ok())
                .map(|d| {
                    let names: Vec<String> = d
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    if names.is_empty() {
                        "NOTHING — an empty directory".to_string()
                    } else {
                        names.join(", ")
                    }
                })
                .unwrap_or_else(|| "(not a directory)".to_string()),
            String::from_utf8_lossy(&server.ssh(&format!("ls -la {path} 2>&1 | head -8")).stdout),
            ws.workspace_ids()
                .iter()
                .flat_map(|id| {
                    std::fs::read_dir(ws.workspaces_dir().join(id).join("destinations"))
                        .into_iter()
                        .flatten()
                        .flatten()
                        .filter_map(|entry| {
                            std::fs::read_to_string(entry.path().join("ledger")).ok()
                        })
                })
                .any(|t| t.contains(name)),
            ws.rsync_trail(name),
            service.said()
        );
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// Silence is the default, and it is the half that decides whether the
/// other half gets read. On an ordinary day — service healthy, link up,
/// stack running — no command may say a word about the service.
fn scenario_a_healthy_service_interrupts_nobody(server: &TestServer, ws: &Workspace) {
    let out = ws
        .ulak(server)
        .args(["docker", "compose", "ps"])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    for noise in ["service", "not running", "not been able to reach"] {
        assert!(
            !said.contains(noise),
            "a healthy service must not announce itself on `ps`:\n{said}"
        );
    }
}

/// The gap a desktop notification was going to fill, filled where the
/// user already is.
///
/// Measured on this codebase before the fix: `status.json` had exactly
/// ONE reader, `ulak status`. So a service that had been unable to
/// reach the server for hours said nothing to somebody running `up -d`,
/// `logs` and `ps` all day — the file was written, and nobody read it.
/// `ulak status` is what you type once you already suspect something,
/// which is far too late to be the only place the answer lives.
fn scenario_an_ordinary_command_says_the_service_is_gone(server: &TestServer, ws: &Workspace) {
    // The stack is still declared live; what is gone is the service.
    // Removing the heartbeat is exactly what a machine that has never
    // started one looks like.
    let beat = ws.home.join(".local/state/ulak/service/heartbeat.json");
    let _ = std::fs::remove_file(&beat);

    let out = ws
        .ulak(server)
        .args(["docker", "compose", "ps"])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("no ulak service is running"),
        "an ordinary command must say what the service could not do:\n{said}"
    );
    assert!(
        said.contains("ulak service install"),
        "and carry the way out of it:\n{said}"
    );

    // A workspace nobody left up is owed nothing, and must stay silent.
    ws.ulak(server)
        .args(["docker", "compose", "down"])
        .assert()
        .success();
    let out = ws
        .ulak(server)
        .args(["docker", "compose", "ps"])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        !said.contains("no ulak service is running"),
        "nothing is being maintained here, so there is nothing to complain about:\n{said}"
    );

    // The scenarios in this file build on each other's state (see the
    // module header), and the next one needs a RUNNING stack to watch
    // `clean` refuse. Put back what this one borrowed — and read what
    // this `up` SAYS, because this exact command was the hole: the
    // before-command check reads the declaration as `down` left it
    // (`live: false`; on a fresh workspace, absent), while the value
    // that would open its gate is written by this very command a few
    // lines later. So `ps` warned, `logs` warned, and the one command
    // that sends a user to open a browser onto a dead port was silent
    // on every fresh start. This suite itself carried a live repro for
    // it: this `up` used to be checked for success alone.
    let out = ws
        .ulak(server)
        .args(["docker", "compose", "up", "-d"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "up -d failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("no ulak service is running"),
        "the command that creates the expectation must carry the warning:\n{said}"
    );
    assert!(
        said.contains("ulak service install"),
        "and the way out of it:\n{said}"
    );
    assert!(
        said.matches("no ulak service is running").count() == 1,
        "one command, one sentence — the before- and after-command checks must not both fire:\n{said}"
    );
}

/// The half that was never written: typed anywhere else,
/// `status` answers a different and perfectly good question — what is
/// this MACHINE doing? The Docker stacks a user leaves up accumulate, and the
/// answer to that was never a TTL; it was being able to SEE them.
///
/// No new command, no server round-trip: every number comes from local
/// files, so it answers with the laptop off the network.
fn scenario_status_outside_a_project_shows_the_machine(server: &TestServer, ws: &Workspace) {
    // ws.home holds no compose file, which used to be a hard error:
    // "no compose file found in ... or any parent directory".
    let out = ws
        .ulak(server)
        .current_dir(&ws.home)
        .arg("status")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "outside a project this is a question, not a mistake:\n{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !text.contains("no compose file found"),
        "it must stop treating 'no project here' as an error:\n{text}"
    );
    assert!(
        text.contains("this machine") && text.contains("stacks"),
        "it must report on the machine:\n{text}"
    );
    // The workspace this suite has been driving is live, and must be named.
    assert!(
        text.contains("live"),
        "a stack left up must be visible from outside its own directory:\n{text}"
    );
}

/// The 82 seconds the maintainer measured with a stopwatch, and the
/// only sentence that describes them: `up` finished, and nothing was
/// reachable.
///
/// The service does not learn that a stack came up — it ASKS, on a
/// cadence it picks from what it currently believes. Believing nothing is
/// running, it asks once every two minutes (`SILENT_PROBE_EVERY`). And
/// the state right before `up` succeeds is, by definition, "nothing is
/// running yet" — so the slowest cadence is always the one in force at
/// the exact moment the answer changes. Measured on a large monorepo:
/// stack up at 17:52:03, ports home at 17:53:25.
///
/// The fix is a doorbell, not a faster cadence: a compose command leaves
/// a nudge on disk (`intent::nudge`), the service consumes it on its
/// five-second catalog read and asks immediately. The slow cadence stays
/// exactly as it was for the case it was written for — a machine where
/// genuinely nothing is happening.
///
/// A SCENARIO and not a second `#[test]`, which is how it was written
/// first and was wrong: libtest runs the `#[test]`s in one binary in
/// PARALLEL unless somebody says otherwise, and CI says `cargo test` with
/// no `--test-threads`. Two of them here means two stacks built from the
/// same fixture, both publishing 8080 — locally for the tunnel, and on a
/// real host for the container. Whichever loses fails for a reason that
/// has nothing to do with what it tests. Every other server-driving file
/// in this suite has exactly one `#[test]` for the same reason.
fn scenario_the_ports_come_home_right_after_up(
    server: &TestServer,
    ws: &Workspace,
    service: &Service,
) {
    // Put the service in the slow state on purpose, the way `up` itself
    // always does: the intent stays live (`stop` is not `down`, so
    // nothing calls `declare`), but nothing is running — so the probe
    // that lands here schedules the next one two minutes out.
    ws.ulak(server)
        .args(["docker", "compose", "stop"])
        .assert()
        .success();
    service.wait_status(ws, 120, "the stack reported down", |st| {
        tunnels_open(st) == 0
    });

    // …and bring it back. This is the moment being measured.
    ws.ulak(server)
        .args(["docker", "compose", "start"])
        .assert()
        .success();
    let typed = Instant::now();
    service.wait_status(ws, 90, "the ports coming home again", |st| {
        tunnels_open(st) > 0
    });
    let waited = typed.elapsed();

    assert!(
        waited < Duration::from_secs(30),
        "the ports came home {waited:?} after the stack did — without the doorbell this \
         waits out SILENT_PROBE_EVERY (120 s), which is what the maintainer measured \
         as 82 seconds. Service said:\n{}",
        service.said()
    );
}

/// How many of the service's tunnels are actually open, read the way
/// `ulak status` reads it.
fn tunnels_open(st: &serde_json::Value) -> usize {
    st["tunnels"]
        .as_array()
        .map(|ts| ts.iter().filter(|t| t["open"] == true).count())
        .unwrap_or(0)
}
