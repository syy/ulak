//! The stand-in `docker`, end to end.
//!
//! A project's own entry points spell it `docker`, so the question these
//! scenarios answer is not "does the stand-in exist" but "does a script
//! that never heard of Ulak end up on the SERVER'S daemon". Only one
//! assertion can tell those two machines apart, and it is not an exit
//! code: the container has to be found over there and not found here.
//!
//! The other half is the promise the command makes about somebody's
//! machine. `install` may edit a shell profile, and a command that edits
//! one from a script — a CI job, a provisioning run, anything with no
//! terminal — would be a change its owner never agreed to. That is
//! pinned here rather than left to the confirmation prompt's reputation.
//!
//! Every scenario names its server-side resources after itself: these
//! share one server with every other suite.

mod common;

use common::{TestServer, Workspace};

fn scenario_name(what: &str) -> String {
    format!("ulak-e2e-shim-{what}-{}", std::process::id())
}

/// A container the server must lose again even when an assertion panics.
struct RemoteContainer<'a> {
    server: &'a TestServer,
    name: String,
}

impl Drop for RemoteContainer<'_> {
    fn drop(&mut self) {
        let _ = self
            .server
            .ssh(&format!("docker rm -f {} >/dev/null 2>&1", self.name));
    }
}

/// Is this name a container on THIS machine's daemon?
fn here(name: &str) -> bool {
    std::process::Command::new("docker")
        .args(["inspect", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The regression: a project's start script says `docker`, and before
/// the stand-in existed that word reached whichever daemon was local —
/// silently, and with the wrong machine's filesystem underneath it. The
/// script here is deliberately ignorant of Ulak: it is the shape every
/// real entry point has.
#[test]
fn a_scripts_own_docker_call_reaches_the_server_and_not_this_machine() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    server.needs_image("alpine:3.20");
    let ws = Workspace::new();
    ws.set_host(&server.alias);

    let name = scenario_name("reaches");
    let guard = RemoteContainer {
        server: &server,
        name: name.clone(),
    };

    // No `ulak` anywhere in it. `network_mode` is spelled `--network
    // none` for the reason the other suites give: a shared server has a
    // finite number of subnets and nothing here is about networking.
    ws.write(
        "start.sh",
        &format!(
            "#!/bin/sh\nset -e\ndocker run -d --name {name} --network none alpine:3.20 sleep 600\n"
        ),
    );
    let script = ws.project.join("start.sh");
    let mut perms = std::fs::metadata(&script)
        .expect("the script exists")
        .permissions();
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
    }
    std::fs::set_permissions(&script, perms).expect("the script is runnable");

    let out = ws
        .ulak(&server)
        .args(["shim", "run", "--", "./start.sh"])
        .output()
        .expect("ulak ran");
    assert!(
        out.status.success(),
        "the script failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        server
            .ssh(&format!("docker inspect {name} >/dev/null"))
            .status
            .success(),
        "the container is not on the server, so the script's `docker` never left this machine:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !here(&name),
        "the container was created on the LOCAL daemon — the stand-in did not stand in"
    );

    drop(guard);
    ws.forget_on_server(&server);
}

/// `ui::confirm` answers no without a terminal, and this is the promise
/// that answer keeps: a provisioning run or a CI job that types
/// `ulak shim install` gets the stand-ins and an untouched profile. The
/// stand-ins are still written, because writing them changes nothing
/// about how anything resolves until the PATH line exists.
#[test]
fn install_writes_the_stand_ins_but_never_edits_a_profile_without_a_terminal() {
    let ws = Workspace::new();
    let rc = ws.home.join(".zshrc");
    let before = "export EDITOR=vim\nalias ll='ls -la'\n";
    std::fs::write(&rc, before).expect("a profile to protect");

    ws.ulak_alone()
        .args(["shim", "install"])
        .env("SHELL", "/bin/zsh")
        .assert()
        .success();

    let shim = ws.home.join(".local/state/ulak/shim");
    for name in ["docker", "docker-compose"] {
        assert!(
            shim.join(name).is_file(),
            "{name} was not written to {}",
            shim.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(&rc).expect("the profile is still readable"),
        before,
        "a profile was edited by a command nobody could answer"
    );
}
