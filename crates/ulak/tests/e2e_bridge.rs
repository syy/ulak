//! The local file bridge against a real server: every Docker command
//! whose argument names a file on THIS machine.
//!
//! What these scenarios are about is which machine a file ended up on.
//! `docker save -o api.tar` forwarded verbatim exits 0, prints nothing
//! alarming, and writes `api.tar` on the SERVER — in a directory the
//! next sync may delete, while the user goes looking for it here. So
//! "it ran and exited 0" is never the assertion: the archive is compared
//! byte for byte against one the server computed, and the server is
//! asked whether it is holding a file it should never have seen.
//!
//! The two `tar` binaries at the ends of `docker cp` are not the same
//! program — bsdtar packs here, busybox tar unpacks on the fixture, and
//! the reverse coming home. Names and permission bits are asserted on
//! both legs, because that asymmetry is where they go missing.
//!
//! Twenty scenarios, twenty verdicts. They used to be one `#[test]`
//! calling them in sequence, which reported "1 passed" for all twenty
//! and — worse — aborted every scenario after the first failure, so a
//! regression sweep saw one red line and no way to tell whether one
//! thing or nineteen were broken. They share one fixture
//! (`TestServer::shared`) and run in parallel now, which is why every
//! image, container, secret and config below carries the name of the
//! scenario that made it, and why the one assertion about a SHARED
//! resource — the server's staging directories — is polled instead of
//! sampled: another scenario's copy in flight is not a leak.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{TestServer, Workspace};

/// The image the `docker cp` scenarios run their container from.
const BASE: &str = "alpine:3.20";

// ─── what each scenario builds for itself ───────────────────────────

/// One scenario's own image, container and Swarm objects on the shared
/// daemon — every one of them named for the scenario, and removed
/// however that scenario ends.
///
/// A panicking test must not leave the next run's `docker save` looking
/// at yesterday's image and calling it a pass, and the backend may be
/// somebody's real host.
struct Probe<'a> {
    server: &'a TestServer,
    /// `<pid>-<scenario>`: unique against another checkout on the same
    /// server, and against another scenario in this process.
    scope: String,
    images: Vec<String>,
    containers: Vec<String>,
    secrets: Vec<String>,
    configs: Vec<String>,
}

impl Probe<'_> {
    fn new<'a>(server: &'a TestServer, what: &str) -> Probe<'a> {
        Probe {
            server,
            scope: format!("{}-{what}", std::process::id()),
            images: Vec::new(),
            containers: Vec::new(),
            secrets: Vec::new(),
            configs: Vec::new(),
        }
    }

    fn name(&self, what: &str) -> String {
        format!("ulak-bridge-{what}-{}", self.scope)
    }

    /// A `FROM scratch` image built ON THE SERVER, so `docker save` has
    /// something to hand over that this machine never had a copy of.
    ///
    /// Scratch keeps the archive at 12 kB — measured — which is what
    /// makes comparing every byte of it cheap enough to do on every leg
    /// rather than once. What is NOT stable across two saves of it is
    /// the archive as a whole; see `members_of` for which two bytes move
    /// and why the comparison is per member, and `runnable` for why it is
    /// also per RUNNABLE member — the daemon composes the rest of the
    /// archive at export time and two saves can disagree about it.
    fn image(&mut self) -> String {
        let image = format!("{}:test", self.name("img"));
        self.images.push(image.clone());
        let out = self.server.ssh(&format!(
            "set -e; d=$(mktemp -d); \
             printf 'BRIDGE-MARKER\\n' > \"$d/marker\"; \
             printf 'FROM scratch\\nCOPY marker /marker\\n' > \"$d/Dockerfile\"; \
             docker build -q -t {image} \"$d\" >/dev/null; \
             rm -rf \"$d\"",
            image = q(&image),
        ));
        assert!(
            out.status.success(),
            "could not build the probe image on the server:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        image
    }

    /// A tag this scenario will create later and must remove either way.
    fn tag(&mut self, what: &str) -> String {
        let image = format!("{}:test", self.name(what));
        self.images.push(image.clone());
        image
    }

    /// A container with the two things every `docker cp` scenario needs:
    /// a directory that already exists — the whole placement rule turns
    /// on whether the destination is one — and a small tree to copy
    /// home. Seeded here rather than by whichever scenario happened to
    /// run first, which is what made the down-leg scenarios ordered.
    fn container(&mut self) -> String {
        let name = self.name("ctr");
        self.containers.push(name.clone());
        // Once for the whole binary, not once per scenario: a dozen
        // simultaneous anonymous pulls of one tag is a Docker Hub rate
        // limit, and a suite that goes red for that teaches people to
        // re-run instead of read.
        self.server.needs_image(BASE);
        let out = self.server.ssh(&format!(
            "set -e; \
             docker run -d --name {c} {BASE} sleep 900 >/dev/null; \
             docker exec {c} mkdir -p /existing/dir /from/tree; \
             docker exec {c} sh -c 'printf DOWN-BODY > /from/plain.txt && \
             chmod 741 /from/plain.txt && printf INNER-DOWN > /from/tree/inner.txt'",
            c = q(&name),
        ));
        assert!(
            out.status.success(),
            "could not start the probe container on the server:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        name
    }

    fn secret(&mut self) -> String {
        let name = self.name("secret");
        self.secrets.push(name.clone());
        name
    }

    fn config(&mut self) -> String {
        let name = self.name("config");
        self.configs.push(name.clone());
        name
    }
}

impl Drop for Probe<'_> {
    fn drop(&mut self) {
        let mut removals: Vec<String> = Vec::new();
        removals.extend(self.containers.iter().map(|c| format!("docker rm -f {c}")));
        removals.extend(
            self.images
                .iter()
                .map(|i| format!("docker image rm -f {i}")),
        );
        removals.extend(self.secrets.iter().map(|s| format!("docker secret rm {s}")));
        removals.extend(self.configs.iter().map(|c| format!("docker config rm {c}")));
        let script = removals
            .iter()
            .map(|r| format!("{r} >/dev/null 2>&1 || true"))
            .collect::<Vec<_>>()
            .join("; ");
        self.server.ssh(&script);
    }
}

/// A workspace pointed at the shared server. Each scenario gets its own,
/// so one scenario's `api.tar` can never be another's evidence.
fn wired(server: &TestServer) -> Workspace {
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws
}

// ─── save / load ────────────────────────────────────────────────────

/// The headline: the bytes that leave the server are the bytes that land
/// here.
///
/// Every byte of every member is compared, not "the file is not empty",
/// because the failure this guards against — a pty anywhere in the path,
/// translating newlines — produces an archive that looks entirely normal
/// until the day something tries to read it.
#[test]
fn a_saved_archive_arrives_byte_for_byte() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "save-bytes");
    let image = probe.image();

    let want = members_on_the_server(&server, &format!("docker save {}", q(&image)));

    let out = ws
        .ulak(&server)
        .args(["docker", "save", "-o", "round-trip.tar", &image])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ulak docker save -o round-trip.tar {image}` failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let local = ws.project.join("round-trip.tar");
    let manifest = manifest_body_of(&local);
    assert_eq!(
        runnable(&members_of(&local), &manifest),
        runnable(&want, &manifest),
        "the archive changed in transit — {} bytes arrived and their contents are not the \
         server's",
        size_of_file(&local)
    );
    no_partial_archive_outlived_it(&ws);

    ws.forget_on_server(&server);
}

/// The whole point of the module, and nothing else proves it: `-o
/// api.tar` names a file on this machine, so this machine is where it
/// has to be — and the server's login directory, where a forwarded
/// `docker save` would have put it, has to stay empty.
#[test]
fn the_saved_archive_is_here_and_not_on_the_server() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "save-here");
    let image = probe.image();
    // The name is this scenario's alone: the check below asks the whole
    // server whether it is holding one, and a shared name would find
    // another scenario's file and call it a leak.
    let archive = format!("{}.tar", probe.name("here"));

    ws.ulak(&server)
        .args(["docker", "save", "-o", &archive, &image])
        .assert()
        .success();

    let here = ws.project.join(&archive);
    assert!(
        here.is_file(),
        "{archive} is not in the directory the command ran from ({}); that directory holds {:?}",
        ws.project.display(),
        siblings(&here)
    );

    // ssh lands in the home directory, so a forwarded `-o …` puts it
    // there; the workspace tree is the other place it could hide.
    let stray = server.ssh(&format!(
        "ls -la ~/{archive} 2>/dev/null; find ~/.ulak -name {} -print 2>/dev/null",
        q(&archive)
    ));
    let found = String::from_utf8_lossy(&stray.stdout);
    assert!(
        found.trim().is_empty(),
        "the server is holding a {archive} it should never have seen:\n{found}"
    );

    ws.forget_on_server(&server);
}

/// The other half of the round trip: the archive on this machine is a
/// real one, and the proof is a server that has lost the image getting
/// it back from nothing but our local file.
#[test]
fn a_removed_image_comes_back_from_the_local_archive() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "load");
    let image = probe.image();

    ws.ulak(&server)
        .args(["docker", "save", "-o", "round-trip.tar", &image])
        .assert()
        .success();

    server.ssh(&format!("docker image rm -f {}", q(&image)));
    assert!(
        !has_image(&server, &image),
        "the image must be gone before the load, or this scenario proves nothing"
    );

    let out = ws
        .ulak(&server)
        .args(["docker", "load", "-i", "round-trip.tar"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ulak docker load -i round-trip.tar` failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        has_image(&server, &image),
        "the server still does not have {image} after loading the archive that was saved from it"
    );

    ws.forget_on_server(&server);
}

/// The archive you already had is not collateral for the one you failed
/// to make — and a truncated tar that looks whole is not left where the
/// next `docker load` will find it on a different day.
///
/// `File::create` truncates before ssh is even spawned, so writing
/// straight onto the target meant `docker save -o backup.tar
/// no-such-image` left the user with neither the new archive nor the
/// backup — the one moment they were most likely to need it.
///
/// Both destinations in one scenario, because they are one claim seen
/// twice: a failed save writes nothing anywhere. The one that had a file
/// there before must still have that file; the one that had none must
/// still have none; and the successful save at the end is what stops the
/// whole thing passing by never writing anything at all.
#[test]
fn a_failed_save_leaves_the_archive_you_already_had_and_makes_no_new_one() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "keep");
    let image = probe.image();

    let keep = ws.project.join("keep.tar");
    std::fs::write(&keep, "PRECIOUS-BACKUP").expect("write keep.tar");

    for target in ["keep.tar", "half.tar"] {
        let out = ws
            .ulak(&server)
            .args(["docker", "save", "-o", target, "ulak-no-such-image:nope"])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "saving an image that does not exist must fail; `-o {target}` exited {:?}",
            out.status.code()
        );
    }
    assert_local_file(&keep, "PRECIOUS-BACKUP", None);
    let half = ws.project.join("half.tar");
    assert!(
        !half.exists(),
        "half.tar survived a failed save, {} bytes of it — the next `docker load` is where \
         that would have been discovered",
        size_of_file(&half)
    );
    no_partial_archive_outlived_it(&ws);

    // And the successful case puts the real archive there, so the
    // scenario cannot pass by never writing anything at all.
    ws.ulak(&server)
        .args(["docker", "save", "-o", "keep.tar", &image])
        .assert()
        .success();
    assert!(
        size_of_file(&keep) > 1024,
        "the successful save left {} bytes at keep.tar",
        size_of_file(&keep)
    );

    ws.forget_on_server(&server);
}

/// A bridged command is still Docker's command, so the number the shell
/// gets has to be Docker's number.
#[test]
fn a_failing_bridge_command_returns_dockers_own_code() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "exitcode");
    let image = probe.image();

    // Measured on this server rather than remembered: Docker answers an
    // unknown flag with 125 and a missing image with 1, and only the
    // first can tell "Docker's code" apart from "some code".
    let asked = server.ssh(&format!(
        "docker save --ulak-no-such-flag {} >/dev/null 2>&1; echo $?",
        q(&image)
    ));
    let dockers_own: i32 = first_word(&asked)
        .parse()
        .expect("the server should print an exit code");
    assert_ne!(
        dockers_own, 1,
        "this scenario needs a code other than 1 to mean anything, and Docker now answers an \
         unknown flag with 1 — pick a different failure"
    );

    let out = ws
        .ulak(&server)
        .args([
            "docker",
            "save",
            "-o",
            "never.tar",
            "--ulak-no-such-flag",
            &image,
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(dockers_own),
        "Docker exited {dockers_own} on the server and ulak reported {:?}:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !ws.project.join("never.tar").exists(),
        "never.tar was left behind by a command that never produced one"
    );

    ws.forget_on_server(&server);
}

/// No `-o` and no `-i` means Docker's own stdio form, which the bridge
/// must leave completely alone — including the part where our stdout is
/// the user's and may be a file, a pipe, or a terminal.
#[test]
fn the_stream_forms_still_use_our_own_stdio() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "stream");
    let image = probe.image();

    let want = members_on_the_server(&server, &format!("docker save {}", q(&image)));

    let streamed = ws.project.join("streamed.tar");
    let sink = std::fs::File::create(&streamed).expect("create streamed.tar");
    let status = ws
        .ulak_raw(&server)
        .args(["docker", "save", &image])
        .stdout(Stdio::from(sink))
        .status()
        .expect("spawn ulak docker save");
    assert!(status.success(), "`ulak docker save {image} > file` failed");
    let manifest = manifest_body_of(&streamed);
    assert_eq!(
        runnable(&members_of(&streamed), &manifest),
        runnable(&want, &manifest),
        "the streamed archive changed in transit — {} bytes reached our stdout and their \
         contents are not the server's",
        size_of_file(&streamed)
    );

    server.ssh(&format!("docker image rm -f {}", q(&image)));
    assert!(
        !has_image(&server, &image),
        "the image must be gone before the load, or this scenario proves nothing"
    );

    let source = std::fs::File::open(&streamed).expect("open streamed.tar");
    let status = ws
        .ulak_raw(&server)
        .args(["docker", "load"])
        .stdin(Stdio::from(source))
        .status()
        .expect("spawn ulak docker load");
    assert!(status.success(), "`ulak docker load < file` failed");
    assert!(
        has_image(&server, &image),
        "the server still does not have {image} after `ulak docker load < file`"
    );

    ws.forget_on_server(&server);
}

// ─── docker cp ──────────────────────────────────────────────────────

/// Local → container, in the three placements Docker distinguishes and
/// only the far side can decide between.
#[test]
fn copying_up_lands_where_docker_would_have_put_it() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "cp-up");
    let container = probe.container();

    ws.write("cp-up/plain.txt", "PLAIN-UP\n");
    // 0741 on purpose: every bit of it survives a 022 umask, so a mode
    // that arrives wrong means something dropped it rather than masked
    // it.
    chmod(&ws.project.join("cp-up/plain.txt"), 0o741);

    // A destination that is a directory keeps the source's own name.
    ws.ulak(&server)
        .args(["docker", "cp", "cp-up/plain.txt"])
        .arg(format!("{container}:/existing/dir/"))
        .assert()
        .success();
    assert_in_container(
        &server,
        &container,
        "/existing/dir/plain.txt",
        "PLAIN-UP\n",
        Some("741"),
    );

    // A destination that names a file renames it.
    ws.ulak(&server)
        .args(["docker", "cp", "cp-up/plain.txt"])
        .arg(format!("{container}:/existing/dir/renamed.txt"))
        .assert()
        .success();
    assert_in_container(
        &server,
        &container,
        "/existing/dir/renamed.txt",
        "PLAIN-UP\n",
        None,
    );

    // A directory goes INTO an existing directory, not over it.
    ws.write("cp-up/tree/inner.txt", "INNER-UP\n");
    ws.ulak(&server)
        .args(["docker", "cp", "cp-up/tree"])
        .arg(format!("{container}:/existing/dir"))
        .assert()
        .success();
    assert_in_container(
        &server,
        &container,
        "/existing/dir/tree/inner.txt",
        "INNER-UP\n",
        None,
    );

    ws.forget_on_server(&server);
}

/// Container → local, where the same placement rule has to be applied
/// against THIS filesystem, because only this machine knows whether the
/// destination is a directory.
#[test]
fn copying_down_lands_where_docker_would_have_put_it() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "cp-down");
    let container = probe.container();

    let landing = ws.project.join("landing");
    std::fs::create_dir_all(&landing).expect("mkdir landing");
    // The destination's OWN mode is the thing nothing asserted. The
    // remote packs `tar -C "$S" -cf - .`, so the archive's first entry is
    // the staging directory `mktemp -d` made 0700 — and `tar -xp` lands
    // that mode on the destination. Real `docker cp` never touches it, so
    // an unrestored copy silently takes a shared directory private.
    chmod(&landing, 0o755);

    // An existing local directory keeps the source's own name.
    ws.ulak(&server)
        .args(["docker", "cp"])
        .arg(format!("{container}:/from/plain.txt"))
        .arg("landing")
        .assert()
        .success();
    assert_local_file(
        &ws.project.join("landing/plain.txt"),
        "DOWN-BODY",
        Some(0o741),
    );
    assert_eq!(mode_of(&landing), 0o755, "after a file copy INTO it");

    // A local path that does not exist yet renames it.
    ws.ulak(&server)
        .args(["docker", "cp"])
        .arg(format!("{container}:/from/plain.txt"))
        .arg("landing/renamed.txt")
        .assert()
        .success();
    assert_local_file(&ws.project.join("landing/renamed.txt"), "DOWN-BODY", None);
    assert_eq!(mode_of(&landing), 0o755, "after a renaming file copy");

    // A container directory goes INTO the existing local one.
    ws.ulak(&server)
        .args(["docker", "cp"])
        .arg(format!("{container}:/from/tree"))
        .arg("landing")
        .assert()
        .success();
    assert_local_file(
        &ws.project.join("landing/tree/inner.txt"),
        "INNER-DOWN",
        None,
    );
    assert_eq!(mode_of(&landing), 0o755, "after a directory copy INTO it");
    // …and the directory the container sent keeps the mode the container
    // gave it, which is the half `-p` is there for. Restoring the
    // destination must not undo that.
    let sent = mode_in_container(&server, &container, "/from/tree");
    assert_eq!(
        mode_of(&ws.project.join("landing/tree")),
        sent,
        "the copied directory arrived with a different mode from the one it left with"
    );

    ws.forget_on_server(&server);
}

/// Both directions run through shell-quoted remote scripts, so a name
/// with a space or a quote in it is where a quoting bug shows up — as a
/// file split in two, or as a command that never ran.
#[test]
fn names_with_a_space_or_a_quote_survive_the_shell() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "odd-names");
    let container = probe.container();
    std::fs::create_dir_all(ws.project.join("landing")).expect("mkdir landing");

    // `data.` is here for a different reason from the other two, and it
    // used to be a scenario of its own: a dot is a character a filename
    // is allowed to END in, and stripping trailing dots to find the `/.`
    // contents spelling turned a file honestly named `data.` into a
    // missing path. That decision is `split_contents_only`, pinned
    // exactly at the unit level; what is worth an e2e is that the name
    // survives the round trip, which is what this loop already does for
    // the other two. Spelled with a leading `./` for the same reason —
    // that is the shape the parse has to get right.
    for name in ["with space.txt", "it's.txt", "data."] {
        ws.write(&format!("odd/{name}"), "ODD-NAME\n");
        ws.ulak(&server)
            .args(["docker", "cp"])
            .arg(format!("./odd/{name}"))
            .arg(format!("{container}:/existing/dir/"))
            .assert()
            .success();
        assert_in_container(
            &server,
            &container,
            &format!("/existing/dir/{name}"),
            "ODD-NAME\n",
            None,
        );
    }

    for name in ["with space.txt", "it's.txt", "data."] {
        ws.ulak(&server)
            .args(["docker", "cp"])
            .arg(format!("{container}:/existing/dir/{name}"))
            .arg("landing")
            .assert()
            .success();
        assert_local_file(
            &ws.project.join(format!("landing/{name}")),
            "ODD-NAME\n",
            None,
        );
    }

    ws.forget_on_server(&server);
}

/// `-` at either end is already a stream, and Docker's own form for it
/// is the right one — our stdio is the user's, so nothing needs staging.
#[test]
fn a_copy_can_be_a_stream_at_either_end() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "cp-stream");
    let container = probe.container();

    let from_stdout = ws.project.join("from-stdout.tar");
    let sink = std::fs::File::create(&from_stdout).expect("create from-stdout.tar");
    let status = ws
        .ulak_raw(&server)
        .args(["docker", "cp"])
        .arg(format!("{container}:/from/plain.txt"))
        .arg("-")
        .stdout(Stdio::from(sink))
        .status()
        .expect("spawn ulak docker cp to stdout");
    assert!(
        status.success(),
        "`ulak docker cp ctr:/from/plain.txt -` failed"
    );
    let members = tar_members(&from_stdout);
    assert!(
        members.iter().any(|m| m.ends_with("plain.txt")),
        "`cp … -` should have written a tar of plain.txt to stdout; it wrote {members:?}"
    );

    ws.write("stdin-src/from-stdin.txt", "VIA-STDIN\n");
    let packed = ws.project.join("to-stdin.tar");
    pack(&ws.project.join("stdin-src"), "from-stdin.txt", &packed);
    let source = std::fs::File::open(&packed).expect("open to-stdin.tar");
    let status = ws
        .ulak_raw(&server)
        .args(["docker", "cp", "-"])
        .arg(format!("{container}:/existing/dir"))
        .stdin(Stdio::from(source))
        .status()
        .expect("spawn ulak docker cp from stdin");
    assert!(
        status.success(),
        "`ulak docker cp - ctr:/existing/dir` failed"
    );
    assert_in_container(
        &server,
        &container,
        "/existing/dir/from-stdin.txt",
        "VIA-STDIN\n",
        None,
    );

    ws.forget_on_server(&server);
}

// Bundled shorthands moved into `archive_mode_carries_the_ownership_…`,
// which already builds the container and drives the `-a` road. The other
// half of what stood here — `cp -az` naming `-z` as the bad letter — is
// refused inside `CpArgs::parse`, before `Remote::open()` is reached, so
// the ssh round trip and the probe container bought nothing that
// bridge.rs's own test of the identical sentence does not already own.

/// A directory the local tar cannot read whole must not be reported as
/// a copy that did not happen.
///
/// `tar` exits non-zero on an unreadable member and still writes a
/// complete archive of everything else — measured, bsdtar 3.5.3 exits 1
/// on `Cannot open: Permission denied` — so the container ends up
/// holding PART of the tree. Saying only "could not read <src> to send
/// it" reads as "nothing happened", which is the one thing it does not
/// mean, and the user goes on to trust a container that has half of what
/// they think it has.
#[test]
fn a_send_that_could_not_read_everything_says_what_landed() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "half-send");
    let container = probe.container();

    ws.write("partial/readable.txt", "READABLE\n");
    ws.write("partial/locked.txt", "LOCKED\n");
    chmod(&ws.project.join("partial/locked.txt"), 0o000);

    let out = ws
        .ulak(&server)
        .args(["docker", "cp", "partial"])
        .arg(format!("{container}:/existing/dir"))
        .output()
        .unwrap();
    // Back to readable before anything can panic, or the workspace's own
    // temp-directory cleanup silently fails at the end of the run.
    chmod(&ws.project.join("partial/locked.txt"), 0o644);
    let said = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        !out.status.success(),
        "a copy that could not read part of its source must fail; it exited {:?}\n{said}",
        out.status.code()
    );
    assert!(
        said.contains("could not read all of partial"),
        "the source that could not be read whole has to be named:\n{said}"
    );
    assert!(
        said.contains(&format!("{container}:/existing/dir")) && said.contains("has changed"),
        "the container destination has already been written to, and the message is the \
         only place that can say so:\n{said}"
    );
    // And the destination really did change, which is what makes that
    // sentence true rather than merely cautious. The claim is deliberately
    // "something is there", not "readable.txt is there": measured with
    // bsdtar 3.5.3, the archive that reached the container held the
    // `partial` directory entry and not the sibling file, and WHICH
    // members survive an interrupted pack is tar's business and varies.
    // The user-visible fact — the container is no longer as they left it —
    // is the same either way.
    assert!(
        server
            .ssh(&format!(
                "docker exec {c} test -e /existing/dir/partial",
                c = q(&container)
            ))
            .status
            .success(),
        "nothing reached the container, so the message's promise that some of it did \
         is the wrong warning; /existing/dir holds:\n{}",
        listing(&server, &container, "/existing/dir")
    );

    ws.forget_on_server(&server);
}

/// `--` ends flag parsing, which is the only way to name a source that
/// starts with a dash. Docker accepts it; refusing it made those paths
/// unreachable through ulak entirely.
#[test]
fn a_double_dash_makes_a_dash_leading_path_reachable() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "double-dash");
    let container = probe.container();

    ws.write("-dashed.txt", "DASH-LEADING\n");
    ws.ulak(&server)
        .args(["docker", "cp", "--", "-dashed.txt"])
        .arg(format!("{container}:/existing/dir/"))
        .assert()
        .success();
    assert_in_container(
        &server,
        &container,
        "/existing/dir/-dashed.txt",
        "DASH-LEADING\n",
        None,
    );

    ws.forget_on_server(&server);
}

/// A trailing `/.` means the CONTENTS of the directory and not the
/// directory. Both directions, because the up leg was fixed first and
/// the down leg answered the same command with "the copy arrived as 2
/// separate entries".
#[test]
fn a_trailing_slash_dot_copies_the_contents_both_ways() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "slash-dot");
    let container = probe.container();

    ws.write("dotted/inner.txt", "CONTENTS-UP\n");
    ws.ulak(&server)
        .args(["docker", "cp", "dotted/."])
        .arg(format!("{container}:/existing/dir"))
        .assert()
        .success();
    assert_in_container(
        &server,
        &container,
        "/existing/dir/inner.txt",
        "CONTENTS-UP\n",
        None,
    );
    let stray = server.ssh(&format!(
        "docker exec {c} test -e /existing/dir/dotted && echo present",
        c = q(&container),
    ));
    assert!(
        String::from_utf8_lossy(&stray.stdout).trim().is_empty(),
        "`cp dotted/. ctr:/existing/dir` copied the directory instead of what is in it"
    );

    // Coming home the destination may not exist yet, and Docker creates
    // it — `docker cp ctr:/tree/. /tmp/new` is measured to do exactly
    // that.
    ws.ulak(&server)
        .args(["docker", "cp"])
        .arg(format!("{container}:/from/tree/."))
        .arg("contents")
        .assert()
        .success();
    assert_local_file(&ws.project.join("contents/inner.txt"), "INNER-DOWN", None);
    assert!(
        !ws.project.join("contents/tree").exists(),
        "`cp ctr:/from/tree/. contents` copied the directory instead of what is in it"
    );

    // The other outcome of the same road, which is the one that leaves
    // litter: a `/.` copy has to CREATE the destination before any bytes
    // move — being a directory is what makes the tar merge into it — so
    // a copy that then fails has made a directory the user never had.
    // Measured on 29.4.0: real `docker cp ctr:/no/such/path/. ./fresh`
    // exits 1 and leaves `./fresh` uncreated.
    let out = ws
        .ulak(&server)
        .args(["docker", "cp"])
        .arg(format!("{container}:/no/such/path/."))
        .arg("fresh")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "copying a path the container does not have must fail; it exited {:?}\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !ws.project.join("fresh").exists(),
        "`fresh` was created for a copy that never happened, and nothing will ever \
         take it back: it holds {:?}",
        siblings(&ws.project.join("fresh/anything"))
    );

    ws.forget_on_server(&server);
}

// A trailing dot is part of a name, and that claim now rides in the
// odd-names loop above: everything after `split_contents_only` decided
// it was a path is byte-identical to the up leg those names already
// cover, and the decision itself is pinned in bridge.rs's own tests.

/// `docker cp` copies a dangling symlink happily: the link arrives
/// intact and points at nothing, which is what was asked for. Testing
/// the source with `exists()` follows the link and calls it missing.
#[test]
fn a_dangling_symlink_travels_as_the_broken_link_it_is() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "dangling");
    let container = probe.container();

    let link = ws.project.join("dangling");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink("/nowhere/at/all", &link).expect("make a dangling symlink");

    ws.ulak(&server)
        .args(["docker", "cp", "dangling"])
        .arg(format!("{container}:/existing/dir/"))
        .assert()
        .success();

    let target = server.ssh(&format!(
        "docker exec {c} readlink /existing/dir/dangling",
        c = q(&container),
    ));
    assert_eq!(
        String::from_utf8_lossy(&target.stdout).trim(),
        "/nowhere/at/all",
        "the link arrived pointing somewhere else, or arrived as a file; {} holds:\n{}",
        "/existing/dir",
        listing(&server, &container, "/existing/dir"),
    );

    ws.forget_on_server(&server);
}

/// `-a` promises to carry uid and gid, and the staging road cannot keep
/// that promise: the untar on the far side runs as the ssh user, so
/// ownership is gone before the real `docker cp -a` sees it. Measured, a
/// 501:0 file arrived 1000:1000.
///
/// Both shapes, because the fix narrowed the command as well as
/// correcting it: piping into `docker cp -a -` is what lets the daemon
/// read ownership out of the archive, and `-` is also what makes a
/// renaming destination impossible.
#[test]
fn archive_mode_carries_the_ownership_that_staging_drops() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "archive-mode");
    let container = probe.container();

    ws.write("archived/owned.txt", "ARCHIVE-MODE\n");
    let mine = owner_of(&ws.project.join("archived/owned.txt"));

    ws.ulak(&server)
        .args(["docker", "cp", "-a", "archived/owned.txt"])
        .arg(format!("{container}:/existing/dir"))
        .assert()
        .success();
    let landed = first_word(&server.ssh(&format!(
        "docker exec {c} stat -c %u:%g /existing/dir/owned.txt",
        c = q(&container),
    )));
    assert_eq!(
        landed, mine,
        "`cp -a` is supposed to carry uid and gid: the file is {mine} here and arrived {landed}"
    );

    // pflag BUNDLES shorthands, so `-aL` is `-a -L` and docker accepts
    // it — measured, it exits 0. Matching the whole word against a flag
    // list refused it, which is the difference between accepting what
    // docker accepts and inventing a stricter CLI. It rides here rather
    // than in a scenario of its own because `-a` is the road it has to
    // travel, and this is the test that already builds it: what it adds
    // is that `-L` alongside `-a` on a source that is NOT a symlink does
    // not trip the `-a and -L together` refusal.
    ws.write("archived/bundled.txt", "BUNDLED\n");
    ws.ulak(&server)
        .args(["docker", "cp", "-aL", "archived/bundled.txt"])
        .arg(format!("{container}:/existing/dir"))
        .assert()
        .success();
    assert_in_container(
        &server,
        &container,
        "/existing/dir/bundled.txt",
        "BUNDLED\n",
        None,
    );

    // The cost of that road, in Docker's own words rather than ours: a
    // tar on stdin can only be unpacked INTO a directory, so `-a` cannot
    // rename.
    let out = ws
        .ulak(&server)
        .args(["docker", "cp", "-a", "archived/owned.txt"])
        .arg(format!("{container}:/existing/dir/renamed-by-a.txt"))
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "`cp -a` to a destination that is not a directory must fail; it succeeded"
    );
    assert!(
        said.contains("must be a directory"),
        "the refusal should be Docker's own sentence, not ours; it said:\n{said}"
    );

    ws.forget_on_server(&server);
}

/// An extraction that fails halfway must not become a destination.
///
/// `Command::status()` returns `Ok` for a tar that exited 2 — it reports
/// whether the process could be SPAWNED, not whether it worked — so
/// before the staging step this deleted the user's destination, renamed
/// a partial tree over it, and handed back ssh's 0. This is the only
/// cover anywhere for that gate.
///
/// What stood here injected the failure with a non-UTF-8 filename and
/// returned early on any filesystem that accepts one — every
/// ext4/btrfs/xfs Linux box, which is where this is most likely to be
/// run. libtest captures stderr for a passing test, so on those the gate
/// read as covered while covering nothing at all.
///
/// The portable injection the code's own comment names —
/// `docker cp web:/dev ./dev`, where tar exits 2 on `Cannot mknod` —
/// turns out not to reach this gate through Ulak at all, and that was
/// measured rather than assumed: `down_script` has the SERVER materialise
/// the tree into a staging directory first, and as an unprivileged ssh
/// user that step is the one that fails, with docker's own "operation not
/// permitted" and before a single byte is unpacked here. It is only
/// reachable when the server's user may create device nodes and this
/// machine's may not.
///
/// So the failure is injected where the product actually reaches for it:
/// `unpack` runs `tar` off PATH, and this hands it one that writes part
/// of the tree and then exits 2 — which is exactly what a real tar does
/// when it meets an entry it cannot create, and is the only spelling of
/// that which is true on every filesystem and every user. The stream is
/// drained on purpose: a `tar` that died early would SIGPIPE the far
/// side, and the branch under test is the one where ssh SUCCEEDED and
/// only the unpack failed.
#[test]
fn an_unpack_that_fails_leaves_a_new_destination_uncreated() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "bad-unpack");
    let container = probe.container();
    let made = server.ssh(&format!(
        "docker exec {c} sh -c 'mkdir -p /unpackable && echo fine > /unpackable/fine.txt'",
        c = q(&container),
    ));
    assert!(
        made.status.success(),
        "could not make the tree to copy:\n{}",
        String::from_utf8_lossy(&made.stderr)
    );

    let mut cmd = ws.ulak(&server);
    cmd.env("PATH", path_with_a_tar_that_fails_halfway(&ws, &server));
    let out = cmd
        .args(["docker", "cp"])
        .arg(format!("{container}:/unpackable"))
        .arg("landing-new")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "a copy whose unpack failed must fail; it exited {:?}\n{said}",
        out.status.code()
    );
    assert!(
        said.contains("nothing at the destination was changed"),
        "the staged branch is the one that CAN promise this, and should say so; it \
         exited {:?} and said:\nstdout: {}\nstderr: {said}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        !ws.project.join("landing-new").exists(),
        "landing-new was created out of a copy that never finished; it holds {:?}",
        siblings(&ws.project.join("landing-new/anything"))
    );
    assert!(
        staging_dirs_in(&ws.project).is_empty(),
        "a local staging directory outlived the copy: {:?}",
        staging_dirs_in(&ws.project)
    );

    ws.forget_on_server(&server);
}

/// The same failure with a destination that already exists, where the
/// tar extracts straight into it — that is what gives Docker's merge
/// behaviour for free, and it is also why the reassuring sentence from
/// the staged branch would be a lie here.
#[test]
fn an_unpack_that_fails_into_an_existing_directory_says_so() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "bad-unpack-existing");
    let container = probe.container();

    let landing = ws.project.join("landing-existing");
    std::fs::create_dir_all(&landing).expect("mkdir landing-existing");
    // Unwritable, so tar cannot create the tree it was handed. Portable
    // in a way the non-UTF-8 name is not, and it exercises the branch
    // that has something to admit.
    chmod(&landing, 0o500);

    let out = ws
        .ulak(&server)
        .args(["docker", "cp"])
        .arg(format!("{container}:/from/tree"))
        .arg("landing-existing")
        .output()
        .unwrap();
    // Back to writable before anything can panic, or the workspace's own
    // temp-directory cleanup silently fails at the end of the run.
    chmod(&landing, 0o755);

    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "a copy whose unpack failed must fail; it exited {:?}",
        out.status.code()
    );
    assert!(
        said.contains("may already be in") && said.contains("landing-existing"),
        "extracting into an existing destination cannot promise it is untouched, and the \
         message has to name the directory:\n{said}"
    );

    ws.forget_on_server(&server);
}

/// Every copy stages a directory under the server's `~/.ulak` and traps
/// its own removal. A staging directory that outlives the command is a
/// copy of somebody's files sitting on a shared server.
///
/// This is the one assertion in the file about a resource the scenarios
/// SHARE — `mktemp -d "$HOME/.ulak/cp.XXXXXX"` is not scoped to a
/// workspace — so it is polled rather than sampled once. A directory
/// belonging to a copy still in flight goes away on its own; a leaked
/// one never does, and that is exactly the difference the wait measures.
#[test]
fn a_finished_copy_leaves_nothing_staged_on_the_server() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "staging");
    let container = probe.container();

    ws.write("staged/plain.txt", "STAGED\n");
    ws.ulak(&server)
        .args(["docker", "cp", "staged/plain.txt"])
        .arg(format!("{container}:/existing/dir/"))
        .assert()
        .success();
    ws.ulak(&server)
        .args(["docker", "cp"])
        .arg(format!("{container}:/from/plain.txt"))
        .arg("came-home.txt")
        .assert()
        .success();

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let left = server.ssh("ls -d ~/.ulak/cp.* 2>/dev/null");
        let found = String::from_utf8_lossy(&left.stdout).trim().to_string();
        if found.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "staging directories outlived their copies:\n{found}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    ws.forget_on_server(&server);
}

// ─── import / export ────────────────────────────────────────────────

/// `docker import` takes a file or a URL in the same positional, and
/// they belong to different machines: the tarball is ours to read, the
/// URL is the server's to fetch.
#[test]
fn a_tarball_is_read_here_and_a_url_is_fetched_there() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);
    let mut probe = Probe::new(&server, "import");
    let imported = probe.tag("imported");
    let exported = probe.name("exp");
    probe.containers.push(exported.clone());

    ws.write("rootfs/hello.txt", "IMPORTED-ROOTFS\n");
    let tarball = ws.project.join("rootfs.tar");
    pack(&ws.project.join("rootfs"), ".", &tarball);

    let out = ws
        .ulak(&server)
        .args(["docker", "import", "rootfs.tar", &imported])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ulak docker import rootfs.tar {imported}` failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        has_image(&server, &imported),
        "the server has no {imported} after importing a tarball from this machine"
    );

    // The bytes really are ours: a container made from the import,
    // exported straight back, still holds the file this machine wrote.
    let made = server.ssh(&format!(
        "docker create --name {e} {i} /nothing >/dev/null",
        e = q(&exported),
        i = q(&imported),
    ));
    assert!(
        made.status.success(),
        "could not create a container from the imported image:\n{}",
        String::from_utf8_lossy(&made.stderr)
    );
    ws.ulak(&server)
        .args(["docker", "export", "-o", "exported.tar", &exported])
        .assert()
        .success();
    let members = tar_members(&ws.project.join("exported.tar"));
    assert!(
        members.iter().any(|m| m.ends_with("hello.txt")),
        "the exported filesystem lost the file that was imported into it; it holds {members:?}"
    );

    // A URL never touches this filesystem. "cannot read ./https:…" is
    // exactly the sentence that means it did.
    let out = ws
        .ulak(&server)
        .args([
            "docker",
            "import",
            "https://ulak.invalid/rootfs.tar",
            "ulak-bridge-url:test",
        ])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "importing an unreachable URL should fail; it succeeded and said {said}"
    );
    assert!(
        !said.contains("cannot read"),
        "the URL was opened as a local path instead of being handed to the server: {said}"
    );

    ws.forget_on_server(&server);
}

// ─── help ───────────────────────────────────────────────────────────

/// A bridged command's `--help` names no file, so it is Docker's help
/// and not ours — and it exits 0, which the file-handling paths above
/// would not.
///
/// Two commands, not six. `help_wanted` decides this before the
/// `match kind`, so every bridged command takes the identical three
/// lines afterwards and four of the six were six ssh round trips'
/// worth of cosmetically different data. These two are the pair that
/// are not: `save` carries a file-taking carrier flag that help has to
/// step over, and `secret create` is the one whose handler refuses a
/// terminal stdin — a refusal help must reach past rather than trip on.
/// The flag table itself is per-command data, already pinned by
/// bridge.rs's `carrier` and `bool_shorthands` tests.
#[test]
fn help_comes_from_docker_and_still_succeeds() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = wired(&server);

    for (command, flag) in [
        (&["save"][..], "--output"),
        (&["secret", "create"][..], "--label"),
    ] {
        let out = ws
            .ulak(&server)
            .arg("docker")
            .args(command)
            .arg("--help")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            out.status.success(),
            "`ulak docker {} --help` exited {:?}:\n{}",
            command.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains("Usage:") && text.contains(flag),
            "`ulak docker {} --help` did not print Docker's own help — {flag} is missing from:\n{text}",
            command.join(" "),
        );
    }

    ws.forget_on_server(&server);
}

// ─── secret / config ────────────────────────────────────────────────

/// A secret is streamed rather than staged for a second reason beyond
/// the wrong-machine one: written into the workspace it would sit on a
/// shared server's disk until something removed it, and in the audit
/// trail it would sit here forever.
#[test]
fn a_secret_reaches_the_daemon_without_being_written_down() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    if !swarm_is_active(&server) {
        if server.is_real_host() {
            // A test does not get to make somebody's real server into a
            // manager on its own — that part was always right. What was
            // wrong is the shape of the refusal: an `eprintln!` and a
            // `return` ahead of every assertion reported ok, and libtest
            // discards captured stderr on a pass, so nobody ever saw the
            // announcement. This branch skips the WHOLE end-to-end
            // coverage of the Bridge `Confidential` route — no other
            // suite repeats it — so it has to be asked for in writing,
            // the same rule `common/mod.rs::start_docker` applies to a
            // missing daemon and for the same reason.
            match std::env::var("ULAK_TEST_E2E_SWARM").as_deref() {
                // "this host is mine and may become a manager": falls
                // through to the same `swarm init` the fixture takes.
                Ok("init") => {}
                // "I know this leaves the route untested here."
                Ok("skip") => {
                    eprintln!("e2e: skipping secret/config create (ULAK_TEST_E2E_SWARM=skip)");
                    return;
                }
                _ => panic!(
                    "the secret/config route needs a Swarm manager, and this server is not \
                     one. Ulak will not make somebody's real server into a manager on a \
                     test's say-so.\n\
                     Say ULAK_TEST_E2E_SWARM=init if this host is yours and may become one, or \
                     ULAK_TEST_E2E_SWARM=skip and know that the whole Bridge secret route goes \
                     untested on this backend."
                ),
            }
        }
        let init = server.ssh("docker swarm init --advertise-addr 127.0.0.1 >/dev/null");
        assert!(
            init.status.success(),
            "could not put the disposable fixture into Swarm mode:\n{}",
            String::from_utf8_lossy(&init.stderr)
        );
    }

    let ws = wired(&server);
    let mut probe = Probe::new(&server, "secret");
    let secret = probe.secret();
    let config = probe.config();

    ws.write("secrets/api-key.txt", "SECRET-BODY-42");
    let out = ws
        .ulak(&server)
        .args(["docker", "secret", "create", &secret, "secrets/api-key.txt"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ulak docker secret create {secret} secrets/api-key.txt` failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        server
            .ssh(&format!("docker secret inspect {} >/dev/null", q(&secret)))
            .status
            .success(),
        "the daemon has no secret named {secret}"
    );

    // `config create` is the same door with a readable room behind it:
    // Docker hands a config's bytes back, so this is the only place the
    // content that crossed can actually be compared.
    ws.write("secrets/app.conf", "CONFIG-BODY-42");
    let out = ws
        .ulak(&server)
        .args(["docker", "config", "create", &config, "secrets/app.conf"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ulak docker config create {config} secrets/app.conf` failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stored = server.ssh(&format!(
        "docker config inspect {} --format '{{{{ printf \"%s\" .Spec.Data }}}}'",
        q(&config)
    ));
    assert_eq!(
        String::from_utf8_lossy(&stored.stdout).trim_end(),
        "CONFIG-BODY-42",
        "the config's bytes did not survive the crossing"
    );

    // Neither the content nor the path it came from reaches the trail:
    // argv carried a name and a `-`, and that is all it can hold.
    let trail = audit_trail(&ws);
    assert!(
        !trail.contains("SECRET-BODY-42") && !trail.contains("CONFIG-BODY-42"),
        "ulak's own audit trail is holding the bytes it was asked to carry, not write down"
    );
    assert!(
        !trail.contains("api-key.txt") && !trail.contains("app.conf"),
        "the audit trail names a local file that was never meant to reach the server's command line"
    );

    // The WHOLE tree on purpose: this route must not stage the file
    // anywhere ulak owns, and naming the places it could would be this
    // test agreeing with its own list. It cannot be narrowed either —
    // the daemon route registers no workspace of its own, so there is
    // nothing here to scope to.
    //
    // The cost is that the server is shared, so a workspace some earlier
    // run ABANDONED can trip it: a demo fixture ships `configs/app.conf`,
    // and a suite that panicked before its teardown leaves that file
    // sitting there. The path in the message is what tells the two
    // apart — a stale one names a temp directory that no longer exists.
    let strays = server
        .ssh("find ~/.ulak \\( -name 'api-key.txt' -o -name 'app.conf' \\) -print 2>/dev/null");
    let found = String::from_utf8_lossy(&strays.stdout);
    assert!(
        found.trim().is_empty(),
        "the secret was written to the server's disk after all:\n{found}\n\
         (if that path is in a workspace this run did not make, an earlier run died \
          before its teardown — check its manifest's local_root and remove it)"
    );

    ws.forget_on_server(&server);
}

/// The archive is written beside its target and renamed into place, so a
/// `.ulak-partial` anywhere at the end means some path out of `pull`
/// returned without either finishing the rename or clearing up after
/// itself.
fn no_partial_archive_outlived_it(ws: &Workspace) {
    let litter: Vec<String> = std::fs::read_dir(&ws.project)
        .expect("read the project dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".ulak-partial"))
        .collect();
    assert!(
        litter.is_empty(),
        "half-written archives were left in the project directory: {litter:?}"
    );
}

fn swarm_is_active(server: &TestServer) -> bool {
    let out = server.ssh("docker info --format '{{.Swarm.LocalNodeState}}'");
    first_word(&out) == "active"
}

// ─── saying what actually happened ──────────────────────────────────

/// Read a path back out of the container, and when it is not there, say
/// what is — "no such file" alone never says whether the copy landed
/// under the wrong name or never landed at all.
fn assert_in_container(
    server: &TestServer,
    container: &str,
    path: &str,
    body: &str,
    mode: Option<&str>,
) {
    let out = server.ssh(&format!(
        "docker exec {c} cat {p}",
        c = q(container),
        p = q(path),
    ));
    assert!(
        out.status.success(),
        "{path} is not in the container; {} holds:\n{}",
        parent_of(path),
        listing(server, container, parent_of(path)),
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        body,
        "{path} arrived with the wrong contents"
    );
    if let Some(mode) = mode {
        let got = first_word(&server.ssh(&format!(
            "docker exec {c} stat -c %a {p}",
            c = q(container),
            p = q(path),
        )));
        assert_eq!(
            got, mode,
            "{path} arrived as mode {got}, not {mode} — the two tar implementations at the ends \
             of this copy do not agree about permission bits"
        );
    }
}

fn assert_local_file(path: &Path, body: &str, mode: Option<u32>) {
    let got = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "{} did not arrive on this machine ({e}); its directory holds {:?}",
            path.display(),
            siblings(path)
        )
    });
    assert_eq!(
        got,
        body,
        "{} arrived with the wrong contents",
        path.display()
    );
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        let got = std::fs::metadata(path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(
            got,
            mode,
            "{} arrived as mode {got:o}, not {mode:o} — the two tar implementations at the ends \
             of this copy do not agree about permission bits",
            path.display()
        );
    }
}

fn listing(server: &TestServer, container: &str, dir: &str) -> String {
    let out = server.ssh(&format!(
        "docker exec {c} ls -la {d}",
        c = q(container),
        d = q(dir),
    ));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn siblings(path: &Path) -> Vec<String> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    std::fs::read_dir(parent)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(head, _)| head).unwrap_or("/")
}

// ─── small tools ────────────────────────────────────────────────────

/// One level of shell quoting, for the command `TestServer::ssh` hands
/// to the server's own shell. The `docker cp` scenarios deliberately use
/// names with a space and a quote in them, so the test itself cannot get
/// away with pasting paths together.
fn q(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}

fn first_word(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// A file's SHA-256, spelled the way whichever machine this is spells
/// it: macOS ships `shasum`, Linux ships `sha256sum`, and the suite runs
/// on both.
fn sha256_of(path: &Path) -> String {
    for (program, args) in [("shasum", &["-a", "256"][..]), ("sha256sum", &[][..])] {
        let Ok(out) = Command::new(program).args(args).arg(path).output() else {
            continue;
        };
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
        }
    }
    panic!(
        "no sha256 tool on this machine (tried shasum and sha256sum) — the byte-for-byte \
         scenarios cannot run without one"
    );
}

/// Every member of a tar archive, as (path, SHA-256 of its contents),
/// sorted — the archive's whole-file checksum would be the obvious thing
/// to compare and is the wrong one.
///
/// Measured on Docker 28.5.2: two `docker save` runs of one settled
/// image differ in exactly two bytes, the mtime of the `blobs/sha256/`
/// directory entry and the header checksum that follows it, because that
/// entry carries the content store's own directory mtime. So a whole-
/// file comparison fails on a clock tick and says "corrupted in
/// transit", which is the one answer that must never be a false alarm.
/// Comparing every byte of every member is immune to that and still
/// catches what this bridge exists to prevent: newline translation lands
/// INSIDE the members, and a truncated stream cannot be unpacked at all.
fn members_of(tarball: &Path) -> Vec<(String, String)> {
    let unpacked = tempfile::TempDir::new().expect("tempdir for unpacking");
    let status = Command::new("tar")
        .arg("-xf")
        .arg(tarball)
        .arg("-C")
        .arg(unpacked.path())
        .status()
        .expect("spawn tar -xf");
    assert!(
        status.success(),
        "{} ({} bytes) could not be unpacked — it is not a whole tar archive",
        tarball.display(),
        size_of_file(tarball)
    );

    let mut members = Vec::new();
    let mut stack = vec![unpacked.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for path in std::fs::read_dir(&dir)
            .expect("read unpacked dir")
            .flatten()
            .map(|e| e.path())
        {
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(unpacked.path())
                    .expect("under the unpack dir")
                    .to_string_lossy()
                    .into_owned();
                let sum = sha256_of(&path);
                members.push((rel, sum));
            }
        }
    }
    members.sort();
    members
}

/// The same fingerprint, taken on the server, of whatever `producer`
/// writes to its stdout — so the two sides are compared without either
/// archive having to cross the wire twice.
fn members_on_the_server(server: &TestServer, producer: &str) -> Vec<(String, String)> {
    let out = server.ssh(&format!(
        "set -e; d=$(mktemp -d); {producer} | tar -xf - -C \"$d\"; \
         cd \"$d\" && find . -type f -exec sha256sum {{}} + | sort; rm -rf \"$d\""
    ));
    assert!(
        out.status.success(),
        "could not fingerprint `{producer}` on the server; it said:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut members: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once("  "))
        .map(|(sum, path)| {
            (
                path.trim_start_matches("./").to_string(),
                sum.trim().to_string(),
            )
        })
        .collect();
    assert!(
        !members.is_empty(),
        "`{producer}` produced an archive with no files in it, so there is nothing to compare"
    );
    members.sort();
    members
}

fn has_image(server: &TestServer, image: &str) -> bool {
    server
        .ssh(&format!("docker image inspect {} >/dev/null", q(image)))
        .status
        .success()
}

/// `manifest.json` as the archive carries it: the legacy index naming the
/// runnable image's config and layers by blob path.
fn manifest_body_of(tarball: &Path) -> String {
    let out = Command::new("tar")
        .arg("-xOf")
        .arg(tarball)
        .arg("manifest.json")
        .output()
        .expect("spawn tar -xOf");
    assert!(
        out.status.success(),
        "{} has no manifest.json — it is not an image archive",
        tarball.display()
    );
    String::from_utf8(out.stdout).expect("manifest.json is not UTF-8")
}

/// The members the RUNNABLE image is made of: `oci-layout`,
/// `manifest.json`, and the blobs `manifest.json` names — its config and
/// every layer. `index.json`, and the blobs only it reaches, are left out.
///
/// Two `docker save` runs of one tag are not two copies of one archive.
/// The fixture is `docker:29.6.2-dind`, and Engine v29 makes the
/// containerd image store the default for a new installation, so `save`
/// composes its output from live store state AT EXPORT TIME: an
/// attestation or referrer attached between the two runs adds descriptors
/// to `index.json` and its manifest, config and layer to the archive, and
/// moves nothing else. Measured on CI: the local archive carried three
/// blobs the server's did not and a different `index.json`, while
/// `oci-layout`, `manifest.json` and every blob `manifest.json` names were
/// byte-identical — two valid exports, seconds apart, of content several
/// scenarios here build identically on one shared daemon.
///
/// `manifest.json` can never gain those entries: an attestation is
/// `platform: unknown/unknown` and the legacy format cannot represent one.
/// So this projection is stable across saves while still carrying every
/// byte the image is made of. Damage in transit is not selective — a
/// newline translated by a pty, or a truncated stream, lands in the layer
/// blob, the config blob and `manifest.json` alike — so nothing these
/// scenarios exist to catch can hide in what is excluded.
fn runnable(members: &[(String, String)], manifest: &str) -> Vec<(String, String)> {
    let mut keep = vec!["oci-layout".to_string(), "manifest.json".to_string()];
    // `manifest.json` spells Config and Layers as archive paths, so the
    // blobs it names lift out without a JSON parser — this crate has no
    // serde_json in dev-dependencies and one member list does not earn it.
    for (i, _) in manifest.match_indices("blobs/sha256/") {
        let hex: String = manifest[i + "blobs/sha256/".len()..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if hex.len() == 64 {
            keep.push(format!("blobs/sha256/{hex}"));
        }
    }
    keep.sort();
    keep.dedup();
    let kept: Vec<(String, String)> = members
        .iter()
        .filter(|(path, _)| keep.binary_search(path).is_ok())
        .cloned()
        .collect();
    // oci-layout, manifest.json, one config and at least one layer. A
    // manifest this scan could not read would otherwise pass by keeping
    // nothing, which is the vacuous green AGENTS.md forbids.
    assert!(
        kept.len() >= 4,
        "the archive names no runnable image: kept {kept:?} of {members:?}"
    );
    kept
}

fn size_of_file(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// `uid:gid` of a local path, in the same spelling `stat -c %u:%g`
/// prints on the far side, so the two can be compared as written.
fn owner_of(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).expect("stat");
    format!("{}:{}", meta.uid(), meta.gid())
}

/// A PATH whose `tar` writes part of the tree and then exits 2.
///
/// `bridge::unpack` runs `Command::new("tar")`, so this substitutes the
/// external tool rather than reaching into the product — the same thing
/// the harness already does for `ssh`. Everything else (rsync, ssh)
/// still resolves further down the PATH it is prepended to.
///
/// It writes one file before failing because "exited non-zero" and "left
/// a half-extracted tree behind" are the two halves of the bug, and a
/// stub that produced nothing at all would let a `place` that only
/// checked for emptiness pass.
fn path_with_a_tar_that_fails_halfway(ws: &Workspace, server: &TestServer) -> String {
    let bin = ws.home.join("fake-tar-bin");
    std::fs::create_dir_all(&bin).expect("mkdir fake tar bin");
    let tar = bin.join("tar");
    std::fs::write(
        &tar,
        "#!/bin/sh\n\
         while [ $# -gt 0 ]; do\n\
         \x20 case \"$1\" in -C) shift; into=\"$1\";; esac\n\
         \x20 shift\n\
         done\n\
         mkdir -p \"$into/unpackable\"\n\
         echo half >\"$into/unpackable/fine.txt\"\n\
         cat >/dev/null\n\
         echo 'tar: nodedev: Cannot mknod: Operation not permitted' >&2\n\
         exit 2\n",
    )
    .expect("write fake tar");
    let mut perms = std::fs::metadata(&tar)
        .expect("stat fake tar")
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&tar, perms).expect("chmod fake tar");
    // Prepended to the SERVER's PATH and not this process's: the harness
    // reaches the fixture through an `ssh` shim that lives on that one,
    // and a PATH without it would point ulak at the developer's own
    // ~/.ssh/config.
    let inherited = server
        .env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
    format!("{}:{inherited}", bin.display())
}

/// Local `docker cp` staging directories, which name themselves so a
/// leaked one can be found and reported rather than merely deleted.
fn staging_dirs_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(".ulak-cp-"))
                .collect()
        })
        .unwrap_or_default()
}

fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("cannot stat {}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// The mode a path has INSIDE the container, in the same octal the local
/// side is compared in — so "it arrived with the mode it left with" is
/// one comparison rather than an argument.
fn mode_in_container(server: &TestServer, container: &str, path: &str) -> u32 {
    let out = server.ssh(&format!(
        "docker exec {c} stat -c %a {p}",
        c = q(container),
        p = q(path),
    ));
    let text = first_word(&out);
    u32::from_str_radix(&text, 8)
        .unwrap_or_else(|e| panic!("{path} in the container reported mode {text:?}: {e}"))
}

fn pack(dir: &Path, member: &str, into: &Path) {
    let sink = std::fs::File::create(into).expect("create tarball");
    let status = Command::new("tar")
        // This fixture models a portable tar supplied on stdin. macOS
        // metadata is not part of the stream behaviour under test and a
        // Linux Docker daemon cannot restore its unnamespaced xattrs.
        .env("COPYFILE_DISABLE", "1")
        .arg("--no-xattrs")
        .arg("-C")
        .arg(dir)
        .args(["-cf", "-", "--", member])
        .stdout(Stdio::from(sink))
        .status()
        .expect("spawn tar");
    assert!(
        status.success(),
        "could not pack {member} from {}",
        dir.display()
    );
}

fn tar_members(tarball: &Path) -> Vec<String> {
    let out = Command::new("tar")
        .arg("-tf")
        .arg(tarball)
        .output()
        .expect("spawn tar -tf");
    assert!(
        out.status.success(),
        "{} is not a readable tar archive: {}",
        tarball.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Every `commands.jsonl` this sandbox produced, concatenated. The trail
/// is keyed by a hash of the project path, so the scenarios read it by
/// walking rather than by recomputing that hash.
fn audit_trail(ws: &Workspace) -> String {
    let mut text = String::new();
    let mut stack = vec![ws.home.join(".local/state/ulak")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()) {
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name() == Some(std::ffi::OsStr::new("commands.jsonl"))
                && let Ok(body) = std::fs::read_to_string(&path)
            {
                text.push_str(&body);
            }
        }
    }
    text
}
