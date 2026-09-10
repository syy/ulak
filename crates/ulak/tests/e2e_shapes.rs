//! The project SHAPES the prototype could not handle.
//!
//! The prototype's discipline was fine; its fixture was not. Every shape
//! here was outside the one shape it tested (compose.yaml at the root,
//! loopback ports, no `-p`, never called from a subdirectory) — and
//! every known bug lived in exactly that gap.
//!
//! Shapes 2+3 need a stack RUNNING to prove where its mounts point, and
//! they are one test for that reason: bringing the stack up and then
//! generating a file into it from inside the container is one story told
//! in two halves. Shapes 4 and 5 need no container at all — they ask
//! `doctor` and `sync` where a path belongs — and they are their own
//! tests because they were the ones being lost: under a single
//! orchestrating `#[test]`, a stack that would not come up (a missing
//! image did it, repeatedly) aborted the run before either of them was
//! ever asked. Two claims that cannot fail for that reason were
//! reported red for it, and worse, were never actually checked.
//!
//! The fixture is not multiplied by that: `TestServer::shared` boots one
//! server for the binary. What each test does owe is its own workspace,
//! since cargo runs them in parallel and they rewrite compose files.

mod common;

use std::path::Path;

use common::{TestServer, Workspace};

/// A directory whose size would dominate any tree-shaped sync. The
/// footprint must never even look inside it.
const NOISE_MB: usize = 40;

/// The workspace every shape starts from: a monorepo whose stack is a
/// few KB in one corner and whose bulk has nothing to do with it.
///
/// `needs_image` because every shape that RUNS anything runs it from
/// nginx, with the command overridden to a sleep. What is under test is
/// where the mounts point, so any long-lived image would do — but the
/// compose files say nginx, and a fixture that says one thing and runs
/// another is a fixture nobody trusts.
fn monorepo(server: &TestServer) -> Workspace {
    server.needs_image("nginx:alpine");
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
    ws.set_host(&server.alias);
    build_monorepo(&ws);
    ws
}

/// Shapes 2 and 3, and then the same running stack writing back into the
/// repo. One test because the second half has nothing to say without the
/// first half's containers.
#[test]
fn a_stack_in_a_subdirectory_mounts_its_siblings_and_writes_back_into_the_repo() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = monorepo(&server);
    // Armed BEFORE the first container, not after the last assertion.
    let _teardown = Teardown {
        server: &server,
        ws: &ws,
        stack: true,
    };

    shape_subdir_with_sibling_mount(&server, &ws);
    the_workspace_runs_both_ways(&server, &ws);
}

/// Shape 5, which needs no container: where a reference two levels up
/// puts the anchor.
#[test]
fn a_reference_two_levels_up_keeps_its_layout_on_the_server() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = monorepo(&server);
    let _teardown = Teardown {
        server: &server,
        ws: &ws,
        stack: false,
    };
    shape_deep_reference(&server, &ws);
}

/// Shape 4, which needs no container either: which of three spellings of
/// a path is ours.
#[test]
fn absolute_tilde_and_relative_paths_are_told_apart() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = monorepo(&server);
    let _teardown = Teardown {
        server: &server,
        ws: &ws,
        stack: false,
    };
    shape_escaping_paths(&server, &ws);
}

/// A monorepo: the stack is a few KB in one corner, the rest is bulk
/// that has nothing to do with it.
fn build_monorepo(ws: &Workspace) {
    // ── shape 2: compose in a subdirectory, NON-standard names, and a
    //    mount that reaches a SIBLING directory.
    ws.write(
        "stack/compose.dev.yaml",
        "services:\n  web:\n    image: nginx:alpine\n    \
         command: [\"sh\", \"-c\", \"sleep 600\"]\n    volumes:\n      \
         - ./site:/site:ro\n      - ../shared/certs:/certs:ro\n      \
         - ./work:/work\n",
    );
    // ── shape 3: a second -f whose values must win (order preserved).
    ws.write(
        "stack/compose.extra.yaml",
        "services:\n  web:\n    environment:\n      LAYER: second-file\n",
    );
    ws.write("stack/site/index.html", "SITE-OK");
    ws.write("stack/work/.keep", "");
    ws.write("shared/certs/dev.pem", "CERT-OK");

    // ── shape 5: a reference climbing TWO levels out of the compose
    //    file's own directory.
    ws.write(
        "deep/a/b/compose.yaml",
        "services:\n  reader:\n    image: nginx:alpine\n    \
         command: [\"sh\", \"-c\", \"sleep 600\"]\n    volumes:\n      \
         - ../../assets:/assets:ro\n",
    );
    ws.write("deep/assets/logo.txt", "ASSET-OK");

    // ── the bulk that must never be scanned or sent.
    let noise = ws.project.join("noise/vendor");
    std::fs::create_dir_all(&noise).unwrap();
    std::fs::write(noise.join("blob.bin"), vec![0u8; NOISE_MB * 1024 * 1024]).unwrap();
}

/// Shapes 2 + 3 together, the way they actually occur: `cd` into the
/// subdirectory and name two compose files. The prototype stopped its
/// root walk at the first compose file it saw, so `../shared` fell
/// outside the root and arrived MISSING.
fn shape_subdir_with_sibling_mount(server: &TestServer, ws: &Workspace) {
    // No `-p`. This scenario is about PATHS — a compose file in a
    // subdirectory reaching a sibling two levels out — and a `-p` here
    // only ever muddied it: docker resolves the project name per
    // command, so a name typed on the `up` and nowhere else means every
    // later `exec`, `sync` and `status` in this scenario addresses a
    // different project. That is docker's behaviour and it is pinned
    // where it belongs, in `e2e_flags` and `e2e_passthrough`.
    let out = ws
        .ulak(server)
        .current_dir(ws.project.join("stack"))
        .args([
            "docker",
            "compose",
            "-f",
            "compose.dev.yaml",
            "-f",
            "compose.extra.yaml",
            "up",
            "-d",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "up from a subdirectory failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The proof: both mounts are real inside the container — the local
    // one AND the one that climbed out to a sibling.
    let seen = exec(
        server,
        ws,
        "cat /site/index.html /certs/dev.pem; echo \"$LAYER\"",
    );
    for expected in ["SITE-OK", "CERT-OK", "second-file"] {
        assert!(
            seen.contains(expected),
            "{expected:?} never reached the container:\n{seen}"
        );
    }

    // The bulk stayed home. rsync is not told to skip it — no rule
    // includes it, so rsync never descends into it at all.
    let workspace = sync_and_locate(server, ws);
    let listing = server.ssh(&format!("ls -A {workspace}"));
    let listing = String::from_utf8_lossy(&listing.stdout).to_string();
    assert!(
        listing.contains("stack") && listing.contains("shared"),
        "the workspace must hold exactly the footprint's anchor layout:\n{listing}"
    );
    assert!(
        !listing.contains("noise"),
        "a directory nothing references must not be synced:\n{listing}"
    );
    let du = server.ssh(&format!("du -sm {workspace} | cut -f1"));
    let mb: usize = String::from_utf8_lossy(&du.stdout)
        .trim()
        .parse()
        .unwrap_or(9999);
    assert!(
        mb < NOISE_MB / 2,
        "the workspace is {mb} MB — the {NOISE_MB} MB sibling must not have travelled"
    );

    // From a SUBDIRECTORY of where `up` ran. Root management can recover a
    // unique validated Desired declaration, but explicit globals still win
    // and must resolve relative to THIS command's cwd. This half pins that
    // accepting path; `e2e_management_context` pins the implicit one and
    // keeps it out of Docker's own command path.
    let out = ws
        .ulak(server)
        .current_dir(ws.project.join("stack/site"))
        .args(["-f", "../compose.dev.yaml", "-f", "../compose.extra.yaml"])
        .arg("status")
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "status from a subdirectory failed:\n{shown}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        shown.contains("compose.dev.yaml") && shown.contains("compose.extra.yaml"),
        "the explicit invocation did not reach a subdirectory:\n{shown}"
    );
}

/// A workspace is a workspace: what the stack produces lands in the repo, the
/// way a local bind mount would put it there. And the reverse direction
/// must not cost the user anything they did not ask for — a file they
/// really deleted still goes, a file the container made stays.
fn the_workspace_runs_both_ways(server: &TestServer, ws: &Workspace) {
    let dir = ws.project.join("stack");

    // The everyday case: a generator (prisma, rails, go mod, codegen)
    // runs in the container and writes into the repo.
    let out = ws
        .ulak(server)
        .current_dir(&dir)
        .args([
            "docker",
            "compose",
            "-f",
            "compose.dev.yaml",
            "-f",
            "compose.extra.yaml",
            "exec",
            "-T",
            "web",
            "sh",
            "-c",
            "mkdir -p /work/migrations && echo CREATE-TABLE > /work/migrations/001_init.sql",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the generator command failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let landed = dir.join("work/migrations/001_init.sql");
    assert!(
        landed.is_file(),
        "what the container produced must land in the repo, like a local bind mount"
    );
    assert!(
        std::fs::read_to_string(&landed)
            .unwrap()
            .contains("CREATE-TABLE")
    );

    // …and it survives the next save. The prototype deleted it here,
    // silently, because it could not tell "you removed this" from "this
    // was born on the server".
    ws.write("stack/site/index.html", "SITE-OK-EDITED");
    ws.ulak(server)
        .current_dir(&dir)
        .args(["-f", "compose.dev.yaml", "-f", "compose.extra.yaml"])
        .arg("sync")
        .assert()
        .success();
    assert!(
        landed.is_file(),
        "a generated file must survive an ordinary edit"
    );

    // A file the user REALLY deletes still goes: the ledger knows we put
    // that one there.
    std::fs::remove_file(dir.join("work/.keep")).unwrap();
    ws.ulak(server)
        .current_dir(&dir)
        .args(["-f", "compose.dev.yaml", "-f", "compose.extra.yaml"])
        .arg("sync")
        .assert()
        .success();
    let workspace = sync_and_locate(server, ws);
    assert!(
        server
            .ssh(&format!("test ! -e {workspace}/stack/work/.keep"))
            .status
            .success(),
        "a file the user deleted must be removed on the server too"
    );
    assert!(
        server
            .ssh(&format!(
                "test -e {workspace}/stack/work/migrations/001_init.sql"
            ))
            .status
            .success(),
        "…while the container's own file stays put"
    );
}

/// Shape 5: the anchor has to climb two levels for the mount to exist,
/// and the workspace must reproduce that layout, not flatten it.
fn shape_deep_reference(server: &TestServer, ws: &Workspace) {
    let out = ws
        .ulak(server)
        .current_dir(ws.project.join("deep/a/b"))
        .arg("sync")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "deep-reference sync failed:\n{stderr}"
    );
    let workspace = common::extract_remote_dir(&stderr).expect("remote dir");

    // anchor = deep/, so the compose file keeps its a/b/ depth and the
    // assets sit beside it exactly as they do locally.
    for (rel, needle) in [
        ("a/b/compose.yaml", "reader"),
        ("assets/logo.txt", "ASSET-OK"),
    ] {
        let cat = server.ssh(&format!("cat {workspace}/{rel} 2>&1"));
        assert!(
            String::from_utf8_lossy(&cat.stdout).contains(needle),
            "{rel} did not arrive with its layout intact"
        );
    }
    server.ssh(&format!("rm -rf {}", parent_of(&workspace)));
}

/// Shape 4: absolute, tilde and relative in one file. Only the relative
/// one is ours; `~` belongs to the REMOTE home, and an absolute path is
/// the server's own.
fn shape_escaping_paths(server: &TestServer, ws: &Workspace) {
    ws.write(
        "escaping/compose.yaml",
        "services:\n  mix:\n    image: nginx:alpine\n    volumes:\n      \
         - ./local:/a:ro\n      - /etc/hostname:/b:ro\n      \
         - ~/ulak-e2e-nowhere:/c:ro\n",
    );
    ws.write("escaping/local/here.txt", "LOCAL-OK");

    let out = ws
        .ulak(server)
        .current_dir(ws.project.join("escaping"))
        .arg("doctor")
        .output()
        .unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("SYNC") && text.contains("local"),
        "the relative mount must be synced:\n{text}"
    );
    assert!(
        text.contains("SERVER") && text.contains("/etc/hostname"),
        "an absolute path must classify as the server's own:\n{text}"
    );
    // `~` expands against the REMOTE home, so it is server-side too —
    // and missing, which doctor has to fail on rather than workspace.
    assert!(
        text.contains("MISSING") && text.contains("ulak-e2e-nowhere"),
        "a tilde path must classify server-side, not as a local one:\n{text}"
    );
    assert!(
        !out.status.success(),
        "a missing server path must fail doctor"
    );

    let out = ws
        .ulak(server)
        .current_dir(ws.project.join("escaping"))
        .arg("sync")
        .output()
        .unwrap();
    if let Some(workspace) = common::extract_remote_dir(&String::from_utf8_lossy(&out.stderr)) {
        server.ssh(&format!("rm -rf {}", parent_of(&workspace)));
    }
}

// ─── helpers ────────────────────────────────────────────────────────

/// `docker compose exec` from `stack/`, with BOTH streams, because a
/// failure here shows up as an empty stdout and the assertion that
/// follows would then blame the bytes instead of the container.
///
/// No `-p`, and the `up` this follows has none either. The two `-f` paths
/// are repeated because that is Docker's invocation model: neither file
/// selection nor project name crosses from one command into the next.
fn exec(server: &TestServer, ws: &Workspace, script: &str) -> String {
    let out = ws
        .ulak(server)
        .current_dir(ws.project.join("stack"))
        .args([
            "docker",
            "compose",
            "-f",
            "compose.dev.yaml",
            "-f",
            "compose.extra.yaml",
            "exec",
            "-T",
            "web",
            "sh",
            "-c",
            script,
        ])
        .output()
        .unwrap();
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Sync from `stack/` and answer with the workspace directory the server
/// now holds.
///
/// The name says `sync` because it syncs: this reads like an accessor and
/// is a side effect, and it is called three times, which is three syncs
/// the suite pays for. Kept because the remote directory is only ever
/// printed by a command that reached the server, so there is no cheaper
/// honest way to learn it.
fn sync_and_locate(server: &TestServer, ws: &Workspace) -> String {
    let out = ws
        .ulak(server)
        .current_dir(ws.project.join("stack"))
        .args(["-f", "compose.dev.yaml", "-f", "compose.extra.yaml"])
        .arg("sync")
        .output()
        .unwrap();
    common::extract_remote_dir(&String::from_utf8_lossy(&out.stderr)).expect("remote dir")
}

fn parent_of(remote_dir: &str) -> String {
    Path::new(remote_dir)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| remote_dir.to_string())
}

/// Teardown that runs on the FAILURE path too.
///
/// It used to be the last statement of each scenario, with fourteen
/// unwinding assertions in front of it. Under `ULAK_TEST_E2E=host:<name>` a
/// panic through those left `web` running `sleep 600` on somebody's real
/// machine with nothing anywhere to reap it — `TestServer::drop` removes
/// only a container it started itself, and this suite starts its own
/// through Compose.
///
/// Worse, the ids `forget_on_server` reads live in the workspace's own
/// TempDir HOME. That TempDir is destroyed when `Workspace` drops, so a
/// panic did not postpone the cleanup, it made it impossible — the
/// record of WHICH remote directories to remove went with it. A guard
/// declared after the workspace drops before it, which is what keeps
/// those ids readable here.
///
/// `stack` says whether a Compose stack was ever brought up: the
/// scenarios that only measure layout have no containers to stop, and
/// asking Compose to stop them would be a round trip per test for
/// nothing.
struct Teardown<'a> {
    server: &'a TestServer,
    ws: &'a Workspace,
    stack: bool,
}

impl Drop for Teardown<'_> {
    fn drop(&mut self) {
        if self.stack {
            self.ws
                .ulak(self.server)
                .current_dir(self.ws.project.join("stack"))
                .args([
                    "docker",
                    "compose",
                    "-f",
                    "compose.dev.yaml",
                    "-f",
                    "compose.extra.yaml",
                    "down",
                    "--remove-orphans",
                ])
                .output()
                .ok();
        }
        // Reads the ids locally and removes each workspace by id, so it
        // needs no sync and cannot panic — the old teardown located the
        // remote directory by SYNCING first, which is the one thing that
        // must not happen while a test is already unwinding.
        self.ws.forget_on_server(self.server);
        if self.server.is_real_host() {
            self.server
                .ssh_quiet("docker rmi nginx:alpine >/dev/null 2>&1; true");
        }
    }
}
