//! The dashboard's state, and what every input does to it.
//!
//! Pure on purpose: no terminal, no network, no clock except the `now`
//! handed in. `tui.rs` feeds this keys, clicks and `feed::Msg`s and
//! carries out the `Effect`s that come back; `view.rs` draws it. Every
//! decision the dashboard makes lives here, which is what lets the tests
//! drive a whole session — select, stop, confirm, read logs — with no
//! server and no terminal.
//!
//! Three rules shape it:
//!
//! - **The cursor follows the thing, not the row number.** The list is
//!   re-read every few seconds and containers come and go; selection is
//!   kept by key (`Sel::key`) and re-found after every change, so a
//!   restart does not move the cursor onto a neighbour a keypress would
//!   then act on.
//! - **Destructive verbs ask first, and No is the default** — the same
//!   contract as `ui::confirm`. Enter on a fresh dialog cancels, and so
//!   does a click outside it. A click on a footer hint or a button is
//!   turned into the key it names (`key_named`) and goes through
//!   `on_key`, so the mouse can never reach a path the keys do not.
//! - **The project you opened the dashboard in is always on screen**,
//!   even with nothing running, because "start it" is the first thing
//!   anyone wants to do there. Stacks this machine declared on the same
//!   server are listed with it, so `up` is one key away for those too.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::engine::{
    self, Container, Disk, Image, Info, Network, Notice, Snapshot, Stat, Usage, Volume, Want,
};
use crate::feed::{Feed, Local, Msg};
use crate::verbs::{self, Action, Driver};

/// Log lines kept per view; older ones are dropped and counted.
pub const LOG_CAP: usize = 20_000;
/// Stats samples kept per container — two minutes at one a second.
pub const HISTORY: usize = 120;
/// Output lines kept per task.
const TASK_CAP: usize = 2_000;
/// Finished tasks kept in the list.
const TASKS_KEPT: usize = 30;
const TOAST_FOR: Duration = Duration::from_secs(5);
const BAD_TOAST_FOR: Duration = Duration::from_secs(10);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// When `docker events` is down, the list is re-read this often instead.
const BLIND_POLL: Duration = Duration::from_secs(5);
/// How long running tasks get to stop once asked (`feed::Task::cancel`)
/// when quitting, before what is left is killed. Waited out here, inside
/// the loop, so the screen keeps drawing and reading while they stop:
/// see `App::leave`.
pub const LEAVE_GRACE: Duration = Duration::from_secs(4);
const FEED_BACKOFF: &[u64] = &[2, 5, 15, 30];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tab {
    Containers,
    Images,
    Volumes,
    Networks,
    Project,
}

pub const TABS: [Tab; 5] = [
    Tab::Containers,
    Tab::Images,
    Tab::Volumes,
    Tab::Networks,
    Tab::Project,
];

impl Tab {
    pub fn title(self) -> &'static str {
        match self {
            Tab::Containers => "Containers",
            Tab::Images => "Images",
            Tab::Volumes => "Volumes",
            Tab::Networks => "Networks",
            Tab::Project => "Ulak",
        }
    }

    fn index(self) -> usize {
        TABS.iter().position(|t| *t == self).unwrap_or(0)
    }

    /// What this tab needs every round, beyond the containers.
    fn interest(self) -> Want {
        match self {
            Tab::Containers => Want::default(),
            Tab::Images => Want {
                images: true,
                usage: true,
                ..Want::default()
            },
            Tab::Volumes => Want {
                volumes: true,
                usage: true,
                ..Want::default()
            },
            Tab::Networks => Want {
                networks: true,
                ..Want::default()
            },
            Tab::Project => Want::default(),
        }
    }
}

/// The project the dashboard was opened in.
#[derive(Debug, Clone)]
pub struct Here {
    pub identity: String,
    pub dest: String,
    pub local_dir: String,
    pub compose: String,
    pub remote_dir: String,
    pub driver: Driver,
}

/// One Compose project (or the standalone containers) on this server.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The Compose project name; empty for standalone containers.
    pub key: String,
    pub members: Vec<usize>,
    /// The project this dashboard was opened in.
    pub current: bool,
    /// This machine declared it; `live` says whether it still wants it up.
    pub declared: Option<Declared>,
    pub driver: Option<Driver>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Declared {
    pub stack_id: String,
    pub live: bool,
}

impl Group {
    pub fn title(&self) -> &str {
        if self.key.is_empty() {
            "standalone"
        } else {
            &self.key
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Group(usize),
    /// A container, by index into `App::containers`.
    Container(usize),
    /// The hint under a group with nothing in it.
    Empty(usize),
    Image(usize),
    Volume(usize),
    Network(usize),
    /// The project the dashboard was opened in, on the Ulak tab — listed
    /// before it is declared anywhere, so "up" is one key away.
    Here,
    /// A declared stack, by index into `Local::catalog`.
    Stack(usize),
}

/// A list's cursor: the key of the selected row, re-found after changes.
#[derive(Debug, Clone, Default)]
pub struct Sel {
    pub index: usize,
    pub key: Option<String>,
    /// First visible row, kept by the view between frames.
    pub offset: std::cell::Cell<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    Connecting,
    Up { took: Duration },
    Down { why: String, since: Instant },
}

#[derive(Debug, Clone)]
pub struct History {
    pub cpu: VecDeque<f64>,
    pub mem: VecDeque<f64>,
    /// Memory in use, in bytes, beside `mem`'s share of the limit.
    pub used: VecDeque<f64>,
    pub last: Stat,
    pub at: Instant,
}

#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub id: String,
    pub ended: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub source: usize,
    pub stamp: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct LogView {
    pub stream: u64,
    pub sources: Vec<Source>,
    pub lines: VecDeque<LogLine>,
    pub dropped: u64,
    pub follow: bool,
    /// Logical lines between the newest line and the bottom of the view.
    pub scroll: usize,
    pub wrap: bool,
    pub timestamps: bool,
    pub search: Option<String>,
    /// The line the last search jump landed on, drawn marked.
    pub hit: Option<usize>,
    /// Rows the view last drew, so a jump can show a match mid-screen.
    pub page: std::cell::Cell<usize>,
}

impl LogView {
    fn new(stream: u64, sources: Vec<Source>) -> LogView {
        LogView {
            stream,
            sources,
            lines: VecDeque::new(),
            dropped: 0,
            follow: true,
            scroll: 0,
            wrap: false,
            timestamps: false,
            search: None,
            hit: None,
            page: std::cell::Cell::new(0),
        }
    }

    /// Lines arrive per container and per pipe: stdout and stderr are read
    /// apart, so even one container's backlog lands as two bursts. Each
    /// line is merged in by Docker's fixed-width stamp, scanning back only
    /// as far as it must.
    fn push(&mut self, source: usize, raw: &str) {
        let (stamp, text) = engine::split_timestamp(raw);
        let line = LogLine {
            source,
            stamp: stamp.map(str::to_string),
            text: text.to_string(),
        };
        let at = match &line.stamp {
            Some(stamp) => {
                let mut at = self.lines.len();
                let floor = at.saturating_sub(2_000);
                while at > floor {
                    match &self.lines[at - 1].stamp {
                        Some(prev) if prev > stamp => at -= 1,
                        _ => break,
                    }
                }
                at
            }
            _ => self.lines.len(),
        };
        self.lines.insert(at, line);
        if let Some(hit) = self.hit.as_mut()
            && at <= *hit
        {
            *hit += 1;
        }
        if !self.follow && at >= self.lines.len() - 1 - self.scroll.min(self.lines.len() - 1) {
            // Keep a reader's place: a line landing below what they see
            // must not scroll the text out from under them.
            self.scroll += 1;
        }
        if self.lines.len() > LOG_CAP {
            self.lines.pop_front();
            self.dropped += 1;
            self.hit = self.hit.and_then(|h| h.checked_sub(1));
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.lines.len().saturating_sub(1);
        let next = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
        self.scroll = next;
        self.follow = next == 0;
    }

    /// Jump to the next match older or newer than the last one — or,
    /// before any, than the bottom line — and show it mid-screen.
    fn jump(&mut self, older: bool) -> bool {
        let Some(term) = self.search.as_ref().map(|s| s.to_lowercase()) else {
            return false;
        };
        let len = self.lines.len();
        if len == 0 {
            return false;
        }
        let bottom = len - 1 - self.scroll.min(len - 1);
        let matches = |i: &usize| contains(&self.lines[*i].text, &term);
        let found = if older {
            (0..self.hit.unwrap_or(bottom + 1)).rev().find(matches)
        } else {
            (self.hit.unwrap_or(bottom) + 1..len).find(matches)
        };
        match found {
            Some(i) => {
                self.hit = Some(i);
                self.scroll = (len - 1 - i).saturating_sub(self.page.get() / 2);
                self.follow = self.scroll == 0;
                true
            }
            None => false,
        }
    }
}

impl LogView {
    /// Which match the marked line is, oldest first, and how many lines
    /// match the search; `None` without a search.
    pub fn tally(&self) -> Option<(Option<usize>, usize)> {
        let term = self.search.as_ref()?.to_lowercase();
        let mut nth = None;
        let mut total = 0;
        for (i, line) in self.lines.iter().enumerate() {
            if contains(&line.text, &term) {
                total += 1;
                if self.hit == Some(i) {
                    nth = Some(total);
                }
            }
        }
        Some((nth, total))
    }
}

/// A scrollable text: an inspect document.
#[derive(Debug, Clone, Default)]
pub struct TextView {
    pub req: u64,
    pub lines: Vec<String>,
    pub scroll: usize,
    pub loading: bool,
    pub error: Option<String>,
    pub search: Option<String>,
}

impl TextView {
    fn jump(&mut self, forward: bool) -> bool {
        let Some(term) = self.search.as_ref().map(|s| s.to_lowercase()) else {
            return false;
        };
        let hit = |i: &usize| self.lines[*i].to_lowercase().contains(&term);
        let found = if forward {
            (self.scroll + 1..self.lines.len()).find(hit)
        } else {
            (0..self.scroll).rev().find(hit)
        };
        match found {
            Some(i) => {
                self.scroll = i;
                true
            }
            None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Logs,
    Inspect,
    Stats,
}

/// What a detail screen is about.
#[derive(Debug, Clone, PartialEq)]
pub enum Subject {
    Container(String),
    Stack(String),
    Image(String),
    Volume(String),
    Network(String),
}

impl Subject {
    fn kind(&self) -> &'static str {
        match self {
            Subject::Container(_) | Subject::Stack(_) => "container",
            Subject::Image(_) => "image",
            Subject::Volume(_) => "volume",
            Subject::Network(_) => "network",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Detail {
    pub subject: Subject,
    pub title: String,
    pub pane: Pane,
    pub logs: Option<LogView>,
    pub inspect: TextView,
}

impl Detail {
    pub fn panes(&self) -> &'static [Pane] {
        match self.subject {
            Subject::Container(_) => &[Pane::Logs, Pane::Inspect, Pane::Stats],
            Subject::Stack(_) => &[Pane::Logs],
            _ => &[Pane::Inspect],
        }
    }
}

#[derive(Debug, Clone)]
pub enum Screen {
    Main,
    Detail(Box<Detail>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Then {
    Run(Action),
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub question: String,
    pub then: Then,
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    Filter,
    Search,
    Command,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    pub prompt: Prompt,
    pub text: String,
    /// What the filter was before typing began, for Esc.
    pub before: String,
}

/// `Leaving` is quitting while the tasks that were running stop: `App::leave`.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    None,
    Help { scroll: usize },
    Confirm(Confirm),
    Input(Input),
    Tasks { selected: usize, scroll: usize },
    Servers { selected: usize },
    Leaving { since: Instant },
}

#[derive(Debug, Clone)]
pub struct TaskView {
    pub id: u64,
    pub title: String,
    pub lines: VecDeque<String>,
    pub started: Instant,
    pub done: Option<(Option<i32>, Instant)>,
    pub refresh: Want,
}

impl TaskView {
    pub fn running(&self) -> bool {
        self.done.is_none()
    }

    pub fn failed(&self) -> bool {
        matches!(self.done, Some((code, _)) if code != Some(0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Bad,
    Note,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub tone: Tone,
    pub at: Instant,
}

/// What `tui.rs` must do on the dashboard's behalf.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Run { task: u64, action: Action },
    Cancel { task: u64 },
    Shell(Box<Container>),
    OpenLogs { stream: u64, ids: Vec<String> },
    CloseLogs { stream: u64 },
    Fetch { req: u64, argv: Vec<String> },
    Poke(Want),
    Interest(Want),
    Open(String),
    Copy(String),
    Switch(String),
    RestartFeed(Feed),
}

/// Where the view drew the things a click can hit.
#[derive(Debug, Clone, Default)]
pub struct Hits {
    pub tabs: Vec<(Rect, Tab)>,
    pub panes: Vec<(Rect, Pane)>,
    /// The list body: its area, and the row index drawn at its top.
    pub list: Option<(Rect, usize)>,
    /// Footer hints, dialog buttons and the back arrow, by the key each
    /// names: clicking one is pressing that key.
    pub keys: Vec<(Rect, String)>,
    /// A dialog's list (tasks, servers): its area and the row at its top.
    pub picks: Option<(Rect, usize)>,
    /// A dialog's frame; a click outside it is esc, which never confirms.
    pub dialog: Option<Rect>,
}

/// The key a footer hint or button names, as `on_key` reads it. Hints
/// for a family of keys ("↑↓", "PgUp/PgDn") are not buttons.
pub fn key_named(label: &str) -> Option<KeyEvent> {
    let code = match label {
        "⏎" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "n/N" => KeyCode::Char('n'),
        _ => {
            let mut chars = label.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_graphic() => KeyCode::Char(c),
                _ => return None,
            }
        }
    };
    Some(KeyEvent::from(code))
}

pub struct App {
    pub dest: String,
    pub era: u64,
    pub here: Option<Here>,
    /// Servers to offer besides the ones the catalog names.
    pub known: Vec<String>,

    pub containers: Vec<Container>,
    pub images: Vec<Image>,
    pub volumes: Vec<Volume>,
    pub networks: Vec<Network>,
    pub info: Option<Info>,
    pub disk: Option<Disk>,
    pub usage: Vec<Usage>,
    pub trouble: Vec<String>,
    pub engine: bool,
    /// Which sections have arrived at least once on this server.
    pub loaded: Want,
    pub containers_loaded: bool,
    pub stats: BTreeMap<String, History>,
    pub local: Local,
    pub drivers: BTreeMap<String, Driver>,
    pub groups: Vec<Group>,

    pub link: Link,
    pub events_up: bool,
    pub stats_up: bool,
    feed_retry: Vec<(Feed, Instant, usize)>,
    last_poke: Instant,

    pub tab: Tab,
    pub sel: [Sel; 5],
    pub filter: String,
    pub collapsed: BTreeSet<String>,
    pub show_stopped: bool,
    pub screen: Screen,
    pub overlay: Overlay,
    pub tasks: Vec<TaskView>,
    pub toasts: Vec<Toast>,
    pub frame: u64,
    /// Unix seconds, for "synced 4m ago"; the loop sets it each frame.
    pub clock: u64,
    pub quit: bool,
    next_id: u64,
    last_click: Option<(Instant, Position)>,
}

fn contains(hay: &str, needle_lower: &str) -> bool {
    hay.to_lowercase().contains(needle_lower)
}

impl App {
    pub fn new(dest: String, here: Option<Here>, known: Vec<String>, now: Instant) -> App {
        let mut app = App {
            dest,
            era: 1,
            here,
            known,
            containers: Vec::new(),
            images: Vec::new(),
            volumes: Vec::new(),
            networks: Vec::new(),
            info: None,
            disk: None,
            usage: Vec::new(),
            trouble: Vec::new(),
            engine: true,
            loaded: Want::default(),
            containers_loaded: false,
            stats: BTreeMap::new(),
            local: Local::default(),
            drivers: BTreeMap::new(),
            groups: Vec::new(),
            link: Link::Connecting,
            events_up: false,
            stats_up: false,
            feed_retry: Vec::new(),
            last_poke: now,
            tab: Tab::Containers,
            sel: Default::default(),
            filter: String::new(),
            collapsed: BTreeSet::new(),
            show_stopped: true,
            screen: Screen::Main,
            overlay: Overlay::None,
            tasks: Vec::new(),
            toasts: Vec::new(),
            frame: 0,
            clock: 0,
            quit: false,
            next_id: 1,
            last_click: None,
        };
        app.regroup();
        app
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn toast(&mut self, tone: Tone, text: impl Into<String>, now: Instant) {
        let text = text.into();
        // The same sentence twice in a row is one sentence.
        if let Some(last) = self.toasts.last_mut()
            && last.text == text
        {
            last.at = now;
            return;
        }
        self.toasts.push(Toast {
            text,
            tone,
            at: now,
        });
        let excess = self.toasts.len().saturating_sub(4);
        self.toasts.drain(..excess);
    }

    pub fn running_tasks(&self) -> usize {
        self.tasks.iter().filter(|t| t.running()).count()
    }

    /// Whether something on screen moves by itself — a spinner — so the
    /// loop knows a frame is due without any input.
    pub fn animating(&self) -> bool {
        let loading = match self.tab {
            Tab::Images => !self.loaded.images,
            Tab::Volumes => !self.loaded.volumes,
            Tab::Networks => !self.loaded.networks,
            Tab::Containers | Tab::Project => false,
        };
        matches!(self.link, Link::Connecting)
            || !self.containers_loaded
            || loading
            || self.running_tasks() > 0
            || matches!(&self.screen, Screen::Detail(d) if d.inspect.loading)
    }

    pub fn here_is_this_server(&self) -> bool {
        self.here.as_ref().is_some_and(|h| h.dest == self.dest)
    }

    /// Every server worth offering: where we are, where the dashboard
    /// was opened, and every destination this machine declared on.
    pub fn servers(&self) -> Vec<String> {
        let mut all: BTreeSet<String> = self.known.iter().cloned().collect();
        all.insert(self.dest.clone());
        if let Some(h) = &self.here {
            all.insert(h.dest.clone());
        }
        for (_, d) in &self.local.catalog {
            all.insert(d.destination.clone());
        }
        all.into_iter().collect()
    }

    // ─── derived lists ──────────────────────────────────────────────

    /// Rebuild the groups from the containers and what this machine
    /// declared on this server.
    fn regroup(&mut self) {
        let mut by: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, c) in self.containers.iter().enumerate() {
            by.entry(c.project.clone()).or_default().push(i);
        }
        let declared: BTreeMap<String, Declared> = self
            .local
            .catalog
            .iter()
            .filter(|(_, d)| d.destination == self.dest)
            .map(|(id, d)| {
                (
                    d.identity.clone(),
                    Declared {
                        stack_id: id.clone(),
                        live: d.live,
                    },
                )
            })
            .collect();
        let current = self
            .here
            .as_ref()
            .filter(|h| h.dest == self.dest)
            .map(|h| h.identity.clone());
        let mut keys: BTreeSet<String> = by.keys().filter(|k| !k.is_empty()).cloned().collect();
        keys.extend(declared.keys().cloned());
        if let Some(c) = &current {
            keys.insert(c.clone());
        }
        let mut order: Vec<String> = Vec::new();
        if let Some(c) = &current {
            order.push(c.clone());
        }
        order.extend(keys.into_iter().filter(|k| Some(k) != current.as_ref()));
        if by.contains_key("") {
            order.push(String::new());
        }
        let containers = &self.containers;
        self.groups = order
            .into_iter()
            .map(|key| {
                let mut members = by.remove(&key).unwrap_or_default();
                members.sort_by(|a, b| {
                    let (a, b) = (&containers[*a], &containers[*b]);
                    (a.oneoff(), &a.service, &a.name).cmp(&(b.oneoff(), &b.service, &b.name))
                });
                let is_current = current.as_deref() == Some(key.as_str()) && !key.is_empty();
                let declared = declared.get(&key).cloned();
                let driver = if is_current {
                    self.here.as_ref().map(|h| h.driver.clone())
                } else {
                    declared
                        .as_ref()
                        .and_then(|d| self.drivers.get(&d.stack_id).cloned())
                };
                Group {
                    key,
                    members,
                    current: is_current,
                    declared,
                    driver,
                }
            })
            .collect();
    }

    fn visible(&self, c: &Container, needle: &str) -> bool {
        if !self.show_stopped && !(c.running() || c.paused()) {
            return false;
        }
        needle.is_empty()
            || [
                &c.name, &c.image, &c.service, &c.project, &c.status, &c.ports,
            ]
            .iter()
            .any(|f| contains(f, needle))
    }

    /// The rows of the current tab, in display order.
    pub fn rows(&self) -> Vec<Row> {
        self.rows_of(self.tab)
    }

    pub fn rows_of(&self, tab: Tab) -> Vec<Row> {
        let needle = self.filter.to_lowercase();
        match tab {
            Tab::Containers => {
                let mut out = Vec::new();
                for (g, group) in self.groups.iter().enumerate() {
                    let members: Vec<usize> = group
                        .members
                        .iter()
                        .copied()
                        .filter(|&i| self.visible(&self.containers[i], &needle))
                        .collect();
                    let keep_empty = (group.current || group.declared.is_some())
                        && (needle.is_empty() || contains(&group.key, &needle));
                    if members.is_empty() && !keep_empty {
                        continue;
                    }
                    out.push(Row::Group(g));
                    if self.collapsed.contains(&group.key) {
                        continue;
                    }
                    if members.is_empty() {
                        out.push(Row::Empty(g));
                    }
                    out.extend(members.into_iter().map(Row::Container));
                }
                out
            }
            Tab::Images => {
                let mut idx: Vec<usize> = (0..self.images.len())
                    .filter(|&i| {
                        let im = &self.images[i];
                        needle.is_empty()
                            || contains(&im.reference(), &needle)
                            || contains(&im.id, &needle)
                    })
                    .collect();
                idx.sort_by(|a, b| {
                    let (a, b) = (&self.images[*a], &self.images[*b]);
                    (a.dangling(), a.reference()).cmp(&(b.dangling(), b.reference()))
                });
                idx.into_iter().map(Row::Image).collect()
            }
            Tab::Volumes => {
                let mut idx: Vec<usize> = (0..self.volumes.len())
                    .filter(|&i| {
                        let v = &self.volumes[i];
                        needle.is_empty()
                            || contains(&v.name, &needle)
                            || contains(&v.project, &needle)
                    })
                    .collect();
                idx.sort_by(|a, b| {
                    let (a, b) = (&self.volumes[*a], &self.volumes[*b]);
                    (a.anonymous(), &a.name).cmp(&(b.anonymous(), &b.name))
                });
                idx.into_iter().map(Row::Volume).collect()
            }
            Tab::Networks => {
                let mut idx: Vec<usize> = (0..self.networks.len())
                    .filter(|&i| {
                        let n = &self.networks[i];
                        needle.is_empty()
                            || contains(&n.name, &needle)
                            || contains(&n.driver, &needle)
                    })
                    .collect();
                idx.sort_by(|a, b| {
                    let (a, b) = (&self.networks[*a], &self.networks[*b]);
                    (a.builtin(), &a.name).cmp(&(b.builtin(), &b.name))
                });
                idx.into_iter().map(Row::Network).collect()
            }
            Tab::Project => {
                let matches = |identity: &str, dest: &str| {
                    needle.is_empty() || contains(identity, &needle) || contains(dest, &needle)
                };
                let here = self.here.as_ref();
                let mut idx: Vec<usize> = (0..self.local.catalog.len())
                    .filter(|&i| {
                        let d = &self.local.catalog[i].1;
                        let is_here = here
                            .is_some_and(|h| h.identity == d.identity && h.dest == d.destination);
                        !is_here && matches(&d.identity, &d.destination)
                    })
                    .collect();
                idx.sort_by(|a, b| {
                    let (a, b) = (&self.local.catalog[*a].1, &self.local.catalog[*b].1);
                    (!a.live, &a.identity, &a.destination).cmp(&(
                        !b.live,
                        &b.identity,
                        &b.destination,
                    ))
                });
                let mut out: Vec<Row> = here
                    .filter(|h| matches(&h.identity, &h.dest))
                    .map(|_| Row::Here)
                    .into_iter()
                    .collect();
                out.extend(idx.into_iter().map(Row::Stack));
                out
            }
        }
    }

    fn row_key(&self, row: Row) -> String {
        match row {
            Row::Group(g) => format!("g:{}", self.groups[g].key),
            Row::Empty(g) => format!("e:{}", self.groups[g].key),
            Row::Container(i) => format!("c:{}", self.containers[i].id),
            Row::Image(i) => format!("i:{}:{}", self.images[i].id, self.images[i].reference()),
            Row::Volume(i) => format!("v:{}", self.volumes[i].name),
            Row::Network(i) => format!("n:{}", self.networks[i].id),
            Row::Here => "h".to_string(),
            Row::Stack(i) => format!("s:{}", self.local.catalog[i].0),
        }
    }

    /// Re-find every tab's cursor after the lists changed.
    fn fix_selection(&mut self) {
        for tab in TABS {
            let rows = self.rows_of(tab);
            let found = self.sel[tab.index()]
                .key
                .as_ref()
                .and_then(|key| rows.iter().position(|r| self.row_key(*r) == *key));
            let index = match found {
                Some(i) => i,
                None => self.sel[tab.index()]
                    .index
                    .min(rows.len().saturating_sub(1)),
            };
            let key = rows.get(index).map(|r| self.row_key(*r));
            let sel = &mut self.sel[tab.index()];
            sel.index = index;
            sel.key = key;
        }
    }

    pub fn selected(&self) -> Option<Row> {
        self.rows().get(self.sel[self.tab.index()].index).copied()
    }

    fn select(&mut self, index: usize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let index = index.min(rows.len() - 1);
        let key = self.row_key(rows[index]);
        let sel = &mut self.sel[self.tab.index()];
        sel.index = index;
        sel.key = Some(key);
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.rows().len();
        if len == 0 {
            return;
        }
        let at = self.sel[self.tab.index()].index as isize + delta;
        self.select(at.clamp(0, len as isize - 1) as usize);
    }

    /// The group a row belongs to.
    fn group_of(&self, row: Row) -> Option<usize> {
        match row {
            Row::Group(g) | Row::Empty(g) => Some(g),
            Row::Container(i) => self.groups.iter().position(|g| g.members.contains(&i)),
            _ => None,
        }
    }

    /// localhost ports Ulak keeps for this container's service, from the
    /// service's own report — and only while the stack is declared live,
    /// because a report about a stack nobody keeps is history.
    pub fn ports_home(&self, project: &str, service: &str) -> Vec<u32> {
        if project.is_empty() {
            return Vec::new();
        }
        let id = crate::intent::stack_id(&self.dest, project);
        let live = self
            .local
            .catalog
            .iter()
            .any(|(sid, d)| *sid == id && d.live);
        if !live {
            return Vec::new();
        }
        let mut ports: Vec<u32> = self
            .local
            .statuses
            .iter()
            .filter(|(sid, _)| *sid == id)
            .flat_map(|(_, st)| st.tunnels.iter())
            .filter(|t| t.open && (service.is_empty() || t.service == service))
            .map(|t| t.port)
            .collect();
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    pub fn stack_status(&self, project: &str) -> Option<&crate::intent::Status> {
        let id = crate::intent::stack_id(&self.dest, project);
        self.local
            .statuses
            .iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, st)| st)
    }

    /// `docker system df`'s row for `kind`: `Images`, `Build Cache`, …
    pub fn usage_of(&self, kind: &str) -> Option<&Usage> {
        self.usage.iter().find(|u| u.kind == kind)
    }

    pub fn container_by_id(&self, id: &str) -> Option<&Container> {
        self.containers.iter().find(|c| c.id == id)
    }

    pub fn history(&self, c: &Container) -> Option<&History> {
        self.stats.get(c.short_id())
    }

    // ─── messages ───────────────────────────────────────────────────

    pub fn on_msg(&mut self, msg: Msg, now: Instant) -> Vec<Effect> {
        let mut fx = Vec::new();
        match msg {
            Msg::Snapshot { era, result, took } if era == self.era => match result {
                Ok(snap) => self.take_snapshot(snap, took, now),
                Err(why) => {
                    if !matches!(self.link, Link::Down { .. }) {
                        self.toast(Tone::Bad, format!("lost {}: {why}", self.dest), now);
                        self.link = Link::Down { why, since: now };
                    } else if let Link::Down { why: old, .. } = &mut self.link {
                        *old = why;
                    }
                }
            },
            Msg::Stats { era, samples } if era == self.era => {
                self.stats_up = true;
                for s in samples {
                    self.take_stat(s, now);
                }
            }
            Msg::Event { era, event } if era == self.era => {
                self.events_up = true;
                if let Some(want) = event.stale() {
                    self.last_poke = now;
                    fx.push(Effect::Poke(want));
                }
                match event.notice() {
                    Some(Notice::Bad(text)) => self.toast(Tone::Bad, text, now),
                    Some(Notice::Note(text)) => self.toast(Tone::Note, text, now),
                    None => {}
                }
            }
            Msg::FeedUp { era, feed } if era == self.era => match feed {
                Feed::Events => self.events_up = true,
                Feed::Stats => self.stats_up = true,
            },
            Msg::FeedEnded { era, feed, why } if era == self.era => {
                match feed {
                    Feed::Events => self.events_up = false,
                    Feed::Stats => self.stats_up = false,
                }
                let tries = self
                    .feed_retry
                    .iter()
                    .find(|(f, _, _)| *f == feed)
                    .map(|(_, _, n)| *n)
                    .unwrap_or(0);
                self.feed_retry.retain(|(f, _, _)| *f != feed);
                let wait = FEED_BACKOFF[tries.min(FEED_BACKOFF.len() - 1)];
                self.feed_retry
                    .push((feed, now + Duration::from_secs(wait), tries + 1));
                if tries == 0 && matches!(self.link, Link::Up { .. }) {
                    let name = match feed {
                        Feed::Events => "live updates",
                        Feed::Stats => "live CPU and memory",
                    };
                    self.toast(Tone::Note, format!("{name} paused ({why}) — retrying"), now);
                }
            }
            Msg::Snapshot { .. }
            | Msg::Stats { .. }
            | Msg::Event { .. }
            | Msg::FeedUp { .. }
            | Msg::FeedEnded { .. } => {}
            Msg::Lines {
                stream,
                source,
                lines,
            } => {
                if let Some(view) = self.logs_mut(stream) {
                    for line in &lines {
                        view.push(source, line);
                    }
                }
            }
            Msg::StreamEnded {
                stream,
                source,
                why,
            } => {
                if let Some(view) = self.logs_mut(stream)
                    && let Some(src) = view.sources.get_mut(source)
                {
                    src.ended = Some(why);
                }
            }
            Msg::TaskLine { task, line } => {
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
                    t.lines.push_back(line);
                    if t.lines.len() > TASK_CAP {
                        t.lines.pop_front();
                    }
                }
            }
            Msg::TaskDone { task, code } => {
                let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) else {
                    return fx;
                };
                t.done = Some((code, now));
                let title = t.title.clone();
                let refresh = t.refresh;
                let last = t.lines.iter().rev().find(|l| !l.trim().is_empty()).cloned();
                if matches!(self.overlay, Overlay::Leaving { .. }) {
                    if self.running_tasks() == 0 {
                        self.quit = true;
                    }
                    return fx;
                }
                if code == Some(0) {
                    let said = match last.as_deref().and_then(freed) {
                        Some(size) => format!("{title} · {size} freed"),
                        None => title,
                    };
                    self.toast(Tone::Good, said, now);
                } else {
                    let tail = last.map(|l| format!(": {l}")).unwrap_or_default();
                    self.toast(
                        Tone::Bad,
                        format!("{title} failed{tail}  (T shows the output)"),
                        now,
                    );
                }
                self.last_poke = now;
                fx.push(Effect::Poke(refresh));
                self.trim_tasks();
            }
            Msg::Fetched { req, result } => {
                if let Screen::Detail(d) = &mut self.screen
                    && d.inspect.req == req
                {
                    d.inspect.loading = false;
                    match result {
                        Ok(text) => {
                            d.inspect.lines = pretty(&text).lines().map(str::to_string).collect();
                            d.inspect.error = None;
                        }
                        Err(why) => d.inspect.error = Some(why),
                    }
                }
            }
            Msg::Local(local) => {
                self.drivers = local.drivers.clone();
                self.local = *local;
                self.regroup();
                self.fix_selection();
            }
        }
        fx
    }

    fn trim_tasks(&mut self) {
        let finished: Vec<u64> = self
            .tasks
            .iter()
            .filter(|t| !t.running())
            .map(|t| t.id)
            .collect();
        let excess = finished.len().saturating_sub(TASKS_KEPT);
        let drop: BTreeSet<u64> = finished.into_iter().take(excess).collect();
        self.tasks.retain(|t| !drop.contains(&t.id));
    }

    fn logs_mut(&mut self, stream: u64) -> Option<&mut LogView> {
        match &mut self.screen {
            Screen::Detail(d) => d.logs.as_mut().filter(|l| l.stream == stream),
            Screen::Main => None,
        }
    }

    fn take_snapshot(&mut self, snap: Snapshot, took: Duration, now: Instant) {
        if matches!(self.link, Link::Down { .. }) {
            self.toast(Tone::Good, format!("{} is back", self.dest), now);
        }
        self.link = Link::Up { took };
        self.engine = snap.engine;
        self.containers = snap.containers;
        self.containers_loaded = true;
        if let Some(images) = snap.images {
            self.images = images;
            self.loaded.images = true;
        }
        if let Some(volumes) = snap.volumes {
            self.volumes = volumes;
            self.loaded.volumes = true;
        }
        if let Some(networks) = snap.networks {
            self.networks = networks;
            self.loaded.networks = true;
        }
        if let Some(info) = snap.info {
            self.info = Some(info);
            self.loaded.info = true;
        }
        if snap.disk.is_some() {
            self.disk = snap.disk;
        }
        if let Some(usage) = snap.usage {
            self.usage = usage;
            self.loaded.usage = true;
        }
        self.trouble = snap.trouble;
        // Samples for containers that are gone are history nobody can see.
        let alive: BTreeSet<&str> = self
            .containers
            .iter()
            .filter(|c| c.running() || c.paused())
            .map(|c| c.short_id())
            .collect();
        self.stats.retain(|id, _| alive.contains(id.as_str()));
        self.regroup();
        self.fix_selection();
    }

    /// One sample a second at most: the stream ticks twice a second, and
    /// a sparkline of half-second jitter says less than one of seconds.
    fn take_stat(&mut self, s: Stat, now: Instant) {
        match self.stats.get_mut(&s.id) {
            Some(h) => {
                let fresh = now.duration_since(h.at) >= Duration::from_millis(900);
                if fresh {
                    h.at = now;
                }
                for (series, value) in [
                    (&mut h.cpu, s.cpu),
                    (&mut h.mem, s.mem),
                    (&mut h.used, s.mem_bytes),
                ] {
                    match series.back_mut() {
                        Some(last) if !fresh => *last = value,
                        _ => series.push_back(value),
                    }
                    while series.len() > HISTORY {
                        series.pop_front();
                    }
                }
                h.last = s;
            }
            None => {
                self.stats.insert(
                    s.id.clone(),
                    History {
                        cpu: VecDeque::from([s.cpu]),
                        mem: VecDeque::from([s.mem]),
                        used: VecDeque::from([s.mem_bytes]),
                        last: s,
                        at: now,
                    },
                );
            }
        }
    }

    /// The UI thread's heartbeat: expire toasts, restart feeds, keep the
    /// list fresh while live updates are down, and hand over any message
    /// Ulak's own helpers held back while the dashboard owned the screen.
    pub fn on_tick(&mut self, now: Instant, held: Vec<(crate::ui::Level, String)>) -> Vec<Effect> {
        self.frame = self.frame.wrapping_add(1);
        if let Overlay::Leaving { since } = self.overlay
            && now.duration_since(since) >= LEAVE_GRACE
        {
            self.quit = true;
        }
        for (level, text) in held {
            let tone = match level {
                crate::ui::Level::Warn => Tone::Bad,
                crate::ui::Level::Ok => Tone::Good,
                crate::ui::Level::Info | crate::ui::Level::Dim => Tone::Note,
            };
            self.toast(tone, text, now);
        }
        self.toasts.retain(|t| {
            let ttl = if t.tone == Tone::Bad {
                BAD_TOAST_FOR
            } else {
                TOAST_FOR
            };
            now.duration_since(t.at) < ttl
        });
        let mut fx = Vec::new();
        let due: Vec<Feed> = self
            .feed_retry
            .iter()
            .filter(|(_, at, _)| now >= *at)
            .map(|(f, _, _)| *f)
            .collect();
        for feed in due {
            // Keep the attempt count; a healthy feed resets it below.
            if let Some(entry) = self.feed_retry.iter_mut().find(|(f, _, _)| *f == feed) {
                entry.1 = now + Duration::from_secs(3600);
            }
            fx.push(Effect::RestartFeed(feed));
        }
        if self.events_up {
            self.feed_retry.retain(|(f, _, _)| *f != Feed::Events);
        }
        if self.stats_up {
            self.feed_retry.retain(|(f, _, _)| *f != Feed::Stats);
        }
        if !self.events_up
            && matches!(self.link, Link::Up { .. })
            && now.duration_since(self.last_poke) >= BLIND_POLL
        {
            self.last_poke = now;
            fx.push(Effect::Poke(Want::default()));
        }
        fx
    }

    // ─── input ──────────────────────────────────────────────────────

    pub fn on_key(&mut self, key: KeyEvent, now: Instant) -> Vec<Effect> {
        let ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        match self.overlay.clone() {
            // Read, so that it never reaches the shell, and dropped: only
            // the quit keys mean anything now, and they mean "do not wait".
            Overlay::Leaving { .. } => {
                if ctrl_c || key.code == KeyCode::Char('q') {
                    self.quit = true;
                }
                return Vec::new();
            }
            _ if ctrl_c => return self.leave(now),
            Overlay::None => {}
            Overlay::Help { scroll } => {
                // Scrolling keys page through it; anything else closes it.
                self.overlay = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => Overlay::Help {
                        scroll: scroll.saturating_sub(1),
                    },
                    KeyCode::Down | KeyCode::Char('j') => Overlay::Help { scroll: scroll + 1 },
                    KeyCode::PageUp => Overlay::Help {
                        scroll: scroll.saturating_sub(10),
                    },
                    KeyCode::PageDown => Overlay::Help {
                        scroll: scroll + 10,
                    },
                    _ => Overlay::None,
                };
                return Vec::new();
            }
            Overlay::Confirm(c) => return self.on_confirm_key(c, key, now),
            Overlay::Input(input) => return self.on_input_key(input, key, now),
            Overlay::Tasks { selected, scroll } => return self.on_tasks_key(selected, scroll, key),
            Overlay::Servers { selected } => return self.on_servers_key(selected, key, now),
        }
        // Keys every screen shares.
        match key.code {
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help { scroll: 0 };
                return Vec::new();
            }
            KeyCode::Char('T') => {
                self.overlay = Overlay::Tasks {
                    selected: self.tasks.len().saturating_sub(1),
                    scroll: 0,
                };
                return Vec::new();
            }
            KeyCode::Char('@') => {
                let servers = self.servers();
                let selected = servers.iter().position(|s| *s == self.dest).unwrap_or(0);
                self.overlay = Overlay::Servers { selected };
                return Vec::new();
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.last_poke = now;
                self.toast(Tone::Note, "refreshing…", now);
                return vec![Effect::Poke(Want::all())];
            }
            KeyCode::Char(':') => {
                self.overlay = Overlay::Input(Input {
                    prompt: Prompt::Command,
                    text: String::new(),
                    before: String::new(),
                });
                return Vec::new();
            }
            _ => {}
        }
        match &self.screen {
            Screen::Main => self.on_main_key(key, now),
            Screen::Detail(_) => self.on_detail_key(key, now),
        }
    }

    fn on_main_key(&mut self, key: KeyEvent, now: Instant) -> Vec<Effect> {
        let page = 10;
        match key.code {
            KeyCode::Char('q') => return self.ask_quit(),
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.fix_selection();
                }
                return Vec::new();
            }
            KeyCode::Tab | KeyCode::Right => return self.switch_tab(self.tab.index() as isize + 1),
            KeyCode::BackTab | KeyCode::Left => {
                return self.switch_tab(self.tab.index() as isize - 1);
            }
            KeyCode::Char(c @ '1'..='5') => {
                return self.switch_tab(c as isize - '1' as isize);
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(usize::MAX),
            KeyCode::Char('/') => {
                self.overlay = Overlay::Input(Input {
                    prompt: Prompt::Filter,
                    text: self.filter.clone(),
                    before: self.filter.clone(),
                });
            }
            // The tab's own verbs, so they work with nothing selected: a
            // filter that matches nothing, or nothing left to list.
            KeyCode::Char('P') if self.tab != Tab::Project => {
                let action = match self.tab {
                    Tab::Images => verbs::prune_images(self.usage_of("Images")),
                    Tab::Volumes => verbs::prune_volumes(),
                    Tab::Networks => verbs::prune_networks(),
                    Tab::Containers | Tab::Project => verbs::prune_containers(),
                };
                return self.act(action, now);
            }
            KeyCode::Char('B') if self.tab == Tab::Images => {
                return self.act(verbs::prune_build_cache(self.usage_of("Build Cache")), now);
            }
            KeyCode::Char('a') if self.tab == Tab::Containers => {
                self.show_stopped = !self.show_stopped;
                self.fix_selection();
                self.toast(
                    Tone::Note,
                    if self.show_stopped {
                        "showing stopped containers"
                    } else {
                        "hiding stopped containers"
                    },
                    now,
                );
            }
            _ => return self.on_row_key(key, now),
        }
        Vec::new()
    }

    fn switch_tab(&mut self, to: isize) -> Vec<Effect> {
        let n = TABS.len() as isize;
        self.tab = TABS[to.rem_euclid(n) as usize];
        let want = self.tab.interest();
        let missing = Want {
            images: want.images && !self.loaded.images,
            volumes: want.volumes && !self.loaded.volumes,
            networks: want.networks && !self.loaded.networks,
            usage: want.usage && !self.loaded.usage,
            info: false,
        };
        let mut fx = vec![Effect::Interest(want)];
        if missing != Want::default() {
            fx.push(Effect::Poke(missing));
        }
        fx
    }

    /// Quit, after asking running tasks to stop. While they stop, the
    /// dashboard stays up, drawn and reading every key, and quits when
    /// the last one is done, when `LEAVE_GRACE` is up, or at a second `q`
    /// or Ctrl-C. The wait used to come after the loop, with nobody
    /// reading: every click and key made during it was left in the
    /// terminal for the shell.
    fn leave(&mut self, now: Instant) -> Vec<Effect> {
        let running: Vec<u64> = self
            .tasks
            .iter()
            .filter(|t| t.running())
            .map(|t| t.id)
            .collect();
        if running.is_empty() {
            self.quit = true;
            return Vec::new();
        }
        self.overlay = Overlay::Leaving { since: now };
        running
            .into_iter()
            .map(|task| Effect::Cancel { task })
            .collect()
    }

    fn ask_quit(&mut self) -> Vec<Effect> {
        let running = self.running_tasks();
        if running == 0 {
            self.quit = true;
        } else {
            self.overlay = Overlay::Confirm(Confirm {
                question: format!(
                    "{running} task(s) still running. Quit and stop them (like Ctrl-C would)?"
                ),
                then: Then::Quit,
                yes: false,
            });
        }
        Vec::new()
    }

    /// Ask first when the action says so, otherwise run it now.
    fn act(&mut self, action: Action, now: Instant) -> Vec<Effect> {
        if let Some(question) = action.confirm.clone() {
            self.overlay = Overlay::Confirm(Confirm {
                question,
                then: Then::Run(action),
                yes: false,
            });
            return Vec::new();
        }
        self.run(action, now)
    }

    fn run(&mut self, action: Action, now: Instant) -> Vec<Effect> {
        let task = self.id();
        self.tasks.push(TaskView {
            id: task,
            title: action.title.clone(),
            lines: VecDeque::new(),
            started: now,
            done: None,
            refresh: action.refresh,
        });
        vec![Effect::Run { task, action }]
    }

    fn on_row_key(&mut self, key: KeyEvent, now: Instant) -> Vec<Effect> {
        let Some(row) = self.selected() else {
            return Vec::new();
        };
        match row {
            Row::Container(i) => self.on_container_key(self.containers[i].clone(), key, now),
            Row::Group(g) | Row::Empty(g) => self.on_group_key(g, key, now),
            Row::Image(i) => {
                let image = self.images[i].clone();
                match key.code {
                    KeyCode::Enter | KeyCode::Char('i') => self.open(
                        Subject::Image(image.id.clone()),
                        image.reference(),
                        Pane::Inspect,
                    ),
                    KeyCode::Char('d') | KeyCode::Delete => {
                        self.act(verbs::remove_image(&image), now)
                    }
                    KeyCode::Char('p') => match verbs::pull_image(&image) {
                        Some(action) => self.act(action, now),
                        None => {
                            self.toast(
                                Tone::Note,
                                "nothing names this image, so there is nothing to pull",
                                now,
                            );
                            Vec::new()
                        }
                    },
                    KeyCode::Char('y') => self.copy(image.reference(), now),
                    _ => Vec::new(),
                }
            }
            Row::Volume(i) => {
                let volume = self.volumes[i].clone();
                match key.code {
                    KeyCode::Enter | KeyCode::Char('i') => self.open(
                        Subject::Volume(volume.name.clone()),
                        volume.name.clone(),
                        Pane::Inspect,
                    ),
                    KeyCode::Char('d') | KeyCode::Delete => {
                        self.act(verbs::remove_volume(&volume), now)
                    }
                    KeyCode::Char('y') => self.copy(volume.name.clone(), now),
                    _ => Vec::new(),
                }
            }
            Row::Network(i) => {
                let network = self.networks[i].clone();
                match key.code {
                    KeyCode::Enter | KeyCode::Char('i') => self.open(
                        Subject::Network(network.id.clone()),
                        network.name.clone(),
                        Pane::Inspect,
                    ),
                    KeyCode::Char('d') | KeyCode::Delete => {
                        if network.builtin() {
                            self.toast(
                                Tone::Note,
                                format!(
                                    "{} is Docker's own network and cannot be removed",
                                    network.name
                                ),
                                now,
                            );
                            Vec::new()
                        } else {
                            self.act(verbs::remove_network(&network), now)
                        }
                    }
                    KeyCode::Char('y') => self.copy(network.id.clone(), now),
                    _ => Vec::new(),
                }
            }
            Row::Here | Row::Stack(_) => self.on_stack_key(row, key, now),
        }
    }

    fn on_container_key(&mut self, c: Container, key: KeyEvent, now: Instant) -> Vec<Effect> {
        match key.code {
            KeyCode::Enter | KeyCode::Char('l') => {
                let title = c.name.clone();
                self.open(Subject::Container(c.id.clone()), title, Pane::Logs)
            }
            KeyCode::Char('i') => {
                let title = c.name.clone();
                self.open(Subject::Container(c.id.clone()), title, Pane::Inspect)
            }
            KeyCode::Char(' ') => {
                if let Some(g) = self.group_of(Row::Container(
                    self.containers
                        .iter()
                        .position(|x| x.id == c.id)
                        .unwrap_or(0),
                )) {
                    self.toggle(g);
                }
                Vec::new()
            }
            _ => self.container_verb(&c, key, now),
        }
    }

    /// The verbs a container answers on the list and on its own page.
    fn container_verb(&mut self, c: &Container, key: KeyEvent, now: Instant) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('e') => {
                if c.running() {
                    vec![Effect::Shell(Box::new(c.clone()))]
                } else {
                    self.toast(
                        Tone::Note,
                        format!("{} is not running — start it first (s)", c.name),
                        now,
                    );
                    Vec::new()
                }
            }
            KeyCode::Char('s') => {
                let action = if c.paused() {
                    verbs::pause(c)
                } else if c.running() {
                    verbs::stop(&[c])
                } else {
                    verbs::start(&[c])
                };
                self.act(action, now)
            }
            KeyCode::Char('r') => self.act(verbs::restart(&[c]), now),
            KeyCode::Char('p') => {
                if c.running() || c.paused() {
                    self.act(verbs::pause(c), now)
                } else {
                    Vec::new()
                }
            }
            KeyCode::Char('K') => {
                if c.running() {
                    self.act(verbs::kill(c), now)
                } else {
                    Vec::new()
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => self.act(verbs::remove(&[c]), now),
            KeyCode::Char('o') => self.open_browser(&c.project, &c.service, &c.name, now),
            KeyCode::Char('y') => self.copy(c.id.clone(), now),
            _ => Vec::new(),
        }
    }

    fn open_browser(
        &mut self,
        project: &str,
        service: &str,
        name: &str,
        now: Instant,
    ) -> Vec<Effect> {
        match self.ports_home(project, service).first() {
            Some(port) => {
                let url = format!("http://localhost:{port}");
                self.toast(Tone::Note, format!("opening {url}"), now);
                vec![Effect::Open(url)]
            }
            None => {
                self.toast(
                    Tone::Note,
                    format!("{name} has no localhost port — Ulak forwards the published ports of stacks it keeps live"),
                    now,
                );
                Vec::new()
            }
        }
    }

    fn copy(&mut self, text: String, now: Instant) -> Vec<Effect> {
        self.toast(Tone::Note, format!("copied {text}"), now);
        vec![Effect::Copy(text)]
    }

    fn toggle(&mut self, g: usize) {
        let key = self.groups[g].key.clone();
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        // Collapsing hides the cursor's row; land on the group itself.
        let rows = self.rows();
        if let Some(i) = rows.iter().position(|r| *r == Row::Group(g)) {
            self.select(i);
        }
    }

    fn on_group_key(&mut self, g: usize, key: KeyEvent, now: Instant) -> Vec<Effect> {
        let group = self.groups[g].clone();
        let members: Vec<Container> = group
            .members
            .iter()
            .map(|&i| self.containers[i].clone())
            .filter(|c| !c.oneoff())
            .collect();
        let refs: Vec<&Container> = members.iter().collect();
        let any_running = members.iter().any(|c| c.running() || c.paused());
        let stack = group.title().to_string();
        let no_driver = |app: &mut App| {
            app.toast(
                Tone::Note,
                format!("{stack} was not started from this machine — Compose verbs need its files; manage its containers one by one"),
                now,
            );
            Vec::new()
        };
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.toggle(g);
                Vec::new()
            }
            KeyCode::Char('l') => {
                if members.is_empty() {
                    return Vec::new();
                }
                let stream = self.id();
                let sources: Vec<Source> = members
                    .iter()
                    .map(|c| Source {
                        name: if c.service.is_empty() {
                            c.name.clone()
                        } else {
                            c.service.clone()
                        },
                        id: c.id.clone(),
                        ended: None,
                    })
                    .collect();
                let ids = sources.iter().map(|s| s.id.clone()).collect();
                self.close_detail_fx();
                self.screen = Screen::Detail(Box::new(Detail {
                    subject: Subject::Stack(group.key.clone()),
                    title: stack,
                    pane: Pane::Logs,
                    logs: Some(LogView::new(stream, sources)),
                    inspect: TextView::default(),
                }));
                vec![Effect::OpenLogs { stream, ids }]
            }
            KeyCode::Char('u') => match &group.driver {
                Some(d) => {
                    let action = verbs::compose(d, &stack, &["up", "-d"]);
                    self.act(action, now)
                }
                None if group.key.is_empty() => Vec::new(),
                None => no_driver(self),
            },
            KeyCode::Char('D') => match &group.driver {
                Some(d) => {
                    let action = verbs::compose(d, &stack, &["down"]);
                    self.act(action, now)
                }
                None if group.key.is_empty() => Vec::new(),
                None => no_driver(self),
            },
            KeyCode::Char('s') => {
                let action = match (&group.driver, any_running) {
                    (Some(d), true) => verbs::compose(d, &stack, &["stop"]),
                    (Some(d), false) if !members.is_empty() => {
                        verbs::compose(d, &stack, &["start"])
                    }
                    (Some(d), false) => verbs::compose(d, &stack, &["up", "-d"]),
                    (None, _) if refs.is_empty() => return Vec::new(),
                    (None, true) => verbs::stop(
                        &refs
                            .iter()
                            .copied()
                            .filter(|c| c.running() || c.paused())
                            .collect::<Vec<_>>(),
                    ),
                    (None, false) => verbs::start(&refs),
                };
                self.act(action, now)
            }
            KeyCode::Char('r') => match (&group.driver, refs.is_empty()) {
                (_, true) => Vec::new(),
                (Some(d), false) => {
                    let action = verbs::compose(d, &stack, &["restart"]);
                    self.act(action, now)
                }
                (None, false) => self.act(verbs::restart(&refs), now),
            },
            KeyCode::Char('S') => match &group.driver {
                Some(d) => {
                    let action = verbs::sync(d, &stack);
                    self.act(action, now)
                }
                None => no_driver(self),
            },
            KeyCode::Char('o') => self.open_browser(&group.key, "", &stack, now),
            _ => Vec::new(),
        }
    }

    /// A stack on the Ulak tab: who it is, where, and how to drive it.
    pub fn stack_of(&self, row: Row) -> Option<(String, String, Option<Driver>)> {
        match row {
            Row::Here => self
                .here
                .as_ref()
                .map(|h| (h.identity.clone(), h.dest.clone(), Some(h.driver.clone()))),
            Row::Stack(i) => self.local.catalog.get(i).map(|(id, d)| {
                (
                    d.identity.clone(),
                    d.destination.clone(),
                    self.drivers.get(id).cloned(),
                )
            }),
            _ => None,
        }
    }

    fn on_stack_key(&mut self, row: Row, key: KeyEvent, now: Instant) -> Vec<Effect> {
        let Some((stack, dest, driver)) = self.stack_of(row) else {
            return Vec::new();
        };
        let elsewhere = dest != self.dest;
        match key.code {
            KeyCode::Enter if elsewhere => self.switch(dest, now),
            KeyCode::Enter => {
                // Show it on the containers tab.
                self.tab = Tab::Containers;
                if let Some(g) = self.groups.iter().position(|g| g.key == stack) {
                    self.collapsed.remove(&stack);
                    let rows = self.rows();
                    if let Some(at) = rows.iter().position(|r| *r == Row::Group(g)) {
                        self.select(at);
                    }
                }
                vec![Effect::Interest(Tab::Containers.interest())]
            }
            KeyCode::Char('u' | 'D' | 'S' | 'c') => {
                let Some(d) = driver else {
                    self.toast(Tone::Bad, format!("the declaration for {stack} no longer rebuilds — its compose file may be gone"), now);
                    return Vec::new();
                };
                let action = match key.code {
                    KeyCode::Char('u') => verbs::compose(&d, &stack, &["up", "-d"]),
                    KeyCode::Char('D') => verbs::compose(&d, &stack, &["down"]),
                    KeyCode::Char('S') => verbs::sync(&d, &stack),
                    _ => verbs::doctor(&d, &stack),
                };
                self.act(action, now)
            }
            _ => Vec::new(),
        }
    }

    pub fn switch(&mut self, dest: String, now: Instant) -> Vec<Effect> {
        if dest == self.dest {
            return Vec::new();
        }
        let mut fx = self.close_detail_fx();
        self.screen = Screen::Main;
        self.dest = dest.clone();
        self.era += 1;
        self.containers.clear();
        self.images.clear();
        self.volumes.clear();
        self.networks.clear();
        self.usage.clear();
        self.info = None;
        self.disk = None;
        self.trouble.clear();
        self.stats.clear();
        self.loaded = Want::default();
        self.containers_loaded = false;
        self.events_up = false;
        self.stats_up = false;
        self.feed_retry.clear();
        self.link = Link::Connecting;
        self.sel = Default::default();
        self.regroup();
        self.toast(Tone::Note, format!("switching to {dest}"), now);
        fx.push(Effect::Switch(dest));
        fx
    }

    fn open(&mut self, subject: Subject, title: String, pane: Pane) -> Vec<Effect> {
        let mut fx = self.close_detail_fx();
        let mut detail = Detail {
            subject,
            title,
            pane,
            logs: None,
            inspect: TextView::default(),
        };
        fx.extend(self.load_pane(&mut detail));
        self.screen = Screen::Detail(Box::new(detail));
        fx
    }

    /// Start whatever the pane shows, if it has not started yet.
    fn load_pane(&mut self, detail: &mut Detail) -> Vec<Effect> {
        match (detail.pane, &detail.subject) {
            (Pane::Logs, Subject::Container(id)) if detail.logs.is_none() => {
                let stream = self.id();
                let name = self
                    .container_by_id(id)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| id.clone());
                detail.logs = Some(LogView::new(
                    stream,
                    vec![Source {
                        name,
                        id: id.clone(),
                        ended: None,
                    }],
                ));
                vec![Effect::OpenLogs {
                    stream,
                    ids: vec![id.clone()],
                }]
            }
            (Pane::Inspect, subject)
                if !detail.inspect.loading && detail.inspect.lines.is_empty() =>
            {
                let id = match subject {
                    Subject::Container(id)
                    | Subject::Image(id)
                    | Subject::Volume(id)
                    | Subject::Network(id) => id.clone(),
                    Subject::Stack(_) => return Vec::new(),
                };
                match verbs::inspect_argv(subject.kind(), &id) {
                    Ok(argv) => {
                        let req = self.id();
                        detail.inspect.req = req;
                        detail.inspect.loading = true;
                        detail.inspect.error = None;
                        vec![Effect::Fetch { req, argv }]
                    }
                    Err(e) => {
                        detail.inspect.error = Some(crate::ui::flatten(&e));
                        Vec::new()
                    }
                }
            }
            _ => Vec::new(),
        }
    }

    fn close_detail_fx(&mut self) -> Vec<Effect> {
        match &self.screen {
            Screen::Detail(d) => match &d.logs {
                Some(logs) => vec![Effect::CloseLogs {
                    stream: logs.stream,
                }],
                None => Vec::new(),
            },
            Screen::Main => Vec::new(),
        }
    }

    fn back(&mut self) -> Vec<Effect> {
        let fx = self.close_detail_fx();
        self.screen = Screen::Main;
        fx
    }

    fn on_detail_key(&mut self, key: KeyEvent, now: Instant) -> Vec<Effect> {
        let Screen::Detail(detail) = &self.screen else {
            return Vec::new();
        };
        let panes = detail.panes();
        let pane = detail.pane;
        let subject = detail.subject.clone();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => return self.back(),
            KeyCode::Tab | KeyCode::Right | KeyCode::BackTab | KeyCode::Left if panes.len() > 1 => {
                let at = panes.iter().position(|p| *p == pane).unwrap_or(0) as isize;
                let step = if matches!(key.code, KeyCode::Tab | KeyCode::Right) {
                    1
                } else {
                    -1
                };
                let next = panes[(at + step).rem_euclid(panes.len() as isize) as usize];
                return self.show_pane(next);
            }
            KeyCode::Char(c @ '1'..='3') => {
                if let Some(next) = panes.get(c as usize - '1' as usize).copied() {
                    return self.show_pane(next);
                }
                return Vec::new();
            }
            KeyCode::Char('/') if pane != Pane::Stats => {
                self.overlay = Overlay::Input(Input {
                    prompt: Prompt::Search,
                    text: String::new(),
                    before: String::new(),
                });
                return Vec::new();
            }
            _ => {}
        }
        if let Some(fx) = self.on_pane_key(pane, key) {
            return fx;
        }
        // The subject's own verbs, as on the list.
        match subject {
            Subject::Container(id) => match self.container_by_id(&id).cloned() {
                Some(c) => self.container_verb(&c, key, now),
                None => Vec::new(),
            },
            Subject::Image(_) | Subject::Volume(_) | Subject::Network(_) | Subject::Stack(_) => {
                Vec::new()
            }
        }
    }

    fn show_pane(&mut self, next: Pane) -> Vec<Effect> {
        let Screen::Detail(detail) = &mut self.screen else {
            return Vec::new();
        };
        let mut detail = std::mem::replace(
            detail,
            Box::new(Detail {
                subject: Subject::Stack(String::new()),
                title: String::new(),
                pane: Pane::Logs,
                logs: None,
                inspect: TextView::default(),
            }),
        );
        detail.pane = next;
        let fx = self.load_pane(&mut detail);
        self.screen = Screen::Detail(detail);
        fx
    }

    /// Scrolling and toggles inside a pane; `None` when the key is not
    /// the pane's, so the subject's verbs get a turn.
    fn on_pane_key(&mut self, pane: Pane, key: KeyEvent) -> Option<Vec<Effect>> {
        let Screen::Detail(detail) = &mut self.screen else {
            return None;
        };
        let page = 20isize;
        match pane {
            Pane::Logs => {
                let logs = detail.logs.as_mut()?;
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => logs.scroll_by(1),
                    KeyCode::Down | KeyCode::Char('j') => logs.scroll_by(-1),
                    KeyCode::PageUp => logs.scroll_by(page),
                    KeyCode::PageDown => logs.scroll_by(-page),
                    KeyCode::Home | KeyCode::Char('g') => logs.scroll_by(isize::MAX / 2),
                    KeyCode::End | KeyCode::Char('G') => logs.scroll_by(isize::MIN / 2),
                    KeyCode::Char('f') => {
                        logs.follow = !logs.follow;
                        if logs.follow {
                            logs.scroll = 0;
                        }
                    }
                    KeyCode::Char('w') => logs.wrap = !logs.wrap,
                    KeyCode::Char('t') => logs.timestamps = !logs.timestamps,
                    KeyCode::Char('n') => {
                        logs.jump(true);
                    }
                    KeyCode::Char('N') => {
                        logs.jump(false);
                    }
                    KeyCode::Char('c') => {
                        logs.lines.clear();
                        logs.scroll = 0;
                        logs.follow = true;
                        logs.hit = None;
                    }
                    _ => return None,
                }
                Some(Vec::new())
            }
            Pane::Inspect => {
                let text = &mut detail.inspect;
                let max = text.lines.len().saturating_sub(1);
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => text.scroll = text.scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => text.scroll = (text.scroll + 1).min(max),
                    KeyCode::PageUp => text.scroll = text.scroll.saturating_sub(page as usize),
                    KeyCode::PageDown => text.scroll = (text.scroll + page as usize).min(max),
                    KeyCode::Home | KeyCode::Char('g') => text.scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => text.scroll = max,
                    KeyCode::Char('n') => {
                        text.jump(true);
                    }
                    KeyCode::Char('N') => {
                        text.jump(false);
                    }
                    _ => return None,
                }
                Some(Vec::new())
            }
            Pane::Stats => None,
        }
    }

    fn on_confirm_key(&mut self, mut c: Confirm, key: KeyEvent, now: Instant) -> Vec<Effect> {
        let decided = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                Some(false)
            }
            KeyCode::Enter => Some(c.yes),
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Char('h')
            | KeyCode::Char('l') => {
                c.yes = !c.yes;
                self.overlay = Overlay::Confirm(c);
                return Vec::new();
            }
            _ => None,
        };
        match decided {
            None => Vec::new(),
            Some(false) => {
                self.overlay = Overlay::None;
                // Said, so an Enter on the default reads as the No it is
                // and not as a command that ran and did nothing.
                if let Then::Run(action) = &c.then {
                    self.toast(Tone::Note, format!("{}: cancelled", action.title), now);
                }
                Vec::new()
            }
            Some(true) => {
                self.overlay = Overlay::None;
                match c.then {
                    Then::Quit => self.leave(now),
                    Then::Run(action) => {
                        let deleting_here = matches!(
                            (&self.screen, &action.plan),
                            (Screen::Detail(d), verbs::Plan::Daemon(argv))
                                if matches!(&d.subject, Subject::Container(id) if argv.first().map(String::as_str) == Some("rm") && argv.contains(id))
                        );
                        let mut fx = self.run(action, now);
                        if deleting_here {
                            fx.extend(self.back());
                        }
                        fx
                    }
                }
            }
        }
    }

    fn on_input_key(&mut self, mut input: Input, key: KeyEvent, now: Instant) -> Vec<Effect> {
        match key.code {
            KeyCode::Esc => {
                if input.prompt == Prompt::Filter {
                    self.filter = input.before.clone();
                    self.fix_selection();
                }
                self.overlay = Overlay::None;
                return Vec::new();
            }
            KeyCode::Enter => {
                self.overlay = Overlay::None;
                return self.submit(input, now);
            }
            KeyCode::Backspace => {
                input.text.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.text.clear()
            }
            KeyCode::Char(ch) => input.text.push(ch),
            _ => {}
        }
        if input.prompt == Prompt::Filter {
            // Live: the list narrows as you type.
            self.filter = input.text.clone();
            self.fix_selection();
            self.select(0);
        }
        self.overlay = Overlay::Input(input);
        Vec::new()
    }

    fn submit(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        match input.prompt {
            Prompt::Filter => {
                self.filter = input.text;
                self.fix_selection();
                Vec::new()
            }
            Prompt::Search => {
                let term = (!input.text.is_empty()).then_some(input.text);
                let mut missed = None;
                if let Screen::Detail(d) = &mut self.screen {
                    match d.pane {
                        Pane::Logs => {
                            if let Some(logs) = d.logs.as_mut() {
                                logs.search = term.clone();
                                logs.hit = None;
                                if term.is_some() && !logs.jump(true) && !logs.jump(false) {
                                    missed = term.clone();
                                }
                            }
                        }
                        Pane::Inspect => {
                            d.inspect.search = term.clone();
                            if term.is_some() {
                                d.inspect.scroll = 0;
                                let first = d.inspect.lines.first().is_some_and(|l| {
                                    l.to_lowercase()
                                        .contains(&term.clone().unwrap_or_default().to_lowercase())
                                });
                                if !first && !d.inspect.jump(true) {
                                    missed = term.clone();
                                }
                            }
                        }
                        Pane::Stats => {}
                    }
                }
                if let Some(term) = missed {
                    self.toast(Tone::Note, format!("no match for “{term}”"), now);
                }
                Vec::new()
            }
            Prompt::Command => {
                if input.text.trim().is_empty() {
                    return Vec::new();
                }
                let (cwd, env) = match self.here.as_ref().filter(|h| h.dest == self.dest) {
                    Some(h) => (h.driver.cwd.clone(), h.driver.env.clone()),
                    None => (
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
                        crate::config::env_var_for("host").map(|v| (v, self.dest.clone())),
                    ),
                };
                match verbs::typed(cwd, env, &input.text) {
                    Ok(action) => {
                        let fx = self.run(action, now);
                        self.overlay = Overlay::Tasks {
                            selected: self.tasks.len().saturating_sub(1),
                            scroll: 0,
                        };
                        fx
                    }
                    Err(e) => {
                        self.toast(Tone::Bad, crate::ui::flatten(&e), now);
                        Vec::new()
                    }
                }
            }
        }
    }

    fn on_tasks_key(&mut self, selected: usize, scroll: usize, key: KeyEvent) -> Vec<Effect> {
        let len = self.tasks.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('T') => self.overlay = Overlay::None,
            KeyCode::Up | KeyCode::Char('k') => {
                self.overlay = Overlay::Tasks {
                    selected: selected.saturating_sub(1),
                    scroll: 0,
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.overlay = Overlay::Tasks {
                    selected: (selected + 1).min(len.saturating_sub(1)),
                    scroll: 0,
                }
            }
            KeyCode::PageUp => {
                self.overlay = Overlay::Tasks {
                    selected,
                    scroll: scroll + 10,
                }
            }
            KeyCode::PageDown => {
                self.overlay = Overlay::Tasks {
                    selected,
                    scroll: scroll.saturating_sub(10),
                }
            }
            KeyCode::Char('x') => {
                if let Some(t) = self.tasks.get(selected).filter(|t| t.running()) {
                    return vec![Effect::Cancel { task: t.id }];
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_servers_key(&mut self, selected: usize, key: KeyEvent, now: Instant) -> Vec<Effect> {
        let servers = self.servers();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('@') => self.overlay = Overlay::None,
            KeyCode::Up | KeyCode::Char('k') => {
                self.overlay = Overlay::Servers {
                    selected: selected.saturating_sub(1),
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.overlay = Overlay::Servers {
                    selected: (selected + 1).min(servers.len().saturating_sub(1)),
                }
            }
            KeyCode::Enter => {
                self.overlay = Overlay::None;
                if let Some(dest) = servers.get(selected).cloned() {
                    return self.switch(dest, now);
                }
            }
            _ => {}
        }
        Vec::new()
    }

    pub fn on_mouse(&mut self, ev: MouseEvent, hits: &Hits, now: Instant) -> Vec<Effect> {
        if let Overlay::Help { scroll } = self.overlay {
            self.overlay = match ev.kind {
                MouseEventKind::ScrollUp => Overlay::Help {
                    scroll: scroll.saturating_sub(3),
                },
                MouseEventKind::ScrollDown => Overlay::Help { scroll: scroll + 3 },
                MouseEventKind::Down(_) => Overlay::None,
                _ => Overlay::Help { scroll },
            };
            return Vec::new();
        }
        let at = Position::new(ev.column, ev.row);
        if ev.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(key) = hits
                .keys
                .iter()
                .find(|(r, _)| r.contains(at))
                .and_then(|(_, label)| key_named(label))
        {
            return self.on_key(key, now);
        }
        if self.overlay != Overlay::None {
            return self.on_dialog_mouse(ev, at, hits, now);
        }
        match ev.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = matches!(ev.kind, MouseEventKind::ScrollUp);
                match &mut self.screen {
                    Screen::Main => self.move_by(if up { -3 } else { 3 }),
                    Screen::Detail(d) => match d.pane {
                        Pane::Logs => {
                            if let Some(l) = d.logs.as_mut() {
                                l.scroll_by(if up { 3 } else { -3 });
                            }
                        }
                        Pane::Inspect => {
                            let max = d.inspect.lines.len().saturating_sub(1);
                            d.inspect.scroll = if up {
                                d.inspect.scroll.saturating_sub(3)
                            } else {
                                (d.inspect.scroll + 3).min(max)
                            };
                        }
                        Pane::Stats => {}
                    },
                }
                Vec::new()
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, tab)) = hits.tabs.iter().find(|(r, _)| r.contains(at)) {
                    let to = tab.index() as isize;
                    self.screen = Screen::Main;
                    return self.switch_tab(to);
                }
                if let Some((_, pane)) = hits.panes.iter().find(|(r, _)| r.contains(at)) {
                    return self.show_pane(*pane);
                }
                if let (Screen::Main, Some((area, first))) = (&self.screen, hits.list)
                    && area.contains(at)
                {
                    let index = first + (at.y - area.y) as usize;
                    if index < self.rows().len() {
                        let double = self.last_click.is_some_and(|(t, p)| {
                            now.duration_since(t) < DOUBLE_CLICK && p.y == at.y
                        });
                        self.select(index);
                        self.last_click = Some((now, at));
                        if double {
                            self.last_click = None;
                            return self.on_row_key(KeyEvent::from(KeyCode::Enter), now);
                        }
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

impl App {
    fn on_dialog_mouse(
        &mut self,
        ev: MouseEvent,
        at: Position,
        hits: &Hits,
        now: Instant,
    ) -> Vec<Effect> {
        if let Overlay::Tasks { selected, scroll } = self.overlay {
            let scroll = match ev.kind {
                MouseEventKind::ScrollUp => scroll + 3,
                MouseEventKind::ScrollDown => scroll.saturating_sub(3),
                _ => scroll,
            };
            self.overlay = Overlay::Tasks { selected, scroll };
        }
        if ev.kind != MouseEventKind::Down(MouseButton::Left) {
            return Vec::new();
        }
        if hits.dialog.is_some_and(|d| !d.contains(at)) {
            return self.on_key(KeyEvent::from(KeyCode::Esc), now);
        }
        let Some((area, first)) = hits.picks.filter(|(r, _)| r.contains(at)) else {
            return Vec::new();
        };
        let index = first + (at.y - area.y) as usize;
        let double = self
            .last_click
            .is_some_and(|(t, p)| now.duration_since(t) < DOUBLE_CLICK && p.y == at.y);
        self.last_click = Some((now, at));
        match self.overlay {
            Overlay::Tasks { .. } if index < self.tasks.len() => {
                self.overlay = Overlay::Tasks {
                    selected: index,
                    scroll: 0,
                };
            }
            Overlay::Servers { .. } if index < self.servers().len() => {
                self.overlay = Overlay::Servers { selected: index };
                if double {
                    self.last_click = None;
                    return self.on_key(KeyEvent::from(KeyCode::Enter), now);
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

/// What a prune's last line says it freed: `Total reclaimed space:
/// 3.404GB` from an image, container, volume or system prune, `Total:
/// 3.532GB` (tab-separated) from a builder prune.
fn freed(line: &str) -> Option<&str> {
    let line = line.trim();
    let size = line
        .strip_prefix("Total reclaimed space:")
        .or_else(|| line.strip_prefix("Total:"))?
        .trim();
    (!size.is_empty()).then_some(size)
}

/// Pretty-print a JSON document; anything else is shown as it came.
fn pretty(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::intent::{Desired, Status, TunnelState};
    use std::path::PathBuf;

    pub(crate) fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    pub(crate) fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    pub(crate) fn driver() -> Driver {
        Driver {
            cwd: PathBuf::from("/work/demo"),
            env: None,
            compose: vec![
                "docker".into(),
                "compose".into(),
                "-p".into(),
                "demo".into(),
            ],
            root: vec!["-p".into(), "demo".into()],
        }
    }

    pub(crate) fn container(name: &str, project: &str, service: &str, state: &str) -> Container {
        Container {
            id: format!("{name:0<12}aaaaaaaaaaaaaaaa"),
            name: name.into(),
            image: format!("{service}:latest"),
            state: state.into(),
            status: if state == "running" {
                "Up 3 minutes".into()
            } else {
                "Exited (0) 1 minute ago".into()
            },
            project: project.into(),
            service: service.into(),
            ..Container::default()
        }
    }

    pub(crate) fn app() -> App {
        let now = Instant::now();
        let here = Here {
            identity: "demo".into(),
            dest: "box".into(),
            local_dir: "/work/demo".into(),
            compose: "-f compose.yaml".into(),
            remote_dir: "~/.ulak/workspaces/n/abc/demo".into(),
            driver: driver(),
        };
        let mut app = App::new("box".into(), Some(here), Vec::new(), now);
        let snap = Snapshot {
            engine: true,
            containers: vec![
                container("demo-web-1", "demo", "web", "running"),
                container("demo-db-1", "demo", "db", "running"),
                container("other-api-1", "other", "api", "exited"),
                container("lonely", "", "", "running"),
            ],
            ..Snapshot::default()
        };
        app.on_msg(
            Msg::Snapshot {
                era: 1,
                result: Ok(snap),
                took: Duration::from_millis(250),
            },
            now,
        );
        app
    }

    fn names(app: &App) -> Vec<String> {
        app.rows()
            .iter()
            .map(|r| match r {
                Row::Group(g) => format!("[{}]", app.groups[*g].title()),
                Row::Empty(_) => "(empty)".into(),
                Row::Container(i) => app.containers[*i].name.clone(),
                _ => "?".into(),
            })
            .collect()
    }

    #[test]
    fn the_current_project_comes_first_and_standalone_last() {
        let app = app();
        assert_eq!(
            names(&app),
            vec![
                "[demo]",
                "demo-db-1",
                "demo-web-1",
                "[other]",
                "other-api-1",
                "[standalone]",
                "lonely"
            ]
        );
        assert!(app.groups[0].current && app.groups[0].driver.is_some());
        assert!(app.groups[1].driver.is_none(), "not declared here");
    }

    #[test]
    fn the_current_project_stays_on_screen_with_nothing_running() {
        let mut a = app();
        a.on_msg(
            Msg::Snapshot {
                era: 1,
                result: Ok(Snapshot {
                    engine: true,
                    ..Snapshot::default()
                }),
                took: Duration::ZERO,
            },
            Instant::now(),
        );
        assert_eq!(names(&a), vec!["[demo]", "(empty)"]);
        // `u` on the hint starts it, through Ulak's own compose.
        let fx = a.on_key(key('u'), Instant::now());
        let [Effect::Run { action, .. }] = fx.as_slice() else {
            panic!("{fx:?}");
        };
        let verbs::Plan::Ulak(driven) = &action.plan else {
            panic!("compose up must be Ulak itself");
        };
        assert_eq!(
            driven.args,
            vec!["docker", "compose", "-p", "demo", "up", "-d"]
        );
    }

    #[test]
    fn the_cursor_follows_the_container_when_the_list_reorders() {
        let mut a = app();
        a.on_key(code(KeyCode::Down), Instant::now());
        a.on_key(code(KeyCode::Down), Instant::now());
        let Some(Row::Container(i)) = a.selected() else {
            panic!("expected a container");
        };
        assert_eq!(a.containers[i].name, "demo-web-1");
        // A new container sorts in above it; the cursor must not move to
        // the neighbour a keypress would then act on.
        let mut snap = Snapshot {
            engine: true,
            containers: a.containers.clone(),
            ..Snapshot::default()
        };
        snap.containers
            .insert(0, container("demo-cache-1", "demo", "cache", "running"));
        a.on_msg(
            Msg::Snapshot {
                era: 1,
                result: Ok(snap),
                took: Duration::ZERO,
            },
            Instant::now(),
        );
        let Some(Row::Container(i)) = a.selected() else {
            panic!("expected a container");
        };
        assert_eq!(a.containers[i].name, "demo-web-1");
    }

    #[test]
    fn a_destructive_verb_asks_and_enter_on_a_fresh_dialog_is_no() {
        let mut a = app();
        a.on_key(code(KeyCode::Down), Instant::now());
        assert!(a.on_key(key('d'), Instant::now()).is_empty());
        assert!(matches!(a.overlay, Overlay::Confirm(_)));
        assert!(a.on_key(code(KeyCode::Enter), Instant::now()).is_empty());
        assert_eq!(a.overlay, Overlay::None);
        assert!(a.tasks.is_empty(), "No must run nothing");

        a.on_key(key('d'), Instant::now());
        let fx = a.on_key(key('y'), Instant::now());
        let [Effect::Run { action, .. }] = fx.as_slice() else {
            panic!("{fx:?}");
        };
        assert!(matches!(&action.plan, verbs::Plan::Daemon(argv) if argv[0] == "rm"));
        assert_eq!(a.running_tasks(), 1);
    }

    #[test]
    fn s_stops_a_running_container_and_starts_a_stopped_one() {
        let mut a = app();
        a.on_key(code(KeyCode::Down), Instant::now());
        let fx = a.on_key(key('s'), Instant::now());
        assert!(
            matches!(&fx[..], [Effect::Run { action, .. }] if action.title == "stop demo-db-1")
        );
        // other-api-1 is exited.
        a.select(4);
        let fx = a.on_key(key('s'), Instant::now());
        assert!(
            matches!(&fx[..], [Effect::Run { action, .. }] if action.title == "start other-api-1"),
            "{fx:?}"
        );
    }

    #[test]
    fn a_stack_nobody_declared_here_gets_container_verbs_not_compose() {
        let mut a = app();
        a.select(3); // [other]
        assert_eq!(a.selected(), Some(Row::Group(1)));
        assert!(a.on_key(key('D'), Instant::now()).is_empty());
        assert!(
            a.toasts
                .last()
                .unwrap()
                .text
                .contains("not started from this machine")
        );
        // `s` still works: plain `docker start` on its containers.
        let fx = a.on_key(key('s'), Instant::now());
        assert!(
            matches!(&fx[..], [Effect::Run { action, .. }] if matches!(&action.plan, verbs::Plan::Daemon(argv) if argv[0] == "start")),
            "{fx:?}"
        );
    }

    #[test]
    fn the_filter_narrows_live_and_esc_puts_it_back() {
        let mut a = app();
        a.on_key(key('/'), Instant::now());
        for c in "web".chars() {
            a.on_key(key(c), Instant::now());
        }
        assert_eq!(names(&a), vec!["[demo]", "demo-web-1"]);
        a.on_key(code(KeyCode::Esc), Instant::now());
        assert_eq!(a.filter, "");
        assert_eq!(names(&a).len(), 7);
    }

    #[test]
    fn collapsing_a_group_hides_its_rows_and_keeps_the_cursor_on_it() {
        let mut a = app();
        a.on_key(code(KeyCode::Enter), Instant::now());
        assert_eq!(
            names(&a)[..2],
            ["[demo]".to_string(), "[other]".to_string()]
        );
        assert_eq!(a.selected(), Some(Row::Group(0)));
    }

    #[test]
    fn enter_opens_logs_and_esc_closes_the_stream() {
        let mut a = app();
        a.on_key(code(KeyCode::Down), Instant::now());
        let fx = a.on_key(code(KeyCode::Enter), Instant::now());
        let [Effect::OpenLogs { stream, ids }] = fx.as_slice() else {
            panic!("{fx:?}");
        };
        let stream = *stream;
        assert_eq!(ids.len(), 1);
        a.on_msg(
            Msg::Lines {
                stream,
                source: 0,
                lines: vec!["2026-10-12T21:08:44.039917653Z ready".into()],
            },
            Instant::now(),
        );
        let Screen::Detail(d) = &a.screen else {
            panic!("expected the container page");
        };
        let logs = d.logs.as_ref().unwrap();
        assert_eq!(logs.lines[0].text, "ready");
        assert_eq!(
            logs.lines[0].stamp.as_deref(),
            Some("2026-10-12T21:08:44.039917653Z")
        );
        let fx = a.on_key(code(KeyCode::Esc), Instant::now());
        assert_eq!(fx, vec![Effect::CloseLogs { stream }]);
        assert!(matches!(a.screen, Screen::Main));
    }

    #[test]
    fn a_stack_view_merges_its_containers_lines_by_time() {
        let mut view = LogView::new(
            1,
            vec![
                Source {
                    name: "web".into(),
                    id: "a".into(),
                    ended: None,
                },
                Source {
                    name: "db".into(),
                    id: "b".into(),
                    ended: None,
                },
            ],
        );
        view.push(0, "2026-10-12T21:00:02.000000000Z web two");
        view.push(1, "2026-10-12T21:00:01.000000000Z db one");
        view.push(1, "2026-10-12T21:00:03.000000000Z db three");
        let texts: Vec<&str> = view.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["db one", "web two", "db three"]);
    }

    #[test]
    fn one_containers_stderr_backlog_lands_between_its_stdout_lines() {
        let mut view = LogView::new(
            1,
            vec![Source {
                name: "worker".into(),
                id: "a".into(),
                ended: None,
            }],
        );
        // The stdout reader drains its burst before the stderr reader runs.
        view.push(0, "2026-10-12T21:00:01.000000000Z tick 1");
        view.push(0, "2026-10-12T21:00:03.000000000Z tick 2");
        view.push(0, "2026-10-12T21:00:05.000000000Z tick 3");
        view.push(0, "2026-10-12T21:00:04.000000000Z ERROR job 2");
        let texts: Vec<&str> = view.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["tick 1", "tick 2", "ERROR job 2", "tick 3"]);
    }

    #[test]
    fn scrolling_up_stops_following_and_new_lines_keep_the_readers_place() {
        let mut view = LogView::new(
            1,
            vec![Source {
                name: "web".into(),
                id: "a".into(),
                ended: None,
            }],
        );
        for i in 0..10 {
            view.push(0, &format!("line {i}"));
        }
        view.scroll_by(3);
        assert!(!view.follow);
        view.push(0, "line 10");
        assert_eq!(view.scroll, 4, "the reader's line stays put");
        view.scroll_by(isize::MIN / 2);
        assert!(view.follow && view.scroll == 0);
    }

    #[test]
    fn the_log_cap_drops_the_oldest_and_counts_them() {
        let mut view = LogView::new(
            1,
            vec![Source {
                name: "web".into(),
                id: "a".into(),
                ended: None,
            }],
        );
        for i in 0..LOG_CAP + 5 {
            view.push(0, &format!("{i}"));
        }
        assert_eq!(view.lines.len(), LOG_CAP);
        assert_eq!(view.dropped, 5);
        assert_eq!(view.lines[0].text, "5");
    }

    #[test]
    fn search_jumps_to_older_matches_first() {
        let mut view = LogView::new(
            1,
            vec![Source {
                name: "web".into(),
                id: "a".into(),
                ended: None,
            }],
        );
        for text in ["boot", "ERROR one", "ok", "error two", "ok"] {
            view.push(0, text);
        }
        view.search = Some("error".into());
        assert!(view.jump(true));
        assert_eq!(view.scroll, 1, "error two is one above the bottom");
        assert!(view.jump(true));
        assert_eq!(view.scroll, 3);
        assert!(!view.jump(true), "nothing older");
        assert!(view.jump(false));
        assert_eq!(view.scroll, 1);
        // With a page drawn, the match lands mid-screen, and the next
        // jump starts from the match, not from the bottom.
        view.page.set(4);
        view.hit = None;
        view.scroll = 0;
        assert!(view.jump(true));
        assert_eq!(
            (view.hit, view.scroll),
            (Some(3), 0),
            "too near the bottom to centre"
        );
        assert!(view.jump(true));
        assert_eq!((view.hit, view.scroll), (Some(1), 1));
        // A line arriving keeps the mark on the same text.
        view.push(0, "later");
        assert_eq!(view.lines[view.hit.unwrap()].text, "ERROR one");
    }

    #[test]
    fn a_message_from_an_old_server_is_dropped() {
        let mut a = app();
        let fx = a.switch("elsewhere".into(), Instant::now());
        assert!(fx.contains(&Effect::Switch("elsewhere".into())));
        assert_eq!(a.era, 2);
        a.on_msg(
            Msg::Snapshot {
                era: 1,
                result: Ok(Snapshot {
                    engine: true,
                    containers: vec![container("ghost", "", "", "running")],
                    ..Snapshot::default()
                }),
                took: Duration::ZERO,
            },
            Instant::now(),
        );
        assert!(a.containers.is_empty(), "era 1 is gone");
        assert!(!a.here_is_this_server());
    }

    #[test]
    fn a_failed_task_says_so_with_its_last_line_and_refreshes() {
        let mut a = app();
        a.on_key(code(KeyCode::Down), Instant::now());
        let fx = a.on_key(key('r'), Instant::now());
        let [Effect::Run { task, .. }] = fx.as_slice() else {
            panic!("{fx:?}");
        };
        let task = *task;
        a.on_msg(
            Msg::TaskLine {
                task,
                line: "Error response from daemon: boom".into(),
            },
            Instant::now(),
        );
        let fx = a.on_msg(
            Msg::TaskDone {
                task,
                code: Some(1),
            },
            Instant::now(),
        );
        assert!(matches!(fx.as_slice(), [Effect::Poke(_)]));
        let toast = a.toasts.last().unwrap();
        assert_eq!(toast.tone, Tone::Bad);
        assert!(toast.text.contains("boom"), "{}", toast.text);
        assert!(a.tasks[0].failed());
    }

    /// A restart of the second row, started and still running.
    fn running_task(a: &mut App) -> u64 {
        a.on_key(code(KeyCode::Down), Instant::now());
        let fx = a.on_key(key('r'), Instant::now());
        let [Effect::Run { task, .. }] = fx.as_slice() else {
            panic!("{fx:?}");
        };
        *task
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    #[test]
    fn quitting_with_a_running_task_asks_then_stops_it_and_waits_on_screen() {
        let mut a = app();
        let task = running_task(&mut a);
        let now = Instant::now();
        a.on_key(key('q'), now);
        assert!(!a.quit);
        assert!(matches!(&a.overlay, Overlay::Confirm(c) if c.then == Then::Quit));
        let fx = a.on_key(key('y'), now);
        assert_eq!(fx, vec![Effect::Cancel { task }]);
        assert!(!a.quit, "it waits for the task, on screen");
        assert!(
            matches!(a.overlay, Overlay::Leaving { .. }),
            "{:?}",
            a.overlay
        );
        let fx = a.on_msg(
            Msg::TaskDone {
                task,
                code: Some(130),
            },
            now,
        );
        assert!(a.quit, "the last task to stop ends the wait");
        assert!(fx.is_empty(), "nothing is refreshed on the way out: {fx:?}");
    }

    #[test]
    fn while_quitting_every_key_is_read_and_only_q_or_ctrl_c_ends_the_wait() {
        let mut a = app();
        running_task(&mut a);
        let now = Instant::now();
        let fx = a.on_key(ctrl_c(), now);
        assert!(matches!(fx.as_slice(), [Effect::Cancel { .. }]), "{fx:?}");
        let waiting = a.overlay.clone();
        assert!(matches!(waiting, Overlay::Leaving { .. }), "{waiting:?}");
        for k in [
            key('j'),
            key('d'),
            key('y'),
            key('e'),
            key(':'),
            code(KeyCode::Enter),
            code(KeyCode::Esc),
        ] {
            assert_eq!(a.on_key(k, now), Vec::new(), "{k:?}");
            assert_eq!(a.overlay, waiting, "{k:?}");
            assert!(!a.quit, "{k:?}");
        }
        // A click outside the dialog is esc, which does not end it either.
        let hits = Hits {
            dialog: Some(Rect::new(20, 8, 40, 7)),
            ..Hits::default()
        };
        assert_eq!(a.on_mouse(left_click(2, 2), &hits, now), Vec::new());
        assert!(!a.quit);
        a.on_key(key('q'), now);
        assert!(a.quit);

        let mut b = app();
        running_task(&mut b);
        b.on_key(ctrl_c(), now);
        assert!(!b.quit);
        b.on_key(ctrl_c(), now);
        assert!(b.quit, "a second ctrl-c does not wait either");
    }

    #[test]
    fn quitting_stops_waiting_when_the_grace_is_up() {
        let mut a = app();
        running_task(&mut a);
        let start = Instant::now();
        a.on_key(ctrl_c(), start);
        a.on_tick(start + LEAVE_GRACE - Duration::from_millis(100), Vec::new());
        assert!(!a.quit);
        a.on_tick(start + LEAVE_GRACE, Vec::new());
        assert!(a.quit);
    }

    #[test]
    fn quitting_after_tasks_have_finished_quits_at_once() {
        for quit in [key('q'), ctrl_c()] {
            let mut a = app();
            let task = running_task(&mut a);
            a.on_msg(
                Msg::TaskDone {
                    task,
                    code: Some(0),
                },
                Instant::now(),
            );
            let fx = a.on_key(quit, Instant::now());
            assert_eq!(fx, Vec::new(), "nothing is left to stop");
            assert!(a.quit, "{quit:?}");
        }
    }

    #[test]
    fn the_quitting_dialogs_button_leaves_now() {
        let mut a = app();
        running_task(&mut a);
        a.on_key(ctrl_c(), Instant::now());
        let hits = Hits {
            keys: vec![(Rect::new(30, 12, 30, 1), "q".into())],
            dialog: Some(Rect::new(20, 8, 40, 7)),
            ..Hits::default()
        };
        a.on_mouse(left_click(35, 12), &hits, Instant::now());
        assert!(a.quit);
    }

    fn declare(a: &mut App, identity: &str, live: bool, ports: &[(u32, &str, bool)]) {
        let desired = Desired {
            schema: crate::intent::SCHEMA,
            live,
            workspace_id: "abc".into(),
            workspace_namespace: "n".into(),
            destination: a.dest.clone(),
            identity: identity.into(),
            cwd: "/work/demo".into(),
            argv_globals: Vec::new(),
            compose_env: BTreeMap::new(),
            updated_unix: 1,
        };
        let id = crate::intent::stack_id(&a.dest, identity);
        let status = Status {
            schema: crate::intent::SCHEMA,
            connection: "up".into(),
            tunnels: ports
                .iter()
                .map(|(port, service, open)| TunnelState {
                    port: *port,
                    service: (*service).into(),
                    open: *open,
                    service_running: Some(true),
                })
                .collect(),
            ..Status::default()
        };
        let mut drivers = BTreeMap::new();
        drivers.insert(id.clone(), driver());
        a.on_msg(
            Msg::Local(Box::new(Local {
                catalog: vec![(id.clone(), desired)],
                statuses: vec![(id, status)],
                drivers,
                ..Local::default()
            })),
            Instant::now(),
        );
    }

    #[test]
    fn localhost_ports_come_from_a_live_stacks_tunnels_only() {
        let mut a = app();
        declare(
            &mut a,
            "demo",
            true,
            &[(8080, "web", true), (5432, "db", false)],
        );
        assert_eq!(a.ports_home("demo", "web"), vec![8080]);
        assert_eq!(
            a.ports_home("demo", "db"),
            Vec::<u32>::new(),
            "a port not tunneled is not home"
        );
        assert_eq!(a.ports_home("demo", ""), vec![8080]);
        declare(&mut a, "demo", false, &[(8080, "web", true)]);
        assert!(
            a.ports_home("demo", "web").is_empty(),
            "history is not a port"
        );
        // `o` on the web row opens it while live.
        declare(&mut a, "demo", true, &[(8080, "web", true)]);
        a.select(2);
        let fx = a.on_key(key('o'), Instant::now());
        assert_eq!(fx, vec![Effect::Open("http://localhost:8080".into())]);
    }

    #[test]
    fn a_stack_declared_on_this_server_gets_compose_verbs_even_when_down() {
        let mut a = app();
        declare(&mut a, "idle", false, &[]);
        let g = a.groups.iter().position(|g| g.key == "idle").unwrap();
        assert!(a.groups[g].driver.is_some());
        let at = a.rows().iter().position(|r| *r == Row::Group(g)).unwrap();
        a.select(at);
        let fx = a.on_key(key('u'), Instant::now());
        assert!(
            matches!(&fx[..], [Effect::Run { action, .. }] if action.title == "compose up -d (idle)"),
            "{fx:?}"
        );
    }

    #[test]
    fn switching_to_a_tab_asks_for_what_it_shows() {
        let mut a = app();
        let fx = a.on_key(key('2'), Instant::now());
        assert_eq!(a.tab, Tab::Images);
        assert!(fx.contains(&Effect::Interest(Tab::Images.interest())));
        assert!(fx.iter().any(|f| matches!(f, Effect::Poke(w) if w.images)));
    }

    #[test]
    fn healthcheck_chatter_does_not_poke_but_a_crash_toasts() {
        let mut a = app();
        let e = |action: &str, exit: Option<i32>| Msg::Event {
            era: 1,
            event: engine::Event {
                kind: "container".into(),
                action: action.into(),
                name: "demo-web-1".into(),
                exit_code: exit,
                ..engine::Event::default()
            },
        };
        assert!(
            a.on_msg(e("exec_start: sh -c curl", None), Instant::now())
                .is_empty()
        );
        let fx = a.on_msg(e("die", Some(1)), Instant::now());
        assert!(matches!(fx.as_slice(), [Effect::Poke(_)]));
        assert_eq!(a.toasts.last().unwrap().tone, Tone::Bad);
    }

    #[test]
    fn a_dead_events_feed_is_restarted_with_backoff_and_the_list_polled_meanwhile() {
        let mut a = app();
        let t0 = Instant::now();
        a.events_up = true;
        a.on_msg(
            Msg::FeedEnded {
                era: 1,
                feed: Feed::Events,
                why: "ended".into(),
            },
            t0,
        );
        assert!(!a.events_up);
        let fx = a.on_tick(t0 + Duration::from_secs(1), Vec::new());
        assert!(!fx.contains(&Effect::RestartFeed(Feed::Events)));
        let fx = a.on_tick(t0 + Duration::from_secs(6), Vec::new());
        assert!(fx.contains(&Effect::RestartFeed(Feed::Events)), "{fx:?}");
        assert!(
            fx.iter().any(|f| matches!(f, Effect::Poke(_))),
            "blind poll: {fx:?}"
        );
    }

    #[test]
    fn held_warnings_become_toasts() {
        let mut a = app();
        a.on_tick(
            Instant::now(),
            vec![(crate::ui::Level::Warn, "box stopped answering".into())],
        );
        assert_eq!(a.toasts.last().unwrap().text, "box stopped answering");
    }

    #[test]
    fn a_double_click_opens_the_row() {
        let mut a = app();
        let hits = Hits {
            list: Some((Rect::new(0, 5, 80, 20), 0)),
            ..Hits::default()
        };
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 6,
            modifiers: KeyModifiers::NONE,
        };
        let t0 = Instant::now();
        assert!(a.on_mouse(click, &hits, t0).is_empty());
        assert_eq!(a.sel[0].index, 1);
        let fx = a.on_mouse(click, &hits, t0 + Duration::from_millis(100));
        assert!(matches!(fx.as_slice(), [Effect::OpenLogs { .. }]), "{fx:?}");
    }

    fn left_click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn hints_that_name_one_key_are_buttons_and_families_are_not() {
        assert_eq!(key_named("⏎"), Some(KeyEvent::from(KeyCode::Enter)));
        assert_eq!(key_named("esc"), Some(KeyEvent::from(KeyCode::Esc)));
        assert_eq!(key_named("tab"), Some(KeyEvent::from(KeyCode::Tab)));
        assert_eq!(key_named("D"), Some(KeyEvent::from(KeyCode::Char('D'))));
        assert_eq!(key_named("n/N"), Some(KeyEvent::from(KeyCode::Char('n'))));
        for family in ["↑↓", "←→", "PgUp/PgDn", "any other key", ""] {
            assert_eq!(key_named(family), None, "{family}");
        }
    }

    #[test]
    fn clicking_a_hint_presses_its_key() {
        let mut a = app();
        let hits = Hits {
            keys: vec![(Rect::new(10, 23, 8, 1), "?".into())],
            ..Hits::default()
        };
        a.on_mouse(left_click(12, 23), &hits, Instant::now());
        assert!(matches!(a.overlay, Overlay::Help { .. }), "{:?}", a.overlay);
    }

    #[test]
    fn a_click_outside_a_confirm_dialog_answers_no() {
        let mut a = app();
        a.select(1);
        a.on_key(KeyEvent::from(KeyCode::Char('d')), Instant::now());
        assert!(matches!(a.overlay, Overlay::Confirm(_)), "{:?}", a.overlay);
        let hits = Hits {
            dialog: Some(Rect::new(20, 8, 40, 7)),
            ..Hits::default()
        };
        let inside = a.on_mouse(left_click(30, 10), &hits, Instant::now());
        assert!(inside.is_empty());
        assert!(matches!(a.overlay, Overlay::Confirm(_)));
        let outside = a.on_mouse(left_click(2, 2), &hits, Instant::now());
        assert!(outside.is_empty(), "{outside:?}");
        assert_eq!(a.overlay, Overlay::None);
    }

    #[test]
    fn a_confirm_button_click_is_its_answer() {
        let mut a = app();
        a.select(1);
        a.on_key(KeyEvent::from(KeyCode::Char('d')), Instant::now());
        let hits = Hits {
            keys: vec![
                (Rect::new(30, 12, 4, 1), "n".into()),
                (Rect::new(37, 12, 5, 1), "y".into()),
            ],
            dialog: Some(Rect::new(20, 8, 40, 7)),
            ..Hits::default()
        };
        let fx = a.on_mouse(left_click(38, 12), &hits, Instant::now());
        assert!(matches!(fx.as_slice(), [Effect::Run { .. }]), "{fx:?}");
        assert_eq!(a.overlay, Overlay::None);
    }

    #[test]
    fn prune_is_the_tabs_key_and_works_with_nothing_selected() {
        let mut a = app();
        a.on_key(key('2'), Instant::now());
        a.filter = "matches nothing".into();
        a.fix_selection();
        assert!(a.selected().is_none());
        a.on_key(key('P'), Instant::now());
        let Overlay::Confirm(c) = &a.overlay else {
            panic!("{:?}", a.overlay);
        };
        let Then::Run(action) = &c.then else {
            panic!("{:?}", c.then);
        };
        assert_eq!(action.title, "prune unused images");
        for (tab, title) in [
            ('1', "prune stopped containers"),
            ('3', "prune unused anonymous volumes"),
            ('4', "prune unused networks"),
        ] {
            let mut a = app();
            a.on_key(key(tab), Instant::now());
            a.on_key(key('P'), Instant::now());
            assert!(
                matches!(&a.overlay, Overlay::Confirm(Confirm { then: Then::Run(x), .. }) if x.title == title),
                "{tab}: {:?}",
                a.overlay
            );
        }
        let mut a = app();
        a.on_key(key('5'), Instant::now());
        a.on_key(key('P'), Instant::now());
        assert_eq!(
            a.overlay,
            Overlay::None,
            "the Ulak tab has nothing to prune"
        );
    }

    #[test]
    fn b_on_the_images_tab_asks_to_prune_the_build_cache_with_dockers_numbers() {
        let mut a = app();
        a.usage = vec![Usage {
            kind: "Build Cache".into(),
            total: "264".into(),
            active: "11".into(),
            size: "9.975GB".into(),
            reclaimable: "3.532GB".into(),
        }];
        a.on_key(key('2'), Instant::now());
        a.on_key(key('B'), Instant::now());
        let Overlay::Confirm(c) = &a.overlay else {
            panic!("{:?}", a.overlay);
        };
        assert!(
            c.question.contains("(3.532GB of 9.975GB)"),
            "{}",
            c.question
        );
        let mut a = app();
        a.on_key(key('B'), Instant::now());
        assert_eq!(a.overlay, Overlay::None, "only the Images tab has it");
    }

    #[test]
    fn declining_a_question_says_nothing_was_done() {
        for no in [key('n'), code(KeyCode::Esc), code(KeyCode::Enter)] {
            let mut a = app();
            a.on_key(key('2'), Instant::now());
            a.on_key(key('P'), Instant::now());
            let fx = a.on_key(no, Instant::now());
            assert!(fx.is_empty(), "{no:?}: {fx:?}");
            assert_eq!(a.overlay, Overlay::None);
            let toast = a.toasts.last().expect("a word about it");
            assert_eq!(toast.text, "prune unused images: cancelled", "{no:?}");
        }
    }

    #[test]
    fn a_prune_that_worked_says_what_docker_says_it_freed() {
        assert_eq!(freed("Total reclaimed space: 3.404GB"), Some("3.404GB"));
        assert_eq!(freed("Total:\t3.532GB"), Some("3.532GB"));
        assert_eq!(freed("Total reclaimed space: 0B"), Some("0B"));
        assert_eq!(freed("demo_default"), None);
        assert_eq!(freed("Total:"), None);
        let mut a = app();
        a.on_key(key('2'), Instant::now());
        a.on_key(key('P'), Instant::now());
        let fx = a.on_key(key('y'), Instant::now());
        let [Effect::Run { task, .. }] = fx.as_slice() else {
            panic!("{fx:?}");
        };
        let task = *task;
        for line in [
            "Deleted Images:",
            "deleted: sha256:4f1c…",
            "",
            "Total reclaimed space: 3.404GB",
        ] {
            a.on_msg(
                Msg::TaskLine {
                    task,
                    line: line.into(),
                },
                Instant::now(),
            );
        }
        a.on_msg(
            Msg::TaskDone {
                task,
                code: Some(0),
            },
            Instant::now(),
        );
        let toast = a.toasts.last().unwrap();
        assert_eq!(toast.tone, Tone::Good);
        assert_eq!(toast.text, "prune unused images · 3.404GB freed");
    }

    #[test]
    fn inspect_text_is_pretty_printed() {
        assert_eq!(pretty("[{\"a\":1}]"), "[\n  {\n    \"a\": 1\n  }\n]");
        assert_eq!(pretty("not json"), "not json");
    }
}
