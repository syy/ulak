//! Getting out from under a server that no longer exists.
//!
//! Reproduced on a real machine before any of this existed: a user
//! destroyed the server their stack was declared on and pointed the
//! checkout at a new one. Every root command kept addressing the corpse —
//! `ulak config` printed the new host while `ulak doctor` one directory
//! away timed out against the old one — and nothing in the product could
//! end it. `down` needs the server; `clean` refuses while the declaration
//! is live and its next steps lead back to `down`. The only way out was
//! `rm -rf ~/.local/state/ulak/stacks/<id>`.
//!
//! The offline half needs no server, deliberately: the failure it guards
//! is what happens when there is not one. 192.0.2.1 is the RFC 5737
//! documentation range, which nothing routes — it swallows the SYN, the
//! shape of a destroyed host, where a name that fails to resolve would
//! return instantly and prove nothing about deadlines.
//!
//! The accepting half needs a real declaration, and a declaration can only
//! be made by reaching a server: the stack is brought up on the fixture,
//! the config is then pointed elsewhere, and the drift plus its retirement
//! are exercised against the declaration `up` actually wrote.

mod common;

use std::time::{Duration, Instant};

use common::{TestServer, Workspace, desired};

/// Two ssh attempts at ConnectTimeout=10 plus process start is ~20s, and
/// the point of the offline path is that it spends NONE of that. Ten
/// seconds is far under one attempt and far over a local file walk.
const OFFLINE: Duration = Duration::from_secs(10);

const IMAGE: &str = "nginx:alpine";

fn unreachable_workspace() -> Workspace {
    let ws = Workspace::new();
    ws.set_host("192.0.2.1");
    ws
}

/// Every file below `root` with its bytes, so "nothing under here changed"
/// is compared exactly rather than by directory names.
fn files_under(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.push((path, bytes));
            }
        }
    }
    out.sort();
    out
}

fn both_streams(out: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The refusal has to say what this checkout DOES know, or a user who
/// mistyped the host has been handed another dead end — the entire class
/// of bug this flag exists to end.
#[test]
fn forgetting_a_destination_this_checkout_never_declared_says_so_and_stays_local() {
    let ws = unreachable_workspace();
    let started = Instant::now();
    let out = ws
        .ulak_alone()
        .args(["clean", "--forget-destination", "some-dead-server"])
        .output()
        .unwrap();
    let took = started.elapsed();

    assert!(
        took < OFFLINE,
        "the offline forget waited {took:?} — it reached for the server it exists to give up on"
    );
    assert!(
        !out.status.success(),
        "nothing was declared, so nothing can be retired"
    );
    let said = both_streams(&out);
    assert!(
        said.contains("some-dead-server"),
        "the refusal must quote the destination as typed: {said}"
    );
    assert!(
        said.contains("declared no stack at all") || said.contains("declared here:"),
        "a refusal with no next step is the bug, not the fix: {said}"
    );
}

/// Compose globals select by file; the flag selects by server. Silently
/// ignoring `-f` would let a user believe they had scoped the command.
#[test]
fn compose_globals_are_refused_rather_than_ignored_by_the_offline_forget() {
    let ws = unreachable_workspace();
    let out = ws
        .ulak_alone()
        .args([
            "-f",
            "compose.yaml",
            "clean",
            "--forget-destination",
            "some-dead-server",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        said.contains("would be ignored") && said.contains("compose.yaml"),
        "the refusal must quote the globals it is refusing: {said}"
    );
    assert!(
        said.contains("--forget-destination some-dead-server"),
        "and hand back the command that does work: {said}"
    );
}

/// The exact symptom that was reported. `fetch_facts` used to abort the
/// whole report with `?`, so `ok server <host>` was the LAST line a user
/// with a destroyed server ever saw: no SERVER rows, no problem count, no
/// rerun line. A report that stops mid-sentence reads as a broken tool
/// rather than a diagnosis.
#[test]
fn doctor_against_a_server_that_never_answers_still_finishes_its_report() {
    let ws = unreachable_workspace();
    let out = ws.ulak_alone().arg("doctor").output().unwrap();

    let report = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        report.contains("192.0.2.1"),
        "the destination has to be named: {report}"
    );
    assert!(
        report.contains("problem(s)"),
        "doctor must reach its own summary even when the server is gone: {report}"
    );
    assert!(
        report.contains("then rerun: ulak") && report.trim_end().ends_with("doctor"),
        "and end with the rerun line every other doctor run ends with: {report}"
    );
    assert!(
        !report.contains("ok   server         192.0.2.1\n"),
        "an unqualified `ok server` reads as a working connection: {report}"
    );
    assert!(
        report.contains("did not answer") || report.contains("could not be reached"),
        "the server rows must say what happened, not go missing: {report}"
    );
}

/// The checkout this flag exists for has often lost more than its server:
/// the `-f` file the declaration named may be gone or renamed by the time
/// anyone tries to retire it. A retirement that needed to rebuild that
/// invocation answered "nothing declared" and offered itself again — the
/// declaration is written by hand here, with a Compose file that does not
/// exist, so the escape is proven on exactly that shape.
#[test]
fn a_declaration_whose_compose_file_is_gone_can_still_be_retired() {
    let ws = unreachable_workspace();
    let checkout = ws.project.canonicalize().unwrap();
    let dir = ws.stacks_dir().join("handwritten");
    std::fs::create_dir_all(&dir).unwrap();
    let declaration = serde_json::json!({
        "schema": 3,
        "live": true,
        "workspace_id": "abc123",
        "workspace_namespace": "client-test",
        "destination": "dead-server",
        "identity": "vanished",
        "cwd": checkout.display().to_string(),
        "argv_globals": ["-f", checkout.join("gone.yaml").display().to_string()],
        "compose_env": {},
        "updated_unix": 1
    });
    std::fs::write(dir.join("desired.json"), declaration.to_string()).unwrap();
    // A second stack declared from the same directory through another first
    // `-f` file lives in another sync workspace. One retirement by server
    // owes both; refusing used to point back at this very directory.
    let other = ws.stacks_dir().join("handwritten-too");
    std::fs::create_dir_all(&other).unwrap();
    let mut second = declaration.clone();
    second["workspace_id"] = serde_json::json!("def456");
    second["identity"] = serde_json::json!("vanished-too");
    second["live"] = serde_json::json!(false);
    std::fs::write(other.join("desired.json"), second.to_string()).unwrap();

    let started = Instant::now();
    let out = ws
        .ulak_alone()
        .args(["clean", "--forget-destination", "dead-server"])
        .output()
        .unwrap();
    let took = started.elapsed();
    let said = both_streams(&out);
    assert!(out.status.success(), "{said}");
    assert!(took < OFFLINE, "the offline forget waited {took:?}");
    assert!(
        said.contains("vanished") && said.contains("declared live"),
        "{said}"
    );
    assert!(
        said.contains("vanished-too") && said.contains("already idle"),
        "both workspaces' declarations are retired in one go: {said}"
    );
    assert!(
        !dir.join("desired.json").exists() && !other.join("desired.json").exists(),
        "the declarations are gone even though their Compose file never existed"
    );
}

/// The flag is the escape hatch; a user who cannot find it has none.
#[test]
fn clean_documents_the_offline_escape_in_its_own_help() {
    let ws = Workspace::new();
    let out = ws.ulak_alone().args(["clean", "--help"]).output().unwrap();
    let help = String::from_utf8_lossy(&out.stdout).to_string();

    assert!(out.status.success(), "{help}");
    assert!(help.contains("--forget-destination"), "{help}");
    assert!(
        help.contains("SSH_DEST"),
        "it takes the server by name, never a bare --force: {help}"
    );
}

/// A stack declared on one server, a config that now names another: the
/// declaration keeps winning, and every surface says so. Then the
/// declaration is retired offline and the checkout is unwedged — with its
/// workspace claim, and therefore its ledger, left exactly where it was.
///
/// The accepting case for the whole feature, on the real declaration `up`
/// writes rather than one a test composed by hand.
#[test]
fn a_declared_server_outranks_the_config_until_it_is_retired_offline() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image(IMAGE);
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let identity = format!("ulak-e2e-forget-{}", std::process::id());

    // Armed before the first assertion: a partially failed `up` can leave
    // containers behind, and teardown must not depend on anything this test
    // is about to break — it restores the alias so `down` binds to the
    // server that has the stack.
    struct Down<'a> {
        ws: &'a Workspace,
        server: &'a TestServer,
        identity: &'a str,
    }
    impl Drop for Down<'_> {
        fn drop(&mut self) {
            self.ws.set_host(&self.server.alias);
            self.ws
                .ulak(self.server)
                .args([
                    "docker",
                    "compose",
                    "-p",
                    self.identity,
                    "down",
                    "--remove-orphans",
                    "--volumes",
                ])
                .output()
                .ok();
            self.ws.forget_on_server(self.server);
        }
    }
    let _down = Down {
        ws: &ws,
        server: &server,
        identity: &identity,
    };
    let up = ws
        .ulak(&server)
        .args(["docker", "compose", "-p", &identity, "up", "-d"])
        .output()
        .unwrap();
    assert!(up.status.success(), "up failed:\n{}", both_streams(&up));
    let declared = common::desired_for(&ws, &identity).expect("up declares the stack");
    assert_eq!(declared["live"], true);
    assert_eq!(declared["destination"], server.alias);

    // The config now names a server that will never answer. The declared
    // one still does, and the declaration must keep addressing it.
    ws.set_host("192.0.2.1");
    let status = ws.ulak(&server).arg("status").output().unwrap();
    let said = both_streams(&status);
    assert!(
        status.status.success(),
        "status followed the config to a dead server instead of the declaration:\n{said}"
    );
    assert!(
        !said.contains("did not answer") && !said.contains("192.0.2.1 true"),
        "status must not have tried 192.0.2.1 at all:\n{said}"
    );
    assert!(
        said.contains(&server.alias) && said.contains("192.0.2.1"),
        "the drift must name both servers:\n{said}"
    );
    assert!(
        said.contains(&format!("--forget-destination {}", server.alias)),
        "the way out must be spelled where the user is stuck:\n{said}"
    );
    assert!(
        said.contains(&format!("env ULAK_HOST={} ulak", server.alias)),
        "a pasted `down` must carry the declared server, or it binds to the config:\n{said}"
    );

    // Doctor under the same drift: the server row is BAD and names both,
    // the declared server is still contacted (the drift must not trip the
    // "remote half would only mislead" gate), and the summary lists it.
    let doctor = ws.ulak(&server).arg("doctor").output().unwrap();
    let report = String::from_utf8_lossy(&doctor.stdout).to_string();
    assert!(
        report.contains("BAD  server") && report.contains("192.0.2.1"),
        "doctor must mark the drifted server row:\n{report}"
    );
    assert!(
        report.contains("connected as"),
        "doctor must still reach the declared server:\n{report}"
    );
    assert!(
        report.contains("problem(s)") && report.contains("--forget-destination"),
        "the summary must carry the drift and its way out:\n{report}"
    );

    // And the report the user reached for: `ulak config` shows the host the
    // layers name AND the declaration that outranks it here.
    let config = ws.ulak_alone().arg("config").output().unwrap();
    let shown = String::from_utf8_lossy(&config.stdout).to_string();
    assert!(
        shown.contains("DECLARED") && shown.contains(&format!("on {}", server.alias)),
        "config must show the declaration beside the host:\n{shown}"
    );

    // Snapshotted right before the forget and after every command above:
    // `status` and `doctor` each push, and a push writes receipts here, so
    // an earlier snapshot would blame the retirement for their bytes.
    let receipts_before = files_under(&ws.workspaces_dir());
    assert!(
        !receipts_before.is_empty(),
        "the accepting control has no workspace claim or receipt to keep"
    );
    let started = Instant::now();
    let forget = ws
        .ulak_alone()
        .args(["clean", "--forget-destination", &server.alias])
        .output()
        .unwrap();
    let took = started.elapsed();
    let said = both_streams(&forget);
    assert!(forget.status.success(), "{said}");
    assert!(
        took < OFFLINE,
        "the offline forget waited {took:?} — it reached for a server"
    );
    assert!(
        said.contains(&identity) && said.contains("declared live"),
        "the receipt names the retired stack and that it was live:\n{said}"
    );
    assert!(
        said.contains("not contacted") && said.contains("nothing on it was deleted"),
        "{said}"
    );

    assert!(
        common::desired_for(&ws, &identity).is_none() && desired(&ws).is_none(),
        "the declaration is gone"
    );
    assert_eq!(
        files_under(&ws.workspaces_dir()),
        receipts_before,
        "every receipt under workspaces/ — the ledger above all — must survive a retirement byte for byte"
    );

    // The same command again has nothing left to retire, and says what it
    // does know rather than pretending to succeed.
    let again = ws
        .ulak_alone()
        .args(["clean", "--forget-destination", &server.alias])
        .output()
        .unwrap();
    assert!(!again.status.success());
    assert!(
        both_streams(&again).contains("declared no stack at all"),
        "{}",
        both_streams(&again)
    );
}
