//! SSH transport: always the real `ssh` binary, always the user's own
//! ~/.ssh/config (ProxyJump, agents and IdentityFile keep working).
//!
//! Multiplexing per the architecture contract: ControlMaster=auto +
//! ControlPersist with a short hashed socket name (NOT ssh's %C — see
//! `control_path`) in a private 0700 runtime dir. Every wait is bounded
//! by `proc::run_bounded`; when a command times out or fails at the ssh
//! layer the shared master is RETIRED (`-O exit`) and the command is
//! retried exactly once without multiplexing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::Result;

use crate::proc;
use crate::ui::{self, fail};

const CONTROL_PERSIST: &str = "600";

/// Options every connection carries, in both transports.
///
/// `ConnectTimeout` is the one that was missing and it is the one that
/// hurts: measured, `ulak status` against an unreachable server sat
/// silent for **150 seconds** (75 + 75 — macOS's
/// `net.inet.tcp.keepinit`), and the identical call with
/// `ConnectTimeout=10` failed honestly in 10.1 s with a real error.
///
/// The keepalives matter too, but only for the process that CARRIES the
/// TCP — the control master. A command sent through the mux is a slave:
/// its own `ServerAliveInterval` is dead letter. Measured, when the link
/// dies the master needs ~72 s to notice, the slave then falls out of
/// the mux and reconnects DIRECTLY inside the same process (verified
/// with lsof: fd3 a dead unix socket, fd4 a brand new ESTABLISHED TCP).
/// `ConnectTimeout` is what stops that second connection from hanging
/// forever; `proc::run_bounded` is what stops everything else.
const LINK_OPTS: &[&str] = &[
    "ConnectTimeout=10",
    "ServerAliveInterval=15",
    "ServerAliveCountMax=4",
    "TCPKeepAlive=yes",
];

#[derive(Debug, Clone)]
pub struct Ssh {
    pub dest: String,
    control_dir: PathBuf,
    /// Identity and host-key options resolved once, from the
    /// environment, and carried by every connection this handle makes —
    /// including rsync's, which rides the same transport string.
    identity: Vec<String>,
}

impl Ssh {
    pub fn new(dest: &str) -> Result<Ssh> {
        use std::io::IsTerminal;
        let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        Ssh::new_in(
            dest,
            &std::env::vars().collect(),
            interactive,
            &identity_dir()?,
        )
    }

    pub(crate) fn new_in(
        dest: &str,
        env: &BTreeMap<String, String>,
        interactive: bool,
        identity_dir: &Path,
    ) -> Result<Ssh> {
        Ok(Ssh {
            dest: dest.to_string(),
            control_dir: control_dir()?,
            identity: identity_args(env, interactive, identity_dir)?,
        })
    }

    /// Socket name is our own short hash of the destination, not ssh's
    /// %C: %C is 40 hex chars and ssh appends a ~17-char temp suffix on
    /// creation, which overflows the ~104-byte unix socket path limit
    /// under macOS's deep $TMPDIR. Measured, not theoretical.
    ///
    /// The directory is re-created here, on every call, because ssh does
    /// NOT degrade gracefully without it. Measured against a real
    /// server: with the directory gone, `ssh -o ControlPath=…/sock`
    /// prints `unix_listener: cannot bind to path …` and fails outright
    /// — it does not fall back to a plain connection. Creating it once at
    /// startup is therefore not enough for a process meant to run for
    /// weeks: a `/tmp` cleaner, a `XDG_RUNTIME_DIR` that gets swept, or
    /// anything else that removes it would leave the service unable to
    /// reach the server ever again, with no error that names the cause.
    pub fn control_path(&self) -> String {
        let _ = crate::invocation::private_dir(&self.control_dir);
        let hash = &crate::hashid::fnv1a128_hex(self.dest.as_bytes())[..16];
        format!("{}/{hash}", self.control_dir.display())
    }

    fn multiplex_args(&self) -> Vec<String> {
        let mut args = vec![
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}", self.control_path()),
            "-o".into(),
            format!("ControlPersist={CONTROL_PERSIST}"),
        ];
        for k in LINK_OPTS {
            args.push("-o".into());
            args.push((*k).into());
        }
        args.extend(self.identity.iter().cloned());
        args
    }

    /// Base `ssh` invocation with multiplexing, no remote command yet.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.args(self.multiplex_args());
        cmd
    }

    /// Run a POSIX script on the server, capturing output. The remote
    /// command line is just `sh -s` and the script travels on stdin —
    /// so a csh/fish/zsh login shell only ever parses two safe words,
    /// and a real POSIX sh interprets the script.
    ///
    /// A non-zero exit is a normal answer (footprint resolution reads
    /// compose's failure on purpose). A DEADLINE is not: it means the
    /// link is unusable, and it comes back as an error.
    pub fn run_script(&self, script: &str) -> Result<Output> {
        let first = self.script_attempt(script, true)?;
        if !first.timed_out && first.status.code() != Some(255) {
            return Ok(first.into_output());
        }
        // The retry is no longer a HEALTH question. `-O check` asks the
        // LOCAL unix socket and nothing else: measured, it kept
        // answering "Master running" for 72 seconds after the link died
        // — precisely the window the retry existed for. So the master is
        // not interrogated any more, it is retired.
        if first.timed_out {
            ui::warn(&format!(
                "{} stopped answering within {}s — dropping the shared connection and retrying directly",
                self.dest,
                proc::SCRIPT.as_secs()
            ));
        }
        self.close_master();
        let second = self.script_attempt(script, false)?;
        if second.timed_out {
            return Err(dead_link(&self.dest, proc::SCRIPT));
        }
        Ok(second.into_output())
    }

    /// One bounded question, asked once. No retry, no master surgery:
    /// the caller is the service, which runs on a cadence and has its
    /// own opinion about what a silence means — turning every failed
    /// probe into a retry and an `-O exit` would make a flaky link cost
    /// three connections instead of one.
    pub fn probe(&self, script: &str) -> Result<proc::Bounded> {
        self.script_attempt(script, true)
    }

    /// The same question, asked without the shared master. This is the
    /// sparse probe the service uses when every stack it watches is
    /// down: that workspace is meant to be SILENT, and a probe
    /// that opens or renews a shared master is not silence.
    pub fn probe_direct(&self, script: &str) -> Result<proc::Bounded> {
        self.script_attempt(script, false)
    }

    /// A connection that deliberately ignores the shared master.
    ///
    /// Two callers, one reason. The retry path uses it because the
    /// master is already the suspect; the tunnels use it because a
    /// forward handed to the shared master belongs to nobody — measured,
    /// 21 ports outlived every ulak process by 61 minutes. Off that
    /// socket they are a child this process owns, and `-O exit` stops
    /// being a grenade aimed at every other project on the server.
    ///
    /// It used to go out bare (no ConnectTimeout, no keepalive), which
    /// made the fallback the likeliest place in the program to hang.
    pub fn direct_command(&self) -> Command {
        let mut c = Command::new("ssh");
        c.args(["-o", "ControlMaster=no", "-S", "none"]);
        for o in LINK_OPTS {
            c.arg("-o").arg(o);
        }
        c
    }

    fn script_attempt(&self, script: &str, multiplex: bool) -> Result<proc::Bounded> {
        let mut cmd = if multiplex {
            self.command()
        } else {
            self.direct_command()
        };
        cmd.arg("--").arg(&self.dest).arg("sh -s");
        let out = proc::run_bounded(&mut cmd, Some(script.as_bytes().to_vec()), proc::SCRIPT)?;
        // The script travels on stdin, so record it explicitly — the
        // argv alone would just say "sh -s".
        let mut argv: Vec<String> = vec![cmd.get_program().to_string_lossy().into_owned()];
        argv.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
        argv.push(format!("<<stdin: {}>>", script.trim()));
        crate::audit::record("ssh", &argv, out.status.code());
        Ok(out)
    }

    /// Like run_script but turns a failure into a guided error.
    pub fn run_checked(&self, script: &str, what: &str) -> Result<Output> {
        let out = self.run_script(script)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stderr = stderr.trim();
            return Err(connect_fail(&self.dest, what, out.status.code(), stderr));
        }
        Ok(out)
    }

    /// Retire the shared connection.
    ///
    /// `-O exit` is a blunt instrument: the control path is keyed on the
    /// DESTINATION alone, so one master carries every project on that
    /// server plus every rsync in flight (the tunnels are the exception
    /// — they ride `direct_command`, which is what keeps this from being
    /// a grenade). Two paths fire it. After a command of OURS timed out
    /// or died at the ssh layer (`run_script` above, `service`'s failed
    /// probe) the master is the prime suspect rather than an innocent
    /// bystander. On a HARD WAKE (`service::on_wake`) it is buried
    /// unasked, because `-O check` keeps answering "Master running" for
    /// the ~72 s ssh needs to notice a dead link — and on that path
    /// another project's rsync can be riding the socket when it goes.
    pub fn close_master(&self) {
        let mut cmd = Command::new("ssh");
        cmd.args(["-o", &format!("ControlPath={}", self.control_path())])
            .args(["-O", "exit", "--"])
            .arg(&self.dest);
        let code = proc::run_bounded(&mut cmd, None, proc::PROBE)
            .ok()
            .and_then(|b| b.status.code());
        crate::audit::record_command("ssh", &cmd, code);
    }

    /// The `-e` transport string for rsync, sharing our warm socket.
    /// rsync splits this on whitespace honoring quotes; the ControlPath
    /// is quoted with whichever quote it does not itself contain
    /// (TMPDIR/HOME can hold apostrophes).
    pub fn rsync_transport(&self) -> String {
        let quoted = rsync_quote(&format!("ControlPath={}", self.control_path()));
        let link = LINK_OPTS
            .iter()
            .map(|k| format!(" -o {k}"))
            .collect::<String>();
        // Every word rsync will re-split gets the same quoting the
        // ControlPath already needed: a key path under HOME can hold a
        // space or an apostrophe just as easily.
        let identity = self
            .identity
            .iter()
            .map(|a| format!(" {}", rsync_quote(a)))
            .collect::<String>();
        format!(
            "ssh -o ControlMaster=auto -o {quoted} -o ControlPersist={CONTROL_PERSIST}{link}{identity}"
        )
    }
}

/// Private 0700 directory for control sockets. The full socket path
/// (dir + 16-hex name + ssh's ~17-char temp suffix) must stay under the
/// ~104-byte unix limit, so candidates are tried in order of preference
/// and skipped when too deep.
fn control_dir() -> Result<PathBuf> {
    const MAX_DIR_LEN: usize = 69; // 69 + 1 + 16 + 17 ≤ 103

    let candidates = [
        std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(|x| PathBuf::from(x).join("ulak")),
        std::env::var_os("TMPDIR")
            .filter(|v| !v.is_empty())
            .map(|t| PathBuf::from(t).join("ulak-ctl")),
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(|h| PathBuf::from(h).join(".ulak/ctl")),
    ];
    let base = candidates
        .into_iter()
        .flatten()
        .find(|p| p.as_os_str().len() <= MAX_DIR_LEN)
        .ok_or_else(|| {
            fail!("cannot find a short enough directory for ssh control sockets")
                .now("set XDG_RUNTIME_DIR to a short private path, e.g. /tmp/run-$USER")
                .into_err()
        })?;

    // 0700: a control socket anyone can reach is a shell on the server
    // for anyone who can reach it.
    crate::invocation::private_dir(&base).map_err(|e| {
        fail!(
            "cannot prepare the ssh control directory {}: {e}",
            base.display()
        )
        .now("check its ownership, or point XDG_RUNTIME_DIR at a private path you own")
        .into_err()
    })?;
    Ok(base)
}

/// Quote a string for a POSIX remote shell.
///
/// `~` is the one byte here that is not about safety. A tilde cannot
/// produce a command, so leaving it unquoted is not an injection — but
/// the remote shell EXPANDS it, and a quoter that expands one of its
/// inputs is not doing its job. `ulak docker exec web ls '~'` listed the
/// ssh user's home inside a container that has no such user.
///
/// It is still passed through in exactly one shape: a LEADING `~/`.
/// That shape is load-bearing. `-v ~/data:/app` is how a user says "the
/// directory in the SERVER's home" — compose reads a tilde volume the
/// same way, and `runspec` and `composepaths` both classify it as
/// server-side and deliberately leave the argv alone, on the
/// understanding that the tilde survives to the far shell. Quoting it
/// turned that into a literal `~/data`, which docker rejects as neither
/// an absolute path nor a valid volume name.
///
/// So: `~/x` expands, `~`, `~root/x` and `a~b` do not. Only a leading
/// tilde expands at all (measured in bash, /bin/sh and zsh), so
/// `web:~/app` was never at risk either way.
///
/// What remains unfaithful, knowingly: a container command argument
/// that is a literal `~/x`. That is rarer than the volume spelling, and
/// it is the behaviour this had before either way.
pub fn sh_quote(s: &str) -> String {
    let body = s.strip_prefix("~/").unwrap_or(s);
    if !s.is_empty()
        && body.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/' | b'-' | b'_' | b':' | b'@')
        })
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The link swallowed both the multiplexed attempt and the direct one.
/// Saying "ssh failed" here would be a lie by omission: nothing failed,
/// nothing answered.
fn dead_link(dest: &str, limit: Duration) -> anyhow::Error {
    fail!(
        "{dest} did not answer within {}s, twice — the connection is not usable right now",
        limit.as_secs()
    )
    .now(format!(
        "test it by hand: ssh -o ConnectTimeout=10 {dest} true"
    ))
    .now("if you just woke the laptop or changed networks, run the command again — the stale connection has already been dropped")
    .maybe_now(crate::management::retirement_step(dest))
    .into_err()
}

fn connect_fail(dest: &str, what: &str, code: Option<i32>, stderr: &str) -> anyhow::Error {
    let detail = if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    };
    let mut f = fail!(
        "{what} failed on {dest} (ssh exit {}){detail}",
        code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
    );
    if code == Some(255) {
        // Both steps ask the user to reach the host, which is the wrong
        // diagnosis for one that has been DESTROYED; a declaration on it is
        // the only local evidence of that, and management knows which.
        f = f
            .now(format!("check the connection by hand: ssh {dest} true"))
            .now("if the host is unknown, add it to ~/.ssh/config, then rerun")
            .maybe_now(crate::management::retirement_step(dest));
    } else {
        f = f.now(format!("inspect the server side: ssh {dest}"));
    }
    f.into_err()
}

// ─── identity from the environment ──────────────────────────────────────

/// The SSH identity, when the machine has no `~/.ssh` worth speaking of.
///
/// Ulak's default is still total delegation: with none of these set,
/// every connection is exactly the one the user's own `ssh` would make,
/// agent and `~/.ssh/config` included. That is Docker's model for
/// `DOCKER_HOST=ssh://` and it stays the common case.
///
/// What it adds is the CI shape, where none of that exists: the key
/// arrives as a secret and the runner is wiped after the job. Both doors
/// are offered because both are real — a secret store hands you contents,
/// a mounted volume hands you a path — and every comparable tool that
/// grew this surface (rclone, Ansible, Terraform, Kamal) ended up with
/// the pair.
///
/// Three refusals rather than a best guess: two keys named at once, an
/// encrypted key, and an unknown host. The last is the important one —
/// `StrictHostKeyChecking` is never relaxed here, in any mode. A CI job
/// that accepts whatever key answers is a CI job that can be handed a
/// different server, and the whole point of carrying files to a machine
/// is knowing which machine it is.
fn identity_args(
    env: &BTreeMap<String, String>,
    interactive: bool,
    dir: &Path,
) -> Result<Vec<String>> {
    let mut args = Vec::new();

    let content = env.get("ULAK_SSH_KEY").filter(|v| !v.is_empty());
    let path = env.get("ULAK_SSH_KEY_FILE").filter(|v| !v.is_empty());
    if content.is_some() && path.is_some() {
        return Err(fail!("ULAK_SSH_KEY and ULAK_SSH_KEY_FILE are both set")
            .now("keep the one that holds this run's key and unset the other")
            .into_err());
    }

    if let Some(raw) = content {
        let key = normalise_key(raw);
        check_unencrypted(&key, "ULAK_SSH_KEY")?;
        let dest = dir.join("id");
        write_secret(&dest, key.as_bytes())?;
        args.push("-i".into());
        args.push(dest.display().to_string());
    } else if let Some(raw) = path {
        let dest = PathBuf::from(raw);
        let key = std::fs::read_to_string(&dest).map_err(|e| {
            fail!("cannot read the key at {}: {e}", dest.display())
                .now("check ULAK_SSH_KEY_FILE names a readable private key")
                .into_err()
        })?;
        check_unencrypted(&key, "ULAK_SSH_KEY_FILE")?;
        args.push("-i".into());
        args.push(dest.display().to_string());
    }

    // Ask for THIS key and no other. An agent holding a dozen identities
    // otherwise offers them all and the server closes the connection with
    // "Too many authentication failures" long before ours is reached.
    if !args.is_empty() {
        args.push("-o".into());
        args.push("IdentitiesOnly=yes".into());
    }

    if let Some(hosts) = env.get("ULAK_KNOWN_HOSTS").filter(|v| !v.is_empty()) {
        let dest = dir.join("known_hosts");
        write_secret(&dest, ensure_final_newline(hosts).as_bytes())?;
        args.push("-o".into());
        args.push(format!("UserKnownHostsFile={}", dest.display()));
    }

    // No terminal, so no prompt can be answered: fail honestly instead of
    // blocking on a question nobody will ever see. Same line `ui::confirm`
    // draws when it answers NO without a terminal.
    if !interactive {
        args.push("-o".into());
        args.push("BatchMode=yes".into());
    }
    Ok(args)
}

/// A key pasted through a secret store arrives one of two ways, and both
/// are accepted because guessing wrong costs an unreadable failure deep
/// inside ssh.
///
/// Multi-line is the usual one; GitLab documents that its value must end
/// with a newline, and a key without the trailing LF is rejected by
/// OpenSSH, so the newline is restored rather than diagnosed. The other
/// is rclone's single line with literal `\n`, which exists because some
/// stores will not carry a multi-line value at all. There is no ambiguity
/// between them: a PEM body is base64 plus dashes, so a backslash can
/// only be an escape.
fn normalise_key(raw: &str) -> String {
    let expanded = if raw.contains('\n') {
        raw.to_string()
    } else {
        raw.replace("\\n", "\n")
    };
    ensure_final_newline(&expanded)
}

fn ensure_final_newline(raw: &str) -> String {
    if raw.ends_with('\n') {
        raw.to_string()
    } else {
        format!("{raw}\n")
    }
}

/// Refuse an encrypted key by name, because the alternative is ssh
/// prompting for a passphrase in a job with no terminal, or worse,
/// failing with a message about a bad format.
///
/// Both encodings are detectable without a base64 dependency. The old PEM
/// form announces itself in a header. The current OpenSSH form stores the
/// cipher name in the blob's first bytes: `openssh-key-v1\0` then the
/// length-prefixed cipher, which for an unencrypted key is `none` — and
/// that fixed 19-byte prefix base64-encodes to the constant below.
/// Measured: `ssh-keygen -t ed25519 -N ""` produces it, and the same key
/// with a passphrase starts `b3BlbnNzaC1rZXktdjEAAAAAC2FlczI1Ni1jdHI`.
const OPENSSH_PLAINTEXT: &str = "b3BlbnNzaC1rZXktdjEAAAAABG5vbmU";

fn check_unencrypted(key: &str, var: &str) -> Result<()> {
    let encrypted = if key.contains("Proc-Type: 4,ENCRYPTED") || key.contains("DEK-Info:") {
        true
    } else if key.contains("BEGIN OPENSSH PRIVATE KEY") {
        !key.lines().any(|l| l.trim().starts_with(OPENSSH_PLAINTEXT))
    } else {
        false
    };
    if encrypted {
        return Err(fail!("the key in {var} is protected by a passphrase")
            .now("use a key with no passphrase for unattended runs")
            .now("or load it into an agent and let Ulak use that: ssh-add <key>")
            .into_err());
    }
    Ok(())
}

/// 0600, and replaced by rename so ssh never opens a half-written key.
fn write_secret(dest: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dest.parent() {
        crate::invocation::private_dir(parent).map_err(|e| {
            fail!("cannot create {}: {e}", parent.display())
                .now("check that the local state directory is writable")
                .into_err()
        })?;
    }
    let tmp = dest.with_extension("tmp");
    crate::invocation::write_private(&tmp, bytes)
        .and_then(|()| std::fs::rename(&tmp, dest))
        .map_err(|e| {
            fail!("cannot write {}: {e}", dest.display())
                .now("check that the local state directory is writable")
                .into_err()
        })
}

fn identity_dir() -> Result<PathBuf> {
    let dir = crate::invocation::state_dir_required()?.join("ssh");
    Ok(dir)
}

/// One quoting answer for every word rsync will re-split, whichever
/// quote the value itself does not contain (TMPDIR and HOME can hold
/// apostrophes).
fn rsync_quote(raw: &str) -> String {
    if raw.contains('\'') {
        format!("\"{raw}\"")
    } else {
        format!("'{raw}'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> Ssh {
        Ssh {
            dest: "example".into(),
            control_dir: PathBuf::from("/tmp/ulak-test"),
            identity: Vec::new(),
        }
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    const PLAINTEXT_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAE\n-----END OPENSSH PRIVATE KEY-----";

    /// With nothing set, every connection is exactly the one the user's
    /// own ssh would make. This is the common case and it must not have
    /// moved: the agent and ~/.ssh/config keep working untouched.
    #[test]
    fn an_empty_environment_adds_nothing_to_the_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let args = identity_args(&BTreeMap::new(), true, tmp.path()).unwrap();
        assert!(args.is_empty(), "{args:?}");
    }

    /// A secret store hands you contents; a mounted volume hands you a
    /// path. Both doors reach the same place, and the key asked for is
    /// the only one offered — an agent holding a dozen identities
    /// otherwise burns the server's attempt limit before ours is tried.
    #[test]
    fn either_door_names_the_key_and_asks_for_no_other() {
        let tmp = tempfile::tempdir().unwrap();
        let args =
            identity_args(&env(&[("ULAK_SSH_KEY", PLAINTEXT_KEY)]), true, tmp.path()).unwrap();
        assert_eq!(args[0], "-i");
        assert!(args.contains(&"IdentitiesOnly=yes".to_string()), "{args:?}");
        let written = std::fs::read_to_string(&args[1]).unwrap();
        assert!(written.starts_with("-----BEGIN OPENSSH"), "{written}");

        let on_disk = tmp.path().join("from-volume");
        std::fs::write(&on_disk, PLAINTEXT_KEY).unwrap();
        let args = identity_args(
            &env(&[("ULAK_SSH_KEY_FILE", on_disk.to_str().unwrap())]),
            true,
            tmp.path(),
        )
        .unwrap();
        assert_eq!(args[1], on_disk.display().to_string());
    }

    /// 0600: a key the rest of the machine can read is not a secret, and
    /// ssh refuses to use one anyway.
    #[test]
    fn a_key_from_the_environment_lands_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let args =
            identity_args(&env(&[("ULAK_SSH_KEY", PLAINTEXT_KEY)]), true, tmp.path()).unwrap();
        let mode = std::fs::metadata(&args[1]).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    /// Two spellings arrive from the wild: the ordinary multi-line value,
    /// and rclone's single line with literal \n for stores that will not
    /// carry a newline. Both must reach identical bytes, and the trailing
    /// newline OpenSSH insists on is restored rather than diagnosed.
    #[test]
    fn both_spellings_of_a_key_reach_the_same_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let multi = identity_args(
            &env(&[("ULAK_SSH_KEY", PLAINTEXT_KEY)]),
            true,
            &tmp.path().join("a"),
        )
        .unwrap();
        let escaped = identity_args(
            &env(&[("ULAK_SSH_KEY", &PLAINTEXT_KEY.replace('\n', "\\n"))]),
            true,
            &tmp.path().join("b"),
        )
        .unwrap();
        let a = std::fs::read_to_string(&multi[1]).unwrap();
        let b = std::fs::read_to_string(&escaped[1]).unwrap();
        assert_eq!(a, b);
        assert!(a.ends_with('\n'), "OpenSSH rejects a key with no final LF");
    }

    /// Naming two keys is a mistake with no safe reading: picking one
    /// would authenticate as an identity the job did not mean to use.
    #[test]
    fn naming_two_keys_is_refused_rather_than_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let err = identity_args(
            &env(&[("ULAK_SSH_KEY", PLAINTEXT_KEY), ("ULAK_SSH_KEY_FILE", "/k")]),
            true,
            tmp.path(),
        )
        .unwrap_err();
        let err = crate::ui::flatten(&err);
        assert!(
            err.contains("ULAK_SSH_KEY") && err.contains("ULAK_SSH_KEY_FILE"),
            "{err}"
        );
    }

    /// Otherwise ssh asks for a passphrase in a job with no terminal, or
    /// fails with a message about key formats that names nothing.
    #[test]
    fn an_encrypted_key_is_refused_by_name_in_both_encodings() {
        let tmp = tempfile::tempdir().unwrap();
        let openssh = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAAC2FlczI1Ni1jdHI\n-----END OPENSSH PRIVATE KEY-----";
        let old_pem = "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,1\n\nabc\n-----END RSA PRIVATE KEY-----";
        for key in [openssh, old_pem] {
            let err = identity_args(&env(&[("ULAK_SSH_KEY", key)]), true, tmp.path()).unwrap_err();
            let err = crate::ui::flatten(&err);
            assert!(
                err.contains("passphrase") && err.contains("ssh-add"),
                "{err}"
            );
        }
        // And the plaintext one is not caught by the same net.
        assert!(check_unencrypted(PLAINTEXT_KEY, "ULAK_SSH_KEY").is_ok());
    }

    /// A classic PEM key — `ssh-keygen -m PEM`, or anything older than
    /// the OpenSSH container — announces neither `Proc-Type` nor
    /// `openssh-key-v1`, so it matches neither net. The answer that has
    /// to come back is "not encrypted": every other key in this suite is
    /// the new format, so a detector that refused whatever it did not
    /// recognise would lock out every classic key and tell its owner to
    /// remove a passphrase they never set.
    #[test]
    fn a_key_in_neither_known_encoding_is_taken_as_unencrypted() {
        const CLASSIC: &str =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\n";
        for key in [
            CLASSIC,
            "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEEIA\n-----END EC PRIVATE KEY-----\n",
            "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2Vw\n-----END PRIVATE KEY-----\n",
        ] {
            assert!(check_unencrypted(key, "ULAK_SSH_KEY").is_ok(), "{key}");
        }
        // And the whole door, not just the detector: the key is written
        // and named like any other.
        let tmp = tempfile::tempdir().unwrap();
        let args = identity_args(&env(&[("ULAK_SSH_KEY", CLASSIC)]), true, tmp.path()).unwrap();
        assert_eq!(args[0], "-i");
        assert_eq!(std::fs::read_to_string(&args[1]).unwrap(), CLASSIC);
    }

    /// The refusal is worth nothing on one door alone: a key mounted as
    /// a file would reach ssh unread and prompt for a passphrase in a job
    /// with no terminal — the failure the check exists to prevent. It
    /// names the variable this run actually set, because being sent to
    /// fix ULAK_SSH_KEY when only ULAK_SSH_KEY_FILE is set is being sent
    /// to an empty name.
    #[test]
    fn an_encrypted_key_is_refused_through_the_file_door_too() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, key) in [
            (
                "old-pem",
                "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,1\n\nabc\n-----END RSA PRIVATE KEY-----\n",
            ),
            (
                "openssh",
                "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAAC2FlczI1Ni1jdHI\n-----END OPENSSH PRIVATE KEY-----\n",
            ),
        ] {
            let on_disk = tmp.path().join(name);
            std::fs::write(&on_disk, key).unwrap();
            let err = identity_args(
                &env(&[("ULAK_SSH_KEY_FILE", on_disk.to_str().unwrap())]),
                true,
                tmp.path(),
            )
            .unwrap_err();
            let err = crate::ui::flatten(&err);
            assert!(
                err.contains("passphrase") && err.contains("ssh-add"),
                "{err}"
            );
            assert!(err.contains("ULAK_SSH_KEY_FILE"), "{err}");
        }
    }

    /// The usual secret already ends with the newline: ssh-keygen writes
    /// one and GitLab requires one. Restoring it unconditionally gave the
    /// contents door a trailing blank line the file door never adds, so
    /// one key took two shapes depending on which variable carried it and
    /// the written file stopped matching the secret it was pasted from.
    #[test]
    fn a_value_that_already_ends_in_a_newline_does_not_gain_a_second() {
        let tmp = tempfile::tempdir().unwrap();
        let terminated = format!("{PLAINTEXT_KEY}\n");

        let on_disk = tmp.path().join("from-volume");
        std::fs::write(&on_disk, &terminated).unwrap();
        let by_path = identity_args(
            &env(&[("ULAK_SSH_KEY_FILE", on_disk.to_str().unwrap())]),
            true,
            tmp.path(),
        )
        .unwrap();
        let by_content = identity_args(
            &env(&[("ULAK_SSH_KEY", terminated.as_str())]),
            true,
            tmp.path(),
        )
        .unwrap();
        let written = std::fs::read_to_string(&by_content[1]).unwrap();
        assert_eq!(written, std::fs::read_to_string(&by_path[1]).unwrap());
        assert_eq!(written, terminated);

        // Same rule for host keys, which leave ssh-keyscan with the
        // newline already on the end.
        let args = identity_args(
            &env(&[("ULAK_KNOWN_HOSTS", "server ssh-ed25519 AAAA\n")]),
            true,
            tmp.path(),
        )
        .unwrap();
        assert!(args.join(" ").contains("UserKnownHostsFile="), "{args:?}");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("known_hosts")).unwrap(),
            "server ssh-ed25519 AAAA\n"
        );
    }

    /// Host keys are pinned from the environment, and StrictHostKeyChecking
    /// is never relaxed — in any mode. A run that accepts whichever key
    /// answers can be handed a different server, which defeats the point
    /// of knowing where the files went.
    #[test]
    fn host_keys_are_pinned_and_checking_is_never_relaxed() {
        let tmp = tempfile::tempdir().unwrap();
        let args = identity_args(
            &env(&[("ULAK_KNOWN_HOSTS", "server ssh-ed25519 AAAA")]),
            true,
            tmp.path(),
        )
        .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("UserKnownHostsFile="), "{joined}");
        let written = std::fs::read_to_string(tmp.path().join("known_hosts")).unwrap();
        assert_eq!(written, "server ssh-ed25519 AAAA\n");

        // Nothing anywhere may weaken it, whatever else is set.
        for env in [
            BTreeMap::new(),
            env(&[
                ("ULAK_KNOWN_HOSTS", "h k v"),
                ("ULAK_SSH_KEY", PLAINTEXT_KEY),
            ]),
        ] {
            for interactive in [true, false] {
                let args = identity_args(&env, interactive, tmp.path())
                    .unwrap()
                    .join(" ");
                assert!(!args.contains("StrictHostKeyChecking"), "{args}");
            }
        }
    }

    /// The pure half of an is_terminal() decision: with no terminal ssh
    /// must fail honestly rather than block on a prompt nobody can see —
    /// the line `ui::confirm` already draws.
    #[test]
    fn a_run_with_no_terminal_refuses_to_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let headless = identity_args(&BTreeMap::new(), false, tmp.path()).unwrap();
        assert_eq!(headless, ["-o", "BatchMode=yes"]);
        assert!(
            identity_args(&BTreeMap::new(), true, tmp.path())
                .unwrap()
                .is_empty()
        );
    }

    /// rsync rides the same transport, so it must authenticate the same
    /// way — and every word it re-splits needs the quoting the
    /// ControlPath already taught us.
    #[test]
    fn rsync_shares_the_identity_and_quotes_every_word_of_it() {
        let mut ssh = fake();
        ssh.identity = vec![
            "-i".into(),
            "/keys/with a space/id".into(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
        ];
        let transport = ssh.rsync_transport();
        assert!(transport.contains("'/keys/with a space/id'"), "{transport}");
        assert!(transport.contains("'IdentitiesOnly=yes'"), "{transport}");
        assert_eq!(rsync_quote("has'apostrophe"), "\"has'apostrophe\"");
    }

    /// rsync's half is asserted below; this is the OTHER half. An
    /// identity that reached rsync but not ssh would sync the files and
    /// then fail to run docker, which reads as a broken server.
    #[test]
    fn the_ssh_side_carries_the_identity_too() {
        let mut ssh = fake();
        ssh.identity = vec![
            "-i".into(),
            "/keys/id".into(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
        ];
        let args = ssh.multiplex_args().join(" ");
        assert!(args.contains("-i /keys/id"), "{args}");
        assert!(args.contains("IdentitiesOnly=yes"), "{args}");
        // And the link options are still there beside it.
        assert!(args.contains("ConnectTimeout=10"), "{args}");
    }

    /// The seam the two tests above stop short of: everything else here
    /// fills `identity` by hand, so a constructor that resolved the
    /// environment and then dropped the answer would leave the suite
    /// green while every CI run authenticated as nobody and hung on a
    /// passphrase prompt no one can see.
    #[test]
    fn a_connection_built_from_the_environment_carries_the_key_and_batch_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let ssh = Ssh::new_in(
            "example",
            &env(&[("ULAK_SSH_KEY", PLAINTEXT_KEY)]),
            false,
            tmp.path(),
        )
        .unwrap();
        let key = tmp.path().join("id");
        let argv: Vec<String> = ssh
            .command()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(argv.contains(&"-i".to_string()), "{argv:?}");
        assert!(argv.contains(&key.display().to_string()), "{argv:?}");
        assert!(argv.contains(&"IdentitiesOnly=yes".to_string()), "{argv:?}");
        assert!(argv.contains(&"BatchMode=yes".to_string()), "{argv:?}");
        assert!(
            std::fs::read_to_string(&key)
                .unwrap()
                .starts_with("-----BEGIN OPENSSH"),
            "the key named on the command line is not the one from the environment"
        );
        // rsync rides the same handle, so it inherits the same answer.
        let transport = ssh.rsync_transport();
        assert!(transport.contains("'BatchMode=yes'"), "{transport}");
    }

    /// The path came from a mounted volume that did not mount, or a
    /// secret that did not render. ssh's own message for this names a
    /// file it was never given.
    #[test]
    fn a_key_file_that_is_not_there_is_refused_by_path() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("never-mounted/id");
        let err = identity_args(
            &env(&[("ULAK_SSH_KEY_FILE", missing.to_str().unwrap())]),
            true,
            tmp.path(),
        )
        .unwrap_err();
        let err = crate::ui::flatten(&err);
        assert!(err.contains("never-mounted/id"), "{err}");
        assert!(err.contains("ULAK_SSH_KEY_FILE"), "{err}");
    }

    #[test]
    fn every_connection_carries_the_link_options() {
        // ConnectTimeout is the measured 150s → 10.1s difference on an
        // unreachable server; the keepalives are what let the MASTER
        // notice a link that died mid-command. Both transports carry
        // both, or one of them hangs where the other does not.
        let ssh = fake();
        let args = ssh.multiplex_args().join(" ");
        let transport = ssh.rsync_transport();
        for opt in [
            "ConnectTimeout=10",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=4",
        ] {
            assert!(args.contains(opt), "ssh args lost {opt}: {args}");
            assert!(
                transport.contains(opt),
                "the rsync transport lost {opt}: {transport}"
            );
        }
    }

    #[test]
    fn the_unmultiplexed_retry_is_not_bare() {
        // The retry path is the one taken when the shared connection is
        // already suspect — it used to go out with no ConnectTimeout at
        // all, so the fallback could hang longer than the thing it was
        // falling back from.
        let argv: Vec<String> = fake()
            .direct_command()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(argv.contains(&"ConnectTimeout=10".to_string()), "{argv:?}");
        assert!(
            argv.contains(&"ServerAliveInterval=15".to_string()),
            "{argv:?}"
        );
        // …and it really is unmultiplexed, or retrying after `-O exit`
        // would just recreate the master we just retired.
        assert!(argv.contains(&"none".to_string()), "{argv:?}");
        assert!(argv.contains(&"ControlMaster=no".to_string()), "{argv:?}");
    }

    #[test]
    fn a_swept_runtime_directory_does_not_end_the_service() {
        // Measured against a real server: with the control directory
        // gone, ssh prints `unix_listener: cannot bind to path …` and
        // FAILS — it does not quietly drop multiplexing. A process meant
        // to run for weeks cannot rely on a directory it created once at
        // startup, so the path is re-established every time it is asked
        // for.
        let dir = std::env::temp_dir().join(format!("ulak-ctl-{}", std::process::id()));
        let ssh = Ssh {
            dest: "example".into(),
            control_dir: dir.clone(),
            identity: Vec::new(),
        };
        let _ = ssh.control_path();
        assert!(dir.is_dir());

        std::fs::remove_dir_all(&dir).unwrap();
        let path = ssh.control_path();
        assert!(
            dir.is_dir(),
            "the control directory must come back, or every later ssh fails"
        );
        assert!(path.starts_with(&dir.display().to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quoting_covers_shell_metacharacters() {
        assert_eq!(sh_quote("simple-path_1.txt"), "simple-path_1.txt");
        assert_eq!(sh_quote("has space"), "'has space'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote("$HOME;rm -rf"), "'$HOME;rm -rf'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn only_a_leading_tilde_slash_is_left_for_the_server_to_expand() {
        // The shape that has to keep expanding: `-v ~/data:/app` is how
        // a user names a directory in the SERVER's home, and both
        // `runspec` and `composepaths` classify it as server-side and
        // leave the argv alone on the understanding that it does.
        assert_eq!(sh_quote("~/data:/app"), "~/data:/app");
        assert_eq!(sh_quote("~/x"), "~/x");

        // The shapes that must not. A bare tilde reached a container's
        // `ls` and listed the ssh user's home on the server instead.
        assert_eq!(sh_quote("~"), "'~'");
        assert_eq!(sh_quote("~root/x"), "'~root/x'");
        assert_eq!(sh_quote("a~b"), "'a~b'");
        // Only the leading position expands, so this was never at risk —
        // it is quoted now because the body is not otherwise safe.
        assert_eq!(sh_quote("~/a b"), "'~/a b'");
    }
}
