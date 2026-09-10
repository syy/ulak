//! The service: close the lid, open it in the morning, keep working.
//!
//! Four complaints, one structural hole: nothing in ulak owned a
//! long-lived job, so nobody created the tunnels, nobody could kill
//! them, and nobody noticed when they died. This module is the owner.
//!
//! **One process, one loop, and a main thread that never touches the
//! network.** Its only blocking point is `recv_timeout(TICK)`. That
//! single invariant is what makes the class launchd cannot see —
//! "running, and wedged forever" — impossible to construct here: every
//! byte that crosses the wire crosses it inside a worker, behind a
//! deadline, and the loop that decides what happens next is never the
//! thread waiting for it.
//!
//! **One worker per destination.** Everything that reaches a server —
//! probe, rsync, reconcile — serializes in that server's worker. That
//! preserves the invariant `ssh -O exit` needs: no second job is in
//! flight on the master we are about to retire.
//!
//! **One engine.** The service calls `sync::reconcile_once`, and so does
//! `ulak sync`. There is no second code path that could drift.
//!
//! **The clock is the only sensor.** Suspend, a wifi handover, a VPN
//! drop and a server reboot are the same event — the connection is
//! gone — and `SystemTime` is the only thing that sees all four.
//! `Instant` on macOS is `CLOCK_UPTIME_RAW`: measured, it saw **0
//! seconds of a 41-hour sleep**. So the wake signal is a wall-clock
//! jump, and its answer is not "check whether the link survived" but
//! "assume it did not". Nothing in the chain waits out a keepalive's
//! 60 seconds or the OS's 75-second connect timeout.
//!
//! **A stack that is down is left alone.** No push, no pull, no tunnel,
//! just a sparse unmultiplexed probe. The old idle tick cost 1.14 MB and
//! ~2.9 s a round whether or not anything was running — about 1.6 GB a
//! night for a stack nobody had started.
//!
//! **A project label is not workspace ownership.** Two clients can address
//! the same Docker project name while their transported workspaces remain
//! separate. Every live container's Compose config/working-directory labels
//! must point into the job's workspace before the service syncs it or keeps
//! its tunnels. The manifest proves who owns a directory; Docker's labels
//! prove which directory the running stack actually uses. Both are required.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};

use crate::config::{self, Project};
use crate::footprint::{self, Footprint};
use crate::forward::{self, Tunnels};
use crate::intent::{self, Status};
use crate::lockfile::WorkspaceLock;
use crate::ssh::{Ssh, sh_quote};
use crate::sync;
use crate::ui;

// ─── cadences ───────────────────────────────────────────────────────

/// The main loop's heartbeat. It costs nothing — the thread does only
/// local file reads — and a second is the resolution the wake signal
/// needs: the lid opens, and within one tick the clock has jumped.
const TICK: Duration = Duration::from_secs(1);

/// How often the intent files are re-read. A handful of small private
/// files; a project is only rebuilt when its intent actually changed.
const CATALOG_EVERY: Duration = Duration::from_secs(5);

const DEBOUNCE: Duration = Duration::from_millis(200);

/// Above this, a wall-clock jump is a real suspend and everything is
/// assumed dead: retire the master, kill the tunnels, rebuild.
const HARD_WAKE: Duration = Duration::from_secs(90);
/// Between the two, one probe decides — an NTP step or a short VM pause
/// must not tear down tunnels that are perfectly alive.
const SOFT_WAKE: Duration = Duration::from_secs(5);

/// The backoff ladder in seconds. It starts fast because the common case
/// is a link that is already back (the laptop woke, the wifi returned),
/// and it ends slow because a server that has been unreachable for ten
/// minutes will not be reached by asking harder.
const BACKOFF: &[u64] = &[2, 5, 15, 30, 60, 120];
/// Where a link that never comes back settles. Still probing — a
/// service that gives up permanently is a service you have to remember
/// to restart.
const BLOCKED_EVERY: Duration = Duration::from_secs(300);

/// How often a live stack is asked about. One batched `docker ps --format`
/// answers for EVERY project on that server: is the link alive, is the stack
/// up, which services are running, and which transported workspace every
/// running container's Compose labels name.
const PROBE_EVERY: Duration = Duration::from_secs(15);
/// A destination whose every live stack is down is asked this rarely,
/// and without multiplexing: silence that keeps a shared master warm is
/// not silence.
const SILENT_PROBE_EVERY: Duration = Duration::from_secs(120);
/// A running stack reconciles at least this often even if nobody typed
/// anything — a container may have written a file while you were away.
const IDLE_RECONCILE: Duration = Duration::from_secs(60);
/// How long to wait out a human who holds the per-workspace lock. Short
/// enough that "the next tick is seconds away" stays honest, long enough
/// that stepping aside is a step and not a spin.
const LOCK_HELD_RETRY: Duration = Duration::from_secs(1);
/// How long a stack whose compose files will not resolve waits for
/// its next attempt.
///
/// Coarser than `LOCK_HELD_RETRY` on purpose: a held lock clears by
/// itself in seconds, a YAML error does not clear until somebody edits
/// the file — and every attempt is the whole bootstrap again, because a
/// failed resolve stores no cache entry and so never gets cheaper (an
/// `rm -rf`/`mkdir` over ssh, an rsync, and `docker compose config` on
/// the server, each time). Nothing waits this out in the common case:
/// saving the file is a `Dirty` nudge and a typed command is a doorbell,
/// and either wakes the worker at once. Well inside `IDLE_RECONCILE`, so
/// a fix made outside the watched set is not sat on for a whole minute.
const RESOLVE_FAILED_RETRY: Duration = Duration::from_secs(15);

/// The service's audit trail is ONE file answering one question. Binding
/// it per-workspace would make "what did the service run?" unanswerable the
/// moment it looks after two.
const AUDIT: &str = "service";

/// The opening of the note a stopped stack gets. Shared, because the
/// other side of the product has to RECOGNISE it: repeating "the stack
/// is not running" to somebody who is in the middle of typing `up` is
/// the one complaint that is pure noise. A constant rather than two
/// copies of a sentence, so editing it here cannot silently break the
/// suppression there.
pub const STACK_DOWN: &str = "the stack is not running on";

// ─── the loop ───────────────────────────────────────────────────────

pub fn run(foreground: bool) -> Result<ExitCode> {
    // `auto = false` means "never in the background". It cannot mean
    // "never at all", or the documented way to run the service by hand
    // would be disabled by the switch that turns the automatic one off.
    if !foreground && !config::machine_config().service.auto {
        ui::info("[service] auto = false — Ulak runs nothing in the background on this machine");
        ui::dim("run it by hand whenever you want it: ulak service run --foreground");
        return Ok(ExitCode::SUCCESS);
    }

    // A launch-agent label can change, a user can start the service by
    // hand, and an installer can be interrupted halfway through a
    // restart. None of those may create a second loop racing the first
    // one for the heartbeat, stack state and local tunnel ports.
    let Some(_service_lock) = crate::lockfile::ServiceLock::try_acquire()? else {
        ui::info("another Ulak service is already running on this machine — leaving it in charge");
        return Ok(ExitCode::SUCCESS);
    };
    crate::audit::set_context(AUDIT);

    // Kill-on-drop covers every ordinary end but not SIGKILL, and
    // launchd promises exactly five seconds before it sends one. A
    // leftover tunnel is a local port pointing at a server nobody
    // maintains — and the next service would read it as "busy" forever.
    let swept = forward::sweep_orphans();
    if swept > 0 {
        ui::warn(&format!(
            "closed {swept} tunnel(s) a previous service left behind"
        ));
    }

    let (tx, rx) = channel::<Signal>();
    let files = tx.clone();
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| {
        let _ = files.send(Signal::Files(res));
    })?;

    let mut fleet = Fleet::new(tx);
    let mut next_catalog = Instant::now();
    let mut last_seen = SystemTime::now();
    // Written from the main thread and nowhere else, which is what makes
    // it worth reading: this thread never touches the network, so a
    // heartbeat that stops while the process lives is the one signal
    // launchd structurally cannot give ("running" is all it knows).
    let started = intent::now_unix();
    crate::agent::beat(started);
    let mut next_beat = Instant::now() + crate::agent::BEAT_EVERY;
    ui::info("Ulak service — looking after every stack you left up   (Ctrl-C stops it)");

    loop {
        match rx.recv_timeout(TICK) {
            Ok(Signal::Files(result)) => fleet.on_files(result),
            Ok(Signal::Watch {
                stack,
                dirs,
                excluded,
            }) => fleet.on_watch(&mut debouncer, stack, dirs, excluded),
            Err(RecvTimeoutError::Timeout) => {}
            // Every sender is gone, which cannot happen while the fleet
            // holds one — so this is the watcher dying, and a service
            // that cannot see files is not the service.
            Err(RecvTimeoutError::Disconnected) => {
                return Err(crate::ui::fail!("the file watcher stopped unexpectedly")
                    .now("restart the service: ulak service run --foreground")
                    .into_err());
            }
        }

        // The one sensor. Read on every pass, not on a cadence: the
        // whole point is to see the jump within a tick of the lid
        // opening, and the loop may have been woken by a file event.
        if let Some(hard) = woke(&mut last_seen) {
            fleet.wake(hard);
        }

        if Instant::now() >= next_catalog {
            next_catalog = Instant::now() + CATALOG_EVERY;
            fleet.reload(&mut debouncer);
        }
        if Instant::now() >= next_beat {
            next_beat = Instant::now() + crate::agent::BEAT_EVERY;
            crate::agent::beat(started);
        }
        fleet.keep_workers_alive();
    }
}

/// Did the wall clock jump, and is it big enough to be a suspend?
///
/// `SystemTime` and nothing else. Measured: `Instant` on macOS is
/// `CLOCK_UPTIME_RAW` and saw zero seconds of a 41-hour sleep, and on
/// Linux the same type means the opposite — one portable sensor without
/// reaching for libc, and this is it.
fn woke(last: &mut SystemTime) -> Option<bool> {
    let now = SystemTime::now();
    // A clock that stepped BACKWARDS is not a suspend; ignore it.
    let jump = now.duration_since(*last).unwrap_or(Duration::ZERO);
    *last = now;
    match jump {
        j if j >= HARD_WAKE => Some(true),
        j if j >= SOFT_WAKE => Some(false),
        _ => None,
    }
}

// ─── messages ───────────────────────────────────────────────────────

enum Signal {
    Files(DebounceEventResult),
    /// A worker resolved a footprint and needs these directories
    /// watched. It cannot subscribe itself: only the main thread owns
    /// the watcher, only workers touch the network, and keeping that
    /// line straight is what keeps the main thread unblockable.
    Watch {
        stack: String,
        dirs: Vec<PathBuf>,
        excluded: Vec<PathBuf>,
    },
}

enum Nudge {
    /// The full job list for this destination — the worker keeps the
    /// runtime state of the stacks that survive and drops the rest
    /// (which closes their tunnels).
    Jobs(Vec<Job>),
    Dirty(String),
    /// Somebody ran a compose command against a stack on this
    /// destination, so this worker's picture of it is stale: ask now
    /// instead of at the end of the cadence. `Dirty` cannot stand in for
    /// this — it schedules a reconcile, and a reconcile never happens for
    /// a stack the worker has not yet SEEN running. Only a probe answers
    /// that question. See `intent::nudge` for the 82 seconds it costs.
    Look,
    Wake {
        hard: bool,
    },
}

#[derive(Clone)]
struct Job {
    id: String,
    project: Project,
}

// ─── the main thread's bookkeeping ──────────────────────────────────

struct Fleet {
    signals: Sender<Signal>,
    workers: BTreeMap<String, Worker>,
    jobs: BTreeMap<String, Job>,
    /// What each stack's intent looked like when its job was built, so
    /// an unchanged file costs a read and not a rebuild.
    built: BTreeMap<String, intent::Desired>,
    watched: BTreeMap<String, Watch>,
    /// The last complaint made about a stack, so a compose file that
    /// has been deleted is reported once and not every five seconds.
    complained: BTreeMap<String, String>,
}

struct Watch {
    dirs: Vec<PathBuf>,
    excluded: Vec<PathBuf>,
}

struct Worker {
    tx: Sender<Nudge>,
    handle: std::thread::JoinHandle<()>,
    ssh: Ssh,
    jobs: Vec<Job>,
}

impl Fleet {
    fn new(signals: Sender<Signal>) -> Fleet {
        Fleet {
            signals,
            workers: BTreeMap::new(),
            jobs: BTreeMap::new(),
            built: BTreeMap::new(),
            watched: BTreeMap::new(),
            complained: BTreeMap::new(),
        }
    }

    /// Re-read the intent files and hand each destination its job list.
    fn reload(&mut self, debouncer: &mut Debouncer) {
        let catalog = intent::catalog();
        let present: BTreeSet<String> = catalog.iter().map(|(id, _)| id.clone()).collect();
        // Taken for every stack, acted on only for the ones that end up
        // with a job: a nudge left by a `down` has nothing to ask about,
        // and consuming it here is what stops it from firing later.
        let nudged: BTreeSet<String> = catalog
            .iter()
            .filter(|(id, _)| intent::take_nudge(id))
            .map(|(id, _)| id.clone())
            .collect();

        for (id, desired) in &catalog {
            if !desired.live {
                self.retire(debouncer, id);
                continue;
            }
            if self.built.get(id) == Some(desired) {
                continue;
            }
            // `desired.json` is untrusted input by contract: it is plain
            // JSON on disk that a process holding the user's ssh keys
            // will act on. Rebuilding through `Invocation` is what gives
            // its paths the same checking a typed `-f` gets. The pinned
            // workspace binds those paths to the bytes the declaration
            // used; the directory id independently binds the file to one
            // Docker project on one daemon. Neither may be rewritten to
            // make the service act on an arbitrary checkout or host.
            let rebuilt = desired.project_for_stack(id);
            match rebuilt {
                Ok(project) => {
                    self.complained.remove(id);
                    self.built.insert(id.clone(), desired.clone());
                    self.jobs.insert(
                        id.clone(),
                        Job {
                            id: id.clone(),
                            project,
                        },
                    );
                }
                Err(e) => {
                    self.complain(id, &ui::flatten(&e));
                    self.retire(debouncer, id);
                }
            }
        }
        // A stack whose state directory is gone (a `clean`) stops being
        // anyone's business, tunnels included.
        let stale: Vec<String> = self
            .jobs
            .keys()
            .filter(|id| !present.contains(*id))
            .cloned()
            .collect();
        for id in stale {
            self.retire(debouncer, &id);
        }
        self.dispatch(&nudged);
    }

    /// Group the jobs by destination and give every worker its share.
    fn dispatch(&mut self, nudged: &BTreeSet<String>) {
        let mut by_dest: BTreeMap<String, Vec<Job>> = BTreeMap::new();
        let mut homeless: Vec<(String, String)> = Vec::new();
        for job in self.jobs.values() {
            match job.project.ssh_dest() {
                Ok(dest) => by_dest.entry(dest).or_default().push(job.clone()),
                Err(e) => homeless.push((job.id.clone(), ui::flatten(&e))),
            }
        }
        for (id, why) in homeless {
            self.complain(&id, &why);
        }
        for (dest, jobs) in &by_dest {
            self.ensure_worker(dest);
            if let Some(w) = self.workers.get_mut(dest) {
                w.jobs = jobs.clone();
                let _ = w.tx.send(Nudge::Jobs(jobs.clone()));
                // The job list itself cannot carry this: it is sent on
                // every catalog read, so a flag inside it would either
                // fire once and be forgotten or fire forever. The nudge
                // was consumed from disk, so it is spent here.
                if jobs.iter().any(|j| nudged.contains(&j.id)) {
                    let _ = w.tx.send(Nudge::Look);
                }
            }
        }
        // A destination with nothing left keeps its worker but is told
        // so: retiring the thread while it might be mid-reconcile would
        // mean either blocking the main loop on a join or racing a
        // replacement, and an idle worker costs one sleeping thread.
        let idle: Vec<String> = self
            .workers
            .keys()
            .filter(|d| !by_dest.contains_key(*d))
            .cloned()
            .collect();
        for dest in idle {
            if let Some(w) = self.workers.get_mut(&dest)
                && !w.jobs.is_empty()
            {
                w.jobs.clear();
                let _ = w.tx.send(Nudge::Jobs(Vec::new()));
            }
        }
    }

    fn ensure_worker(&mut self, dest: &str) {
        if self.workers.contains_key(dest) {
            return;
        }
        // Built HERE, not in the worker: it only reads the environment
        // and creates a private directory, and a failure must not become
        // a thread that dies on every respawn.
        let ssh = match Ssh::new(dest) {
            Ok(ssh) => ssh,
            Err(e) => {
                ui::render_error(&e);
                return;
            }
        };
        let worker = spawn_worker(dest.to_string(), ssh.clone(), self.signals.clone());
        self.workers.insert(
            dest.to_string(),
            Worker {
                tx: worker.0,
                handle: worker.1,
                ssh,
                jobs: Vec::new(),
            },
        );
    }

    /// A worker that panicked leaves the main thread's heartbeat fresh
    /// and every `status.json` it wrote still saying "up". So the loop
    /// asks, every tick, whether the threads it is reporting for are
    /// still there — and starts them again if they are not.
    fn keep_workers_alive(&mut self) {
        let dead: Vec<String> = self
            .workers
            .iter()
            .filter(|(_, w)| w.handle.is_finished())
            .map(|(dest, _)| dest.clone())
            .collect();
        for dest in dead {
            ui::warn(&format!(
                "the worker for {dest} stopped unexpectedly — starting it again"
            ));
            let Some(old) = self.workers.remove(&dest) else {
                continue;
            };
            let (tx, handle) = spawn_worker(dest.clone(), old.ssh.clone(), self.signals.clone());
            let _ = tx.send(Nudge::Jobs(old.jobs.clone()));
            self.workers.insert(
                dest,
                Worker {
                    tx,
                    handle,
                    ssh: old.ssh,
                    jobs: old.jobs,
                },
            );
        }
    }

    fn wake(&mut self, hard: bool) {
        if self.workers.is_empty() {
            return;
        }
        if hard {
            ui::dim("the clock jumped — assuming every connection is dead and rebuilding");
        }
        for w in self.workers.values() {
            let _ = w.tx.send(Nudge::Wake { hard });
        }
    }

    fn on_files(&mut self, result: DebounceEventResult) {
        let mut touched: BTreeSet<String> = BTreeSet::new();
        match result {
            // The OS says "I dropped events, rescan" — a rescan event
            // carries NO paths, so a path-based filter drops it in
            // silence. Everything gets reconciled instead, which is the
            // only honest answer to "you missed something".
            Ok(events) if events.iter().any(|e| e.need_rescan()) => {
                touched.extend(self.watched.keys().cloned());
            }
            Ok(events) => {
                for path in events.iter().flat_map(|e| e.paths.iter()) {
                    for (stack, watch) in &self.watched {
                        if watch.covers(path) {
                            touched.insert(stack.clone());
                        }
                    }
                }
            }
            // Watcher errors mean unknown missed events: reconcile.
            Err(errors) => {
                ui::warn(&format!(
                    "file watcher hiccup ({} error(s)) — reconciling everything",
                    errors.len()
                ));
                touched.extend(self.watched.keys().cloned());
            }
        }
        for stack in touched {
            self.nudge(&stack);
        }
    }

    fn on_watch(
        &mut self,
        debouncer: &mut Debouncer,
        stack: String,
        dirs: Vec<PathBuf>,
        excluded: Vec<PathBuf>,
    ) {
        if let Some(old) = self.watched.get(&stack)
            && old.dirs == dirs
        {
            self.watched.insert(stack, Watch { dirs, excluded });
            return;
        }
        self.unwatch(debouncer, &stack);
        for dir in &dirs {
            // Two `-p` stacks from one checkout watch the same bytes.
            // `unwatch` is path-wide, not owner-aware: subscribing twice
            // and retiring one stack would silently blind the other. One
            // physical watch therefore serves every logical stack.
            let already_watched = self.watched.values().any(|w| w.dirs.contains(dir));
            if !already_watched && let Err(e) = debouncer.watch(dir, RecursiveMode::Recursive) {
                ui::warn(&format!("cannot watch {}: {e}", dir.display()));
            }
        }
        self.watched.insert(stack, Watch { dirs, excluded });
    }

    fn unwatch(&mut self, debouncer: &mut Debouncer, stack: &str) {
        if let Some(old) = self.watched.remove(stack) {
            for dir in &old.dirs {
                if !self.watched.values().any(|w| w.dirs.contains(dir)) {
                    let _ = debouncer.unwatch(dir);
                }
            }
        }
    }

    fn retire(&mut self, debouncer: &mut Debouncer, stack: &str) {
        self.unwatch(debouncer, stack);
        self.jobs.remove(stack);
        self.built.remove(stack);
    }

    fn nudge(&self, stack: &str) {
        let Some(job) = self.jobs.get(stack) else {
            return;
        };
        let Ok(dest) = job.project.ssh_dest() else {
            return;
        };
        if let Some(w) = self.workers.get(&dest) {
            let _ = w.tx.send(Nudge::Dirty(stack.to_string()));
        }
    }

    fn complain(&mut self, stack: &str, what: &str) {
        if self.complained.get(stack).map(String::as_str) == Some(what) {
            return;
        }
        self.complained.insert(stack.to_string(), what.to_string());
        ui::warn(&format!("stack {stack}: {what}"));
    }
}

impl Watch {
    fn covers(&self, path: &Path) -> bool {
        self.dirs.iter().any(|d| path.starts_with(d))
            && !self.excluded.iter().any(|ex| path.starts_with(ex))
    }
}

type Debouncer = notify_debouncer_full::Debouncer<
    notify_debouncer_full::notify::RecommendedWatcher,
    notify_debouncer_full::RecommendedCache,
>;

// ─── one destination ────────────────────────────────────────────────

fn spawn_worker(
    dest: String,
    ssh: Ssh,
    signals: Sender<Signal>,
) -> (Sender<Nudge>, std::thread::JoinHandle<()>) {
    let (tx, rx) = channel::<Nudge>();
    let handle = std::thread::Builder::new()
        .name(format!("ulak:{dest}"))
        .spawn(move || Dest::new(dest, ssh, signals).run(rx))
        .expect("cannot start a worker thread");
    (tx, handle)
}

/// How the link to one destination is doing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Link {
    Up,
    Down {
        attempts: u32,
    },
    /// The ladder ran out. Still probed, just rarely — and the word is
    /// different from "down" on purpose, because "it has been failing
    /// for ten minutes" is a different sentence from "it just failed".
    Blocked,
}

impl Link {
    fn word(self) -> &'static str {
        match self {
            Link::Up => "up",
            Link::Down { .. } => "down",
            Link::Blocked => "blocked",
        }
    }
}

/// Everything the service knows about one Docker stack while it runs.
struct Live {
    project: Project,
    fp: Option<Footprint>,
    tunnels: Option<Tunnels>,
    /// A file changed and the reconcile has not happened yet. Cleared
    /// only once the lock is actually in hand — a tick skipped for a
    /// human's `up` must not swallow the save that triggered it.
    dirty: bool,
    due: Instant,
    stack_up: bool,
    /// The services the last probe saw a running, non-one-off container
    /// for — the difference between a tunnel that is OPEN and one that
    /// ANSWERS. The tunnel set deliberately follows the compose model,
    /// not the containers (holding a later service's port from the first
    /// moment is the feature), so this is the only thing that can say
    /// "open, but nobody home". Filled where `stack_up` is, cleared
    /// where the tunnels are dropped.
    running_services: BTreeSet<String>,
    /// Why this stack is not being maintained, if it is not.
    blocked: Option<String>,
    last_sync: u64,
    pulled_total: u64,
    trouble: Option<String>,
    /// Warnings already spoken. The service runs for weeks; saying the
    /// same thing every fifteen seconds is how a log becomes unreadable.
    said: BTreeSet<String>,
    watched: Vec<PathBuf>,
}

impl Live {
    fn new(project: Project, id: &str) -> Live {
        // Carry the counters across a restart: "142 files came back
        // while you were away" is the one number a user cannot
        // reconstruct afterwards, and resetting it on every service
        // start would quietly make it a lie.
        let previous = intent::read_status(id);
        Live {
            project,
            fp: None,
            tunnels: None,
            dirty: true, // a fresh service reconciles once, on purpose
            due: Instant::now(),
            stack_up: false,
            running_services: BTreeSet::new(),
            blocked: None,
            last_sync: previous.as_ref().map(|s| s.last_sync_unix).unwrap_or(0),
            pulled_total: previous.as_ref().map(|s| s.pulled_total).unwrap_or(0),
            trouble: None,
            said: BTreeSet::new(),
            watched: Vec::new(),
        }
    }

    fn say(&mut self, msg: &str) {
        if self.said.insert(msg.to_string()) {
            ui::warn(msg);
        }
    }
}

struct Dest {
    dest: String,
    ssh: Ssh,
    signals: Sender<Signal>,
    link: Link,
    since: u64,
    next_probe: Instant,
    stacks: BTreeMap<String, Live>,
    /// Why the link is down, when ssh said something worth repeating.
    /// It belongs to the destination and not to a stack: a key the
    /// server will not take is not one project's problem.
    trouble: Option<String>,
    /// The last one said out loud. A service runs for weeks; repeating
    /// the same sentence every two seconds is how a log stops being read.
    said_trouble: Option<String>,
}

impl Dest {
    fn new(dest: String, ssh: Ssh, signals: Sender<Signal>) -> Dest {
        Dest {
            dest,
            ssh,
            signals,
            link: Link::Down { attempts: 0 },
            since: intent::now_unix(),
            next_probe: Instant::now(),
            stacks: BTreeMap::new(),
            trouble: None,
            said_trouble: None,
        }
    }

    fn run(mut self, rx: Receiver<Nudge>) {
        loop {
            match rx.recv_timeout(self.wait()) {
                Ok(Nudge::Jobs(jobs)) => self.take(jobs),
                Ok(Nudge::Dirty(id)) => {
                    if let Some(m) = self.stacks.get_mut(&id) {
                        m.dirty = true;
                    }
                }
                // `step()` runs at the bottom of this loop, so the probe
                // is the very next thing that happens.
                //
                // The ladder restarts too, deliberately matching
                // `on_wake`'s soft path below: somebody typing a command
                // is the same signal as a lid opening — the world may
                // have moved. Without it every doorbell rung during an
                // outage BURNS a rung (`link_failed` counts each failed
                // probe), so five `ulak docker compose ps` while the wifi is out
                // would declare the link Blocked — a sentence about ten
                // minutes of failure, said about seventy seconds of it —
                // and then hold the tunnels shut for the five-minute
                // plateau after the wifi came back.
                Ok(Nudge::Look) => {
                    if let Link::Down { .. } | Link::Blocked = self.link {
                        self.set_link(Link::Down { attempts: 0 });
                    }
                    self.next_probe = Instant::now();
                }
                Ok(Nudge::Wake { hard }) => self.on_wake(hard),
                Err(RecvTimeoutError::Timeout) => {}
                // The main loop is gone; so is any reason to keep
                // running. Dropping the stacks closes the tunnels.
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.step();
        }
    }

    /// How long to sleep before something is due. The channel wakes us
    /// earlier whenever a file changes, so this only bounds the idle
    /// cadences.
    fn wait(&self) -> Duration {
        if self.stacks.is_empty() {
            return TICK * 5;
        }
        let now = Instant::now();
        let mut next = self.next_probe;
        if self.link == Link::Up {
            // `blocked` is why this is not simply `stack_up`. A stack
            // refused for a UUID mismatch is still RUNNING on the server,
            // so `probe` keeps `stack_up` true, while `tend` returns at
            // its first line and never reaches the `reconcile` that moves
            // `due` — leaving a `due` in the past that drags this sleep
            // down to the 50 ms floor below, for as long as the mismatch
            // stands (which is until a human removes the local `uuid`
            // file). And this sleep belongs to the whole DESTINATION, so
            // every other stack on this worker is tended twenty times
            // a second along with it. Nothing is owed to a stack
            // nothing can advance: only a probe clears a mismatch, and
            // `next_probe` already schedules that — the same exclusion
            // `probe` makes when it settles its own cadence.
            for m in self
                .stacks
                .values()
                .filter(|m| m.stack_up && m.blocked.is_none())
            {
                next = next.min(m.due);
            }
        }
        next.saturating_duration_since(now)
            .max(Duration::from_millis(50))
    }

    fn take(&mut self, jobs: Vec<Job>) {
        let wanted: BTreeSet<String> = jobs.iter().map(|j| j.id.clone()).collect();
        // Dropping a Live closes its tunnels — that is how `ulak
        // down` gets the ports back without anyone sending a signal.
        self.stacks.retain(|id, _| wanted.contains(id));
        for job in jobs {
            match self.stacks.get_mut(&job.id) {
                // The intent changed under us (a new -f, a new profile):
                // take the new project, keep the counters.
                Some(live) => live.project = job.project,
                None => {
                    let live = Live::new(job.project, &job.id);
                    self.stacks.insert(job.id, live);
                }
            }
        }
    }

    /// The lid opened, or the clock stepped.
    ///
    /// Hard means the connection is not interrogated, it is buried:
    /// retire the master, kill the tunnels, probe now. Waiting for ssh
    /// to notice by itself costs the 72 seconds the master needs to
    /// declare a dead link, and `-O check` lies for every one of them.
    fn on_wake(&mut self, hard: bool) {
        if hard {
            for m in self.stacks.values_mut() {
                m.tunnels = None;
                // The sensor died with the connection: what was running
                // before the lid closed is a memory, not a report.
                m.running_services.clear();
                m.dirty = true;
            }
            self.ssh.close_master();
            self.set_link(Link::Down { attempts: 0 });
        } else if let Link::Down { .. } | Link::Blocked = self.link {
            // A short jump does not tear anything down, but it does mean
            // the world may have changed — so the ladder starts over
            // rather than making a laptop that just woke wait out a
            // five-minute plateau.
            self.set_link(Link::Down { attempts: 0 });
        }
        self.next_probe = Instant::now();
    }

    fn step(&mut self) {
        if self.stacks.is_empty() {
            return;
        }
        if Instant::now() >= self.next_probe {
            self.probe();
            self.publish();
        }
        if self.link != Link::Up {
            return;
        }
        let ids: Vec<String> = self.stacks.keys().cloned().collect();
        let mut worked = false;
        for id in ids {
            worked |= self.tend(&id);
        }
        if worked {
            self.publish();
        }
    }

    // ─── the probe ──────────────────────────────────────────────────

    fn probe(&mut self) {
        // Decision 2: when every live stack on this server is down, this
        // destination is meant to be SILENT — and a probe that opens or
        // renews a shared master is not silence.
        let silent = self.stacks.values().all(|m| !m.stack_up);
        let script = self.probe_script();
        // The ladder spaces ATTEMPTS, not gaps between them. A probe
        // against an unreachable server spends `ConnectTimeout` — ten
        // measured seconds — going nowhere, and that time is already
        // everything backoff exists to buy. Counting it twice made a
        // healed link take three rungs to notice.
        let attempted = Instant::now();
        let result = if silent {
            self.ssh.probe_direct(&script)
        } else {
            self.ssh.probe(&script)
        };

        let out = match result {
            Ok(out) if !out.timed_out && out.status.code() != Some(255) => out,
            // ssh's own words are the difference between "the server is
            // asleep" and "your key is locked" — two silences that look
            // identical from the outside and need opposite answers.
            Ok(out) => {
                let why = ssh_trouble(&self.dest, &String::from_utf8_lossy(&out.stderr));
                return self.link_failed(attempted, why);
            }
            Err(_) => return self.link_failed(attempted, None),
        };
        let sighting = parse_probe(&String::from_utf8_lossy(&out.stdout));

        let was = self.link;
        self.trouble = None;
        self.said_trouble = None;
        self.set_link(Link::Up);
        if !matches!(was, Link::Up) {
            for m in self.stacks.values_mut() {
                // Everything on the far side may have moved while we
                // were away: reconcile before believing the stack.
                m.dirty = true;
                m.said.clear();
            }
        }
        let dest = self.dest.clone();
        for (id, m) in &mut self.stacks {
            let identity = m.project.compose_identity();
            let was_up = m.stack_up;
            let containers = sighting
                .containers
                .get(&identity)
                .map(Vec::as_slice)
                .unwrap_or_default();
            m.stack_up = if sighting.labels_readable {
                !containers.is_empty()
            } else {
                was_up
            };
            if sighting.labels_readable {
                // The same rows, one more question: which services those
                // containers vouch for. Unreadable labels keep the old
                // answer for the same reason `stack_up` keeps its own.
                m.running_services = crate::compose::running_services(containers);
            }
            if was_up && !m.stack_up {
                // Not an error: `ulak docker compose down` looks exactly like this.
                m.tunnels = None;
                m.said.clear();
            }
            if !was_up && m.stack_up {
                // Whatever we last complained about belonged to a stack
                // that was not running; carrying it forward would leave
                // `status` explaining a state that has just ended.
                m.trouble = None;
                m.said.clear();
            }
            let workspace_id = m.project.workspace_id();
            m.blocked = if !sighting.labels_readable {
                Some(format!(
                    "Docker's Compose ownership labels on {dest} could not be read, so the service will not sync or keep tunnels — inspect them: ssh {dest} docker ps"
                ))
            } else {
                let sync = crate::management::root_command(&m.project, &["sync"]);
                mismatch(workspace_id, sighting.manifests.get(id), &dest, &sync)
                    .or_else(|| running_workspace_mismatch(&m.project, containers, &dest))
            };
            if let Some(why) = m.blocked.clone() {
                m.tunnels = None;
                m.say(&why);
            }
        }
        // Settled after the stacks are, not before: the cadence is a
        // consequence of what this round just learned.
        let busy = sighting.engine
            && (!sighting.labels_readable
                || self
                    .stacks
                    .values()
                    .any(|m| m.stack_up && m.blocked.is_none()));
        self.next_probe = Instant::now()
            + if busy {
                PROBE_EVERY
            } else {
                SILENT_PROBE_EVERY
            };
    }

    /// One round trip for the whole destination: every running Compose
    /// container's project/config/working-dir labels, and what each
    /// workspace's manifest says.
    ///
    /// Deliberately NOT through `config::bind`. That path resolves the
    /// compose model, and a stale footprint cache turns this 0.27-second
    /// question into a bootstrap of 3–13 remote calls — for a stack that
    /// may not even be running. (Re-measured after the round became
    /// `docker ps --format`: 0.25–0.28 s over a warm control socket,
    /// Docker 29.6.2, 21 Compose containers on the destination.)
    fn probe_script(&self) -> String {
        let mut script = ps_one_round();
        for (id, m) in &self.stacks {
            script.push_str("printf '==ULAK:manifest %s==\\n' ");
            script.push_str(&sh_quote(id));
            script.push('\n');
            script.push_str("cat ");
            script.push_str(&sh_quote(&m.project.remote_manifest_path()));
            script.push_str(" 2>/dev/null\necho\n");
        }
        script
    }

    fn link_failed(&mut self, attempted: Instant, why: Option<String>) {
        // Said once per distinct sentence, and kept for `status` — a
        // service with no terminal has nowhere else to be heard, and a
        // locked key must not turn into a silent climb up the ladder.
        if let Some(why) = &why
            && self.said_trouble.as_ref() != Some(why)
        {
            self.said_trouble = Some(why.clone());
            ui::warn(why);
        }
        self.trouble = why;
        let attempts = match self.link {
            Link::Down { attempts } => attempts + 1,
            Link::Up => {
                // The master carried the command that just failed, so it
                // is the prime suspect rather than an innocent
                // bystander — and with the tunnels off that socket,
                // retiring it costs nothing but a reconnect.
                self.ssh.close_master();
                ui::warn(&format!(
                    "{} stopped answering — retrying, and everything will be rebuilt when it is back",
                    self.dest
                ));
                1
            }
            Link::Blocked => BACKOFF.len() as u32 + 1,
        };
        // The tunnels cannot outlive the link they run over, and leaving
        // them holding local ports would make `localhost:8080` a socket
        // that connects and then answers nothing.
        for m in self.stacks.values_mut() {
            m.tunnels = None;
            m.running_services.clear();
        }
        let (rung, link) = match BACKOFF.get(attempts as usize - 1) {
            Some(secs) => (Duration::from_secs(*secs), Link::Down { attempts }),
            None => (BLOCKED_EVERY, Link::Blocked),
        };
        self.set_link(link);
        // From when the attempt BEGAN: a rung the failed connect already
        // outlasted has been served.
        self.next_probe = (attempted + rung).max(Instant::now());
        self.publish();
    }

    /// `since` follows the WORD, not the attempt count: `status` says
    /// "down for 3 minutes", and a counter that reset the clock on every
    /// failed retry would make that read "down for 2 seconds" forever.
    fn set_link(&mut self, link: Link) {
        if self.link.word() != link.word() {
            self.since = intent::now_unix();
        }
        self.link = link;
    }

    // ─── one workspace, one pass ───────────────────────────────────────

    /// Returns whether anything happened worth republishing.
    fn tend(&mut self, id: &str) -> bool {
        let Some(m) = self.stacks.get(id) else {
            return false;
        };
        if m.blocked.is_some() {
            return false;
        }
        // Decision 2, and the whole of the measured nightly cost: a
        // stack that is not running gets nothing at all.
        if !m.stack_up {
            let up = crate::management::compose_command(&m.project, &["up", "-d"]);
            let had = m.tunnels.is_some();
            if let Some(m) = self.stacks.get_mut(id) {
                m.tunnels = None;
                m.trouble = Some(format!(
                    "{STACK_DOWN} {} — nothing is synced or tunneled until: {up}",
                    self.dest,
                ));
            }
            return had;
        }
        if !self.resolve(id) {
            // `wait()` sleeps until the earliest `due` of every live
            // stack, so returning with a `due` still in the past is
            // not "try again later" — it is trying again at the 50 ms
            // floor, forever. That costs more here than anywhere else:
            // a failed resolve caches nothing, so every one of those
            // passes pays the full bootstrap round trip against the
            // server for as long as the compose file stays broken.
            if let Some(m) = self.stacks.get_mut(id) {
                m.due = Instant::now() + RESOLVE_FAILED_RETRY;
            }
            return true;
        }
        let tunnels_changed = self.tunnel(id);

        let Some(m) = self.stacks.get(id) else {
            return tunnels_changed;
        };
        if !m.dirty && Instant::now() < m.due {
            return tunnels_changed;
        }
        self.reconcile(id);
        true
    }

    /// Make sure the footprint is current, and tell the main thread what
    /// to watch when it moved.
    ///
    /// `resolve_cached` re-resolves by itself when a compose or env file
    /// changed, so editing one moves the watched set with it — no
    /// separate "did a model file change?" bookkeeping, which is where
    /// `watch` used to get it wrong.
    fn resolve(&mut self, id: &str) -> bool {
        let Dest {
            ssh,
            dest,
            stacks,
            signals,
            ..
        } = self;
        let Some(m) = stacks.get_mut(id) else {
            return false;
        };
        let fp = match footprint::resolve_cached(&m.project, ssh, dest) {
            Ok(fp) => fp,
            Err(e) => {
                let msg = ui::flatten(&e);
                m.trouble = Some(msg.clone());
                m.say(&msg);
                return false;
            }
        };
        m.project.adopt_existing_stack(&fp);
        let dirs = fp.watch_dirs();
        if dirs != m.watched {
            let excluded = excluded_subtrees(&m.project, &fp);
            m.watched = dirs.clone();
            let _ = signals.send(Signal::Watch {
                stack: id.to_string(),
                dirs,
                excluded,
            });
        }
        m.fp = Some(fp);
        true
    }

    fn tunnel(&mut self, id: &str) -> bool {
        let Dest { ssh, stacks, .. } = self;
        let Some(m) = stacks.get_mut(id) else {
            return false;
        };
        if !m.project.config.forward.auto {
            m.tunnels = None;
            return false;
        }
        let Some(fp) = &m.fp else { return false };
        let doctor = crate::management::root_command(&m.project, &["doctor"]);
        let plan = match forward::plan(fp, &doctor) {
            Ok(plan) => plan,
            Err(e) => {
                m.say(&ui::flatten(&e));
                return false;
            }
        };
        for w in &plan.warnings {
            m.say(w);
        }

        // A healthy set that already covers the plan is left alone —
        // unless a port it had to leave behind has since been freed.
        // `covers` counts a blocked port as covered, so a partially
        // blocked set matched and this returned before any probe: the
        // user quit whatever held 5432 and ulak went on reporting it as
        // NOT tunneled until the stack cycled or the service restarted.
        let mut freed = false;
        if let Some(open) = &mut m.tunnels
            && !open.died()
            && open.covers(&plan.wanted)
        {
            freed = open.a_blocked_port_came_free();
            if !freed {
                return false;
            }
        }
        let stopped = m.tunnels.take().is_some_and(|t| t.was_running());
        if freed {
            m.say("a port that was busy is free again — opening the tunnels for it");
        } else if stopped {
            m.say("the port tunnels stopped — opening them again");
        }
        if plan.wanted.is_empty() {
            return false;
        }
        match Tunnels::open(ssh, id, plan.wanted) {
            Ok(tunnels) => {
                for t in tunnels.blocked() {
                    // Named, never skipped in silence: a forward ulak
                    // announced but did not make is the failure class
                    // this product exists to remove.
                    m.say(&format!(
                        "{} NOT tunneled ({}) — something on this machine already listens there; who: lsof -i :{}",
                        t.local_port, t.service, t.local_port
                    ));
                }
                m.tunnels = Some(tunnels);
                true
            }
            Err(e) => {
                m.say(&ui::flatten(&e));
                false
            }
        }
    }

    fn reconcile(&mut self, id: &str) {
        let Dest { ssh, stacks, .. } = self;
        let Some(m) = stacks.get_mut(id) else {
            return;
        };
        let Some(fp) = m.fp.clone() else { return };

        // The service never waits for a lock. A human's `up` is holding
        // it; the next tick is seconds away and `dirty` is still set, so
        // nothing is lost by stepping aside.
        //
        // Moving `due` is what makes that sentence TRUE. `wait()` sleeps
        // until the earliest of `next_probe` and every live stack's
        // `due`, with a 50 ms floor — so returning here without touching
        // a `due` that is already in the past is not stepping aside, it
        // is spinning this thread at 20 Hz. Each of those passes re-reads
        // the footprint cache, re-parses the compose model and rewrites
        // status.json, for as long as the lock is held; for `up` that is
        // the whole pull back. The hole predates the doorbell, but the
        // doorbell is what makes the service arrive here on every `up`.
        let workspace_id = m.project.workspace_id();
        let lock = match WorkspaceLock::try_acquire(workspace_id) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                m.due = Instant::now() + LOCK_HELD_RETRY;
                return;
            }
            Err(e) => {
                m.say(&ui::flatten(&e));
                m.due = Instant::now() + LOCK_HELD_RETRY;
                return;
            }
        };
        m.dirty = false;
        m.due = Instant::now() + IDLE_RECONCILE;

        let result = sync::reconcile_once(
            &m.project,
            &fp,
            ssh,
            &sync::SyncOptions {
                dry_run: false,
                max_delete_override: None,
                quiet: true,
                // Never Ask: a service has no terminal to ask in, and a
                // deletion budget must never be the reason a workspace
                // stops being maintained.
                over_budget: sync::OverBudget::Report,
            },
        );
        drop(lock);

        match result {
            Ok(report) => {
                m.last_sync = intent::now_unix();
                m.pulled_total += report.pulled as u64;
                m.trouble = None;
                m.said.remove("reconcile");
                if report.pushed + report.deleted + report.pulled > 0 {
                    ui::dim(&format!(
                        "{id}: {} pushed, {} deleted, {} came back",
                        report.pushed, report.deleted, report.pulled
                    ));
                }
                if report.pending_deletions > 0 {
                    let budget = format!("--max-delete={}", report.pending_deletions);
                    let sync = crate::management::root_command(&m.project, &["sync", &budget]);
                    m.trouble = Some(format!(
                        "{} file(s) the project no longer has are still on the server (over the deletion budget) — clear them when ready: {sync}",
                        report.pending_deletions,
                    ));
                }
            }
            Err(e) => {
                let msg = ui::flatten(&e);
                m.trouble = Some(msg.clone());
                m.say(&msg);
                // A failed reconcile is still owed: keep it dirty rather
                // than waiting out the idle cadence.
                m.dirty = true;
            }
        }
    }

    /// Write what `ulak status` reads. One writer per file — this
    /// worker owns every stack on this destination — so the atomic
    /// rename in `intent` is the whole of the concurrency story.
    fn publish(&self) {
        for (id, m) in &self.stacks {
            let mut tunnels = m.tunnels.as_ref().map(Tunnels::states).unwrap_or_default();
            // Stamped HERE, not in forward.rs: liveness is the probe's
            // answer, and `states()` stays the owner of "did I make this
            // forwarding". A second computation path in the tunnel
            // engine is what the One-Answer rule exists to prevent.
            mark_service_running(&mut tunnels, &m.running_services);
            let note = m
                .blocked
                .clone()
                // Before the stack's own trouble: when the link is the
                // problem, everything else a stack could say is a
                // consequence of it.
                .or_else(|| self.trouble.clone())
                .or_else(|| m.trouble.clone())
                .or_else(|| {
                    let shut: Vec<String> = tunnels
                        .iter()
                        .filter(|t| !t.open)
                        .map(|t| t.port.to_string())
                        .collect();
                    (!shut.is_empty()).then(|| {
                        format!(
                            "port(s) taken on this machine, not tunneled: {}",
                            shut.join(", ")
                        )
                    })
                });
            let _ = intent::write_status(
                id,
                &Status {
                    schema: intent::SCHEMA,
                    updated_unix: intent::now_unix(),
                    connection: self.link.word().to_string(),
                    since_unix: self.since,
                    tunnels,
                    last_sync_unix: m.last_sync,
                    pulled_total: m.pulled_total,
                    note,
                },
            );
        }
    }
}

/// The probe's liveness answer, stamped onto the tunnel report the CLI
/// reads. Three states leave here: open and answered, open with nobody
/// behind it (`Some(false)`), and not open at all — where the first two
/// used to be one line, and a user debugged a server-side service they
/// had never started. A blocked port is stamped too: the answer is just
/// as true for it, and a special case would be a rule nobody needs.
fn mark_service_running(tunnels: &mut [crate::intent::TunnelState], running: &BTreeSet<String>) {
    for t in tunnels {
        t.service_running = Some(running.contains(&t.service));
    }
}

/// Subtrees whose churn must not wake a sync — `node_modules`, `.git`
/// internals, whatever the ignore rules exclude. Computed where the
/// footprint is, so the main thread's event triage stays a prefix match.
fn excluded_subtrees(project: &Project, fp: &Footprint) -> Vec<PathBuf> {
    let build = fp.build_filter();
    fp.sync_dirs()
        .iter()
        .flat_map(|dir| {
            crate::walk::plan_in(&fp.anchor, dir, &project.config.sync, &build)
                .map(|plan| {
                    plan.excludes
                        .iter()
                        .map(|rel| fp.anchor.join(String::from_utf8_lossy(rel).as_ref()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect()
}

// ─── reading the probe ──────────────────────────────────────────────

/// Every RUNNING Compose container's ownership labels, in one SSH round
/// and one DOCKER round. No compose model and no per-container
/// subprocess participates; `compose::running_labels_script` owns the
/// label format so `clean` and `up` cannot ask a different question.
///
/// The single docker process is load-bearing, not a saving: asking `ps`
/// for the ids and `inspect` for their labels lets a container end in
/// between, and a nonzero `inspect` then reads as "this destination's
/// labels are unreadable" — which blocks every stack on it and drops
/// every tunnel. See `compose::label_format`.
fn ps_one_round() -> String {
    format!(
        "{} 2>/dev/null || echo ULAK_NO_ENGINE\n",
        crate::compose::running_labels_script()
    )
}

const MANIFEST_MARK: &str = "==ULAK:manifest ";

#[derive(Debug, Default, PartialEq)]
struct Sighting {
    engine: bool,
    labels_readable: bool,
    /// Compose project → every running container carrying that project
    /// label. Keeping the rows, rather than only the project names, is what
    /// lets each job prove the stack uses its transported workspace.
    containers: BTreeMap<String, Vec<crate::compose::ContainerLabels>>,
    /// stack id → the UUID its sync workspace's manifest carries. Absent means
    /// there is no manifest there at all, which is simply "not synced
    /// yet" and never a conflict.
    manifests: BTreeMap<String, String>,
}

fn parse_probe(text: &str) -> Sighting {
    let mut s = Sighting {
        engine: true,
        labels_readable: true,
        ..Sighting::default()
    };
    let mut workspace: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(MANIFEST_MARK) {
            workspace = rest.strip_suffix("==").map(str::to_string);
            continue;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match &workspace {
            Some(id) => {
                if let Some(uuid) = uuid_of(line) {
                    s.manifests.insert(id.clone(), uuid);
                }
            }
            None if line == "ULAK_NO_ENGINE" => s.engine = false,
            None => {
                let Ok(labels) = serde_json::from_str::<crate::compose::ContainerLabels>(line)
                else {
                    s.labels_readable = false;
                    continue;
                };
                if let Some(project) = crate::compose::container_project(&labels) {
                    s.containers
                        .entry(project.to_string())
                        .or_default()
                        .push(labels);
                }
            }
        }
    }
    s
}

fn uuid_of(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(value.get("uuid")?.as_str()?.to_string())
}

fn running_workspace_mismatch(
    project: &Project,
    containers: &[crate::compose::ContainerLabels],
    dest: &str,
) -> Option<String> {
    let ownership = crate::compose::workspace_use(containers, &project.remote_workspace_root());
    let kind = match ownership {
        crate::compose::WorkspaceUse::Absent | crate::compose::WorkspaceUse::All => return None,
        crate::compose::WorkspaceUse::Other => "a different transported workspace",
        crate::compose::WorkspaceUse::Mixed => "mixed transported workspaces",
    };
    let recreate = crate::management::compose_command(
        project,
        &["up", "-d", "--force-recreate", "--remove-orphans"],
    );
    Some(format!(
        "Docker project {} on {dest} is running from {kind}, not wholly from {} — the service will not sync this workspace or keep its tunnels. From the checkout that should own the whole stack, recreate it: {recreate}",
        project.compose_identity(),
        project.remote_dir_shown()
    ))
}

/// Does the workspace on the server still belong to this machine?
///
/// Automatic client namespaces keep ordinary independent machines apart.
/// Two clients can deliberately configure the same namespace, however, or
/// copy/reset their local state, and then the manifest's identity and
/// local_root can be identical for both. Only the UUID can tell those claims
/// apart, and only if each client kept the one it minted.
///
/// The cost of being wrong is not a one-off accident: two services
/// running around the clock would push and delete each other's files
/// forever. So a workspace that fails this is not taken live, and the
/// reason is said out loud rather than shown as a mysterious churn.
fn mismatch(id: &str, on_server: Option<&String>, dest: &str, sync: &str) -> Option<String> {
    let server_uuid = on_server?;
    let mine = crate::invocation::recorded_uuid(id)?;
    if *server_uuid == mine {
        return None;
    }
    // Measured on a real project: the old wording said "ANOTHER machine"
    // and offered `ulak clean` — destroying the workspace — as the only
    // way out. Both were wrong for the case that actually happened: this
    // machine's own v0.3 workspace, whose local record had been lost. The
    // sentence now names both readings, and the way back is the one that
    // keeps the data.
    Some(format!(
        "this machine's claim on the workspace at {dest} does not match the one there, so the service will not touch it — two clients maintaining one workspace delete each other's files forever. If that workspace is somebody else's, configure a different [workspace] namespace here. If it is YOURS (this machine refused it once, or its state was reset), take it back: rm {}   then: {sync}",
        crate::intent::workspace_dir_path(id)
            .map(|d| d.join("uuid").display().to_string())
            .unwrap_or_else(|| format!("~/.local/state/ulak/workspaces/{id}/uuid"))
    ))
}

/// What ssh said, turned into a sentence with a way out of it.
///
/// Measured for this phase, because it never had been: a
/// passphrase-protected key, with no agent and no terminal, produces **no
/// prompt at all**. ssh skips that key in silence and falls through to
/// the next method, so a LOCKED key and a WRONG key leave byte-for-byte
/// the same stderr — `Permission denied (publickey,password)`. There is
/// one signature to match, and its sentence has to carry both readings.
///
/// Without this, the whole class arrives as silence: a service that has
/// been climbing the backoff ladder since the last reboot, because the
/// keychain was not unlocked yet when it started.
fn ssh_trouble(dest: &str, stderr: &str) -> Option<String> {
    let said = |s: &str| stderr.contains(s);
    if said("Permission denied") || said("Too many authentication failures") {
        return Some(format!(
            "{dest} refused every ssh key — and a service running in the background has no terminal to type a passphrase into. If your key has one, unlock it once: ssh-add   (to see what the server says: ssh {dest} true)"
        ));
    }
    if said("sign_and_send_pubkey") || said("agent refused operation") {
        return Some(format!(
            "the ssh agent would not sign for {dest} — the key it was holding is locked or gone. Add it again: ssh-add"
        ));
    }
    if said("REMOTE HOST IDENTIFICATION HAS CHANGED") || said("Host key verification failed") {
        return Some(format!(
            "{dest} is presenting a different host key than the one in ~/.ssh/known_hosts, so Ulak will not connect. If you rebuilt that server on purpose: ssh-keygen -R {dest}"
        ));
    }
    if said("Could not resolve hostname") {
        return Some(format!(
            "{dest} does not resolve on this network — check the name, or whether your VPN is up"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace nothing can advance must not set the sleep floor.
    ///
    /// `wait()` sleeps until the earliest `due` of the workspaces it
    /// looks at, floored at 50 ms, and that sleep belongs to the whole
    /// DESTINATION. So a `due` left in the past is not "try again soon"
    /// — it is twenty passes a second, over every workspace on the
    /// worker. Two paths left one there: a UUID mismatch, where `tend`
    /// returns at its first line and only a probe can ever clear the
    /// state; and a failed resolve, which is the expensive one, because
    /// nothing is cached and each pass pays a full bootstrap round trip
    /// to the server.
    ///
    /// Pinned on the source because the alternative is a live `Dest`,
    /// which needs a server, an ssh link and a resolved project — and
    /// what broke here was one missing clause and one missing
    /// assignment, both of which read plainly right here.
    #[test]
    fn nothing_that_cannot_advance_drags_the_sleep_to_its_floor() {
        let mine = include_str!("service.rs");

        let at = mine.find("fn wait(&self)").expect("wait exists");
        let wait = &mine[at..][..mine[at..].find("\n    }\n").expect("wait ends")];
        assert!(
            wait.contains("blocked.is_none()"),
            "wait() paces itself by a workspace that is blocked, and only a probe \
             can unblock one — so every other workspace on this destination is \
             tended at the 50 ms floor with it"
        );

        let at = mine.find("fn tend(&mut self").expect("tend exists");
        let tend = &mine[at..][..mine[at..].find("\n    }\n").expect("tend ends")];
        let resolve_at = tend.find("if !self.resolve(id)").expect("tend resolves");
        let arm = &tend[resolve_at..][..tend[resolve_at..]
            .find("\n        }")
            .expect("the arm ends")];
        assert!(
            arm.contains("m.due = "),
            "the failed-resolve path returns without moving `due`, so it is retried \
             at the 50 ms floor — and a failed resolve caches nothing, so each retry \
             is another rm/mkdir, rsync and `docker compose config` on the server"
        );
    }

    #[test]
    fn one_round_answers_link_stack_and_ownership() {
        // One batched `docker ps` script answers three separate
        // questions. The manifests ride along so the ownership check
        // costs no extra SSH trip.
        let text = "{\"com.docker.compose.project\":\"app-9f2\",\"com.docker.compose.project.config_files\":\"/home/dev/.ulak/workspaces/alice/a/proj/compose.yaml\",\"com.docker.compose.project.working_dir\":\"/home/dev/.ulak/workspaces/alice/a/proj\"}\n\
                    {\"com.docker.compose.project\":\"other-project\",\"com.docker.compose.project.config_files\":\"/home/dev/.ulak/workspaces/alice/b/proj/compose.yaml\",\"com.docker.compose.project.working_dir\":\"/home/dev/.ulak/workspaces/alice/b/proj\"}\n\
                    {\"com.docker.compose.project\":\"app-9f2\",\"com.docker.compose.project.config_files\":\"/home/dev/.ulak/workspaces/alice/a/proj/compose.yaml\",\"com.docker.compose.project.working_dir\":\"/home/dev/.ulak/workspaces/alice/a/proj\"}\n\
                    ==ULAK:manifest aaa==\n\
                    {\"version\":1,\"uuid\":\"U-1\",\"identity\":\"app-9f2\"}\n\
                    \n\
                    ==ULAK:manifest bbb==\n\
                    \n";
        let s = parse_probe(text);
        assert!(s.engine);
        assert_eq!(
            s.containers.keys().cloned().collect::<BTreeSet<_>>(),
            ["app-9f2", "other-project"]
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>()
        );
        assert_eq!(s.containers["app-9f2"].len(), 2);
        assert!(s.labels_readable);
        assert_eq!(s.manifests.get("aaa").map(String::as_str), Some("U-1"));
        // No manifest is "not synced yet", never a conflict.
        assert!(!s.manifests.contains_key("bbb"));
    }

    /// The compose model opens a tunnel for EVERY published port, and
    /// that is right (the port is held for the service before it
    /// starts); what was wrong was the report: `up -d web` of a
    /// two-service model listed both ports the same way, and the user
    /// went debugging a server-side service they had never started.
    /// The stamp is what tells "open and answered" from "open, nobody
    /// home" — and it must not touch what `states()` decided about the
    /// port being bound at all.
    #[test]
    fn an_open_tunnel_for_a_service_nobody_started_is_stamped_not_dressed_up() {
        let mut tunnels = vec![
            crate::intent::TunnelState {
                port: 18082,
                service: "started".into(),
                open: true,
                service_running: None,
            },
            crate::intent::TunnelState {
                port: 18083,
                service: "later".into(),
                open: true,
                service_running: None,
            },
            crate::intent::TunnelState {
                port: 5432,
                service: "started".into(),
                open: false,
                service_running: None,
            },
        ];
        let running: BTreeSet<String> = ["started".to_string()].into();
        mark_service_running(&mut tunnels, &running);
        assert_eq!(tunnels[0].service_running, Some(true));
        assert_eq!(
            tunnels[1].service_running,
            Some(false),
            "the never-started service's port must say nobody is behind it"
        );
        // The stamp answers its own question and nobody else's.
        assert!(tunnels[0].open && tunnels[1].open && !tunnels[2].open);
        assert_eq!(tunnels[2].service_running, Some(true));
    }

    #[test]
    fn a_dead_engine_is_not_an_empty_server() {
        let s = parse_probe("ULAK_NO_ENGINE\n");
        assert!(!s.engine);
        assert!(s.containers.is_empty());
    }

    /// A row Ulak cannot read is not an empty server. Treating it as one
    /// would let a stack whose ownership is UNKNOWN keep syncing and keep
    /// its tunnels — the exact assumption the workspace labels exist to
    /// stop — so the probe fails closed and `settle` blocks instead.
    /// Wide by design: a garbled row means the destination's answer as a
    /// whole is untrustworthy, not that one project is.
    #[test]
    fn an_unreadable_container_label_blocks_the_probe() {
        let s = parse_probe("not-json\n");
        assert!(!s.labels_readable);
        assert!(s.containers.is_empty());
    }

    #[test]
    fn asking_about_a_workspace_does_not_create_one() {
        // A read that writes is how 22 directories appeared in the
        // developer's real state dir, one per `cargo test` run — and,
        // with a service enumerating workspaces around the clock, they would
        // never go away again. The question must leave nothing behind.
        let unknown = format!("no-such-workspace-{}", std::process::id());
        assert!(crate::invocation::recorded_uuid(&unknown).is_none());
        let path = intent::workspace_dir_path(&unknown).expect("a state dir");
        assert!(
            !path.exists(),
            "asking about {unknown} created {}",
            path.display()
        );
    }

    #[test]
    fn a_workspace_without_a_recorded_uuid_is_never_called_foreign() {
        // The check must be silent about workspaces it cannot judge: a
        // machine that has never synced this workspace has nothing to
        // compare, and refusing on ignorance would break the first run.
        let unknown = format!("no-such-workspace-{}", std::process::id());
        assert!(mismatch(&unknown, Some(&"U-1".to_string()), "srv", "ulak sync").is_none());
        // And no manifest on the server is simply "not synced yet".
        assert!(mismatch(&unknown, None, "srv", "ulak sync").is_none());
    }

    #[test]
    fn the_backoff_ladder_climbs_and_then_plateaus() {
        // It starts fast because the common case is a link that is
        // already back, and ends slow because a server unreachable for
        // ten minutes is not reached by asking harder.
        assert!(BACKOFF.windows(2).all(|w| w[0] < w[1]), "{BACKOFF:?}");
        assert_eq!(BACKOFF[0], 2, "a woken laptop must not wait");
        assert!(
            Duration::from_secs(*BACKOFF.last().unwrap()) <= BLOCKED_EVERY,
            "the plateau must not be shorter than the last rung"
        );
    }

    #[test]
    fn only_a_real_suspend_tears_things_down() {
        let now = SystemTime::now();

        // A normal tick sees nothing.
        let mut last = now;
        assert_eq!(woke(&mut last), None);

        // An NTP step or a short VM pause: probe, do not demolish.
        let mut last = now - Duration::from_secs(30);
        assert_eq!(woke(&mut last), Some(false));

        // A closed lid: everything is assumed dead.
        let mut last = now - Duration::from_secs(3600);
        assert_eq!(woke(&mut last), Some(true));

        // A clock that stepped BACKWARDS is not a suspend.
        let mut last = now + Duration::from_secs(600);
        assert_eq!(woke(&mut last), None);
    }

    #[test]
    fn a_key_the_server_will_not_take_becomes_a_sentence_not_a_silence() {
        // The exact stderr, measured against the test server with an
        // isolated ssh config. Both runs — a key the server does not
        // know, and a passphrase-protected key with no agent and no tty
        // — printed THIS, with no prompt of any kind in between. So the
        // service cannot tell the two apart and must not pretend to.
        let measured = "Permission denied, please try again.\n\
                        Permission denied, please try again.\n\
                        dev@203.0.113.10: Permission denied (publickey,password).\n";
        let said = ssh_trouble("my-server", measured).expect("this must never be silent");
        assert!(said.contains("my-server"), "{said}");
        assert!(
            said.contains("ssh-add"),
            "the sentence has to carry the way out: {said}"
        );
        assert!(
            said.contains("no terminal"),
            "and why the service could not just ask: {said}"
        );

        // A rebuilt server is a different problem with a different fix.
        let host = ssh_trouble(
            "my-server",
            "@@@@@\nWARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!\n",
        )
        .expect("a changed host key must be named");
        assert!(host.contains("ssh-keygen -R my-server"), "{host}");

        // An ordinary unreachable server says nothing special, and must
        // not be dressed up as an authentication problem.
        assert!(
            ssh_trouble(
                "my-server",
                "ssh: connect to host my-server port 22: Operation timed out\n"
            )
            .is_none()
        );
        assert!(ssh_trouble("my-server", "").is_none());
    }

    #[test]
    fn event_triage_is_a_prefix_match_on_both_sides() {
        let w = Watch {
            dirs: vec!["/p/app".into(), "/p/shared".into()],
            excluded: vec!["/p/app/node_modules".into(), "/p/app/.git".into()],
        };
        assert!(w.covers(Path::new("/p/app/src/main.py")));
        assert!(w.covers(Path::new("/p/shared/x.env")));
        assert!(!w.covers(Path::new("/p/app/node_modules/x/y.js")));
        assert!(!w.covers(Path::new("/p/app/.git/objects/ab")));
        // A sibling repo in the same monorepo must never wake a sync.
        assert!(!w.covers(Path::new("/p/other-repo/file")));
    }
}
