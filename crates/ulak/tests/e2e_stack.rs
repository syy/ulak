//! `docker stack config` and `docker stack deploy` against a real
//! server: the route whose whole model is read by the CLIENT.
//!
//! Swarm takes a service spec, not a directory. Everything that turns
//! YAML into that spec happens in the docker CLI before the daemon hears
//! a word: the `-c` files are merged and interpolated and every
//! `env_file:` is read into the environment. Under Ulak that CLI runs on
//! the SERVER, so `-c stack.yml` forwarded verbatim is resolved against
//! whatever directory the ssh session landed in — and a server that
//! happens to have a file of that name answers with it, exits 0, and
//! deploys somebody else's model.
//!
//! `Route::Stack` had zero end-to-end coverage until this file, which is
//! the same shape as the gap that hid this review's worst bug:
//! `Route::Footprint` was the only other transport with none, and a
//! `.env` bind source turned out to be silently forwarded to the wrong
//! machine. So the method here is e2e_footprint's, for the same reason —
//! a file of the same name and different BYTES is planted in the
//! server's login directory, and only the bytes tell the two machines
//! apart. An exit status tells them nothing.
//!
//! The one scenario that runs `deploy` never reaches a daemon, and that
//! is deliberate rather than lucky: its model interpolates a variable
//! that `stack` will not read a `.env` for, so docker refuses it in the
//! client with "invalid reference format". Measured on 29.4.0 — that is
//! the exact trap `stack.rs::warn_about_unread_env_files` exists to
//! head off, and it means this suite creates no swarm services even when
//! `ULAK_TEST_E2E=host:<name>` points it at somebody's real swarm manager.
//!
//! Cost: these scenarios share ONE fixture (`TestServer::shared`) and
//! run in parallel, so everything each of them plants on the server
//! carries the scenario's own name. Nothing here starts a container, so
//! nothing here needs an image.

mod common;

use common::{TestServer, Workspace};

/// A workspace pointed at the shared server with no Compose file in it
/// at all.
///
/// A stack project is not a Compose project: `stack` never looks for a
/// `compose.yaml` of its own accord, and leaving the harness's default
/// one in place would let a scenario pass because Compose found
/// something the stack route never asked for.
fn stack_workspace(server: &TestServer) -> Workspace {
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
    ws.set_host(&server.alias);
    ws
}

/// A file of the same name in the server's LOGIN directory, holding
/// different bytes.
///
/// This is the whole method. An ssh session lands in `$HOME`, so a
/// forwarded `-c app/stack.yml` is opened by the server's own docker
/// against `$HOME` — and a server that has such a file answers happily.
/// Planting one turns a silent success into a comparison.
fn plant_a_decoy(server: &TestServer, rel: &str, body: &str) {
    // `printf %s` with the whole YAML document inside one pair of single
    // quotes: these decoys carry newlines and colons, and anything that
    // interpolated them would produce a broken decoy and a green test.
    let wrote = server.ssh(&format!(
        "set -e; mkdir -p \"$(dirname ~/{rel})\"; printf %s {body} > ~/{rel}",
        body = q(body),
    ));
    assert!(
        wrote.status.success(),
        "could not plant the decoy ~/{rel} on the server: {}",
        String::from_utf8_lossy(&wrote.stderr)
    );
}

/// Read a decoy back. A scenario asserts on this as well as on what
/// docker printed: the local model must be the one that was read AND the
/// server's own copy must be left exactly as it was, because a sync that
/// overwrote it would satisfy the first assertion for the wrong reason.
fn decoy_says(server: &TestServer, rel: &str) -> String {
    let out = server.ssh(&format!("cat ~/{rel} 2>/dev/null"));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Everything a scenario leaves on the server, removed however it ends.
/// The backend may be somebody's real host, so the decoys go too.
struct RemoteCleanup<'a> {
    server: &'a TestServer,
    decoys: Vec<String>,
}

impl RemoteCleanup<'_> {
    fn new(server: &TestServer) -> RemoteCleanup<'_> {
        RemoteCleanup {
            server,
            decoys: Vec::new(),
        }
    }
}

impl Drop for RemoteCleanup<'_> {
    fn drop(&mut self) {
        // Silenced and forgiven one at a time: a single `|| true` at the
        // end would let a real failure hide behind an expected one.
        let script = self
            .decoys
            .iter()
            .map(|d| format!("rm -rf ~/{d} >/dev/null 2>&1 || true"))
            .collect::<Vec<_>>()
            .join("; ");
        self.server.ssh(&script);
    }
}

// ─── the model is read here ─────────────────────────────────────────

/// The headline claim: the `-c` file and every `env_file:` it names are
/// read on THIS machine, and what reaches the swarm is built from those
/// bytes.
///
/// `env_file:` is the readout because it is the one part of the model
/// whose CONTENTS come back out of `stack config`: docker inlines them
/// into the service's `environment:`. Measured on 29.4.0 — the compose
/// file's own path is visible in the output too, but a path proves only
/// that the rewrite happened, and this route's failure is about which
/// machine's bytes were read.
#[test]
fn a_stack_config_reads_this_machines_model_and_not_the_servers() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = stack_workspace(&server);
    let dir = unique("model");
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.decoys.push(dir.clone());

    let model = "services:\n  web:\n    image: alpine:3.20\n    env_file:\n      - ./probe.env\n";
    ws.write(&format!("{dir}/stack.yml"), model);
    ws.write(&format!("{dir}/probe.env"), "PROBE=LOCAL-STACK-MODEL\n");
    // The decoy is a WORKING model, not a broken one: a forwarded
    // command has to be able to succeed on it, or the test would be
    // proving that the far side failed rather than that it read us.
    plant_a_decoy(&server, &format!("{dir}/stack.yml"), model);
    plant_a_decoy(&server, &format!("{dir}/probe.env"), "PROBE=SERVER-DECOY\n");

    // The decoy is LIVE, not merely present: the same words, run in the
    // directory an ssh session lands in, answer with the server's bytes
    // and exit 0. Without this line "LOCAL-STACK-MODEL" could be the
    // only answer there ever was, and the scenario below would prove
    // nothing at all.
    let forwarded = server.ssh(&format!("docker stack config -c {dir}/stack.yml"));
    assert!(
        forwarded.status.success()
            && String::from_utf8_lossy(&forwarded.stdout).contains("SERVER-DECOY"),
        "the decoy is not the answer a forwarded command would get, so this scenario \
         cannot tell the two machines apart:\n{}{}",
        String::from_utf8_lossy(&forwarded.stdout),
        String::from_utf8_lossy(&forwarded.stderr)
    );

    let out = ws
        .ulak(&server)
        .args(["docker", "stack", "config", "-c"])
        .arg(format!("{dir}/stack.yml"))
        .output()
        .unwrap();
    assert_ran(&out, "stack config -c <dir>/stack.yml");
    let printed = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        printed.contains("PROBE: LOCAL-STACK-MODEL"),
        "the stack's env_file was read on the wrong machine; the server's own copy of \
         ~/{dir}/probe.env holds {:?} and docker printed:\n{printed}",
        decoy_says(&server, &format!("{dir}/probe.env"))
    );
    assert!(
        !printed.contains("SERVER-DECOY"),
        "the model came from the server's login directory:\n{printed}"
    );

    // The two halves that separate "read the right file" from "wrote
    // over the wrong one": the server's copy is untouched, and the local
    // env file really did travel into the workspace.
    assert_eq!(
        decoy_says(&server, &format!("{dir}/probe.env")),
        "PROBE=SERVER-DECOY\n",
        "the sync wrote over the server's login directory instead of the workspace"
    );
    // Named from the workspace ROOT, which for a stack is the first
    // compose file's own directory: every path this model resolves hangs
    // off that directory, so it is what the footprint anchors on and the
    // env file lands beside the compose file rather than under a mirror
    // of the local tree.
    assert_eq!(
        remote_read(&ws, &server, "probe.env"),
        "PROBE=LOCAL-STACK-MODEL\n",
        "the env file never travelled — the stack's footprint did not name it"
    );

    ws.forget_on_server(&server);
}

/// Docker's own rule, measured on 29.4.0 and impossible to get right by
/// accident: a relative path inside ANY `-c` file resolves against the
/// FIRST file's directory — not against that file's own directory, and
/// not against the cwd.
///
/// Both candidates exist here, holding different bytes, so the answer
/// cannot be arrived at by finding the only file there is. Get this
/// wrong and ulak syncs one file and rewrites the `-c` list so the
/// server reads another, which is a stack deployed from a model nobody
/// wrote.
#[test]
fn the_first_compose_file_decides_where_a_later_ones_paths_point() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = stack_workspace(&server);
    let dir = unique("twofile");
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.decoys.push(dir.clone());

    ws.write(
        &format!("{dir}/stack.yml"),
        "services:\n  web:\n    image: alpine:3.20\n",
    );
    // Lives at the project root, names `./second.env`, and docker
    // resolves that against `<dir>/` — where the first file is.
    ws.write(
        "second.yml",
        "services:\n  web:\n    env_file:\n      - ./second.env\n",
    );
    ws.write(
        &format!("{dir}/second.env"),
        "FROM=THE-FIRST-FILES-DIRECTORY\n",
    );
    ws.write("second.env", "FROM=THE-CWD-WRONG\n");

    let out = ws
        .ulak(&server)
        .args(["docker", "stack", "config", "-c"])
        .arg(format!("{dir}/stack.yml"))
        .args(["-c", "second.yml"])
        .output()
        .unwrap();
    assert_ran(&out, "stack config -c <dir>/stack.yml -c second.yml");
    let printed = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        printed.contains("FROM: THE-FIRST-FILES-DIRECTORY"),
        "a later -c file's relative path was resolved against the wrong directory:\n{printed}"
    );
    assert!(
        !printed.contains("THE-CWD-WRONG"),
        "the second file's env_file was resolved against the cwd:\n{printed}"
    );

    ws.forget_on_server(&server);
}

// ─── the `.env` that is never read ──────────────────────────────────

/// The trap this route cannot fix, only refuse to be quiet about.
///
/// `stack` substitutes from the process environment alone — measured on
/// 29.4.0 in both places a user would put one, a `.env` beside the
/// compose file and a `.env` in the cwd. On the server that environment
/// is the ssh session's, and nothing of this shell's travels, so
/// `image: app:${TAG}` becomes `app:` and docker reports "invalid
/// reference format" while naming neither the variable nor the reason.
///
/// So the two halves are asserted in one run: the substitution really
/// is empty (docker's own refusal, which also proves the model came from
/// HERE — the decoy on the server would have failed differently), and
/// the user was told which file is not going to be read before docker
/// ever got the chance to be cryptic about it.
#[test]
fn a_stack_is_told_which_env_file_will_not_be_read_before_docker_is_cryptic() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = stack_workspace(&server);
    let dir = unique("unread");
    let mut cleanup = RemoteCleanup::new(&server);
    cleanup.decoys.push(dir.clone());

    ws.write(
        &format!("{dir}/stack.yml"),
        "services:\n  web:\n    image: app:${TAG}\n",
    );
    ws.write(&format!("{dir}/.env"), "TAG=1.2.3\n");
    // A decoy that fails in a DIFFERENT sentence, so the two machines'
    // answers cannot be confused — and one that can never deploy
    // anything, because this scenario has to be safe against a backend
    // that really is a swarm manager.
    plant_a_decoy(
        &server,
        &format!("{dir}/stack.yml"),
        "services:\n  web:\n    image: alpine:3.20\n    env_file:\n      - ./nowhere.env\n",
    );

    let name = unique("stack");
    let out = ws
        .ulak(&server)
        .args(["docker", "stack", "deploy", "-c"])
        .arg(format!("{dir}/stack.yml"))
        .arg(&name)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        !out.status.success(),
        "`app:${{TAG}}` with nothing to substitute is not a deployable image; it exited {:?}\n{said}",
        out.status.code()
    );
    assert!(
        said.contains("invalid reference format"),
        "docker had to be the one to refuse this, from OUR model — the decoy on the server \
         names a missing env file and would have said something else:\n{said}"
    );
    assert!(
        !said.contains("nowhere.env"),
        "the deploy read the decoy in the server's login directory:\n{said}"
    );
    assert!(
        said.contains(&format!("{dir}/.env"))
            && said.contains("not read when this stack's variables are substituted"),
        "the file that is not going to be read has to be named:\n{said}"
    );
    for advice in ["ssh session", "empty string", "export"] {
        assert!(
            said.contains(advice),
            "naming the file without naming the way out leaves the user with a fact and no \
             move — {advice:?} is missing from:\n{said}"
        );
    }

    // And the same model through `config`, which is where the emptiness
    // is visible rather than merely fatal: docker prints `app:` and says
    // nothing at all. That silence is the whole reason the warning above
    // has to exist, so it is pinned here rather than described.
    let out = ws
        .ulak(&server)
        .args(["docker", "stack", "config", "-c"])
        .arg(format!("{dir}/stack.yml"))
        .output()
        .unwrap();
    assert_ran(&out, "stack config -c <dir>/stack.yml");
    let printed = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        printed.contains("image: 'app:'"),
        "docker substitutes the empty string and prints it without comment; if that has \
         changed, the warning's wording has to change with it:\n{printed}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not read when this stack's variables"),
        "`config` reads the model the same way `deploy` does, so it owes the same warning:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    ws.forget_on_server(&server);
}

// ─── the shape that must NOT be touched ─────────────────────────────

/// A stack that names no compose file has no model for ulak to resolve,
/// and docker's own complaint is the right one to hear.
///
/// The claim worth an e2e is the second one: nothing is synced and no
/// workspace is made. A route that resolved a model out of habit would
/// push a tree for a command that reads no files at all, and the only
/// place that is visible is out here.
#[test]
fn a_stack_that_names_no_compose_file_is_handed_over_untouched() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = stack_workspace(&server);

    let out = ws
        .ulak(&server)
        .args(["docker", "stack", "config"])
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "docker refuses a stack with no -c, and so must the round trip:\n{said}"
    );
    // Case-folded because the sentence is docker's and its first letter
    // has already changed once between the client this was measured on
    // and the one in the fixture.
    assert!(
        said.to_lowercase().contains("specify a compose file"),
        "the complaint has to be docker's own, not one of ours:\n{said}"
    );
    assert!(
        ws.workspace_ids().is_empty(),
        "a command that reads no files resolved a model and claimed a workspace: {:?}",
        ws.workspace_ids()
    );

    ws.forget_on_server(&server);
}

// ─── helpers ────────────────────────────────────────────────────────

/// A name no other scenario on this shared server can be using: the
/// suite's own prefix, the process id (two checkouts, one host) and a
/// counter (two scenarios in one process).
fn unique(what: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "ulak-st-{what}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Fail with BOTH streams. A `stack config` that never reached the
/// daemon comes back with empty stdout, and the content assertion that
/// follows would then blame the bytes instead of the connection.
fn assert_ran(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "`ulak docker {what}` failed with {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A file in this workspace as it exists ON THE SERVER — which separates
/// "the footprint never named it" from "the model was read elsewhere".
fn remote_read(ws: &Workspace, server: &TestServer, rel: &str) -> String {
    let ids = ws.workspace_ids();
    let id = ids.first().expect("a workspace was registered");
    let root = ws.remote_workspace_root(id);
    // The directory under the workspace id is named for the PROJECT, and
    // a stack's project name is whatever `Invocation` settled on rather
    // than anything this test chose — so it is globbed rather than
    // spelled, which also keeps the failure message about the file.
    let out = server.ssh(&format!("cat {root}/*/{rel}"));
    assert!(
        out.status.success(),
        "{rel} is not in the remote workspace either; it holds:\n{}",
        String::from_utf8_lossy(&server.ssh(&format!("ls -aR {root} 2>&1")).stdout)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// One level of shell quoting for the scripts the harness hands to the
/// server's own shell.
fn q(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}
