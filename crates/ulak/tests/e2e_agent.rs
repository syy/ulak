//! Phase 4: the service becomes part of the system.
//!
//! Not one test here runs `launchctl` or `systemctl` — the acceptance
//! rule, and the reason a fake `launchctl` sits on PATH throughout: if
//! anything in ulak ever reaches for it during an ordinary command,
//! the shim records the call and the test fails. What IS tested is the
//! content of the file the system will act on, which is the only part of
//! "it comes back after a reboot" that can be checked without one.
//!
//! Nothing here needs a server or docker: the agent, the heartbeat and
//! the intent-file integrity check are all local truths.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::Workspace;

/// A sandbox whose PATH starts with a directory holding fake `launchctl`
/// and `systemctl` binaries. They record every call and exit 0, so a
/// ulak that reached for them would be caught here rather than on the
/// developer's own login items.
struct Sandbox {
    ws: Workspace,
    calls: PathBuf,
    bin: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let ws = Workspace::new();
        let bin = ws.home.join("fake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let calls = ws.home.join("system-calls.log");
        for tool in ["launchctl", "systemctl", "loginctl"] {
            let shim = bin.join(tool);
            // `print` answers the way the real one does once a job is
            // gone: non-zero. That is what `wait_until_gone` reads, and
            // a shim that said "still loaded" to everything would make
            // every install sit out the full grace period.
            std::fs::write(
                &shim,
                format!(
                    "#!/bin/sh\necho \"{tool} $*\" >> {}\n\
                     case \"$1\" in print) exit 1 ;; esac\nexit 0\n",
                    calls.display()
                ),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Sandbox { ws, calls, bin }
    }

    fn ulak(&self) -> assert_cmd::Command {
        let mut cmd = self.ws.ulak_alone();
        cmd.env(
            "PATH",
            format!(
                "{}:{}",
                self.bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        cmd
    }

    /// What ulak asked the system to do. Empty is the answer every
    /// test in this file expects unless it explicitly installed.
    fn system_calls(&self) -> String {
        std::fs::read_to_string(&self.calls).unwrap_or_default()
    }

    /// Where the unit lands on this platform. Both are checked because
    /// the generator for both is compiled on both.
    fn unit(&self) -> Option<PathBuf> {
        let candidates = [
            self.ws.home.join("Library/LaunchAgents/dev.ulak.plist"),
            self.ws.home.join(".config/systemd/user/ulak.service"),
        ];
        candidates.into_iter().find(|p| p.is_file())
    }
}

/// The file a reboot will act on. Everything the acceptance criteria ask
/// about "no command typed" lives in this content and nowhere else.
#[test]
fn the_installed_unit_is_what_brings_the_service_back() {
    let sb = Sandbox::new();

    sb.ulak()
        .args(["service", "install", "--no-start"])
        .assert()
        .success();

    let unit = sb
        .unit()
        .expect("install must write a unit for this platform");
    let text = std::fs::read_to_string(&unit).unwrap();

    // It must run the service the way the SWITCH can still stop it.
    // `--foreground` means "I am asking for this by hand" and overrides
    // `[service] auto = false`, so an agent that passed it would make
    // the machine-wide off switch a lie.
    assert!(
        !text.contains("--foreground"),
        "the unit must never pass --foreground:\n{text}"
    );
    assert!(text.contains("service"), "{text}");

    // And it must point at the binary that installed it.
    let exe = assert_cmd::cargo::cargo_bin("ulak");
    let exe = exe.canonicalize().unwrap_or(exe);
    assert!(
        text.contains(&exe.display().to_string()),
        "the unit must name the running binary ({}):\n{text}",
        exe.display()
    );

    if cfg!(target_os = "macos") {
        // The measured trap: launchd enforces a 10-second minimum
        // runtime, so plain `KeepAlive = true` against a service that
        // exits 0 under `auto = false` is ~8 640 spawns a day.
        assert!(text.contains("<key>SuccessfulExit</key>"), "{text}");
        assert!(text.contains("<key>RunAtLoad</key>"), "{text}");
    } else {
        assert!(text.contains("Restart=on-failure"), "{text}");
        assert!(!text.contains("Restart=always"), "{text}");
        assert!(text.contains("KillMode=control-group"), "{text}");
    }

    // Only its own permissions: the unit names the user's paths.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&unit).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Installing again is how a user restarts the service after an
    // upgrade, so it has to be idempotent rather than an error.
    sb.ulak()
        .args(["service", "install", "--no-start"])
        .assert()
        .success();
    assert_eq!(std::fs::read_to_string(&unit).unwrap(), text);

    // `--no-start` promised not to touch the system, and kept it.
    assert_eq!(
        sb.system_calls(),
        "",
        "install --no-start must not call launchctl/systemctl"
    );
}

/// The hand-off to the system, argv and all.
///
/// It runs against the fake `launchctl` on PATH — the real one is never
/// reached, which is the whole reason the shim is there — so what is
/// checked is the SHAPE of the request. `bootout` has to come first,
/// because `bootstrap` refuses a label that is already loaded and
/// re-installing is how a user restarts the service after an upgrade.
#[test]
fn starting_it_asks_the_system_in_the_one_shape_that_works() {
    let sb = Sandbox::new();
    let out = sb.ulak().args(["service", "install"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let calls = sb.system_calls();
    let unit = sb.unit().expect("a unit was written").display().to_string();
    if cfg!(target_os = "macos") {
        let boot = calls.find("launchctl bootout gui/").expect(&calls);
        let strap = calls.find("launchctl bootstrap gui/").expect(&calls);
        assert!(boot < strap, "bootstrap refuses a loaded label:\n{calls}");
        // And it must WAIT for the old job in between. `bootout` returns
        // while launchd is still tearing it down; measured in the field,
        // bootstrapping straight after gave `Bootstrap failed: 5:
        // Input/output error` — with the old service already stopped, so
        // the upgrade left the machine with no service at all.
        let waited = calls.find("launchctl print gui/").expect(&calls);
        assert!(
            boot < waited && waited < strap,
            "the old label must be waited out between bootout and bootstrap:\n{calls}"
        );
        assert!(
            calls.contains(&unit),
            "bootstrap must name the file it just wrote:\n{calls}"
        );
    } else {
        assert!(calls.contains("systemctl --user daemon-reload"), "{calls}");
        // `enable --now` would leave a running unit on the old binary;
        // `restart` after `enable` covers both a stopped and a running one.
        let enable = calls
            .find("systemctl --user enable ulak.service")
            .expect(&calls);
        let restart = calls
            .find("systemctl --user restart ulak.service")
            .expect(&calls);
        assert!(enable < restart, "restart must follow enable:\n{calls}");
        // Pointed at, never run for the user: linger changes what happens
        // to their processes at logout, which is theirs to decide.
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(said.contains("enable-linger"), "{said}");
        assert!(!calls.contains("loginctl"), "{calls}");
    }
}

/// Removing it is one command, and afterwards there is no agent left —
/// nor a claim that there was one when there was not.
#[test]
fn uninstall_is_one_command_and_says_the_truth_either_way() {
    let sb = Sandbox::new();

    // Nothing installed: an honest answer, not an error.
    sb.ulak()
        .args(["service", "uninstall"])
        .assert()
        .success()
        .stderr(predicates::str::contains("no service agent"));
    assert_eq!(sb.system_calls(), "", "there was nothing to boot out");

    sb.ulak()
        .args(["service", "install", "--no-start"])
        .assert()
        .success();
    assert!(sb.unit().is_some());

    sb.ulak()
        .args(["service", "uninstall"])
        .assert()
        .success()
        .stderr(predicates::str::contains("agent removed"));
    assert!(
        sb.unit().is_none(),
        "the unit file must be gone after uninstall"
    );

    // THIS is the one place a system call is correct: a file removed
    // while the service keeps running is a service the user believes is
    // gone and which keeps syncing until the next reboot.
    let calls = sb.system_calls();
    let stopped = if cfg!(target_os = "macos") {
        calls.contains("launchctl bootout")
    } else {
        calls.contains("systemctl --user disable --now")
    };
    assert!(
        stopped,
        "uninstall must stop the running service: {calls:?}"
    );
}

/// "Uninstalled" has to mean the ports too.
///
/// The system's promise on the way out is five seconds and then SIGKILL,
/// which no `Drop` survives — so a tunnel child can outlive the service
/// that owned it, and a local port pointing at a server nobody maintains
/// is precisely the failure this product exists to remove. Removing the
/// agent while leaving that behind would be the worst possible half-job.
#[test]
fn uninstall_closes_the_tunnels_the_service_owned() {
    let sb = Sandbox::new();

    // A real ssh carrying the tunnel marker, held open by a ProxyCommand
    // that does nothing but sleep: ssh waits for a banner that never
    // comes, so it sits there without a packet leaving this machine.
    //
    // It used to sit on 192.0.2.1 (RFC 5737 TEST-NET-1) and that made the
    // test a question about the network underneath it. A network that
    // answers the unroutable address with "unreachable" instead of
    // silence kills ssh at once; nothing has reaped it yet, so `ps` reads
    // `[ssh] <defunct>`, the marker is gone with the argv, and the sweep
    // correctly finds nothing to close. The failure then lands on
    // `uninstall`, which did its job, and says the message is missing.
    // CI fell into it on a run whose predecessor had passed.
    //
    // The sweep only kills processes that still carry the marker — pids
    // are recycled, and a stale file must never become a weapon.
    let mut orphan = common::ChildGuard(
        Command::new("ssh")
            .args([
                "-o",
                "SendEnv=ULAK_TUNNEL",
                "-o",
                "ProxyCommand=sleep 60",
                "-N",
                "stand-in-for-a-tunnel",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a stand-in tunnel"),
    );
    let pid = orphan.0.id();
    let stack = sb.ws.stacks_dir().join("aaaaaaaaaaaaaaaa");
    std::fs::create_dir_all(&stack).unwrap();
    std::fs::write(stack.join("tunnels.pid"), pid.to_string()).unwrap();

    sb.ulak()
        .args(["service", "install", "--no-start"])
        .assert()
        .success();
    // Said before the thing under test runs, so that a stand-in which
    // died on its own is named as such instead of being reported as a
    // sweep that stayed quiet.
    assert!(
        orphan.0.try_wait().unwrap().is_none(),
        "the stand-in tunnel (pid {pid}) died before uninstall could sweep \
         it — there was nothing left to close, so this test can say nothing \
         about whether uninstall closes tunnels"
    );

    sb.ulak()
        .args(["service", "uninstall"])
        .assert()
        .success()
        .stderr(predicates::str::contains("port tunnel"));

    let deadline = Instant::now() + Duration::from_secs(10);
    while orphan.0.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "uninstall left a tunnel holding a local port (pid {pid})"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !stack.join("tunnels.pid").exists(),
        "and the ledger entry must go with it"
    );
}

/// The gate that makes the acceptance rule structural: ulak installs
/// itself on a machine where a human can read the sentence explaining it,
/// which means a test, a script and a CI job can never reach `launchctl`
/// through an ordinary command.
#[test]
fn ordinary_commands_never_touch_the_system_without_a_terminal() {
    let sb = Sandbox::new();

    // Commands that all go through the auto-install path.
    for args in [vec!["doctor"], vec!["status"], vec!["sync", "--dry-run"]] {
        let _ = sb.ulak().args(&args).output().unwrap();
    }
    assert_eq!(
        sb.system_calls(),
        "",
        "no command may install a login item where nobody can read about it"
    );
    assert!(
        sb.unit().is_none(),
        "and no unit may appear behind a script's back"
    );
}

/// The other half of the off switch. The plist says "do not revive a
/// clean exit"; this says the exit IS clean.
///
/// If `service run` failed instead — an obvious way to write "I was told
/// not to run" — then `KeepAlive` would revive it, and against launchd's
/// measured 10-second minimum runtime that is ~8 640 spawns a day for a
/// user who explicitly switched the service off.
#[test]
fn the_off_switch_is_an_exit_the_system_leaves_alone() {
    let sb = Sandbox::new();
    let global = sb.ws.home.join(".config/ulak/config.toml");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, "[service]\nauto = false\n").unwrap();

    let started = Instant::now();
    let out = sb.ulak().args(["service", "run"]).output().unwrap();
    assert!(
        out.status.success(),
        "a service told not to run must exit 0, or the system revives it forever:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "and it must exit at once, not after doing work"
    );
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("auto = false"), "{said}");
    // The manual crank must survive the switch: `--foreground` means "I
    // am asking for this by hand", which is not what the switch is about.
    assert!(said.contains("--foreground"), "{said}");

    // And nothing installs itself while the switch is off.
    assert!(sb.unit().is_none());
    assert_eq!(sb.system_calls(), "");
}

/// `StandardErrorPath` grows for as long as the service lives. Rotating
/// it while the service runs is not ours to do — the system opened that
/// fd before our first instruction, and a rename would carry our own
/// stderr off with it — so the moment we DO own is the install.
#[test]
fn the_service_log_does_not_grow_forever_in_silence() {
    let sb = Sandbox::new();
    let log = sb.ws.home.join(".local/state/ulak/service/service.log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, vec![b'x'; 9 * 1024 * 1024]).unwrap();

    sb.ulak()
        .args(["service", "install", "--no-start"])
        .assert()
        .success()
        .stderr(predicates::str::contains("kept as"));

    assert!(
        log.with_extension("log.1").is_file(),
        "the grown log must be kept, not deleted"
    );
    assert!(
        !log.exists() || std::fs::metadata(&log).unwrap().len() == 0,
        "and the service must start writing to a fresh one"
    );

    // A small log is left exactly where it is.
    std::fs::write(&log, b"one line\n").unwrap();
    sb.ulak()
        .args(["service", "install", "--no-start"])
        .assert()
        .success();
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "one line\n");
}

/// The likeliest way this whole phase fails quietly: the unit carries an
/// absolute path, and a `cargo install` into a new prefix, a moved binary
/// or a deleted checkout all leave something that looks installed and
/// runs nothing at all. Silence is the worst possible symptom, so it has
/// to have a name in `doctor`.
#[test]
fn an_agent_pointing_at_a_binary_that_is_gone_is_named() {
    let sb = Sandbox::new();
    let moved = sb.ws.home.join("moved-ulak");
    std::fs::copy(assert_cmd::cargo::cargo_bin("ulak"), &moved).unwrap();

    // Install FROM the copy, so the unit names the copy.
    let ok = Command::new(&moved)
        .current_dir(&sb.ws.project)
        .env("HOME", &sb.ws.home)
        .env("XDG_STATE_HOME", sb.ws.home.join(".local/state"))
        .env_remove("XDG_CONFIG_HOME")
        .args(["service", "install", "--no-start"])
        .status()
        .expect("install from the copy")
        .success();
    assert!(ok);
    std::fs::remove_file(&moved).unwrap();

    // doctor stops at "server not configured" here, but the LOCAL rows —
    // this one among them — are printed before it gets there.
    let out = sb.ulak().arg("doctor").output().unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        said.contains("moved-ulak"),
        "doctor must name the binary the agent points at:\n{said}"
    );
    assert!(
        said.contains("ulak service install"),
        "and the command that repoints it:\n{said}"
    );
}

/// "Is the service running, and is it the binary you just used?" — one
/// file, because no other one can answer it: a workspace whose stack is
/// down is deliberately silent, which looks exactly like a dead service.
#[test]
fn the_running_service_says_who_it_is() {
    let sb = Sandbox::new();
    let beat = sb.ws.home.join(".local/state/ulak/service/heartbeat.json");

    let mut child = {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ulak"));
        cmd.current_dir(&sb.ws.project)
            .env("HOME", &sb.ws.home)
            .env("XDG_STATE_HOME", sb.ws.home.join(".local/state"))
            .env_remove("XDG_CONFIG_HOME")
            .args(["service", "run", "--foreground"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        common::ChildGuard(cmd.spawn().expect("spawn service"))
    };

    let value = wait_for_json(&beat, 20).expect("the service must report that it is running");
    assert_eq!(value["schema"], 1);
    assert_eq!(
        value["pid"].as_u64(),
        Some(child.0.id() as u64),
        "the heartbeat must name the process that wrote it: {value}"
    );
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        value["exe"].as_str().unwrap_or_default().contains("ulak"),
        "and the binary it came from, so an upgrade can be noticed: {value}"
    );
    let first = value["updated_unix"].as_u64().unwrap_or(0);
    assert!(first > 0);
    assert_eq!(value["started_unix"].as_u64(), Some(first));

    // Only its owner's business, like every other file ulak keeps.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&beat).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let _ = child.0.kill();
    let _ = child.0.wait();
}

/// The service manager is not the only way a second process can appear:
/// somebody can run one by hand, or an install can be interrupted between
/// two labels. The process itself therefore owns the final singleton
/// guarantee.
#[test]
fn a_second_service_leaves_the_first_one_in_charge() {
    let sb = Sandbox::new();
    let beat = sb.ws.home.join(".local/state/ulak/service/heartbeat.json");

    let mut first = {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ulak"));
        cmd.current_dir(&sb.ws.project)
            .env("HOME", &sb.ws.home)
            .env("XDG_STATE_HOME", sb.ws.home.join(".local/state"))
            .env_remove("XDG_CONFIG_HOME")
            .args(["service", "run", "--foreground"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        common::ChildGuard(cmd.spawn().expect("spawn first service"))
    };
    wait_for_json(&beat, 20).expect("the first service must take ownership");

    let started = Instant::now();
    let second = sb
        .ulak()
        .args(["service", "run", "--foreground"])
        .output()
        .unwrap();
    assert!(second.status.success(), "the duplicate must leave cleanly");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the duplicate waited instead of leaving"
    );
    let said = String::from_utf8_lossy(&second.stderr);
    assert!(said.contains("already running"), "{said}");
    assert!(
        first.0.try_wait().unwrap().is_none(),
        "the original service must remain in charge"
    );

    let _ = first.0.kill();
    let _ = first.0.wait();
}

/// `desired.json` is untrusted input that a process holding the user's
/// ssh keys acts on, and this is the check that binds the paths inside
/// it to the place it was found. Stack identity and sync workspace are
/// both pinned, so rewriting either cannot redirect the service.
#[test]
fn an_intent_file_cannot_point_the_service_somewhere_else() {
    let sb = Sandbox::new();
    let elsewhere = sb.ws.home.join("not-my-project");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("compose.yaml"), "services: {}\n").unwrap();

    // A directory whose name is a plausible stack id, holding an intent
    // that names a compose file belonging somewhere else entirely.
    let planted = sb.ws.stacks_dir().join("0123456789abcdef");
    std::fs::create_dir_all(&planted).unwrap();
    std::fs::write(
        planted.join("desired.json"),
        serde_json::json!({
            "schema": 2,
            "live": true,
            "workspace_id": "fedcba9876543210",
            "destination": "planted-server",
            "identity": "planted-project",
            "cwd": elsewhere.display().to_string(),
            "argv_globals": [
                "-f", elsewhere.join("compose.yaml").display().to_string(),
                "--project-directory", elsewhere.display().to_string(),
            ],
            "compose_env": {},
            "updated_unix": 1,
        })
        .to_string(),
    )
    .unwrap();

    let log = sb.ws.home.join("service-refusal.log");
    let mut child = {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ulak"));
        cmd.current_dir(&sb.ws.project)
            .env("HOME", &sb.ws.home)
            .env("XDG_STATE_HOME", sb.ws.home.join(".local/state"))
            .env_remove("XDG_CONFIG_HOME")
            .args(["service", "run", "--foreground"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&log).unwrap()));
        common::ChildGuard(cmd.spawn().expect("spawn service"))
    };

    let said = wait_for_text(&log, "does not match its stack or sync workspace", 20);
    let _ = child.0.kill();
    let _ = child.0.wait();

    assert!(
        said.contains("does not match its stack or sync workspace"),
        "the service must refuse an intent whose paths do not belong to it; it said:\n{said}"
    );
    assert!(
        said.contains("ulak docker compose up -d"),
        "and say how to declare it properly: {said}"
    );
    // Refused means refused: no status was ever published for it.
    assert!(
        !planted.join("status.json").exists(),
        "a refused stack must not be maintained at all"
    );
}

fn wait_for_json(path: &Path, secs: u64) -> Option<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_text(path: &Path, needle: &str, secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) || Instant::now() >= deadline {
            return text;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
