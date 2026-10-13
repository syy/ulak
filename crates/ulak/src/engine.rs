//! What the server's Docker Engine holds, read for the dashboard in ONE
//! bounded round trip.
//!
//! `ulak ui` is a window onto a daemon somewhere else, so every refresh
//! costs a connection's latency. One script answers every question the
//! current screen asks; `Want` leaves out the sections nobody is looking
//! at, because `docker system df` and `docker info` are the slow ones.
//! Each section opens with a marker line, as in `commands::status`.
//!
//! Formats are explicit `{{json …}}` fields, never the display table and
//! never `.Labels`: that one is a comma-joined string whose values may
//! hold commas themselves (see `compose::label_format`), so each Compose
//! label is asked for by name. `.HealthStatus` is deliberately absent —
//! Docker 29 has it and older engines fail the whole template on it —
//! and health is read off `.Status`, which every engine prints.
//!
//! A section folds stderr into stdout. Every line that is not a JSON
//! object is therefore the engine's own complaint and lands in
//! `Snapshot::trouble` verbatim, so "permission denied … docker.sock"
//! reaches the screen as Docker said it.
//!
//! The live feeds have parsers here too: `docker stats` lines (after
//! `ansi::strip`), `docker events` lines, and the timestamp `docker logs
//! --timestamps` puts in front of every line.

use serde::Deserialize;

use crate::compose::{ONEOFF_LABEL, PROJECT_LABEL, SERVICE_LABEL, WORKING_DIR_LABEL};
use crate::ssh::sh_quote;

const MARK: &str = "==ULAK:";
const NO_ENGINE: &str = "ULAK_NO_ENGINE";

/// Which optional sections one round asks for. Containers always come.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Want {
    pub images: bool,
    pub volumes: bool,
    pub networks: bool,
    pub info: bool,
    pub usage: bool,
}

impl Want {
    pub fn all() -> Want {
        Want {
            images: true,
            volumes: true,
            networks: true,
            info: true,
            usage: true,
        }
    }

    pub fn union(self, other: Want) -> Want {
        Want {
            images: self.images || other.images,
            volumes: self.volumes || other.volumes,
            networks: self.networks || other.networks,
            info: self.info || other.info,
            usage: self.usage || other.usage,
        }
    }
}

/// One answer. `None` sections were not asked for this round, which is
/// different from an empty list: the screen keeps what it had.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub containers: Vec<Container>,
    pub images: Option<Vec<Image>>,
    pub volumes: Option<Vec<Volume>>,
    pub networks: Option<Vec<Network>>,
    pub info: Option<Info>,
    pub disk: Option<Disk>,
    pub usage: Option<Vec<Usage>>,
    /// False when `docker` itself is missing or would not run at all.
    pub engine: bool,
    /// What the engine said instead of an answer, line by line.
    pub trouble: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub command: String,
    pub state: String,
    pub status: String,
    pub ports: String,
    pub created: String,
    pub networks: String,
    pub mounts: String,
    pub project: String,
    pub service: String,
    pub number: String,
    pub oneoff: String,
    pub workdir: String,
}

/// How a row should read at a glance: the colour of its dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Busy,
    Bad,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Unhealthy,
    Starting,
}

impl Container {
    pub fn short_id(&self) -> &str {
        short(&self.id)
    }

    pub fn running(&self) -> bool {
        self.state == "running"
    }

    pub fn paused(&self) -> bool {
        self.state == "paused"
    }

    /// Read off `.Status`, which every engine prints: `Up 3 minutes
    /// (healthy)`, `(unhealthy)`, `(health: starting)`.
    pub fn health(&self) -> Option<Health> {
        if self.status.contains("(healthy)") {
            Some(Health::Healthy)
        } else if self.status.contains("(unhealthy)") {
            Some(Health::Unhealthy)
        } else if self.status.contains("(health: starting)") {
            Some(Health::Starting)
        } else {
            None
        }
    }

    /// `Exited (137) 2 minutes ago` → 137.
    pub fn exit_code(&self) -> Option<i32> {
        let rest = self.status.strip_prefix("Exited (")?;
        rest.split(')').next()?.parse().ok()
    }

    pub fn tone(&self) -> Tone {
        match self.state.as_str() {
            "running" => match self.health() {
                Some(Health::Unhealthy) => Tone::Bad,
                Some(Health::Starting) => Tone::Busy,
                _ => Tone::Good,
            },
            "paused" | "restarting" | "created" | "removing" => Tone::Busy,
            "dead" => Tone::Bad,
            "exited" => match self.exit_code() {
                Some(0) | None => Tone::Idle,
                Some(_) => Tone::Bad,
            },
            _ => Tone::Idle,
        }
    }

    /// Whether it is attached to network `name`. `.Networks` is Docker's
    /// display join of the names with ",", and a name may itself hold one
    /// (`docker network create 'a,b'` succeeds on 29.6.2), so the string
    /// cannot be split; the name is matched as a whole delimited run.
    pub fn on_network(&self, name: &str) -> bool {
        !name.is_empty() && format!(",{},", self.networks).contains(&format!(",{name},"))
    }

    /// A Compose `run` container, which `down` and the service ignore.
    pub fn oneoff(&self) -> bool {
        self.oneoff.eq_ignore_ascii_case("true")
    }

    /// Whether this container was created from `image`, by the name it
    /// was given (`nginx` means `nginx:latest`) or by id.
    pub fn uses(&self, image: &Image) -> bool {
        let wanted = self.image.as_str();
        if image.id.is_empty() {
            return false;
        }
        let id = image.id.trim_start_matches("sha256:");
        let given = wanted.trim_start_matches("sha256:");
        if given.len() >= 12 && id.starts_with(given) {
            return true;
        }
        if image.repository == "<none>" {
            return false;
        }
        let tag = if image.tag.is_empty() || image.tag == "<none>" {
            "latest"
        } else {
            image.tag.as_str()
        };
        wanted == format!("{}:{tag}", image.repository)
            || (tag == "latest" && wanted == image.repository)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Image {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Repository")]
    pub repository: String,
    #[serde(rename = "Tag")]
    pub tag: String,
    #[serde(rename = "CreatedSince")]
    pub created_since: String,
    #[serde(rename = "CreatedAt")]
    pub created_at: String,
    #[serde(rename = "Size")]
    pub size: String,
}

impl Image {
    /// `repo:tag`, or the short id for an image nothing names.
    pub fn reference(&self) -> String {
        if self.repository.is_empty() || self.repository == "<none>" {
            return self.short_id().to_string();
        }
        if self.tag.is_empty() || self.tag == "<none>" {
            return self.repository.clone();
        }
        format!("{}:{}", self.repository, self.tag)
    }

    pub fn short_id(&self) -> &str {
        short(self.id.trim_start_matches("sha256:"))
    }

    pub fn dangling(&self) -> bool {
        self.repository == "<none>" && self.tag == "<none>"
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Volume {
    pub name: String,
    pub driver: String,
    pub scope: String,
    pub mountpoint: String,
    pub project: String,
    /// Filled from the `dangling=true` section, not from Docker's JSON.
    #[serde(skip)]
    pub unused: bool,
}

impl Volume {
    /// Anonymous volumes are named by a 64-hex id. Their label carries an
    /// empty value, which `.Label` cannot tell from an absent one.
    pub fn anonymous(&self) -> bool {
        self.name.len() == 64 && self.name.chars().all(|c| c.is_ascii_hexdigit())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Network {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
    pub internal: String,
    pub created: String,
    pub project: String,
}

impl Network {
    pub fn short_id(&self) -> &str {
        short(&self.id)
    }

    /// Docker's own three, which cannot be removed.
    pub fn builtin(&self) -> bool {
        matches!(self.name.as_str(), "bridge" | "host" | "none")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Info {
    pub version: String,
    pub os: String,
    pub arch: String,
    pub name: String,
    pub ncpu: u64,
    pub mem: u64,
    pub running: u64,
    pub containers: u64,
}

/// The filesystem Docker keeps its data on, in KiB.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Disk {
    pub total_kb: u64,
    pub used_kb: u64,
    pub free_kb: u64,
}

/// One row of `docker system df`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Usage {
    #[serde(rename = "Type")]
    pub kind: String,
    #[serde(rename = "TotalCount")]
    pub total: String,
    #[serde(rename = "Active")]
    pub active: String,
    #[serde(rename = "Size")]
    pub size: String,
    #[serde(rename = "Reclaimable")]
    pub reclaimable: String,
}

/// A sample from the `docker stats` stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stat {
    /// Docker's 12-character id; match with `Container::id.starts_with`.
    pub id: String,
    pub name: String,
    pub cpu: f64,
    pub mem: f64,
    /// Memory in use, in bytes: the left half of `mem_usage`.
    pub mem_bytes: f64,
    pub mem_usage: String,
    pub net_io: String,
    pub block_io: String,
    pub pids: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawStat {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "CPUPerc")]
    cpu: String,
    #[serde(rename = "MemPerc")]
    mem: String,
    #[serde(rename = "MemUsage")]
    mem_usage: String,
    #[serde(rename = "NetIO")]
    net_io: String,
    #[serde(rename = "BlockIO")]
    block_io: String,
    #[serde(rename = "PIDs")]
    pids: String,
}

/// One `docker events` record, reduced to what the dashboard acts on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Event {
    pub kind: String,
    pub action: String,
    pub id: String,
    pub name: String,
    pub project: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawEvent {
    #[serde(rename = "Type")]
    kind: String,
    #[serde(rename = "Action")]
    action: String,
    #[serde(rename = "Actor")]
    actor: RawActor,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawActor {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Attributes")]
    attributes: std::collections::BTreeMap<String, String>,
}

/// What an event says about the world, for the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Bad(String),
    Note(String),
}

impl Event {
    /// The sections an event makes stale, or `None` for the chatter.
    ///
    /// A healthcheck is an `exec_create`/`exec_start`/`exec_die` triple
    /// every interval per container, and `attach`, `top`, `mount` and
    /// network `connect` say nothing a row shows. Refreshing on those
    /// would turn the event feed into a polling loop with extra steps.
    pub fn stale(&self) -> Option<Want> {
        let action = self.action.split(':').next().unwrap_or_default();
        match self.kind.as_str() {
            "container" => matches!(
                action,
                "create"
                    | "start"
                    | "die"
                    | "destroy"
                    | "pause"
                    | "unpause"
                    | "rename"
                    | "restart"
                    | "stop"
                    | "kill"
                    | "oom"
                    | "health_status"
                    | "update"
            )
            .then(|| Want {
                // `images` counts in-use; a destroyed container frees one.
                images: matches!(action, "create" | "destroy"),
                volumes: matches!(action, "create" | "destroy"),
                ..Want::default()
            }),
            "image" => matches!(
                action,
                "pull" | "delete" | "tag" | "untag" | "load" | "import" | "save" | "prune"
            )
            .then(|| Want {
                images: true,
                usage: true,
                ..Want::default()
            }),
            "volume" => matches!(action, "create" | "destroy" | "prune").then(|| Want {
                volumes: true,
                usage: true,
                ..Want::default()
            }),
            "network" => {
                matches!(action, "create" | "destroy" | "remove" | "prune").then(|| Want {
                    networks: true,
                    ..Want::default()
                })
            }
            _ => None,
        }
    }

    /// The events worth a toast: a container that stopped on its own
    /// terms, ran out of memory, or failed its healthcheck.
    pub fn notice(&self) -> Option<Notice> {
        if self.kind != "container" {
            return None;
        }
        let who = if self.name.is_empty() {
            short(&self.id).to_string()
        } else {
            self.name.clone()
        };
        match self.action.as_str() {
            "die" => match self.exit_code {
                Some(0) | None => None,
                // 137/143 after a `stop` are the stop itself, and the stop
                // already said so; anything else is the container's own.
                Some(137) | Some(143) => None,
                Some(code) => Some(Notice::Bad(format!("{who} exited with code {code}"))),
            },
            "oom" => Some(Notice::Bad(format!("{who} ran out of memory"))),
            "health_status: unhealthy" => Some(Notice::Bad(format!("{who} is unhealthy"))),
            "health_status: healthy" => Some(Notice::Note(format!("{who} is healthy"))),
            _ => None,
        }
    }
}

// ─── the round trip ─────────────────────────────────────────────────

/// One JSON object per row: `(key, template field)` pairs, then
/// `(key, label)` pairs read with `.Label`, which Docker JSON-escapes.
fn object_format(fields: &[(&str, &str)], labels: &[(&str, &str)]) -> String {
    let mut parts: Vec<String> = fields
        .iter()
        .map(|(key, field)| format!("\"{key}\":{{{{json {field}}}}}"))
        .collect();
    parts.extend(
        labels
            .iter()
            .map(|(key, label)| format!("\"{key}\":{{{{json (.Label \"{label}\")}}}}")),
    );
    format!("{{{}}}", parts.join(","))
}

fn ps_format() -> String {
    object_format(
        &[
            ("id", ".ID"),
            ("name", ".Names"),
            ("image", ".Image"),
            ("command", ".Command"),
            ("state", ".State"),
            ("status", ".Status"),
            ("ports", ".Ports"),
            ("created", ".CreatedAt"),
            ("networks", ".Networks"),
            ("mounts", ".Mounts"),
        ],
        &[
            ("project", PROJECT_LABEL),
            ("service", SERVICE_LABEL),
            ("number", "com.docker.compose.container-number"),
            ("oneoff", ONEOFF_LABEL),
            ("workdir", WORKING_DIR_LABEL),
        ],
    )
}

fn volume_format() -> String {
    object_format(
        &[
            ("name", ".Name"),
            ("driver", ".Driver"),
            ("scope", ".Scope"),
            ("mountpoint", ".Mountpoint"),
        ],
        &[("project", PROJECT_LABEL)],
    )
}

fn network_format() -> String {
    object_format(
        &[
            ("id", ".ID"),
            ("name", ".Name"),
            ("driver", ".Driver"),
            ("scope", ".Scope"),
            ("internal", ".Internal"),
            ("created", ".CreatedAt"),
        ],
        &[("project", PROJECT_LABEL)],
    )
}

fn info_format() -> String {
    "{\"version\":{{json .ServerVersion}},\"os\":{{json .OperatingSystem}},\
     \"arch\":{{json .Architecture}},\"name\":{{json .Name}},\"ncpu\":{{json .NCPU}},\
     \"mem\":{{json .MemTotal}},\"running\":{{json .ContainersRunning}},\
     \"containers\":{{json .Containers}}}"
        .to_string()
}

/// The script for one round. Read-only by construction: every line is a
/// listing, and the audit trail records it whole (`Ssh::probe`).
pub fn script(want: Want) -> String {
    let json = sh_quote("{{json .}}");
    let mut s = format!(
        "echo '{MARK}ps=='\ndocker ps -a --no-trunc --format {} 2>&1 || echo {NO_ENGINE}\n",
        sh_quote(&ps_format())
    );
    if want.images {
        s.push_str(&format!(
            "echo '{MARK}images=='\ndocker images --format {json} 2>&1\n"
        ));
    }
    if want.volumes {
        s.push_str(&format!(
            "echo '{MARK}volumes=='\ndocker volume ls --format {} 2>&1\n\
             echo '{MARK}dangling=='\ndocker volume ls -q --filter dangling=true 2>&1\n",
            sh_quote(&volume_format())
        ));
    }
    if want.networks {
        s.push_str(&format!(
            "echo '{MARK}networks=='\ndocker network ls --no-trunc --format {} 2>&1\n",
            sh_quote(&network_format())
        ));
    }
    if want.info {
        s.push_str(&format!(
            "echo '{MARK}info=='\ndocker info --format {} 2>&1\n\
             echo '{MARK}disk=='\n\
             r=$(docker info --format '{{{{.DockerRootDir}}}}' 2>/dev/null)\n\
             df -Pk \"${{r:-$HOME}}\" 2>/dev/null | tail -n 1\n",
            sh_quote(&info_format())
        ));
    }
    if want.usage {
        s.push_str(&format!(
            "echo '{MARK}usage=='\ndocker system df --format {json} 2>&1\n"
        ));
    }
    s
}

/// Read one round's output back into a snapshot.
pub fn parse(text: &str) -> Snapshot {
    let mut snap = Snapshot {
        engine: true,
        ..Snapshot::default()
    };
    let mut dangling: Vec<String> = Vec::new();
    let mut section = "";
    for raw in text.lines() {
        if let Some(name) = raw.strip_prefix(MARK).and_then(|r| r.strip_suffix("==")) {
            section = match name {
                "ps" => "ps",
                "images" => {
                    snap.images = Some(Vec::new());
                    "images"
                }
                "volumes" => {
                    snap.volumes = Some(Vec::new());
                    "volumes"
                }
                "dangling" => "dangling",
                "networks" => {
                    snap.networks = Some(Vec::new());
                    "networks"
                }
                "info" => "info",
                "disk" => "disk",
                "usage" => {
                    snap.usage = Some(Vec::new());
                    "usage"
                }
                _ => "",
            };
            continue;
        }
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line == NO_ENGINE {
            snap.engine = false;
            continue;
        }
        match section {
            "dangling" => {
                if is_object(line) {
                    continue;
                }
                // `volume ls -q` prints bare names; an error has spaces.
                if line.contains(' ') {
                    snap.trouble.push(line.to_string());
                } else {
                    dangling.push(line.to_string());
                }
            }
            "disk" => snap.disk = parse_df(line),
            _ if !is_object(line) => snap.trouble.push(line.to_string()),
            "ps" => push(&mut snap.containers, line, &mut snap.trouble),
            "images" => push_opt(&mut snap.images, line, &mut snap.trouble),
            "volumes" => push_opt(&mut snap.volumes, line, &mut snap.trouble),
            "networks" => push_opt(&mut snap.networks, line, &mut snap.trouble),
            "usage" => push_opt(&mut snap.usage, line, &mut snap.trouble),
            "info" => match serde_json::from_str::<Info>(line) {
                Ok(info) => snap.info = Some(info),
                Err(_) => snap.trouble.push(line.to_string()),
            },
            _ => {}
        }
    }
    if let Some(volumes) = &mut snap.volumes {
        for v in volumes.iter_mut() {
            v.unused = dangling.contains(&v.name);
        }
    }
    snap.trouble.dedup();
    snap
}

fn is_object(line: &str) -> bool {
    line.starts_with('{') && line.ends_with('}')
}

fn push<T: for<'de> Deserialize<'de>>(into: &mut Vec<T>, line: &str, trouble: &mut Vec<String>) {
    match serde_json::from_str(line) {
        Ok(row) => into.push(row),
        Err(e) => trouble.push(format!("unreadable row from Docker ({e}): {line}")),
    }
}

fn push_opt<T: for<'de> Deserialize<'de>>(
    into: &mut Option<Vec<T>>,
    line: &str,
    trouble: &mut Vec<String>,
) {
    if let Some(rows) = into {
        push(rows, line, trouble);
    }
}

/// `Filesystem 1024-blocks Used Available Capacity Mounted-on`, POSIX `-P`.
fn parse_df(line: &str) -> Option<Disk> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let num = |i: usize| fields.get(i)?.parse::<u64>().ok();
    Some(Disk {
        total_kb: num(1)?,
        used_kb: num(2)?,
        free_kb: num(3)?,
    })
}

/// Every stats sample in a chunk of the stream. A frame repeats each
/// container once; later samples in the same chunk win at the caller.
pub fn parse_stats(chunk: &str) -> Vec<Stat> {
    chunk
        .lines()
        .map(crate::ansi::strip)
        .filter(|l| is_object(l.trim()))
        .filter_map(|l| serde_json::from_str::<RawStat>(l.trim()).ok())
        .filter(|raw| !raw.id.is_empty())
        .map(|raw| Stat {
            cpu: percent(&raw.cpu),
            mem: percent(&raw.mem),
            mem_bytes: raw
                .mem_usage
                .split('/')
                .next()
                .and_then(size)
                .unwrap_or(0.0),
            id: raw.id,
            name: raw.name,
            mem_usage: raw.mem_usage,
            net_io: raw.net_io,
            block_io: raw.block_io,
            pids: raw.pids,
        })
        .collect()
}

/// A size as Docker prints it, in bytes: `688KiB`, `4.5MiB` (binary,
/// memory) or `1.43kB`, `126B` (decimal, I/O) — go-units' two spellings.
pub fn size(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    let at = raw.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (number, unit) = raw.split_at(at);
    let n: f64 = number.parse().ok()?;
    let scale = match unit.trim() {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(n * scale)
}

/// `"12.34%"` → 12.34; `"--"` (a container that is not running) → 0.
fn percent(raw: &str) -> f64 {
    raw.trim()
        .trim_end_matches('%')
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0)
}

pub fn parse_event(line: &str) -> Option<Event> {
    let raw: RawEvent = serde_json::from_str(line.trim()).ok()?;
    let mut attrs = raw.actor.attributes;
    Some(Event {
        kind: raw.kind,
        action: raw.action,
        id: raw.actor.id,
        name: attrs.remove("name").unwrap_or_default(),
        project: attrs.remove(PROJECT_LABEL).unwrap_or_default(),
        exit_code: attrs.get("exitCode").and_then(|c| c.parse().ok()),
    })
}

/// `docker logs --timestamps` puts an RFC 3339 stamp with fixed
/// nanoseconds in front of every line (`2026-10-12T21:08:44.039917653Z`,
/// measured on 29.6.2). Fixed width is what lets two containers' lines be
/// merged by comparing the strings.
pub fn split_timestamp(line: &str) -> (Option<&str>, &str) {
    let Some((stamp, rest)) = line.split_once(' ') else {
        return (None, line);
    };
    let b = stamp.as_bytes();
    let looks = b.len() >= 20
        && b.len() <= 40
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[10] == b'T';
    if looks {
        (Some(stamp), rest)
    } else {
        (None, line)
    }
}

fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One real row from Docker 29.6.2 for each section, as the script
    /// asks for it.
    const ROUND: &str = r#"==ULAK:ps==
{"id":"50ac9ff6d76cd2a10abb1e73478b54d886186353af2ccb90bbd543a094f40dd0","name":"ulak-tui-probe-web","image":"nginx:alpine","command":"\"/docker-entrypoint.sh nginx -g 'daemon off;'\"","state":"running","status":"Up 3 minutes (healthy)","ports":"127.0.0.1:18080->80/tcp","created":"2026-10-12 21:08:25 +0000 UTC","networks":"bridge","mounts":"","project":"demo","service":"web","number":"1","oneoff":"False","workdir":"/home/u/.ulak/workspaces/n/abc/demo"}
{"id":"aaaaaaaaaaaa1111","name":"lonely","image":"busybox","command":"sh","state":"exited","status":"Exited (1) 2 minutes ago","ports":"","created":"","networks":"","mounts":"","project":"","service":"","number":"","oneoff":"","workdir":""}
==ULAK:images==
{"Containers":"0","CreatedAt":"2026-09-22 22:10:17 +0000 UTC","CreatedSince":"2 weeks ago","Digest":"<none>","ID":"df221db836e1","Repository":"nginx","SharedSize":"N/A","Size":"94.4MB","Tag":"alpine","UniqueSize":"N/A"}
{"Containers":"0","CreatedAt":"2026-09-16 07:35:53 +0000 UTC","CreatedSince":"3 weeks ago","Digest":"<none>","ID":"c4624b89a382","Repository":"<none>","SharedSize":"N/A","Size":"561MB","Tag":"<none>","UniqueSize":"N/A"}
==ULAK:volumes==
{"name":"0a5ae41411982c04eb77ea18056871c4c77cc2a7e86cc0b80da122bf186be0e7","driver":"local","scope":"local","mountpoint":"/var/lib/docker/volumes/0a5a/_data","project":""}
{"name":"demo_db","driver":"local","scope":"local","mountpoint":"/var/lib/docker/volumes/demo_db/_data","project":"demo"}
==ULAK:dangling==
0a5ae41411982c04eb77ea18056871c4c77cc2a7e86cc0b80da122bf186be0e7
==ULAK:networks==
{"id":"28fcf94110f986442800e8f9e1f0c263f13d445b3f05edbad51fba50f62c759d","name":"bridge","driver":"bridge","scope":"local","internal":"false","created":"2026-09-01 10:42:21.896298801 +0000 UTC","project":""}
==ULAK:info==
{"version":"29.6.2","os":"Ubuntu 26.04 LTS","arch":"x86_64","name":"ubuntu-8gb-hel1-2","ncpu":4,"mem":8122327040,"running":1,"containers":2}
==ULAK:disk==
/dev/sda1         78408684 15079152  60079920      21% /
==ULAK:usage==
{"Active":"1","Reclaimable":"5.878GB (80%)","Size":"7.271GB","TotalCount":"19","Type":"Images"}
"#;

    #[test]
    fn one_round_reads_back_into_every_section() {
        let snap = parse(ROUND);
        assert!(snap.engine);
        assert!(snap.trouble.is_empty(), "{:?}", snap.trouble);
        assert_eq!(snap.containers.len(), 2);
        let web = &snap.containers[0];
        assert_eq!(web.short_id(), "50ac9ff6d76c");
        assert_eq!(web.project, "demo");
        assert_eq!(web.service, "web");
        assert_eq!(web.health(), Some(Health::Healthy));
        assert_eq!(web.tone(), Tone::Good);
        assert!(!web.oneoff());
        assert_eq!(snap.containers[1].exit_code(), Some(1));
        assert_eq!(snap.containers[1].tone(), Tone::Bad);

        let images = snap.images.unwrap();
        assert_eq!(images[0].reference(), "nginx:alpine");
        assert!(images[1].dangling());
        assert_eq!(images[1].reference(), "c4624b89a382");
        assert!(web.uses(&images[0]));
        assert!(!web.uses(&images[1]));

        let volumes = snap.volumes.unwrap();
        assert!(volumes[0].anonymous() && volumes[0].unused);
        assert!(!volumes[1].anonymous() && !volumes[1].unused);
        assert_eq!(volumes[1].project, "demo");

        let networks = snap.networks.unwrap();
        assert!(networks[0].builtin());
        assert_eq!(networks[0].short_id(), "28fcf94110f9");

        let info = snap.info.unwrap();
        assert_eq!((info.ncpu, info.version.as_str()), (4, "29.6.2"));
        assert_eq!(snap.disk.unwrap().free_kb, 60079920);
        assert_eq!(snap.usage.unwrap()[0].reclaimable, "5.878GB (80%)");
    }

    /// A section that was not asked for stays `None`, so the screen keeps
    /// the list it had instead of showing an empty one.
    #[test]
    fn a_section_nobody_asked_for_is_absent_not_empty() {
        let snap = parse("==ULAK:ps==\n");
        assert!(snap.containers.is_empty());
        assert!(snap.images.is_none() && snap.volumes.is_none() && snap.info.is_none());
        let snap = parse("==ULAK:ps==\n==ULAK:images==\n");
        assert_eq!(snap.images, Some(Vec::new()));
    }

    #[test]
    fn what_the_engine_says_instead_of_json_is_kept_verbatim() {
        let snap = parse(
            "==ULAK:ps==\npermission denied while trying to connect to the docker API at unix:///var/run/docker.sock\n",
        );
        assert_eq!(snap.trouble.len(), 1);
        assert!(snap.trouble[0].contains("permission denied"));
        let snap = parse("==ULAK:ps==\nsh: 1: docker: not found\nULAK_NO_ENGINE\n");
        assert!(!snap.engine);
    }

    #[test]
    fn the_script_asks_only_for_what_is_wanted_and_quotes_every_template() {
        let lean = script(Want::default());
        assert!(lean.contains("docker ps -a --no-trunc --format '{"));
        assert!(!lean.contains("docker images") && !lean.contains("system df"));
        assert!(lean.contains(r#"(.Label "com.docker.compose.project")"#));
        let full = script(Want::all());
        for needle in [
            "docker images --format '{{json .}}'",
            "docker volume ls -q --filter dangling=true",
            "docker network ls --no-trunc",
            "docker info --format",
            "df -Pk",
            "docker system df --format '{{json .}}'",
        ] {
            assert!(full.contains(needle), "missing {needle}:\n{full}");
        }
        // Each template must be one valid JSON object once Docker fills
        // every field with a JSON string.
        let fill = |format: String| {
            format
                .split("{{json ")
                .enumerate()
                .map(|(i, part)| {
                    if i == 0 {
                        part.to_string()
                    } else {
                        let (_, rest) = part.split_once("}}").unwrap();
                        format!("\"x\"{rest}")
                    }
                })
                .collect::<String>()
        };
        let row: Container = serde_json::from_str(&fill(ps_format())).unwrap();
        assert_eq!(row.workdir, "x");
        let row: Volume = serde_json::from_str(&fill(volume_format())).unwrap();
        assert_eq!(row.project, "x");
        let row: Network = serde_json::from_str(&fill(network_format())).unwrap();
        assert_eq!(row.created, "x");
    }

    /// The frame shape Docker 29.6.2 streams, two frames in one read.
    #[test]
    fn stats_frames_read_back_into_samples() {
        let chunk = "\x1b[H{\"BlockIO\":\"0B / 8.19kB\",\"CPUPerc\":\"1.50%\",\"Container\":\"50ac\",\"ID\":\"50ac9ff6d76c\",\"MemPerc\":\"0.07%\",\"MemUsage\":\"5.105MiB / 7.565GiB\",\"Name\":\"web\",\"NetIO\":\"200B / 42B\",\"PIDs\":\"5\"}\x1b[K\n\x1b[K\n\x1b[J\x1b[H{\"CPUPerc\":\"--\",\"ID\":\"aaaaaaaaaaaa\",\"MemPerc\":\"--\"}\x1b[K\n";
        let got = parse_stats(chunk);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "50ac9ff6d76c");
        assert!((got[0].cpu - 1.5).abs() < 1e-9);
        assert_eq!(got[0].mem_usage, "5.105MiB / 7.565GiB");
        assert_eq!(got[1].cpu, 0.0);
    }

    #[test]
    fn healthchecks_do_not_refresh_but_lifecycle_events_do() {
        let die = parse_event(r#"{"Type":"container","Action":"die","Actor":{"ID":"50ac","Attributes":{"name":"web","exitCode":"2","com.docker.compose.project":"demo"}}}"#).unwrap();
        assert!(die.stale().is_some());
        assert_eq!(die.project, "demo");
        assert_eq!(
            die.notice(),
            Some(Notice::Bad("web exited with code 2".into()))
        );
        for chatter in [
            "exec_create: sh -c true",
            "exec_start: sh -c true",
            "exec_die",
            "attach",
            "top",
        ] {
            let e = Event {
                kind: "container".into(),
                action: chatter.into(),
                ..Event::default()
            };
            assert_eq!(e.stale(), None, "{chatter}");
        }
        let unhealthy = Event {
            kind: "container".into(),
            action: "health_status: unhealthy".into(),
            name: "api".into(),
            ..Event::default()
        };
        assert!(unhealthy.stale().is_some());
        assert_eq!(
            unhealthy.notice(),
            Some(Notice::Bad("api is unhealthy".into()))
        );
        let stopped = Event {
            exit_code: Some(143),
            ..die.clone()
        };
        assert_eq!(stopped.notice(), None, "a stop already said so");
        let pulled = Event {
            kind: "image".into(),
            action: "pull".into(),
            ..Event::default()
        };
        assert!(pulled.stale().unwrap().images);
    }

    #[test]
    fn a_log_timestamp_is_split_off_and_a_plain_line_is_left_alone() {
        let (ts, rest) = split_timestamp("2026-10-12T21:08:44.039917653Z hello world");
        assert_eq!(ts, Some("2026-10-12T21:08:44.039917653Z"));
        assert_eq!(rest, "hello world");
        assert_eq!(
            split_timestamp("plain line here"),
            (None, "plain line here")
        );
        assert_eq!(split_timestamp("2026 is a year"), (None, "2026 is a year"));
        assert_eq!(split_timestamp(""), (None, ""));
    }

    #[test]
    fn an_image_is_in_use_by_name_implicit_latest_or_id() {
        let img = Image {
            id: "sha256:df221db836e1aaaa".into(),
            repository: "nginx".into(),
            tag: "latest".into(),
            ..Image::default()
        };
        let by = |image: &str| Container {
            image: image.into(),
            ..Container::default()
        };
        assert!(by("nginx").uses(&img));
        assert!(by("nginx:latest").uses(&img));
        assert!(by("df221db836e1").uses(&img));
        assert!(!by("nginx:alpine").uses(&img));
        assert!(!by("df22").uses(&img), "too short to be an id");
    }

    #[test]
    fn network_membership_matches_whole_names_even_with_commas() {
        let c = Container {
            networks: "bridge,demo_default,odd,name".into(),
            ..Container::default()
        };
        assert!(c.on_network("demo_default"));
        assert!(c.on_network("odd,name"), "a comma inside a name is legal");
        assert!(!c.on_network("demo"), "a prefix is not a member");
        assert!(!c.on_network(""));
    }

    #[test]
    fn sizes_read_both_of_dockers_spellings() {
        assert_eq!(size("126B"), Some(126.0));
        assert_eq!(size("1.43kB"), Some(1430.0));
        assert_eq!(size("688KiB"), Some(688.0 * 1024.0));
        assert_eq!(size(" 4.5MiB "), Some(4.5 * 1024.0 * 1024.0));
        assert_eq!(size("7.565GiB"), Some(7.565 * 1024.0 * 1024.0 * 1024.0));
        assert_eq!(size("--"), None);
        assert_eq!(size("12parsecs"), None);
    }
}
