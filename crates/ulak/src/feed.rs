//! The dashboard's long-lived work: the snapshot poller, the remote
//! streams (`docker events`, `docker stats`, `docker logs -f`), one-shot
//! fetches, the tasks a keypress starts, and the local state reader.
//!
//! **The UI thread never waits.** Every byte that crosses the network or
//! a pipe crosses it here, in a worker, and comes back as a `Msg` — the
//! same invariant `service.rs` keeps for its own main loop, for the same
//! reason: a loop that blocks on a dead link cannot even draw the words
//! "link down".
//!
//! **Every child has an owner.** Dropping a `Stream` or a `Task` ends its
//! process. A remote stream needs more than that: killing the local mux
//! client does NOT end the remote command. Measured against Docker 29.6.2
//! on Ubuntu 26.04 with OpenSSH multiplexing: `docker events` was still
//! running on the server after its `ssh` was `kill -9`ed, and would have
//! stayed until it next wrote to the dead channel — on a quiet daemon,
//! indefinitely. So each remote stream runs under `watchdog`, which ties
//! the remote command's life to its ssh channel's stdin: when our end of
//! that pipe closes — `Drop`, a crash, the dashboard killed by a signal —
//! the remote side reads EOF and kills the command (measured: gone within
//! the same second). The reverse also holds: when the command ends on its
//! own, the watcher is killed and ssh exits (measured: ~1 s).
//!
//! **A stack's logs are one session.** Every session the dashboard holds
//! is a slot on the master the service shares, and sshd gives a master
//! ten (`MaxSessions`). A stack log view used to follow each container
//! in its own session: an 18-container stack asked for 18 more, and ten
//! were refused (measured against OpenSSH 9.6p1: "Session open refused
//! by peer"). ssh then falls back to a fresh connection for each one, and
//! the service's own probe has to fall back the same way. When that probe
//! fails, the service decides the link is down and closes every tunnel.
//! So `watchdog_stack` runs all of a stack's `docker logs` in one session
//! and tags each line with the container it came from.
//!
//! **No child reads the terminal.** The dashboard holds it in raw mode,
//! and an ssh that inherited stdin would eat keypresses. stdin is a pipe
//! this process holds, or null; stdout and stderr are always piped.
//!
//! **Eras.** Messages from the poller and the daemon feeds carry
//! the era of the server they came from. Switching servers bumps
//! it; the receiver drops whatever an old worker still had in flight.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::engine::{self, Event, Snapshot, Stat, Want};
use crate::intent::{Desired, Status};
use crate::proc;
use crate::ssh::{Ssh, sh_quote};
use crate::verbs::Driver;

/// How often the snapshot is re-read when nothing pokes it. `docker
/// events` is what makes changes appear at once; this is the safety net,
/// paced like the service's own probe (`service::PROBE_EVERY`), because
/// every round is one more line in the audit trail.
const POLL_EVERY: Duration = Duration::from_secs(15);

/// `docker info` and `df` change slowly and `info` is the slow call.
const INFO_EVERY: Duration = Duration::from_secs(60);

/// Pokes arriving this close together share one round: a `compose up`
/// emits a burst of events for every container it touches.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// What the first round, and the first after a failure, asks for: every
/// list, so the tabs can count, but not `docker system df`, which walks
/// every layer and volume to size them (measured on Docker 29.6.2 with
/// 19 images: ~0.9 s, against ~0.3 s for everything else together). Only
/// the Images and Volumes tabs show it, and they ask for it themselves.
const FIRST: Want = Want {
    images: true,
    volumes: true,
    networks: true,
    info: true,
    usage: false,
};

/// Retry ladder after a failed round, in seconds.
const BACKOFF: &[u64] = &[2, 5, 10, 30];

/// The local files (`intent`, the service heartbeat) are re-read this
/// often. They are tiny and the read never touches the network.
const LOCAL_EVERY: Duration = Duration::from_secs(3);

/// How long a stopped task gets after SIGINT before it is killed.
const CANCEL_GRACE: Duration = Duration::from_secs(3);

pub enum Msg {
    Snapshot {
        era: u64,
        result: std::result::Result<Snapshot, String>,
        took: Duration,
    },
    Stats {
        era: u64,
        samples: Vec<Stat>,
    },
    Event {
        era: u64,
        event: Event,
    },
    /// A daemon feed's channel is open, before it has said anything.
    FeedUp {
        era: u64,
        feed: Feed,
    },
    /// A daemon feed (`events`, `stats`) ended; the app restarts it.
    FeedEnded {
        era: u64,
        feed: Feed,
        why: String,
    },
    /// Log lines; `source` says which container of a stack view.
    Lines {
        stream: u64,
        source: usize,
        lines: Vec<String>,
    },
    StreamEnded {
        stream: u64,
        source: usize,
        why: String,
    },
    TaskLine {
        task: u64,
        line: String,
    },
    TaskDone {
        task: u64,
        code: Option<i32>,
    },
    Fetched {
        req: u64,
        result: std::result::Result<String, String>,
    },
    Local(Box<Local>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    Events,
    Stats,
}

/// What this machine knows without asking any server.
#[derive(Debug, Clone, Default)]
pub struct Local {
    pub catalog: Vec<(String, Desired)>,
    pub statuses: Vec<(String, Status)>,
    /// How to drive each declared stack whose declaration still rebuilds.
    pub drivers: BTreeMap<String, Driver>,
    pub service_line: String,
    pub service_problem: Option<String>,
}

// ─── the poller ─────────────────────────────────────────────────────

/// Re-reads the snapshot on a cadence, on demand, and with backoff.
/// Dropping it stops the thread at its next wake.
pub struct Poller {
    poke: Sender<Want>,
    interest: Arc<Mutex<Want>>,
}

impl Poller {
    pub fn start(era: u64, ssh: Ssh, out: Sender<Msg>) -> Poller {
        let (poke, pokes) = std::sync::mpsc::channel::<Want>();
        let interest = Arc::new(Mutex::new(Want::default()));
        let shared = Arc::clone(&interest);
        std::thread::spawn(move || {
            let mut pending = FIRST;
            let mut wait = Duration::ZERO;
            let mut failures = 0usize;
            let mut next_info = Instant::now();
            loop {
                match pokes.recv_timeout(wait) {
                    Ok(want) => pending = pending.union(want),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                let settle = Instant::now() + DEBOUNCE;
                while let Ok(want) =
                    pokes.recv_timeout(settle.saturating_duration_since(Instant::now()))
                {
                    pending = pending.union(want);
                }
                let mut want = pending.union(*shared.lock().unwrap_or_else(|p| p.into_inner()));
                if Instant::now() >= next_info {
                    want.info = true;
                }
                let started = Instant::now();
                let result = round(&ssh, want);
                let took = started.elapsed();
                match &result {
                    Ok(_) => {
                        pending = Want::default();
                        failures = 0;
                        wait = POLL_EVERY;
                        if want.info {
                            next_info = Instant::now() + INFO_EVERY;
                        }
                    }
                    Err(_) => {
                        // Ask everything again once the link is back: the
                        // screen may have missed a whole stack meanwhile.
                        pending = FIRST;
                        wait = Duration::from_secs(BACKOFF[failures.min(BACKOFF.len() - 1)]);
                        failures += 1;
                    }
                }
                if out.send(Msg::Snapshot { era, result, took }).is_err() {
                    return;
                }
            }
        });
        Poller { poke, interest }
    }

    /// Refresh now, including these sections.
    pub fn poke(&self, want: Want) {
        let _ = self.poke.send(want);
    }

    /// The sections every round includes from now on — what is on screen.
    pub fn interest(&self, want: Want) {
        *self.interest.lock().unwrap_or_else(|p| p.into_inner()) = want;
    }
}

/// One round over the link. A timeout or an ssh-level failure (255)
/// retires the shared master, as `Ssh::run_script` does: it carried the
/// command that just failed, so it is the prime suspect.
fn round(ssh: &Ssh, want: Want) -> std::result::Result<Snapshot, String> {
    let out = ssh
        .probe(&engine::script(want))
        .map_err(|e| crate::ui::flatten(&e))?;
    if out.timed_out {
        ssh.close_master();
        return Err(format!(
            "{} did not answer within {}s",
            ssh.dest,
            proc::SCRIPT.as_secs()
        ));
    }
    if out.status.code() == Some(255) {
        ssh.close_master();
        return Err(
            last_words(&out.stderr).unwrap_or_else(|| format!("ssh could not reach {}", ssh.dest))
        );
    }
    Ok(engine::parse(&String::from_utf8_lossy(&out.stdout)))
}

fn last_words(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(str::to_string)
}

// ─── remote streams ─────────────────────────────────────────────────

/// A remote command whose output arrives over time. Dropping it closes
/// the watchdog's stdin, then kills the local ssh.
pub struct Stream {
    stdin: Option<ChildStdin>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        // EOF first: that is what the remote watcher is waiting for.
        drop(self.stdin.take());
        if let Some(mut child) = self.child.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The remote side of a stream: the command in the background, a watcher
/// reading the ssh channel's stdin, and whichever ends first ends the
/// other. One line, `;`-joined, because `Remote::script` hands it to the
/// login shell inside `sh -c '…'` and a csh-family shell refuses a
/// newline inside single quotes.
///
/// fd 3 keeps the channel's stdin: a non-interactive sh gives every
/// background job `/dev/null` as stdin, so the watcher must read an fd
/// that rule does not touch. `read` is a builtin, so killing the watcher
/// subshell leaves no orphan `cat` holding the channel open.
///
/// The first line is always `UP`: proof the channel is open and the
/// script running. `docker events` on a quiet daemon prints nothing for
/// hours, and a feed that only counted as live after its first event
/// read as down on exactly the servers where nothing was wrong.
pub fn watchdog(argv: &[String]) -> String {
    let docker = std::iter::once("docker".to_string())
        .chain(argv.iter().map(|a| sh_quote(a)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "echo {UP}; exec 3<&0 </dev/null; {docker} & p=$!; \
         (while read -r _ <&3; do :; done; kill $p 2>/dev/null) >/dev/null 2>&1 & w=$!; \
         wait $p; s=$?; kill $w 2>/dev/null; exit $s"
    )
}

/// The line `watchdog` prints before anything else.
const UP: &str = "==ULAK:stream-up==";

/// What `watchdog_stack` prints, untagged, when one container's logs end.
const ENDED: &str = "==ULAK:ended==";

/// The remote side of a stack's log view: one `docker <argv>` per
/// container in a single session, each line tagged `<source>\t` by awk.
/// awk flushes every line, so the containers interleave line by line.
///
/// A pipeline's `$!` is its last stage. Killing awk would leave a quiet
/// container's `docker logs -f` running on the server, so the watcher
/// ends the whole session with `kill 0` instead. sshd starts each session
/// in a process group of its own, and nothing else is in that group
/// (measured with a bash login shell: the remote `sh` is the leader of
/// its own session and group). Untagged `ENDED <source>` lines mark a
/// container whose logs ended on their own, e.g. because it was removed.
pub fn watchdog_stack(argvs: &[Vec<String>]) -> String {
    let mut line = format!("echo {UP}; exec 3<&0 </dev/null; ");
    let mut pids = Vec::new();
    for (source, argv) in argvs.iter().enumerate() {
        let docker = std::iter::once("docker".to_string())
            .chain(argv.iter().map(|a| sh_quote(a)))
            .collect::<Vec<_>>()
            .join(" ");
        line.push_str(&format!(
            "({docker} 2>&1 | awk '{{ print \"{source}\\t\" $0; fflush() }}'; \
             echo '{ENDED} {source}') & p{source}=$!; "
        ));
        pids.push(format!("$p{source}"));
    }
    line.push_str(&format!(
        "(while read -r _ <&3; do :; done; kill 0) >/dev/null 2>&1 & w=$!; \
         wait {}; kill $w 2>/dev/null; exit 0",
        pids.join(" ")
    ));
    line
}

/// What a stream does with the lines it reads.
#[derive(Clone)]
pub enum Sink {
    /// `docker logs`: both pipes carry the container's own output.
    Logs {
        stream: u64,
        source: usize,
    },
    /// `watchdog_stack`: stdout carries every container's lines, each
    /// tagged with its source; stderr is the session's own.
    Stack {
        stream: u64,
        sources: usize,
    },
    Stats {
        era: u64,
    },
    Events {
        era: u64,
    },
}

/// Start `docker <argv>` on the server as a stream. The argv must have
/// been validated by the caller (`verbs`), like every other route.
pub fn stream(ssh: &Ssh, argv: &[String], sink: Sink, out: Sender<Msg>) -> Result<Stream> {
    open(ssh, watchdog(argv), sink, out)
}

/// Follow several containers' logs in one session (`watchdog_stack`);
/// source `i` is `argvs[i]`. Validated by the caller, like `stream`.
pub fn stack_logs(
    ssh: &Ssh,
    argvs: &[Vec<String>],
    stream: u64,
    out: Sender<Msg>,
) -> Result<Stream> {
    let sink = Sink::Stack {
        stream,
        sources: argvs.len(),
    };
    open(ssh, watchdog_stack(argvs), sink, out)
}

/// One line of `watchdog_stack`'s stdout.
#[derive(Debug, PartialEq)]
enum Tagged<'a> {
    Line(usize, &'a str),
    Ended(usize),
    /// Not from a container: the session's own words, kept as the reason.
    Other,
}

fn tagged(line: &str, sources: usize) -> Tagged<'_> {
    let source = |s: &str| s.parse::<usize>().ok().filter(|i| *i < sources);
    if let Some(rest) = line.strip_prefix(ENDED).and_then(|r| r.strip_prefix(' '))
        && let Some(i) = source(rest)
    {
        return Tagged::Ended(i);
    }
    match line.split_once('\t') {
        Some((head, text)) => match source(head) {
            Some(i) => Tagged::Line(i, text),
            None => Tagged::Other,
        },
        None => Tagged::Other,
    }
}

fn open(ssh: &Ssh, remote: String, sink: Sink, out: Sender<Msg>) -> Result<Stream> {
    let mut cmd =
        crate::docker::Remote { ssh: ssh.clone() }.script(&remote, crate::docker::Tty::Never);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .context("cannot spawn ssh for a dashboard stream")?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(Some(child)));

    // stderr: container output for logs, the reason for anything else.
    let tail = Arc::new(Mutex::new(Vec::<String>::new()));
    let err_thread = stderr.map(|pipe| {
        let sink = sink.clone();
        let out = out.clone();
        let tail = Arc::clone(&tail);
        std::thread::spawn(move || {
            for line in lines(pipe) {
                if let Sink::Logs { stream, source } = sink {
                    let _ = out.send(Msg::Lines {
                        stream,
                        source,
                        lines: vec![line],
                    });
                } else {
                    let mut t = tail.lock().unwrap_or_else(|p| p.into_inner());
                    t.push(line);
                    let excess = t.len().saturating_sub(5);
                    t.drain(..excess);
                }
            }
        })
    });

    let owner = Arc::clone(&child);
    std::thread::spawn(move || {
        // A stack's sources that already said they ended.
        let mut ended = match &sink {
            Sink::Stack { sources, .. } => vec![false; *sources],
            _ => Vec::new(),
        };
        if let Some(pipe) = stdout {
            let mut first = true;
            for line in lines(pipe) {
                if std::mem::take(&mut first) && line == UP {
                    let up = match &sink {
                        Sink::Logs { .. } | Sink::Stack { .. } => continue,
                        Sink::Stats { era } => Msg::FeedUp {
                            era: *era,
                            feed: Feed::Stats,
                        },
                        Sink::Events { era } => Msg::FeedUp {
                            era: *era,
                            feed: Feed::Events,
                        },
                    };
                    if out.send(up).is_err() {
                        return;
                    }
                    continue;
                }
                let msg = match &sink {
                    Sink::Logs { stream, source } => Msg::Lines {
                        stream: *stream,
                        source: *source,
                        lines: vec![line],
                    },
                    Sink::Stack { stream, sources } => match tagged(&line, *sources) {
                        Tagged::Line(source, text) => Msg::Lines {
                            stream: *stream,
                            source,
                            lines: vec![text.to_string()],
                        },
                        Tagged::Ended(source) => {
                            ended[source] = true;
                            Msg::StreamEnded {
                                stream: *stream,
                                source,
                                why: "ended".to_string(),
                            }
                        }
                        Tagged::Other => {
                            let mut t = tail.lock().unwrap_or_else(|p| p.into_inner());
                            t.push(line);
                            let excess = t.len().saturating_sub(5);
                            t.drain(..excess);
                            continue;
                        }
                    },
                    Sink::Stats { era } => {
                        let samples = engine::parse_stats(&line);
                        if samples.is_empty() {
                            continue;
                        }
                        Msg::Stats { era: *era, samples }
                    }
                    Sink::Events { era } => match engine::parse_event(&line) {
                        Some(event) => Msg::Event { era: *era, event },
                        None => continue,
                    },
                };
                if out.send(msg).is_err() {
                    return;
                }
            }
        }
        if let Some(t) = err_thread {
            let _ = t.join();
        }
        // Reap outside the lock, so `Drop` never waits behind a wait.
        let taken = owner.lock().unwrap_or_else(|p| p.into_inner()).take();
        let code = taken.and_then(
            |mut c| match proc::wait_for(&mut c, Duration::from_secs(5)) {
                Ok(Some(status)) => status.code(),
                _ => {
                    let _ = c.kill();
                    c.wait().ok().and_then(|s| s.code())
                }
            },
        );
        crate::passthrough::record_ssh_audit(&cmd, &remote, code);
        let said = tail.lock().unwrap_or_else(|p| p.into_inner()).join(" · ");
        let why = match (code, said.is_empty()) {
            (_, false) => said,
            (Some(0), true) => "ended".to_string(),
            (Some(255), true) => "the connection dropped".to_string(),
            (Some(c), true) => format!("ended with code {c}"),
            (None, true) => "stopped".to_string(),
        };
        let last = match sink {
            Sink::Logs { stream, source } => vec![Msg::StreamEnded {
                stream,
                source,
                why,
            }],
            Sink::Stack { stream, .. } => (0..ended.len())
                .filter(|source| !ended[*source])
                .map(|source| Msg::StreamEnded {
                    stream,
                    source,
                    why: why.clone(),
                })
                .collect(),
            Sink::Stats { era } => vec![Msg::FeedEnded {
                era,
                feed: Feed::Stats,
                why,
            }],
            Sink::Events { era } => vec![Msg::FeedEnded {
                era,
                feed: Feed::Events,
                why,
            }],
        };
        for msg in last {
            let _ = out.send(msg);
        }
    });
    Ok(Stream { stdin, child })
}

/// Lines from a pipe, decoded for display. Log output is not a path, so
/// a lossy decode is the honest rendering of bytes that are not UTF-8.
fn lines(pipe: impl Read) -> impl Iterator<Item = String> {
    let mut reader = BufReader::new(pipe);
    std::iter::from_fn(move || {
        let mut buf = Vec::new();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                    buf.pop();
                }
                Some(String::from_utf8_lossy(&buf).into_owned())
            }
        }
    })
}

// ─── one-shot fetches ───────────────────────────────────────────────

/// Run one remote `docker` read (an inspect) and hand back its stdout.
pub fn fetch(req: u64, ssh: Ssh, argv: Vec<String>, out: Sender<Msg>) {
    std::thread::spawn(move || {
        let remote = crate::docker::Remote { ssh };
        let (mut cmd, redacted) = remote.spell(&argv, None, crate::docker::Tty::Never, &[]);
        let result = match proc::run_bounded(&mut cmd, None, proc::SCRIPT) {
            Ok(b) => {
                crate::passthrough::record_ssh_audit(&cmd, &redacted, b.status.code());
                if b.timed_out {
                    Err(format!("no answer within {}s", proc::SCRIPT.as_secs()))
                } else if b.status.success() {
                    Ok(String::from_utf8_lossy(&b.stdout).into_owned())
                } else {
                    Err(last_words(&b.stderr).unwrap_or_else(|| "docker refused".into()))
                }
            }
            Err(e) => Err(crate::ui::flatten(&e)),
        };
        let _ = out.send(Msg::Fetched { req, result });
    });
}

// ─── tasks ──────────────────────────────────────────────────────────

/// A command a keypress started: its output streams into the task list,
/// it can be cancelled, and the dashboard never waits on it.
///
/// Not bounded by a deadline, deliberately, like every foreground stream
/// (`proc.rs`): its output is on screen and it can be cancelled.
pub struct Task {
    pid: u32,
    child: Arc<Mutex<Option<Child>>>,
}

impl Task {
    /// What Ctrl-C would do in a terminal: SIGINT, via the `kill` binary
    /// (production `unsafe` belongs to `dirfd.rs` alone), then SIGKILL if
    /// it is still there after a grace.
    pub fn cancel(&self) {
        let mut kill = Command::new("kill");
        kill.args(["-INT", &self.pid.to_string()]);
        let _ = proc::run_bounded(&mut kill, None, proc::PROBE);
        let child = Arc::clone(&self.child);
        std::thread::spawn(move || {
            std::thread::sleep(CANCEL_GRACE);
            if let Some(c) = child.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
                let _ = c.kill();
            }
        });
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if let Some(c) = self
            .child
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
        {
            let _ = c.kill();
        }
    }
}

/// Start `cmd` as task `id`. `audit` is the redacted remote line for an
/// ssh task; a task that is Ulak itself audits on its own.
pub fn task(id: u64, mut cmd: Command, audit: Option<String>, out: Sender<Msg>) -> Result<Task> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot start {}", cmd.get_program().to_string_lossy()))?;
    let pid = child.id();
    let pipes = [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    ];
    let child = Arc::new(Mutex::new(Some(child)));
    let readers: Vec<_> = pipes
        .into_iter()
        .flatten()
        .map(|pipe| {
            let out = out.clone();
            std::thread::spawn(move || {
                for line in lines(pipe) {
                    let line = crate::ansi::strip(&line);
                    if out.send(Msg::TaskLine { task: id, line }).is_err() {
                        return;
                    }
                }
            })
        })
        .collect();
    let owner = Arc::clone(&child);
    std::thread::spawn(move || {
        for r in readers {
            let _ = r.join();
        }
        let code = loop {
            let mut guard = owner.lock().unwrap_or_else(|p| p.into_inner());
            let Some(c) = guard.as_mut() else { break None };
            match c.try_wait() {
                Ok(Some(status)) => {
                    guard.take();
                    break status.code();
                }
                Ok(None) => {}
                Err(_) => {
                    guard.take();
                    break None;
                }
            }
            drop(guard);
            std::thread::sleep(Duration::from_millis(50));
        };
        if let Some(redacted) = &audit {
            crate::passthrough::record_ssh_audit(&cmd, redacted, code);
        }
        let _ = out.send(Msg::TaskDone { task: id, code });
    });
    Ok(Task { pid, child })
}

// ─── local state ────────────────────────────────────────────────────

/// Re-reads this machine's own files on a cadence. Dropping the handle
/// stops the thread at its next wake.
pub struct LocalReader {
    _stop: Sender<()>,
}

pub fn local(out: Sender<Msg>) -> LocalReader {
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        loop {
            let catalog = crate::intent::catalog();
            let statuses = catalog
                .iter()
                .filter_map(|(id, _)| Some((id.clone(), crate::intent::read_status(id)?)))
                .collect();
            // Through the same validation the service and root commands
            // use: a declaration is untrusted input until it rebuilds.
            let drivers = catalog
                .iter()
                .filter_map(|(id, d)| {
                    let project = d.project_for_stack(id).ok()?;
                    Some((id.clone(), Driver::of(&project)))
                })
                .collect();
            let health = crate::agent::health();
            let view = Local {
                catalog,
                statuses,
                drivers,
                service_line: health.line,
                service_problem: health.problem,
            };
            if out.send(Msg::Local(Box::new(view))).is_err() {
                return;
            }
            match stopped.recv_timeout(LOCAL_EVERY) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => return,
            }
        }
    });
    LocalReader { _stop: stop }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper must be one line (a csh login shell rejects a newline
    /// inside the `sh -c '…'` quotes) and quote every argv word.
    #[test]
    fn the_watchdog_is_one_line_and_quotes_every_word() {
        let line = watchdog(&["logs".into(), "-f".into(), "it's".into()]);
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains("docker logs -f 'it'\\''s' & p=$!"), "{line}");
        assert!(
            line.starts_with(&format!("echo {UP}; exec 3<&0 </dev/null; ")),
            "{line}"
        );
        assert!(line.ends_with("exit $s"));
    }

    /// The watchdog's contract, run locally under `sh`: closing stdin
    /// ends a command that would otherwise never end, and a command that
    /// ends on its own ends the wrapper while stdin is still open.
    #[test]
    fn the_watchdog_ends_with_its_stdin_and_with_its_command() {
        let script = watchdog(&[]).replacen("docker", "sleep 30", 1);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        drop(child.stdin.take());
        let ended = proc::wait_for(&mut child, Duration::from_secs(5)).unwrap();
        if ended.is_none() {
            let _ = child.kill();
        }
        assert!(ended.is_some(), "closing stdin must end the command");

        let script = watchdog(&[]).replacen("docker", "true", 1);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let _held = child.stdin.take();
        let ended = proc::wait_for(&mut child, Duration::from_secs(5)).unwrap();
        if ended.is_none() {
            let _ = child.kill();
        }
        assert_eq!(
            ended.and_then(|s| s.code()),
            Some(0),
            "the command ending must end ssh"
        );
    }

    #[test]
    fn the_watchdog_says_up_before_the_command_speaks() {
        let script = watchdog(&[]).replacen("docker", "echo hello", 1);
        let out = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("{UP}\nhello\n")
        );
    }

    #[test]
    fn the_stack_watchdog_is_one_line_and_tags_every_source() {
        let line = watchdog_stack(&[
            vec!["logs".into(), "it's".into()],
            vec!["logs".into(), "b".into()],
        ]);
        assert!(!line.contains('\n'), "{line}");
        assert!(line.starts_with(&format!("echo {UP}; exec 3<&0 </dev/null; ")));
        assert!(
            line.contains("(docker logs 'it'\\''s' 2>&1 | awk"),
            "{line}"
        );
        assert!(line.contains(r#"print "0\t" $0; fflush()"#), "{line}");
        assert!(line.contains(r#"print "1\t" $0; fflush()"#), "{line}");
        assert!(
            line.contains(&format!("echo '{ENDED} 1') & p1=$!")),
            "{line}"
        );
        assert!(line.ends_with("wait $p0 $p1; kill $w 2>/dev/null; exit 0"));
    }

    #[test]
    fn stack_lines_are_routed_by_their_tag_and_nothing_else() {
        assert_eq!(tagged("0\thello", 2), Tagged::Line(0, "hello"));
        assert_eq!(tagged("1\t\tindented", 2), Tagged::Line(1, "\tindented"));
        assert_eq!(tagged("1\t", 2), Tagged::Line(1, ""));
        assert_eq!(tagged(&format!("{ENDED} 1"), 2), Tagged::Ended(1));
        // A source the view does not have, and anything untagged, is the
        // session talking rather than a container.
        assert_eq!(tagged("2\tbeyond", 2), Tagged::Other);
        assert_eq!(tagged(&format!("{ENDED} 2"), 2), Tagged::Other);
        assert_eq!(tagged("x\tnot a source", 2), Tagged::Other);
        assert_eq!(tagged("sh: 1: awk: not found", 2), Tagged::Other);
        assert_eq!(tagged("", 2), Tagged::Other);
    }

    /// A stand-in `docker` for running `watchdog_stack` locally: the last
    /// word names how it behaves, and a follower records its pid first,
    /// so a test can prove nothing it started outlives the session.
    fn fake_docker(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let docker = bin.join("docker");
        std::fs::write(
            &docker,
            format!(
                "#!/bin/sh\n\
                 for a; do id=$a; done\n\
                 case \"$id\" in\n\
                 follow) echo \"$$\" >> '{pids}'; echo 'hello'; exec sleep 30 ;;\n\
                 shout) echo 'to stderr' >&2 ;;\n\
                 *) echo \"$id one\"; echo \"$id two\" ;;\n\
                 esac\n",
                pids = dir.join("pids").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    /// Run the script the way sshd would: in a process group of its own,
    /// which is what makes `kill 0` end exactly what the script started
    /// (and, here, not the test runner).
    fn run_stack(dir: &std::path::Path, ids: &[&str]) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        let argvs: Vec<Vec<String>> = ids
            .iter()
            .map(|id| vec!["logs".into(), "--follow".into(), (*id).into()])
            .collect();
        let path = format!(
            "{}:{}",
            fake_docker(dir).display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("sh")
            .arg("-c")
            .arg(watchdog_stack(&argvs))
            .env("PATH", path)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn alive(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[test]
    fn a_stack_session_tags_each_container_and_ends_when_they_all_have() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = run_stack(dir.path(), &["a", "b", "shout"]);
        let _held = child.stdin.take();
        let out = child.stdout.take().unwrap();
        let got: Vec<String> = lines(out).collect();
        let ended = proc::wait_for(&mut child, Duration::from_secs(5)).unwrap();
        if ended.is_none() {
            let _ = child.kill();
        }
        assert_eq!(
            ended.and_then(|s| s.code()),
            Some(0),
            "every container ending must end the session while stdin is open"
        );
        assert_eq!(got.first().map(String::as_str), Some(UP), "{got:?}");
        // Each container's own order is kept; the interleaving is not.
        for (source, want) in [
            (0, vec!["a one", "a two"]),
            (1, vec!["b one", "b two"]),
            (2, vec!["to stderr"]),
        ] {
            let mine: Vec<&str> = got
                .iter()
                .filter_map(|l| match tagged(l, 3) {
                    Tagged::Line(s, text) if s == source => Some(text),
                    _ => None,
                })
                .collect();
            assert_eq!(mine, want, "source {source} in {got:?}");
            assert!(
                got.iter().any(|l| tagged(l, 3) == Tagged::Ended(source)),
                "source {source} must say it ended: {got:?}"
            );
        }
    }

    #[test]
    fn closing_a_stack_session_ends_every_follower_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = run_stack(dir.path(), &["follow", "follow", "follow"]);
        let mut out = lines(child.stdout.take().unwrap());
        let mut hellos = 0;
        while hellos < 3 {
            match out.next() {
                Some(l) if matches!(tagged(&l, 3), Tagged::Line(_, "hello")) => hellos += 1,
                Some(_) => {}
                None => break,
            }
        }
        assert_eq!(hellos, 3, "every follower must have spoken");
        let pids = std::fs::read_to_string(dir.path().join("pids")).unwrap();
        let pids: Vec<&str> = pids.lines().collect();
        assert_eq!(pids.len(), 3, "{pids:?}");
        assert!(pids.iter().all(|p| alive(p)), "followers run until told");

        drop(child.stdin.take());
        let ended = proc::wait_for(&mut child, Duration::from_secs(5)).unwrap();
        if ended.is_none() {
            let _ = child.kill();
        }
        assert!(ended.is_some(), "closing stdin must end the session");
        // `kill 0` reached the followers themselves, not just the script.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while pids.iter().any(|p| alive(p)) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let left: Vec<&&str> = pids.iter().filter(|p| alive(p)).collect();
        for p in &left {
            let _ = Command::new("kill").args(["-9", p]).status();
        }
        assert!(left.is_empty(), "followers outlived the session: {left:?}");
    }

    #[test]
    fn a_task_streams_both_pipes_and_reports_its_code() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 3"]);
        let _task = task(7, cmd, None, tx).unwrap();
        let mut lines = Vec::new();
        let code = loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                Msg::TaskLine { task: 7, line } => lines.push(line),
                Msg::TaskDone { task: 7, code } => break code,
                _ => {}
            }
        };
        lines.sort();
        assert_eq!(lines, vec!["err", "out"]);
        assert_eq!(code, Some(3));
    }

    #[test]
    fn a_cancelled_task_ends() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let t = task(1, cmd, None, tx).unwrap();
        t.cancel();
        let done = loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Msg::TaskDone { task: 1, code }) => break Some(code),
                Ok(_) => {}
                Err(_) => break None,
            }
        };
        assert!(done.is_some(), "a cancelled task must report that it ended");
    }

    #[test]
    fn lines_strip_line_endings_and_survive_bad_bytes() {
        let got: Vec<String> = lines(&b"one\r\ntwo\n\xffthree"[..]).collect();
        assert_eq!(got, vec!["one", "two", "\u{fffd}three"]);
    }
}
