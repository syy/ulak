//! E2e harness: gives every test an SSH-reachable "server" with rsync,
//! docker and compose v2 on it.
//!
//! Backends, selected via `ULAK_TEST_E2E`:
//!   - `docker` (default when a local docker daemon is available):
//!     dockerized sshd + DinD fixture from `e2e/sshd/`, non-root user.
//!   - `host:<name>`: a real server reachable as `ssh <name>` through the
//!     user's own ~/.ssh/config (e.g. `host:my-server`). Tests must only
//!     touch their own `~/.ulak/workspaces/<namespace>/*` and docker resources they create.
//!   - `skip`: e2e tests become no-ops (unit tests still run).
//!
//! What each backend COSTS was measured over the same suites: the
//! dockerized fixture ~4 min, a real host ~3.3 min. The fixture is the
//! SLOWER one, because it pays ~20 s a suite building and booting its
//! own container and waiting for dockerd, while a real host is already
//! up. So "a real server is the expensive backend" is measured false —
//! the fixture is the default for isolation, not for speed.
//!
//! The fixture is reached WITHOUT touching the user's ~/.ssh/config: the
//! harness prepends a PATH shim that makes `ssh` inject `-F <our config>`,
//! so the ulak binary under test stays 100% free of test seams.
//!
//! No suite pulls an image: every scenario that runs a container asks
//! `needs_image` to hand that image over from THIS machine's daemon,
//! because a suite whose colour depends on an anonymous internet quota
//! is a suite people learn to re-run instead of read. See `needs_image`
//! for what that cost, measured. `ULAK_TEST_E2E_HUB=block` takes Docker Hub
//! away from the fixture entirely and is how that claim gets audited —
//! see `no_registry_hosts`.
//!
//! Long unattended runs want `caffeinate -i`. Every deadline in here is
//! `Instant`-based and `Instant` does not tick through a macOS sleep, so
//! a laptop that dozes mid-suite freezes the local clock while the
//! server keeps running: the link dies and heals underneath the test.
//! The result is neither green nor red, and the failure mode PASSES —
//! no red run will ever hand this to you.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const IMAGE: &str = "ulak-e2e-sshd:latest";

/// The fixture's baseline compose. It mounts the whole project ON
/// PURPOSE: under the footprint model what travels is what compose
/// REFERENCES, so a scenario that expects its whole tree on the server
/// has to say so — exactly as a real project would. Tests that swap
/// compose.yaml out and then keep testing the tree must restore THIS,
/// not a bare image line. (Read-only: these scenarios are about
/// transfer fidelity, not about container writes.)
pub const TREE_COMPOSE: &str =
    "services:\n  web:\n    image: nginx:alpine\n    volumes:\n      - .:/app:ro\n";

pub struct TestServer {
    /// SSH destination: `ssh <alias>` must reach the server.
    pub alias: String,
    /// Extra environment for every process under test (PATH shim etc.).
    pub env: Vec<(String, String)>,
    kind: Kind,
    link: Link,
    /// Base images already put onto this daemon, and what the attempt
    /// said. See `needs_image`.
    seeded: Mutex<Vec<(String, String)>>,
}

enum Kind {
    Docker { container_id: String },
    Real,
}

/// Everything needed to reach the server — and to CUT that reach without
/// touching the server itself.
///
/// ulak under test runs with a PATH shim whose `ssh` adds
/// `-F <active>`; swapping the contents of that one file is a link
/// failure indistinguishable from wifi going away, and it works on the
/// dockerized fixture and on a real host alike. That is what closes the
/// hole: `restart()` only ever worked on the docker backend, so the
/// entire connection axis went untested against my-server.
///
/// The harness's OWN ssh deliberately bypasses the shim, so a test can
/// still look at the server while ulak cannot reach it.
struct Link {
    /// Holds the shim and both configs; dropped with the server.
    _state: TempDir,
    /// The file the shim passes to `-F`. Its CONTENT is what changes.
    active: PathBuf,
    /// The same healthy config, never swapped — what the harness's own
    /// ssh uses so it can still see the server during a break.
    good_path: PathBuf,
    /// A Mutex rather than a RefCell because `TestServer::shared` hands
    /// one server to every `#[test]` in a binary and those run on
    /// different threads: a RefCell here is what would make the whole
    /// struct un-Sync and the sharing impossible.
    good: Mutex<String>,
    broken: String,
    real_ssh: String,
    /// Short private dir for ulak's ssh control sockets. Short
    /// because the full socket path has to stay under the ~104-byte
    /// unix limit, and per-test so cutting the link can take the live
    /// masters with it without touching anyone else's.
    runtime: PathBuf,
    /// Armed: how many seconds to hold the PULL leg still. See
    /// `stall_the_pull`.
    stall: PathBuf,
    /// Written by the shim the moment a held pull begins.
    pull_began: PathBuf,
    /// The same two files for the PUSH leg. See `stall_the_push`.
    stall_push: PathBuf,
    push_began: PathBuf,
}

impl Link {
    fn apply(&self, text: &str) {
        std::fs::write(&self.active, text).expect("write ssh config");
    }

    fn heal(&self) {
        self.apply(&self.healthy());
    }

    fn healthy(&self) -> String {
        self.good
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .to_string()
    }

    /// The fixture's published port changes on every container restart,
    /// so the healthy config is not a constant.
    fn rewrite_good(&self, text: String) {
        std::fs::write(&self.good_path, &text).expect("write good ssh config");
        *self.good.lock().unwrap_or_else(|e| e.into_inner()) = text;
        self.heal();
    }

    /// Kill every control master under this test's runtime dir. An
    /// established connection survives a config change on its own, so
    /// without this "the link is down" would only apply to connections
    /// ulak has not made yet.
    fn drop_masters(&self) {
        let _ = Command::new("pkill")
            .arg("-f")
            .arg(self.runtime.display().to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(self.runtime.join("ulak"));
    }
}

impl TestServer {
    /// Start (or attach to) a test server. `None` means "skip this test"
    /// and the reason has been printed to stderr.
    pub fn start() -> Option<TestServer> {
        sweep_leaked_services();
        match std::env::var("ULAK_TEST_E2E").as_deref() {
            Ok("skip") => {
                eprintln!("e2e: skipped (ULAK_TEST_E2E=skip)");
                None
            }
            Ok(host) if host.starts_with("host:") => Some(Self::attach_real(
                host.trim_start_matches("host:").to_string(),
            )),
            _ => Self::start_docker(),
        }
    }

    pub fn is_real_host(&self) -> bool {
        matches!(self.kind, Kind::Real)
    }

    /// One server for a whole test binary, so a suite can be many
    /// `#[test]` functions instead of one.
    ///
    /// The shape this replaces: twenty named scenarios called in
    /// sequence from a single `#[test]`. That reports "1 passed" for all
    /// twenty, and the first failure aborts the other nineteen — so a
    /// regression sweep sees one red line and no idea how much else
    /// broke. Splitting is the fix, and the reason it was not done is
    /// cost: `start()` builds and boots a privileged DinD container,
    /// which is most of what a suite spends.
    ///
    /// So the server is shared rather than duplicated. The first test
    /// through boots it, every test that overlaps it borrows the same
    /// one, and the LAST holder drops it — which keeps the container's
    /// removal in `TestServer::drop`, where a panicking test and a
    /// finished one both reach it, instead of handing it to a reaper
    /// that would have to outlive the process.
    ///
    /// Two things a caller owes this: cargo runs a binary's tests in
    /// PARALLEL, so every container, image and volume a scenario makes
    /// must be named for that scenario, and any "the server is holding
    /// nothing" assertion has to be scoped or polled — another test's
    /// work in flight is not a leak. A suite that cannot honour that
    /// should keep calling `start()` and own its own server.
    ///
    /// The other difference from `start()`: only `ULAK_TEST_E2E=skip` buys
    /// silence here. A fixture that could not START is not a pass, and
    /// `start()` answers `None` for both — so on a machine with no
    /// reachable docker daemon every e2e suite reports ok and a whole
    /// axis of this product goes untested with a green tick on it.
    /// Saying "skip" is a decision; a missing daemon is an accident.
    pub fn shared() -> Option<Arc<TestServer>> {
        static LIVE: Mutex<Weak<TestServer>> = Mutex::new(Weak::new());
        // Held across the boot on purpose: without it every test that
        // starts before the first one is ready boots a server of its own.
        let mut slot = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(server) = slot.upgrade() {
            return Some(server);
        }
        let Some(server) = TestServer::start() else {
            assert_eq!(
                std::env::var("ULAK_TEST_E2E").as_deref(),
                Ok("skip"),
                "no e2e backend came up, and these tests must not report ok for that. \
                 Start docker, or point the suite at a server with ULAK_TEST_E2E=host:<name>, \
                 or say ULAK_TEST_E2E=skip and mean it"
            );
            return None;
        };
        let server = Arc::new(server);
        *slot = Arc::downgrade(&server);
        Some(server)
    }

    /// A real host reached through the user's own ~/.ssh/config — but
    /// still behind the shim, so the link can be cut. The healthy config
    /// is nothing but an Include of theirs; the broken one puts a
    /// blackhole address (RFC 5737 TEST-NET-1, which nothing routes) in
    /// front of it. ssh takes the FIRST value it obtains for a keyword,
    /// so the override has to come before the Include.
    fn attach_real(alias: String) -> TestServer {
        let state = TempDir::new().expect("tempdir");
        let user_config = std::env::var("HOME")
            .map(|h| format!("{h}/.ssh/config"))
            .unwrap_or_default();
        let good = format!("Include {user_config}\n");
        let broken = format!(
            "Host {alias}\n\x20 HostName 192.0.2.1\n\x20 ConnectTimeout 5\nInclude {user_config}\n"
        );
        let server = TestServer {
            alias,
            env: Vec::new(),
            kind: Kind::Real,
            link: Self::wire(state, good, broken),
            seeded: Mutex::new(Vec::new()),
        };
        server.finish_wiring()
    }

    /// Write the shim, publish the environment every process under test
    /// inherits, and start out healthy.
    fn wire(state: TempDir, good: String, broken: String) -> Link {
        let active = state.path().join("ssh_config");
        let good_path = state.path().join("ssh_config.good");
        let stall = state.path().join("stall-the-pull");
        let pull_began = state.path().join("pull-began");
        let stall_push = state.path().join("stall-the-push");
        let push_began = state.path().join("push-began");
        let link = Link {
            active,
            good_path,
            good: Mutex::new(String::new()),
            broken,
            real_ssh: which_ssh(),
            runtime: short_runtime_dir(),
            stall,
            pull_began,
            stall_push,
            push_began,
            _state: state,
        };
        link.rewrite_good(good);
        link
    }

    fn finish_wiring(mut self) -> TestServer {
        let bin = self.link._state.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir shim bin");
        let shim = bin.join("ssh");
        std::fs::write(
            &shim,
            // Each leg of the reconcile names itself in the command rsync
            // asks the far side to run: the far side is a `--sender` only
            // when the bytes are coming DOWN, and it is a plain
            // `rsync --server` receiver only when they are going UP. Those
            // are the two threads of the reconcile a test can hold still,
            // and holding either one opens — on demand, every time — a
            // window this product actually loses files in: the walk that
            // decided what travels has finished, and rsync has not yet
            // looked at this machine. See `stall_the_pull` and
            // `stall_the_push`.
            //
            // Order matters: the pull's command carries BOTH words, so it
            // has to be matched first or a held pull would be answered by
            // the push's clause.
            format!(
                "#!/bin/sh\n\
                 case \"$*\" in\n\
                 \x20 *--sender*)\n\
                 \x20   if [ -f {stall} ]; then\n\
                 \x20     : > {began}\n\
                 \x20     sleep \"$(cat {stall})\"\n\
                 \x20   fi\n\
                 \x20   ;;\n\
                 \x20 *\"rsync --server\"*)\n\
                 \x20   if [ -f {stall_push} ]; then\n\
                 \x20     : > {push_began}\n\
                 \x20     sleep \"$(cat {stall_push})\"\n\
                 \x20   fi\n\
                 \x20   ;;\n\
                 esac\n\
                 exec {real} -F {cfg} \"$@\"\n",
                stall = self.link.stall.display(),
                began = self.link.pull_began.display(),
                stall_push = self.link.stall_push.display(),
                push_began = self.link.push_began.display(),
                real = self.link.real_ssh,
                cfg = self.link.active.display()
            ),
        )
        .expect("write ssh shim");
        make_executable(&shim);

        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        self.env = vec![
            ("PATH".into(), path),
            (
                "XDG_RUNTIME_DIR".into(),
                self.link.runtime.display().to_string(),
            ),
        ];
        self
    }

    /// Cut ulak's reach to the server without touching the server.
    /// Existing control masters go too, or "the link is down" would only
    /// apply to connections ulak has not opened yet.
    pub fn break_link(&self) {
        self.link.apply(&self.link.broken);
        self.link.drop_masters();
    }

    pub fn heal_link(&self) {
        self.link.heal();
    }

    /// Hold the workspace's DOWN leg still for `secs`, and say when it has
    /// started — a stopwatch on the race instead of a dice roll.
    ///
    /// ulak decides what the pull must not carry home BEFORE it spawns
    /// rsync, and rsync looks at this machine only after the server has
    /// built and sent its file list. Everything a user does in between is
    /// invisible to that decision. On my-server that window is a few
    /// hundred milliseconds and the suite hit it in 3 runs out of 7;
    /// widened here it is hit in all of them, which is the difference
    /// between a test that can prove a fix and one that cannot.
    ///
    /// The product does not know any of this exists: the pull is held by
    /// the ssh shim the harness already owns, on the one flag that tells
    /// the two directions apart (`--sender`).
    pub fn stall_the_pull(&self, secs: u64) {
        let _ = std::fs::remove_file(&self.link.pull_began);
        std::fs::write(&self.link.stall, format!("{secs}\n")).expect("arm the pull stall");
    }

    pub fn stop_stalling_the_pull(&self) {
        let _ = std::fs::remove_file(&self.link.stall);
        let _ = std::fs::remove_file(&self.link.pull_began);
    }

    /// Block until the held pull has begun. `false` means it never did —
    /// which is a real answer (nothing pulled), not a timeout to hide.
    pub fn wait_until_the_pull_starts(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while !self.link.pull_began.exists() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// Hold the workspace's UP leg still for `secs`, and say when it has
    /// started — the other side of `stall_the_pull`, and the same
    /// stopwatch instead of the same dice roll.
    ///
    /// The window this opens is the one a save lands in and becomes
    /// undeletable: ulak walks the tree to decide what the ledger will
    /// claim, then talks to the server, and only then does rsync scan
    /// this machine — so a file written in between travels up while the
    /// ledger has never heard of it. Measured, and this is why the stall
    /// exists rather than a sleep: the scenario that owns that bug used
    /// to guess at the window with five sleeps from 200 ms to 900 ms, and
    /// on the dockerized fixture it missed with all five, every run — a
    /// warm control master gets ulak from its walk to rsync's scan in
    /// well under the shortest of them. It then printed a note and
    /// returned, which libtest captures for a test that PASSES.
    ///
    /// Held on the SPAWN of the transport rather than anywhere inside
    /// rsync, which is what makes it exact: rsync starts its remote shell
    /// before it builds the file list (measured — a file created during a
    /// held rsh is transferred), so a write made while this holds is
    /// strictly after the walk and strictly before the scan.
    ///
    /// What a caller owes this, and `stall_the_pull` alike: the shim is
    /// the server's, not the test's, so while a stall is armed it answers
    /// EVERY leg going that way. Under `TestServer::shared` that means a
    /// scenario holding a window must be the only one syncing at that
    /// moment, or a sibling test's push satisfies the wait and the window
    /// the caller thinks it is standing in belongs to somebody else.
    /// Exclusive use of the server, for the window a stall is armed in.
    ///
    /// `stall_the_push` above states the obligation — "a scenario holding
    /// a window must be the only one syncing at that moment" — and
    /// stating it was the whole of what enforced it. The shim belongs to
    /// the SERVER: while a stall is armed it answers every leg going that
    /// way, and `wait_until_the_push_starts` only tests that a marker
    /// file exists, so it cannot tell whose leg wrote it. `e2e_sync` puts
    /// three `#[test]`s on one `shared()` server, cargo runs them in
    /// parallel, and all three drive real rsync legs — so a sibling's
    /// push satisfied the wait and the window the caller thought it was
    /// standing in belonged to somebody else.
    ///
    /// Held for the whole scenario rather than just the arming, because
    /// the hazard is the sibling's TRANSFER and not the write. Every test
    /// in a binary that stalls owes this, including the ones that never
    /// stall anything: a lock one side ignores is not a lock.
    ///
    /// Poisoning is deliberately swallowed. A panicking test has already
    /// failed and reported; turning its neighbours red as well would
    /// bury the one diagnosis worth reading.
    pub fn exclusive_sync() -> std::sync::MutexGuard<'static, ()> {
        static SOLO: Mutex<()> = Mutex::new(());
        SOLO.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn stall_the_push(&self, secs: u64) {
        let _ = std::fs::remove_file(&self.link.push_began);
        std::fs::write(&self.link.stall_push, format!("{secs}\n")).expect("arm the push stall");
    }

    pub fn stop_stalling_the_push(&self) {
        let _ = std::fs::remove_file(&self.link.stall_push);
        let _ = std::fs::remove_file(&self.link.push_began);
    }

    /// Block until the held push has begun. `false` means it never did —
    /// which is a real answer (nothing was pushed), not a timeout to hide.
    pub fn wait_until_the_push_starts(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while !self.link.push_began.exists() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// Make ssh fail to AUTHENTICATE rather than fail to connect —
    /// without touching the server, and without touching a key.
    ///
    /// Measured (phase 4): a passphrase-protected key with no agent and
    /// no terminal produces no prompt at all; ssh skips it in silence and
    /// leaves `Permission denied (publickey,password)`, byte-for-byte
    /// what an unknown key leaves. Turning every client-side method off
    /// reproduces exactly that stderr on both backends, so the sentence
    /// the service owes a user whose keychain was locked at boot can be
    /// tested without locking anybody's keychain.
    pub fn refuse_keys(&self) {
        let refused = format!(
            "Host {alias}\n\x20 PubkeyAuthentication no\n\x20 PasswordAuthentication no\n\
             \x20 KbdInteractiveAuthentication no\n\x20 GSSAPIAuthentication no\n{good}",
            alias = self.alias,
            good = self.link.healthy(),
        );
        self.link.apply(&refused);
        // A command riding an established master authenticates nobody —
        // measured: it succeeds no matter what the config now says.
        self.link.drop_masters();
    }

    fn start_docker() -> Option<TestServer> {
        // A missing daemon is a broken environment, not a decision, and
        // it must not be answered the way `ULAK_TEST_E2E=skip` is. libtest
        // captures stderr for tests that PASS, so a scenario that
        // printed its reason and returned is indistinguishable from one
        // that ran — and with the whole suite gated here, every binary
        // reports green having asserted nothing. Whoever added a
        // regression test would watch it pass on a machine that never
        // executed it. Skipping stays available; it just has to be
        // asked for, in writing.
        assert!(
            status_ok(Command::new("docker").args(["info"])),
            "e2e needs a Docker daemon and none answered `docker info`.\n\
             Start one (OrbStack or Docker Desktop), or point the suite at a real \
             server with ULAK_TEST_E2E=host:<name>.\n\
             To run without it — and know that every e2e assertion below is being \
             skipped — set ULAK_TEST_E2E=skip."
        );

        let fixture_dir = repo_root().join("e2e/sshd");
        let build = Command::new("docker")
            .args(["build", "-t", IMAGE])
            .arg(&fixture_dir)
            .output()
            .expect("docker build spawn");
        assert!(
            build.status.success(),
            "fixture image build failed:\n{}",
            String::from_utf8_lossy(&build.stderr)
        );

        let run = Command::new("docker")
            .args([
                "run",
                "-d",
                "--privileged",
                "-e",
                "DOCKER_TLS_CERTDIR=",
                "-p",
                "127.0.0.1:0:22",
            ])
            .args(no_registry_hosts())
            .arg(IMAGE)
            .output()
            .expect("docker run spawn");
        assert!(
            run.status.success(),
            "fixture container start failed:\n{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let container_id = String::from_utf8_lossy(&run.stdout).trim().to_string();

        let server = Self::configure_docker(container_id, fixture_dir);
        Some(server)
    }

    fn configure_docker(container_id: String, _fixture_dir: PathBuf) -> TestServer {
        let state = TempDir::new().expect("tempdir");
        let port = mapped_ssh_port(&container_id);

        // Ephemeral client key, authorized inside the container.
        let key = state.path().join("id_ed25519");
        let keygen = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .output()
            .expect("ssh-keygen spawn");
        assert!(keygen.status.success(), "ssh-keygen failed");
        let pubkey = std::fs::read(key.with_extension("pub")).expect("read pubkey");

        let mut exec = Command::new("docker")
            .args([
                "exec",
                "-i",
                &container_id,
                "sh",
                "-c",
                // The entrypoint also creates ~/.ssh; do it here too so
                // this exec can't race the container's boot sequence.
                "mkdir -p /home/dev/.ssh \
                 && cat > /home/dev/.ssh/authorized_keys \
                 && chown -R dev:dev /home/dev/.ssh \
                 && chmod 700 /home/dev/.ssh \
                 && chmod 600 /home/dev/.ssh/authorized_keys",
            ])
            .stdin(Stdio::piped())
            .spawn()
            .expect("docker exec spawn");
        use std::io::Write;
        exec.stdin.take().unwrap().write_all(&pubkey).unwrap();
        assert!(
            exec.wait().unwrap().success(),
            "authorized_keys install failed"
        );

        // ssh client config for the fixture, behind the same shim the
        // real-host backend uses — so break_link() works on both.
        let alias = "ulak-e2e";
        let server = TestServer {
            alias: alias.to_string(),
            env: Vec::new(),
            kind: Kind::Docker { container_id },
            link: {
                let good = fixture_config(state.path(), alias, port);
                Self::wire(state, good, blackhole_config(alias))
            },
            seeded: Mutex::new(Vec::new()),
        }
        .finish_wiring();

        server.wait_until_ready();
        server
    }

    /// Block until sshd answers and dockerd inside the fixture is up.
    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if self.try_ssh("true") {
                break;
            }
            assert!(Instant::now() < deadline, "fixture sshd never became ready");
            std::thread::sleep(Duration::from_millis(500));
        }
        loop {
            if self.try_ssh("docker info >/dev/null 2>&1") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fixture dockerd never became ready"
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn try_ssh(&self, remote_cmd: &str) -> bool {
        self.ssh_raw(remote_cmd)
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    /// Run a command on the server; panics on spawn failure.
    pub fn ssh(&self, remote_cmd: &str) -> Output {
        self.ssh_raw(remote_cmd).expect("ssh spawn")
    }

    /// The same round trip for a caller that must not panic: teardown,
    /// which runs while a test is already failing and, in a `Drop`,
    /// while it is unwinding.
    pub fn ssh_quiet(&self, remote_cmd: &str) -> Option<Output> {
        self.ssh_raw(remote_cmd).ok()
    }

    /// Put a base image the scenarios build on onto the server's daemon,
    /// ONCE however many of them ask for it — and never by pulling it
    /// from a registry.
    ///
    /// Every scenario that runs a container owes this call. Measured the
    /// hard way, twice. First: nine tests sharing a server started
    /// together, each ran `docker pull alpine:3.20`, and Docker Hub
    /// answered the tail of them with `toomanyrequests: You have reached
    /// your unauthenticated pull rate limit`. Then, with the pull made
    /// once instead of nine times, the whole suite still spent the
    /// anonymous quota in four runs of one day and seven of sixteen test
    /// binaries went red — none of them for a reason that has anything
    /// to do with ulak. A red that means nothing is worse than no test
    /// at all: it teaches people to re-run instead of read, and the next
    /// red that DOES mean something gets the same shrug.
    ///
    /// So the bytes come from THIS machine's daemon, which already has
    /// them: `docker save | ssh docker load`, the same trick
    /// `ulak docker save | ulak docker load` gives a user. The suite is
    /// then green or red for reasons that live on this machine, and an
    /// image nobody here has is a loud, named failure rather than a
    /// quiet skip or a stack of registry HTTP.
    ///
    /// The lock is held across the transfer on purpose: that is what
    /// turns nine simultaneous requests into one, and the eight that
    /// wait get the same verdict rather than a second transfer. The memo
    /// lives on the server, not in a static, because "this image is
    /// here" is a fact about one daemon and a suite may own more than
    /// one.
    pub fn needs_image(&self, image: &str) {
        let mut seeded = self.seeded.lock().unwrap_or_else(|e| e.into_inner());
        let said = match seeded.iter().find(|(name, _)| name == image) {
            Some((_, said)) => said.clone(),
            None => {
                let said = self.seed(image).err().unwrap_or_default();
                seeded.push((image.to_string(), said.clone()));
                said
            }
        };
        assert!(
            said.is_empty(),
            "the e2e server needs the image {image}, and {said}.\n\
             \n\
             This suite never pulls: Docker Hub counts its anonymous limit per IP, \
             so a suite that pulls goes red on the fourth run of a day for reasons \
             that have nothing to do with ulak. It seeds the server from your own \
             daemon instead.\n\
             \n\
             Fix it once, here, with:  docker pull {image}"
        );
    }

    /// Get `image` onto the server's daemon without a registry in the
    /// picture. `Err` is the sentence that goes in front of the
    /// instructions in `needs_image`.
    fn seed(&self, image: &str) -> Result<(), String> {
        if self
            .ssh(&format!("docker image inspect {image} >/dev/null 2>&1"))
            .status
            .success()
        {
            return Ok(());
        }
        if !status_ok(Command::new("docker").args(["image", "inspect", image])) {
            return Err("this machine's own daemon has no copy to hand over".to_string());
        }
        self.hand_over(image)
            .map_err(|e| format!("handing it over from this machine failed: {e}"))
    }

    /// Stream an image from this machine's daemon into the server's,
    /// which is what `ulak docker save | ulak docker load` does for a
    /// user and works here for the same reason: the bytes are on this
    /// machine already.
    fn hand_over(&self, image: &str) -> Result<(), String> {
        let mut save = Command::new("docker")
            .args(["save", image])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run docker save: {e}"))?;
        let feed = save
            .stdout
            .take()
            .ok_or_else(|| "docker save produced no stdout".to_string())?;
        let load = Command::new(&self.link.real_ssh)
            .arg("-F")
            .arg(&self.link.good_path)
            .args(["-o", "ConnectTimeout=5", "-o", "BatchMode=yes"])
            .arg(&self.alias)
            .arg("docker load")
            .stdin(Stdio::from(feed))
            .output()
            .map_err(|e| format!("cannot run docker load over ssh: {e}"))?;
        let saved = save.wait().map_err(|e| e.to_string())?;
        if !saved.success() {
            return Err(format!("`docker save {image}` failed on this machine"));
        }
        if !load.status.success() {
            return Err(String::from_utf8_lossy(&load.stderr).trim().to_string());
        }
        Ok(())
    }

    /// Deliberately NOT through the shim, and always with the healthy
    /// config: while `break_link()` holds, the test still has to be able
    /// to look at the server. Only ulak is blindfolded.
    fn ssh_raw(&self, remote_cmd: &str) -> std::io::Result<Output> {
        Command::new(&self.link.real_ssh)
            .arg("-F")
            .arg(&self.link.good_path)
            .args(["-o", "ConnectTimeout=5", "-o", "BatchMode=yes"])
            .arg(&self.alias)
            .arg(remote_cmd)
            .output()
    }

    /// Apply this server's environment to a command under test.
    pub fn env_for(&self, cmd: &mut Command) {
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
    }

    /// Restart the fixture container, which kills every live connection
    /// including the control masters. Only the docker backend can do
    /// this; `break_link()` is the portable way to lose a connection and
    /// is what the service scenarios use.
    pub fn restart(&self) -> bool {
        let Kind::Docker { container_id } = &self.kind else {
            return false;
        };
        let ok = Command::new("docker")
            .args(["restart", container_id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            // An ephemeral published port gets a NEW number on restart —
            // the ssh config must follow it (measured, not theoretical).
            let port = mapped_ssh_port(container_id);
            self.link
                .rewrite_good(fixture_config(self.link._state.path(), &self.alias, port));
            self.wait_until_ready();
        }
        ok
    }
}

/// Kill the services a killed test binary could not.
///
/// `Service::drop` is the normal way one ends, and it cannot run when
/// the test binary is SIGKILLed or `cargo test` is interrupted. What is
/// left behind then never stops on its own: a service's intent is
/// deliberately TTL-free, so it keeps probing the server for as long
/// as the machine is up.
///
/// Measured, and this is why the sweep exists rather than a comment:
/// leaked services were found still running DAYS after the run that
/// spawned them, still opening ssh connections to the test server, one
/// stuck forever in the "refused every ssh key" state the `refuse_keys`
/// scenario leaves behind. A test that leaks a permanent SSH client is
/// not a tidy one.
///
/// The pattern is the test binary's OWN path, so it can never match the
/// user's installed `ulak` — a real service maintaining real workspaces
/// runs from `~/.cargo/bin` and must survive this untouched. The cost of
/// the bluntness that remains: two suites deliberately run in parallel
/// from the same checkout would sweep each other. cargo runs test
/// targets one at a time, so that is a thing someone has to go out of
/// their way to do.
fn sweep_leaked_services() {
    let exe = env!("CARGO_BIN_EXE_ulak");
    let _ = Command::new("pkill")
        .arg("-f")
        .arg(format!("{exe} service run"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `ULAK_TEST_E2E_HUB=block`: `--add-host` entries that put Docker Hub out of
/// the fixture's reach, so a scenario that forgot `needs_image` cannot
/// hide behind a machine that still has pull quota.
///
/// This is how you AUDIT the claim that the suite needs no registry, and
/// it is how that claim was established: with it set, every e2e binary
/// but one passes, so nothing in them reaches Docker Hub. Run it after
/// adding a scenario that starts a container.
///
/// It is not the default, and the reason is honest rather than
/// principled: under it `e2e_service` failed five runs out of five and
/// passed both runs without it, while no registry error appears anywhere
/// in the failing output — what breaks is `clean`, on container-written
/// root-owned files, which is a race that suite already had. Blocking
/// evidently shifts its timing. Until somebody explains that, the
/// enforcement stays a switch you throw, because a harness that makes a
/// suite red for an unexplained reason is the same disease this whole
/// mechanism was built to cure.
///
/// `--add-host` rather than an exec that edits /etc/hosts, because
/// docker rewrites that file on every container start and would undo it
/// at the first `restart()`; these entries it writes back itself.
/// 127.0.0.1 rather than a blackhole address so the refusal is instant —
/// an audit run must not pay a connect timeout per image. Only ever the
/// disposable fixture: a real host under `ULAK_TEST_E2E=host:<name>` is
/// somebody's machine, and its name resolution is not the harness's to
/// touch.
fn no_registry_hosts() -> Vec<String> {
    if std::env::var("ULAK_TEST_E2E_HUB").as_deref() != Ok("block") {
        return Vec::new();
    }
    [
        "registry-1.docker.io",
        "index.docker.io",
        "registry.docker.io",
        "auth.docker.io",
        "production.cloudflare.docker.com",
    ]
    .iter()
    .flat_map(|host| ["--add-host".to_string(), format!("{host}:127.0.0.1")])
    .collect()
}

fn fixture_config(state: &Path, alias: &str, port: u16) -> String {
    format!(
        "Host {alias}\n\
         \x20 HostName 127.0.0.1\n\
         \x20 Port {port}\n\
         \x20 User dev\n\
         \x20 IdentityFile {key}\n\
         \x20 IdentitiesOnly yes\n\
         \x20 StrictHostKeyChecking accept-new\n\
         \x20 UserKnownHostsFile {known_hosts}\n",
        key = state.join("id_ed25519").display(),
        known_hosts = state.join("known_hosts").display(),
    )
}

/// A short, private directory for ulak's ssh control sockets.
/// SHORT because the socket path has to stay under the ~104-byte unix
/// limit and macOS's own $TMPDIR is already most of that; PER-TEST so
/// that cutting the link can take this test's masters with it and
/// nobody else's. Removed when the server is dropped.
fn short_runtime_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = PathBuf::from(format!(
        "/tmp/ulak-e2e-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("runtime dir");
    dir
}

/// 192.0.2.1 is RFC 5737 TEST-NET-1: nothing routes it, so a connection
/// attempt goes into the void exactly the way it does when the laptop's
/// wifi disappears — rather than failing fast the way an unresolvable
/// name would, which would test nothing.
fn blackhole_config(alias: &str) -> String {
    format!("Host {alias}\n\x20 HostName 192.0.2.1\n\x20 Port 22\n\x20 ConnectTimeout 5\n")
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.link.drop_masters();
        let _ = std::fs::remove_dir_all(&self.link.runtime);
        if let Kind::Docker { container_id, .. } = &self.kind {
            // `-v` because the fixture's base image declares
            // `VOLUME /var/lib/docker`: every boot gets an anonymous
            // volume holding the DinD daemon's own storage — the images
            // `needs_image` hands over live in there — and `rm` without
            // it takes the container while leaving that behind. A full
            // run boots roughly eighteen fixtures, so the leftovers are
            // not a curiosity: this machine had 16 of them holding 2 GB
            // before the flag was added. `-v` removes only anonymous
            // volumes, which is all the fixture has.
            let _ = Command::new("docker")
                .args(["rm", "-f", "-v", container_id])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

/// A local fixture for driving the ulak binary: a fake HOME (config
/// layers land there) plus a scratch compose project.
pub struct Workspace {
    _tmp: TempDir,
    pub home: PathBuf,
    pub project: PathBuf,
}

impl Workspace {
    pub fn new() -> Workspace {
        let tmp = TempDir::new().expect("workspace tempdir");
        let home = tmp.path().join("home");
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("compose.yaml"), TREE_COMPOSE).unwrap();
        Workspace {
            _tmp: tmp,
            home,
            project,
        }
    }

    /// Point this workspace's project at a server without running init.
    /// The local layer is appropriate here because every test fixture
    /// gets its own ephemeral destination.
    pub fn set_host(&self, alias: &str) {
        std::fs::write(
            self.project.join("ulak.local.toml"),
            format!("host = \"{alias}\"\n"),
        )
        .unwrap();
    }

    /// Copy the examples/demo fixture (the 5-mechanism prober project)
    /// into this workspace's project dir, minus the server-owned data/ dir.
    pub fn use_demo_fixture(&self) {
        let src = repo_root().join("examples/demo");
        copy_tree(&src, &self.project);
        let _ = std::fs::remove_dir_all(self.project.join("data"));
    }

    /// A raw std Command for the ulak binary (long-running commands
    /// like watch/forward need spawn(), which assert_cmd hides).
    pub fn ulak_raw(&self, server: &TestServer) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ulak"));
        cmd.current_dir(&self.project);
        self.sandbox_env(&mut cmd);
        for (k, v) in &server.env {
            cmd.env(k, v);
        }
        cmd
    }

    /// The developer's real config and state must never leak into a test
    /// sandbox (nor the test's into theirs).
    fn sandbox_env<C: EnvSink>(&self, cmd: &mut C) {
        cmd.set_env("HOME", self.home.as_os_str());
        cmd.set_env("XDG_STATE_HOME", self.home.join(".local/state").as_os_str());
        cmd.unset_env("XDG_CONFIG_HOME");
    }

    /// A ulak invocation with no test server at all. Scenarios about
    /// the CLIENT side — deadlines above all — need an unreachable
    /// destination, not a working one, and must run everywhere.
    pub fn ulak_alone(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::cargo_bin("ulak").expect("ulak binary");
        cmd.current_dir(&self.project);
        self.sandbox_env(&mut cmd);
        cmd
    }

    /// A ulak invocation wired to this workspace and the test server.
    pub fn ulak(&self, server: &TestServer) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::cargo_bin("ulak").expect("ulak binary");
        cmd.current_dir(&self.project);
        self.sandbox_env(&mut cmd);
        for (k, v) in &server.env {
            cmd.env(k, v);
        }
        cmd
    }

    /// Where sync markers live for this sandbox.
    pub fn workspaces_dir(&self) -> PathBuf {
        self.home.join(".local/state/ulak/workspaces")
    }

    /// Where Docker-stack intent, status and tunnel pid files live.
    pub fn stacks_dir(&self) -> PathBuf {
        self.home.join(".local/state/ulak/stacks")
    }

    /// Remove every workspace this workspace made on the server.
    ///
    /// Belt and braces over each suite's own teardown, and needed
    /// because the two do not see the same thing: a workspace id follows
    /// the FIRST `-f`, so a suite that uses several compose files makes
    /// several workspaces, and cleaning by the path one command happened to
    /// print leaves the others behind. The test server is not the
    /// suite's alone — what a test makes there, a test removes.
    pub fn forget_on_server(&self, server: &TestServer) {
        for id in self.workspace_ids() {
            // Never `ssh`, which `expect`s the spawn. Cleanup runs on the
            // FAILURE path too, and now from inside a `Drop` during an
            // unwind — where a second panic aborts the whole binary and
            // takes the first one's diagnosis with it. A workspace that
            // could not be removed is a leak; a double panic is a suite
            // that cannot say what broke.
            let root = self.remote_workspace_root(&id);
            let _ = server.ssh_quiet(&format!("rm -rf {root}"));
        }
        if let Some(namespace) = self.workspace_namespace() {
            let _ = server.ssh_quiet(&format!("rmdir .ulak/workspaces/{namespace}"));
        }
    }

    /// Every workspace this workspace touched, by id.
    ///
    /// The layout contract (`~/.ulak/workspaces/<namespace>/<workspace-id>/…`) makes this
    /// the reliable way to clean up on a real server: parsing a path out
    /// of a command's output only works when that command succeeded, and
    /// the scenarios that most need cleaning are the ones about failure.
    pub fn workspace_ids(&self) -> Vec<String> {
        std::fs::read_dir(self.workspaces_dir())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// This test client's stable namespace, once any workspace command has
    /// resolved it. A missing value means the fixture has not touched a
    /// workspace yet.
    pub fn workspace_namespace(&self) -> Option<String> {
        std::fs::read_to_string(self.home.join(".local/state/ulak/client/namespace")).ok()
    }

    /// Full remote root for one local workspace id.
    pub fn remote_workspace_root(&self, id: &str) -> String {
        let namespace = self
            .workspace_namespace()
            .expect("a workspace id must have a client namespace");
        format!(".ulak/workspaces/{namespace}/{id}")
    }

    pub fn write(&self, rel: &str, contents: &str) {
        let p = self.project.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }

    /// Every rsync the SERVICE ran, in order, as "direction + whether the
    /// name was held back" — read from the audit trail the product keeps
    /// anyway, so a diagnosis needs no test seam in the product.
    ///
    /// A path that will not leave the server is always one of two
    /// stories, and only the ORDER of the legs tells them apart: a push
    /// that recreates it, or a pull that carries it home. `dn`/`up` plus
    /// the hold rule that was (or was not) in force answers both.
    pub fn rsync_trail(&self, needle: &str) -> String {
        let trail = self.home.join(".local/state/ulak/service/commands.jsonl");
        let Ok(text) = std::fs::read_to_string(&trail) else {
            return format!("(no audit trail at {})", trail.display());
        };
        let mut lines = Vec::new();
        for entry in text
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        {
            if entry["kind"] != "rsync" {
                continue;
            }
            let argv: Vec<String> = entry["argv"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            // The last argument is the destination: a remote one carries
            // "host:", so the direction is readable off the argv alone.
            let dir = match argv.last() {
                Some(dst) if dst.contains(':') => "up",
                Some(_) => "dn",
                None => "??",
            };
            let held: Vec<&String> = argv
                .iter()
                .filter(|a| a.starts_with("--filter=- ") && a.contains(needle))
                .collect();
            // The destination verbatim, because "it pushed and the file
            // did not change" has two readings — the bytes never left,
            // or they landed somewhere else — and only the path the
            // argv actually named tells them apart.
            let dest = argv.last().cloned().unwrap_or_default();
            lines.push(format!(
                "  {} {} exit={} dest={dest} held={:?}",
                entry["ts_unix"], dir, entry["exit"], held
            ));
        }
        lines.join("\n")
    }
}

/// The intent ulak recorded for this workspace, read the way the
/// service will read it. `None` means no command has declared anything
/// yet — which is a real answer, not a missing one.
pub fn desired(ws: &Workspace) -> Option<serde_json::Value> {
    stack_file(ws, "desired.json")
}

/// One Compose project's intent when a scenario deliberately creates
/// several stacks from the same checkout.
pub fn desired_for(ws: &Workspace, identity: &str) -> Option<serde_json::Value> {
    stack_file_for(ws, "desired.json", identity)
}

/// What the service last reported — the same file `ulak status` reads.
pub fn service_status(ws: &Workspace) -> Option<serde_json::Value> {
    stack_file(ws, "status.json")
}

pub fn service_status_for(ws: &Workspace, identity: &str) -> Option<serde_json::Value> {
    stack_dir_for(ws, identity)
        .and_then(|dir| std::fs::read_to_string(dir.join("status.json")).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
}

fn stack_file(ws: &Workspace, name: &str) -> Option<serde_json::Value> {
    stack_files(ws, name).into_iter().next()
}

fn stack_file_for(ws: &Workspace, name: &str, identity: &str) -> Option<serde_json::Value> {
    let dir = stack_dir_for(ws, identity)?;
    std::fs::read_to_string(dir.join(name))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

fn stack_dir_for(ws: &Workspace, identity: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(ws.stacks_dir()).ok()?;
    entries.flatten().map(|entry| entry.path()).find(|dir| {
        std::fs::read_to_string(dir.join("desired.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .is_some_and(|desired| desired["identity"] == identity)
    })
}

fn stack_files(ws: &Workspace, name: &str) -> Vec<serde_json::Value> {
    let Ok(entries) = std::fs::read_dir(ws.stacks_dir()) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path().join(name))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect()
}

/// A running `ulak service run --foreground`.
///
/// Its words go to a file rather than to /dev/null: a service scenario
/// that fails, fails as a silent timeout, and "the file never arrived"
/// is a symptom, never a diagnosis. The log is reprinted on any panic.
pub struct Service {
    child: std::process::Child,
    log: PathBuf,
    stacks: PathBuf,
}

impl Service {
    pub fn start(ws: &Workspace, server: &TestServer) -> Service {
        let log = ws.home.join("../service.log");
        let sink = std::fs::File::create(&log).expect("service log");
        let child = ws
            .ulak_raw(server)
            .args(["service", "run", "--foreground"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(sink))
            .spawn()
            .expect("spawn ulak service");
        Service {
            child,
            log,
            stacks: ws.stacks_dir(),
        }
    }

    /// Block until the service's own report satisfies `want`.
    pub fn wait_status(
        &self,
        ws: &Workspace,
        secs: u64,
        what: &str,
        want: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(st) = service_status(ws)
                && want(&st)
            {
                return st;
            }
            assert!(
                Instant::now() < deadline,
                "the service never reported {what}; last status: {:?}",
                service_status(ws)
            );
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    pub fn wait_status_for(
        &self,
        ws: &Workspace,
        identity: &str,
        secs: u64,
        what: &str,
        want: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(st) = service_status_for(ws, identity)
                && want(&st)
            {
                return st;
            }
            assert!(
                Instant::now() < deadline,
                "the service never reported {what} for {identity}; last status: {:?}",
                service_status_for(ws, identity)
            );
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    pub fn said(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Ctrl-C in a real terminal signals the whole process group and
        // takes the tunnel children with it. A test's SIGKILL does not,
        // so the pid ledger the service keeps for exactly this case is
        // what closes them here — the same file its own startup sweep
        // reads.
        if let Ok(entries) = std::fs::read_dir(&self.stacks) {
            for pid_file in entries.flatten().map(|e| e.path().join("tunnels.pid")) {
                let Ok(text) = std::fs::read_to_string(&pid_file) else {
                    continue;
                };
                let _ = std::fs::remove_file(&pid_file);
                if let Ok(pid) = text.trim().parse::<u32>() {
                    let _ = Command::new("kill")
                        .arg(pid.to_string())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
        if std::thread::panicking() {
            match std::fs::read_to_string(&self.log) {
                Ok(text) if !text.trim().is_empty() => {
                    eprintln!("--- ulak service said: ---\n{text}\n-----------------------------");
                }
                _ => eprintln!("--- ulak service said nothing at all ---"),
            }
        }
    }
}

/// A named sequence of scenarios that hand each other one workspace, so
/// a break says what fell over and what never got the chance to.
///
/// One `#[test]` that runs several scenarios reports one line, and that
/// line says nothing about WHERE it broke — while everything queued
/// behind the break leaves no trace at all, indistinguishable from a
/// scenario that ran and passed. Splitting is the better fix and is what
/// the sibling suites did; this is for the suites that genuinely cannot
/// be split, where every step is the state the next one needs.
///
/// The panic is re-raised UNCHANGED, so libtest still prints the
/// original assertion, its message and its location, and the map lands
/// in the same captured stderr immediately after it.
///
/// It lives here rather than in one suite because four of them need it
/// and a second copy of a map is a second thing to keep in step.
pub struct Chain {
    plan: &'static [&'static str],
    /// How many links have been STARTED — so the one that broke is
    /// `at - 1` while the panic is in flight.
    at: usize,
}

impl Chain {
    pub fn new(plan: &'static [&'static str]) -> Chain {
        Chain { plan, at: 0 }
    }

    pub fn link<T>(&mut self, body: impl FnOnce() -> T) -> T {
        assert!(
            self.at < self.plan.len(),
            "the chain ran more links than its plan names — add the new scenario to the \
             plan, or the map will point at the wrong one"
        );
        self.at += 1;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            Ok(value) => value,
            Err(panic) => {
                eprintln!("{}", self.map());
                std::panic::resume_unwind(panic)
            }
        }
    }

    fn map(&self) -> String {
        let broke = self.at - 1;
        let mut out = String::from("\n--- where the chain broke ---\n");
        for (i, name) in self.plan.iter().enumerate() {
            let mark = match i.cmp(&broke) {
                std::cmp::Ordering::Less => "  passed     ",
                std::cmp::Ordering::Equal => "  FAILED     ",
                std::cmp::Ordering::Greater => "  never ran  ",
            };
            out.push_str(mark);
            out.push_str(name);
            out.push('\n');
        }
        out.push_str(
            "\nEverything under the break was abandoned, not passed: these scenarios \
             hand each other one workspace. The panic above is the failure — this is \
             only the map.\n",
        );
        out
    }

    /// Every link the plan names actually ran. Without this the plan and
    /// the calls drift apart in silence and the map starts naming the
    /// wrong scenario, which is the one thing a map may never do.
    pub fn finish(self) {
        assert_eq!(
            self.at,
            self.plan.len(),
            "the plan names {} links and the test ran {} — the map cannot be trusted \
             until they agree",
            self.plan.len(),
            self.at
        );
    }
}

/// Both command flavours the tests use, behind one tiny trait so the
/// sandbox rules are written once.
pub trait EnvSink {
    fn set_env(&mut self, key: &str, value: &std::ffi::OsStr);
    fn unset_env(&mut self, key: &str);
}

impl EnvSink for Command {
    fn set_env(&mut self, key: &str, value: &std::ffi::OsStr) {
        self.env(key, value);
    }
    fn unset_env(&mut self, key: &str) {
        self.env_remove(key);
    }
}

impl EnvSink for assert_cmd::Command {
    fn set_env(&mut self, key: &str, value: &std::ffi::OsStr) {
        self.env(key, value);
    }
    fn unset_env(&mut self, key: &str) {
        self.env_remove(key);
    }
}

/// Kills the child on drop — a panicking test must not leak watchers
/// or tunnel processes.
pub struct ChildGuard(pub std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Pull `~/.ulak/workspaces/<namespace>/<hash>/<name>` out of ulak's info line.
pub fn extract_remote_dir(stderr: &str) -> Option<String> {
    let start = stderr.find("~/.ulak/workspaces/")?;
    let rest = &stderr[start + 2..]; // drop "~/"
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '\u{1b}')
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn mapped_ssh_port(container_id: &str) -> u16 {
    let out = Command::new("docker")
        .args(["port", container_id, "22/tcp"])
        .output()
        .expect("docker port spawn");
    assert!(out.status.success(), "docker port failed");
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.rsplit(':').next()?.trim().parse().ok())
        .expect("parse mapped port")
}

fn which_ssh() -> String {
    let out = Command::new("sh")
        .args(["-c", "command -v ssh"])
        .output()
        .expect("which ssh");
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!p.is_empty(), "no ssh binary on PATH");
    p
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).expect("stat shim").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod shim");
}

fn status_ok(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
