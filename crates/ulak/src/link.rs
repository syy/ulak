//! The link: one ssh connection per server, and every command a channel
//! inside it rather than a session of its own.
//!
//! Through the shared master (`ssh.rs`) every remote command used to be
//! an ssh SESSION, and sessions are what sshd rations: `MaxSessions`, 10 by
//! default, per connection. Measured from OpenSSH_10.3p1 against
//! demo-server-new (OpenSSH_9.6p1 Ubuntu-3ubuntu13.20, MaxSessions 10), 13
//! concurrent sessions on one master: 10 ran, and 3 printed
//! `mux_client_request_session: session request failed: Session open
//! refused by peer` and `ControlSocket … already exists, disabling
//! multiplexing` into the command's own stderr and paid for a whole new
//! connection each (+0.7 s). The service's observers and stream channels
//! and every `logs -f` hold a session for as long as they run, so a few
//! parallel `docker` calls from a project's scripts reached that wall.
//!
//! A session also outlives the caller that asked for it. A client killed
//! mid-command leaves its remote command running: sshd closes the channel
//! and sends the command nothing, so it runs until it next writes, or for
//! good if it never does. A client whose caller is killed instead never
//! notices, and holds the caller's pipe open for as long as the command
//! runs. Same versions and server, `echo $$; exec sleep 300`: alive after
//! its client was killed, alive after its client's parent was, and in the
//! second case the pipe was still open 8 s later.
//!
//! A forwarded unix socket is not a session. `ssh -O forward -L` asks the
//! master to listen on a local socket and to open one `direct-streamlocal`
//! channel per connection to it, which MaxSessions does not count. Same
//! server: 40 of 40 concurrent channels ran; a new channel and its first
//! answer took 107 ms where `ssh -S <master> host true` took 120–140 ms
//! (RTT 53 ms); a client killed with SIGKILL was seen gone by the far end
//! 0.1 s later.
//!
//! What listens on the far socket is `link.py`, one helper per connection
//! and helper version, started through the master: its commands have the
//! environment, groups and forwarded agent of that connection's login, and
//! a new connection starts a new one, as it would get new sessions. A
//! helper takes new channels for the master's ControlPersist, or until a
//! login would get other groups than it has, and then leaves them to the
//! next one, which the next command starts. Each channel carries one
//! command: the helper runs it under `/bin/sh -c` in its own process group,
//! with the environment and directory a session would have given it when
//! the helper started, and frames its stdout, stderr and exit status apart.
//! A channel that closes before the command ends is a caller that went
//! away, and the command's group is hung up; the client closes it when its
//! own caller dies, which ssh never noticed.
//!
//! The client is this binary, `ulak __link`, addressed exactly like ssh:
//! `Ssh::command` hands it the arguments ssh would have had, so every
//! caller keeps its `Command`, its pipes and `proc::run_bounded`. Whatever
//! it cannot carry — an option it does not know, no remote command, a
//! server without python3 or without unix socket forwarding — it hands to
//! the real ssh by `exec`, arguments untouched, which is the behaviour
//! every caller had before. A server that cannot have the link is
//! remembered for `OFF_FOR`, so later commands do not pay to find out.
//!
//! With no master yet, the session that starts the helper is the login
//! that makes one: ssh's prompts and messages reach this terminal as they
//! would have, on ssh's own clock, and the command then goes over the link
//! like any other. The first command after an idle ControlPersist used to
//! be ssh's instead, and a session again. Measured, same versions and
//! server, a fresh control path each time: its remote command outlived a
//! killed client, and a killed parent left the pipe open as well; sixteen
//! first commands at once were sixteen logins, and sshd (MaxStartups
//! 10:30:100) reset 3 of them with `kex_exchange_identification: read:
//! Connection reset by peer`. Behind the queue they are one login.
//!
//! The request goes out only after the helper's greeting. Until then
//! nothing can have run, which is what makes handing a command to ssh safe;
//! after it there is no second attempt, because the command may already be
//! running — a channel lost then is a 255, as a connection lost under ssh
//! is.
//!
//! Where the link answers differently from ssh, it answers as a shell
//! would. A command killed by a signal exits 128 + n by this machine's
//! number for the signal's name; ssh exits 255, which `run_script` takes
//! for a lost connection and runs the script again. A `-tt` pty with no
//! terminal here is 80×24, not ssh's 0×0. The first TERM, INT or HUP this
//! process gets goes to the command's group, which ends with its own
//! answer; the second ends the channel. Copies that arrive together are one
//! signal: one sent to a whole process group (`timeout`, a CI runner
//! cancelling a job) reaches the caller as well, and a caller that passes
//! it on sends it again. Nor does a caller that dies of it cut the
//! command's own ending short: `compose up` stops its containers on TERM,
//! and only after `ENDING` is the channel closed under it.
//!
//! What stays on ssh sessions: rsync (its transport string is ssh's, its
//! throughput and exit codes its own contract), the tunnels (`forward.rs`,
//! unmultiplexed by design) and `run_script`'s direct retry.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, ErrorKind, IsTerminal, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;

use crate::proc;
use crate::ssh::sh_quote;
use crate::ui::{self, fail};

const HELPER: &str = include_str!("link.py");

/// The hidden subcommand `Ssh::command` runs in ssh's place.
pub const SUBCOMMAND: &str = "__link";

/// The protocol `link.py` speaks (its `VERSION`).
const PROTOCOL: u64 = 1;

/// How long the helper's greeting may take. One round trip on a working
/// link; a master whose TCP died under a sleeping laptop accepts the local
/// connection and then says nothing at all.
const GREETING: Duration = Duration::from_secs(10);

/// Starting the helper: one session through a master that is already up,
/// and python's start.
const START: Duration = Duration::from_secs(30);

/// How long the login's stdout may stay open once its ssh has exited. The
/// master closes its copy as the session ends; this bounds one that did not.
const SPOKEN: Duration = Duration::from_secs(5);

/// How long a server that cannot have the link is not asked again.
const OFF_FOR: Duration = Duration::from_secs(600);

/// How long a command waits for another one that is bringing the link up
/// before it goes to ssh instead.
const QUEUE: Duration = Duration::from_secs(45);

/// How long a command that was passed a signal may take to end once its
/// caller has gone: Docker's own stop timeout.
const ENDING: Duration = Duration::from_secs(10);

/// How close together two signals are one signal delivered twice.
const ECHO: Duration = Duration::from_millis(500);

const CHUNK: usize = 1 << 16;
const MAX_FRAME: u32 = 1 << 24;

const HELLO: u8 = b'H';
const OPEN: u8 = b'O';
const STDIN: u8 = b'0';
const STDOUT: u8 = b'1';
const STDERR: u8 = b'2';
const SIGNAL: u8 = b'K';
const WINDOW: u8 = b'W';
const EXIT: u8 = b'X';
const FAILED: u8 = b'F';

/// The program `Ssh::command` runs: this binary, while the file it was
/// started from is still the one on disk. A service outlives upgrades,
/// and the build that replaced it there may not be this one — a downgrade
/// may not know `__link` at all, and every command would fail on it. Once
/// the file has changed, Linux still runs this build through
/// `/proc/self/exe`; elsewhere the answer is None, and with it plain ssh.
pub fn program() -> Option<PathBuf> {
    if cfg!(test) {
        // A unit test's own binary is the test harness, not ulak.
        return None;
    }
    static STARTED: std::sync::OnceLock<Option<(PathBuf, Stamp)>> = std::sync::OnceLock::new();
    let (exe, stamp) = STARTED
        .get_or_init(|| {
            let exe = std::env::current_exe().ok()?;
            let stamp = stamp_of(&exe)?;
            Some((exe, stamp))
        })
        .as_ref()?;
    if stamp_of(exe).as_ref() == Some(stamp) {
        Some(exe.clone())
    } else if cfg!(target_os = "linux") {
        Some(PathBuf::from("/proc/self/exe"))
    } else {
        None
    }
}

/// Which file a path names, and which build of it: device, inode and
/// modification time.
type Stamp = (u64, u64, i64, i64);

fn stamp_of(path: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some((m.dev(), m.ino(), m.mtime(), m.mtime_nsec()))
}

/// A digest of the helper's source. It names the helper's socket on the
/// server and the forward on this machine, so two builds never share one.
fn version() -> String {
    crate::hashid::fnv1a128_hex(HELPER.as_bytes())[..16].to_string()
}

// ─── frames ─────────────────────────────────────────────────────────────

fn write_frame(out: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let size = u32::try_from(payload.len())
        .ok()
        .filter(|size| *size <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "a frame too large to send"))?;
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(kind);
    frame.extend_from_slice(&size.to_be_bytes());
    frame.extend_from_slice(payload);
    out.write_all(&frame)
}

/// One frame, or None at a clean end between frames. An end inside one
/// is an error: the link broke, it did not finish.
fn read_frame(input: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    let mut got = 0;
    while got < head.len() {
        match input.read(&mut head[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let size = u32::from_be_bytes([head[1], head[2], head[3], head[4]]);
    if size > MAX_FRAME {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "a frame larger than the link allows",
        ));
    }
    let mut payload = vec![0u8; size as usize];
    input.read_exact(&mut payload)?;
    Ok(Some((head[0], payload)))
}

// ─── what ssh was asked ─────────────────────────────────────────────────

/// ssh's options that take a value (ssh(1), OpenSSH 10.3p1), and the ones
/// the link carries. Everything else is ssh's alone, and a command that
/// uses it goes to ssh.
const WITH_VALUE: &[u8] = b"BbcDEeFIiJLlmOoPpQRSWw";
const CARRIED_VALUE: &[u8] = b"BbcFIiJlmoPpS";
const CARRIED_FLAG: &[u8] = b"46aCkqTtnxy";

/// `-o` keywords that shape the remote SESSION or add to the connection,
/// neither of which a channel has. A caller that sets one wanted ssh.
const SESSION_KEYWORDS: &[&str] = &[
    "dynamicforward",
    "forkafterauthentication",
    "forwardagent",
    "forwardx11",
    "forwardx11trusted",
    "localforward",
    "remotecommand",
    "remoteforward",
    "requesttty",
    "sendenv",
    "sessiontype",
    "setenv",
    "stdinnull",
    "tunnel",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tty {
    /// `-T`, or no `-t`.
    No,
    /// `-t`: a pty when this side's stdin is a terminal.
    Ask,
    /// `-tt`: a pty whatever stdin is.
    Force,
}

/// One ssh invocation the link can carry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    /// The connection options as given, without the ones that shape only
    /// this command (`-t`, `-T`, `-n`): what the link's own sessions use.
    connection: Vec<String>,
    control: String,
    dest: String,
    /// The remote words, joined with spaces as ssh joins them.
    line: String,
    tty: Tty,
    stdin_null: bool,
}

impl Plan {
    /// None: ssh's own job.
    fn of(args: &[String]) -> Option<Plan> {
        let mut connection = Vec::new();
        let mut control = None;
        let mut master = None;
        let mut tty = Tty::No;
        let mut stdin_null = false;
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg == "--" {
                i += 1;
                break;
            }
            let Some(letters) = arg.strip_prefix('-').filter(|l| !l.is_empty()) else {
                break;
            };
            for (at, letter) in letters.bytes().enumerate() {
                if WITH_VALUE.contains(&letter) {
                    if !CARRIED_VALUE.contains(&letter) {
                        return None;
                    }
                    // `-p2222` and `-p 2222` alike.
                    let value = if at + 1 < letters.len() {
                        letters[at + 1..].to_string()
                    } else {
                        i += 1;
                        args.get(i)?.clone()
                    };
                    match letter {
                        b'o' => {
                            let (key, val) = option_pair(&value);
                            if SESSION_KEYWORDS.contains(&key.as_str()) {
                                return None;
                            }
                            match key.as_str() {
                                "controlpath" => control = Some(val),
                                "controlmaster" => master = Some(val.to_ascii_lowercase()),
                                _ => {}
                            }
                        }
                        b'S' => control = Some(value.clone()),
                        _ => {}
                    }
                    connection.push(format!("-{}", letter as char));
                    connection.push(value);
                    break;
                }
                if !CARRIED_FLAG.contains(&letter) {
                    return None;
                }
                // ssh's own rules: the last of -t/-T wins, and a second -t
                // forces.
                match letter {
                    b't' => {
                        tty = if tty == Tty::Ask {
                            Tty::Force
                        } else {
                            Tty::Ask
                        }
                    }
                    b'T' => tty = Tty::No,
                    b'n' => stdin_null = true,
                    other => connection.push(format!("-{}", other as char)),
                }
            }
            i += 1;
        }
        let dest = args.get(i)?.clone();
        let words = &args[i + 1..];
        if words.is_empty() || dest.starts_with('-') {
            return None;
        }
        // The link lives beside a shared master; without one it has no
        // home, and a connection that refused one wanted ssh's own.
        let control = control.filter(|c| !c.is_empty() && c != "none")?;
        if master.as_deref() == Some("no") {
            return None;
        }
        Some(Plan {
            connection,
            control,
            dest,
            line: words.join(" "),
            tty,
            stdin_null,
        })
    }
}

/// Whether the link would carry this ssh argv, for `ssh.rs`'s tests: what
/// `Ssh` builds and what the link knows are two lists that must agree.
#[cfg(test)]
pub(crate) fn carries(args: &[String]) -> bool {
    Plan::of(args).is_some()
}

/// `Key=Value` or `Key Value`, the keyword folded as ssh folds it.
fn option_pair(value: &str) -> (String, String) {
    let (key, val) = match value.find(['=', ' ', '\t']) {
        Some(at) => (
            &value[..at],
            value[at + 1..].trim_start_matches(['=', ' ', '\t']),
        ),
        None => (value, ""),
    };
    (key.to_ascii_lowercase(), val.to_string())
}

// ─── where the link lives on this machine ───────────────────────────────

/// The local end and its bookkeeping, beside the master's own socket.
struct Place {
    /// What the master listens on for this helper version.
    socket: PathBuf,
    /// Queues the commands that would bring the link up at once.
    lock: PathBuf,
    /// Why the link cannot be had on this server; heeded while younger
    /// than `OFF_FOR`.
    off: PathBuf,
}

impl Place {
    fn of(control: &str) -> Place {
        let base = format!("{control}-l{}", &version()[..8]);
        Place {
            socket: PathBuf::from(&base),
            lock: PathBuf::from(format!("{base}.lock")),
            off: PathBuf::from(format!("{base}.off")),
        }
    }

    fn off_reason(&self) -> Option<String> {
        let age = std::fs::metadata(&self.off)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())?;
        (age < OFF_FOR).then(|| std::fs::read_to_string(&self.off).unwrap_or_default())
    }

    /// Read by every other command to this server, so never half written.
    fn turn_off(&self, why: &str) {
        let _ = crate::ledger::write_atomic(&self.off, why.as_bytes());
    }
}

/// Whether commands through the master at `control` travel over the link
/// (`Ok`), are kept off it for now and why (`Err`), or neither is known
/// yet. For `doctor`: the link hands commands to ssh in silence, so this
/// is where a user can see that it did.
pub fn state(control: &str) -> Option<Result<(), String>> {
    let place = Place::of(control);
    if let Some(why) = place.off_reason() {
        return Some(Err(why));
    }
    match greet(&place.socket, GREETING) {
        Ok(_) | Err(Fault::Full) => Some(Ok(())),
        Err(_) => None,
    }
}

// ─── reaching the helper ────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum Fault {
    /// Nothing listens locally: the master has no forward, or is gone.
    Absent,
    /// The master closed the channel before the helper spoke: no helper
    /// there, or a server that refuses to forward to a unix socket.
    Refused,
    /// Connected, and nothing came back in time.
    Silent,
    /// Something answered that is not this helper.
    Garbled,
    /// The helper answered, and already runs all the commands it takes.
    Full,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Hello {
    v: u64,
    full: bool,
}

fn greet(socket: &Path, wait: Duration) -> Result<UnixStream, Fault> {
    let mut stream = UnixStream::connect(socket).map_err(|_| Fault::Absent)?;
    stream
        .set_read_timeout(Some(wait))
        .map_err(|_| Fault::Absent)?;
    match read_frame(&mut stream) {
        Ok(Some((HELLO, payload))) => {
            let hello: Hello = serde_json::from_slice(&payload).map_err(|_| Fault::Garbled)?;
            if hello.v != PROTOCOL {
                return Err(Fault::Garbled);
            }
            if hello.full {
                return Err(Fault::Full);
            }
            stream.set_read_timeout(None).map_err(|_| Fault::Absent)?;
            Ok(stream)
        }
        Ok(Some(_)) => Err(Fault::Garbled),
        Ok(None) => Err(Fault::Refused),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            Err(Fault::Silent)
        }
        Err(_) => Err(Fault::Refused),
    }
}

/// Where one command goes.
enum Route {
    /// Over the link, on this greeted channel.
    Link(UnixStream),
    /// To ssh, as every command went before the link.
    Ssh,
    /// Nowhere: ssh could not log in and has said why. Its 255 is the
    /// answer, and a second ssh would only ask again.
    LoggedOut,
    /// Nowhere: the caller left while this command waited its turn, and
    /// nobody reads its answer.
    Gone,
}

fn connect(plan: &Plan, parent: u32) -> Route {
    let place = Place::of(&plan.control);
    if place.off_reason().is_some() {
        return Route::Ssh;
    }
    match greet(&place.socket, GREETING) {
        Ok(stream) => return Route::Link(stream),
        Err(Fault::Garbled) => {
            place.turn_off("something other than Ulak's link helper answers its socket");
            return Route::Ssh;
        }
        // Not refused before anything ran, as ssh's MaxSessions refuses: the
        // link keeps its limit by handing the command to ssh.
        Err(Fault::Full) => return Route::Ssh,
        Err(Fault::Silent) => {
            if !master_answers(plan) {
                // The master is the suspect, as in `run_script`: retired,
                // and the link logs in afresh.
                retire(plan);
            }
        }
        Err(Fault::Absent | Fault::Refused) => {}
    }
    bring_up(plan, &place, parent)
}

/// Start the helper and the forward, once, for every command waiting.
fn bring_up(plan: &Plan, place: &Place, parent: u32) -> Route {
    let Some(_queue) = Queue::join(&place.lock, parent) else {
        return if caller_gone(parent) {
            Route::Gone
        } else {
            Route::Ssh
        };
    };
    // Whoever held the queue may just have done all of this.
    let fault = match greet(&place.socket, GREETING) {
        Ok(stream) => return Route::Link(stream),
        Err(fault) => fault,
    };
    if matches!(fault, Fault::Garbled | Fault::Full) || place.off_reason().is_some() {
        return Route::Ssh;
    }
    // A forward the master already has leads to the same path: the
    // helper's name is its version and its connection, and its directory
    // the account's.
    let mut forwarded = false;
    // Twice at most. The helper `start` found answering may give its
    // socket up before the first channel reaches it (its life ended, or its
    // groups went stale), and the master then closes the channel unanswered
    // exactly as a server that refuses the forward does. Only a helper that
    // was just started refusing too is the server's answer.
    for _ in 0..2 {
        let remote = match start_helper(plan, place) {
            Ok(remote) => remote,
            Err(route) => return route,
        };
        if fault == Fault::Absent && !forwarded {
            // A socket file nobody listens on is what a master that exited
            // leaves behind; the new one cannot bind over it.
            if UnixStream::connect(&place.socket)
                .is_err_and(|e| e.kind() == ErrorKind::ConnectionRefused)
            {
                let _ = std::fs::remove_file(&place.socket);
            }
            if forward(plan, place, &remote).is_none() {
                return Route::Ssh;
            }
            forwarded = true;
        }
        match greet(&place.socket, GREETING) {
            Ok(stream) => return Route::Link(stream),
            Err(Fault::Refused) => {}
            Err(_) => return Route::Ssh,
        }
    }
    place.turn_off(if forwarded {
        "the server refuses to forward to a unix socket (sshd_config: AllowStreamLocalForwarding)"
    } else {
        "the shared connection's forward leads to no link helper"
    });
    Route::Ssh
}

/// Whether the process that started this one has gone. Its pid was taken
/// when this one started; 1 is no caller worth watching.
fn caller_gone(parent: u32) -> bool {
    parent != 1 && std::os::unix::process::parent_id() != parent
}

/// The file lock that queues commands bringing the link up, held for as
/// long as the value lives. None after `QUEUE`: something is stuck, and
/// this command goes to ssh rather than wait behind it. None as well once
/// the caller has gone: the login ahead may be a passphrase nobody types.
struct Queue(File);

impl Queue {
    fn join(path: &Path, parent: u32) -> Option<Queue> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)
            .ok()?;
        let until = Instant::now() + QUEUE;
        loop {
            match file.try_lock() {
                Ok(()) => return Some(Queue(file)),
                Err(std::fs::TryLockError::WouldBlock)
                    if Instant::now() < until && !caller_gone(parent) =>
                {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => return None,
            }
        }
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// One session through the master: start the helper unless one of this
/// version already answers, and learn where its socket is. With no master
/// yet, this session is the login that makes it.
fn start_helper(plan: &Plan, place: &Place) -> Result<String, Route> {
    let mut cmd = Command::new("ssh");
    cmd.args(&plan.connection)
        .arg("--")
        .arg(&plan.dest)
        .arg(start_line());
    let master = UnixStream::connect(&plan.control).is_ok();
    let started = if master {
        proc::run_bounded(&mut cmd, Some(HELPER.as_bytes().to_vec()), START)
            .ok()
            .map(|out| (out.stdout, out.status.code(), out.timed_out))
    } else {
        log_in(&mut cmd, &plan.control)
    };
    let Some((stdout, code, timed_out)) = started else {
        return Err(Route::Ssh);
    };
    crate::audit::record_command_in("link", "ssh-link", &cmd, code);
    let logged_in = master || UnixStream::connect(&plan.control).is_ok();
    match helper_answer(&stdout, code, timed_out, logged_in) {
        Started::Ready(path) => Ok(path),
        Started::Never(why) => {
            place.turn_off(&why);
            Err(Route::Ssh)
        }
        Started::NotNow => Err(Route::Ssh),
        Started::LoggedOut => Err(Route::LoggedOut),
    }
}

/// The helper's start as the first connection to the server. Its stderr is
/// this process's, so ssh's messages read as they would have; its clock is
/// ssh's own until the master is up — a passphrase takes as long as the
/// person typing it — and `START` from then on.
fn log_in(cmd: &mut Command, control: &str) -> Option<(Vec<u8>, Option<i32>, bool)> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().ok()?;
    let mut sink = child.stdin.take()?;
    std::thread::spawn(move || {
        let _ = sink.write_all(HELPER.as_bytes());
    });
    let mut pipe = child.stdout.take()?;
    let (tx, said) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = pipe.read_to_end(&mut out);
        let _ = tx.send(out);
    });
    let status = loop {
        if let Some(status) = proc::wait_for(&mut child, Duration::from_millis(100)).ok()? {
            break Some(status);
        }
        if UnixStream::connect(control).is_ok() {
            break proc::wait_for(&mut child, START).ok()?;
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        return Some((Vec::new(), None, true));
    };
    let stdout = said.recv_timeout(SPOKEN).unwrap_or_default();
    Some((stdout, status.code(), false))
}

fn start_line() -> String {
    format!(
        "python3 -c {} start link-{} {}",
        sh_quote(&format!(
            "import sys;exec(sys.stdin.buffer.read({}))",
            HELPER.len()
        )),
        version(),
        crate::ssh::CONTROL_PERSIST,
    )
}

#[derive(Debug, PartialEq, Eq)]
enum Started {
    Ready(String),
    /// This server cannot have the link; remembered for `OFF_FOR`.
    Never(String),
    /// Not this time; the next command asks again.
    NotNow,
    /// ssh could not log in, and there is still no master.
    LoggedOut,
}

/// `logged_in`: a master was there before the start or is there after it.
fn helper_answer(stdout: &[u8], code: Option<i32>, timed_out: bool, logged_in: bool) -> Started {
    for line in stdout.split(|b| *b == b'\n') {
        if let Some(path) = line.strip_prefix(b"ulak-link ready ") {
            let Ok(path) = std::str::from_utf8(path) else {
                return Started::Never("the helper's socket path is not UTF-8".into());
            };
            // `-L local:remote` splits on the colon.
            if !path.starts_with('/') || path.contains([':', ' ', '\t']) {
                return Started::Never(format!(
                    "the helper's socket path cannot be forwarded: {path}"
                ));
            }
            return Started::Ready(path.to_string());
        }
        if let Some(why) = line.strip_prefix(b"ulak-link refused: ") {
            return Started::Never(String::from_utf8_lossy(why).into_owned());
        }
    }
    if timed_out {
        return Started::NotNow;
    }
    match code {
        // The remote shell's "command not found".
        Some(127) => Started::Never("the server has no python3".into()),
        // ssh's own failure, or a signal's: before a login, the command's
        // answer; after one, nothing is known about the server yet.
        Some(255) | None if !logged_in => Started::LoggedOut,
        Some(255) | None => Started::NotNow,
        // python3 ran and did not get as far as an answer: too old, or
        // something in its way that the next command would meet again.
        Some(_) => Started::Never("the server's python3 could not start the link helper".into()),
    }
}

/// Ask the master to listen for the helper's channels.
fn forward(plan: &Plan, place: &Place, remote: &str) -> Option<()> {
    let local = place.socket.to_str()?;
    if local.contains(':') {
        place.turn_off("the ssh control directory's path has a colon, which -L cannot carry");
        return None;
    }
    let mut cmd = Command::new("ssh");
    cmd.arg("-o")
        .arg(format!("ControlPath={}", plan.control))
        .args(["-O", "forward", "-L"])
        .arg(format!("{local}:{remote}"))
        .arg("--")
        .arg(&plan.dest);
    let out = proc::run_bounded(&mut cmd, None, proc::PROBE).ok()?;
    crate::audit::record_command_in("link", "ssh-link", &cmd, out.status.code());
    if out.timed_out || out.status.success() {
        return (!out.timed_out).then_some(());
    }
    // The master answered, and no: `mux_client_forward: forwarding request
    // failed: Port forwarding failed` (OpenSSH_10.3p1, a local path already
    // bound). A master that is gone says `Control socket connect(…)`, and
    // the next one may well say yes.
    let said = String::from_utf8_lossy(&out.stderr);
    if said.contains("forwarding request failed") {
        place.turn_off(&format!(
            "the shared connection refused the forward: {}",
            said.lines().next().unwrap_or_default()
        ));
    }
    None
}

/// Whether the master still carries a session. A helper gone silent
/// behind a master that answers is the helper's fault, and the next start
/// replaces it. On `run_script`'s clock, for `run_script`'s verdict: a
/// master that is only slow — a large transfer ahead of the greeting on a
/// thin uplink — is not retired with every command it carries.
fn master_answers(plan: &Plan) -> bool {
    let mut cmd = Command::new("ssh");
    cmd.args(&plan.connection)
        .arg("--")
        .arg(&plan.dest)
        .arg("true");
    let code = proc::run_bounded(&mut cmd, None, proc::SCRIPT)
        .ok()
        .and_then(|out| (!out.timed_out).then_some(out.status.code()).flatten());
    crate::audit::record_command_in("link", "ssh-link", &cmd, code);
    code == Some(0)
}

fn retire(plan: &Plan) {
    let mut cmd = Command::new("ssh");
    cmd.arg("-o")
        .arg(format!("ControlPath={}", plan.control))
        .args(["-O", "exit", "--"])
        .arg(&plan.dest);
    let code = proc::run_bounded(&mut cmd, None, proc::PROBE)
        .ok()
        .and_then(|b| b.status.code());
    crate::audit::record_command_in("link", "ssh-link", &cmd, code);
}

// ─── one command over a channel ─────────────────────────────────────────

/// How one command ended, as this process should report it.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
    /// The remote command's own exit code.
    Code(u8),
    /// Killed on the server by the named signal (and its number there).
    Signal(String, i32),
    /// The channel ended before the command did.
    Lost,
    /// The helper could not start it.
    Failed(String),
    /// The caller stopped reading our stdout.
    Unread,
}

impl Ending {
    fn code(&self) -> u8 {
        match self {
            Ending::Code(code) => *code,
            // A shell's 128 + n, with n THIS machine's number for the
            // name: SIGUSR1 is 10 on Linux and 30 on macOS.
            Ending::Signal(name, there) => {
                crate::docker::signal_code(signal_number(name).unwrap_or(*there))
            }
            Ending::Lost => 255,
            // A shell's "cannot execute". Not ssh's 255: nothing was lost,
            // and `run_script` would retire the master and every command
            // on it for a server out of processes.
            Ending::Failed(_) => 126,
            Ending::Unread => crate::docker::signal_code(libc::SIGPIPE),
        }
    }
}

fn signal_number(name: &str) -> Option<i32> {
    let number = match name {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "ILL" => libc::SIGILL,
        "TRAP" => libc::SIGTRAP,
        "ABRT" => libc::SIGABRT,
        "BUS" => libc::SIGBUS,
        "FPE" => libc::SIGFPE,
        "KILL" => libc::SIGKILL,
        "USR1" => libc::SIGUSR1,
        "SEGV" => libc::SIGSEGV,
        "USR2" => libc::SIGUSR2,
        "PIPE" => libc::SIGPIPE,
        "ALRM" => libc::SIGALRM,
        "TERM" => libc::SIGTERM,
        "XCPU" => libc::SIGXCPU,
        "XFSZ" => libc::SIGXFSZ,
        "VTALRM" => libc::SIGVTALRM,
        "PROF" => libc::SIGPROF,
        "SYS" => libc::SIGSYS,
        _ => return None,
    };
    Some(number)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Exit {
    code: Option<i64>,
    signal: Option<String>,
    number: i32,
}

/// The request, as the helper reads it: a pty is rows, columns and TERM.
fn request(line: &str, pty: Option<(u16, u16, String)>) -> Vec<u8> {
    let mut value = serde_json::json!({ "sh": line });
    if let Some((rows, cols, term)) = pty {
        value["pty"] = serde_json::json!({ "rows": rows, "cols": cols, "term": term });
    }
    value.to_string().into_bytes()
}

/// The writing half of a channel, shared by every thread that speaks.
#[derive(Clone)]
struct Speaker {
    stream: Arc<Mutex<UnixStream>>,
    /// The same socket, outside the lock: a hangup must not wait behind
    /// a stdin copy blocked on a command that reads nothing.
    plug: Arc<UnixStream>,
}

impl Speaker {
    fn new(stream: &UnixStream) -> io::Result<Speaker> {
        Ok(Speaker {
            stream: Arc::new(Mutex::new(stream.try_clone()?)),
            plug: Arc::new(stream.try_clone()?),
        })
    }

    fn say(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let mut stream = self.stream.lock().unwrap_or_else(PoisonError::into_inner);
        write_frame(&mut *stream, kind, payload)
    }

    /// End the channel from this side; the helper hangs the command up.
    fn hang_up(&self) {
        let _ = self.plug.shutdown(std::net::Shutdown::Both);
    }
}

/// Copy stdin into the channel until it ends, then say so.
fn feed(mut input: impl Read, speaker: &Speaker) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if speaker.say(STDIN, &buf[..n]).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = speaker.say(STDIN, &[]);
}

/// Read the channel until the command's end, writing its two streams.
fn drain(stream: &mut UnixStream, out: &mut impl Write, err: &mut impl Write) -> Ending {
    loop {
        match read_frame(stream) {
            Ok(Some((STDOUT, bytes))) => {
                if out.write_all(&bytes).and_then(|()| out.flush()).is_err() {
                    return Ending::Unread;
                }
            }
            Ok(Some((STDERR, bytes))) => {
                let _ = err.write_all(&bytes).and_then(|()| err.flush());
            }
            Ok(Some((EXIT, payload))) => {
                let exit: Exit = serde_json::from_slice(&payload).unwrap_or_default();
                return match (exit.code, exit.signal) {
                    (Some(code), _) => Ending::Code(u8::try_from(code).unwrap_or(255)),
                    (None, Some(name)) => Ending::Signal(name, exit.number),
                    (None, None) => Ending::Lost,
                };
            }
            Ok(Some((FAILED, payload))) => {
                return Ending::Failed(String::from_utf8_lossy(&payload).into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return Ending::Lost,
        }
    }
}

/// Run one request on a greeted channel with these streams. The stdin
/// copy runs on its own thread and is left behind when the command ends,
/// as ssh leaves a terminal it was reading.
fn exchange(
    stream: UnixStream,
    line: &str,
    pty: Option<(u16, u16, String)>,
    input: Option<Box<dyn Read + Send>>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> io::Result<(Ending, Speaker)> {
    let speaker = Speaker::new(&stream)?;
    let mut reader = stream;
    speaker.say(OPEN, &request(line, pty))?;
    match input {
        Some(input) => {
            let speaker = speaker.clone();
            std::thread::spawn(move || feed(input, &speaker));
        }
        None => speaker.say(STDIN, &[])?,
    }
    let ending = drain(&mut reader, out, err);
    if ending == Ending::Unread {
        // Nobody reads the rest; the command is not left to fill a pipe.
        speaker.hang_up();
    }
    Ok((ending, speaker))
}

// ─── the client process ─────────────────────────────────────────────────

/// `ulak __link <ssh arguments>`: the command over the link when it can
/// be carried, ssh itself otherwise.
pub fn run(argv: Vec<OsString>) -> ExitCode {
    // Taken before a login that may take a while: a caller that leaves
    // during it is gone by the time the command would start.
    let parent = std::os::unix::process::parent_id();
    let plan = argv
        .iter()
        .map(|a| a.to_str().map(str::to_owned))
        .collect::<Option<Vec<String>>>()
        .and_then(|args| Plan::of(&args));
    let Some(plan) = plan else {
        return ssh_instead(&argv);
    };
    match connect(&plan, parent) {
        Route::Link(stream) => carry(stream, &plan, parent),
        Route::Ssh => ssh_instead(&argv),
        Route::LoggedOut | Route::Gone => ExitCode::from(255),
    }
}

/// Become ssh, with exactly the arguments this process was given.
fn ssh_instead(argv: &[OsString]) -> ExitCode {
    let err = Command::new("ssh").args(argv).exec();
    ui::render_error(
        &fail!("cannot start ssh: {err}")
            .now("check that OpenSSH's ssh is on PATH")
            .into_err(),
    );
    ExitCode::from(255)
}

fn carry(stream: UnixStream, plan: &Plan, parent: u32) -> ExitCode {
    let terminal = std::io::stdin().is_terminal();
    let pty = match plan.tty {
        Tty::Force => true,
        Tty::Ask => terminal,
        Tty::No => false,
    };
    let pty_request = pty.then(|| {
        let (cols, rows) = window().unwrap_or((80, 24));
        let term = std::env::var("TERM").unwrap_or_else(|_| "xterm".into());
        (rows, cols, term)
    });
    let (Ok(mut out), Ok(mut err)) = (
        dup(std::io::stdout().as_fd()),
        dup(std::io::stderr().as_fd()),
    ) else {
        return ExitCode::from(255);
    };
    let input: Option<Box<dyn Read + Send>> = if plan.stdin_null {
        None
    } else {
        dup(std::io::stdin().as_fd())
            .ok()
            .map(|file| Box::new(file) as Box<dyn Read + Send>)
    };
    let raw = (pty && terminal).then(Raw::enter).flatten();
    let Ok(watch) = Watch::start(&stream, raw.is_some(), parent) else {
        return ExitCode::from(255);
    };
    let ending = exchange(stream, &plan.line, pty_request, input, &mut out, &mut err)
        .map_or(Ending::Lost, |(ending, _)| ending);
    let hung_by = watch.stop();
    drop(raw);
    match &ending {
        // We ended it: no surprise to report. A signal's 128 + n; a parent
        // that left reads nothing.
        Ending::Lost if hung_by > 0 => {
            return ExitCode::from(crate::docker::signal_code(hung_by));
        }
        Ending::Lost if hung_by != 0 => return ExitCode::from(255),
        Ending::Lost => ui::warn(&format!(
            "the connection to {} closed before the command finished",
            plan.dest
        )),
        Ending::Failed(why) => {
            ui::warn(&format!("{} could not start the command: {why}", plan.dest))
        }
        _ => {}
    }
    ExitCode::from(ending.code())
}

fn dup(fd: BorrowedFd<'_>) -> io::Result<File> {
    Ok(File::from(fd.try_clone_to_owned()?))
}

/// This terminal's size, columns first.
fn window() -> Option<(u16, u16)> {
    ratatui::crossterm::terminal::size().ok()
}

/// The local terminal in raw mode while a pty command runs, as ssh puts
/// it: keys travel as bytes, and the far pty decides what ^C means.
struct Raw;

impl Raw {
    fn enter() -> Option<Raw> {
        ratatui::crossterm::terminal::enable_raw_mode()
            .ok()
            .map(|()| Raw)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
    }
}

/// The threads that watch what surrounds a running command: signals, the
/// window, and the process that started this one.
struct Watch {
    stop: Arc<AtomicBool>,
    /// The signal number that ended the channel from this side, -1 for a
    /// parent that went away, 0 while neither has.
    hung_by: Arc<AtomicI32>,
    signals: signal_hook::iterator::Handle,
}

impl Watch {
    fn start(stream: &UnixStream, window_too: bool, parent: u32) -> io::Result<Watch> {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
        let speaker = Speaker::new(stream)?;
        let stop = Arc::new(AtomicBool::new(false));
        let hung_by = Arc::new(AtomicI32::new(0));
        // When the first signal went to the command, once one has.
        let passed: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let watched = Duration::from_millis(200);

        // The process that started this one went away (SIGKILL reaches
        // nobody else): end the channel, so the command and the caller's
        // pipe end with it rather than whenever the command next writes.
        if parent != 1 {
            let (stop, hung_by, speaker, passed) = (
                stop.clone(),
                hung_by.clone(),
                speaker.clone(),
                passed.clone(),
            );
            std::thread::spawn(move || {
                while !caller_gone(parent) {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(watched);
                }
                // One more look: a signal sent to the caller's whole group
                // reaches this process in the same instant as the caller,
                // and is counted before the hangup is decided. Short, since
                // a caller killed alone waits for it too.
                std::thread::sleep(Duration::from_millis(50));
                let passed_at = *passed.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(at) = passed_at {
                    while !stop.load(Ordering::Relaxed) && at.elapsed() < ENDING {
                        std::thread::sleep(watched);
                    }
                }
                if !stop.load(Ordering::Relaxed) {
                    hung_by.store(-1, Ordering::Relaxed);
                    speaker.hang_up();
                }
            });
        }

        let mut wanted = vec![SIGTERM, SIGINT, SIGHUP];
        if window_too {
            wanted.push(SIGWINCH);
        }
        let mut signals = signal_hook::iterator::Signals::new(&wanted)?;
        let handle = signals.handle();
        let hung = hung_by.clone();
        std::thread::spawn(move || {
            // The first TERM, INT or HUP goes to the command, which ends in
            // its own time and with its own answer; a second one ends the
            // channel, unless it is the first one delivered again.
            let mut first: Option<Instant> = None;
            for number in signals.forever() {
                let name = match number {
                    SIGWINCH => {
                        if let Some((cols, rows)) = window() {
                            let size = serde_json::json!({ "rows": rows, "cols": cols });
                            let _ = speaker.say(WINDOW, size.to_string().as_bytes());
                        }
                        continue;
                    }
                    SIGTERM => "TERM",
                    SIGINT => "INT",
                    _ => "HUP",
                };
                if first.is_some_and(|at| at.elapsed() < ECHO) {
                    continue;
                }
                if first.is_some() || speaker.say(SIGNAL, name.as_bytes()).is_err() {
                    hung.store(number, Ordering::Relaxed);
                    speaker.hang_up();
                    return;
                }
                let now = Instant::now();
                first = Some(now);
                *passed.lock().unwrap_or_else(PoisonError::into_inner) = Some(now);
            }
        });
        Ok(Watch {
            stop,
            hung_by,
            signals: handle,
        })
    }

    /// Let the threads go, and say whether this side ended the channel.
    fn stop(self) -> i32 {
        self.stop.store(true, Ordering::Relaxed);
        self.signals.close();
        self.hung_by.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    /// The arguments `Ssh::command` builds, as the link reads them.
    #[test]
    fn the_shared_masters_arguments_are_carried_over_the_link() {
        let mut args = words(
            "-o ControlMaster=auto -o ControlPath=/run/ulak/0123456789abcdef -o ControlPersist=600 \
             -o ConnectTimeout=10 -i /keys/id -oBatchMode=yes -t -- my-server",
        );
        args.push("docker 'ps' '-a'".into());
        let plan = Plan::of(&args).expect("carried");
        assert_eq!(plan.control, "/run/ulak/0123456789abcdef");
        assert_eq!(plan.dest, "my-server");
        assert_eq!(plan.line, "docker 'ps' '-a'");
        assert_eq!(plan.tty, Tty::Ask);
        assert!(!plan.stdin_null);
        assert_eq!(
            plan.connection,
            words(
                "-o ControlMaster=auto -o ControlPath=/run/ulak/0123456789abcdef \
                 -o ControlPersist=600 -o ConnectTimeout=10 -i /keys/id -o BatchMode=yes"
            )
        );
    }

    /// Like ssh, the words after the destination are one line, joined by
    /// spaces and split again by the far shell.
    #[test]
    fn the_remote_words_are_joined_the_way_ssh_joins_them() {
        let plan = Plan::of(&words("-S /c/x -l me my-server docker ps -a")).expect("carried");
        assert_eq!(plan.dest, "my-server");
        assert_eq!(plan.line, "docker ps -a");
        assert_eq!(plan.connection, words("-S /c/x -l me"));
    }

    #[test]
    fn options_glued_to_their_letter_are_read_like_ssh_reads_them() {
        let plan = Plan::of(&words("-oControlPath=/c/x -Cp2222 -ttn -- h true")).expect("carried");
        assert_eq!(plan.control, "/c/x");
        assert_eq!(plan.tty, Tty::Force);
        assert!(plan.stdin_null);
        assert_eq!(plan.connection, words("-o ControlPath=/c/x -C -p 2222"));
        let spaced = Plan::of(&["-o", "ControlPath /c/y", "h", "true"].map(String::from));
        assert_eq!(spaced.map(|p| p.control).as_deref(), Some("/c/y"));
        // The last of -t and -T wins, as in ssh.
        assert_eq!(
            Plan::of(&words("-S /c/x -t -T h true")).unwrap().tty,
            Tty::No
        );
        assert_eq!(
            Plan::of(&words("-S /c/x -T -t h true")).unwrap().tty,
            Tty::Ask
        );
    }

    /// What only ssh can do goes to ssh, refused here by name, so a caller
    /// asking for it never gets a channel that silently ignores it.
    #[test]
    fn what_needs_a_session_or_ssh_itself_goes_to_ssh() {
        let cases = [
            // no shared master: the direct connection and the retry
            "-o ControlMaster=no -S none -- h true",
            "-o ControlPath=none -- h true",
            "-- h true",
            // a master, but no command, a forward, or one of ssh's verbs
            "-S /c/x -- h",
            "-S /c/x -N -L 1:h:1 -- h",
            "-S /c/x -O exit -- h",
            "-S /c/x -W h:22 -- h",
            "-S /c/x -A -- h true",
            "-S /c/x -v -- h true",
            "-S /c/x -e none -- h true",
            "-S /c/x -o SendEnv=ULAK_TUNNEL -- h true",
            "-S /c/x -o RequestTTY=force -- h true",
            "-S /c/x -o",
        ];
        for case in cases {
            assert_eq!(Plan::of(&words(case)), None, "{case}");
        }
        assert!(Plan::of(&words("-o ControlMaster=auto -S /c/x -- h true")).is_some());
    }

    /// The server may have any python3 from 3.7 (README), and the helper
    /// is compiled whole before it runs: one newer construct and an older
    /// server never gets the link, nor a reason why.
    #[test]
    fn link_py_parses_on_every_python_the_server_may_have() {
        assert!(HELPER.is_ascii(), "link.py must stay ASCII");
        let out = crate::baseline::tests::newer_syntax_in(HELPER);
        assert!(
            out.status.success(),
            "link.py uses syntax older pythons cannot parse:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_frame_survives_the_wire_and_a_broken_one_is_an_error() {
        let mut wire = Vec::new();
        write_frame(&mut wire, STDOUT, b"hello").unwrap();
        write_frame(&mut wire, STDIN, b"").unwrap();
        let mut input = wire.as_slice();
        assert_eq!(
            read_frame(&mut input).unwrap(),
            Some((STDOUT, b"hello".to_vec()))
        );
        assert_eq!(read_frame(&mut input).unwrap(), Some((STDIN, Vec::new())));
        assert_eq!(read_frame(&mut input).unwrap(), None);
        assert!(read_frame(&mut &wire[..7]).is_err());
        let mut huge = vec![STDOUT];
        huge.extend_from_slice(&(MAX_FRAME + 1).to_be_bytes());
        assert!(read_frame(&mut huge.as_slice()).is_err());
    }

    #[test]
    fn the_helpers_answer_decides_whether_to_ask_again() {
        assert_eq!(
            helper_answer(
                b"ulak-link ready /run/user/0/ulak/link-x.sock\n",
                Some(0),
                false,
                true
            ),
            Started::Ready("/run/user/0/ulak/link-x.sock".into())
        );
        assert!(matches!(
            helper_answer(b"ulak-link ready /a:b/x.sock\n", Some(0), false, true),
            Started::Never(_)
        ));
        assert!(matches!(
            helper_answer(
                b"ulak-link refused: no private directory\n",
                Some(1),
                false,
                true
            ),
            Started::Never(_)
        ));
        assert!(matches!(
            helper_answer(b"", Some(127), false, true),
            Started::Never(_)
        ));
        assert!(matches!(
            helper_answer(b"", Some(1), false, true),
            Started::Never(_)
        ));
        assert_eq!(helper_answer(b"", Some(255), false, true), Started::NotNow);
        assert_eq!(helper_answer(b"", None, true, true), Started::NotNow);
        // A path is not guessed at from bytes that are not UTF-8.
        assert!(matches!(
            helper_answer(b"ulak-link ready /tmp/\xff.sock\n", Some(0), false, true),
            Started::Never(_)
        ));
    }

    /// A login that failed has already said why; ssh asked again would
    /// prompt twice, or wait out a second ConnectTimeout. A login that got
    /// as far as a master leaves the command to it.
    #[test]
    fn a_failed_login_is_the_answer_and_a_master_without_a_helper_is_ssh() {
        assert_eq!(
            helper_answer(b"", Some(255), false, false),
            Started::LoggedOut
        );
        assert_eq!(helper_answer(b"", None, false, false), Started::LoggedOut);
        assert_eq!(helper_answer(b"", None, true, false), Started::NotNow);
        assert!(matches!(
            helper_answer(b"", Some(127), false, false),
            Started::Never(_)
        ));
        assert_eq!(
            helper_answer(b"ulak-link ready /tmp/x.sock\n", Some(0), false, true),
            Started::Ready("/tmp/x.sock".into())
        );
    }

    #[test]
    fn a_signal_death_reads_as_this_machines_number_for_its_name() {
        assert_eq!(Ending::Signal("TERM".into(), 15).code(), 143);
        assert_eq!(Ending::Signal("KILL".into(), 9).code(), 137);
        // Linux's USR1 is 10; the answer is this machine's number.
        assert_eq!(
            Ending::Signal("USR1".into(), 10).code(),
            128 + libc::SIGUSR1 as u8
        );
        // A name only the server has keeps the server's number.
        assert_eq!(Ending::Signal("STKFLT".into(), 16).code(), 144);
        assert_eq!(Ending::Code(7).code(), 7);
        assert_eq!(Ending::Lost.code(), 255);
        assert_eq!(Ending::Unread.code(), 141);
        assert_eq!(Ending::Failed("no fork".into()).code(), 126);
        // Past any signal there is, the same 130 as `docker::status_code`.
        assert_eq!(Ending::Signal("RTMAX+9".into(), 300).code(), 130);
    }

    // ─── the helper itself, served locally ──────────────────────────────

    /// link.py serving a socket in a temporary directory, with no ssh in
    /// between: the far end exactly as it runs on a server.
    struct Served {
        child: std::process::Child,
        socket: PathBuf,
        dir: tempfile::TempDir,
    }

    impl Served {
        fn new() -> Served {
            Served::living(crate::ssh::CONTROL_PERSIST)
        }

        /// A helper that takes new channels for `life` seconds.
        fn living(life: &str) -> Served {
            Served::spawn(life, None)
        }

        /// A helper with only this many descriptors.
        fn limited(descriptors: u32) -> Served {
            Served::spawn(crate::ssh::CONTROL_PERSIST, Some(descriptors))
        }

        fn spawn(life: &str, descriptors: Option<u32>) -> Served {
            // Short, for the unix socket path limit under macOS's TMPDIR.
            let dir = tempfile::Builder::new()
                .prefix("ulk")
                .tempdir_in("/tmp")
                .expect("temp dir");
            let script = dir.path().join("link.py");
            std::fs::write(&script, HELPER).unwrap();
            let socket = dir.path().join("s.sock");
            let limit = descriptors.map_or(String::new(), |n| format!("ulimit -n {n} && "));
            let child = Command::new("sh")
                .arg("-c")
                .arg(format!("{limit}exec python3 \"$@\""))
                .arg("sh")
                .arg(&script)
                .arg("serve")
                .arg(&socket)
                .arg(life)
                .spawn()
                .expect("python3 is required by the unit tests (CONTRIBUTING.md)");
            // Listening, not only bound: the name exists a moment before.
            let until = Instant::now() + Duration::from_secs(10);
            while UnixStream::connect(&socket).is_err() {
                assert!(Instant::now() < until, "the helper never listened");
                std::thread::sleep(Duration::from_millis(20));
            }
            Served { child, socket, dir }
        }

        fn run(&self, line: &str, stdin: &[u8]) -> (Ending, Vec<u8>, Vec<u8>) {
            let stream = greet(&self.socket, GREETING).expect("greeted");
            let input: Box<dyn Read + Send> = Box::new(io::Cursor::new(stdin.to_vec()));
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let (ending, _) =
                exchange(stream, line, None, Some(input), &mut out, &mut err).unwrap();
            (ending, out, err)
        }

        /// A request sent by hand, its stdin closed: the channel to speak
        /// on and read from.
        fn open(&self, line: &str) -> (Speaker, UnixStream) {
            let stream = greet(&self.socket, GREETING).expect("greeted");
            let speaker = Speaker::new(&stream).unwrap();
            speaker.say(OPEN, &request(line, None)).unwrap();
            speaker.say(STDIN, &[]).unwrap();
            (speaker, stream)
        }
    }

    impl Drop for Served {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn alive(pid: i32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn wait_for_pid(path: &Path) -> i32 {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                return pid;
            }
            assert!(Instant::now() < until, "the command never started");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_command_over_the_link_keeps_its_streams_and_its_exit_code_apart() {
        let served = Served::new();
        let (ending, out, err) = served.run("printf 'a\\0b'; printf oops >&2; exit 3", b"");
        assert_eq!(ending, Ending::Code(3));
        assert_eq!(out, b"a\0b");
        assert_eq!(err, b"oops");
    }

    #[test]
    fn stdin_reaches_the_command_byte_for_byte_and_then_ends() {
        let served = Served::new();
        let blob: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let (ending, out, _) = served.run("cat", &blob);
        assert_eq!(ending, Ending::Code(0));
        assert!(
            out == blob,
            "{} bytes came back for {}",
            out.len(),
            blob.len()
        );
        // A script on stdin, the way `run_script` sends every one.
        let (ending, out, _) = served.run("sh -s", b"echo one\nexit 4\n");
        assert_eq!((ending, out), (Ending::Code(4), b"one\n".to_vec()));
    }

    /// A command that stops reading stdin and exits while the client is
    /// still sending it still gets its exit status through.
    #[test]
    fn a_command_that_ends_before_its_stdin_does_still_answers() {
        let served = Served::new();
        let blob = vec![b'x'; 8 << 20];
        let (ending, out, _) = served.run("head -c 5; exit 6", &blob);
        assert_eq!((ending, out), (Ending::Code(6), b"xxxxx".to_vec()));
    }

    #[test]
    fn a_command_killed_on_the_server_says_by_which_signal() {
        let served = Served::new();
        let (ending, _, _) = served.run("kill -TERM $$", b"");
        assert!(
            matches!(&ending, Ending::Signal(name, _) if name == "TERM"),
            "{ending:?}"
        );
        assert_eq!(ending.code(), 143);
    }

    #[test]
    fn a_line_the_shell_cannot_run_answers_like_a_shell() {
        let served = Served::new();
        let (ending, _, err) = served.run("no-such-command-ulak-test", b"");
        assert_eq!(ending, Ending::Code(127));
        assert!(!err.is_empty());
    }

    /// The caller went away mid-command: the whole process group is hung
    /// up, not left running until it next writes.
    #[test]
    fn a_channel_that_closes_first_takes_its_command_group_with_it() {
        let served = Served::new();
        let pidfile = served.dir.path().join("pid");
        let (speaker, stream) = served.open(&format!(
            "sleep 300 & echo $! > {}; wait",
            sh_quote(pidfile.to_str().unwrap())
        ));
        let pid = wait_for_pid(&pidfile);
        drop((speaker, stream));
        let until = Instant::now() + Duration::from_secs(10);
        while alive(pid) {
            assert!(
                Instant::now() < until,
                "a background child outlived its channel by 10 s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A command that has answered may leave something running on
    /// purpose; only a channel that closes before the answer hangs up.
    #[test]
    fn a_command_that_answered_keeps_what_it_started() {
        let served = Served::new();
        let pidfile = served.dir.path().join("pid");
        let (ending, _, _) = served.run(
            &format!(
                "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > {}",
                sh_quote(pidfile.to_str().unwrap())
            ),
            b"",
        );
        assert_eq!(ending, Ending::Code(0));
        let pid = wait_for_pid(&pidfile);
        std::thread::sleep(Duration::from_millis(500));
        assert!(alive(pid), "a detached child was hung up after its answer");
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }

    /// A signal passed along the channel reaches the command's group by
    /// name, and the command still ends with its own answer.
    #[test]
    fn a_signal_passed_along_the_channel_reaches_the_command() {
        let served = Served::new();
        let (speaker, mut reader) = served
            .open("trap 'echo got-term; exit 9' TERM; echo ready; while :; do sleep 0.1; done");
        let mut seen = Vec::new();
        let exit = loop {
            match read_frame(&mut reader).unwrap() {
                Some((STDOUT, bytes)) => {
                    let first = seen.is_empty();
                    seen.extend_from_slice(&bytes);
                    if first {
                        speaker.say(SIGNAL, b"TERM").unwrap();
                    }
                }
                Some((EXIT, payload)) => {
                    break serde_json::from_slice::<serde_json::Value>(&payload).unwrap();
                }
                Some(_) => {}
                None => panic!("the channel closed without an answer"),
            }
        };
        assert_eq!(String::from_utf8_lossy(&seen), "ready\ngot-term\n");
        assert_eq!(exit["code"], 9);
    }

    /// A signal sent in the instant before the request is the command's
    /// all the same: it does not end the channel before anything ran.
    #[test]
    fn a_signal_that_races_its_request_still_reaches_the_command() {
        let served = Served::new();
        let stream = greet(&served.socket, GREETING).unwrap();
        let speaker = Speaker::new(&stream).unwrap();
        let mut reader = stream;
        speaker.say(SIGNAL, b"TERM").unwrap();
        speaker.say(OPEN, &request("sleep 30", None)).unwrap();
        speaker.say(STDIN, &[]).unwrap();
        let started = Instant::now();
        let ending = drain(&mut reader, &mut Vec::new(), &mut Vec::new());
        assert!(
            matches!(&ending, Ending::Signal(name, _) if name == "TERM"),
            "{ending:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// Out of room, the helper says so in its greeting, before a request
    /// that could then only fail: the command can still go to ssh. Its room
    /// is what its descriptors allow, so an accept never fails for want of
    /// one; and the room comes back when commands end.
    #[test]
    fn a_helper_out_of_room_says_so_before_the_request() {
        // 72 descriptors: room for two (link.py, `channels`).
        let served = Served::limited(72);
        let held = [served.open("sleep 30"), served.open("sleep 30")];
        assert_eq!(greet(&served.socket, GREETING).err(), Some(Fault::Full));
        drop(held);
        let until = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match greet(&served.socket, GREETING) {
                Ok(stream) => break stream,
                Err(Fault::Full) if Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(fault) => panic!("no room came back: {fault:?}"),
            }
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (ending, _) = exchange(stream, "echo room", None, None, &mut out, &mut err).unwrap();
        assert_eq!((ending, out), (Ending::Code(0), b"room\n".to_vec()));
    }

    /// A helper told to stop takes what it runs with it, as a channel that
    /// closes does: nothing it started outlives it.
    #[test]
    fn a_helper_told_to_stop_hangs_up_what_it_runs() {
        let served = Served::new();
        let pidfile = served.dir.path().join("pid");
        let _held = served.open(&format!(
            "sleep 300 & echo $! > {}; wait",
            sh_quote(pidfile.to_str().unwrap())
        ));
        let pid = wait_for_pid(&pidfile);
        let helper = served.child.id().to_string();
        assert!(
            Command::new("kill")
                .args(["-TERM", &helper])
                .status()
                .unwrap()
                .success()
        );
        let until = Instant::now() + Duration::from_secs(10);
        while alive(pid) {
            assert!(
                Instant::now() < until,
                "a command outlived its helper by 10 s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// What a pty command wrote before it ended reaches a client that
    /// reads slowly, however long after `DRAIN` that is: only what
    /// something left behind goes on writing is cut.
    #[test]
    fn a_pty_commands_last_output_reaches_a_client_that_reads_slowly() {
        /// 200 KB/s, however the bytes come framed.
        struct Slow<R>(R, usize);
        impl<R: Read> Read for Slow<R> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.1 >= 4096 {
                    std::thread::sleep(Duration::from_millis(20));
                    self.1 = 0;
                }
                let n = buf.len().min(4096);
                let got = self.0.read(&mut buf[..n])?;
                self.1 += got;
                Ok(got)
            }
        }
        let served = Served::new();
        let done = served.dir.path().join("done");
        let stream = greet(&served.socket, GREETING).unwrap();
        let speaker = Speaker::new(&stream).unwrap();
        // More than the buffers between the command and this reader hold,
        // so the helper is still waiting to send when the command ends.
        // Measured on Linux, a helper that stopped reading the terminal at
        // DRAIN lost the last 10 KB, END with it, in one run of two.
        let line = format!(
            "head -c 600000 /dev/zero | tr '\\0' x; echo; echo END; touch {}",
            sh_quote(done.to_str().unwrap())
        );
        speaker
            .say(OPEN, &request(&line, Some((24, 80, "dumb".into()))))
            .unwrap();
        speaker.say(STDIN, &[]).unwrap();
        let mut reader = Slow(stream, 0);
        let mut out = Vec::new();
        let mut paused = false;
        let exit = loop {
            if !paused && done.exists() {
                // Well past link.py's DRAIN of one second.
                paused = true;
                std::thread::sleep(Duration::from_millis(2500));
            }
            match read_frame(&mut reader).unwrap() {
                Some((STDOUT, bytes)) => out.extend_from_slice(&bytes),
                Some((EXIT, payload)) => {
                    break serde_json::from_slice::<serde_json::Value>(&payload).unwrap();
                }
                Some(_) => {}
                None => panic!("the channel closed without an answer"),
            }
        };
        let seen = String::from_utf8_lossy(&out);
        assert_eq!(exit["code"], 0);
        assert!(paused, "the command never finished while being read");
        assert_eq!(seen.matches('x').count(), 600_000);
        assert!(seen.contains("END"), "the end of the output was cut");
    }

    #[test]
    fn a_pty_command_has_a_terminal_of_the_size_asked_for_and_follows_a_resize() {
        let served = Served::new();
        let stream = greet(&served.socket, GREETING).unwrap();
        let speaker = Speaker::new(&stream).unwrap();
        let mut reader = stream;
        speaker
            .say(
                OPEN,
                &request(
                    "test -t 0 && test -t 1 && stty size && read go && stty size",
                    Some((33, 101, "xterm".into())),
                ),
            )
            .unwrap();
        let mut seen = String::new();
        let mut resized = false;
        let exit = loop {
            match read_frame(&mut reader).unwrap() {
                Some((STDOUT, bytes)) => {
                    seen.push_str(&String::from_utf8_lossy(&bytes));
                    if seen.contains("33 101") && !resized {
                        resized = true;
                        speaker
                            .say(WINDOW, br#"{"rows": 40, "cols": 120}"#)
                            .unwrap();
                        speaker.say(STDIN, b"go\n").unwrap();
                    }
                }
                Some((EXIT, payload)) => {
                    break serde_json::from_slice::<serde_json::Value>(&payload).unwrap();
                }
                Some(_) => {}
                None => panic!("the channel closed without an answer"),
            }
        };
        assert_eq!(exit["code"], 0, "{seen:?}");
        assert!(seen.contains("40 120"), "{seen:?}");
    }

    /// A pty ends with its command, as sshd ends one, even when something
    /// it started in the background still holds the terminal. Only Linux
    /// keeps that terminal open: macOS revokes it with its session leader.
    #[test]
    fn a_pty_command_answers_when_it_ends_whatever_it_left_behind() {
        let served = Served::new();
        let stream = greet(&served.socket, GREETING).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let started = Instant::now();
        let (ending, _) = exchange(
            stream,
            "trap '' HUP; sleep 30 & echo pid=$!; echo bye",
            Some((24, 80, "xterm".into())),
            None,
            &mut out,
            &mut err,
        )
        .unwrap();
        let elapsed = started.elapsed();
        let seen = String::from_utf8_lossy(&out);
        let pid: i32 = seen
            .split("pid=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|pid| pid.parse().ok())
            .unwrap_or_else(|| panic!("no pid in {seen:?}"));
        let left_behind = alive(pid);
        let _ = Command::new("kill").arg(pid.to_string()).status();
        assert_eq!(ending, Ending::Code(0));
        assert!(seen.contains("bye"), "{seen:?}");
        assert!(
            elapsed < Duration::from_secs(10),
            "the answer waited {elapsed:?} for a background child"
        );
        assert!(left_behind, "a child that ignores SIGHUP was killed");
    }

    /// Far past MaxSessions: every channel is one connection to one
    /// helper, and none waits for another.
    #[test]
    fn forty_commands_at_once_all_run_side_by_side() {
        let served = Arc::new(Served::new());
        let started = Instant::now();
        let handles: Vec<_> = (0..40)
            .map(|i| {
                let served = Arc::clone(&served);
                std::thread::spawn(move || served.run(&format!("sleep 1; echo {i}"), b""))
            })
            .collect();
        for (i, handle) in handles.into_iter().enumerate() {
            let (ending, out, _) = handle.join().unwrap();
            assert_eq!(ending, Ending::Code(0));
            assert_eq!(out, format!("{i}\n").into_bytes());
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "40 one-second commands took {:?}: they waited for each other",
            started.elapsed()
        );
    }

    /// A caller that stops reading one command's output holds back that
    /// channel only.
    #[test]
    fn a_reader_that_stops_reading_holds_back_only_its_own_channel() {
        let served = Served::new();
        let stalled = served.open("yes");
        // Nobody reads `stalled`; its pipe and socket fill and stay full.
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        for _ in 0..5 {
            let (ending, out, _) = served.run("echo fine", b"");
            assert_eq!((ending, out), (Ending::Code(0), b"fine\n".to_vec()));
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(stalled);
    }

    #[test]
    fn a_greeting_from_something_else_is_not_the_helper() {
        let dir = tempfile::Builder::new()
            .prefix("ulk")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let talker = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            write_frame(&mut conn, HELLO, br#"{"v": 99}"#).unwrap();
            let (conn, _) = listener.accept().unwrap();
            drop(conn);
            let (_silent, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(600));
        });
        assert_eq!(greet(&path, GREETING).err(), Some(Fault::Garbled));
        assert_eq!(greet(&path, GREETING).err(), Some(Fault::Refused));
        assert_eq!(
            greet(&path, Duration::from_millis(200)).err(),
            Some(Fault::Silent)
        );
        talker.join().unwrap();
        drop(dir);
        assert_eq!(greet(&path, GREETING).err(), Some(Fault::Absent));
    }

    /// Past its life a helper takes no new channel, finishes the commands
    /// it runs, and leaves: what it began is not cut short, and what comes
    /// next is the next helper's, with a newer login's environment.
    #[test]
    fn a_helper_past_its_life_finishes_its_commands_and_then_leaves() {
        let mut served = Served::living("1");
        let started = served.open("sleep 2; echo finished");
        let (_speaker, mut reader) = started;
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            !served.socket.exists(),
            "a helper past its life kept its name"
        );
        assert_eq!(greet(&served.socket, GREETING).err(), Some(Fault::Absent));
        assert!(
            served.child.try_wait().unwrap().is_none(),
            "the helper left with a command still running"
        );
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(drain(&mut reader, &mut out, &mut err), Ending::Code(0));
        assert_eq!(out, b"finished\n");
        drop(reader);
        let until = Instant::now() + Duration::from_secs(5);
        while served.child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < until,
                "a retired helper with nothing to do stayed"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `start` is how every helper begins: a second start through the same
    /// connection finds the first one answering and starts nothing, and
    /// another connection gets a helper of its own, with its own login.
    #[test]
    fn a_start_finds_its_connections_helper_and_another_connection_gets_its_own() {
        let home = tempfile::Builder::new()
            .prefix("ulk")
            .tempdir_in("/tmp")
            .unwrap();
        let start_from = |connection: &str| {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(start_line())
                .env("XDG_RUNTIME_DIR", home.path())
                .env("HOME", home.path())
                .env("SSH_CONNECTION", connection)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(HELPER.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            out.stdout
        };
        let start = || start_from("192.0.2.1 50000 192.0.2.2 22");
        let first = start();
        let Started::Ready(path) = helper_answer(&first, Some(0), false, true) else {
            panic!("{}", String::from_utf8_lossy(&first));
        };
        assert!(
            path.starts_with(home.path().to_str().unwrap()),
            "{path} is not under XDG_RUNTIME_DIR"
        );
        assert_eq!(start(), first);
        let Started::Ready(other) = helper_answer(
            &start_from("192.0.2.1 50001 192.0.2.2 22"),
            Some(0),
            false,
            true,
        ) else {
            panic!("another connection's helper did not start");
        };
        assert_ne!(other, path);
        let stream = greet(Path::new(&path), GREETING).expect("the started helper answers");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (ending, _) = exchange(stream, "pwd", None, None, &mut out, &mut err).unwrap();
        assert_eq!(ending, Ending::Code(0));
        // It runs from the account's home, where a session would start.
        let pwd = String::from_utf8(out).unwrap();
        assert_eq!(
            Path::new(pwd.trim()).canonicalize().unwrap(),
            home.path().canonicalize().unwrap()
        );
        // Its socket gone, a helper leaves once nothing runs.
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&other).unwrap();
    }
}
