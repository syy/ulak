//! The service becomes part of the system.
//!
//! Everything before this phase still asked for one command:
//! `ulak service run --foreground`. A local bind mount asks for none,
//! so neither may we — the goal sentence is "close the lid, open it in
//! the morning, keep working, **without typing anything**", and the last
//! word of it is what this module removes.
//!
//! **The switch lives in the FILE, not in the code.** The agent runs
//! `ulak service run` without `--foreground`, and `service::run`'s
//! first five lines exit 0 when `[service] auto = false`. So the plist
//! must say `KeepAlive = {SuccessfulExit: false}` and the unit
//! `Restart=on-failure`: a clean exit means "you were told not to run",
//! and reviving it is the opposite of obeying. Measured, plain
//! `KeepAlive = true` against launchd's 10-second minimum runtime is
//! ~8 640 spawns a day for a service the user switched off.
//!
//! **The heartbeat answers two questions with one file.** "Is the
//! service running?" and "is it the binary I just used?" are the same
//! lookup, and neither `status.json` (one per Docker stack, written only while
//! things go well) nor the process table (which cannot say which version
//! it is) can answer them. A stale heartbeat beside a live pid is the
//! one thing launchd structurally cannot see: running, and wedged.
//!
//! **A version mismatch is SAID, never acted on.** Restarting the
//! service in the middle of somebody's `up` is worse than the mismatch.
//!
//! Nothing here is a daemon of ours: `launchctl` and `systemctl` are
//! ordinary subprocesses with visible argv, recorded like every other
//! privileged step ulak takes.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::invocation::{self, write_private};
use crate::proc;
use crate::ui::{self, fail};

/// Reverse-DNS from the owned product domain, because launchd wants a stable label the
/// user can recognise in `launchctl list`. This is an identifier, not a
/// documentation URL; the website does not need to exist for it to be valid.
pub const LABEL: &str = "dev.ulak";
const UNIT: &str = "ulak.service";

/// The heartbeat is written every 15 s, so a minute of silence is four
/// missed beats — long enough that a busy tick never looks like death,
/// short enough that "it is running" is a fresh claim and not a memory.
const STALE: u64 = 60;
pub const BEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// A service that runs for months writes to one file forever. Rotation
/// while it runs is not ours to do — launchd opened that fd before our
/// first instruction and a rename would take our own stderr with it — so
/// the moment we DO own is the install, and the size is said out loud
/// everywhere else.
const LOG_CAP: u64 = 8 * 1024 * 1024;

const SCHEMA: u32 = 1;

/// Where the agent's own subprocesses are recorded: one trail for the
/// service, the same one `service::run` writes to.
const AUDIT: &str = "service";

/// Every binary the service runs locally is `ssh`, `rsync`, `ps`, `kill`.
/// Three of those are in the system paths and rsync is found by absolute
/// path (`sync::RSYNC_CANDIDATES`), so this list is hygiene rather than
/// life support — measured, launchd hands an agent
/// `/usr/bin:/bin:/usr/sbin:/sbin` and ulak stopped depending on it.
const MAC_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
const LINUX_PATH: &str =
    "/home/linuxbrew/.linuxbrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Launchd,
    Systemd,
}

/// Both generators are always compiled, on both platforms, so the unit a
/// Linux user gets is covered by the tests a macOS developer runs.
fn kind() -> Result<Kind> {
    if cfg!(target_os = "macos") {
        Ok(Kind::Launchd)
    } else if cfg!(target_os = "linux") {
        Ok(Kind::Systemd)
    } else {
        Err(
            fail!("ulak has no background service for this platform yet")
                .now("run it by hand when you need it: ulak service run --foreground")
                .into_err(),
        )
    }
}

// ─── where things live ──────────────────────────────────────────────

fn unit_path(kind: Kind) -> Result<PathBuf> {
    Ok(match kind {
        Kind::Launchd => launchd_unit_path(LABEL)?,
        Kind::Systemd => {
            let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
                Some(x) => PathBuf::from(x),
                None => config::home_dir()?.join(".config"),
            };
            base.join("systemd/user").join(UNIT)
        }
    })
}

fn launchd_unit_path(label: &str) -> Result<PathBuf> {
    Ok(config::home_dir()?
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist")))
}

fn service_dir() -> Result<PathBuf> {
    let dir = invocation::state_dir_required()?.join("service");
    invocation::private_dir(&dir)?;
    Ok(dir)
}

pub fn log_path() -> Result<PathBuf> {
    Ok(service_dir()?.join("service.log"))
}

fn beat_path() -> Option<PathBuf> {
    Some(invocation::state_dir()?.join("service/heartbeat.json"))
}

// ─── the file the system reads ──────────────────────────────────────

/// The launchd agent.
///
/// `RunAtLoad` covers the login; `KeepAlive` covers the crash — and the
/// dictionary form covers the switch: an exit 0 is ulak obeying
/// `[service] auto = false`, and launchd must let it stay dead.
/// That the pair really does bring the service back was measured once,
/// end to end on a real machine: boot to tunnels open in 62 s with
/// nothing typed, and the service reporting `runs = 1` — one start,
/// not a restart loop.
///
/// **`ProcessType` is `Standard`, and that is a reversal.** The plan said
/// `Background`, which reads as good manners for a sync daemon. Measured
/// here, on an IDLE disk, with the same 39 MB of content:
///
///   10 000 small files   normal 2.85 s   background 55.27 s   (19.4×)
///   8 large files        normal 0.18 s   background  0.99 s   ( 5.5×)
///
/// Darwin's background policy throttles per I/O OPERATION, not per byte,
/// and it is inherited by children — so it lands squarely on rsync
/// walking a tree of small files, which is the only shape ulak ever
/// has (a real workspace is tens of thousands). A service whose promise is
/// "your save is on the server in half a second" cannot run twenty times
/// slower to be polite. Contention does not explain it either: the idle
/// number is the worse one.
///
/// Linux needs no counterpart: the unit sets no `IOSchedulingClass`, so
/// systemd leaves the scheduler alone.
fn plist(exe: &Path, log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>service</string>
		<string>run</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ProcessType</key>
	<string>Standard</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>{path}</string>
	</dict>
	<key>WorkingDirectory</key>
	<string>/</string>
	<key>StandardOutPath</key>
	<string>/dev/null</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        label = LABEL,
        exe = xml(&exe.to_string_lossy()),
        path = MAC_PATH,
        log = xml(&log.to_string_lossy()),
    )
}

/// The systemd user unit.
///
/// `Restart=on-failure` is the same decision as the plist's dictionary.
/// `KillMode=control-group` is the one that matters on the way out: the
/// tunnels are children of this process, and stopping the service while
/// leaving local ports pointing at a server nobody maintains is exactly
/// the orphan class this phase exists to close.
fn systemd_unit(exe: &Path, log: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=ulak — keeps the stacks you left up in sync with your server\n\
         Documentation=https://github.com/syy/ulak\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} service run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         KillMode=control-group\n\
         TimeoutStopSec=5\n\
         Environment=PATH={path}\n\
         StandardOutput=null\n\
         StandardError=append:{log}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
        path = LINUX_PATH,
        log = log.display(),
    )
}

fn unit_text(kind: Kind, exe: &Path, log: &Path) -> String {
    match kind {
        Kind::Launchd => plist(exe, log),
        Kind::Systemd => systemd_unit(exe, log),
    }
}

/// A path may legally contain `&` or `<`; a plist that contains them raw
/// is not a plist, and launchd's answer to that is to run nothing at all
/// and say nothing about it.
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The program the installed unit actually points at.
///
/// Read back rather than remembered: the file on disk is the only thing
/// the system will act on, and `current_exe()` was baked into it whenever
/// it was last written (a `cargo install` into a new path, a moved
/// binary, a deleted checkout — all of them leave a unit that looks
/// installed and runs nothing).
fn unit_program(text: &str, kind: Kind) -> Option<PathBuf> {
    match kind {
        Kind::Launchd => {
            let array = text.split("<array>").nth(1)?;
            let first = array.split("<string>").nth(1)?;
            Some(PathBuf::from(first.split("</string>").next()?.trim()))
        }
        Kind::Systemd => {
            let line = text.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
            Some(PathBuf::from(line.split_whitespace().next()?))
        }
    }
}

// ─── install ────────────────────────────────────────────────────────

/// Write the unit and (unless told otherwise) start it now.
///
/// Idempotent on purpose, and that is also the answer to a version
/// mismatch: re-running it rewrites the unit with the binary you are
/// holding and restarts the service — which is a decision the USER makes,
/// by typing this, rather than one ulak makes behind their back.
///
/// What installing cannot fix, on macOS: a launchd USER AGENT runs
/// outside the TCC grant a terminal has, so a project living under
/// ~/Desktop, ~/Documents or iCloud Drive is read with EPERM — no
/// prompt, no dialog. The service then fails silently on exactly the
/// projects where the same command, typed in a terminal, succeeds.
pub fn install(start: bool) -> Result<ExitCode> {
    let kind = kind()?;
    let path = unit_path(kind)?;
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| {
            fail!("cannot find the ulak binary that is running ({e})")
                .now("reinstall ulak, then retry: ulak service install")
                .into_err()
        })?;
    let log = log_path()?;
    rotate_log(&log);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            fail!("cannot create {} ({e})", parent.display())
                .now("check the permissions on your home directory and retry")
                .into_err()
        })?;
    }
    let text = unit_text(kind, &exe, &log);
    write_private(&path, text.as_bytes()).map_err(|e| {
        fail!("cannot write {} ({e})", path.display())
            .now("check the permissions on that directory and retry")
            .into_err()
    })?;

    ui::ok(&format!("service agent written: {}", path.display()));
    ui::dim(&format!("it runs: {} service run", exe.display()));
    if !config::machine_config().service.auto {
        // Installing is an explicit request, so it is honoured — but an
        // agent that starts and exits within the second, forever, is
        // worth a word before the user goes looking for what it did.
        ui::warn(
            "[service] auto = false, so this agent will start, do nothing and exit — which is exactly what that switch means",
        );
    }

    let mut started_now = true;
    if start {
        match load(kind, &path) {
            Ok(()) => {
                ui::ok("the service is running, and it will come back by itself after a reboot")
            }
            Err(e) => {
                // The unit is written and valid; the system just would
                // not take it right now. Deleting it would make the next
                // command try again and fail again — forever, since this
                // also runs automatically — and leaving it in place means
                // the login after the next reboot picks it up anyway.
                started_now = false;
                ui::render_error(&e);
                ui::warn("the agent is in place and will start at your next login");
            }
        }
    } else {
        ui::info("not started now — it will start at your next login");
    }
    if kind == Kind::Systemd {
        // Without it a user unit dies at logout and does not return until
        // the next login — which is most of "survives a reboot" gone.
        ui::dim("so it survives a logout too, run once: loginctl enable-linger $USER");
    }
    ui::dim(&format!("what it says goes to {}", log.display()));
    ui::dim(
        "remove the service: ulak service uninstall (also set [service] auto = false in the global config to keep it off)",
    );
    Ok(if started_now {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Hand the unit to the system.
///
/// `bootout` first because `bootstrap` refuses a label that is already
/// loaded, and re-installing must be the way a user restarts the service.
fn load(kind: Kind, path: &Path) -> Result<()> {
    match kind {
        Kind::Launchd => {
            let target = format!("gui/{}", uid());
            let label = format!("{target}/{LABEL}");
            // `bootout` is ASYNCHRONOUS: it returns while launchd is
            // still tearing the old job down, and bootstrapping the same
            // label into a domain that has not let go of it yet fails
            // with `Bootstrap failed: 5: Input/output error`. Measured in
            // the field, on the very first upgrade anyone performed — and
            // it fails the worst possible way, because the old service is
            // already stopped by then. So the label is waited out, and
            // the bootstrap is tried once more if it still loses.
            run_tool("launchctl", &["bootout", &label]);
            let _ = wait_until_gone(&label);
            let mut out = run_tool(
                "launchctl",
                &["bootstrap", &target, &path.to_string_lossy()],
            );
            if !out.ok {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let _ = wait_until_gone(&label);
                out = run_tool(
                    "launchctl",
                    &["bootstrap", &target, &path.to_string_lossy()],
                );
            }
            if !out.ok {
                return Err(fail!("launchctl refused the agent: {}", out.say)
                    .now(format!("check the file: plutil -lint {}", path.display()))
                    .now("then retry: ulak service install")
                    .into_err());
            }
        }
        Kind::Systemd => {
            run_tool("systemctl", &["--user", "daemon-reload"]);
            // `enable --now` leaves an already-running unit alone, so a
            // re-install after an upgrade kept the OLD binary running.
            // `restart` starts a stopped unit and replaces a running one.
            for args in [["--user", "enable", UNIT], ["--user", "restart", UNIT]] {
                let out = run_tool("systemctl", &args);
                if !out.ok {
                    return Err(fail!("systemctl refused the unit: {}", out.say)
                        .now(format!(
                            "check the file: systemd-analyze --user verify {}",
                            path.display()
                        ))
                        .now("then retry: ulak service install")
                        .into_err());
                }
            }
        }
    }
    Ok(())
}

/// How long to let launchd finish letting go of a label. Generous, and
/// it costs nothing in the normal case: the first probe usually already
/// says the job is gone.
const BOOTOUT_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Block until `launchctl print` can no longer find the job.
///
/// Not audited: this is a question, and a poll loop would bury the two
/// argv that actually change something under fifty that do not.
fn wait_until_gone(label: &str) -> bool {
    let deadline = std::time::Instant::now() + BOOTOUT_GRACE;
    loop {
        let mut cmd = Command::new("launchctl");
        cmd.args(["print", label]);
        let loaded = proc::run_bounded(&mut cmd, None, proc::PROBE)
            .map(|b| b.status.success() && !b.timed_out)
            .unwrap_or(false);
        if !loaded {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

// ─── uninstall ──────────────────────────────────────────────────────

/// One command, and afterwards there is no agent, no process and no
/// tunnel.
///
/// Deleting the file alone would leave the user with a service they
/// believe is gone and which keeps syncing until the next reboot — and
/// with local ports still pointing at a server nobody maintains, because
/// launchd's promise on the way out is five seconds and then SIGKILL,
/// which no `Drop` survives.
pub fn uninstall() -> Result<ExitCode> {
    let kind = kind()?;
    let path = unit_path(kind)?;
    let mut existed = path.exists();

    match kind {
        Kind::Launchd => {
            let target = format!("gui/{}", uid());
            if path.exists() {
                existed = true;
                let loaded = format!("{target}/{LABEL}");
                run_tool("launchctl", &["bootout", &loaded]);
                let _ = wait_until_gone(&loaded);
                std::fs::remove_file(&path).map_err(|e| {
                    fail!("cannot remove service agent {} ({e})", path.display())
                        .now("check its permissions and retry")
                        .into_err()
                })?;
                ui::ok(&format!("agent removed: {}", path.display()));
            }
        }
        Kind::Systemd if existed => {
            run_tool("systemctl", &["--user", "disable", "--now", UNIT]);
            std::fs::remove_file(&path).map_err(|e| {
                fail!("cannot remove service agent {} ({e})", path.display())
                    .now("check its permissions and retry")
                    .into_err()
            })?;
            ui::ok(&format!("agent removed: {}", path.display()));
        }
        Kind::Systemd => {}
    }
    if !existed {
        ui::info("no service agent was installed on this machine");
    }

    // The service may already be gone; give it the system's five seconds
    // plus a second of margin before deciding what is left behind is an
    // orphan.
    let stopped = wait_gone(std::time::Duration::from_secs(6));
    let swept = crate::forward::sweep_orphans();
    if swept > 0 {
        ui::ok(&format!("closed {swept} port tunnel(s) it had open"));
    }
    if let Some(beat) = read_beat().filter(|_| !stopped) {
        // A `--foreground` service is somebody's terminal, not ours to
        // kill: say where it is instead of pretending it is gone.
        ui::warn(&format!(
            "a ulak service (pid {}) is still running — if you started it by hand, stop it with Ctrl-C in that terminal",
            beat.pid
        ));
    } else if let Some(p) = beat_path() {
        let _ = std::fs::remove_file(p);
    }
    if existed {
        // Otherwise this command is a trapdoor with a spring: `ensure`
        // puts the agent back on the next command typed in a terminal,
        // and the user is left believing they removed something.
        ui::dim(&format!(
            "the next ulak command in a terminal installs it again — to keep it off, put [service] auto = false in {}",
            config::global_config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "your global config".into())
        ));
        ui::dim("or bring it back yourself any time: ulak service install");
    }
    Ok(ExitCode::SUCCESS)
}

fn wait_gone(limit: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        match read_beat() {
            None => return true,
            Some(beat) if !is_ulak(beat.pid) => return true,
            Some(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

// ─── installed by being installed ───────────────────────────────────

/// ulak is on this machine, so the service is on this machine.
///
/// The maintainer's call, and it is not tied to `init` or to `up -d`:
/// a service that looks after every stack is a property of the MACHINE,
/// and hanging it off a project command would mean a user who upgraded
/// (and therefore never runs `init` again) silently never gets one.
/// `cargo install` and Homebrew have no post-install hook, so the first
/// run of the binary is the moment we have.
///
/// Gated on a terminal, which is not a trick: this writes a file into the
/// user's login items and starts a background process, and doing that
/// where nobody can read the sentence explaining it would be a surprise.
/// It also means a test, a CI job and a script cannot reach `launchctl`
/// through ulak by construction, rather than by convention.
pub fn ensure() {
    if !std::io::stderr().is_terminal() {
        return;
    }
    if !config::machine_config().service.auto {
        return;
    }
    let Ok(kind) = kind() else { return };
    let Ok(path) = unit_path(kind) else { return };

    match std::fs::read_to_string(&path) {
        // Trap 1: the unit names an absolute path, and a moved or
        // reinstalled binary leaves an agent that looks installed and
        // runs nothing at all. Only the path VANISHING is worth a word —
        // a second ulak in a checkout is normal and must not nag.
        Ok(text) => {
            if let Some(program) = unit_program(&text, kind)
                && !program.exists()
            {
                ui::warn(&format!(
                    "the background service points at {}, which is not there any more — it has not been running",
                    program.display()
                ));
                ui::dim("point it at this ulak: ulak service install");
            }
        }
        Err(_) => {
            ui::info("setting up the ulak background service on this machine (once)");
            // A failed START is not a failed install and never deletes
            // the unit — `install` says so itself. Only a unit that could
            // not be WRITTEN lands here, and then there is nothing to
            // undo; what the user needs is the reason and the off switch.
            if let Err(e) = install(true) {
                ui::render_error(&e);
                ui::dim(&format!(
                    "ulak works without it; the stacks you leave up just will not be looked after. Silence this with [service] auto = false in {}",
                    config::global_config_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "your global config".into())
                ));
            }
        }
    }
}

// ─── the heartbeat ──────────────────────────────────────────────────

/// What the running service says about itself. One file for the whole
/// machine, because "is the service running?" is not a per-workspace
/// question and `status.json` cannot answer it: a Docker stack that is
/// down is deliberately silent, which looks exactly like a dead service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Beat {
    pub schema: u32,
    pub pid: u32,
    pub version: String,
    pub exe: String,
    /// Bumped by every rebuild, so a developer's `cargo install` is
    /// caught even when the version string did not move.
    #[serde(default)]
    pub exe_mtime_unix: u64,
    #[serde(default)]
    pub started_unix: u64,
    #[serde(default)]
    pub updated_unix: u64,
}

/// Written by the service and by nobody else — the same one-writer rule
/// the intent files use, so there is no lock here either.
pub fn beat(started_unix: u64) {
    let Some(path) = beat_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = invocation::private_dir(parent);
    }
    let exe = std::env::current_exe().unwrap_or_default();
    let value = Beat {
        schema: SCHEMA,
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        exe: exe.to_string_lossy().into_owned(),
        exe_mtime_unix: mtime_of(&exe),
        started_unix,
        updated_unix: crate::intent::now_unix(),
    };
    if let Ok(json) = serde_json::to_vec_pretty(&value) {
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        if write_private(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

pub fn read_beat() -> Option<Beat> {
    let bytes = std::fs::read(beat_path()?).ok()?;
    let beat: Beat = serde_json::from_slice(&bytes).ok()?;
    (beat.schema <= SCHEMA).then_some(beat)
}

fn mtime_of(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the heartbeat plus the process table say together.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Alive {
    /// Nothing is running: no heartbeat, or the pid belongs to something
    /// else entirely.
    No,
    /// The pid is ours and it has stopped writing. This is the state
    /// launchd cannot see — running, and wedged — and the reason the
    /// service's main thread is forbidden from touching the network.
    Wedged,
    Yes,
}

/// Pure, so the interesting case can be tested without a wedged process.
fn liveness(beat: &Beat, now: u64, pid_is_ours: bool) -> Alive {
    if !pid_is_ours {
        return Alive::No;
    }
    if now.saturating_sub(beat.updated_unix) > STALE {
        return Alive::Wedged;
    }
    Alive::Yes
}

/// Does this pid still belong to a ulak? Asked with `ps` rather than
/// `kill -0` for the same reason the tunnel ledger does: pids are
/// recycled, and a stale file must never become a verdict about somebody
/// else's process.
fn is_ulak(pid: u32) -> bool {
    let mut cmd = Command::new("ps");
    cmd.args(["-o", "command=", "-p", &pid.to_string()]);
    proc::run_bounded(&mut cmd, None, proc::PROBE)
        .map(|out| String::from_utf8_lossy(&out.stdout).contains("ulak"))
        .unwrap_or(false)
}

// ─── what doctor prints ─────────────────────────────────────────────

/// One line about the service, plus the problem behind it when there is
/// one. Both halves are needed: doctor's rows are a glance, and its
/// problem list is what carries the "now do this".
pub struct Health {
    pub line: String,
    pub problem: Option<String>,
}

pub fn health() -> Health {
    let auto = config::machine_config().service.auto;
    let Ok(kind) = kind() else {
        return Health {
            line: "not available on this platform".into(),
            problem: None,
        };
    };
    let text = unit_path(kind)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok());
    let installed = text.is_some();
    // The absolute path was baked in when the unit was last written, so a
    // `cargo install` into a new prefix, a moved binary or a deleted
    // checkout all leave something that looks installed and runs nothing.
    // It is the likeliest way this whole phase fails quietly, so it gets
    // its own sentence rather than a generic "not running".
    let vanished = text
        .as_deref()
        .and_then(|t| unit_program(t, kind))
        .filter(|p| !p.exists());
    let beat = read_beat();
    let alive = beat
        .as_ref()
        .map(|b| liveness(b, crate::intent::now_unix(), is_ulak(b.pid)))
        .unwrap_or(Alive::No);

    if !auto {
        return Health {
            line: format!(
                "off ([service] auto = false){}",
                if alive == Alive::Yes {
                    ", but one is running by hand"
                } else {
                    ""
                }
            ),
            problem: None,
        };
    }
    // "Not installed" is only a PROBLEM where ulak would have
    // installed it. In a script, a CI job or a test there is deliberately
    // no agent (see `ensure`), and calling ulak's own policy a fault
    // would make every pipeline's doctor red for doing the right thing.
    // In a terminal the reading is the opposite and exact: `ensure` ran
    // before this command's body, so an absent agent means the install
    // actually failed.
    let here = std::io::stderr().is_terminal();
    match (installed, alive) {
        (false, Alive::Yes) => Health {
            line: "running by hand (not installed)".into(),
            problem: here.then(|| {
                "nothing will look after your stacks once that terminal closes — now: ulak service install".into()
            }),
        },
        (false, _) => Health {
            line: if here {
                "not installed".into()
            } else {
                "not installed (ulak only installs it from a terminal)".into()
            },
            problem: here.then(|| {
                "no background service on this machine: the stacks you leave up are not synced and their ports are not forwarded — now: ulak service install".into()
            }),
        },
        (true, Alive::No) => match vanished {
            Some(gone) => Health {
                line: format!("installed, but points at {} — gone", gone.display()),
                problem: Some(format!(
                    "the service agent still names {}, which is not there any more, so it has been running nothing — now: ulak service install   (points it at this ulak)",
                    gone.display()
                )),
            },
            None => Health {
                line: "installed, NOT running".into(),
                problem: Some(format!(
                    "the service agent is installed but nothing is running — now: ulak service install   (and read {})",
                    log_path().map(|p| p.display().to_string()).unwrap_or_default()
                )),
            },
        },
        (true, Alive::Wedged) => Health {
            line: format!(
                "installed, running but silent for {}s",
                beat.as_ref()
                    .map(|b| crate::intent::now_unix().saturating_sub(b.updated_unix))
                    .unwrap_or(0)
            ),
            problem: Some(format!(
                "the service process is alive but has stopped reporting — now: ulak service install   (restarts it; then read {})",
                log_path().map(|p| p.display().to_string()).unwrap_or_default()
            )),
        },
        (true, Alive::Yes) => {
            let beat = beat.expect("Alive::Yes needs a heartbeat");
            let mut line = format!("running (pid {}, v{})", beat.pid, beat.version);
            let mut problem = None;
            if let Some(drift) = drift(&beat) {
                line.push_str("  ← older than this ulak");
                problem = Some(drift);
            }
            let size = log_path().map(|p| size_of(&p)).unwrap_or(0);
            if size > LOG_CAP {
                problem.get_or_insert_with(|| {
                    format!(
                        "the service log has grown to {} MB — now: ulak service install   (rotates it and restarts)",
                        size / (1024 * 1024)
                    )
                });
            }
            Health { line, problem }
        }
    }
}

// ─── the service's voice, where you already are ─────────────────────

/// Subcommands that are already fixing the one complaint about a stopped
/// stack. Repeating "the stack is not running" at somebody who is
/// starting it is the only note in the set that is pure noise.
const STARTERS: &[&str] = &["up", "start", "restart", "create"];

/// The one sentence about a dead service, in one place: two call paths
/// say it (`stack_complaint` before a command, `stack_complaint_after`
/// once a declaration held), and two copies would drift the way
/// `service::STACK_DOWN`'s doc already paid to learn.
const NO_SERVICE: &str = "you left this stack up, but no ulak service is running — nothing is syncing and no ports are forwarded. Start it: ulak service install";

/// What the machine-level gates say about the service, before any
/// per-stack question is worth asking.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ServiceWord {
    /// `[service] auto = false`: silence, on purpose — about the
    /// heartbeat AND about everything the service last reported.
    Off,
    /// No fresh heartbeat. A stale one counts: a status.json behind a
    /// dead service is a memory, not a report.
    Silent,
    Beating,
}

/// Pure, with the config and the clock handed in: `machine_config()` is
/// not sandboxed under `cfg!(test)` the way `state_dir()` is, so a test
/// calling the reading half would answer differently per developer
/// machine. This half is where the decision lives and gets pinned.
fn service_word(auto: bool, beat: Option<&Beat>, now: u64) -> ServiceWord {
    if !auto {
        return ServiceWord::Off;
    }
    let beating = beat.is_some_and(|b| now.saturating_sub(b.updated_unix) <= STALE);
    if beating {
        ServiceWord::Beating
    } else {
        ServiceWord::Silent
    }
}

fn machine_service_word() -> ServiceWord {
    service_word(
        config::machine_config().service.auto,
        read_beat().as_ref(),
        crate::intent::now_unix(),
    )
}

/// What the service would tell you about this workspace, if you were
/// looking — and you are not, because you typed `up` or `logs`.
///
/// Measured on this codebase: `status.json` had exactly ONE reader,
/// `ulak status`. A service that had been unable to reach the server
/// for hours therefore said nothing to somebody running `up -d`, `logs`
/// and `ps` all day; the file was written and nobody read it. That is
/// the whole gap a desktop notification was going to fill, and it does
/// not need one — the user is already in a terminal, typing at us.
///
/// Silent by design when there is nothing to maintain (no live intent),
/// when the service is switched off on purpose, and when everything is
/// simply fine. On an ordinary day this returns `None` every time.
///
/// The first gate reads the declaration as it stood BEFORE the command,
/// which is exactly what makes `stack_complaint_after` necessary — see
/// its doc for the fresh-`up` hole this half structurally cannot see.
pub fn stack_complaint(stack_id: &str, doing: &str) -> Option<String> {
    // Nothing was left up, so nothing is owed. A user who has never run
    // `up -d` is not missing a service.
    if !crate::intent::read_desired(stack_id)?.live {
        return None;
    }
    match machine_service_word() {
        ServiceWord::Off => return None,
        ServiceWord::Silent => return Some(NO_SERVICE.into()),
        ServiceWord::Beating => {}
    }

    let status = crate::intent::read_status(stack_id)?;
    if status.connection != "up" {
        let since = crate::intent::now_unix().saturating_sub(status.since_unix);
        return Some(format!(
            "the service has not been able to reach the server for {} — your saves are not going anywhere. What it says: {}",
            span(since),
            log_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ));
    }
    let note = status.note?;
    if already_answering(doing, &note) {
        return None;
    }
    Some(note)
}

/// The heartbeat half of `stack_complaint`, asked again AFTER the
/// command — because the command is what creates the expectation.
///
/// Measured against a real server: `stack_complaint` runs before the
/// command and its first gate reads the declaration as it stood THEN.
/// On a fresh workspace there is none, and after a `down` it says
/// `live: false` — while the value that would open the gate is written
/// by the very `up` being typed, some twenty lines later in
/// `passthrough`. So the warning fired on `ps`, on `logs`, on a
/// re-typed `up` — on every command EXCEPT the one that made the user
/// open a browser onto a dead port. Every fresh start was silent.
///
/// Reading the declaration again, after `IntentGuard::settle`, is what
/// keeps this to one rule and no new ones: a failed `up` was already
/// rolled back by `declaration_holds`, so this stays silent over a
/// failure's own error; `[service] auto = false` is the same first gate
/// as always; and a half-successful `up` keeps `live`, so the warning
/// rightly survives. Only the heartbeat is consulted — the stack's
/// status.json cannot say anything yet about a stack that just changed.
///
/// The caller suppresses this when the BEFORE half already spoke, so
/// one command never carries the sentence twice — the unconditional
/// before-and-after double check is the crying-wolf shape this file
/// deliberately avoids.
pub fn stack_complaint_after(stack_id: &str) -> Option<String> {
    if !crate::intent::read_desired(stack_id)?.live {
        return None;
    }
    (machine_service_word() == ServiceWord::Silent).then(|| NO_SERVICE.into())
}

/// Whether the note the service left is the one this command is already
/// the answer to.
///
/// Split out from `stack_complaint` for the reason `docker.rs`
/// splits `asked_for_a_tty`: everything above it reads this machine's
/// state directory and its config, so a test that called the whole
/// function would be exercising the seeding, and the one decision worth
/// pinning would go untested. It did — the test asserted
/// `STARTERS.contains(doing)` for a `doing` bound by `for doing in
/// STARTERS`, against a note it had built out of `STACK_DOWN` itself,
/// so both halves read `true` by construction. Measured: this gate
/// could be deleted outright and all 425 tests stayed green.
///
/// What the test on this pins, measured one mutation at a time: the
/// condition inverted, the `starts_with` half dropped, `create` taken
/// out of `STARTERS`, and `logs` put into it — all four now fail. What
/// it does NOT pin is the CALL above: delete that line and the suite
/// stays green. Closing that would mean calling `stack_complaint`
/// itself, and it reads `machine_config()`, which — unlike `state_dir`
/// — is not sandboxed under `cfg!(test)` and would read the developer's
/// own `[service] auto`. A test that changes answer per machine is
/// worse than this gap, so the gap stays and says so.
fn already_answering(doing: &str, note: &str) -> bool {
    STARTERS.contains(&doing) && note.starts_with(crate::service::STACK_DOWN)
}

fn span(secs: u64) -> String {
    match secs {
        s if s < 90 => format!("{s}s"),
        s if s < 5400 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

/// Is the running service a different build from the one being typed?
///
/// Deliberately never acted on. Restarting a service in the middle of
/// somebody's `up` is worse than the mismatch it fixes, so this ends as a
/// sentence and the user decides when.
fn drift(beat: &Beat) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let mine = env!("CARGO_PKG_VERSION");
    if beat.version != mine {
        return Some(format!(
            "the running service is v{} and this ulak is v{mine} — nothing restarts by itself (that would cut somebody's `up` in half). When nothing is mid-flight: ulak service install",
            beat.version
        ));
    }
    // Same version string, same path, different build: the developer's
    // daily case and every `cargo install` that did not bump a number.
    if beat.exe == exe.to_string_lossy() && beat.exe_mtime_unix != mtime_of(&exe) {
        return Some(format!(
            "the service is running an older build of {} — nothing restarts by itself. When nothing is mid-flight: ulak service install",
            exe.display()
        ));
    }
    None
}

fn size_of(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// The only moment the log can be rotated safely: before the system
/// opens it. While the service runs, that fd belongs to launchd or
/// systemd and a rename would carry our own stderr away with it.
fn rotate_log(log: &Path) {
    if size_of(log) <= LOG_CAP {
        return;
    }
    let old = log.with_extension("log.1");
    if std::fs::rename(log, &old).is_ok() {
        ui::dim(&format!(
            "the previous service log had grown past {} MB — kept as {}",
            LOG_CAP / (1024 * 1024),
            old.display()
        ));
    }
}

// ─── subprocesses ───────────────────────────────────────────────────

struct Ran {
    ok: bool,
    say: String,
}

/// Every privileged step is a visible argv, recorded like the ssh and
/// rsync ones — into the SERVICE's own trail, never the caller's, because
/// this also runs inside the ordinary command that triggers the one-time
/// install. A failure is never fatal here on its own: `bootout` on a
/// label that is not loaded is a normal answer, not an error.
fn run_tool(program: &str, args: &[&str]) -> Ran {
    let mut cmd = Command::new(program);
    cmd.args(args);
    let out = proc::run_bounded(&mut cmd, None, proc::PROBE);
    let code = out.as_ref().ok().and_then(|b| b.status.code());
    crate::audit::record_command_in(AUDIT, "system", &cmd, code);
    match out {
        Ok(b) => Ran {
            ok: b.status.success() && !b.timed_out,
            say: String::from_utf8_lossy(&b.stderr).trim().to_string(),
        },
        Err(e) => Ran {
            ok: false,
            say: format!("{program} could not be run ({e})"),
        },
    }
}

/// `id -u`, because a uid without libc is a subprocess — and `gui/<uid>`
/// is the only domain a LaunchAgent belongs to.
fn uid() -> String {
    let mut cmd = Command::new("id");
    cmd.arg("-u");
    proc::run_bounded(&mut cmd, None, proc::PROBE)
        .ok()
        .map(|b| String::from_utf8_lossy(&b.stdout).trim().to_string())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or_else(|| "501".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(kind: Kind) -> String {
        unit_text(
            kind,
            Path::new("/Users/x/.cargo/bin/ulak"),
            Path::new("/Users/x/.local/state/ulak/service/service.log"),
        )
    }

    #[test]
    fn the_agent_runs_the_service_without_foreground() {
        // The whole switch depends on this word being absent. With
        // `--foreground` the service ignores `[service] auto = false`
        // (that flag means "I am asking for it by hand"), so the machine
        // switch would silently do nothing.
        for kind in [Kind::Launchd, Kind::Systemd] {
            let text = sample(kind);
            assert!(text.contains("service"), "{text}");
            assert!(
                !text.contains("--foreground"),
                "the installed unit must never pass --foreground:\n{text}"
            );
        }
    }

    #[test]
    fn a_clean_exit_is_never_revived() {
        // Measured: launchd enforces a 10-second minimum runtime, so a
        // plain `KeepAlive = true` against a service that exits 0 on
        // `auto = false` is ~8 640 spawns a day. The dictionary form is
        // what makes the config switch real.
        let text = sample(Kind::Launchd);
        assert!(
            text.contains(
                "<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>"
            ),
            "KeepAlive must be the dictionary, not <true/>:\n{text}"
        );
        assert!(!text.contains("<key>KeepAlive</key>\n\t<true/>"));
        assert!(text.contains("<key>RunAtLoad</key>\n\t<true/>"), "{text}");

        let unit = sample(Kind::Systemd);
        assert!(
            unit.contains("Restart=on-failure"),
            "Restart=always would revive a service told not to run:\n{unit}"
        );
        assert!(!unit.contains("Restart=always"));
    }

    #[test]
    fn the_agent_is_never_shipped_at_background_io_priority() {
        // Measured, idle disk, same 39 MB: 10 000 small files took 2.85 s
        // normally and 55.27 s under Darwin's background policy — 19.4×,
        // because the throttle is per I/O OPERATION and children inherit
        // it. rsync walking a tree of small files is the only shape
        // ulak has. `Background` reads as good manners and is in fact
        // the single most expensive line this file could carry.
        let text = sample(Kind::Launchd);
        assert!(
            !text.contains("<string>Background</string>"),
            "an agent at background I/O priority is ~20x slower at exactly ulak's workload:\n{text}"
        );
        assert!(
            text.contains("<key>ProcessType</key>\n\t<string>Standard</string>"),
            "{text}"
        );
        // The Linux side must not grow the equivalent by accident.
        let unit = sample(Kind::Systemd);
        assert!(!unit.contains("IOSchedulingClass"), "{unit}");
        assert!(!unit.contains("Nice="), "{unit}");
    }

    #[test]
    fn the_unit_takes_the_tunnels_with_it_and_does_not_wait_forever() {
        let unit = sample(Kind::Systemd);
        // The tunnels are children of the service; stopping only the
        // parent leaves local ports pointing at an unmaintained server.
        assert!(unit.contains("KillMode=control-group"), "{unit}");
        assert!(unit.contains("TimeoutStopSec=5"), "{unit}");
        assert!(unit.contains("WantedBy=default.target"), "{unit}");
    }

    #[test]
    fn both_units_name_a_path_and_a_log() {
        for kind in [Kind::Launchd, Kind::Systemd] {
            let text = sample(kind);
            assert!(text.contains("/Users/x/.cargo/bin/ulak"), "{text}");
            assert!(text.contains("service/service.log"), "{text}");
            assert!(
                text.contains("/usr/bin"),
                "an explicit PATH is hygiene:\n{text}"
            );
        }
    }

    #[test]
    fn a_path_that_needs_escaping_stays_a_valid_plist() {
        // A directory called `A & B` is legal and would otherwise produce
        // a plist launchd refuses — silently, running nothing at all.
        let text = plist(Path::new("/Users/A & B/ulak"), Path::new("/tmp/l<og"));
        assert!(text.contains("/Users/A &amp; B/ulak"), "{text}");
        assert!(text.contains("/tmp/l&lt;og"), "{text}");
        assert!(!text.contains("A & B/ulak"));
    }

    /// The one check that proves launchd will accept what we wrote.
    /// `plutil` only reads; nothing here loads, starts or touches the
    /// user's login items.
    #[test]
    #[cfg(target_os = "macos")]
    fn the_plist_is_a_plist_launchd_can_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.plist");
        std::fs::write(
            &path,
            plist(Path::new("/Users/A & B/ulak"), Path::new("/tmp/l.log")),
        )
        .unwrap();
        let out = Command::new("plutil").arg("-lint").arg(&path).output();
        let Ok(out) = out else {
            return; // no plutil: nothing to prove, nothing to fail
        };
        assert!(
            out.status.success(),
            "plutil rejected the agent: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn the_program_can_be_read_back_out_of_both_units() {
        // How a moved binary is caught: the file on disk is the only
        // thing the system will act on, so the answer has to come from
        // the file and not from what we think we wrote.
        for kind in [Kind::Launchd, Kind::Systemd] {
            assert_eq!(
                unit_program(&sample(kind), kind),
                Some(PathBuf::from("/Users/x/.cargo/bin/ulak")),
                "{kind:?}"
            );
        }
        assert_eq!(unit_program("nothing useful", Kind::Systemd), None);
        assert_eq!(unit_program("<plist></plist>", Kind::Launchd), None);
    }

    fn a_beat() -> Beat {
        Beat {
            schema: SCHEMA,
            pid: 4242,
            version: env!("CARGO_PKG_VERSION").to_string(),
            exe: "/opt/ulak".into(),
            exe_mtime_unix: 100,
            started_unix: 1000,
            updated_unix: 1000,
        }
    }

    #[test]
    fn a_live_pid_that_stopped_writing_is_the_state_launchd_cannot_see() {
        let beat = a_beat();
        // Fresh.
        assert_eq!(liveness(&beat, 1005, true), Alive::Yes);
        // Alive, silent: "running" to launchd, broken to the user. This
        // is the class the service's main thread is designed to make
        // impossible, and the heartbeat is how we would ever find out it
        // happened anyway.
        assert_eq!(liveness(&beat, 1000 + STALE + 1, true), Alive::Wedged);
        // The pid belongs to somebody else now: not our process at all.
        assert_eq!(liveness(&beat, 1005, false), Alive::No);
    }

    #[test]
    fn a_service_from_another_build_is_named_but_never_restarted() {
        let mut beat = a_beat();
        beat.version = "0.0.1-old".into();
        let said = drift(&beat).expect("an older service must be reported");
        assert!(said.contains("0.0.1-old"), "{said}");
        assert!(
            said.contains("ulak service install"),
            "the user needs the command that fixes it: {said}"
        );
        // Same version, and the recorded mtime is not this binary's:
        // a second checkout, not a stale service.
        let mut other = a_beat();
        other.exe = "/somewhere/else/ulak".into();
        assert!(drift(&other).is_none());
    }

    /// The gates behind the "no service is running" sentence, pinned on
    /// the pure half because the reading half goes through
    /// `machine_config()`, which is not sandboxed under `cfg!(test)` and
    /// would make the test answer differently per developer machine.
    /// The regression this guards: the sentence must key on the
    /// HEARTBEAT alone (a status.json behind a dead service is a
    /// memory), and `auto = false` must silence it entirely — those two
    /// gates are shared by the before- and after-command checks, so one
    /// mistake here would fire or silence both.
    #[test]
    fn the_no_service_sentence_keys_on_the_heartbeat_and_the_switch_alone() {
        let beat = a_beat(); // updated_unix: 1000
        assert_eq!(
            service_word(true, Some(&beat), 1000 + STALE),
            ServiceWord::Beating
        );
        assert_eq!(
            service_word(true, Some(&beat), 1000 + STALE + 1),
            ServiceWord::Silent,
            "a stale heartbeat is a dead service, not a busy one"
        );
        assert_eq!(
            service_word(true, None, 1000),
            ServiceWord::Silent,
            "a machine that never started a service looks exactly like this"
        );
        assert_eq!(
            service_word(false, None, 1000),
            ServiceWord::Off,
            "auto = false is silence on purpose, never a complaint"
        );
    }

    #[test]
    fn the_one_complaint_a_starting_stack_must_not_hear() {
        // Every other note is worth interrupting for. This one is the
        // answer to a question the user is already answering: telling
        // somebody typing `up` that the stack is not up is noise, and a
        // warning that cries wolf is how the useful ones stop being read.
        let note = format!(
            "{} my-server — nothing is synced or tunneled until: ulak docker compose up -d",
            crate::service::STACK_DOWN
        );
        // Written out rather than looped over `STARTERS`, which is the
        // whole difference between this and a test that cannot fail:
        // `for doing in STARTERS` asks the table about its own
        // elements, so dropping `create` from it shortened the loop
        // instead of failing it. These four are the specification;
        // `STARTERS` is one implementation of it.
        for doing in ["up", "start", "restart", "create"] {
            assert!(
                already_answering(doing, &note),
                "{doing} is starting the stack, so the note must be suppressed"
            );
        }
        // …but the same note IS worth hearing when you are doing
        // something that assumes the stack is already up.
        for doing in ["logs", "exec", "ps", "sync"] {
            assert!(
                !already_answering(doing, &note),
                "{doing} does not start anything, so the note must survive"
            );
        }
        // The other half of the gate. Suppressing on the subcommand
        // alone would swallow every note a starting stack can get,
        // including the ones it most needs to hear.
        assert!(
            !already_answering(
                "up",
                "the service has not been able to reach the server for 4m"
            ),
            "only the stack-is-down note is noise to somebody starting the stack"
        );
    }

    #[test]
    fn how_long_is_said_in_units_a_person_reads() {
        assert_eq!(span(12), "12s");
        assert_eq!(span(600), "10m");
        assert_eq!(span(7200), "2h");
        // The boundaries: seconds stop being useful long before 90, and
        // minutes long before three figures.
        assert_eq!(span(90), "1m");
        assert_eq!(span(5400), "1h");
    }

    #[test]
    fn a_heartbeat_survives_the_round_trip() {
        let json = serde_json::to_vec(&a_beat()).unwrap();
        let back: Beat = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.pid, 4242);
        assert_eq!(back.schema, SCHEMA);
        // A field a newer ulak adds must not break this reader.
        let extra = br#"{"schema":1,"pid":7,"version":"9.9","exe":"/x","invented_later":true}"#;
        let back: Beat = serde_json::from_slice(extra).unwrap();
        assert_eq!(back.pid, 7);
        assert_eq!(back.updated_unix, 0);
    }
}
