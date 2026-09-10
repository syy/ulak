//! The canonical invocation: ONE object, built once per command, that
//! everything else is derived from.
//!
//! Why it exists: ulak is an orchestrator, so "which compose call is
//! this?" must have exactly one answer. The prototype asked that
//! question in three different places (passthrough injected an identity,
//! `down` recomputed one, `status` a third) and they could disagree — a
//! user `-p` made `down` report success while stopping nothing.
//!
//! The rules here are DOCKER's rules, deliberately:
//!   * `-f` paths resolve against the CWD;
//!   * relative paths inside a compose file resolve against the project
//!     directory = dirname of the first `-f` (or `--project-directory`);
//!   * `COMPOSE_FILE` is the fallback, then a walk up from the cwd.
//!
//! WHICH PROJECT a command addresses is docker's rule too, and all five
//! rungs of it, measured on Compose v5.3.1:
//!
//!   1. `-p`
//!   2. `COMPOSE_PROJECT_NAME` in the environment
//!   3. `COMPOSE_PROJECT_NAME` in `--env-file`, else in `.env`
//!   4. a top-level `name:` (interpolated; the last `-f` wins)
//!   5. the project directory's basename, normalized
//!
//! This module answers 1 and 2, which live in argv and the environment.
//! It does not answer 3, 4 or 5 alone, because 3 and 4 mean opening
//! files this crate does not open — `compose::project_name` takes them
//! off the model the SERVER resolved, and `compose_identity` below is
//! only the fallback for routes that never reach one.
//!
//! There is NO sixth rung, and there is no remembered invocation on the
//! Docker path. A previous version remembered `-p` and `-f` per directory
//! and appended `-<12 hex>` to a derived name. All three changed what a
//! later bare Docker command addressed. Measured separately on Compose
//! v5.3.1: a plain command after `-f alt.yaml` discovers the default file
//! rather than `alt.yaml`; a plain `up` after `-p chosen up` opens a
//! SECOND default-named stack, whose `down` leaves `chosen-web-1`
//! running. Ulak now does the same. Its per-stack `Desired` record is the
//! lifecycle contract shared by the background service and Ulak's own root
//! management commands; it never feeds a later Docker invocation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::docker_project_name;
use crate::ui::fail;
use anyhow::{Context, Result};

/// Standard file names compose looks for, in its own order.
const STANDARD_FILES: &[&str] = &[
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// Compose global flags that carry a value and that ulak OWNS — they
/// are captured here and re-emitted (remapped) on the server side.
const OWNED_VALUE_FLAGS: &[&str] = &[
    "-f",
    "--file",
    "-p",
    "--project-name",
    "--profile",
    "--env-file",
    "--project-directory",
];

/// Global flags ulak passes through untouched but must still skip
/// correctly, or the scan would mistake their value for the subcommand.
const PASSTHRU_VALUE_FLAGS: &[&str] = &["--ansi", "--log-level", "--parallel", "--progress"];

/// Value-less globals, passed through untouched.
const PASSTHRU_BOOL_FLAGS: &[&str] = &["--compatibility", "--dry-run", "--all-resources"];

/// `COMPOSE_*` variables ulak owns: forwarding them would fight the
/// flags we emit ourselves, or point compose at a local path.
const OWNED_COMPOSE_ENV: &[&str] = &[
    "COMPOSE_FILE",
    "COMPOSE_PATH_SEPARATOR",
    "COMPOSE_PROJECT_NAME",
    "COMPOSE_PROJECT_DIR",
    "COMPOSE_ENV_FILE",
    "COMPOSE_ENV_FILES",
    // captured into `profiles` and re-emitted as --profile
    "COMPOSE_PROFILES",
];

/// Whether this process supplied Compose input for the current invocation.
///
/// Root management may recover a declared context only when the user did
/// not select one now. Keeping this decision beside `OWNED_COMPOSE_ENV`
/// prevents that precedence check from growing a second, divergent env list.
pub(crate) fn compose_environment_is_explicit() -> bool {
    any_owned_compose_environment(|name| {
        std::env::var_os(name).is_some_and(|value| !value.is_empty())
    })
}

fn any_owned_compose_environment(mut has_value: impl FnMut(&str) -> bool) -> bool {
    OWNED_COMPOSE_ENV.iter().any(|name| has_value(name))
}

#[derive(Debug, Clone)]
pub struct Invocation {
    /// Where the user stood. Meaningful to docker, so meaningful here.
    pub cwd: PathBuf,
    /// Absolute, canonical, in the order compose will merge them.
    pub compose_files: Vec<PathBuf>,
    /// Base for relative paths inside the compose files.
    pub project_dir: PathBuf,
    /// An explicit `-p` / `COMPOSE_PROJECT_NAME`, if the user gave one.
    pub project_name: Option<String>,
    /// `--env-file` arguments, absolute.
    pub env_files: Vec<PathBuf>,
    pub profiles: Vec<String>,
    /// `COMPOSE_*` variables worth carrying to the server.
    pub compose_env: BTreeMap<String, String>,
    /// The compose subcommand and its arguments (globals stripped).
    pub args: Vec<String>,
    /// First non-flag word of `args`, for the read-only/sync heuristic.
    pub subcommand: Option<String>,
    /// Where `subcommand` sits in `args`. A reader that needs the
    /// subcommand's OWN arguments — `composepaths::scan` — starts one
    /// past this, because `args` may open with passthrough globals.
    pub subcommand_at: Option<usize>,
}

impl Invocation {
    /// A project-shaped context for commands that sync files but do
    /// not speak Compose (currently `docker build`).  Keeping this
    /// constructor here lets the existing workspace/sync safety machinery
    /// retain one stable identity without inventing a Compose file.
    pub fn workspace(cwd: PathBuf, project_dir: PathBuf) -> Invocation {
        Invocation {
            cwd,
            compose_files: Vec::new(),
            project_dir,
            project_name: None,
            env_files: Vec::new(),
            profiles: Vec::new(),
            compose_env: BTreeMap::new(),
            args: Vec::new(),
            subcommand: None,
            subcommand_at: None,
        }
    }

    /// `argv` is compose-style: leading globals, then the subcommand.
    /// ulak's own commands pass the globals typed before the command
    /// (`Cli::globals`) — an empty slice only when none were typed.
    pub fn capture(argv: &[String]) -> Result<Invocation> {
        let cwd = std::env::current_dir().context("cannot read current directory")?;
        Self::capture_in(&cwd, argv, std::env::vars().collect())
    }

    pub fn capture_in(
        cwd: &Path,
        argv: &[String],
        env: BTreeMap<String, String>,
    ) -> Result<Invocation> {
        Self::build(cwd, argv, env)
    }

    /// This is the door `desired.json` comes through, so the file gets
    /// exactly the checking a typed `-f` gets — it is untrusted input by
    /// contract: plain JSON on disk that a process holding the user's ssh
    /// keys will act on. Construction has no stateful side effects, so a
    /// service rebuild and a command typed by a human take the same path.
    pub fn rebuild_in(
        cwd: &Path,
        argv: &[String],
        env: BTreeMap<String, String>,
    ) -> Result<Invocation> {
        Self::build(cwd, argv, env)
    }

    fn build(cwd: &Path, argv: &[String], env: BTreeMap<String, String>) -> Result<Invocation> {
        let cwd = cwd
            .canonicalize()
            .with_context(|| format!("cannot canonicalize {}", cwd.display()))?;
        let split = split_globals(argv)?;

        let project_name = split
            .project_name
            .or_else(|| env.get("COMPOSE_PROJECT_NAME").cloned())
            .filter(|n| !n.is_empty());
        let mut profiles = split.profiles;
        if profiles.is_empty()
            && let Some(p) = env.get("COMPOSE_PROFILES")
        {
            profiles = p
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        let env_files: Vec<PathBuf> = split
            .env_files
            .iter()
            .map(|f| absolutize(&cwd, f))
            .collect();
        let project_directory = split.project_directory.map(|d| absolutize(&cwd, &d));

        // ── where do the compose files come from? ────────────────────
        let compose_files = if !split.files.is_empty() {
            resolve_listed(&cwd, &split.files, "-f")?
        } else if let Some(list) = env.get("COMPOSE_FILE").filter(|v| !v.is_empty()) {
            let sep = env
                .get("COMPOSE_PATH_SEPARATOR")
                .filter(|s| !s.is_empty())
                .map(String::as_str)
                .unwrap_or(":");
            let parts: Vec<String> = list.split(sep).map(str::to_string).collect();
            resolve_listed(&cwd, &parts, "COMPOSE_FILE")?
        } else {
            discover(&cwd)?
        };

        // Docker resolves both files and project name from scratch on every
        // command. `Desired` remembers an invocation for lifecycle consumers,
        // but feeding it into this constructor would turn that contract into
        // a hidden sixth Docker precedence rung.
        let project_dir = match project_directory {
            Some(d) => d,
            None => compose_files[0]
                .parent()
                .unwrap_or(Path::new("/"))
                .to_path_buf(),
        };

        let compose_env = env
            .into_iter()
            .filter(|(k, _)| {
                k.starts_with("COMPOSE_")
                    && !OWNED_COMPOSE_ENV.contains(&k.as_str())
                    && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
            .collect();

        let inv = Invocation {
            cwd,
            compose_files,
            project_dir,
            project_name,
            env_files,
            profiles,
            compose_env,
            args: split.rest,
            subcommand: split.subcommand,
            subcommand_at: split.subcommand_at,
        };
        Ok(inv)
    }

    /// The compose globals in compose's OWN argv shape, absolute — the
    /// exact input `capture_in` needs to reconstruct this invocation.
    ///
    /// Deliberately re-emitted from the RESOLVED invocation rather than
    /// echoing what the user typed. A relative `-f` only means anything
    /// next to the cwd it was typed in; absolute flags let `Desired`
    /// rebuild the same declaration from anywhere.
    pub fn globals_argv(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut push = |flag: &str, value: String| {
            out.push(flag.to_string());
            out.push(value);
        };
        for f in &self.compose_files {
            push("-f", f.to_string_lossy().into_owned());
        }
        if let Some(p) = &self.project_name {
            push("-p", p.clone());
        }
        for p in &self.profiles {
            push("--profile", p.clone());
        }
        for e in &self.env_files {
            push("--env-file", e.to_string_lossy().into_owned());
        }
        // Always emitted: it is derived from the first `-f` when the user
        // does not give it, and pinning it is what stops the rebuild from
        // re-deriving a different answer later.
        push(
            "--project-directory",
            self.project_dir.to_string_lossy().into_owned(),
        );
        out
    }

    /// Human-readable echo of the compose globals ulak will emit.
    pub fn flags_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for f in &self.compose_files {
            parts.push(format!("-f {}", short(&self.cwd, f)));
        }
        if let Some(p) = &self.project_name {
            parts.push(format!("-p {p}"));
        }
        for p in &self.profiles {
            parts.push(format!("--profile {p}"));
        }
        for e in &self.env_files {
            parts.push(format!("--env-file {}", short(&self.cwd, e)));
        }
        parts.join(" ")
    }

    /// Deepest directory that contains every path the invocation itself
    /// names. F2 widens it with the resolved footprint; on its own it
    /// already makes `-f ../a.yaml -f b.yaml` survive the trip, because
    /// the workspace preserves layout RELATIVE to the anchor.
    pub fn base_anchor(&self) -> PathBuf {
        let mut paths: Vec<&Path> = vec![&self.project_dir];
        paths.extend(self.compose_files.iter().filter_map(|f| f.parent()));
        paths.extend(self.env_files.iter().filter_map(|f| f.parent()));
        common_ancestor(&paths)
    }

    /// Compose's own last rung: the project directory's basename,
    /// normalized the way compose normalizes it (see
    /// `config::docker_project_name` for the measurements).
    ///
    /// May come back EMPTY, for a directory whose name survives none of
    /// that (`...`). That is docker's answer too — it refuses with
    /// "project name must not be empty" — and letting the refusal come
    /// from docker, in docker's words, is the faithful thing to do with
    /// it.
    pub fn default_name(&self) -> String {
        docker_project_name(
            self.project_dir
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default()
                .as_ref(),
        )
    }

    /// The local path which identifies transported bytes. Config owns the
    /// namespace and turns this into a `WorkspaceKey`; keeping that join
    /// there prevents Docker's project identity from leaking into it.
    pub(crate) fn workspace_identity_path(&self) -> &Path {
        self.compose_files
            .first()
            .map(PathBuf::as_path)
            .unwrap_or(&self.project_dir)
    }

    /// The `-p` value every ulak-issued compose call carries, as far as
    /// argv and the environment can answer it: docker's first two rungs,
    /// then its last.
    ///
    /// It used to append `-<12 hex of host+path>` to the derived name so
    /// two same-named checkouts could not collide on one server. That
    /// was ulak's invention, and it cost more than it bought: a project
    /// whose directory is `api` and whose scripts pass `-p api` agrees
    /// with itself under plain docker and could NEVER agree under a
    /// hashed name — the one case where ulak was worse than the thing it
    /// wraps. The collision it prevented is docker's own behaviour,
    /// measured on Compose v5.3.1: two checkouts both called `api` share
    /// one project, and a `down` in either removes the other's
    /// containers. Docker's answer to that is to name the project —
    /// `name:` in the file, `COMPOSE_PROJECT_NAME` in `.env`, or `-p` —
    /// and ulak now honours all three rather than inventing a fourth.
    ///
    /// The rungs between (an env file's `COMPOSE_PROJECT_NAME`, the
    /// YAML's `name:`) cannot be answered without reading those files,
    /// which this crate deliberately does not do. `Project::identity`
    /// takes the answer from the server's own `docker compose config`
    /// instead; this is the fallback for the paths that never reach a
    /// server.
    pub fn compose_identity(&self) -> String {
        match &self.project_name {
            Some(name) => name.clone(),
            None => self.default_name(),
        }
    }
}

/// Deepest directory that is an ancestor of (or equal to) every input.
/// `/` when they share nothing — legal, and the caller decides whether
/// such a spread is acceptable.
pub fn common_ancestor(dirs: &[&Path]) -> PathBuf {
    let Some((first, rest)) = dirs.split_first() else {
        return PathBuf::from("/");
    };
    let mut common: Vec<_> = first.components().collect();
    for d in rest {
        let other: Vec<_> = d.components().collect();
        let keep = common
            .iter()
            .zip(&other)
            .take_while(|(a, b)| a == b)
            .count();
        common.truncate(keep);
    }
    common.iter().collect()
}

/// Path of `p` inside the workspace, i.e. relative to the anchor. `None`
/// when `p` escapes the anchor, which is always a ulak bug.
pub fn anchor_rel(anchor: &Path, p: &Path) -> Option<String> {
    let rel = p.strip_prefix(anchor).ok()?;
    if rel.as_os_str().is_empty() {
        return Some(".".into());
    }
    Some(rel.to_string_lossy().into_owned())
}

fn short(cwd: &Path, p: &Path) -> String {
    p.strip_prefix(cwd)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

fn absolutize(cwd: &Path, raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    let joined = if p.is_absolute() { p } else { cwd.join(p) };
    joined.canonicalize().unwrap_or(joined)
}

fn resolve_listed(cwd: &Path, list: &[String], origin: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for raw in list {
        if raw == "-" {
            return Err(fail!("Ulak cannot read a compose file from stdin (`-f -`)")
                .now("write it to a file and pass that path instead")
                .into_err());
        }
        let path = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            cwd.join(raw)
        };
        let path = path.canonicalize().map_err(|e| {
            fail!("compose file {raw} (from {origin}) cannot be read: {e}")
                .now(format!("check the path from {}", cwd.display()))
                .into_err()
        })?;
        if !path.is_file() {
            return Err(fail!("{} is not a file", path.display())
                .now("point -f at a compose YAML file")
                .into_err());
        }
        out.push(path);
    }
    if out.is_empty() {
        return Err(fail!("no compose file given in {origin}")
            .now("pass one: ulak docker compose -f compose.yaml <command>")
            .into_err());
    }
    Ok(out)
}

/// Compose's own discovery: standard names in the cwd, then upward.
/// An `*.override.*` twin next to the winner joins it, as compose does.
fn discover(cwd: &Path) -> Result<Vec<PathBuf>> {
    for dir in cwd.ancestors() {
        let Some(name) = STANDARD_FILES.iter().find(|f| dir.join(f).is_file()) else {
            continue;
        };
        let mut files = vec![dir.join(name)];
        let (stem, ext) = name.rsplit_once('.').unwrap_or((name, "yaml"));
        for over_ext in [ext, if ext == "yaml" { "yml" } else { "yaml" }] {
            let over = dir.join(format!("{stem}.override.{over_ext}"));
            if over.is_file() {
                files.push(over);
                break;
            }
        }
        return Ok(files);
    }
    Err(fail!("{NO_PROJECT} {} or any parent directory", cwd.display())
        .now("cd into your compose project, or name the files: ulak docker compose -f a.yaml -f b.yaml <command>")
        .into_err())
}

// ─── global-flag split ──────────────────────────────────────────────

#[derive(Debug, Default)]
struct Split {
    files: Vec<String>,
    project_name: Option<String>,
    profiles: Vec<String>,
    env_files: Vec<String>,
    project_directory: Option<String>,
    /// Untouched globals + the subcommand + its arguments.
    rest: Vec<String>,
    subcommand: Option<String>,
    /// Where `subcommand` sits inside `rest`. Not always 0: the
    /// passthrough globals come first.
    subcommand_at: Option<usize>,
}

/// `-papi` → `("-p", "api")`, and `-p=api` the same. pflag lets a short
/// flag swallow its own value inside one word, either spelling, and
/// Compose's only short globals are `-f` and `-p` — both of which take
/// a value, so there is no value-less short to bundle in front and a
/// longer word starting with one of them can only be this shape.
///
/// The spelling that went uncaptured was `-papi`. `-p=api` was already
/// LOUD — the stray check below splits on `=` and recognised the `-p`
/// — but `-papi` matched neither that check nor the flag table, so it
/// travelled to the server as an unknown global while ulak keyed its
/// own state to a name derived from the directory. Compose read the
/// later `-papi` and won: `up` brought up project `api`, and the
/// `down` that followed said it had stopped it while stopping nothing.
/// That is the exact failure this module's header was written about,
/// reached through the one spelling the fail-safe did not cover.
///
/// An empty value (`-p=`) deliberately answers `None`: it then reaches
/// the stray check as the unrecognised word it is and errors there,
/// rather than quietly becoming a project name of no characters.
fn attached_owned_short(arg: &str) -> Option<(&'static str, String)> {
    let flag = OWNED_VALUE_FLAGS
        .iter()
        .copied()
        .find(|f| f.len() == 2 && arg.len() > 2 && arg.starts_with(f))?;
    let rest = &arg[2..];
    let value = rest.strip_prefix('=').unwrap_or(rest);
    (!value.is_empty()).then(|| (flag, value.to_string()))
}

fn split_globals(argv: &[String]) -> Result<Split> {
    let mut s = Split::default();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if !arg.starts_with('-') || arg == "-" {
            break; // the subcommand
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => match attached_owned_short(arg) {
                Some((f, v)) => (f, Some(v)),
                None => (arg.as_str(), None),
            },
        };

        if OWNED_VALUE_FLAGS.contains(&flag) {
            let value = match inline {
                Some(v) => v,
                None => {
                    i += 1;
                    argv.get(i).cloned().ok_or_else(|| {
                        fail!("{flag} needs a value")
                            .now(format!("e.g. {flag} <value>"))
                            .into_err()
                    })?
                }
            };
            match flag {
                "-f" | "--file" => {
                    s.files.push(value);
                }
                "-p" | "--project-name" => {
                    s.project_name = Some(value);
                }
                "--profile" => s.profiles.push(value),
                "--env-file" => s.env_files.push(value),
                "--project-directory" => s.project_directory = Some(value),
                _ => unreachable!("OWNED_VALUE_FLAGS is exhaustive"),
            }
            i += 1;
            continue;
        }

        if PASSTHRU_BOOL_FLAGS.contains(&flag) {
            s.rest.push(arg.clone());
            i += 1;
            continue;
        }
        if PASSTHRU_VALUE_FLAGS.contains(&flag) {
            s.rest.push(arg.clone());
            i += 1;
            if inline.is_none()
                && let Some(v) = argv.get(i)
            {
                s.rest.push(v.clone());
                i += 1;
            }
            continue;
        }
        break; // unrecognized global: stop scanning, stay conservative
    }

    // The scan stopped either ON the subcommand (the good case) or on a
    // flag we do not know. Looking for the subcommand from THERE — not
    // from the start of `rest` — keeps `--ansi never config` classified
    // as the read-only `config`, not as the value word "never".
    //
    // Where it sits in `rest` is recorded at the same time, from the same
    // scan, because `rest` begins with the passthrough globals this loop
    // consumed and a reader that assumed `rest[0]` was the subcommand
    // read one of those instead. `composepaths::scan` was that reader:
    // `compose --progress plain run -v ./data:/data` matched none of its
    // subcommands, so the bind source never joined the footprint and the
    // server mounted an empty directory it had just created. Deriving the
    // index later cannot be made to agree — an unknown global's VALUE is
    // a bare word too — so the one scan that knows says both.
    let globals_len = s.rest.len();
    let at = argv[i..].iter().position(|a| !a.starts_with('-'));
    s.subcommand = at.map(|p| argv[i + p].clone());
    s.subcommand_at = at.map(|p| globals_len + p);
    s.rest.extend_from_slice(&argv[i..]);

    // Conservative stop above must never hide a `-f`/`-p` sitting behind
    // an unknown global: that would silently split the identity in two,
    // which is exactly the class of bug this module exists to kill.
    // Only the region BEFORE the subcommand is checked — `exec -T db
    // psql -p 5432` carries a container flag, not a project name.
    let head_len = s
        .rest
        .iter()
        .position(|a| Some(a.as_str()) == s.subcommand.as_deref())
        .unwrap_or(s.rest.len());
    if let Some(stray) = s.rest[..head_len].iter().find(|a| {
        let f = a.split('=').next().unwrap_or(a);
        OWNED_VALUE_FLAGS.contains(&f) || attached_owned_short(a).is_some()
    }) {
        return Err(fail!(
            "the compose global {stray} came after a flag Ulak does not know, so it could not be captured"
        )
        .now(format!("put {stray} first: ulak docker compose {stray} … <command>"))
        .into_err());
    }
    Ok(s)
}

/// The opening of "you are not standing in a project". Shared, because
/// one caller has to RECOGNISE it rather than report it: `status` run
/// from anywhere else is not a mistake to correct, it is a different and
/// perfectly good question — "what is this MACHINE doing?"
pub const NO_PROJECT: &str = "no compose file found in";

/// THE state root — every ledger, lock, cache and audit trail hangs off
/// this one function. It used to be computed twice: ledger and
/// invocations honoured `XDG_STATE_HOME` while lock and audit hard-coded
/// `~/.local/state`, so a service and a CLI started from two different
/// shells could look at two different sets of books.
pub fn state_dir() -> Option<PathBuf> {
    // A unit test may never reach the developer's own state. It has no
    // sandbox for a process-wide root — the e2e suites get one by
    // driving the real binary with a fake HOME, but a unit test runs
    // INSIDE the process and cannot set an environment variable without
    // racing every other test in the same binary.
    //
    // Measured on a real machine running the suite: 23 empty
    // `workspaces/no-such-workspace-*` directories appeared in the
    // developer's state. Patching each writer is whack-a-mole; the root
    // is this function.
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!("ulak-unit-{}", std::process::id())));
    }
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => crate::config::home_dir().ok()?.join(".local/state"),
    };
    Some(base.join("ulak"))
}

/// Same root, but as an error when HOME is unset — for callers that
/// cannot silently do nothing (the lock, above all).
pub fn state_dir_required() -> Result<PathBuf> {
    state_dir().ok_or_else(|| {
        fail!("HOME is not set, so ulak has nowhere to keep its state")
            .now("export HOME to your home directory and retry")
            .into_err()
    })
}

/// Create a directory only its owner can enter. The state tree names
/// hosts and paths and holds the resolved compose model — it is nobody
/// else's business on a shared machine.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    let mut perms = std::fs::metadata(dir)?.permissions();
    if perms.mode() & 0o077 != 0 {
        perms.set_mode(0o700);
        std::fs::set_permissions(dir, perms)?;
    }
    Ok(())
}

/// Write a file only its owner can read. The mode is applied twice on
/// purpose: `OpenOptions::mode` only takes effect when the file is
/// CREATED, so a file that already exists at 0644 would keep it.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(bytes)
}

// ─── "has this workspace ever been synced to this destination?" ────────
//
// Every compose subcommand parses the compose FILES, so even `ps` and
// `down` need them present in the workspace. Read-only commands skip the
// sync for speed — which is only safe once the workspace exists. Asking the
// server would cost the round-trip the skip exists to save, so the
// answer is cached locally. It is a fact about one remote filesystem, so
// reusing it for another destination would skip that destination's required
// first sync. Losing the cache costs one extra sync.

fn receipt_marker(key: &crate::config::WorkspaceStateKey, name: &str) -> Option<PathBuf> {
    Some(crate::intent::workspace_state_dir_path(key)?.join(name))
}

fn writable_receipt_marker(key: &crate::config::WorkspaceStateKey, name: &str) -> Option<PathBuf> {
    Some(crate::intent::workspace_state_dir(key)?.join(name))
}

pub(crate) fn mark_synced(key: &crate::config::WorkspaceStateKey) {
    if let Some(path) = writable_receipt_marker(key, crate::intent::SYNCED) {
        let _ = write_private(&path, b"");
    }
}

pub(crate) fn ever_synced(key: &crate::config::WorkspaceStateKey) -> bool {
    receipt_marker(key, crate::intent::SYNCED).is_some_and(|p| p.exists())
}

/// The workspace's complete local state (uuid, anchor and every
/// destination's sync receipts), plus every stack state that referred to it.
///
/// Test-only, and honest about it: production has no caller and `clean`
/// deliberately uses the destination-specific sibling below. It survives
/// as the CONTRAST the receipt tests measure against — a delete is only
/// provably narrow when the wholesale one is right beside it.
#[cfg(test)]
pub fn forget_workspace(workspace_id: &str) {
    crate::intent::forget_workspace(workspace_id);
}

/// Drop one destination's receipts after that remote copy was cleaned,
/// without touching another destination's deletion ownership.
pub(crate) fn forget_workspace_destination(
    key: &crate::config::WorkspaceStateKey,
    destination: &str,
) {
    crate::intent::forget_workspace_destination(key, destination);
}

/// Deletions the workspace still owes: listed, over budget, not applied.
///
/// A branch switch produces hundreds at once, and the prototype answered
/// by re-prompting on every save while the workspace drifted quietly.
/// Recording the count lets `status` and `doctor` state the drift out
/// loud, with the one command that ends it.
pub(crate) fn record_pending_deletions(key: &crate::config::WorkspaceStateKey, count: usize) {
    let Some(path) = writable_receipt_marker(key, "pending") else {
        return;
    };
    if count == 0 {
        let _ = std::fs::remove_file(path);
        return;
    }
    let _ = write_private(&path, count.to_string().as_bytes());
}

pub(crate) fn pending_deletions(key: &crate::config::WorkspaceStateKey) -> usize {
    receipt_marker(key, "pending")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0)
}

/// When the last push started reading this workspace, and the last
/// SECOND it was still reading in. The two ends are measured
/// differently on purpose — `sync::hidden_by_the_quick_check` is the
/// only caller and says why.
///
/// Three integers, so neither end needs a float:
/// `<start secs> <start nanos> <end secs>`. On disk rather than in
/// memory because `ulak sync` typed by hand is a different process from
/// the service, and the hole is the same one for both.
pub(crate) fn record_push_window(
    key: &crate::config::WorkspaceStateKey,
    from: std::time::SystemTime,
    to: u64,
) {
    let Some(path) = writable_receipt_marker(key, "pushed") else {
        return;
    };
    let Ok(d) = from.duration_since(std::time::UNIX_EPOCH) else {
        return;
    };
    let _ = write_private(
        &path,
        format!("{} {} {to}", d.as_secs(), d.subsec_nanos()).as_bytes(),
    );
}

pub(crate) fn last_push_window(
    key: &crate::config::WorkspaceStateKey,
) -> Option<(std::time::SystemTime, u64)> {
    let text = std::fs::read_to_string(receipt_marker(key, "pushed")?).ok()?;
    let mut fields = text.split_whitespace();
    let secs: u64 = fields.next()?.parse().ok()?;
    let nanos: u32 = fields.next()?.parse().ok()?;
    let to: u64 = fields.next()?.parse().ok()?;
    Some((
        std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos),
        to,
    ))
}

/// Seconds since the epoch, which is the only clock the comparison can
/// use: it is what a file's mtime is measured against too.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── which machine owns this workspace ─────────────────────────────────
//
// The automatic client namespace keeps independent local state roots out
// of one another before the id is even considered. A configured shared
// namespace is intentional, though, and copied/reset state can still make
// two clients claim the same locator. The manifest UUID remains the last
// ownership proof — but only if each client remembers the one IT minted.
// Comparing against whatever the server happens to hold would agree with
// itself.

/// The UUID this machine minted for this workspace, if it ever did.
///
/// A pure read: it uses the path that is NOT created, because this is
/// asked on every probe and about ids that may belong to nobody. The
/// creating variant turned each question into a permanent directory.
pub fn recorded_uuid(workspace_id: &str) -> Option<String> {
    let path = crate::intent::workspace_dir_path(workspace_id)?.join("uuid");
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The write, which does need the directory to exist.
pub fn record_uuid(workspace_id: &str, uuid: &str) {
    if let Some(dir) = crate::intent::workspace_dir(workspace_id) {
        let _ = write_private(&dir.join("uuid"), uuid.as_bytes());
    }
}

/// The anchor the workspace on the server is currently laid out under,
/// as this machine last left it. Read like `recorded_uuid`: a pure read,
/// on a path that is not created for the asking.
pub fn recorded_anchor(workspace_id: &str) -> Option<PathBuf> {
    let path = crate::intent::workspace_dir_path(workspace_id)?.join("anchor");
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim_end_matches('\n');
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// Written where the server manifest is settled, so the two agree.
pub fn record_anchor(workspace_id: &str, anchor: &Path) {
    if let Some(dir) = crate::intent::workspace_dir(workspace_id) {
        let _ = write_private(&dir.join("anchor"), anchor.as_os_str().as_encoded_bytes());
    }
}

/// The anchor to lay this invocation out under: the one it computed for
/// itself, widened to cover the one the workspace already uses.
///
/// One workspace is driven by several commands that compute the anchor
/// differently. `docker build` climbs to the common ancestor of the
/// root, the context and the Dockerfile — so a Dockerfile kept beside
/// the project anchors it ABOVE the root — while `run`, `create` and
/// `bake` anchor AT the root. They share a workspace id (all four hash
/// the root), hence one remote directory, one manifest and one ledger.
///
/// Left alone, the two spellings alternate, and `ensure_workspace`
/// answers every change of anchor by laying the workspace out again:
/// `rm -rf` and a forgotten ledger, or — with no terminal to confirm in,
/// which is every script and every CI job — a hard refusal. Two commands
/// a developer alternates all day destroyed and re-pushed each other's
/// tree.
///
/// Widening is the whole fix, and it is safe because the anchor decides
/// LAYOUT only: the workspace reproduces `entry - anchor`, never the
/// anchor's own tree, so a wider one carries exactly the same files one
/// level deeper. Narrowing is what has no answer — the server would
/// still be holding the wide layout — so it never happens. The result is
/// always an ancestor of the root, because both inputs are.
pub fn workspace_anchor(workspace_id: &str, computed: PathBuf) -> PathBuf {
    widen(recorded_anchor(workspace_id).as_deref(), computed)
}

/// Split out from `workspace_anchor` so the decision can be tested:
/// the read around it goes through a process-wide state directory that a
/// unit test has no way to sandbox (see `state_dir`), which would have
/// left the only interesting half of this unexercised.
fn widen(recorded: Option<&Path>, computed: PathBuf) -> PathBuf {
    let Some(recorded) = recorded else {
        return computed;
    };
    if recorded == computed {
        return computed;
    }
    common_ancestor(&[recorded, computed.as_path()])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// `synced`, pending deletions and the push window all describe one
    /// remote copy. Sharing any one of them lets activity on server A alter
    /// the safety decision made for server B.
    #[test]
    fn sync_receipts_are_partitioned_by_destination() {
        let workspace = crate::config::WorkspaceKey::from_namespace(
            "receipt-tests",
            Path::new("/same/checkout/compose.yaml"),
        )
        .unwrap();
        let a = workspace.state_key("server-a");
        let b = workspace.state_key("server-b");
        forget_workspace(workspace.id());

        mark_synced(&a);
        record_pending_deletions(&a, 7);
        let from = std::time::UNIX_EPOCH + std::time::Duration::new(42, 9);
        record_push_window(&a, from, 43);

        assert!(ever_synced(&a));
        assert!(!ever_synced(&b));
        assert_eq!(pending_deletions(&a), 7);
        assert_eq!(pending_deletions(&b), 0);
        assert_eq!(last_push_window(&a), Some((from, 43)));
        assert_eq!(last_push_window(&b), None);

        mark_synced(&b);
        record_pending_deletions(&b, 3);
        forget_workspace_destination(&a, "server-a");
        assert!(!ever_synced(&a));
        assert_eq!(pending_deletions(&a), 0);
        assert!(ever_synced(&b));
        assert_eq!(pending_deletions(&b), 3);

        forget_workspace(workspace.id());
    }

    /// The UUID claims the checkout's ownership of its remote copies, so
    /// it outlives any single `clean` — but not ALL of them.
    ///
    /// The regression: narrowing `clean` to one destination's receipts
    /// stopped it clearing the claim at all. `settle_uuid` only asks who
    /// owns a workspace when nothing is recorded, so a checkout that had
    /// cleaned its every server still carried a claim, and the next sync
    /// into a deliberately shared namespace would adopt another machine's
    /// workspace in silence instead of refusing an ambiguous owner.
    #[test]
    fn the_last_destination_cleaned_takes_the_checkout_claim_with_it() {
        let workspace = crate::config::WorkspaceKey::from_namespace(
            "claim-tests",
            Path::new("/same/checkout/compose.yaml"),
        )
        .unwrap();
        let a = workspace.state_key("server-a");
        let b = workspace.state_key("server-b");
        forget_workspace(workspace.id());

        record_uuid(workspace.id(), "U-1");
        record_anchor(workspace.id(), Path::new("/same/checkout"));
        mark_synced(&a);
        mark_synced(&b);

        forget_workspace_destination(&a, "server-a");
        assert_eq!(
            recorded_uuid(workspace.id()).as_deref(),
            Some("U-1"),
            "server-b still holds a remote copy this UUID owns"
        );
        assert_eq!(
            recorded_anchor(workspace.id()).as_deref(),
            Some(Path::new("/same/checkout"))
        );

        forget_workspace_destination(&b, "server-b");
        assert_eq!(
            recorded_uuid(workspace.id()),
            None,
            "no remote copy is left for the claim to own"
        );
        assert_eq!(recorded_anchor(workspace.id()), None);

        forget_workspace(workspace.id());
    }

    /// The first command on a workspace has nothing to widen against and
    /// must be left exactly as it computed itself, or every workspace
    /// would start life one level too high.
    #[test]
    fn the_first_anchor_on_a_workspace_is_the_one_it_computed() {
        assert_eq!(
            widen(None, PathBuf::from("/w/repo")),
            PathBuf::from("/w/repo")
        );
    }

    /// The failure this exists for: `docker build -f ../shared/Dockerfile .`
    /// anchors at `/w`, then `docker run -v ./app:/app` computes `/w/repo`
    /// for the SAME workspace id. Narrowing there asked `ensure_workspace`
    /// to lay the workspace out again — `rm -rf` with a terminal, a hard
    /// refusal without one — and the next build asked for it right back.
    #[test]
    fn a_narrower_anchor_widens_to_the_one_the_workspace_already_uses() {
        assert_eq!(
            widen(Some(Path::new("/w")), PathBuf::from("/w/repo")),
            PathBuf::from("/w"),
        );
    }

    /// The other direction is a real move and must still be one: the
    /// server is holding a layout that has no room for the new reference,
    /// so the relayout this returns a different anchor to trigger is the
    /// correct answer, not the bug above.
    #[test]
    fn an_anchor_that_genuinely_widens_is_not_held_back() {
        assert_eq!(
            widen(Some(Path::new("/w/repo")), PathBuf::from("/w")),
            PathBuf::from("/w"),
        );
    }

    /// Two anchors neither of which contains the other resolve to the one
    /// that covers both — never to either input, which would leave half
    /// the workspace unreachable from the anchor it is laid out under.
    #[test]
    fn two_sibling_anchors_resolve_to_the_one_that_holds_both() {
        assert_eq!(
            widen(Some(Path::new("/w/a")), PathBuf::from("/w/b")),
            PathBuf::from("/w"),
        );
    }

    /// Idempotence, which is what keeps a settled workspace from
    /// re-laying itself out on every command.
    #[test]
    fn an_unchanged_anchor_stays_put() {
        assert_eq!(
            widen(Some(Path::new("/w/repo")), PathBuf::from("/w/repo")),
            PathBuf::from("/w/repo"),
        );
    }

    #[test]
    fn a_unit_test_can_never_reach_the_developers_own_state() {
        // Twice now, running this suite on the machine that USES ulak
        // damaged its real state: 23 empty workspace directories the first
        // time, and the second time an `invocations.json` holding only
        // temp-dir entries from tests — which broke `ulak docker compose ps` in a
        // real project, because the invocation it had been taught was
        // gone. A unit test runs inside the process and cannot sandbox a
        // process-wide root by setting an environment variable without
        // racing every other test in the binary, so the root sandboxes
        // itself. This is the assertion that keeps it that way.
        let dir = state_dir().expect("tests always have a state dir");
        assert!(
            dir.starts_with(std::env::temp_dir()),
            "a unit test's state must live in the temp dir, not {}",
            dir.display()
        );
        if let Ok(home) = std::env::var("HOME") {
            assert!(
                !dir.starts_with(&home),
                "{} is inside the developer's home",
                dir.display()
            );
        }
    }

    #[test]
    fn globals_are_split_from_the_subcommand() {
        let s = split_globals(&v(&[
            "-f",
            "a.yaml",
            "--file",
            "b.yaml",
            "-p",
            "api",
            "--profile",
            "dev",
            "up",
            "-d",
        ]))
        .unwrap();
        assert_eq!(s.files, vec!["a.yaml", "b.yaml"]);
        assert_eq!(s.project_name.as_deref(), Some("api"));
        assert_eq!(s.profiles, vec!["dev"]);
        assert_eq!(s.rest, v(&["up", "-d"]));
        assert_eq!(s.subcommand.as_deref(), Some("up"));
    }

    #[test]
    fn subcommand_flags_are_never_mistaken_for_globals() {
        // The documented trap: `-p 5432` here is psql's port, not a
        // project name, because it sits AFTER the subcommand.
        let s = split_globals(&v(&["exec", "-T", "db", "psql", "-p", "5432"])).unwrap();
        assert!(s.project_name.is_none());
        assert!(s.files.is_empty());
        assert_eq!(s.subcommand.as_deref(), Some("exec"));
        // `logs -f` is follow, not a file.
        let s = split_globals(&v(&["logs", "-f", "web"])).unwrap();
        assert!(s.files.is_empty());
        assert_eq!(s.subcommand.as_deref(), Some("logs"));
    }

    #[test]
    fn inline_values_and_passthrough_globals() {
        let s = split_globals(&v(&[
            "--ansi",
            "never",
            "--file=a.yaml",
            "--compatibility",
            "config",
        ]))
        .unwrap();
        assert_eq!(s.files, vec!["a.yaml"]);
        assert_eq!(s.rest, v(&["--ansi", "never", "--compatibility", "config"]));
        assert_eq!(s.subcommand.as_deref(), Some("config"));
    }

    /// The spelling that reached neither the flag table nor the stray
    /// check. `-papi` left `project_name` at `None`, so ulak keyed its
    /// manifest, model and tunnel state to the directory's derived name
    /// while the word travelled on and Compose — reading the last `-p` —
    /// ran project `api`. `up` started containers ulak's own `down`,
    /// `status` and background service could not then see.
    #[test]
    fn an_attached_short_global_is_read_the_way_compose_reads_it() {
        let s = split_globals(&v(&["-papi", "up", "-d"])).unwrap();
        assert_eq!(s.project_name.as_deref(), Some("api"));
        // The word is CAPTURED, so it must not also travel: two `-p`s on
        // the remote line would put Compose back on the user's name.
        assert_eq!(s.rest, v(&["up", "-d"]));
        assert_eq!(s.subcommand.as_deref(), Some("up"));

        // `-f` merges rather than overrides — Compose's `--file` is a
        // repeatable list — so an uncaptured one left ulak resolving a
        // different set of compose files than the server would.
        let s = split_globals(&v(&["-fstack.yml", "-fextra.yml", "config"])).unwrap();
        assert_eq!(s.files, vec!["stack.yml", "extra.yml"]);
        assert_eq!(s.rest, v(&["config"]));

        // pflag's other one-word spelling. This one was already loud —
        // the stray check splits on `=` — and is now simply correct.
        let s = split_globals(&v(&["-p=api", "-f=a.yaml", "up"])).unwrap();
        assert_eq!(s.project_name.as_deref(), Some("api"));
        assert_eq!(s.files, vec!["a.yaml"]);
        assert_eq!(s.rest, v(&["up"]));
    }

    /// A short global that only LOOKS attached carries no name, and
    /// inventing one from no characters would be the silent split this
    /// module exists to prevent. It stays an error.
    #[test]
    fn an_attached_short_global_with_no_value_stays_loud() {
        let err = split_globals(&v(&["-p=", "up"])).unwrap_err();
        assert!(err.to_string().contains("could not be captured"), "{err}");
        // The separated spelling with nothing after it keeps its own
        // error, which names the flag rather than the word.
        let err = split_globals(&v(&["-p"])).unwrap_err();
        assert!(err.to_string().contains("needs a value"), "{err}");
    }

    #[test]
    fn an_uncapturable_global_is_an_error_not_a_silent_split() {
        let err = split_globals(&v(&["--weird-flag", "-f", "a.yaml", "up"])).unwrap_err();
        assert!(err.to_string().contains("could not be captured"), "{err}");
        // Behind an unknown global the attached spelling cannot be
        // captured either, and the fail-safe has to recognise it there
        // too — it was written for `-f`/`-p` and read only those.
        let err = split_globals(&v(&["--weird-flag", "-papi", "up"])).unwrap_err();
        assert!(err.to_string().contains("could not be captured"), "{err}");
        assert!(err.to_string().contains("-papi"), "{err}");
    }

    /// The scan must not read a passthrough global's own value as an
    /// attached `-p`: erroring there would refuse a command Compose
    /// accepts.
    #[test]
    fn a_passthrough_global_is_not_mistaken_for_an_attached_short() {
        let s = split_globals(&v(&["--progress", "plain", "--profile", "dev", "up"])).unwrap();
        assert!(s.project_name.is_none());
        assert_eq!(s.profiles, vec!["dev"]);
        assert_eq!(s.rest, v(&["--progress", "plain", "up"]));
        assert_eq!(s.subcommand.as_deref(), Some("up"));
    }

    #[test]
    fn missing_flag_value_is_reported() {
        let err = split_globals(&v(&["-f"])).unwrap_err();
        assert!(err.to_string().contains("needs a value"));
    }

    #[test]
    fn discovery_walks_up_and_picks_the_override_twin() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(root.join("compose.override.yaml"), "services: {}\n").unwrap();
        let deep = root.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();

        let found = discover(&deep).unwrap();
        assert_eq!(
            found,
            vec![
                root.join("compose.yaml"),
                root.join("compose.override.yaml")
            ]
        );
        // A directory with nothing above it gets a guided error.
        let bare = tempfile::tempdir().unwrap();
        let err = discover(bare.path()).unwrap_err();
        assert!(err.to_string().contains("no compose file found"));
    }

    /// Docker's rungs 1, 2 and 5 — the ones argv and the environment can
    /// answer without opening a file.
    ///
    /// The bare case used to append `-<12 hex of host+path>`, which is
    /// what made `-p api` in a directory called `api` unable to agree
    /// with itself. Now the derived name IS docker's: the directory,
    /// normalized.
    #[test]
    fn identity_is_dockers_own_cascade_as_far_as_argv_can_answer_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("api");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("compose.yaml"), "services: {}\n").unwrap();

        let plain = Invocation::capture_in(&root, &v(&["up", "-d"]), BTreeMap::new()).unwrap();
        assert_eq!(plain.compose_identity(), "api", "rung 5: the directory");

        let named =
            Invocation::capture_in(&root, &v(&["-p", "chosen", "up", "-d"]), BTreeMap::new())
                .unwrap();
        assert_eq!(named.compose_identity(), "chosen", "rung 1: -p");

        let env = BTreeMap::from([("COMPOSE_PROJECT_NAME".to_string(), "fromenv".to_string())]);
        let from_env = Invocation::capture_in(&root, &v(&["up", "-d"]), env.clone()).unwrap();
        assert_eq!(from_env.compose_identity(), "fromenv", "rung 2");

        let both = Invocation::capture_in(&root, &v(&["-p", "chosen", "up"]), env).unwrap();
        assert_eq!(both.compose_identity(), "chosen", "rung 1 beats rung 2");

        // The name decides which CONTAINERS; the path decides which
        // WORKSPACE. Naming a project must not move its files.
        assert_eq!(
            named.workspace_identity_path(),
            plain.workspace_identity_path()
        );
    }

    /// The directory a `-p` names, addressed by a command that names
    /// neither — the shape a wrapper script writes, and the one place a
    /// hashed default could never match plain docker.
    ///
    /// Measured on Compose v5.3.1: in a directory called `api`, both of
    /// these resolve to the project `api`, so the second really does
    /// stop what the first started. That is docker's own doing, not a
    /// memory of ours: change the directory's name and docker stops
    /// agreeing too, which is exactly what it should do here.
    #[test]
    fn a_p_that_matches_the_directory_is_found_again_by_a_command_without_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("api");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("base.yaml"), "services: {}\n").unwrap();
        std::fs::write(root.join("extra.yaml"), "services: {}\n").unwrap();

        let up = Invocation::capture_in(
            &root,
            &v(&["-p", "api", "-f", "base.yaml", "-f", "extra.yaml", "up"]),
            BTreeMap::new(),
        )
        .unwrap();
        let down = Invocation::capture_in(&root, &v(&["-f", "base.yaml", "down"]), BTreeMap::new())
            .unwrap();
        assert_eq!(up.compose_identity(), down.compose_identity());
        assert_eq!(down.compose_identity(), "api");
    }

    /// …and the other half of being faithful: a `-p` that does NOT match
    /// the directory is not remembered, because docker does not remember
    /// it either.
    ///
    /// Measured: `compose -p chosen up -d` then a plain `compose up -d`
    /// in the same directory starts a SECOND stack, and the `down` after
    /// it removes only that one while `chosen-web-1` keeps running. A
    /// memory here would be ulak quietly addressing containers docker
    /// would have left alone.
    #[test]
    fn a_p_the_directory_does_not_name_is_not_remembered_for_the_next_command() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("myrepo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("compose.yaml"), "services: {}\n").unwrap();

        Invocation::capture_in(&root, &v(&["-p", "chosen", "up", "-d"]), BTreeMap::new()).unwrap();
        let after = Invocation::capture_in(&root, &v(&["down"]), BTreeMap::new()).unwrap();
        assert_eq!(
            after.compose_identity(),
            "myrepo",
            "a remembered -p would make this address `chosen`, which docker never would"
        );
    }

    #[test]
    fn compose_file_env_and_owned_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.yaml"), "services: {}\n").unwrap();
        std::fs::write(root.join("b.yaml"), "services: {}\n").unwrap();

        let env = BTreeMap::from([
            ("COMPOSE_FILE".to_string(), "a.yaml:b.yaml".to_string()),
            ("COMPOSE_PROFILES".to_string(), "dev, debug".to_string()),
            ("COMPOSE_BAKE".to_string(), "true".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ]);
        let inv = Invocation::capture_in(&root, &[], env).unwrap();
        assert_eq!(
            inv.compose_files,
            vec![root.join("a.yaml"), root.join("b.yaml")]
        );
        assert_eq!(inv.profiles, vec!["dev", "debug"]);
        // COMPOSE_FILE is ours (we emit -f); COMPOSE_BAKE travels.
        assert_eq!(
            inv.compose_env.keys().collect::<Vec<_>>(),
            vec!["COMPOSE_BAKE"]
        );
    }

    /// Every owned Compose variable is current invocation input. Missing,
    /// empty and merely forwarded variables must not suppress declaration
    /// recovery for a bare root command.
    #[test]
    fn owned_compose_environment_selects_the_current_invocation() {
        assert!(
            !OWNED_COMPOSE_ENV.is_empty(),
            "the owned set must be tested"
        );
        for name in OWNED_COMPOSE_ENV {
            let env = BTreeMap::from([(name.to_string(), "selected".to_string())]);
            assert!(
                any_owned_compose_environment(|candidate| {
                    env.get(candidate).is_some_and(|value| !value.is_empty())
                }),
                "{name} was not treated as current input"
            );
        }
        let env = BTreeMap::from([
            ("COMPOSE_FILE".to_string(), String::new()),
            ("COMPOSE_BAKE".to_string(), "true".to_string()),
        ]);
        assert!(!any_owned_compose_environment(|candidate| {
            env.get(candidate).is_some_and(|value| !value.is_empty())
        }));
    }

    #[test]
    fn project_dir_follows_the_first_file_and_the_override() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sub = root.join("stack");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.yaml"), "services: {}\n").unwrap();

        let inv = Invocation::capture_in(&root, &v(&["-f", "stack/a.yaml", "up"]), BTreeMap::new())
            .unwrap();
        assert_eq!(inv.project_dir, sub);
        assert_eq!(inv.default_name(), "stack");

        let inv = Invocation::capture_in(
            &root,
            &v(&["-f", "stack/a.yaml", "--project-directory", "."]),
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(inv.project_dir, root);
    }

    /// Docker remembers `-f` no more than it remembers `-p`.
    ///
    /// This is the more dangerous half of a hidden invocation memory: a
    /// first file in another directory also changes Docker's default
    /// project name. Reusing it on a later bare `down` therefore stops a
    /// stack plain Docker would leave alone. The service keeps its own
    /// serialized declaration; the next CLI command must start fresh.
    #[test]
    fn a_file_selection_is_not_remembered_for_the_next_command() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let alternate = root.join("alternate");
        std::fs::create_dir_all(&alternate).unwrap();
        std::fs::write(root.join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(alternate.join("stack.yaml"), "services: {}\n").unwrap();

        let selected = Invocation::capture_in(
            &root,
            &v(&["-f", "alternate/stack.yaml", "up", "-d"]),
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(selected.project_dir, alternate);

        let bare = Invocation::capture_in(&root, &v(&["down"]), BTreeMap::new()).unwrap();
        assert_eq!(bare.compose_files, vec![root.join("compose.yaml")]);
        assert_eq!(bare.project_dir, root);
    }
}
