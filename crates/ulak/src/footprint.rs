//! The compose footprint: exactly which local paths this stack needs.
//!
//! The prototype asked "where is the project root?" — a question docker
//! never asks. Docker mounts a SET of absolute paths; there is no copied
//! tree. So ulak asks the same question docker does, and the answer
//! comes from the same place: `docker compose config` run ON the server.
//!
//! How a local path is told apart from a server path, without parsing a
//! single line of YAML locally: the few files needed to resolve the
//! model are pushed into a bootstrap directory that REPRODUCES THE ABSOLUTE
//! LOCAL PATH (`<boot>/Users/you/repo/compose.yaml`). Then
//!
//!   * anything compose resolves under `<boot>/` was written relative in
//!     the compose file → strip the prefix and you have the local path
//!     back, exactly, however many `../` it climbed;
//!   * anything else (an absolute path, or `~` which compose expands to
//!     the REMOTE home) is server-side, and doctor's SERVER/MISSING
//!     classification owns it.
//!
//! The absolute-path layout is what makes the first rule total: a
//! common-ancestor layout would let `../../shared/x` escape the
//! bootstrap root and be silently misread as a server path.
//!
//! Why the pushed set can be that small — the compose files and the
//! env files, nothing else — is measured: `docker compose config`
//! exits 0 with a bind-mount source, a build context or a `configs:`
//! file that is not there. It refuses only when it cannot READ a file
//! it needs to build the model — `env_file:`, `include:` — and then it
//! names that file, fully resolved, which is what the next round
//! pushes. The whole bootstrap round stands on that asymmetry.
//!
//! The cache also watches the *absence* of the implicit
//! `<project-directory>/.env`. A missing file contributes no ordinary
//! stamp, but its later appearance can change interpolation and the Docker
//! project name. Its present/absent bit is therefore checked alongside the
//! ordinary file stamps. Both states share one cache slot so each transition
//! overwrites the previous state; removing a file cannot revive an older
//! pre-creation cache entry.
//!
//! Foreground model resolution leaves Docker's project-name cascade intact:
//! ulak adds `-p` only when the user supplied one. A service job is resolving
//! the historical model of a stack already declared, so `Project` instead
//! supplies the stored identity as that same override. The command and cache
//! signature both consume `compose_model_project_name`; they must not derive
//! separate answers when `.env` or top-level `name:` changes after `up`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::compose::{self, RefKind};
use crate::config::Project;
use crate::dockerignore::{BuildContext, BuildFilter};
use crate::invocation::{anchor_rel, common_ancestor};
use crate::ssh::{Ssh, sh_quote};
use crate::ui::{self, fail};
use crate::walk;

/// Bumping this invalidates every cached footprint.
const CACHE_VERSION: u32 = 3;
/// Each round is one ssh + one small rsync; compose only ever reports
/// one missing file at a time, so this bounds a pathological chain.
const MAX_ROUNDS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Why {
    ComposeFile,
    EnvFile,
    Volume,
    Build,
    Config,
    Secret,
}

impl Why {
    pub fn label(self) -> &'static str {
        match self {
            Why::ComposeFile => "compose",
            Why::EnvFile => "env_file",
            Why::Volume => "volume",
            Why::Build => "build",
            Why::Config => "config",
            Why::Secret => "secret",
        }
    }

    fn from_kind(kind: RefKind) -> Why {
        match kind {
            RefKind::Volume => Why::Volume,
            RefKind::Build => Why::Build,
            RefKind::Config => Why::Config,
            RefKind::Secret => Why::Secret,
        }
    }
}

/// One local path the stack needs on the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub local: PathBuf,
    /// Directories travel whole (minus gitignore/exclude); files alone.
    pub is_dir: bool,
    /// False when compose named a path that is not there locally.
    pub exists: bool,
    /// True when the path is here but has NOTHING in it. Harmless for a
    /// volume the container writes into; for a read-only mount, a build
    /// context or a config file it means the container will see nothing
    /// — the local face of the ghost-directory disease, which the
    /// prototype reported as a reassuring "SYNC ✓".
    #[serde(default)]
    pub empty: bool,
    pub why: Why,
    pub service: String,
    pub writable: bool,
}

/// A reference that lives on the server, not in the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerRef {
    pub source: String,
    pub why: Why,
    pub service: String,
    pub writable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Footprint {
    /// Deepest directory containing every entry. Determines LAYOUT only:
    /// the workspace reproduces `entry - anchor`, never the anchor's tree.
    pub anchor: PathBuf,
    pub entries: Vec<Entry>,
    pub server_refs: Vec<ServerRef>,
    /// True when an entry IS the anchor (someone mounted the whole
    /// project directory) — then there is nothing to filter down to.
    pub whole_anchor: bool,
    /// The build contexts, kept as PATHS rather than read off `entries`.
    ///
    /// `drop_contained` collapses everything inside a directory that
    /// travels whole into that one directory — on a large monorepo,
    /// several build contexts and many bind mounts became a single
    /// `Why::Build` entry at the anchor. So "apply the rule to build
    /// entries" would have meant "apply it to the whole repo". These two
    /// sets are harvested BEFORE the collapse, which is what makes the
    /// rule a path-level one.
    #[serde(default)]
    pub contexts: Vec<BuildContext>,
    /// Paths the stack needs for a reason a `.dockerignore` has no say
    /// over: a bind mount, a config, a secret, an env_file, a compose
    /// file. Locally a bind mount ignores `.dockerignore` completely, so
    /// the workspace does too.
    #[serde(default)]
    pub pinned: Vec<PathBuf>,
    /// The model that produced this, so callers need not fetch it twice.
    #[serde(default)]
    pub model_json: String,
}

impl Footprint {
    /// Directories whose CONTENTS travel — the only trees walk.rs has to
    /// expand. File entries are named one by one, so walking their
    /// parent would scan a directory ulak does not even sync.
    pub fn sync_dirs(&self) -> Vec<PathBuf> {
        if self.whole_anchor {
            return vec![self.anchor.clone()];
        }
        dedup_sorted(
            self.entries
                .iter()
                .filter(|e| e.is_dir && e.exists)
                .map(|e| e.local.clone())
                .collect(),
        )
    }

    /// Directories the watcher must sit on: the synced trees, plus the
    /// homes of individually-named files (editing compose.yaml has to
    /// trigger a sync too).
    ///
    /// A compose edit can move this set, and nothing here has to notice:
    /// `resolve_cached` stamps the files the model was built from, so
    /// editing one re-resolves and the watched set moves with it. That
    /// used to be a hand-maintained "did a model file change?" check,
    /// and a hand-maintained one is a check that can be wrong.
    pub fn watch_dirs(&self) -> Vec<PathBuf> {
        if self.whole_anchor {
            return vec![self.anchor.clone()];
        }
        let mut dirs = self.sync_dirs();
        dirs.extend(
            self.entries
                .iter()
                .filter(|e| !e.is_dir)
                .filter_map(|e| e.local.parent().map(Path::to_path_buf)),
        );
        // A watched ancestor already covers everything below it.
        let dirs = dedup_sorted(dirs);
        dirs.iter()
            .filter(|d| !dirs.iter().any(|o| *o != **d && d.starts_with(o)))
            .cloned()
            .collect()
    }

    /// What each build context's `.dockerignore` narrows away, and what
    /// it may not touch.
    ///
    /// Read from disk on every call, on purpose: editing `.dockerignore`
    /// has to change the very next sync, and a file read is cheaper than
    /// a staleness check that can be wrong. The footprint cache holds the
    /// paths, never the patterns.
    ///
    /// This does NOT appear in `filter_rules` — the narrowing lands in
    /// the walk's literal exclude list, which sync.rs already ranks ahead
    /// of the footprint's includes. That keeps the local scan cheap too:
    /// a pruned directory is one rsync never descends into and one the
    /// walker never opens.
    pub fn build_filter(&self) -> BuildFilter {
        BuildFilter::new(&self.contexts, &self.pinned)
    }

    /// rsync filter rules that let ONLY the footprint through. rsync
    /// never descends into a directory no rule includes, which is why a
    /// small stack inside a multi-gigabyte monorepo costs a small scan.
    pub fn filter_rules(&self) -> Vec<String> {
        if self.whole_anchor {
            return Vec::new(); // nothing to narrow: the anchor IS the entry
        }
        // Sorted so the rule list is reproducible (and diffable in the
        // audit trail); among includes the order carries no meaning.
        let mut sorted: Vec<&Entry> = self.entries.iter().collect();
        sorted.sort_by(|a, b| a.local.cmp(&b.local));

        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut rules = Vec::new();
        for e in sorted {
            // A path that is not here is not ours to sync. The filter
            // chain is identical in both directions, so including it
            // would also invite whatever the container wrote there to
            // come home on the pull — a real supabase stack references
            // `./volumes/db/data`, postgres' own data directory, without
            // it existing locally (the case behind
            // `a_path_that_is_not_here_gets_no_rule_at_all` below).
            // Docker creates a missing bind source itself; ulak
            // simply stays out of it.
            if !e.exists {
                continue;
            }
            let Some(rel) = anchor_rel(&self.anchor, &e.local) else {
                continue;
            };
            let parts: Vec<&str> = rel.split('/').collect();
            // Every ancestor must be included or rsync never descends.
            for i in 1..parts.len() {
                let dir = parts[..i].join("/");
                if seen.insert(dir.clone()) {
                    rules.push(format!("+ {}/", walk::rsync_filter_pattern(&dir)));
                }
            }
            if !seen.insert(rel.clone()) {
                continue;
            }
            let pattern = walk::rsync_filter_pattern(&rel);
            if e.is_dir {
                rules.push(format!("+ {pattern}/"));
                rules.push(format!("+ {pattern}/***"));
            } else {
                rules.push(format!("+ {pattern}"));
            }
        }
        rules.push("- *".into());
        rules
    }
}

// ─── resolution ─────────────────────────────────────────────────────

/// Resolve, reusing the cached answer while the inputs are unchanged.
pub fn resolve_cached(project: &Project, ssh: &Ssh, dest: &str) -> Result<Footprint> {
    let inputs = input_signature(project, dest);
    let workspace_id = project.workspace_id();
    if let Some(cached) = load_cache(workspace_id, &inputs)
        && cached.version == CACHE_VERSION
        && cached.inputs == inputs
        && implicit_dot_env_matches(&project.inv.project_dir, cached.implicit_dot_env)
        && stamps_match(&cached.stamp)
    {
        return Ok(cached.footprint);
    }
    let (footprint, bootstrapped) = resolve(project, ssh)?;
    // On the MISS only. This is where the answer is computed, and the
    // compose files and env files are the only things that can move it —
    // which is exactly what invalidates the cache. Saying it from the
    // hit path too would turn one fact into a line on every `ps`.
    announce_a_wide_anchor(project, &footprint.anchor);
    store_cache(
        workspace_id,
        &Cached {
            version: CACHE_VERSION,
            inputs,
            destination: dest.to_string(),
            implicit_dot_env: bootstrapped
                .iter()
                .any(|path| path == &implicit_dot_env(&project.inv.project_dir)),
            stamp: bootstrapped.iter().filter_map(stamp_of).collect(),
            footprint: footprint.clone(),
        },
    );
    Ok(footprint)
}

/// Say it when the anchor has climbed out of any one project.
///
/// The climb itself is correct and stays: the anchor must cover every
/// path the stack names, so one `--env-file /etc/stack.env`, or a `-f`
/// pointing at a compose file kept beside the home directory rather
/// than inside the checkout, legitimately lifts the common ancestor to
/// `$HOME` — and two absolute paths in different roots lift it to `/`.
/// `common_ancestor` has no ceiling on purpose; giving it one would
/// refuse a stack that genuinely spans two trees, which is a thing
/// people have.
///
/// What was missing is that it happened in silence, while moving two
/// things at once: the tree the walk scans and the layout reproduced
/// inside the server workspace. It does NOT by itself move the workspace
/// id; `WorkspaceKey` is owned by the namespace plus first compose path. An
/// outside first `-f` can therefore change both for two separate reasons,
/// while an outside `--env-file` changes only the layout. Doctor prints the
/// anchor, but somebody who does not already suspect something has no reason
/// to run it — the same gap `agent::stack_complaint` exists to close for the
/// service. Its next step carries this invocation because the warning may be
/// printed before any declaration exists to recover a nonstandard file from.
fn announce_a_wide_anchor(project: &Project, anchor: &Path) {
    let home = crate::config::home_dir().ok();
    if !anchor_reaches_past_a_project(anchor, home.as_deref()) {
        return;
    }
    ui::warn(&format!(
        "this stack names a path outside its own directory, so the workspace layout starts at {} — only footprint paths travel, laid out relative to that root",
        anchor.display()
    ));
    let doctor = crate::management::root_command(project, &["doctor"]);
    ui::dim(&format!("    see exactly what travels: {doctor}"));
    ui::dim(
        "    to keep it narrow, move that file into the project or reference it by an absolute path the server already has",
    );
}

/// The anchor sits at the filesystem root, or at or above the home
/// directory. Split from the announcement so the rule can be asserted
/// without one — a test cannot set `HOME` for its own process without
/// racing every other test in the binary.
pub fn anchor_reaches_past_a_project(anchor: &Path, home: Option<&Path>) -> bool {
    anchor == Path::new("/") || home.is_some_and(|h| h.starts_with(anchor))
}

/// The full bootstrap round-trip. Returns the footprint plus every file
/// that had to travel to produce it (the cache stamps all of them).
pub fn resolve(project: &Project, ssh: &Ssh) -> Result<(Footprint, Vec<PathBuf>)> {
    // PRIVATE to this invocation, and that is the whole point.
    //
    // One shared `bootstrap/` meant a CLI and the service could both be
    // inside it, and this function's first act is to empty it — so one
    // deleted the other's files and compose then reported a file that
    // was sitting on the user's disk the whole time. Measured on a real
    // upgrade, from the audit trail: two processes on the directory
    // inside one second, the service's rsync out at 23 (files vanished
    // under it) and the CLI's `docker compose config` out at 1.
    //
    // Normally invisible, because a warm cache means this function
    // hardly ever runs — the window opens exactly when the cache is
    // invalidated, i.e. on the version upgrade that introduces it.
    //
    // The pid is enough: the two racers are always processes on THIS
    // machine. Two machines sharing one workspace is a bigger and separate
    // problem — they would be fighting over the workspace itself, not over
    // this directory.
    let boot_rel_dir = format!(
        "{}/bootstrap.{}",
        project.remote_workspace_root(),
        std::process::id()
    );
    let mut set: Vec<PathBuf> = Vec::new();
    for f in &project.inv.compose_files {
        push_unique(&mut set, f.clone());
    }
    for f in &project.inv.env_files {
        push_unique(&mut set, f.clone());
    }
    // Compose auto-loads `.env` from the project directory; it must be
    // there or interpolation silently resolves to empty strings.
    let dot_env = implicit_dot_env(&project.inv.project_dir);
    if dot_env.is_file() {
        push_unique(&mut set, dot_env);
    }

    // Emptied first, because a stale leftover could mask a genuinely
    // missing file — and safe to empty now, because the only thing that
    // could be in a directory named after this process is what this
    // process, or a dead one wearing its number, left there.
    //
    // `umask 077` because this directory is about to hold the user's
    // compose files, and on a FIRST sync it is created before
    // `ensure_workspace` gets round to sealing the parents — the same
    // reason that function opens with it.
    ssh.run_checked(
        &format!(
            "umask 077 && rm -rf {d} && mkdir -p {d} && chmod 700 {d}",
            d = sh_quote(&boot_rel_dir)
        ),
        "preparing the bootstrap directory",
    )?;

    for round in 0..MAX_ROUNDS {
        push_bootstrap(ssh, project, &boot_rel_dir, &set)?;
        let script = format!(
            "cd {d} || exit 9\n\
             echo '==ULAK:root=='; pwd\n\
             echo '==ULAK:model=='\n\
             {cmd}\n",
            d = sh_quote(&boot_rel_dir),
            cmd = bootstrap_config_cmd(project),
        );
        let out = ssh.run_script(&script)?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let (boot_abs, model) = split_two(&stdout);
        let boot_abs = boot_abs.trim().to_string();
        if boot_abs.is_empty() {
            return Err(fail!(
                "the bootstrap directory could not be read on {}:\n    {}",
                ssh.dest,
                stderr.lines().take(4).collect::<Vec<_>>().join("\n    ")
            )
            .now(format!("check the server by hand: ssh {} true", ssh.dest))
            .into_err());
        }
        let boot = Boot { abs: boot_abs };

        if out.status.success() && model.trim_start().starts_with(['{', '[']) {
            let footprint = build(project, &boot, model, &set)?;
            clear_bootstrap(ssh, project, &boot_rel_dir);
            return Ok((footprint, set));
        }

        // Compose names the file it could not open, fully resolved —
        // which back-maps to a local path. Growing the bootstrap set
        // from THAT (never from a guess) keeps every round deterministic.
        let grown: Vec<PathBuf> = missing_local_paths(&stderr, &boot)
            .into_iter()
            .filter(|p| p.is_file() && !set.contains(p))
            .collect();
        // Deliberately NOT cleared on the way out: the error below tells
        // the user to cd into this directory and run compose themselves,
        // and that line is worthless if the directory is gone by the
        // time they paste it.
        if grown.is_empty() || round + 1 == MAX_ROUNDS {
            return Err(model_failure(&boot_rel_dir, ssh, &boot, &stderr));
        }
        for p in grown {
            ui::dim(&format!("bootstrap also needs {}", p.display()));
            set.push(p);
        }
    }
    unreachable!("the loop returns on both exits")
}

/// Take this invocation's bootstrap directory back out, and sweep any an
/// earlier run was killed before removing.
///
/// A DAY is the age test, and the size of the gap is the safety: a
/// resolve lives for seconds, so nothing an hour old — let alone a day —
/// can still be in use by anybody, on this machine or another. Without
/// that gap the sweep would be the very thing this scheme exists to
/// prevent, one process deleting another's work.
///
/// `bootstrap*` and not `bootstrap.*`, so the single shared directory
/// ulak used to keep goes the same way once nothing has touched it
/// for a day.
///
/// Never fatal. The answer is already in hand, and a directory left on
/// the server costs a few kilobytes.
fn clear_bootstrap(ssh: &Ssh, project: &Project, boot_rel_dir: &str) {
    let home = project.remote_workspace_root();
    let _ = ssh.run_checked(
        &format!(
            "rm -rf {d}; \
             find {h} -maxdepth 1 -name 'bootstrap*' -mmin +1440 -exec rm -rf {{}} \\; \
             2>/dev/null; true",
            d = sh_quote(boot_rel_dir),
            h = sh_quote(&home),
        ),
        "clearing the bootstrap directory",
    );
}

/// Remote path ↔ local path, the whole trick in four lines.
struct Boot {
    /// Absolute remote path of the bootstrap root, as the server sees it.
    abs: String,
}

impl Boot {
    /// `/Users/you/repo/x` → `Users/you/repo/x`, relative to the boot dir.
    fn rel_of(local: &Path) -> String {
        local.to_string_lossy().trim_start_matches('/').to_string()
    }

    fn back(&self, remote: &str) -> Option<PathBuf> {
        let rest = remote.strip_prefix(&self.abs)?;
        rest.starts_with('/').then(|| PathBuf::from(rest))
    }
}

/// `docker compose … config --format json`, every path pointing into the
/// bootstrap layout. Same flags the real run will use, so the model that
/// comes back is the model `up` will act on.
///
/// Which is also what the model leaves OUT. Measured: a service behind
/// an inactive profile is dropped from `config` altogether — nothing
/// downstream can so much as name it. Warning about one would take a
/// second `config --profile '*'` call, and that call's exit code and
/// stderr must NEVER reach a footprint decision: the flags here have to
/// stay the flags the run uses.
fn bootstrap_config_cmd(project: &Project) -> String {
    let inv = &project.inv;
    let mut cmd = format!("{}docker compose", compose::env_prefix(&inv.compose_env));
    let mut flag = |name: &str, value: &str| {
        cmd.push(' ');
        cmd.push_str(name);
        cmd.push(' ');
        cmd.push_str(&sh_quote(value));
    };
    // For an ordinary command this is `Some` only when the user typed
    // `-p`; otherwise this call is where Compose gets asked what the
    // project is called. A rebuilt service job carries the one deliberate
    // override: the historical identity of the stack it already owns.
    if let Some(name) = project.compose_model_project_name() {
        flag("-p", name);
    }
    for f in &inv.compose_files {
        flag("-f", &Boot::rel_of(f));
    }
    flag("--project-directory", &Boot::rel_of(&inv.project_dir));
    for p in &inv.profiles {
        flag("--profile", p);
    }
    for e in &inv.env_files {
        flag("--env-file", &Boot::rel_of(e));
    }
    cmd.push_str(" config --format json");
    cmd
}

/// Push the bootstrap files, absolute layout preserved (`-R` from `/`).
fn push_bootstrap(ssh: &Ssh, project: &Project, boot_rel_dir: &str, set: &[PathBuf]) -> Result<()> {
    use std::process::Command;

    let rsync = crate::sync::local_rsync()?;
    let mut cmd = Command::new(&rsync);
    cmd.args([
        "--relative",
        "--links",
        "--perms",
        "--times",
        "--compress",
        "--from0",
        "--files-from=-",
        "--timeout=60",
    ]);
    cmd.arg("-e").arg(ssh.rsync_transport());
    cmd.arg("--");
    cmd.arg("/");
    cmd.arg(format!("{}:{}/", ssh.dest, boot_rel_dir));

    let payload: Vec<u8> = set
        .iter()
        .flat_map(|p| {
            let mut v = Boot::rel_of(p).into_bytes();
            v.push(0);
            v
        })
        .collect();
    let out = crate::proc::run_bounded(&mut cmd, Some(payload), crate::proc::BOOTSTRAP)?;
    crate::audit::record_command("rsync", &cmd, out.status.code());
    if out.timed_out {
        return Err(fail!(
            "sending the compose files to {} took longer than {}s and was stopped",
            ssh.dest,
            crate::proc::BOOTSTRAP.as_secs()
        )
        .now(format!(
            "check the link: ssh -o ConnectTimeout=10 {} true",
            ssh.dest
        ))
        .now("then rerun the command")
        .into_err());
    }
    if !out.status.success() {
        let doctor = crate::management::root_command(project, &["doctor"]);
        return Err(fail!(
            "the compose files could not be sent to {} for resolving:\n    {}",
            ssh.dest,
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .take(4)
                .collect::<Vec<_>>()
                .join("\n    ")
        )
        .now(format!("run the preflight checks: {doctor}"))
        .into_err());
    }
    Ok(())
}

/// Turn the resolved model into the footprint.
fn build(project: &Project, boot: &Boot, model_json: &str, set: &[PathBuf]) -> Result<Footprint> {
    let model = compose::parse_model(model_json)?;
    let mut entries: Vec<Entry> = Vec::new();
    let mut server_refs: Vec<ServerRef> = Vec::new();

    // Everything that had to travel for the model to exist travels for
    // real too — compose parses these files again on every command.
    for f in set {
        entries.push(Entry {
            is_dir: false,
            exists: f.exists(),
            empty: false,
            why: if project.inv.compose_files.contains(f) {
                Why::ComposeFile
            } else {
                Why::EnvFile
            },
            local: f.clone(),
            service: String::new(),
            writable: false,
        });
    }

    for r in compose::local_refs(&model) {
        match boot.back(&r.source) {
            Some(local) => {
                let exists = local.exists();
                let empty = exists
                    && local.is_dir()
                    && std::fs::read_dir(&local).is_ok_and(|mut d| d.next().is_none());
                entries.push(Entry {
                    // A path compose has not created yet is a directory
                    // for mounts and build contexts, a file otherwise —
                    // the same assumption docker makes.
                    is_dir: if exists {
                        local.is_dir()
                    } else {
                        matches!(r.kind, RefKind::Volume | RefKind::Build)
                    },
                    exists,
                    empty,
                    why: Why::from_kind(r.kind),
                    local,
                    service: r.service,
                    writable: r.writable,
                });
            }
            None => server_refs.push(ServerRef {
                source: r.source,
                why: Why::from_kind(r.kind),
                service: r.service,
                writable: r.writable,
            }),
        }
    }

    entries.sort_by(|a, b| a.local.cmp(&b.local).then(a.why.label().cmp(b.why.label())));
    entries.dedup_by(|a, b| a.local == b.local && a.why == b.why && a.service == b.service);
    // Harvested while the entries still say WHY each path is here —
    // `drop_contained` is about to fold most of them into one.
    let contexts = build_contexts(&entries, &dockerfile_map(&model, boot));
    let pinned = pinned_paths(&entries);
    let entries = drop_contained(entries);

    let anchor = common_ancestor(
        &entries
            .iter()
            .map(|e| {
                if e.is_dir {
                    e.local.as_path()
                } else {
                    e.local.parent().unwrap_or(&e.local)
                }
            })
            .collect::<Vec<_>>(),
    );
    let whole_anchor = entries.iter().any(|e| e.is_dir && e.local == anchor);

    Ok(Footprint {
        anchor,
        entries,
        server_refs,
        whole_anchor,
        contexts,
        pinned,
        model_json: model_json.to_string(),
    })
}

/// Context root → the Dockerfile that build reads, on THIS machine.
///
/// The pairing is what lets the filter find `<dockerfile>.dockerignore`,
/// which docker reads in preference to the one at the context root. A
/// dockerfile compose spells relative is resolved against its context; an
/// absolute one is back-mapped like any other path, and one that turns
/// out to live on the SERVER is simply left out — there is nothing on
/// this machine to keep.
fn dockerfile_map(model: &compose::Model, boot: &Boot) -> BTreeMap<PathBuf, PathBuf> {
    let mut out: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for (ctx, df) in compose::builds(model) {
        let Some(root) = boot.back(&ctx) else {
            continue;
        };
        let local = if df.starts_with('/') {
            match boot.back(&df) {
                Some(p) => p,
                None => continue,
            }
        } else {
            root.join(&df)
        };
        out.entry(root).or_insert(local);
    }
    out
}

/// The build contexts that are HERE, each paired with its Dockerfile.
fn build_contexts(
    entries: &[Entry],
    dockerfiles: &BTreeMap<PathBuf, PathBuf>,
) -> Vec<BuildContext> {
    let mut out: Vec<BuildContext> = entries
        .iter()
        .filter(|e| e.why == Why::Build && e.is_dir && e.exists)
        .map(|e| BuildContext {
            dockerfile: dockerfiles.get(&e.local).cloned(),
            root: e.local.clone(),
        })
        .collect();
    out.sort_by(|a, b| a.root.cmp(&b.root));
    out.dedup();
    out
}

/// Every path in the footprint for a reason other than a build.
fn pinned_paths(entries: &[Entry]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = entries
        .iter()
        .filter(|e| e.why != Why::Build)
        .map(|e| e.local.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// A path inside a directory that already travels whole is redundant —
/// and worse, it would emit filter rules that fight the parent's.
fn drop_contained(entries: Vec<Entry>) -> Vec<Entry> {
    let dirs: Vec<PathBuf> = entries
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| e.local.clone())
        .collect();
    entries
        .into_iter()
        .filter(|e| !dirs.iter().any(|d| *d != e.local && e.local.starts_with(d)))
        .collect()
}

fn dedup_sorted(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
    v.sort();
    v.dedup();
    v
}

fn push_unique(set: &mut Vec<PathBuf>, p: PathBuf) {
    if !set.contains(&p) {
        set.push(p);
    }
}

/// Every `<boot>/…` path compose mentioned while failing, back-mapped.
fn missing_local_paths(stderr: &str, boot: &Boot) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for token in stderr.split_whitespace() {
        let token = token.trim_matches(|c| matches!(c, ':' | ',' | '"' | '\'' | '.'));
        if let Some(local) = boot.back(token)
            && !out.contains(&local)
        {
            out.push(local);
        }
    }
    out
}

fn model_failure(boot_rel_dir: &str, ssh: &Ssh, boot: &Boot, stderr: &str) -> anyhow::Error {
    let detail: String = stderr
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(6)
        .map(|l| l.replace(&boot.abs, ""))
        .collect::<Vec<_>>()
        .join("\n    ");
    let named: Vec<String> = missing_local_paths(stderr, boot)
        .iter()
        .filter(|p| !p.exists())
        .map(|p| p.display().to_string())
        .collect();

    // Attribution matters more than the wording: the model is resolved
    // BY COMPOSE, on the server, from the user's own files — so a failure
    // here is the compose file's, never ulak's transport.
    let mut f = fail!(
        "your compose files did not resolve: `docker compose config` on {} rejected them:\n    {detail}",
        ssh.dest
    );
    if !named.is_empty() {
        f = f
            .now(format!(
                "these paths do not exist on this machine: {}",
                named.join(", ")
            ))
            .now("create them, or fix the reference in the compose file");
    } else {
        f = f
            .now("if the file is outside the compose files' own directories (an `include:` or an `env_file:` reached elsewhere), name it explicitly: ulak docker compose --env-file <path> …")
            .now("if it should live on the SERVER, reference it with an absolute path instead of a relative one");
    }
    // The directory named here is the one this invocation actually used,
    // and it is still standing — the success path removes it, the
    // failure path leaves it exactly so this line can be pasted.
    f.now(format!(
        "reproduce it: ssh {} 'cd {boot_rel_dir} && docker compose config'",
        ssh.dest
    ))
    .now("if plain `docker compose config` fails locally too, fix the compose file first — Ulak only relays what compose said")
    .into_err()
}

fn split_two(stdout: &str) -> (&str, &str) {
    let after_root = stdout
        .split_once("==ULAK:root==")
        .map(|(_, r)| r)
        .unwrap_or("");
    after_root.split_once("==ULAK:model==").unwrap_or(("", ""))
}

// ─── cache ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct Cached {
    version: u32,
    /// Everything but file CONTENT that steers the model.
    inputs: String,
    /// Kept separately from the hashed filename so `clean` can retire only
    /// this remote copy's model caches without evicting another destination.
    #[serde(default)]
    destination: String,
    /// The implicit candidate is not in `stamp` while absent. Kept in the
    /// cache value, rather than its path signature, so present → absent cannot
    /// fall back to an older cache entry from before the file was created.
    #[serde(default)]
    implicit_dot_env: bool,
    stamp: Vec<Stamp>,
    footprint: Footprint,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Stamp {
    path: String,
    mtime: u64,
    len: u64,
}

fn implicit_dot_env(project_dir: &Path) -> PathBuf {
    project_dir.join(".env")
}

fn implicit_dot_env_matches(project_dir: &Path, expected: bool) -> bool {
    implicit_dot_env(project_dir).is_file() == expected
}

fn input_signature(project: &Project, dest: &str) -> String {
    let env: BTreeMap<_, _> = project.inv.compose_env.iter().collect();
    format!(
        "{dest}|{}|model-project={:?}|{}|{:?}|{:?}",
        // The LOCAL answer on purpose: the resolved one is what this
        // key guards, so keying on it would ask the cache to know its
        // own contents. An env file or a `name:` that changes the resolved
        // name changes a stamped file; an absent implicit `.env` is validated
        // beside those stamps. The historical model override is its own input
        // because two service jobs may share one checkout.
        project.inv.compose_identity(),
        project.compose_model_project_name(),
        project.inv.flags_line(),
        project.inv.profiles,
        env
    )
}

fn stamp_of(path: &PathBuf) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        path: path.to_string_lossy().into_owned(),
        mtime: meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs(),
        len: meta.len(),
    })
}

fn stamps_match(stamps: &[Stamp]) -> bool {
    !stamps.is_empty()
        && stamps
            .iter()
            .all(|s| stamp_of(&PathBuf::from(&s.path)).is_some_and(|now| now == *s))
}

/// One transported workspace can have several legitimate Compose
/// answers at once: `-p a` and `-p b`, different profiles, or different
/// file overlays. One cache file per input signature keeps those stacks
/// from evicting each other on every service probe.
fn cache_path(workspace_id: &str, inputs: &str) -> Option<PathBuf> {
    let variant = crate::hashid::fnv1a128_hex(inputs.as_bytes());
    Some(
        crate::invocation::state_dir()?
            .join("footprints")
            .join(format!("{workspace_id}-{variant}.json")),
    )
}

fn load_cache(workspace_id: &str, inputs: &str) -> Option<Cached> {
    let bytes = std::fs::read(cache_path(workspace_id, inputs)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The cache holds the RESOLVED compose model — interpolated env
/// VALUES included, which is every secret the stack was handed. It went
/// to disk at 0644, i.e. readable by every account on the machine.
fn store_cache(workspace_id: &str, cached: &Cached) {
    let Some(path) = cache_path(workspace_id, &cached.inputs) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = crate::invocation::private_dir(dir);
    }
    if let Ok(json) = serde_json::to_vec_pretty(cached) {
        // The CLI and service can resolve the same invocation together.
        // Temp + rename means neither can observe the other's truncated
        // JSON, while the 0600 writer keeps interpolated secrets private.
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let written = crate::invocation::write_private(&tmp, &json).is_ok();
        if written && std::fs::rename(&tmp, &path).is_ok() {
            return;
        }
        if tmp.exists() {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

pub fn forget_cache_destination(workspace_id: &str, destination: &str) {
    let Some(root) = crate::invocation::state_dir().map(|dir| dir.join("footprints")) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let prefix = format!("{workspace_id}-");
    for path in entries.flatten().map(|entry| entry.path()) {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(&prefix) || !name.ends_with(".json") {
            continue;
        }
        let cached = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Cached>(&bytes).ok());
        if cached.as_ref().is_none_or(|cached| {
            cached.version != CACHE_VERSION || cached.destination == destination
        }) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_at(root: &Path, args: &[&str]) -> Project {
        std::fs::write(root.join("compose.yaml"), "services: {}\n").unwrap();
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        let inv = crate::invocation::Invocation::capture_in(root, &args, BTreeMap::new()).unwrap();
        let identity = inv.compose_identity();
        let workspace_key = crate::config::WorkspaceKey::from_namespace(
            "footprint-tests",
            inv.workspace_identity_path(),
        )
        .unwrap();
        Project {
            inv,
            config_home: root.to_path_buf(),
            anchor: root.to_path_buf(),
            name: "project".into(),
            identity,
            stack_pin: None,
            config: crate::config::Config::default(),
            workspace_key,
        }
    }

    /// Two Docker projects over one checkout share transported bytes,
    /// but their Compose models may interpolate different paths. They
    /// therefore need separate cache entries instead of evicting each
    /// other on every service probe.
    #[test]
    fn one_workspace_keeps_each_invocations_footprint_cache() {
        let a = cache_path("0123456789abcdef", "project=a").unwrap();
        let b = cache_path("0123456789abcdef", "project=b").unwrap();
        assert_ne!(a, b);
        for path in [a, b] {
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("0123456789abcdef-"));
            assert!(name.ends_with(".json"));
        }
    }

    /// Cleaning server A removes its cached models but must not evict server
    /// B's variants for the same transported checkout. The filename is
    /// intentionally hashed, so the destination stored inside the cache is
    /// the one authoritative selector for this cleanup.
    #[test]
    fn cleaning_one_destination_keeps_the_other_destinations_model_cache() {
        let workspace = format!("cache-destination-{}", std::process::id());
        for (inputs, destination) in [("model-a", "server-a"), ("model-b", "server-b")] {
            store_cache(
                &workspace,
                &Cached {
                    version: CACHE_VERSION,
                    inputs: inputs.to_string(),
                    destination: destination.to_string(),
                    implicit_dot_env: false,
                    stamp: Vec::new(),
                    footprint: fp("/repo", Vec::new()),
                },
            );
        }

        forget_cache_destination(&workspace, "server-a");
        assert!(load_cache(&workspace, "model-a").is_none());
        assert!(load_cache(&workspace, "model-b").is_some());
        forget_cache_destination(&workspace, "server-b");
    }

    /// A missing implicit `.env` has no metadata stamp, so its existence
    /// has to be observed separately. Both states intentionally share one
    /// cache slot: creation invalidates the stored absence, then removal
    /// invalidates the stored presence instead of reviving the old entry.
    ///
    /// Asked of `project.inv.project_dir` — the directory `resolve_cached`
    /// itself passes, and the one Compose reads its implicit `.env` from —
    /// not the tempdir handle this test happens to hold. On macOS those
    /// are two strings for one place (`/var/…` and `/private/var/…`), so a
    /// version asking the convenient one passes while proving nothing
    /// about the path the cache gate actually watches.
    #[test]
    fn the_implicit_dot_env_candidate_invalidates_each_transition() {
        let root = tempfile::tempdir().unwrap();
        let project = project_at(root.path(), &[]);
        let dir = project.inv.project_dir.clone();
        let signature = input_signature(&project, "server");
        let dot_env = implicit_dot_env(&dir);
        assert_eq!(dot_env.parent(), Some(dir.as_path()));
        assert_eq!(dot_env.file_name().unwrap(), ".env");
        assert!(implicit_dot_env_matches(&dir, false));

        std::fs::write(&dot_env, b"COMPOSE_PROJECT_NAME=new\n").unwrap();
        assert_eq!(
            input_signature(&project, "server"),
            signature,
            "existence belongs to cache validation, not a reusable old slot"
        );
        assert!(!implicit_dot_env_matches(&dir, false));
        assert!(implicit_dot_env_matches(&dir, true));

        std::fs::remove_file(&dot_env).unwrap();
        assert!(!implicit_dot_env_matches(&dir, true));
        assert!(implicit_dot_env_matches(&dir, false));
    }

    /// The command that resolves a model and the cache slot holding that
    /// model must consume the same historical project-name pin. Ordinary
    /// invocations still omit `-p`, leaving `.env` and top-level `name:` to
    /// Compose itself.
    #[test]
    fn historical_model_resolution_and_its_cache_share_one_project_name_pin() {
        let root = tempfile::tempdir().unwrap();
        let mut project = project_at(root.path(), &[]);
        let ordinary = bootstrap_config_cmd(&project);
        assert!(
            !ordinary.contains(" -p "),
            "unexpected name pin: {ordinary}"
        );
        let ordinary_signature = input_signature(&project, "server");

        project.pin_existing_stack("server", "historical");
        let historical = bootstrap_config_cmd(&project);
        assert!(
            historical.contains(" -p historical "),
            "the stored identity did not reach Compose config: {historical}"
        );
        assert_ne!(
            input_signature(&project, "server"),
            ordinary_signature,
            "a historical model must not reuse a foreground model's cache"
        );
    }

    /// One `--env-file` or one `-f` outside the checkout lifts the
    /// common ancestor, and the lift is silent: the scan root, the
    /// server-side layout and the workspace identity all move together.
    /// The two shapes worth saying out loud are the ones where the
    /// anchor has stopped being a project at all.
    #[test]
    fn an_anchor_at_the_home_directory_or_the_root_is_no_longer_a_project() {
        let home = Path::new("/home/dev");
        for (anchor, wide, why) in [
            (
                "/home/dev/repo",
                false,
                "the ordinary case, and by far the common one",
            ),
            (
                "/home/dev",
                true,
                "an absolute --env-file beside the checkout lifts it here",
            ),
            ("/home", true, "above the home directory is wider still"),
            (
                "/",
                true,
                "two absolute paths in different roots share only this",
            ),
            (
                "/srv/stacks/app",
                false,
                "a project outside the home directory is still a project",
            ),
        ] {
            assert_eq!(
                anchor_reaches_past_a_project(Path::new(anchor), Some(home)),
                wide,
                "{anchor}: {why}"
            );
        }
        // With no home directory to compare against, the root is still
        // the root — a machine without HOME must not go quiet.
        assert!(anchor_reaches_past_a_project(Path::new("/"), None));
        assert!(!anchor_reaches_past_a_project(Path::new("/srv/app"), None));
    }

    fn entry(local: &str, is_dir: bool) -> Entry {
        Entry {
            local: PathBuf::from(local),
            is_dir,
            exists: true,
            empty: false,
            why: if is_dir {
                Why::Volume
            } else {
                Why::ComposeFile
            },
            service: "web".into(),
            writable: false,
        }
    }

    fn fp(anchor: &str, entries: Vec<Entry>) -> Footprint {
        let whole_anchor = entries
            .iter()
            .any(|e| e.is_dir && e.local == Path::new(anchor));
        Footprint {
            anchor: PathBuf::from(anchor),
            entries,
            server_refs: vec![],
            whole_anchor,
            contexts: vec![],
            pinned: vec![],
            model_json: String::new(),
        }
    }

    #[test]
    fn back_mapping_survives_any_number_of_dot_dots() {
        let boot = Boot {
            abs: "/home/dev/.ulak/workspaces/ab/bootstrap".into(),
        };
        // A sibling reference climbs out of the compose file's dir but
        // NEVER out of the bootstrap root — that is the whole design.
        assert_eq!(
            boot.back("/home/dev/.ulak/workspaces/ab/bootstrap/Users/me/repo/shared/x"),
            Some(PathBuf::from("/Users/me/repo/shared/x"))
        );
        // Server-side paths are left alone.
        assert_eq!(boot.back("/etc/localtime"), None);
        assert_eq!(boot.back("/root/tilde-expanded"), None);
        // A prefix that only LOOKS like the boot dir is not a match.
        assert_eq!(
            boot.back("/home/dev/.ulak/workspaces/ab/bootstrap-x/y"),
            None
        );
        assert_eq!(
            Boot::rel_of(Path::new("/Users/me/a.yaml")),
            "Users/me/a.yaml"
        );
    }

    #[test]
    fn filter_rules_include_ancestors_and_nothing_else() {
        let f = fp(
            "/repo",
            vec![
                entry("/repo/stack", true),
                entry("/repo/api/settings", true),
                entry("/repo/stack/compose.yaml", false),
                entry("/repo/top.env", false),
            ],
        );
        // compose.yaml lives inside stack/ → redundant.
        let f = fp("/repo", drop_contained(f.entries));
        let rules = f.filter_rules();
        assert_eq!(
            rules,
            vec![
                "+ /api/",
                "+ /api/settings/",
                "+ /api/settings/***",
                "+ /stack/",
                "+ /stack/***",
                "+ /top.env",
                "- *",
            ]
        );
    }

    #[test]
    fn a_whole_project_mount_needs_no_narrowing() {
        let f = fp("/repo", vec![entry("/repo", true)]);
        assert!(f.whole_anchor);
        assert!(f.filter_rules().is_empty());
        assert_eq!(f.sync_dirs(), vec![PathBuf::from("/repo")]);
    }

    /// `whole_anchor` is not a defect of its own — it is the honest
    /// reading of "somebody referenced the anchor whole", and WHICH
    /// reference it was decides whether anything may narrow it.
    ///
    /// A build context at the repo root: docker tars the whole thing, so
    /// the only thing that narrows it is the file the user already wrote
    /// for docker. A `.:/app` bind mount at the repo root: docker shows
    /// the container the whole thing, `.dockerignore` never enters the
    /// picture, and neither does ulak. Both cases are exactly what the
    /// user asked docker for — which is why closing this needed no new
    /// concept beyond the one docker already has.
    #[test]
    fn what_narrows_a_whole_anchor_depends_on_why_it_is_whole() {
        let mut ctx = entry("/repo", true);
        ctx.why = Why::Build;
        let files = BTreeMap::from([(PathBuf::from("/repo"), PathBuf::from("/repo/Dockerfile"))]);
        assert_eq!(
            build_contexts(&[ctx], &files),
            vec![BuildContext {
                root: PathBuf::from("/repo"),
                dockerfile: Some(PathBuf::from("/repo/Dockerfile")),
            }],
            "a build context is narrowable — by its own .dockerignore"
        );

        let mut mount = entry("/repo", true);
        mount.why = Why::Volume;
        assert!(
            build_contexts(&[mount.clone()], &BTreeMap::new()).is_empty(),
            "a bind mount is not a build context and nothing narrows it"
        );
        assert_eq!(
            pinned_paths(&[mount]),
            vec![PathBuf::from("/repo")],
            "…it is pinned instead, so no other build's ignore file can touch it"
        );
    }

    #[test]
    fn a_path_that_is_not_here_gets_no_rule_at_all() {
        // Real supabase: ./volumes/db/data does not exist locally, the
        // container fills it on the server. An include rule would make
        // rsync see an empty local side and queue every file postgres
        // wrote for deletion.
        let mut absent = entry("/repo/volumes/db/data", true);
        absent.exists = false;
        let f = fp("/repo", vec![absent, entry("/repo/volumes/api", true)]);
        let rules = f.filter_rules();
        assert!(
            !rules.iter().any(|r| r.contains("db/data")),
            "an absent path must not be included: {rules:?}"
        );
        assert!(
            rules.iter().any(|r| r == "+ /volumes/api/***"),
            "…while the ones that ARE here still travel: {rules:?}"
        );
    }

    #[test]
    fn wildcards_in_names_are_escaped_not_expanded() {
        let f = fp("/repo", vec![entry("/repo/we[ird]/data", true)]);
        let rules = f.filter_rules();
        assert!(
            rules.iter().any(|r| r == "+ /we\\[ird]/"),
            "unescaped brackets would void the include: {rules:?}"
        );
    }

    #[test]
    fn missing_paths_are_extracted_from_compose_prose() {
        let boot = Boot { abs: "/b".into() };
        let stderr = "env file /b/Users/me/repo/missing.env not found: stat \
                      /b/Users/me/repo/missing.env: no such file or directory";
        assert_eq!(
            missing_local_paths(stderr, &boot),
            vec![PathBuf::from("/Users/me/repo/missing.env")]
        );
        let stderr = "open /b/Users/me/shared/inc.yaml: no such file or directory";
        assert_eq!(
            missing_local_paths(stderr, &boot),
            vec![PathBuf::from("/Users/me/shared/inc.yaml")]
        );
    }

    /// The measured shape of a large monorepo, in one assertion.
    ///
    /// Several services build from the repo ROOT and many bind mounts
    /// live inside it, so `drop_contained` folds all of it into entries
    /// that are all `Why::Build` and all AT the anchor. A rule written
    /// against entries would therefore read "narrow the whole repo", and
    /// the mounts it must not touch would be invisible. The two path sets
    /// are taken before the collapse and outlive it — that is what makes
    /// the rule a path-level one.
    #[test]
    fn the_reason_a_path_travels_outlives_the_collapse() {
        let mut ctx_a = entry("/repo", true);
        ctx_a.why = Why::Build;
        let mut ctx_b = entry("/repo", true);
        ctx_b.why = Why::Build;
        ctx_b.service = "second".into();
        let mut mount = entry("/repo/app-core-be/listeners.yml", false);
        mount.why = Why::Volume;
        let compose_file = entry("/repo/compose.yaml", false); // Why::ComposeFile
        let entries = vec![ctx_a, ctx_b, mount, compose_file];

        let dockerfiles = BTreeMap::from([(
            PathBuf::from("/repo"),
            PathBuf::from("/repo/vendor/Dockerfile"),
        )]);
        let contexts = build_contexts(&entries, &dockerfiles);
        let pinned = pinned_paths(&entries);

        // Five builds from one root are ONE thing to filter by.
        assert_eq!(
            contexts,
            vec![BuildContext {
                root: PathBuf::from("/repo"),
                dockerfile: Some(PathBuf::from("/repo/vendor/Dockerfile")),
            }]
        );
        assert_eq!(
            pinned,
            vec![
                PathBuf::from("/repo/app-core-be/listeners.yml"),
                PathBuf::from("/repo/compose.yaml"),
            ]
        );

        // …and here is the collapse the two sets had to survive.
        let collapsed = drop_contained(entries);
        assert!(
            collapsed
                .iter()
                .all(|e| e.local == Path::new("/repo") && e.why == Why::Build),
            "every entry is now the anchor itself, labelled build: {:?}",
            collapsed.iter().map(|e| &e.local).collect::<Vec<_>>()
        );
    }

    #[test]
    fn contained_entries_are_dropped_but_siblings_kept() {
        let kept = drop_contained(vec![
            entry("/repo/app", true),
            entry("/repo/app/Dockerfile", false),
            entry("/repo/apps-other/x", false),
        ]);
        let paths: Vec<_> = kept.iter().map(|e| e.local.display().to_string()).collect();
        // "/repo/apps-other" must NOT be swallowed by "/repo/app".
        assert_eq!(paths, vec!["/repo/app", "/repo/apps-other/x"]);
    }
}
