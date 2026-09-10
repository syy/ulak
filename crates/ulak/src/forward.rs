//! The tunnel engine: the stack's published ports, brought home over an
//! ssh connection THIS PROCESS OWNS.
//!
//! There is no `ulak forward` command any more, and the reason is the
//! measurement that killed it. `ssh -N -L…` over the shared master hands
//! the forwards to `ControlPersist` and exits 0 in about a second —
//! ports were measured surviving 61 minutes with no ulak process
//! alive. Nobody decided to create them, nobody could kill them, nobody
//! noticed when they died. A command whose whole output was a promise it
//! did not keep is worse than no command.
//!
//! So the tunnels move onto `-S none`: a real child, killed on drop.
//! Three things fall out of that one change.
//!
//! 1. **`ssh -O exit` becomes a tool.** The control path is keyed on the
//!    destination alone, so one master carries every project on that
//!    server plus every rsync in flight. With the tunnels off it, the
//!    service can retire a suspect master without taking the ports down.
//! 2. **An exit means failure, and only failure.** With no master to
//!    hand off to, a healthy tunnel child never exits — which is why
//!    `dev`'s babysitter used to announce "the port tunnels stopped" two
//!    seconds into every successful start. That ambiguity is gone.
//! 3. **A busy local port is named, not skipped.** Without
//!    `ExitOnForwardFailure=yes` ssh SKIPS a forward it cannot bind and
//!    still exits 0 — after ulak has already printed
//!    "localhost:5432 → server:5432". Your app then talks to the LOCAL
//!    postgres while you believe you are on the server's: the exact
//!    failure class this product exists to remove, reproduced inside it.
//!
//! Never `-t`, and stdin is null. Measured while `dev` existed: a
//! backgrounded `ssh -t` puts the shared terminal into raw mode and eats
//! the Ctrl-C meant for the whole process group.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::compose::{self, Model};
use crate::footprint::Footprint;
use crate::intent::TunnelState;
use crate::proc;
use crate::ssh::Ssh;
use crate::ui::fail;

/// How long ssh gets to take the ports. Measured: spawn → LISTEN is
/// 0.97 s. The wait almost never runs that long, because readiness is
/// polled rather than slept through — and the wake chain has a ~5 second
/// budget it cannot spend on a fixed sleep.
const SETTLE: Duration = Duration::from_secs(3);

/// An ssh option that does nothing and says everything.
///
/// `SendEnv` names LOCAL variables to offer the server; an unset one is
/// simply not sent, so this is inert on both ends. What it buys is a
/// string in the process's own argv, which is how the startup sweep can
/// tell one of our orphaned tunnel children from whatever else inherited
/// its pid. The orphan defence must not rest on the unproven claim that
/// a child exits when its stdin closes.
const MARKER: &str = "SendEnv=ULAK_TUNNEL";

/// Where a Docker stack's live tunnel child records itself.
const PID_FILE: &str = "tunnels.pid";

/// One tunnel: local 127.0.0.1:`local_port` → remote `connect_ip:remote_port`.
#[derive(Debug, Clone, PartialEq)]
pub struct Tunnel {
    pub local_port: u32,
    pub connect_ip: String,
    pub remote_port: u32,
    pub service: String,
}

/// What the compose model asks for, and everything worth saying about it.
pub struct Plan {
    pub wanted: Vec<Tunnel>,
    /// Said by the caller, once, when they change — the service runs for
    /// weeks and must not repeat itself every fifteen seconds.
    pub warnings: Vec<String>,
}

/// The tunnels the model wants, read from the model the footprint was
/// resolved from — already on this machine, so no second round-trip. The
/// caller supplies the exact doctor command because a service may report this
/// while the user's shell is standing in an unrelated checkout.
pub fn plan(fp: &Footprint, doctor: &str) -> Result<Plan> {
    if fp.model_json.is_empty() {
        return Err(fail!("the compose model is not available yet")
            .now(format!("resolve it: {doctor}"))
            .into_err());
    }
    let model = compose::parse_model(&fp.model_json)?;
    let (wanted, mut warnings) = tunnels_from_model(&model);
    warnings.extend(public_exposure(&model));
    Ok(Plan { wanted, warnings })
}

/// A live tunnel set, owned by this process. Dropping it closes the
/// ports — that is the whole point of the type existing.
pub struct Tunnels {
    /// `None` when every port the stack publishes is taken on this
    /// machine. That is not a reason to stop maintaining the workspace, so
    /// it is a state rather than an error: the ports are named, the
    /// files still sync, and the next attempt costs one local bind probe.
    child: Option<Child>,
    open: Vec<Tunnel>,
    blocked: Vec<Tunnel>,
    pid_file: Option<PathBuf>,
}

impl Tunnels {
    /// Take the ports, or say precisely which ones could not be taken.
    ///
    /// The busy ones are dropped from the set BEFORE ssh sees it.
    /// `ExitOnForwardFailure` alone would refuse all twenty forwards
    /// because of one busy port — correct, and useless, since the busy
    /// port is usually something the user knowingly runs. Probing first
    /// means nineteen ports still come home and the twentieth is named.
    pub fn open(ssh: &Ssh, stack_id: &str, wanted: Vec<Tunnel>) -> Result<Tunnels> {
        let (open, blocked): (Vec<Tunnel>, Vec<Tunnel>) = wanted
            .into_iter()
            .partition(|t| TcpListener::bind(("127.0.0.1", t.local_port as u16)).is_ok());
        if open.is_empty() {
            return Ok(Tunnels {
                child: None,
                open,
                blocked,
                pid_file: None,
            });
        }

        let mut cmd = tunnel_command(ssh, &open);
        let mut child = cmd.spawn().context("cannot spawn ssh for the tunnels")?;
        // Written before `settle` waits it out, because `sweep_orphans`
        // has no other way in: it reads pid files, so a child this
        // process is killed before recording is invisible to every
        // later run and keeps its local ports for good. `settle` blocks
        // for up to SETTLE — measured around a second — and that was
        // the whole window.
        let pid_file = remember_pid(stack_id, child.id());
        crate::audit::record_command("ssh", &cmd, None); // long-lived child

        // `?` on its own left the child running with nobody holding it:
        // std's Child does not kill on drop, so the ssh stayed up
        // holding the very ports the retry would go on to find busy.
        let alive = match settle(&mut child, &open) {
            Ok(alive) => alive,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                forget_pid(pid_file.as_deref());
                return Err(e);
            }
        };
        if !alive {
            let status = child.wait().ok();
            forget_pid(pid_file.as_deref());
            return Err(tunnel_failure(ssh, status));
        }
        Ok(Tunnels {
            child: Some(child),
            open,
            blocked,
            pid_file,
        })
    }

    /// What `status` shows. A blocked port is reported too, with
    /// `open: false` — the honest half, and the reason `forward` stopped
    /// exiting 0 while tunnelling nothing.
    ///
    /// `service_running` stays `None` here on purpose: this type owns
    /// "did I make this forwarding", and whether anything ANSWERS behind
    /// an open one is the probe's answer, stamped over these states in
    /// `service::publish`. Computing it here too would be the second
    /// answer the One-Answer rule forbids.
    pub fn states(&self) -> Vec<TunnelState> {
        let mut states: Vec<TunnelState> = self
            .open
            .iter()
            .map(|t| TunnelState {
                port: t.local_port,
                service: t.service.clone(),
                open: true,
                service_running: None,
            })
            .chain(self.blocked.iter().map(|t| TunnelState {
                port: t.local_port,
                service: t.service.clone(),
                open: false,
                service_running: None,
            }))
            .collect();
        states.sort_by_key(|t| t.port);
        states
    }

    pub fn blocked(&self) -> &[Tunnel] {
        &self.blocked
    }

    /// Whether a port this set had to leave behind is free now.
    ///
    /// `covers` counts a blocked port as covered — it IS in the set,
    /// just not open — so a partially blocked set matched its plan and
    /// the service returned before probing anything. Nothing else
    /// reopens on that path: the child is alive and the model has not
    /// changed. So quitting the local Postgres that held 5432 did not
    /// bring its tunnel back; only a stack cycle, a link failure or a
    /// service restart did, and `status` kept saying NOT tunneled in
    /// the meantime.
    ///
    /// One bind per blocked port, and only while one IS blocked: the
    /// steady state where everything came home iterates an empty list.
    pub fn a_blocked_port_came_free(&self) -> bool {
        self.blocked
            .iter()
            .any(|t| TcpListener::bind(("127.0.0.1", t.local_port as u16)).is_ok())
    }

    /// Was there ever a child to lose? A set where every port was taken
    /// has none — reopening it is a retry, not a death, and announcing
    /// "the tunnels stopped" on every retry is how a log stops being
    /// read.
    pub fn was_running(&self) -> bool {
        self.child.is_some()
    }

    /// The ports this set was built for, so the caller can tell a model
    /// change from a steady state without re-reading the model.
    pub fn covers(&self, wanted: &[Tunnel]) -> bool {
        let mut mine: Vec<u32> = self
            .open
            .iter()
            .chain(&self.blocked)
            .map(|t| t.local_port)
            .collect();
        let mut theirs: Vec<u32> = wanted.iter().map(|t| t.local_port).collect();
        mine.sort_unstable();
        theirs.sort_unstable();
        mine == theirs
    }

    /// Did the child die on us? With `-S none` there is nothing to hand
    /// the forwards to, so a healthy child never exits and this answer
    /// is unambiguous — which it was not while the tunnels rode the
    /// shared master.
    pub fn died(&mut self) -> bool {
        match &mut self.child {
            Some(child) => child.try_wait().ok().flatten().is_some(),
            // Nothing to die: every port was taken. Retrying is the
            // caller's business, and it is cheap.
            None => true,
        }
    }
}

impl Drop for Tunnels {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(path) = &self.pid_file {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The ssh invocation for a set of tunnels.
fn tunnel_command(ssh: &Ssh, tunnels: &[Tunnel]) -> std::process::Command {
    // `direct_command` is the unmultiplexed one: these forwards belong to
    // this child, not to a master every other project shares.
    let mut cmd = ssh.direct_command();
    cmd.args(["-o", "ExitOnForwardFailure=yes"]);
    cmd.args(["-o", MARKER]);
    cmd.arg("-N");
    for t in tunnels {
        cmd.arg("-L").arg(l_spec(t));
    }
    cmd.arg("--").arg(&ssh.dest);
    // No -t, and no stdin: a backgrounded `ssh -t` flips the shared
    // terminal to raw mode and swallows Ctrl-C for the whole group.
    cmd.stdin(Stdio::null());
    cmd
}

/// Wait until ssh holds every port, or until it gives up.
///
/// Readiness is asked LOCALLY, by trying to bind: a port ssh has taken
/// cannot be bound by us, the answer is instant, and nothing crosses the
/// network to obtain it. Sleeping out the full settle window instead
/// would spend most of the wake chain's ~5-second budget doing nothing.
fn settle(child: &mut Child, tunnels: &[Tunnel]) -> Result<bool> {
    let deadline = Instant::now() + SETTLE;
    loop {
        if child
            .try_wait()
            .context("cannot poll the tunnel child")?
            .is_some()
        {
            return Ok(false);
        }
        let bound = tunnels
            .iter()
            .all(|t| TcpListener::bind(("127.0.0.1", t.local_port as u16)).is_err());
        if bound || Instant::now() >= deadline {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn tunnel_failure(ssh: &Ssh, status: Option<std::process::ExitStatus>) -> anyhow::Error {
    let code = status
        .and_then(|s| s.code())
        .map(|c| format!(" (ssh exit {c})"))
        .unwrap_or_default();
    fail!("the tunnels to {} did not open{code}", ssh.dest)
        .now("if a local port is busy, find the holder: lsof -i :<port>")
        .now(format!(
            "if the connection failed, test it: ssh -o ConnectTimeout=10 {} true",
            ssh.dest
        ))
        .into_err()
}

/// One -L spec; IPv6 connect addresses need brackets or ssh rejects
/// the whole forwarding set at startup.
fn l_spec(t: &Tunnel) -> String {
    let ip = if t.connect_ip.contains(':') {
        format!("[{}]", t.connect_ip)
    } else {
        t.connect_ip.clone()
    };
    format!("127.0.0.1:{}:{}:{}", t.local_port, ip, t.remote_port)
}

// ─── orphan defence ─────────────────────────────────────────────────

fn pid_path(stack_id: &str) -> Option<PathBuf> {
    Some(crate::intent::stack_dir(stack_id)?.join(PID_FILE))
}

fn remember_pid(stack_id: &str, pid: u32) -> Option<PathBuf> {
    let path = pid_path(stack_id)?;
    crate::invocation::write_private(&path, pid.to_string().as_bytes()).ok()?;
    Some(path)
}

/// The record for a child that never became a set. `Drop` clears it for
/// the ones that did; these leave before there is anything to drop, and
/// a pid file naming a dead process makes `sweep_orphans` chase it.
fn forget_pid(path: Option<&Path>) {
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
}

/// Kill tunnel children a previous service left behind.
///
/// Kill-on-drop covers every ordinary end, but not SIGKILL — and launchd
/// promises exactly five seconds before it sends one. An orphan here is
/// not a leak, it is a port on this machine pointing at a server nobody
/// is maintaining, so the next service would find it "busy" and refuse
/// to tunnel it forever.
///
/// A recorded pid is only killed when the process still carries our
/// marker: pids are recycled, and a stale file must never become a
/// weapon aimed at whatever inherited the number.
pub fn sweep_orphans() -> usize {
    let mut swept = 0;
    for id in crate::intent::stack_ids() {
        let Some(path) = pid_path(&id) else { continue };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let _ = std::fs::remove_file(&path);
        let Ok(pid) = text.trim().parse::<u32>() else {
            continue;
        };
        if is_our_tunnel(pid) && kill(pid) {
            swept += 1;
        }
    }
    swept
}

/// Does this pid still belong to a tunnel of ours? Asked with `ps`,
/// which is a visible argv like every other privileged step here — and
/// which is also the only portable answer without adding libc.
fn is_our_tunnel(pid: u32) -> bool {
    let mut cmd = std::process::Command::new("ps");
    cmd.args(["-o", "command=", "-p", &pid.to_string()]);
    proc::run_bounded(&mut cmd, None, proc::PROBE)
        .map(|out| String::from_utf8_lossy(&out.stdout).contains(MARKER))
        .unwrap_or(false)
}

fn kill(pid: u32) -> bool {
    let mut cmd = std::process::Command::new("kill");
    cmd.arg(pid.to_string());
    let code = proc::run_bounded(&mut cmd, None, proc::PROBE)
        .ok()
        .and_then(|b| b.status.code());
    crate::audit::record_command("kill", &cmd, code);
    code == Some(0)
}

// ─── the model's published ports ────────────────────────────────────

/// The published-port zoo, from the REMOTE-resolved model:
/// plain ports, host_ip-bound, ranges ("8080-8090"), udp (skipped —
/// ssh -L is TCP), unpublished targets (skipped), network_mode: host
/// (warned — such services never appear in ports).
pub fn tunnels_from_model(model: &Model) -> (Vec<Tunnel>, Vec<String>) {
    let mut tunnels = Vec::new();
    let mut warnings = Vec::new();

    for (name, service) in &model.services {
        if service.network_mode.as_deref() == Some("host") {
            // There is no `ulak forward <port>` to send anyone to any
            // more (measured: `network_mode` appears 0 times in the
            // compose files of a large monorepo). Pointing at a
            // command that does not exist would be worse than the dead
            // end — so this names the dead end.
            warnings.push(format!(
                "service {name} uses network_mode: host — compose never reports its ports, so Ulak cannot see them and cannot tunnel them; reach them over ssh, or publish them with a ports: entry"
            ));
        }
        for port in &service.ports {
            if let Some(proto) = &port.protocol
                && proto.eq_ignore_ascii_case("udp")
            {
                warnings.push(format!(
                    "service {name}: udp port {} skipped (ssh tunnels are TCP-only)",
                    port.published.as_deref().unwrap_or("?")
                ));
                continue;
            }
            let Some(published) = &port.published else {
                continue; // ephemeral host port — nothing stable to bind
            };
            let host_ip = port.host_ip.as_deref().unwrap_or("");
            let connect_ip = if host_ip.is_empty() || host_ip == "0.0.0.0" || host_ip == "::" {
                "127.0.0.1".to_string()
            } else {
                host_ip.to_string()
            };

            let published_ports = match parse_port_or_range(published) {
                Some(p) => p,
                None => {
                    warnings.push(format!(
                        "service {name}: unparseable published port {published:?} skipped"
                    ));
                    continue;
                }
            };
            // Tunnels end at the server's PUBLISHED port; the container
            // target never matters on this side.
            for port_no in published_ports {
                tunnels.push(Tunnel {
                    local_port: port_no,
                    connect_ip: connect_ip.clone(),
                    remote_port: port_no,
                    service: name.clone(),
                });
            }
        }
    }
    // Two publishes can want the same LOCAL port (different host_ips);
    // only one tunnel can bind it — say so instead of silently picking.
    tunnels.sort_by_key(|t| t.local_port);
    let mut kept: Vec<Tunnel> = Vec::new();
    for t in tunnels {
        match kept.last() {
            Some(prev) if prev.local_port == t.local_port => warnings.push(format!(
                "local port {} wanted by both {} and {} — tunneling {} only",
                t.local_port, prev.service, t.service, prev.service
            )),
            _ => kept.push(t),
        }
    }
    (kept, warnings)
}

/// Tunnelling a port to localhost makes it FEEL private; if compose
/// published it without a host IP it is open on the server's public
/// interface, and on a VPS that means the internet. Saying
/// "localhost:8080 → server:8080" without this line is a lie by omission.
fn public_exposure(model: &Model) -> Vec<String> {
    let public = compose::public_ports(model);
    if public.is_empty() {
        return Vec::new();
    }
    let example = &public[0].1;
    vec![format!(
        "{} port(s) are open on EVERY interface of the server, not only its loopback: {} — bind them in compose (ports: [\"127.0.0.1:{example}:{example}\"]) and reach them through the tunnel instead",
        public.len(),
        public
            .iter()
            .map(|(svc, p)| format!("{p} ({svc})"))
            .collect::<Vec<_>>()
            .join(", ")
    )]
}

/// Published ports are parsed as u16 because that is what a TCP port is
/// — anything else cannot be bound locally and cannot be forwarded, so
/// it is refused here rather than becoming a confusing bind failure two
/// steps later.
fn parse_port_or_range(text: &str) -> Option<Vec<u32>> {
    if let Some((a, b)) = text.split_once('-') {
        let (a, b) = (a.parse::<u16>().ok()?, b.parse::<u16>().ok()?);
        if a > b || b - a > 512 {
            return None;
        }
        Some((a..=b).map(u32::from).collect())
    } else {
        Some(vec![u32::from(text.parse::<u16>().ok()?)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(json: &str) -> Model {
        compose::parse_model(json).unwrap()
    }

    fn tunnel(port: u32) -> Tunnel {
        Tunnel {
            local_port: port,
            connect_ip: "127.0.0.1".into(),
            remote_port: port,
            service: "web".into(),
        }
    }

    #[test]
    fn port_zoo() {
        let m = model(
            r#"{"services": {
                "web":   {"ports": [{"target": 80, "published": "8080", "host_ip": "127.0.0.1", "protocol": "tcp"}]},
                "db":    {"ports": [{"target": 5432, "published": "5432", "protocol": "tcp"}]},
                "range": {"ports": [{"target": 9000, "published": "9000-9002", "protocol": "tcp"}]},
                "dns":   {"ports": [{"target": 53, "published": "5353", "protocol": "udp"}]},
                "eph":   {"ports": [{"target": 6379}]},
                "tail":  {"ports": [{"target": 80, "published": "8081", "host_ip": "198.51.100.7", "protocol": "tcp"}]},
                "hostnet": {"network_mode": "host"}
            }}"#,
        );
        let (tunnels, warnings) = tunnels_from_model(&m);
        let ports: Vec<u32> = tunnels.iter().map(|t| t.local_port).collect();
        assert_eq!(ports, vec![5432, 8080, 8081, 9000, 9001, 9002]);
        // specific-IP binds connect to that IP on the server side
        assert_eq!(
            tunnels
                .iter()
                .find(|t| t.local_port == 8081)
                .unwrap()
                .connect_ip,
            "198.51.100.7"
        );
        assert_eq!(
            tunnels
                .iter()
                .find(|t| t.local_port == 8080)
                .unwrap()
                .connect_ip,
            "127.0.0.1"
        );
        // udp skipped + hostnet warned
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("udp")));
        assert!(warnings.iter().any(|w| w.contains("network_mode")));
    }

    #[test]
    fn a_host_network_service_is_a_dead_end_not_a_command_that_no_longer_exists() {
        // Decision 6 removed `ulak forward <port>`; measured, the
        // target repo's compose files use network_mode zero times.
        // The warning has to point at the truth, not at a dead command.
        let m = model(r#"{"services": {"vpn": {"network_mode": "host"}}}"#);
        let (_, warnings) = tunnels_from_model(&m);
        assert_eq!(warnings.len(), 1);
        assert!(
            !warnings[0].contains("ulak forward"),
            "the warning names a command that no longer exists: {warnings:?}"
        );
        assert!(warnings[0].contains("cannot tunnel them"), "{warnings:?}");
    }

    #[test]
    fn ports_open_to_the_world_are_called_out() {
        let m = model(
            r#"{"services": {
                "safe": {"ports": [{"published": "8080", "host_ip": "127.0.0.1"}]},
                "open": {"ports": [{"published": "5432"}]}
            }}"#,
        );
        let w = public_exposure(&m);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("5432") && w[0].contains("open"), "{w:?}");
        assert!(!w[0].contains("8080"), "a loopback bind is not exposure");

        // Nothing to say when everything is bound to loopback.
        let m = model(
            r#"{"services": {"safe": {"ports": [{"published": "80", "host_ip": "127.0.0.1"}]}}}"#,
        );
        assert!(public_exposure(&m).is_empty());
    }

    #[test]
    fn ipv6_specs_get_brackets() {
        let t = Tunnel {
            local_port: 8080,
            connect_ip: "::1".into(),
            remote_port: 80,
            service: "w".into(),
        };
        assert_eq!(l_spec(&t), "127.0.0.1:8080:[::1]:80");
        let t4 = Tunnel {
            local_port: 8080,
            connect_ip: "127.0.0.1".into(),
            remote_port: 80,
            service: "w".into(),
        };
        assert_eq!(l_spec(&t4), "127.0.0.1:8080:127.0.0.1:80");
    }

    #[test]
    fn same_local_port_conflict_is_warned() {
        let m = model(
            r#"{"services": {
                "a": {"ports": [{"published": "8080", "host_ip": "127.0.0.1"}]},
                "b": {"ports": [{"published": "8080", "host_ip": "198.51.100.7"}]}
            }}"#,
        );
        let (tunnels, warnings) = tunnels_from_model(&m);
        assert_eq!(tunnels.len(), 1);
        assert!(
            warnings.iter().any(|w| w.contains("wanted by both")),
            "{warnings:?}"
        );
    }

    #[test]
    fn numeric_published_from_json_number() {
        // compose emits published as a string, but be liberal.
        let m = model(r#"{"services": {"w": {"ports": [{"target": 80, "published": 8088}]}}}"#);
        let (tunnels, _) = tunnels_from_model(&m);
        assert_eq!(tunnels[0].local_port, 8088);
    }

    #[test]
    fn bogus_ranges_are_skipped_with_warning() {
        let m = model(
            r#"{"services": {"w": {"ports": [{"published": "9-1"}, {"published": "1-99999"}]}}}"#,
        );
        let (tunnels, warnings) = tunnels_from_model(&m);
        assert!(tunnels.is_empty());
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn the_tunnel_argv_carries_everything_the_measurements_demand() {
        let ssh = Ssh::new("example").unwrap();
        let cmd = tunnel_command(&ssh, &[tunnel(8080)]);
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // Off the shared master, or the forwards belong to nobody again
        // (measured: ports surviving 61 minutes, no ulak alive).
        assert!(argv.contains(&"none".to_string()), "{argv:?}");
        assert!(argv.contains(&"ControlMaster=no".to_string()), "{argv:?}");
        // Or a busy port is skipped in silence and ulak lies about it.
        assert!(
            argv.contains(&"ExitOnForwardFailure=yes".to_string()),
            "{argv:?}"
        );
        // Or the startup sweep cannot tell an orphan of ours from a
        // recycled pid.
        assert!(argv.contains(&MARKER.to_string()), "{argv:?}");
        // Or a dead link keeps a tunnel child alive forever.
        assert!(argv.contains(&"ConnectTimeout=10".to_string()), "{argv:?}");
        // A tty here eats the Ctrl-C meant for the whole process group.
        assert!(!argv.contains(&"-t".to_string()), "{argv:?}");
    }

    #[test]
    fn a_busy_port_is_reported_rather_than_silently_dropped() {
        // The measured lie, and this product's own reason to exist turned
        // on itself: the port must appear in status as NOT open, not
        // vanish from the list.
        let holder = TcpListener::bind("127.0.0.1:0").expect("hold a port");
        let port = holder.local_addr().unwrap().port() as u32;
        let ssh = Ssh::new("example").unwrap();

        // Only the busy port, so nothing is spawned and the test needs
        // no server at all.
        let t = Tunnels::open(&ssh, "test-busy-port", vec![tunnel(port)]).unwrap();
        let states = t.states();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].port, port);
        assert!(!states[0].open, "a port we could not take must say so");
        assert_eq!(t.blocked().len(), 1);
    }
}
