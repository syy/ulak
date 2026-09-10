//! Catalog sweep: the mandatory edge cases not already embedded in
//! the phase suites — filesystem oddities, compose variety via
//! COMPOSE_* env, interruption recovery, scale smoke, ControlMaster
//! death, concurrency.
//!
//! Ten unrelated edge cases, ten verdicts. They used to be one `#[test]`
//! calling them in sequence off one shared workspace, which is the worst
//! version of that shape in this repo: the cheap scenarios ran first and
//! the expensive ones last, so a failure in the unicode-filename check —
//! the very first — meant the 10k-file sweep, the interrupted transfer,
//! the fifteen-service stack and the ControlMaster scenario were never
//! run at all, and the one red line said nothing about any of them. That
//! is exactly what happened when the base image stopped arriving: one
//! failure, nine silences.
//!
//! `TestServer::shared` keeps the cost where it was — one fixture for
//! the binary, and the scenarios now run in parallel rather than in
//! series. Each takes its OWN workspace, because the shared one was the
//! only thing making them a sequence: half of them rewrite compose.yaml
//! and the other half assert on what a sync left behind.
//!
//! Every scenario here shares that one fixture, including the control
//! master one, which used to boot its own so it could restart the
//! container. Restarting is not portable — it does nothing on the
//! real-host backend — so it kills its own masters instead, in a
//! control-socket directory it alone owns.

mod common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{ChildGuard, TestServer, Workspace, extract_remote_dir};

/// A workspace pointed at the shared server.
fn wired(server: &TestServer) -> Workspace {
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws
}

/// Block until rsync has the blob half-sent on the server, or hand back
/// what it saw instead.
///
/// A half-sent file is not at its final name — that is the whole point
/// of the way ulak runs rsync, and it is what makes "in flight"
/// observable from outside. Both spellings count, and both were measured
/// rather than assumed: while the bytes are moving the receiver holds
/// `.blob.bin.<random>` beside the destination, and once the transfer is
/// cut short what is left goes to `.ulak-partial/blob.bin`. So the test
/// is "an entry that mentions blob.bin and is not the finished file".
///
/// `Err` carries the listing, because "the window never opened" and "it
/// opened somewhere I was not looking" are different problems and the
/// next reader should not have to guess which one they have.
fn wait_until_the_blob_is_half_sent(
    server: &TestServer,
    remote_dir: &str,
    within: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + within;
    loop {
        let seen = server.ssh(&format!(
            "ls -a {remote_dir} {remote_dir}/.ulak-partial 2>/dev/null"
        ));
        let listing = String::from_utf8_lossy(&seen.stdout).into_owned();
        if listing
            .lines()
            .any(|l| l.contains("blob.bin") && l.trim() != "blob.bin")
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(listing);
        }
    }
}

/// A control-socket directory one scenario alone owns.
///
/// SHORT because the full socket path has to stay under the ~104-byte
/// unix limit, and private because killing what is in it must not reach
/// the masters every other test on this shared server is using.
fn private_control_dir() -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/ulak-cat-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("private control dir");
    dir
}

fn sockets_in(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir.join("ulak"))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

/// SIGKILL, so the socket file is left standing. A master asked to leave
/// takes its socket with it, and a socket that is gone is not the state
/// this scenario is about.
fn kill_masters_under(dir: &Path) {
    let _ = std::process::Command::new("pkill")
        .arg("-9")
        .arg("-f")
        .arg(dir.display().to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    // pkill returns before the kernel has reaped them; a socket check
    // that raced the death would read the live state and pass for the
    // wrong reason.
    std::thread::sleep(Duration::from_millis(300));
}

/// Push what this scenario has written and answer with the workspace
/// directory the server now holds.
fn sync_and_locate(ws: &Workspace, server: &TestServer) -> String {
    let out = ws.ulak(server).arg("sync").output().unwrap();
    assert!(
        out.status.success(),
        "sync failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    extract_remote_dir(&String::from_utf8_lossy(&out.stderr)).expect("remote dir")
}

/// Spaces, unicode, symlinks (in- and out-pointing), executable bit.
#[test]
fn odd_names_symlinks_and_the_exec_bit_all_survive_the_trip() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);

    ws.write("with space/ünïcode filé nâme.txt", "unicode-ok");
    ws.write("bin/tool.sh", "#!/bin/sh\necho tool");
    let tool = ws.project.join("bin/tool.sh");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("bin/tool.sh", ws.project.join("link-in")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", ws.project.join("link-out")).unwrap();

    let remote_dir = sync_and_locate(&ws, &server);

    let cat = server.ssh(&format!(
        "cat '{remote_dir}/with space/ünïcode filé nâme.txt'"
    ));
    assert_eq!(String::from_utf8_lossy(&cat.stdout).trim(), "unicode-ok");
    // Executable bit survives (--perms).
    assert!(
        server
            .ssh(&format!("test -x {remote_dir}/bin/tool.sh"))
            .status
            .success(),
        "exec bit lost"
    );
    // Symlinks are copied as symlinks, both directions.
    for link in ["link-in", "link-out"] {
        assert!(
            server
                .ssh(&format!("test -L {remote_dir}/{link}"))
                .status
                .success(),
            "{link} must arrive as a symlink"
        );
    }

    ws.forget_on_server(&server);
}

/// macOS is case-insensitive, Linux is not: after `Foo.txt` → `foo.txt`
/// exactly ONE file (the new casing) must remain remotely.
#[test]
fn a_case_only_rename_leaves_exactly_the_new_casing_on_the_server() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);

    ws.write("Foo.txt", "cased");
    let remote_dir = sync_and_locate(&ws, &server);
    std::fs::rename(ws.project.join("Foo.txt"), ws.project.join("foo.txt")).unwrap();
    ws.ulak(&server).arg("sync").assert().success();

    let ls = server.ssh(&format!(
        "ls {remote_dir} | grep -i '^foo.txt$' | sort | tr '\\n' ' '"
    ));
    assert_eq!(
        String::from_utf8_lossy(&ls.stdout).trim(),
        "foo.txt",
        "exactly the new casing must survive on the case-sensitive server"
    );

    ws.forget_on_server(&server);
}

/// COMPOSE_FILE (multi -f, ORDER matters) + COMPOSE_PROFILES + .env
/// interpolation all travel to the remote model resolution.
#[test]
fn compose_file_order_profiles_and_env_interpolation_reach_the_remote_model() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);

    ws.write(
        "base.yaml",
        "services:\n  app:\n    image: base:${TAG:-none}\n  extra:\n    image: alpine:3.20\n    profiles: [\"debug\"]\n",
    );
    ws.write(
        "override.yaml",
        "services:\n  app:\n    image: override:1\n",
    );
    ws.write(".env", "TAG=interp-worked\n");
    // `config` is spec'd read-only (no auto-sync) — push the new files.
    ws.ulak(&server).arg("sync").assert().success();

    // Order preserved: override.yaml LAST must win.
    let out = ws
        .ulak(&server)
        .env("COMPOSE_FILE", "base.yaml:override.yaml")
        .args(["docker", "compose", "config", "--format", "json"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "remote config failed:\n{text}");
    assert!(
        text.contains("override:1"),
        "-f order was not preserved remotely:\n{text}"
    );
    assert!(
        !text.contains("\"extra\""),
        "profile-gated service must be absent without the profile:\n{text}"
    );

    // Reversed order: base wins, and interpolation uses the root .env.
    let out = ws
        .ulak(&server)
        .env("COMPOSE_FILE", "override.yaml:base.yaml")
        .args(["docker", "compose", "config", "--format", "json"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("base:interp-worked"),
        "reversed -f order / .env interpolation broken:\n{text}"
    );

    // Profiles activate via COMPOSE_PROFILES.
    let out = ws
        .ulak(&server)
        .env("COMPOSE_FILE", "base.yaml")
        .env("COMPOSE_PROFILES", "debug")
        .args(["docker", "compose", "config", "--services"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("extra"),
        "COMPOSE_PROFILES was not captured:\n{text}"
    );

    ws.forget_on_server(&server);
}

/// compose.override.yaml auto-load, the --profile flag, and multiple
/// env_file entries — all resolved by the REMOTE compose.
#[test]
fn an_override_file_a_profile_flag_and_several_env_files_resolve_remotely() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);

    // 1. compose.override.yaml loads automatically next to compose.yaml.
    ws.write("compose.yaml", "services:\n  app:\n    image: base:1\n");
    ws.write(
        "compose.override.yaml",
        "services:\n  app:\n    image: auto-override:1\n",
    );
    ws.ulak(&server).arg("sync").assert().success();
    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "config", "--format", "json"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("auto-override:1"),
        "compose.override.yaml must auto-load remotely:\n{text}"
    );
    std::fs::remove_file(ws.project.join("compose.override.yaml")).unwrap();

    // 2. --profile flag (through the escape hatch, like plain compose).
    ws.write(
        "compose.yaml",
        "services:\n  app:\n    image: base:1\n  extra:\n    image: alpine:3.20\n    profiles: [\"debug\"]\n",
    );
    // 3. multiple env_file entries: both load, the later one wins ties.
    ws.write("a.env", "ONLY_A=1\nSHARED=from-a\n");
    ws.write("b.env", "SHARED=from-b\n");
    let compose = std::fs::read_to_string(ws.project.join("compose.yaml")).unwrap();
    let with_env = compose.replace(
        "  app:\n    image: base:1\n",
        "  app:\n    image: base:1\n    env_file: [a.env, b.env]\n",
    );
    std::fs::write(ws.project.join("compose.yaml"), with_env).unwrap();
    ws.ulak(&server).arg("sync").assert().success();

    let out = ws
        .ulak(&server)
        .args([
            "docker",
            "compose",
            "--profile",
            "debug",
            "config",
            "--services",
        ])
        .output()
        .unwrap();
    let services = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        services.contains("extra"),
        "--profile flag must reach the remote compose:\n{services}"
    );
    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "config", "--services"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("extra"),
        "profile service must stay hidden without the flag"
    );

    let out = ws
        .ulak(&server)
        .args(["docker", "compose", "config", "--format", "json"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("\"ONLY_A\": \"1\"") && text.contains("\"SHARED\": \"from-b\""),
        "multiple env_file entries must merge with later-wins:\n{text}"
    );

    ws.forget_on_server(&server);
}

/// `~/data:/x` resolves against the REMOTE home — doctor must treat it
/// as out-of-root (SERVER/MISSING), never as a synced path.
#[test]
fn a_tilde_volume_belongs_to_the_remote_home_and_is_missing_there() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);

    ws.write(
        "compose.yaml",
        "services:\n  app:\n    image: alpine:3.20\n    volumes:\n      - ~/ulak-e2e-tilde:/x\n",
    );
    let out = ws.ulak(&server).arg("doctor").output().unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "a missing tilde path must fail doctor:\n{text}"
    );
    assert!(
        text.contains("MISSING") && text.contains("ulak-e2e-tilde"),
        "tilde volume must classify out-of-root:\n{text}"
    );

    ws.forget_on_server(&server);
}

/// Kill a sync mid-transfer; the next sync must converge to identical
/// content (partial-dir keeps the tree consistent).
///
/// The kill is aimed with a stopwatch rather than a dice roll. What
/// stood here was a 400 ms sleep, and nothing asserted that it landed:
/// a kill that arrived before rsync started, or after it finished,
/// degraded the scenario to "two ordinary syncs agree on a checksum"
/// and still passed green. rsync writes what it has not finished into
/// `--partial-dir=.ulak-partial`, so that file APPEARING on the server
/// is the transfer saying it is under way — and waiting for it is the
/// same thing `stall_the_push` does for the other race, without arming
/// a stall that every sibling test on this shared server would also
/// have to walk through.
///
/// It is also what let the blob shrink from 150 MB to 40: size was
/// buying the window, and the window is now bought by looking.
#[test]
fn an_interrupted_sync_converges_on_the_next_one() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let remote_dir = sync_and_locate(&ws, &server);

    let big = ws.project.join("blob.bin");
    // The block size is spelled in bytes because a suffix is a dialect:
    // `bs=1m` is BSD's and GNU's dd rejects it outright, so this line was
    // written on a Mac and stayed unrun until CI first got this far and
    // failed here. A plain count is every dd's.
    let out = std::process::Command::new("dd")
        .args(["if=/dev/urandom", "bs=1048576", "count=40"])
        .arg(format!("of={}", big.display()))
        .output()
        .expect("dd");
    // dd's own words, because "could not create the big file" sent
    // exactly this failure to CI with nothing to read.
    assert!(
        out.status.success(),
        "could not create the big file: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );

    let mut child = ws
        .ulak_raw(&server)
        .arg("sync")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let caught = wait_until_the_blob_is_half_sent(&server, &remote_dir, Duration::from_secs(60));
    let _ = child.kill();
    let _ = child.wait();
    if let Err(listing) = caught {
        panic!(
            "the blob was never half-sent, so the kill landed outside the transfer and \
             the convergence below would be two ordinary syncs agreeing with each \
             other; the workspace held:\n{listing}"
        );
    }

    // Recovery: a fresh sync completes and the content matches.
    ws.ulak(&server).arg("sync").assert().success();
    let local_sum = std::process::Command::new("/sbin/md5")
        .arg("-q")
        .arg(&big)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| {
            let o = std::process::Command::new("md5sum")
                .arg(&big)
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .unwrap()
                .to_string()
        });
    let remote = server.ssh(&format!("md5sum {remote_dir}/blob.bin"));
    let remote_sum = String::from_utf8_lossy(&remote.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
    assert_eq!(local_sum, remote_sum, "content diverged after interrupt");
    std::fs::remove_file(&big).unwrap();
    ws.ulak(&server).arg("sync").assert().success();

    ws.forget_on_server(&server);
}

/// 10k-file tree: sync completes, and the second sync sends nothing.
#[test]
fn ten_thousand_files_all_arrive_and_the_no_op_sync_sends_nothing() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let remote_dir = sync_and_locate(&ws, &server);

    for d in 0..100 {
        let dir = ws.project.join(format!("many/d{d:02}"));
        std::fs::create_dir_all(&dir).unwrap();
        for f in 0..100 {
            std::fs::write(dir.join(format!("f{f:02}.txt")), b"x").unwrap();
        }
    }
    ws.ulak(&server).arg("sync").assert().success();

    let count = server.ssh(&format!("find {remote_dir}/many -type f | wc -l"));
    assert_eq!(
        String::from_utf8_lossy(&count.stdout).trim(),
        "10000",
        "all 10k files must arrive"
    );

    let out = ws.ulak(&server).arg("sync").output().unwrap();
    assert!(
        out.status.success(),
        "the no-op sync failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stderr);
    // Timing cannot prove this over a real network: rsync must still stat
    // 10 000 paths and WAN latency can dominate a zero-byte transfer.
    // The itemized rsync receipt is the direct evidence—any retransmitted
    // entry increments this summary.
    assert!(
        said.contains("synced 0 change(s), 0 deletion(s)"),
        "the second sync must transfer nothing, got:\n{said}"
    );

    std::fs::remove_dir_all(ws.project.join("many")).unwrap();
    ws.ulak(&server)
        .args(["sync", "--max-delete", "10200"])
        .assert()
        .success();

    ws.forget_on_server(&server);
}

/// Two ulak processes on the same workspace: the lock serializes them,
/// both succeed.
///
/// The name made two claims and the body used to assert one of them.
/// Two concurrent syncs against one remote directory would very likely
/// both exit 0 with the workspace lock deleted outright — rsync does not
/// fail on a concurrent peer, it just interleaves — so the old
/// `sa.success() && sb.success()` passed with the feature it names
/// removed. `lockfile.rs` pins the waiting with two threads in ONE
/// process; the part only an e2e can reach is the cross-process flock
/// through the real state directory, and that is exactly the part that
/// went unasserted.
///
/// So contention is manufactured rather than hoped for: the test process
/// takes the same lock file the product takes, by path, and a real
/// `ulak sync` then has to queue behind a holder it did not start. That
/// is deterministic — no sleep is racing a transfer — and it is the
/// shape a user meets when the service is mid-reconcile.
#[test]
fn two_syncs_of_one_workspace_serialize_and_the_second_says_it_is_waiting() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);

    ws.write("concurrent.txt", "v1");
    ws.ulak(&server).arg("sync").assert().success();
    let ids = ws.workspace_ids();
    let id = ids.first().expect("the first sync registers the workspace");
    let lock_path = ws
        .home
        .join(".local/state/ulak/locks")
        .join(format!("{id}.lock"));
    assert!(
        lock_path.is_file(),
        "the workspace lock is not where the product puts it ({}), so this scenario \
         would be holding a file nobody contends for",
        lock_path.display()
    );

    // Somebody else is working on this workspace. Held for real, on the
    // real file, by a process that is not ulak.
    let held = std::fs::File::open(&lock_path).expect("open the workspace lock");
    held.lock().expect("hold the workspace lock");

    let waiting = ws
        .ulak_raw(&server)
        .arg("sync")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the queued sync");
    let mut waiting = ChildGuard(waiting);
    // Taken before the wait, and read only after the lock is released:
    // reading to EOF blocks until the child exits, and the child cannot
    // exit while this process holds the lock.
    let mut pipe = waiting.0.stderr.take().expect("the queued sync's stderr");
    // Long enough to get past config, ssh and the footprint cache and
    // reach the lock. Still running is the whole assertion: a sync that
    // walked straight through a held lock has already finished.
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        waiting
            .0
            .try_wait()
            .expect("poll the queued sync")
            .is_none(),
        "a second ulak went through a lock this process is holding — the workspace \
         lock is not cross-process, and two reconciles can interleave over one tree"
    );

    drop(held);
    let mut said = String::new();
    pipe.read_to_string(&mut said)
        .expect("read the queued sync");
    let status = waiting
        .0
        .wait()
        .expect("the queued sync should finish once the lock is free");
    assert!(
        status.success(),
        "the queued sync failed once the lock was free:\n{said}"
    );
    assert!(
        said.contains("waiting for it to finish"),
        "a wait the user cannot see is a hang; it has to be said out loud:\n{said}"
    );

    // And the shape a user actually types: two of them at once, neither
    // of which is this process. Both must come out the other side.
    let mut a = ws
        .ulak_raw(&server)
        .arg("sync")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut b = ws
        .ulak_raw(&server)
        .arg("sync")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let sa = a.wait().unwrap();
    let sb = b.wait().unwrap();
    assert!(
        sa.success() && sb.success(),
        "concurrent syncs must both succeed (serialized), got {sa:?} / {sb:?}"
    );

    ws.forget_on_server(&server);
}

/// A 15-service stack (light images) comes up and reports 15 running.
#[test]
fn a_fifteen_service_stack_comes_up_and_reports_every_one_running() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    // The only scenario in this file that runs a container, so the only
    // one that owes the server an image.
    server.needs_image("alpine:3.20");
    let ws = wired(&server);

    let mut compose = String::from("services:\n");
    for i in 0..15 {
        compose.push_str(&format!(
            "  svc{i:02}:\n    image: alpine:3.20\n    command: [\"sleep\", \"120\"]\n"
        ));
    }
    ws.write("compose.yaml", &compose);
    ws.ulak(&server)
        .args(["docker", "compose", "up", "-d"])
        .assert()
        .success();

    let out = ws
        .ulak(&server)
        .args([
            "docker",
            "compose",
            "ps",
            "--services",
            "--status",
            "running",
        ])
        .output()
        .unwrap();
    let running = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("svc"))
        .count();
    assert_eq!(running, 15, "all 15 services must be running");

    ws.ulak(&server)
        .args(["docker", "compose", "down"])
        .assert()
        .success();

    ws.forget_on_server(&server);
}

/// A shared connection killed under ulak must not take the next command
/// with it: the socket file outlives the master, and ssh has to be
/// allowed to notice that and connect anyway.
///
/// What stood here was `if server.restart() { … }`, and `restart()`
/// answers false on `Kind::Real` — so under `ULAK_TEST_E2E=host:<name>` the
/// whole body was one warm sync and an `eprintln!`, which libtest
/// captures for a PASSING test. It also paid a DinD boot of its own to
/// do it.
///
/// The master is SIGKILLed rather than asked to leave, and that is the
/// point: a master that exits tidily removes its socket and the next
/// command opens a fresh one, which proves nothing. A killed one leaves
/// the socket standing with nothing behind it — what a laptop that slept
/// with the lid shut leaves — and that is the state under test.
///
/// Two things this deliberately does NOT claim, so nobody reads more
/// into it later. It is not the timeout retry: reaching that needs a
/// master that accepts and never answers, and `proc::SCRIPT` plus
/// `proc::PROBE` price that at 75 seconds. And it is not the dead-link
/// retry, which `e2e_deadline::a_link_that_dies_mid_session_fails_fast_and_comes_back`
/// already drives on both backends. What is left is the cheap, portable
/// half nothing else covers.
///
/// It shares the fixture, which the old version could not: ulak's
/// control sockets go under `$XDG_RUNTIME_DIR/ulak`, so pointing this
/// scenario's own invocations at a directory of its own means the kill
/// reaches its masters and nobody else's.
#[test]
fn a_control_master_killed_under_ulak_does_not_take_the_next_command_with_it() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let masters = private_control_dir();

    let mut warm = ws.ulak(&server);
    warm.env("XDG_RUNTIME_DIR", &masters);
    warm.arg("sync").assert().success();
    let sockets = sockets_in(&masters);
    assert!(
        !sockets.is_empty(),
        "no control socket appeared under {} — multiplexing is off, and there is \
         nothing here to kill",
        masters.display()
    );

    kill_masters_under(&masters);
    let survivors = sockets_in(&masters);
    assert_eq!(
        survivors, sockets,
        "the socket file went with the master, so the next command opens a fresh \
         connection and this scenario is not standing in front of anything"
    );

    ws.write("after-the-death.txt", "alive");
    let mut next = ws.ulak(&server);
    next.env("XDG_RUNTIME_DIR", &masters);
    let out = next.arg("sync").output().unwrap();
    assert!(
        out.status.success(),
        "a stale control socket broke the next command:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let ids = ws.workspace_ids();
    let id = ids.first().expect("a workspace was registered");
    let root = ws.remote_workspace_root(id);
    assert!(
        server
            .ssh(&format!("test -e {root}/proj/after-the-death.txt"))
            .status
            .success(),
        "the command reported success without the edit reaching the server"
    );

    kill_masters_under(&masters);
    let _ = std::fs::remove_dir_all(&masters);
    ws.forget_on_server(&server);
}
