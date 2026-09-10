//! The harness's own contract: where the e2e suite's base images come
//! from, and what happens when they are nowhere to be found.
//!
//! This file exists because the suite's colour used to depend on an
//! anonymous internet quota. Docker Hub counts unauthenticated pulls per
//! IP; four runs of the suite in one day spent that quota, and seven of
//! sixteen test binaries went red carrying `429 toomanyrequests` and
//! nothing whatsoever about ulak. A red that means nothing is worse than
//! a missing test, because it teaches the next reader to re-run instead
//! of read — and the next red that DOES mean something gets the same
//! shrug.
//!
//! So every image the scenarios need is handed over from this machine's
//! own daemon. The two tests below are what keeps that true: one proves
//! the bytes really do come from here and not from a registry, the other
//! proves that an image nobody can supply is a named, actionable failure
//! rather than a quiet skip.

mod common;

use std::process::{Command, Stdio};

use common::TestServer;

/// An image every developer machine that can run this suite already has,
/// because half the other suites run their containers from it.
const SEEDABLE: &str = "alpine:3.20";

/// A name no registry can ever answer for, so this test can never
/// accidentally succeed by reaching one.
const NOWHERE: &str = "ulak-e2e-no-such-image:nowhere";

#[test]
fn an_image_reaches_the_server_from_this_machine_and_not_from_a_registry() {
    let Some(server) = TestServer::shared() else {
        return;
    };

    // Provenance, proven rather than asserted: an image built HERE, a
    // moment ago, under a tag no registry has ever heard of. If it turns
    // up on the server's daemon there is only one road it can have
    // taken. Blocking the registry would prove the same thing, but only
    // on a run somebody remembered to block it on — this holds on every
    // run, which is the difference between a guarantee and a habit.
    let local_only = format!("ulak-e2e-local-only-{}:test", std::process::id());
    build_here(&local_only);
    let _cleanup = Cleanup {
        server: &server,
        image: local_only.clone(),
    };

    server.needs_image(&local_only);
    assert!(
        server
            .ssh(&format!("docker image inspect {local_only} >/dev/null"))
            .status
            .success(),
        "{local_only} exists in no registry, so it can only have come from this \
         machine — and it did not arrive at all"
    );

    // And a real, layered image survives the same trip and can still run
    // a container, which a broken save/load would fail at and nowhere
    // earlier.
    server.ssh(&format!("docker image rm -f {SEEDABLE} >/dev/null 2>&1"));
    server.needs_image(SEEDABLE);
    let ran = server.ssh(&format!("docker run --rm {SEEDABLE} true"));
    assert!(
        ran.status.success(),
        "the seeded image cannot run a container:\n{}",
        String::from_utf8_lossy(&ran.stderr)
    );
}

/// The dockerized fixture must exercise the NON-ROOT path, and that is
/// a fact about the suite rather than about the image.
///
/// It matters because the other backend is somebody's real host and, in
/// the case this was written against, root — so if the fixture were root
/// too, nothing anywhere would ever run ulak against a server where a
/// permission can be refused. Everything else that scenario used to
/// assert (sshd, rsync's version, docker, compose v2, a writable home)
/// is a property of `e2e/sshd/Dockerfile` that `wait_until_ready` and
/// `ulak doctor` already enforce, loudly, in every other binary.
#[test]
fn the_fixture_exercises_a_server_ulak_is_not_root_on() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    if server.is_real_host() {
        // The other backend is whoever the user set it up as, which is
        // not this suite's to insist on — but it has to be a DECISION
        // rather than an accident, so the switch that chose it is
        // checked instead of assumed.
        assert!(
            std::env::var("ULAK_TEST_E2E")
                .unwrap_or_default()
                .starts_with("host:"),
            "the backend reports itself a real host without ULAK_TEST_E2E asking for one"
        );
        return;
    }
    let id = server.ssh("id -un");
    assert_eq!(
        String::from_utf8_lossy(&id.stdout).trim(),
        "dev",
        "the fixture is being driven as root, so no scenario in the suite is exercising \
         a server where a permission can be refused"
    );
}

#[test]
fn an_image_nobody_can_supply_is_named_together_with_the_way_to_get_it() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    assert!(
        !this_machine_has(NOWHERE),
        "{NOWHERE} was supposed to be a name nobody has"
    );

    // The failure mode being ruled out is the quiet one: a harness that
    // skips, or warns, when it cannot find an image leaves a green tick
    // over scenarios that never ran. So this must panic — and the panic
    // has to be readable by someone who has never opened this file,
    // which means naming the image and the one command that fixes it.
    let complaint = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        server.needs_image(NOWHERE);
    }))
    .expect_err("an image nobody has must fail the test, not warn and continue");
    let complaint = complaint
        .downcast_ref::<String>()
        .map(String::as_str)
        .unwrap_or("<panic payload was not a string>");

    assert!(
        complaint.contains(NOWHERE),
        "the missing image must be named:\n{complaint}"
    );
    assert!(
        complaint.contains(&format!("docker pull {NOWHERE}")),
        "the reader must be told exactly how to get it:\n{complaint}"
    );
}

/// A one-layer image on THIS machine's daemon. `FROM scratch` so the
/// build itself needs no base and therefore no registry either — the
/// test would otherwise prove its point by breaking its own premise.
fn build_here(tag: &str) {
    let ctx = tempfile::TempDir::new().expect("probe context dir");
    std::fs::write(ctx.path().join("marker"), b"SEED-PROBE\n").unwrap();
    std::fs::write(
        ctx.path().join("Dockerfile"),
        b"FROM scratch\nCOPY marker /marker\n",
    )
    .unwrap();
    let out = Command::new("docker")
        .args(["build", "-q", "-t", tag])
        .arg(ctx.path())
        .output()
        .expect("docker build spawn");
    assert!(
        out.status.success(),
        "could not build the local-only probe image:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The probe image is this test's litter on both daemons.
struct Cleanup<'a> {
    server: &'a TestServer,
    image: String,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        self.server.ssh(&format!(
            "docker image rm -f {} >/dev/null 2>&1 || true",
            self.image
        ));
        let _ = Command::new("docker")
            .args(["image", "rm", "-f", &self.image])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn this_machine_has(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
