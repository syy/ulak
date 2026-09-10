//! Ulak's root management commands, after a non-standard Compose
//! invocation has declared a stack.
//!
//! Docker deliberately does not remember `-f` or `-p`: a later bare
//! `docker compose` invocation performs Docker's ordinary discovery again.
//! Ulak's own commands answer a different question. A `desired.json` is a
//! validated, complete declaration of the stack this workspace is already
//! maintaining; losing it makes `status` fall out to the fleet, makes
//! `doctor` silently skip Compose, and leaves `sync` and `clean` at a dead
//! end unless every selector is typed again.
//!
//! These scenarios never manufacture that declaration. Each first runs an
//! explicit, successful `up`, then omits the selectors only from Ulak's own
//! command. One scenario also leaves a standard Compose file in an ancestor:
//! bare Docker must discover that decoy while root `status` stays with the
//! declared child. That pair is the boundary the fix must preserve.
//!
//! The tests share one server and therefore give every Compose project a
//! scenario-specific `-p` name. The only image they run is handed to the
//! server by `needs_image`; nothing here pulls from a registry.

mod common;

use std::path::PathBuf;
use std::process::Output;
use std::sync::Arc;

use common::{TestServer, Workspace, desired_for, extract_remote_dir};

const COMPOSE_FILE: &str = "compose.context.yaml";
const IMAGE: &str = "redis:7.2";

/// One real declaration plus enough information to repeat it explicitly.
///
/// Teardown owns the same exact invocation. It is deliberately a value,
/// rather than a last line in each test, so a red assertion still takes the
/// scenario's container and remote workspace off a real host.
struct DeclaredProject {
    server: Arc<TestServer>,
    ws: Workspace,
    cwd: PathBuf,
    compose_file: String,
    payload: String,
    identity: String,
    up: Output,
}

impl DeclaredProject {
    fn flat(server: Arc<TestServer>, scenario: &str) -> DeclaredProject {
        Self::new(server, scenario, false)
    }

    fn nested_below_a_compose_project(server: Arc<TestServer>, scenario: &str) -> DeclaredProject {
        Self::new(server, scenario, true)
    }

    fn new(server: Arc<TestServer>, scenario: &str, nested: bool) -> DeclaredProject {
        server.needs_image(IMAGE);
        let ws = Workspace::new();
        ws.set_host(&server.alias);

        let (cwd, compose_rel, payload) = if nested {
            // This is a working decoy, not merely a file with the right
            // name. The test below first proves that bare Docker really
            // discovers it from the child directory.
            ws.write(
                "compose.yaml",
                &format!("services:\n  ancestor_decoy:\n    image: {IMAGE}\n"),
            );
            ws.write(
                "child/ulak.local.toml",
                &format!("host = {:?}\n", server.alias),
            );
            (
                ws.project.join("child"),
                format!("child/{COMPOSE_FILE}"),
                "child/payload/probe.txt".to_string(),
            )
        } else {
            std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
            (
                ws.project.clone(),
                COMPOSE_FILE.to_string(),
                "payload/probe.txt".to_string(),
            )
        };

        ws.write(
            &compose_rel,
            &format!(
                "services:\n  desired_probe:\n    image: {IMAGE}\n    volumes:\n      \
                 - ./payload:/payload:ro\n"
            ),
        );
        ws.write(&payload, "BEFORE\n");

        let identity = format!("ulak-e2e-management-{scenario}-{}", std::process::id());
        let compose_file = COMPOSE_FILE.to_string();
        let up = ws
            .ulak(&server)
            .current_dir(&cwd)
            .args([
                "docker",
                "compose",
                "-f",
                &compose_file,
                "-p",
                &identity,
                "up",
                "-d",
            ])
            .output()
            .unwrap();

        DeclaredProject {
            server,
            ws,
            cwd,
            compose_file,
            payload,
            identity,
            up,
        }
    }

    /// The explicit accepting case is part of every regression: if it did
    /// not create a valid declaration, a later bare command would prove
    /// nothing about whether that declaration can be recovered.
    fn assert_declared(&self) {
        assert!(
            self.up.status.success(),
            "the explicit declaration failed:\n{}{}",
            String::from_utf8_lossy(&self.up.stdout),
            String::from_utf8_lossy(&self.up.stderr)
        );
        let desired = desired_for(&self.ws, &self.identity)
            .unwrap_or_else(|| panic!("up did not declare {}", self.identity));
        assert_eq!(desired["live"], true, "the accepting control is not live");
        let globals = desired["argv_globals"]
            .as_array()
            .expect("desired argv_globals");
        assert!(
            globals.iter().any(|word| {
                word.as_str()
                    .is_some_and(|word| word.ends_with(&self.compose_file))
            }),
            "the exact custom Compose file was not recorded: {desired}"
        );
        assert_eq!(desired["identity"], self.identity);
    }

    fn bare(&self, command: &str) -> Output {
        self.ws
            .ulak(&self.server)
            .current_dir(&self.cwd)
            .arg(command)
            .output()
            .unwrap()
    }

    fn explicit_management(&self, command: &str) -> Output {
        self.ws
            .ulak(&self.server)
            .current_dir(&self.cwd)
            .args(["-f", &self.compose_file, "-p", &self.identity, command])
            .output()
            .unwrap()
    }

    fn docker_compose(&self, tail: &[&str]) -> Output {
        self.ws
            .ulak(&self.server)
            .current_dir(&self.cwd)
            .args(["docker", "compose"])
            .args(tail)
            .output()
            .unwrap()
    }

    fn docker_compose_as(&self, identity: &str, tail: &[&str]) -> Output {
        self.ws
            .ulak(&self.server)
            .current_dir(&self.cwd)
            .args([
                "docker",
                "compose",
                "-f",
                &self.compose_file,
                "-p",
                identity,
            ])
            .args(tail)
            .output()
            .unwrap()
    }

    fn write_payload(&self, text: &str) {
        self.ws.write(&self.payload, text);
    }

    fn remote_payload(&self, remote_dir: &str) -> String {
        let out = self
            .server
            .ssh(&format!("cat {remote_dir}/payload/probe.txt"));
        assert!(
            out.status.success(),
            "the remote payload could not be read:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for DeclaredProject {
    fn drop(&mut self) {
        self.ws
            .ulak(&self.server)
            .current_dir(&self.cwd)
            .args([
                "docker",
                "compose",
                "-f",
                &self.compose_file,
                "-p",
                &self.identity,
                "down",
                "--remove-orphans",
                "--volumes",
            ])
            .output()
            .ok();
        self.ws.forget_on_server(&self.server);
    }
}

/// A second identity over the same invocation, owned separately so an
/// ambiguity assertion cannot leave its container behind on a real host.
struct ExtraStack<'a> {
    project: &'a DeclaredProject,
    identity: String,
}

impl Drop for ExtraStack<'_> {
    fn drop(&mut self) {
        let _ = self
            .project
            .docker_compose_as(&self.identity, &["down", "--remove-orphans", "--volumes"]);
    }
}

/// A declaration made from this directory is the one project-level status
/// can describe. Falling out to the fleet loses its compose files, footprint
/// and remote workspace even though all of them are present in Desired.
#[test]
fn bare_status_reuses_a_declared_nonstandard_compose_context() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let project = DeclaredProject::flat(server, "status");
    project.assert_declared();

    let out = project.bare("status");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "status failed:\n{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains(&format!("identity  {}", project.identity)) && text.contains(COMPOSE_FILE),
        "status lost the declared project's exact Compose context:\n{text}"
    );
    assert!(
        !text.contains("this machine") && !text.contains("details for one"),
        "status answered the fleet question from inside its declared project:\n{text}"
    );
}

/// Compose-free doctor is a useful command in a genuinely Compose-free
/// workspace. It is not a valid substitute once this directory has declared
/// an exact Compose model: silently skipping that model is a false diagnosis.
#[test]
fn bare_doctor_checks_the_declared_nonstandard_compose_model() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let project = DeclaredProject::flat(server, "doctor");
    project.assert_declared();

    let out = project.bare("doctor");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "bare doctor failed:\n{text}");
    assert!(
        text.contains("compose files")
            && text.contains(COMPOSE_FILE)
            && text.contains("COMPOSE REFERENCES"),
        "doctor did not inspect the model recorded by the declaration:\n{text}"
    );
    assert!(
        !text.contains("not configured — skipped"),
        "doctor silently downgraded a declared Compose project:\n{text}"
    );
}

/// The accepting control names the exact invocation once; a later bare sync
/// must move the bytes belonging to that same footprint. Exit status alone is
/// not evidence here — the server's copy has to contain the new bytes.
#[test]
fn bare_sync_uses_the_declared_nonstandard_compose_footprint() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let project = DeclaredProject::flat(server, "sync");
    project.assert_declared();

    let control = project.explicit_management("sync");
    assert!(
        control.status.success(),
        "the explicit sync control failed:\n{}{}",
        String::from_utf8_lossy(&control.stdout),
        String::from_utf8_lossy(&control.stderr)
    );
    let remote_dir = extract_remote_dir(&String::from_utf8_lossy(&control.stderr))
        .expect("explicit sync must name its remote directory");
    assert_eq!(project.remote_payload(&remote_dir), "BEFORE\n");

    project.write_payload("AFTER\n");
    let bare = project.bare("sync");
    let remote = project.remote_payload(&remote_dir);
    assert_eq!(
        remote,
        "AFTER\n",
        "bare sync did not use the declared footprint; it said:\n{}{}",
        String::from_utf8_lossy(&bare.stdout),
        String::from_utf8_lossy(&bare.stderr)
    );
    assert!(
        bare.status.success(),
        "the bytes travelled but sync still failed:\n{}{}",
        String::from_utf8_lossy(&bare.stdout),
        String::from_utf8_lossy(&bare.stderr)
    );
}

/// `clean` must resolve the same workspace before applying its independent
/// intent and Docker-label safety gates. With a live declaration its safe
/// answer is a named refusal, never "no compose file".
#[test]
fn bare_clean_sees_the_live_stack_declared_by_a_nonstandard_context() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let project = DeclaredProject::flat(server, "clean");
    project.assert_declared();

    let out = project.bare("clean");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "clean must refuse a live stack:\n{text}"
    );
    assert!(
        text.contains("workspace is still used by 1 live Docker stack")
            && text.contains(&project.identity)
            && text.contains(COMPOSE_FILE)
            && text.contains("--project-directory"),
        "clean did not reach the live-stack safety gate for the declared workspace:\n{text}"
    );
}

/// Docker's ancestor discovery remains Docker's answer. The same ancestor
/// must not steal Ulak's root management command from a child that already
/// has one exact, validated declaration of its own.
#[test]
fn an_ancestor_compose_file_cannot_steal_a_declared_child_management_context() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let project = DeclaredProject::nested_below_a_compose_project(server, "ancestor");
    project.assert_declared();

    let docker = project.docker_compose(&["config"]);
    let docker_text = String::from_utf8_lossy(&docker.stdout);
    assert!(
        docker.status.success() && docker_text.contains("ancestor_decoy"),
        "the ancestor is not a live Docker-discovery decoy:\n{docker_text}{}",
        String::from_utf8_lossy(&docker.stderr)
    );
    assert!(
        !docker_text.contains("desired_probe"),
        "bare Docker unexpectedly remembered the child's earlier -f:\n{docker_text}"
    );

    let status = project.bare("status");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        status.status.success(),
        "child status failed instead of selecting its declaration:\n{text}"
    );
    assert!(
        text.contains(COMPOSE_FILE)
            && text.contains(&format!("identity  {}", project.identity))
            && !text.contains("ancestor_decoy"),
        "the ancestor Compose project stole the child's management command:\n{text}"
    );
}

/// Two equally specific declarations are two real Docker stacks, not two
/// spellings of one answer. Every management command must refuse to guess;
/// most importantly, `clean` must never choose one workspace by accident.
#[test]
fn equally_specific_declarations_are_refused_with_exact_choices() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let project = DeclaredProject::flat(server, "ambiguous-a");
    project.assert_declared();
    let second_identity = format!("ulak-e2e-management-ambiguous-b-{}", std::process::id());
    let second = ExtraStack {
        project: &project,
        identity: second_identity.clone(),
    };
    let up = project.docker_compose_as(&second.identity, &["up", "-d"]);
    assert!(
        up.status.success(),
        "the second explicit declaration failed:\n{}{}",
        String::from_utf8_lossy(&up.stdout),
        String::from_utf8_lossy(&up.stderr)
    );
    assert_eq!(
        desired_for(&project.ws, &second.identity).map(|desired| desired["live"].clone()),
        Some(serde_json::Value::Bool(true)),
        "the accepting control did not record the second stack"
    );

    let out = project.bare("status");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "an ambiguous status guessed:\n{text}"
    );
    assert!(
        text.contains("more than one declared Compose project")
            && text.contains(&project.identity)
            && text.contains(&second.identity),
        "the ambiguity did not name both choices:\n{text}"
    );
    assert!(
        text.matches(COMPOSE_FILE).count() >= 2 && text.matches("--project-directory").count() >= 2,
        "the choices do not carry both exact invocations:\n{text}"
    );

    let sync = project
        .ws
        .ulak(&project.server)
        .current_dir(&project.cwd)
        .args(["sync", "--dry-run", "--max-delete", "7"])
        .output()
        .unwrap();
    let sync_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&sync.stdout),
        String::from_utf8_lossy(&sync.stderr)
    );
    assert!(
        !sync.status.success(),
        "an ambiguous sync guessed:\n{sync_text}"
    );
    assert!(
        sync_text.matches("--dry-run").count() >= 2
            && sync_text.matches("--max-delete").count() >= 2
            && sync_text.matches(" 7").count() >= 2,
        "the choices changed the requested dry-run into a real sync:\n{sync_text}"
    );
}

/// Process environment is input to this invocation, not remembered state.
/// Ignoring it would make `COMPOSE_PROJECT_NAME=other ulak clean` address
/// the old declared stack even though Docker and the old root path addressed
/// `other`, which is an unsafe precedence reversal.
#[test]
fn current_compose_environment_wins_over_a_declared_context() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let project = DeclaredProject::flat(server, "environment-declared");
    project.assert_declared();
    let current_identity = format!(
        "ulak-e2e-management-environment-current-{}",
        std::process::id()
    );

    let out = project
        .ws
        .ulak(&project.server)
        .current_dir(&project.cwd)
        .env("COMPOSE_FILE", &project.compose_file)
        .env("COMPOSE_PROJECT_NAME", &current_identity)
        .arg("status")
        .output()
        .unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "environment-selected status failed:\n{text}"
    );
    assert!(
        text.contains(&format!("identity  {current_identity}"))
            && !text.contains(&format!("identity  {}", project.identity)),
        "the remembered declaration overrode this invocation's environment:\n{text}"
    );
}

/// `ulak config` reports the nearest Ulak configuration boundary; Compose
/// discovery is irrelevant to that local question. An ancestor Compose file
/// must not make the child report its parent's host or project name.
#[test]
fn config_uses_the_nearest_ulak_workspace_below_an_ancestor_compose_project() {
    let ws = Workspace::new();
    ws.write("ulak.local.toml", "host = \"ancestor-server\"\n");
    ws.write("child/ulak.local.toml", "host = \"child-server\"\n");
    ws.write(
        &format!("child/{COMPOSE_FILE}"),
        &format!("services:\n  child:\n    image: {IMAGE}\n"),
    );
    let child = ws.project.join("child");

    let out = ws
        .ulak_alone()
        .current_dir(&child)
        .args(["config", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "config failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let host = report["settings"]
        .as_array()
        .and_then(|settings| settings.iter().find(|row| row["key"] == "host"))
        .and_then(|row| row["value"].as_str());
    assert_eq!(
        host,
        Some("child-server"),
        "config crossed the nearest Ulak workspace boundary: {report}"
    );
    assert_eq!(
        report["project"], "child",
        "config named the ancestor Compose project: {report}"
    );
}

/// `config` normally reports the nearest Ulak workspace, but an owned
/// Compose environment names the current invocation explicitly. It must
/// keep the same precedence it had before workspace-boundary discovery was
/// added, rather than silently reporting the child configuration.
#[test]
fn config_honors_current_compose_environment_over_the_local_workspace() {
    let ws = Workspace::new();
    ws.write("ulak.local.toml", "host = \"ancestor-server\"\n");
    ws.write("child/ulak.local.toml", "host = \"child-server\"\n");

    let out = ws
        .ulak_alone()
        .current_dir(ws.project.join("child"))
        .env("COMPOSE_FILE", "../compose.yaml")
        .args(["config", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "config failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let host = report["settings"]
        .as_array()
        .and_then(|settings| settings.iter().find(|row| row["key"] == "host"))
        .and_then(|row| row["value"].as_str());
    assert_eq!(
        host,
        Some("ancestor-server"),
        "config ignored the current Compose environment: {report}"
    );
    assert_eq!(
        report["project"], "proj",
        "config ignored the environment-selected Compose project: {report}"
    );
}

/// `init` runs before any stack declaration exists, so its next step cannot
/// rely on declaration recovery. A nonstandard Compose invocation must be
/// carried into the doctor command it prints.
#[test]
fn explicit_init_prints_an_exact_doctor_command_before_any_declaration() {
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
    ws.write(COMPOSE_FILE, "services: {}\n");

    let out = ws
        .ulak_alone()
        .args(["-f", COMPOSE_FILE, "init"])
        .output()
        .unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "explicit init failed:\n{text}");
    let next = text
        .lines()
        .find(|line| line.contains("next:"))
        .unwrap_or_default();
    assert!(
        next.contains(COMPOSE_FILE) && next.contains("--project-directory"),
        "init printed a doctor command that cannot rediscover the project:\n{text}"
    );
}

/// An explicit first doctor likewise has no Desired record to recover on
/// the next Docker command. Its ready line must remain on the exact model it
/// just checked instead of sending `up` through Docker's default discovery.
#[test]
fn explicit_doctor_prints_an_exact_up_command_without_a_declaration() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let project = DeclaredProject::flat(server, "doctor-next");
    project.assert_declared();
    std::fs::remove_dir_all(project.ws.stacks_dir()).unwrap();

    let out = project.explicit_management("doctor");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "explicit doctor failed:\n{text}");
    let next = text
        .lines()
        .find(|line| line.contains("next:"))
        .unwrap_or_default();
    assert!(
        next.contains(COMPOSE_FILE) && next.contains("--project-directory"),
        "doctor printed an up command that cannot rediscover the checked model:\n{text}"
    );
}

/// A first explicit sync can fail before any Desired declaration exists.
/// Its dry-run and deletion-budget remedies must retain the exact custom
/// invocation; a bare retry cannot rediscover this Compose file.
#[test]
fn explicit_sync_refusal_prints_exact_retry_commands_without_a_declaration() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let project = DeclaredProject::flat(server, "sync-remedy");
    project.assert_declared();
    std::fs::remove_dir_all(project.ws.stacks_dir()).unwrap();
    std::fs::remove_file(project.ws.project.join(&project.payload)).unwrap();

    let out = project
        .ws
        .ulak(&project.server)
        .current_dir(&project.cwd)
        .args([
            "-f",
            &project.compose_file,
            "-p",
            &project.identity,
            "sync",
            "--max-delete",
            "0",
        ])
        .output()
        .unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "the zero deletion budget unexpectedly allowed the sync:\n{text}"
    );
    assert!(
        text.contains("review the list:") && text.contains("delete them:"),
        "the refusal did not offer both safe ways forward:\n{text}"
    );
    assert!(
        text.matches(COMPOSE_FILE).count() >= 2 && text.matches("--project-directory").count() >= 2,
        "the retry commands cannot rediscover the explicit project:\n{text}"
    );
}
