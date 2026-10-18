//! `ulak ui`: the dashboard's terminal, its loop, and the one place an
//! `app::Effect` becomes a process.
//!
//! `app.rs` decides and `view.rs` draws; this module owns what neither may
//! touch — the terminal, the workers in `feed.rs`, and every child they
//! start. Four orderings here are load-bearing:
//!
//! - **Connect before raw mode.** The first connection to a server may
//!   need a passphrase or a host-key answer. It is made while the
//!   terminal is still a terminal; every later reader runs `Ssh::batch`
//!   and rides the warm master, so nothing can prompt over the frame.
//!   Switching to a server with no live master steps out of the screen
//!   for the same reason.
//! - **`ui::hush` outlives the screen.** Locals drop in reverse order, so
//!   the hush guard is declared first: the terminal is restored, and only
//!   then are the warnings held meanwhile printed — moved, never lost.
//! - **Nothing the user does reaches the shell.** Whatever is still in
//!   the terminal's input queue when the screen is given back is read by
//!   whoever reads next: the shell after quitting, or the child the
//!   dashboard steps out for. So the mouse reports only what the app
//!   reads (`ClicksOnly`), the loop reads a burst of input in one frame
//!   instead of one event per frame, waiting for tasks to stop happens
//!   inside the loop — drawn, and still reading — rather than after it,
//!   and `settle` stops the mouse and swallows what is in flight before
//!   the terminal changes hands. Measured before, on a macOS pty: the
//!   four seconds `q` waited left every motion report, click and key made
//!   meanwhile for the shell, `^[[<35;54;11M` and all.
//! - **Leaving stops children the way Ctrl-C would.** Running tasks get
//!   SIGINT and a grace to finish (`app::LEAVE_GRACE`, counted by the app)
//!   before anything is killed. A Ulak child stopped mid-sync does not
//!   unwind, it dies of the signal, and that is safe by construction:
//!   its workspace lock is an OS file lock and its state lands through
//!   a temporary file and a rename.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::process::{ExitCode, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, DisableMouseCapture, Event, KeyEventKind, MouseEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::app::{App, Effect, Here, Hits};
use crate::engine::Container;
use crate::feed::{self, Feed, LocalReader, Msg, Poller, Sink, Stream, Task};
use crate::management::Context;
use crate::proc;
use crate::ssh::Ssh;
use crate::ui::{self, fail};
use crate::verbs::{self, Driver};

/// The longest the loop sleeps waiting for input: the spinner's pace.
const FRAME: Duration = Duration::from_millis(100);

/// With nothing moving and nothing new, a frame this often still keeps
/// the clock-based words ("synced 4s ago") honest.
const IDLE_FRAME: Duration = Duration::from_secs(1);

/// Messages handled per frame at most, so a log flood cannot starve
/// the keyboard.
const DRAIN: usize = 4_000;

/// Input events handled per frame at most. A held arrow key, a wheel
/// fling or a paste is a burst, and a burst read one event per frame
/// lagged behind the hand and was still queued when the screen was
/// given back.
const BATCH: usize = 256;

/// Input quiet for this long when the terminal changes hands means the
/// terminal has nothing more on its way to us.
const SETTLED: Duration = Duration::from_millis(50);

/// The most `settle` waits, however busy the input stays.
const SETTLE_AT_MOST: Duration = Duration::from_millis(500);

/// A shell that ends faster than this almost certainly failed to start;
/// its last words are kept on screen until a key is pressed.
const QUICK_EXIT: Duration = Duration::from_secs(2);

pub fn run(globals: &[String]) -> Result<ExitCode> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(
            fail!("ulak ui draws a full-screen dashboard and needs a terminal")
                .now("run it in an interactive terminal")
                .now("for a report a script can read: ulak status")
                .into_err(),
        );
    }
    let start = locate(globals)?;
    warm(&start.ssh);

    let _hush = ui::hush();
    let mut screen = Screen::enter()?;
    let (tx, rx) = std::sync::mpsc::channel::<Msg>();
    let now = Instant::now();
    let mut app = App::new(start.dest.clone(), start.here, start.known, now);
    let mut world = World::start(&app, start.ssh, tx);
    let result = event_loop(&mut screen, &mut app, &mut world, &rx);
    if result.is_ok() {
        // The loop gave running tasks their grace itself, on screen.
        world.leave(&rx, &[], Duration::ZERO);
        drop(screen);
    } else {
        // A loop that failed did not; the terminal goes back first, so
        // nobody types into a frozen frame while they stop.
        let running: Vec<u64> = app
            .tasks
            .iter()
            .filter(|t| t.running())
            .map(|t| t.id)
            .collect();
        drop(screen);
        world.leave(&rx, &running, crate::app::LEAVE_GRACE);
    }
    result.map(|()| ExitCode::SUCCESS)
}

// ─── where to look ──────────────────────────────────────────────────

struct Start {
    dest: String,
    here: Option<Here>,
    known: Vec<String>,
    /// Interactive: it may prompt, before the screen is taken.
    ssh: Ssh,
}

/// The same selection `ulak status` makes: a project when there is one,
/// else the workspace's server, else the servers this machine declared
/// stacks on. Several stacks declared here are each a group on their
/// server already, so none is singled out.
fn locate(globals: &[String]) -> Result<Start> {
    let mut known = declared_servers();
    match crate::management::locate(globals, "ui", &[], None)? {
        Context::Project(project) => {
            // Binding asks the server's compose for the project's real
            // name, which takes a round trip: say so before the wait.
            ui::info(&format!(
                "opening {} on {}…",
                project.compose_identity(),
                project.ssh_dest().unwrap_or_default()
            ));
            let b = crate::config::bind_located(*project)?;
            let here = Here {
                identity: b.project.compose_identity(),
                dest: b.dest.clone(),
                local_dir: tilde(&b.project.inv.project_dir),
                compose: b.project.inv.flags_line(),
                remote_dir: b.project.remote_dir_shown(),
                driver: Driver::of(&b.project),
            };
            Ok(Start {
                dest: b.dest,
                here: Some(here),
                known,
                ssh: b.ssh,
            })
        }
        Context::Several(several) => {
            let listed = several.listed();
            let dest = listed
                .iter()
                .find(|stack| stack.desired.live)
                .or(listed.first())
                .map(|stack| stack.desired.destination.clone())
                .unwrap_or_default();
            ui::info(&format!("connecting to {dest}…"));
            Ok(Start {
                ssh: Ssh::new(&dest)?,
                dest,
                here: None,
                known,
            })
        }
        Context::Workspace(workspace) => {
            let dest = workspace.ssh_dest()?;
            ui::info(&format!("connecting to {dest}…"));
            Ok(Start {
                ssh: Ssh::new(&dest)?,
                dest,
                here: None,
                known,
            })
        }
        Context::Outside => {
            let configured = crate::config::machine_config()
                .host
                .and_then(|h| crate::config::validate_ssh_dest(&h).ok());
            if let Some(host) = &configured
                && !known.contains(host)
            {
                known.insert(0, host.clone());
            }
            let Some(dest) = configured.or_else(|| known.first().cloned()) else {
                return Err(fail!(
                    "no server to show: no project here, and this machine has declared no stack"
                )
                .now("cd into a Compose project with an ulak.toml and run ulak ui there")
                .now("or configure one: ulak init <ssh-host>")
                .into_err());
            };
            ui::info(&format!("connecting to {dest}…"));
            Ok(Start {
                ssh: Ssh::new(&dest)?,
                dest,
                here: None,
                known,
            })
        }
    }
}

/// A local path as a person reads it: the home directory as `~`.
fn tilde(path: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

/// Destinations this machine declared stacks on, live ones first.
fn declared_servers() -> Vec<String> {
    let catalog = crate::intent::catalog();
    let mut live: Vec<String> = Vec::new();
    let mut idle: Vec<String> = Vec::new();
    for (_, d) in catalog {
        let list = if d.live { &mut live } else { &mut idle };
        if !list.contains(&d.destination) {
            list.push(d.destination);
        }
    }
    live.sort();
    idle.sort();
    idle.retain(|d| !live.contains(d));
    live.extend(idle);
    live
}

/// Open the shared connection while a prompt can still be answered. A
/// failure is not fatal: the dashboard shows the link as down and keeps
/// retrying, which is more useful than refusing to open.
fn warm(ssh: &Ssh) {
    match ssh.probe("true") {
        Ok(b) if b.status.success() => {}
        Ok(b) if b.timed_out => ui::warn(&format!(
            "{} did not answer; the dashboard will keep trying",
            ssh.dest
        )),
        Ok(b) => {
            let why = String::from_utf8_lossy(&b.stderr);
            let why = why
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("ssh failed");
            ui::warn(&format!("cannot reach {}: {why}", ssh.dest));
        }
        Err(e) => ui::warn(&ui::flatten(&e)),
    }
}

/// Whether a shared master is already up for this server: asked of the
/// local socket only, so it costs nothing and never prompts.
fn master_alive(ssh: &Ssh) -> bool {
    let mut check = std::process::Command::new("ssh");
    check
        .args(["-o", &format!("ControlPath={}", ssh.control_path())])
        .args(["-O", "check", "--"])
        .arg(&ssh.dest);
    proc::run_bounded(&mut check, None, proc::PROBE).is_ok_and(|b| b.status.success())
}

// ─── the terminal ───────────────────────────────────────────────────

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Raw mode, the alternate screen and mouse reports, for as long as it
/// lives — including through a panic, whose hook puts the terminal back
/// before the message is printed.
struct Screen {
    term: Term,
}

impl Screen {
    fn enter() -> Result<Screen> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        take()?;
        Ok(Screen { term: fresh()? })
    }

    /// Give the terminal back for a foreground child, then take it again.
    fn suspended<T>(&mut self, f: impl FnOnce() -> T) -> Result<T> {
        settle();
        restore();
        let out = f();
        take()?;
        // A new terminal's buffers are blank, like the screen `take` left:
        // the next frame is drawn whole.
        self.term = fresh()?;
        Ok(out)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        // A panic's hook has restored the terminal already, and a panic
        // is no time to wait on input.
        if !std::thread::panicking() {
            settle();
        }
        restore();
    }
}

/// The mouse reporting the dashboard reads: presses and releases, the
/// wheel among them (`?1000h`), in SGR form so a click past column 223
/// still has its column (`?1006h`).
///
/// crossterm's `EnableMouseCapture` asks for every motion as well
/// (`?1003h`): each move over the window became a report the app threw
/// away, but read and drawn for one frame each, and still queued when
/// the screen was given back. `DisableMouseCapture` turns off all of it,
/// so it stays the way out.
struct ClicksOnly;

impl ratatui::crossterm::Command for ClicksOnly {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(CLICKS_ONLY)
    }
}

const CLICKS_ONLY: &str = "\x1b[?1000h\x1b[?1006h";

/// Not every terminal blanks its alternate screen, and ratatui draws
/// differences against a blank one — so the screen is cleared here, by
/// hand: `Terminal::clear` asks the terminal where its cursor is, and a
/// terminal that never answers turns that into a startup failure.
fn take() -> Result<()> {
    enable_raw_mode().context("cannot put the terminal in raw mode")?;
    execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        ClicksOnly,
        Clear(ClearType::All)
    )
    .context("cannot switch to the alternate screen")?;
    Ok(())
}

fn fresh() -> Result<Term> {
    Terminal::new(CrosstermBackend::new(std::io::stdout())).context("cannot draw on this terminal")
}

/// Stop the mouse, then read and drop whatever is already on its way —
/// reports the terminal sent before it saw the stop, keys typed into a
/// screen that is closing — until the input has been quiet for
/// `SETTLED`. Left in the queue, they are read by whoever reads the
/// terminal next. Raw mode stays on throughout: dropped to cooked mode
/// with input queued, the tty echoes it, which is the `^[[<0;12;40m`
/// left on the prompt.
fn settle() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    let started = Instant::now();
    while started.elapsed() < SETTLE_AT_MOST {
        match event::poll(SETTLED) {
            Ok(true) => {
                let _ = event::read();
            }
            _ => break,
        }
    }
}

fn restore() {
    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        ratatui::crossterm::cursor::Show
    );
    let _ = disable_raw_mode();
}

// ─── the loop ───────────────────────────────────────────────────────

fn event_loop(
    screen: &mut Screen,
    app: &mut App,
    world: &mut World,
    rx: &Receiver<Msg>,
) -> Result<()> {
    let mut last_tick = Instant::now() - FRAME;
    let mut last_draw = Instant::now() - IDLE_FRAME;
    let mut dirty = true;
    let mut hits = Hits::default();
    let mut held: Option<Event> = None;
    loop {
        // Draw only when something changed, a spinner turns, or the idle
        // frame is due: a still dashboard should cost nothing.
        if dirty || last_draw.elapsed() >= IDLE_FRAME {
            app.clock = unix_now();
            screen
                .term
                .draw(|f| hits = crate::view::draw(f, app))
                .context("cannot draw the dashboard")?;
            last_draw = Instant::now();
            dirty = false;
        }

        let mut fx = Vec::new();
        let wait = FRAME.saturating_sub(last_tick.elapsed());
        for read in 0..BATCH {
            let event = match held.take() {
                Some(event) => event,
                None => {
                    // Only the first wait is a wait: the rest of a burst
                    // is already here.
                    let wait = if read == 0 { wait } else { Duration::ZERO };
                    if !event::poll(wait).context("cannot read the terminal")? {
                        break;
                    }
                    event::read().context("cannot read the terminal")?
                }
            };
            let turn = turn(&event, read == 0);
            if turn == Turn::Hold {
                held = Some(event);
                break;
            }
            let now = Instant::now();
            dirty = true;
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    fx.extend(app.on_key(key, now))
                }
                Event::Mouse(mouse) => fx.extend(app.on_mouse(mouse, &hits, now)),
                _ => {}
            }
            // Each effect runs before the next event is handled: an
            // effect may take the terminal (`e`, a server switch), and
            // the keys behind it must not be read as if it had not.
            if turn == Turn::Last || !fx.is_empty() || app.quit {
                break;
            }
        }
        let now = Instant::now();
        for msg in rx.try_iter().take(DRAIN) {
            dirty = true;
            fx.extend(app.on_msg(msg, now));
        }
        if last_tick.elapsed() >= FRAME {
            last_tick = now;
            let toasts = app.toasts.len();
            fx.extend(app.on_tick(now, ui::take_hushed()));
            dirty |= app.animating() || app.toasts.len() != toasts;
        }
        dirty |= !fx.is_empty();
        for effect in fx {
            world.apply(effect, app, screen)?;
        }
        if app.quit {
            return Ok(());
        }
    }
}

/// What one input event does to the rest of a frame's read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    /// Handle it and read on: keys, the wheel, a release.
    Read,
    /// Handle it, then draw before reading more: a click, a resize.
    Last,
    /// Not in this frame. A click is aimed at what is on screen, which
    /// the events read before it in the same frame may have changed —
    /// a tab switched, a dialog opened — so it waits for the frame they
    /// produce, and is aimed at that.
    Hold,
}

fn turn(event: &Event, first: bool) -> Turn {
    match event {
        Event::Mouse(m) if matches!(m.kind, MouseEventKind::Down(_)) => {
            if first {
                Turn::Last
            } else {
                Turn::Hold
            }
        }
        Event::Resize(..) => Turn::Last,
        _ => Turn::Read,
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── effects ────────────────────────────────────────────────────────

/// Every worker and child the dashboard owns. Dropping it ends them all.
struct World {
    /// Interactive: for the foreground shell, which has the terminal.
    ssh: Ssh,
    /// The same server for background readers, which must never prompt.
    quiet: Ssh,
    out: Sender<Msg>,
    poller: Poller,
    events: Option<Stream>,
    stats: Option<Stream>,
    logs: BTreeMap<u64, Stream>,
    tasks: BTreeMap<u64, Task>,
    _local: LocalReader,
}

impl World {
    fn start(app: &App, ssh: Ssh, out: Sender<Msg>) -> World {
        let quiet = ssh.clone().batch();
        let mut world = World {
            poller: Poller::start(app.era, quiet.clone(), out.clone()),
            _local: feed::local(out.clone()),
            ssh,
            quiet,
            out,
            events: None,
            stats: None,
            logs: BTreeMap::new(),
            tasks: BTreeMap::new(),
        };
        world.feed(Feed::Events, app.era);
        world.feed(Feed::Stats, app.era);
        world
    }

    /// (Re)start one daemon feed. A failure to start is reported the way
    /// an ended feed is, so the app's backoff covers both.
    fn feed(&mut self, which: Feed, era: u64) {
        let started = match which {
            Feed::Events => verbs::events_argv().and_then(|argv| {
                feed::stream(&self.quiet, &argv, Sink::Events { era }, self.out.clone())
            }),
            Feed::Stats => verbs::stats_argv().and_then(|argv| {
                feed::stream(&self.quiet, &argv, Sink::Stats { era }, self.out.clone())
            }),
        };
        let slot = match which {
            Feed::Events => &mut self.events,
            Feed::Stats => &mut self.stats,
        };
        match started {
            Ok(stream) => *slot = Some(stream),
            Err(e) => {
                *slot = None;
                let _ = self.out.send(Msg::FeedEnded {
                    era,
                    feed: which,
                    why: ui::flatten(&e),
                });
            }
        }
    }

    fn apply(&mut self, effect: Effect, app: &mut App, screen: &mut Screen) -> Result<()> {
        match effect {
            Effect::Run { task, action } => {
                let started = verbs::command(&action, &self.quiet)
                    .and_then(|(cmd, audit)| feed::task(task, cmd, audit, self.out.clone()));
                match started {
                    Ok(t) => {
                        self.tasks.insert(task, t);
                    }
                    Err(e) => {
                        let _ = self.out.send(Msg::TaskLine {
                            task,
                            line: ui::flatten(&e),
                        });
                        let _ = self.out.send(Msg::TaskDone { task, code: None });
                    }
                }
            }
            Effect::Cancel { task } => {
                if let Some(t) = self.tasks.get(&task) {
                    t.cancel();
                }
            }
            Effect::Shell(c) => self.shell(&c, screen)?,
            Effect::OpenLogs { stream, ids } => {
                // One session however many containers: see `feed.rs`.
                let started = ids
                    .iter()
                    .map(|id| verbs::logs_argv(id))
                    .collect::<Result<Vec<_>>>()
                    .and_then(|argvs| match argvs.as_slice() {
                        [argv] => feed::stream(
                            &self.quiet,
                            argv,
                            Sink::Logs { stream, source: 0 },
                            self.out.clone(),
                        ),
                        _ => feed::stack_logs(&self.quiet, &argvs, stream, self.out.clone()),
                    });
                match started {
                    Ok(s) => {
                        self.logs.insert(stream, s);
                    }
                    Err(e) => {
                        for source in 0..ids.len() {
                            let _ = self.out.send(Msg::StreamEnded {
                                stream,
                                source,
                                why: ui::flatten(&e),
                            });
                        }
                    }
                }
            }
            Effect::CloseLogs { stream } => {
                self.logs.remove(&stream);
            }
            Effect::Fetch { req, argv } => {
                feed::fetch(req, self.quiet.clone(), argv, self.out.clone())
            }
            Effect::Poke(want) => self.poller.poke(want),
            Effect::Interest(want) => self.poller.interest(want),
            Effect::Open(url) => {
                let mut cmd = verbs::opener(&url);
                std::thread::spawn(move || {
                    let _ = proc::run_bounded(&mut cmd, None, proc::PROBE);
                });
            }
            Effect::Copy(text) => copy(&text),
            Effect::Switch(dest) => self.switch(dest, app, screen)?,
            Effect::RestartFeed(which) => self.feed(which, app.era),
        }
        Ok(())
    }

    /// A shell inside a container, in the foreground: the dashboard steps
    /// out of the way and comes back when it ends.
    fn shell(&mut self, c: &Container, screen: &mut Screen) -> Result<()> {
        let (mut cmd, redacted) = verbs::shell(&self.ssh, c)?;
        let name = c.name.clone();
        let code = screen.suspended(|| {
            let mut out = std::io::stdout();
            let _ = execute!(out, Clear(ClearType::All), ratatui::crossterm::cursor::MoveTo(0, 0));
            let _ = writeln!(out, "\x1b[2m── shell in {name} · exit or Ctrl-D returns to the dashboard ──\x1b[0m");
            let started = Instant::now();
            let code = cmd
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .ok()
                .and_then(|s| s.code());
            if code != Some(0) && started.elapsed() < QUICK_EXIT {
                let _ = writeln!(out, "\n\x1b[2m── the shell ended at once (exit {}) · press Enter to go back ──\x1b[0m",
                    code.map_or("?".to_string(), |c| c.to_string()));
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
            }
            code
        })?;
        crate::passthrough::record_ssh_audit(&cmd, &redacted, code);
        Ok(())
    }

    fn switch(&mut self, dest: String, app: &mut App, screen: &mut Screen) -> Result<()> {
        let ssh = match Ssh::new(&dest) {
            Ok(ssh) => ssh,
            Err(e) => {
                app.toast(crate::app::Tone::Bad, ui::flatten(&e), Instant::now());
                return Ok(());
            }
        };
        if !master_alive(&ssh) {
            // The first connection may ask something; let it be asked.
            screen.suspended(|| {
                ui::info(&format!("connecting to {dest}…"));
                warm(&ssh);
            })?;
        }
        // Old streams go first, so nothing from the last server lands late.
        self.events = None;
        self.stats = None;
        self.logs.clear();
        self.quiet = ssh.clone().batch();
        self.ssh = ssh;
        self.poller = Poller::start(app.era, self.quiet.clone(), self.out.clone());
        self.feed(Feed::Events, app.era);
        self.feed(Feed::Stats, app.era);
        Ok(())
    }

    /// End every stream and task. `running` — the tasks the app still
    /// sees running — are first stopped the way Ctrl-C would and given
    /// up to `grace` to finish.
    ///
    /// Only those: `tasks` keeps every task ever started, and one that
    /// ended long ago will neither answer a SIGINT (its pid may be
    /// somebody else's by now) nor report done a second time. Waiting
    /// for all of them was a full grace on every quit after any task.
    fn leave(&mut self, rx: &Receiver<Msg>, running: &[u64], grace: Duration) {
        self.events = None;
        self.stats = None;
        self.logs.clear();
        let mut running: Vec<u64> = running
            .iter()
            .copied()
            .filter(|id| self.tasks.contains_key(id))
            .collect();
        if !grace.is_zero() {
            for id in &running {
                if let Some(t) = self.tasks.get(id) {
                    t.cancel();
                }
            }
            let deadline = Instant::now() + grace;
            while !running.is_empty() && Instant::now() < deadline {
                if let Ok(Msg::TaskDone { task, .. }) = rx.recv_timeout(Duration::from_millis(50)) {
                    running.retain(|t| *t != task);
                }
            }
        }
        self.tasks.clear();
    }
}

/// Put text on the clipboard: `pbcopy` on macOS, where Terminal.app
/// ignores OSC 52; elsewhere OSC 52, which also crosses an ssh session.
fn copy(text: &str) {
    if cfg!(target_os = "macos") {
        let text = text.to_string();
        std::thread::spawn(move || {
            let Ok(mut child) = std::process::Command::new("pbcopy")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                return;
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        });
    } else {
        let mut out = std::io::stdout();
        let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
        let _ = out.flush();
    }
}

/// Standard base64 with padding, for OSC 52.
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ABC[(n >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{
        EnableMouseCapture, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent,
    };

    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn ansi(command: impl ratatui::crossterm::Command) -> String {
        let mut out = String::new();
        command.write_ansi(&mut out).unwrap();
        out
    }

    #[test]
    fn a_click_is_aimed_at_a_fresh_frame_and_the_rest_of_a_burst_is_read_at_once() {
        let click = mouse(MouseEventKind::Down(MouseButton::Left));
        assert_eq!(turn(&click, true), Turn::Last);
        assert_eq!(
            turn(&click, false),
            Turn::Hold,
            "read after other input, it waits for the frame that input made"
        );
        assert_eq!(turn(&Event::Resize(80, 24), false), Turn::Last);
        for event in [
            Event::Key(KeyEvent::from(KeyCode::Down)),
            Event::Key(KeyEvent::from(KeyCode::Char('j'))),
            mouse(MouseEventKind::ScrollDown),
            mouse(MouseEventKind::ScrollUp),
            mouse(MouseEventKind::Up(MouseButton::Left)),
        ] {
            assert_eq!(turn(&event, true), Turn::Read, "{event:?}");
            assert_eq!(turn(&event, false), Turn::Read, "{event:?}");
        }
    }

    #[test]
    fn the_mouse_reports_clicks_and_the_wheel_and_never_motion() {
        assert_eq!(ansi(ClicksOnly), "\x1b[?1000h\x1b[?1006h");
        // The reason for ClicksOnly: crossterm 0.29's own capture asks for
        // every motion. An upgrade that changes it fails here, and the
        // rationale above wants reading again.
        let theirs = ansi(EnableMouseCapture);
        assert!(theirs.contains("\x1b[?1003h"), "{theirs:?}");
        // Its teardown stays the way out, so it must cover ours.
        let off = ansi(DisableMouseCapture);
        for mode in ["\x1b[?1000l", "\x1b[?1006l"] {
            assert!(off.contains(mode), "{off:?} leaves {mode:?} on");
        }
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("ü".as_bytes()), "w7w=");
    }
}
