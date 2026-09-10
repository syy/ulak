//! No wait is unbounded — the whole point of v0.4 step 1.
//!
//! These need no server, on purpose: the failure they guard is what
//! happens when there ISN'T one. The destination is 192.0.2.1, the
//! RFC 5737 documentation range, which nothing routes — the closest a
//! test can get to "the laptop lid closed and the wifi went away".
//!
//! Measured before the deadline layer existed: `ulak status` against
//! an unreachable server sat silent for **150 seconds** (75 + 75, macOS
//! `net.inet.tcp.keepinit`) and a command on a link that died mid-flight
//! sat for 13 minutes 21 seconds and never came back at all.

mod common;

use std::time::{Duration, Instant};

use common::{TestServer, Workspace, extract_remote_dir};

/// Generous on purpose: the point is 150 s → tens of seconds, not a
/// stopwatch. Two attempts of ConnectTimeout=10 plus process startup is
/// ~20 s, so a minute leaves room for a loaded CI box without ever
/// letting the old behaviour through.
const CEILING: Duration = Duration::from_secs(60);

fn unreachable_workspace() -> Workspace {
    let ws = Workspace::new();
    // A literal IP, not a name: a name that fails to resolve returns
    // instantly and would test nothing. 192.0.2.1 accepts the SYN into
    // the void, which is exactly the shape of a dead link.
    std::fs::write(ws.project.join("ulak.local.toml"), "host = \"192.0.2.1\"\n").unwrap();
    ws
}

#[test]
fn commands_against_an_unreachable_server_give_up_and_say_why() {
    for command in ["status", "sync", "doctor"] {
        let ws = unreachable_workspace();
        let started = Instant::now();
        let out = ws.ulak_alone().arg(command).output().unwrap();
        let took = started.elapsed();

        assert!(
            took < CEILING,
            "`ulak {command}` waited {took:?} — an unbounded wait is back"
        );
        assert!(
            !out.status.success(),
            "`ulak {command}` must fail against a server that never answers"
        );
        // The no-dead-end-errors contract: whatever went wrong, the user
        // is told what to do next. Both streams, because where that
        // sentence lands is the command's choice and not this test's
        // business: `status` and `sync` die through `render_error` on
        // stderr, while `doctor`'s whole report — the "— now: ..." on
        // every problem included — goes to stdout by contract. Reading
        // only stderr asked doctor for its answer on a channel it had
        // promised not to use, and got the empty string.
        let said = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(
            said.contains("now"),
            "`ulak {command}` left the user with nowhere to go:\n{said}"
        );
    }
}

/// The other half of the same promise, and the one that has NEVER run
/// against a real server: not "there was never a connection" but "there
/// WAS one and it died under me".
///
/// `TestServer::restart()` could only ever do this on the dockerized
/// fixture, so the entire connection axis skipped itself on my-server.
/// `break_link()` does it on both: ulak's control masters are killed
/// and its ssh is pointed into a blackhole, while the harness keeps its
/// own way in — the server is never touched, which is also what the
/// my-server rules require.
#[test]
fn a_link_that_dies_mid_session_fails_fast_and_comes_back() {
    let Some(server) = TestServer::start() else {
        return;
    };
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws.write("site/index.html", "before-the-break");

    let out = ws.ulak(&server).arg("sync").output().unwrap();
    assert!(out.status.success(), "the first sync must work");
    let remote_dir = extract_remote_dir(&String::from_utf8_lossy(&out.stderr)).expect("remote dir");

    // The lid closes.
    server.break_link();
    let started = Instant::now();
    let out = ws.ulak(&server).arg("sync").output().unwrap();
    let took = started.elapsed();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        !out.status.success(),
        "a sync over a dead link must not report success:\n{stderr}"
    );
    assert!(
        took < CEILING,
        "a dead link held the command for {took:?} — this is the 13-minute hang coming back"
    );
    assert!(
        stderr.contains("now"),
        "a dead link must still leave the user somewhere to go:\n{stderr}"
    );

    // The lid opens. Nothing was typed to make this work.
    server.heal_link();
    ws.write("site/index.html", "after-the-break");
    ws.ulak(&server).arg("sync").assert().success();
    let cat = server.ssh(&format!("cat {remote_dir}/site/index.html"));
    assert_eq!(
        String::from_utf8_lossy(&cat.stdout).trim(),
        "after-the-break",
        "the edit made while the link was down must arrive once it is back"
    );

    if let Some((hash_dir, _)) = remote_dir.rsplit_once('/') {
        server.ssh(&format!("rm -rf {hash_dir}"));
    }
}

// `the_config_switch_for_the_service_does_not_break_the_cli` was here,
// and it paid the full connect deadline against 192.0.2.1 — about a
// third of this binary's runtime — for two negative substring checks
// that need no server at all. One of the two could no longer fail
// either: it looked for "not valid ulak TOML" while the code says
// "not valid Ulak TOML". It lives in e2e_smoke.rs now, serverless, and
// asserts what the command DID rather than two things it did not say.
