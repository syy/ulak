//! The build context's `.dockerignore` — the oversized workspace, small.
//!
//! The measured shape, small enough to run: several services build from
//! the repo ROOT, so the anchor itself is a build context and the whole
//! footprint model switches off. The repo's own `.dockerignore` is the
//! answer — the user already wrote down what the build reads — but only
//! for the build. Applying it blindly broke most of a real stack's bind
//! mounts to save under a megabyte.
//!
//! So the two scenarios below are one scenario seen from both sides, and
//! they use the SAME FILE NAME on purpose:
//!
//!   * `app-core-other/listeners.yml` — nothing references it, the
//!     ignore file drops it, and it must not be on the server.
//!   * `app-core-be/listeners.yml` — the same ignore rule drops it,
//!     and compose bind-mounts it, so it must be there AND readable
//!     inside the container.
//!
//! Plus the other two reasons a path overrides the ignore file, both
//! excluded by the very same `*`: an `env_file:` and a `configs: file:`.

mod common;

use std::path::Path;

use common::{Chain, TestServer, Workspace};

/// The three scenarios, in order and by name. They are one story told in
/// three parts over one synced workspace, so `Chain` reports which part
/// broke and which never ran rather than one verdict for all three.
const CHAIN: &[&str] = &[
    "the workspace is read back after an up",
    "the ignore file narrows the build context",
    "every other reason overrides it",
];

/// Big enough that a workspace carrying it is unmistakable in `du`, small
/// enough to write in a moment.
const JUNK_MB: usize = 20;

#[test]
fn a_build_context_travels_narrowed_and_everything_else_overrides_it() {
    let Some(server) = TestServer::start() else {
        return;
    };
    // `app/Dockerfile` is `FROM alpine:3.20`, and the whole scenario
    // turns on that build succeeding on the server.
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    build_the_monorepo(&ws);

    let mut chain = Chain::new(CHAIN);
    let workspace = chain.link(|| up_and_read_the_workspace(&server, &ws));
    chain.link(|| the_ignore_file_narrows_the_build_context(&server, &workspace));
    chain.link(|| every_other_reason_overrides_it(&server, &ws, &workspace));
    chain.finish();

    teardown(&server, &ws, &workspace);
}

/// A repo whose every service builds from its root — and whose root
/// `.dockerignore` is the `*` + `!<dir>` whitelist real monorepos use,
/// because every build says `context: .`.
fn build_the_monorepo(ws: &Workspace) {
    ws.write(
        "compose.yaml",
        "services:\n\
         \x20 builder:\n\
         \x20   build:\n\
         \x20     context: .\n\
         \x20     dockerfile: app/Dockerfile\n\
         \x20   command: [\"sh\", \"-c\", \"sleep 600\"]\n\
         \x20   env_file:\n\
         \x20     - ./app-core-2/service.env\n\
         \x20   volumes:\n\
         \x20     - ./app-core-be/listeners.yml:/conf/listeners.yml:ro\n\
         \x20   configs:\n\
         \x20     - source: app_conf\n\
         \x20       target: /conf/app.conf\n\
         configs:\n\
         \x20 app_conf:\n\
         \x20   file: ./app-core-3/app.conf\n",
    );
    // Every build says `context: .`, so the file has to be a whitelist —
    // this is the shape that was measured, comment and all.
    ws.write(".dockerignore", "# every build is context: .\n*\n!app\n");

    // The build context proper: what a COPY actually reads.
    ws.write("app/Dockerfile", "FROM alpine:3.20\nCOPY app /app\n");
    ws.write("app/hello.txt", "BUILD-CONTEXT-OK");

    // Referenced for reasons a `.dockerignore` has no say over. All three
    // are swallowed by the `*` above.
    ws.write("app-core-be/listeners.yml", "MOUNT-OK");
    ws.write("app-core-2/service.env", "GREETING=ENV-FILE-OK\n");
    ws.write("app-core-3/app.conf", "CONFIG-OK");

    // The same file name, in a repo nothing references. This is the half
    // that must NOT travel, and naming it identically is the point: the
    // rule is about the REASON a path is in the footprint, never about
    // what it is called.
    ws.write("app-core-other/listeners.yml", "SHOULD-NOT-TRAVEL");
    // The rest of a repo whose config IS mounted: the mount buys the one
    // file, not the whole checkout.
    ws.write("app-core-be/src/Bulk.java", "class Bulk {}");

    let junk = ws.project.join("junk/vendor");
    std::fs::create_dir_all(&junk).unwrap();
    std::fs::write(junk.join("blob.bin"), vec![0u8; JUNK_MB * 1024 * 1024]).unwrap();
    std::fs::write(ws.project.join("stray.zip"), vec![0u8; 1024 * 1024]).unwrap();
}

/// Build and run on the server, then find the workspace. `--build` because
/// the whole question is whether the context that arrived is buildable.
fn up_and_read_the_workspace(server: &TestServer, ws: &Workspace) -> String {
    let out = ws
        .ulak(server)
        .args(["docker", "compose", "up", "-d", "--build"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "up -d --build failed — a context narrowed too far breaks HERE:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let out = ws.ulak(server).arg("sync").output().unwrap();
    common::extract_remote_dir(&String::from_utf8_lossy(&out.stderr)).expect("remote dir")
}

/// Scenario one: what the ignore file drops does not reach the server.
fn the_ignore_file_narrows_the_build_context(server: &TestServer, workspace: &str) {
    let listing = listing(server, workspace);
    for gone in ["junk", "stray.zip", "app-core-other"] {
        assert!(
            !listing.lines().any(|l| l == gone),
            "`{gone}` is dropped by the .dockerignore and nothing else asks for it:\n{listing}"
        );
    }
    assert!(
        !remote_exists(server, &format!("{workspace}/app-core-be/src")),
        "a mount buys the file it names, not the whole checkout it sits in"
    );
    assert!(
        !remote_exists(server, &format!("{workspace}/app-core-other/listeners.yml")),
        "the same file name, with no reason to travel, must stay home"
    );

    // The whole point, in one number.
    let du = server.ssh(&format!("du -sm {workspace} | cut -f1"));
    let mb: usize = String::from_utf8_lossy(&du.stdout)
        .trim()
        .parse()
        .unwrap_or(9999);
    assert!(
        mb < JUNK_MB / 2,
        "the workspace is {mb} MB — the {JUNK_MB} MB the build never reads must not have travelled"
    );

    // And the ignore file itself DID travel: without it the server would
    // build from a context nobody narrowed, which is a different image.
    assert!(
        remote_exists(server, &format!("{workspace}/.dockerignore")),
        "docker keeps the ignore file in every context, and so must the workspace"
    );
    assert!(
        remote_exists(server, &format!("{workspace}/app/Dockerfile")),
        "so must the Dockerfile"
    );
}

/// Scenario two: the same rule that dropped the file above lets this one
/// through, because compose named it. Proven where it counts — inside the
/// container, which is the only place a broken mount shows up.
fn every_other_reason_overrides_it(server: &TestServer, ws: &Workspace, workspace: &str) {
    for (rel, why) in [
        ("app-core-be/listeners.yml", "a bind mount"),
        ("app-core-2/service.env", "an env_file"),
        ("app-core-3/app.conf", "a config"),
        ("compose.yaml", "the compose file"),
    ] {
        assert!(
            remote_exists(server, &format!("{workspace}/{rel}")),
            "{rel} is in the footprint as {why} — the .dockerignore has no say over it"
        );
    }

    let seen = exec(
        server,
        ws,
        "cat /conf/listeners.yml /conf/app.conf /app/hello.txt; echo \"$GREETING\"",
    );
    for expected in ["MOUNT-OK", "CONFIG-OK", "BUILD-CONTEXT-OK", "ENV-FILE-OK"] {
        assert!(
            seen.contains(expected),
            "{expected:?} never reached the container — this is what broken \
             mounts looked like:\n{seen}"
        );
    }
    assert!(
        !seen.contains("SHOULD-NOT-TRAVEL"),
        "the unreferenced twin must not be anywhere near the stack:\n{seen}"
    );
}

// ─── helpers ────────────────────────────────────────────────────────

fn listing(server: &TestServer, workspace: &str) -> String {
    let out = server.ssh(&format!("ls -A {workspace}"));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn remote_exists(server: &TestServer, path: &str) -> bool {
    server.ssh(&format!("test -e {path}")).status.success()
}

fn exec(server: &TestServer, ws: &Workspace, script: &str) -> String {
    let out = ws
        .ulak(server)
        .args([
            "docker", "compose", "exec", "-T", "builder", "sh", "-c", script,
        ])
        .output()
        .unwrap();
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn teardown(server: &TestServer, ws: &Workspace, workspace: &str) {
    ws.ulak(server)
        .args([
            "docker",
            "compose",
            "down",
            "-v",
            "--rmi",
            "local",
            "--remove-orphans",
        ])
        .output()
        .ok();
    let hash_dir = Path::new(workspace)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| workspace.to_string());
    server.ssh(&format!("rm -rf {hash_dir}"));
    ws.forget_on_server(server);
    if server.is_real_host() {
        server.ssh("docker rmi alpine:3.20 >/dev/null 2>&1; true");
    }
}
