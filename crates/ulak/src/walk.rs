//! Local tree walk that turns the sync's ignore rules into a LITERAL,
//! NUL-delimitable exclude list for rsync.
//!
//! Why not translate `[sync] exclude` patterns (gitignore syntax) into
//! rsync excludes? Their glob dialects differ in metacharacters,
//! anchoring and negation — a design review confirmed silent
//! divergence. Why not --files-from? Measured: rsync ignores --delete
//! for file lists (and with -r it would re-enumerate ignored dirs
//! itself). So: rsync walks the full tree; we hand it the exact paths
//! to skip.
//!
//! Priority per entry: protect > include (escape hatch) > builtin and
//! config exclude > .dockerignore > keep.
//!
//! `.gitignore` is not on that list, at any depth. Docker never reads
//! it: a bind mount shows the container every file in its source, and a
//! build context is narrowed by its `.dockerignore` alone. Measured with
//! docker 29.6.2 and buildx v0.35.0: beside a `.gitignore` of
//! `secret.txt` and `dist/`, both `COPY . /ctx` (`FROM alpine:3.20`) and
//! `docker run -v "$PWD":/m alpine:3.20 ls -A /m dist` listed
//! `secret.txt` and `dist/bundle.js`. Honouring it put less on the
//! server than the stack reads — a gitignored generated config,
//! certificate or build output that a mount or a `COPY` needs was simply
//! missing there, and only there. What stays local is declared to Ulak,
//! in the same pattern syntax: `[sync] exclude`, which `entry_of` applies
//! to a footprint root as well as to everything below it. Every rule is
//! written from the Compose project directory and reaches the walk
//! anchor-relative through `rebase` (`Project::sync_rules`).
//!
//! `.dockerignore` sits at the bottom because it is the narrowest claim
//! of the four: it speaks only for what a BUILD reads, and only inside a
//! build context. Everything above it — and every path the footprint
//! pins for another reason — overrides it. It lands in the same literal
//! exclude list as `[sync] exclude`, which is what makes a pruned
//! directory one rsync never descends into AND one this walker never
//! opens; see dockerignore.rs for the dialect and why it is not git's.

use std::path::Path;

use anyhow::{Context, Result};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::config::SyncCfg;
use crate::dockerignore::{BuildFilter, Verdict};

/// Built-in excludes, always applied ahead of user config.
/// `.git` is deliberately unanchored: nested checkouts (submodules,
/// monorepo vendoring) must not push their history to the server either.
///
/// The last two are Ulak's own scratch, and each is a GLOB because
/// neither ever appears under a fixed name. `bridge.rs` unpacks a
/// `docker cp` into `.ulak-cp-<pid>-<stamp>/` beside the destination and
/// streams a `docker save` into `<name>.ulak-partial` beside the file
/// asked for — both in the user's own tree, both mid-write. Matching
/// only the literal `.ulak-partial` left both visible to the sync, so a
/// reconcile running alongside a copy could push a half-extracted tree
/// to the server, or delete it out from under the extraction.
const BUILTIN_EXCLUDE: &[&str] = &[
    ".git",
    ".DS_Store",
    ".ulak-partial",
    ".ulak-cp-*",
    "*.ulak-partial",
];

#[derive(Debug, Default)]
pub struct WalkReport {
    /// Paths relative to root that rsync must skip (raw bytes, since
    /// ignored filenames may be non-UTF8 or contain newlines — the
    /// NUL-delimited list carries them safely).
    pub excludes: Vec<Vec<u8>>,
    /// Kept (to-be-synced) paths with names we refuse to sync.
    pub bad_names: Vec<String>,
    /// Anchor-relative paths that ARE synced. This is the ledger: the
    /// answer to "did ulak put this file on the server?", which is the
    /// only thing that separates "you deleted it" from "the container
    /// created it". Every entry is UTF-8 and newline-free — the ones that
    /// are not land in `bad_names` and stop the sync.
    pub kept: Vec<Vec<u8>>,
    /// The DIRECTORIES that are synced, same terms as `kept`.
    ///
    /// rsync creates directories on whichever side receives, so ulak
    /// puts them on the server just as surely as it puts files there —
    /// and the ledger's whole question is "did ulak put this here?".
    /// Without this the answer for a directory was always "no", so an
    /// empty one could never be removed from the workspace and (via
    /// pull_back) colonised every repo that synced with it. Measured on
    /// my-server: `site/hollow/` outlived every sync that followed it.
    pub kept_dirs: Vec<Vec<u8>>,
}

impl WalkReport {
    #[cfg(test)]
    pub fn kept_files(&self) -> usize {
        self.kept.len()
    }
}

/// Walk only `scope`, but speak in `anchor`-relative terms.
///
/// The footprint model needs both: what travels is a handful of
/// directories scattered under the anchor (so walking the anchor itself
/// would defeat the entire point — that is the multi-gigabyte monorepo
/// scan), while every rule the walk applies and every path it reports
/// is anchor-relative, because the anchor IS the workspace root. `cfg`
/// therefore arrives through `rebase`, never as the user wrote it.
pub fn plan_in(
    anchor: &Path,
    scope: &Path,
    cfg: &SyncCfg,
    build: &BuildFilter,
) -> Result<WalkReport> {
    let exclude_matcher = build_matcher(anchor, BUILTIN_EXCLUDE.iter().copied(), &cfg.exclude)?;
    let include_matcher = build_matcher(anchor, [], &cfg.include)?;
    let protect: Vec<String> = cfg
        .protect
        .iter()
        .filter_map(|p| protect_entry(p))
        .collect();

    let rel_scope = scope.strip_prefix(anchor).unwrap_or(Path::new(""));
    let mut report = WalkReport::default();
    let (entry, included) = entry_of(
        &exclude_matcher,
        &include_matcher,
        &protect,
        &cfg.include,
        rel_scope,
    );
    // Below a directory an include named, no `.dockerignore` speaks — the
    // walk from the anchor hands that subtree the same empty filter.
    let unnarrowed = BuildFilter::default();
    let build = if included { &unnarrowed } else { build };
    match entry {
        Entry::Walk => walk_dir(
            anchor,
            rel_scope,
            &exclude_matcher,
            &include_matcher,
            &protect,
            &cfg.include,
            build,
            &mut report,
        )?,
        Entry::Reached => walk_excluded_dir(
            anchor,
            rel_scope,
            &exclude_matcher,
            &include_matcher,
            &protect,
            &cfg.include,
            &mut report,
        )?,
        Entry::Dropped => report.excludes.push(rel_bytes(rel_scope)),
        Entry::Protected => {}
    }
    Ok(report)
}

/// How a walk from the anchor arrives at a directory.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    /// Entered normally: `walk_dir`.
    Walk,
    /// Excluded, but entered because an include reaches inside:
    /// `walk_excluded_dir`.
    Reached,
    /// Never entered.
    Dropped,
    /// Server-owned: never entered and never named, since rsync's own
    /// `P` rules keep it, as for a protect path below a walked directory.
    Protected,
}

/// The verdict `walk_dir` and `walk_excluded_dir` would reach for
/// `rel_scope` itself and every directory above it, so a walk that
/// STARTS there answers as one from the anchor would. Two callers need
/// that: a footprint root, which no walk above it ever judges — without
/// this, `exclude = ["logs/"]` sent a mounted `./logs` whole while doctor
/// called it excluded — and `sync::rescanned_plan`, whose splice is only
/// sound if the rescanned directory is walked the way the full walk
/// walked it. The `.dockerignore` stays with the walk itself: a footprint
/// root is never narrowed by the context around it. The second answer is
/// whether an include named `rel_scope` or a directory above it, below
/// which no `.dockerignore` speaks.
fn entry_of(
    exclude: &Gitignore,
    include: &Gitignore,
    protect: &[String],
    include_lines: &[String],
    rel_scope: &Path,
) -> (Entry, bool) {
    let mut entry = Entry::Walk;
    let mut below_include = false;
    let mut partial = std::path::PathBuf::new();
    for comp in rel_scope.components() {
        partial.push(comp);
        if protect
            .iter()
            .any(|p| protect_covers(p, &partial.to_string_lossy()))
        {
            return (Entry::Protected, below_include);
        }
        // An include always enters; otherwise only a walked parent with no
        // exclude on this directory does — `walk_excluded_dir` enters
        // nothing that is not included or reached.
        let included = matches!(include.matched(&partial, true), Match::Ignore(_));
        let excluded = matches!(exclude.matched(&partial, true), Match::Ignore(_));
        below_include |= included;
        entry = if included || (entry == Entry::Walk && !excluded) {
            Entry::Walk
        } else if include_reaches_into(include_lines, &partial.to_string_lossy()) {
            Entry::Reached
        } else {
            return (Entry::Dropped, below_include);
        };
    }
    (entry, below_include)
}

/// Would the ignore rules swallow this path outright?
///
/// The footprint names paths the compose file asked for, but the ignore
/// rules still win (sync.rs documents why: pushing an excluded `./data`
/// over a live database is the worse failure). So doctor has to be able
/// to say WHICH referenced path silently will not travel — otherwise the
/// server just grows an empty directory where a mount should be. A
/// directory an include reaches into is not swallowed: part of it travels.
pub fn is_ignored(anchor: &Path, path: &Path, cfg: &SyncCfg, build: &BuildFilter) -> Result<bool> {
    let Ok(rel) = path.strip_prefix(anchor) else {
        return Ok(false);
    };
    let exclude = build_matcher(anchor, BUILTIN_EXCLUDE.iter().copied(), &cfg.exclude)?;
    let include = build_matcher(anchor, [], &cfg.include)?;
    let protect: Vec<String> = cfg
        .protect
        .iter()
        .filter_map(|p| protect_entry(p))
        .collect();

    let mut dir = anchor.to_path_buf();
    let mut partial = std::path::PathBuf::new();
    let mut below_include = false;
    // Inside a dropped directory an include reaches into, where only an
    // include or a deeper reach keeps anything (`walk_excluded_dir`).
    let mut reached = false;
    let comps: Vec<_> = rel.components().collect();
    for (i, comp) in comps.iter().enumerate() {
        partial.push(comp);
        dir.push(comp);
        let deeper_to_go = i + 1 < comps.len();
        let is_dir = deeper_to_go || dir.is_dir();
        let rel_str = partial.to_string_lossy();

        // The walker's precedence and reach (`walk_dir`): protect >
        // include > exclude > `.dockerignore`, and a directory either of
        // the last two drops is still entered when an include reaches into
        // it. Asked about a path under one, answering at the first dropped
        // ancestor called `secrets/prod.env` excluded while it travelled,
        // and doctor said a mounted `vendor/lib/` would arrive empty.
        if protect.iter().any(|p| protect_covers(p, &rel_str)) {
            return Ok(true);
        }
        if matches!(include.matched(&partial, is_dir), Match::Ignore(_)) {
            below_include |= is_dir;
            reached = false;
            continue;
        }
        // A build context can sit inside another one, and the outer
        // `.dockerignore` can drop it. Nothing else here is at risk: a
        // context is never judged by its own file, and everything the
        // stack needs for another reason is pinned. Below a directory an
        // include named it has no say, as in the walk.
        let dropped = reached
            || matches!(exclude.matched(&partial, is_dir), Match::Ignore(_))
            || (!below_include
                && matches!(
                    build.verdict(&dir, is_dir),
                    Verdict::Drop { descend: false }
                ));
        if dropped {
            if is_dir && include_reaches_into(&cfg.include, &rel_str) {
                reached = true;
                continue;
            }
            return Ok(true);
        }
    }
    Ok(false)
}

/// Is this anchor-relative path server-owned?
///
/// The walker normalises `[sync] protect` once and asks the question
/// inline, and so does `is_ignored`. It is spelled out here as well
/// because `sync::checked_plan` has no walk to normalise for and still
/// has to ask: the files it names ONE BY ONE — a compose file, an env
/// file, a config — never pass through `walk_dir`, which is the only
/// place that would have refused them.
///
/// Entries arrive as the user typed them — `./data`, `data/` and `data`
/// all name one tree — and an entry covers its own subtree, which is
/// what makes `data/config.json` server-owned under `protect =
/// ["data/"]`.
pub fn protected(protect: &[String], rel: &str) -> bool {
    protect
        .iter()
        .filter_map(|p| protect_entry(p))
        .any(|p| protect_covers(&p, rel))
}

/// One reading of a `[sync] protect` entry, for every place that has to
/// agree about what it names. `None` for an entry that names nothing.
///
/// A leading `/` is stripped rather than refused. These entries are
/// anchor-relative (`rebase` made them so) and already anchored — `data`
/// names the tree at the root and never `api/data` — so `/data` says what the entry meant
/// anyway, and the keys either side of it in the same table (`exclude`,
/// `include`) are gitignore patterns where that slash is both legal and
/// exactly this meaning. The config file trains the habit; refusing it
/// would only move the surprise.
///
/// Leaving it in voided the protection twice over, both measured:
/// `protected` compares against paths that never begin with a slash, so
/// the walker claimed the tree into the ledger and one stale local
/// delete put `rm -f` on the server's only copy — the very chain
/// `checked_plan`'s gate exists to stop. And `rsync_exclude_pattern`
/// prepends its own `/`, so the rule went out as `P //data`, which
/// matches nothing: the push then overwrote the live copy it was meant
/// to leave alone.
pub fn protect_entry(raw: &str) -> Option<String> {
    let parts: Vec<&str> = raw
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The one rule for what a normalised entry reaches: itself, and its
/// own subtree. A prefix is not a parent — `data` leaves `data2` alone.
pub fn protect_covers(entry: &str, rel: &str) -> bool {
    rel.strip_prefix(entry)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `[sync]` as the walk, every guard and every rsync rule read it:
/// anchor-relative.
///
/// The rules are written beside the compose file and mean that folder,
/// but the anchor is whatever the active model reaches — a profile that
/// mounts `../frontend-web` lifts it a level, and the workspace record
/// keeps it there for every run after. Read from the anchor as written,
/// `protect = ["data/"]` named a `data/` BESIDE the project and stopped
/// protecting the database inside it, silently, and only under some
/// profiles. So every rule is read from `base`, the project directory,
/// and respelled from the anchor here, once:
///
///   * a protect entry is a path: `..` reaches a sibling the stack
///     mounts, and an entry that ends up outside the anchor names
///     nothing that travels, so it is dropped;
///   * a pattern with a slash — gitignore's anchored kind — has its
///     leading `./` and `../` resolved the same way and keeps its glob;
///   * a pattern without one, or led by `**/`, already matches at any
///     depth and passes through untouched: `node_modules` is meant in a
///     mounted sibling too.
///
/// Nothing changes when the anchor IS the project directory — every
/// project that names nothing outside itself.
///
/// The project directory and every folder between it and a wider anchor
/// are where the rules are written FROM, never what they judge: a walk
/// that starts at the project directory never asks about them, so one
/// from a wider anchor does not either. Left to the patterns, `exclude =
/// ["api"]` dropped a project folder called `api` whole the moment a
/// profile mounted a sibling. A trailing `!` line per folder keeps them
/// out of every exclude.
///
/// A project directory ABOVE the anchor — `-f docker/compose.yaml` whose
/// every reference sits in `docker/` — is read the same way: what climbs
/// into the anchor keeps its meaning, and the rest names nothing that
/// travels. Only one the anchor does not touch at all (a
/// `--project-directory` elsewhere, or one folder spelled two ways) keeps
/// the rules as written: respelled from there, every anchored rule would
/// name nothing, and a protect entry that names nothing is protection
/// lost. Read as written from the anchor, `protect = ["docker/pgdata/"]`
/// named `docker/docker/pgdata` and the database travelled.
pub fn rebase(cfg: &SyncCfg, base: &Path, anchor: &Path) -> SyncCfg {
    if !base.starts_with(anchor) && !anchor.starts_with(base) {
        return cfg.clone();
    }
    let patterns = |lines: &[String]| {
        lines
            .iter()
            .filter_map(|line| rebase_pattern(line, base, anchor))
            .collect::<Vec<_>>()
    };
    let mut exclude = patterns(&cfg.exclude);
    exclude.extend(writing_folders(base, anchor).map(|rel| format!("!/{}/", escape_glob(&rel))));
    SyncCfg {
        exclude,
        include: patterns(&cfg.include),
        protect: cfg
            .protect
            .iter()
            .filter_map(|entry| rebase_protect(entry, base, anchor))
            .collect(),
        max_delete: cfg.max_delete,
        no_history: cfg.no_history,
    }
}

/// The spelling a `[sync]` entry needs to name this anchor-relative
/// path: from the project directory, `../` and all. What doctor tells
/// somebody to write, so `rebase` reads it back as `rel`.
pub fn as_written(base: &Path, anchor: &Path, rel: &str) -> String {
    if !base.starts_with(anchor) && !anchor.starts_with(base) {
        return rel.to_string(); // `rebase` keeps these rules as written
    }
    let target = anchor.join(rel);
    let mut from = base.to_path_buf();
    let mut up = String::new();
    while !target.starts_with(&from) {
        if !from.pop() {
            return rel.to_string();
        }
        up.push_str("../");
    }
    match target.strip_prefix(&from).ok().and_then(Path::to_str) {
        Some(down) => format!("{up}{down}"),
        None => rel.to_string(),
    }
}

fn rebase_protect(entry: &str, base: &Path, anchor: &Path) -> Option<String> {
    let mut at = base.to_path_buf();
    for part in entry.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                at.pop();
            }
            name => at.push(name),
        }
    }
    if base.starts_with(&at) {
        return None; // the project directory itself, or a folder above it
    }
    match at.strip_prefix(anchor).ok()?.to_str() {
        Some("") => None,
        Some(rel) => Some(rel.to_string()),
        // A folder name no rule string can spell keeps the literal reading.
        None => protect_entry(entry),
    }
}

fn rebase_pattern(line: &str, base: &Path, anchor: &Path) -> Option<String> {
    if line.starts_with('#') || line.trim().is_empty() {
        return Some(line.to_string());
    }
    let (bang, body) = match line.strip_prefix('!') {
        Some(rest) => ("!", rest),
        None => ("", line),
    };
    let (rooted, body) = match body.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let core = body.trim_end();
    let core = core.strip_suffix('/').unwrap_or(core);
    if (!rooted && !core.contains('/')) || core == "**" || core.starts_with("**/") {
        return Some(line.to_string());
    }

    let mut at = base.to_path_buf();
    let mut rest = body;
    loop {
        rest = rest.trim_start_matches('/');
        if let Some(r) = rest.strip_prefix("../") {
            at.pop();
            rest = r;
        } else if rest == ".." {
            at.pop();
            rest = "";
        } else if let Some(r) = rest.strip_prefix("./") {
            rest = r;
        } else if rest == "." {
            rest = "";
        } else {
            break;
        }
    }

    let prefix = if let Ok(below) = at.strip_prefix(anchor) {
        let Some(below) = below.to_str() else {
            // A folder name no pattern can spell keeps the literal reading.
            return Some(line.to_string());
        };
        below.to_string()
    } else {
        // Climbed past the anchor: only a pattern that walks back into it
        // reaches anything that travels — by name, by a glob that matches
        // the name, or by a `**` that matches every folder left to climb
        // and any depth below the anchor.
        let above = anchor.strip_prefix(&at).ok()?;
        for name in above.components() {
            let (head, tail) = rest.split_once('/')?;
            if head == "**" {
                rest = tail;
                if rest.trim_end().trim_end_matches('/').is_empty() {
                    return None;
                }
                return Some(format!("{bang}/**/{rest}"));
            }
            if !name
                .as_os_str()
                .to_str()
                .is_some_and(|name| names(head, name))
            {
                return None;
            }
            rest = tail;
        }
        String::new()
    };
    if rest.trim_end().trim_end_matches('/').is_empty() {
        return None; // the project directory itself, or a folder above it
    }
    let mut out = format!("{bang}/{}", escape_glob(&prefix));
    if !prefix.is_empty() {
        out.push('/');
    }
    out.push_str(rest);
    Some(out)
}

/// Whether one component of a gitignore pattern matches a folder name,
/// read by the same matcher that reads the pattern.
fn names(glob: &str, name: &str) -> bool {
    if !glob.contains(['*', '?', '[', '{', '\\']) {
        return glob == name;
    }
    let mut b = GitignoreBuilder::new("/");
    b.add_line(None, &format!("/{glob}")).is_ok()
        && b.build()
            .is_ok_and(|m| m.matched(Path::new("/").join(name), true).is_ignore())
}

/// A literal folder path as a gitignore pattern has to spell it.
fn escape_glob(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']' | '{' | '}') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The folders from just below the anchor down to the project directory,
/// anchor-relative — where `[sync]` is written from (`rebase`). None when
/// the project directory is the anchor or sits above it; a name no
/// pattern can spell is skipped.
fn writing_folders<'a>(base: &'a Path, anchor: &'a Path) -> impl Iterator<Item = String> + 'a {
    let mut partial = std::path::PathBuf::new();
    base.strip_prefix(anchor)
        .into_iter()
        .flat_map(Path::components)
        .filter_map(move |c| {
            partial.push(c);
            partial.to_str().map(str::to_string)
        })
}

fn build_matcher<'a>(
    root: &Path,
    builtin: impl IntoIterator<Item = &'a str>,
    lines: &[String],
) -> Result<Gitignore> {
    let mut b = GitignoreBuilder::new(root);
    for line in builtin {
        b.add_line(None, line).context("bad builtin pattern")?;
    }
    for line in lines {
        b.add_line(None, line)
            .with_context(|| format!("bad pattern in ulak config: {line:?}"))?;
    }
    Ok(b.build()?)
}

#[allow(clippy::too_many_arguments)]
fn walk_dir(
    root: &Path,
    rel_dir: &Path,
    exclude: &Gitignore,
    include: &Gitignore,
    protect: &[String],
    include_lines: &[String],
    build: &BuildFilter,
    report: &mut WalkReport,
) -> Result<()> {
    let abs_dir = root.join(rel_dir);
    let mut entries = read_dir_entries(&abs_dir)?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        let rel = rel_dir.join(&name);
        // Symlinks are leaves: rsync copies them as links, never follows.
        let file_type = entry
            .file_type()
            .with_context(|| format!("cannot stat {}", entry.path().display()))?;
        let is_dir = file_type.is_dir() && !file_type.is_symlink();

        let rel_str = rel.to_string_lossy();

        // 1. protect: server-owned; rsync gets P/- filter rules, the
        //    walker simply never enters.
        if protect.iter().any(|p| protect_covers(p, &rel_str)) {
            continue;
        }

        // 2. include escape hatch: a path an exclude or a `.dockerignore`
        //    would drop, and the stack needs anyway. A directory takes its
        //    whole subtree back from the `.dockerignore`, which would
        //    otherwise drop every child through its dropped parent; an
        //    exclude is still asked about each child on its own.
        if matches!(include.matched(&rel, is_dir), Match::Ignore(_)) {
            if is_dir {
                keep_dir(&rel, report);
                walk_dir(
                    root,
                    &rel,
                    exclude,
                    include,
                    protect,
                    include_lines,
                    &BuildFilter::default(),
                    report,
                )?;
            } else {
                keep_file(&rel, report);
            }
            continue;
        }

        // 3. builtin + config excludes.
        if matches!(exclude.matched(&rel, is_dir), Match::Ignore(_)) {
            // Don't prune a dir that may hide a force-included path.
            if is_dir && include_reaches_into(include_lines, &rel_str) {
                walk_excluded_dir(root, &rel, exclude, include, protect, include_lines, report)?;
            } else {
                report.excludes.push(rel_bytes(&rel));
            }
            continue;
        }

        // 4. the build context's own `.dockerignore`. A DIRECTORY it
        // drops is still entered when something inside may yet be wanted
        // — docker's own tar walker makes the same exception, and the
        // repos compose mounts config out of depend on it. Entering
        // one is exactly the normal keep path: every child is judged
        // here on its own terms anyway. An include reaches into one the
        // way it reaches into an excluded directory.
        if let Verdict::Drop { descend } = build.verdict(&abs_dir.join(&name), is_dir)
            && !(is_dir && descend)
        {
            if is_dir && include_reaches_into(include_lines, &rel_str) {
                walk_excluded_dir(root, &rel, exclude, include, protect, include_lines, report)?;
            } else {
                report.excludes.push(rel_bytes(&rel));
            }
            continue;
        }

        if is_dir {
            keep_dir(&rel, report);
            walk_dir(
                root,
                &rel,
                exclude,
                include,
                protect,
                include_lines,
                build,
                report,
            )?;
        } else {
            keep_file(&rel, report);
        }
    }
    Ok(())
}

/// Inside an excluded dir kept alive only for force-includes: everything
/// not include-matched is excluded individually. No `.dockerignore` is
/// asked anything here: an include outranks it, and nothing else is kept.
fn walk_excluded_dir(
    root: &Path,
    rel_dir: &Path,
    exclude: &Gitignore,
    include: &Gitignore,
    protect: &[String],
    include_lines: &[String],
    report: &mut WalkReport,
) -> Result<()> {
    let abs_dir = root.join(rel_dir);
    let mut entries = read_dir_entries(&abs_dir)?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let rel = rel_dir.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("cannot stat {}", entry.path().display()))?;
        let is_dir = file_type.is_dir() && !file_type.is_symlink();
        let rel_str = rel.to_string_lossy();

        // Server-owned outranks an include here too, as in `walk_dir`:
        // never entered, so never claimed.
        if protect.iter().any(|p| protect_covers(p, &rel_str)) {
            continue;
        }

        if matches!(include.matched(&rel, is_dir), Match::Ignore(_)) {
            if is_dir {
                keep_dir(&rel, report);
                walk_dir(
                    root,
                    &rel,
                    exclude,
                    include,
                    protect,
                    include_lines,
                    &BuildFilter::default(),
                    report,
                )?;
            } else {
                keep_file(&rel, report);
            }
        } else if is_dir && include_reaches_into(include_lines, &rel_str) {
            walk_excluded_dir(root, &rel, exclude, include, protect, include_lines, report)?;
        } else {
            report.excludes.push(rel_bytes(&rel));
        }
    }
    Ok(())
}

fn read_dir_entries(abs_dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let listed: std::io::Result<Vec<_>> = match std::fs::read_dir(abs_dir) {
        Ok(iter) => iter.collect(),
        Err(e) => Err(e),
    };
    listed.map_err(|e| {
        crate::ui::fail!("cannot read directory {}: {e}", abs_dir.display())
            .now("check its permissions, or exclude it in ulak.toml [sync] exclude")
            .into_err()
    })
}

/// Could an include pattern match something INSIDE this excluded dir?
///
/// Only slash-containing (anchored) patterns reach into excluded dirs:
/// their literal prefix (up to the first wildcard) is compared against
/// the dir. Bare-name patterns like ".env" deliberately do NOT — they
/// would force descending into every node_modules-sized tree; rescuing
/// a file from an ignored dir takes an anchored path ("config/prod.env",
/// "config/*.env") or an explicit deep glob ("**/prod.env").
fn include_reaches_into(include_lines: &[String], rel_dir: &str) -> bool {
    include_lines.iter().any(|raw| {
        let line = raw.trim_end_matches('/');
        let body = line.strip_prefix('/').unwrap_or(line);
        let anchored = line.starts_with('/') || body.contains('/');
        if !anchored {
            return false;
        }
        let prefix = literal_head(body);
        let dir_slash = format!("{rel_dir}/");
        prefix.starts_with(&dir_slash) || dir_slash.starts_with(&prefix)
    })
}

/// A pattern's literal start, up to its first wildcard, with escapes
/// undone — `rebase` escapes a folder name it puts in front.
fn literal_head(pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(next) => out.push(next),
                None => break,
            },
            '*' | '?' | '[' | '{' => break,
            c => out.push(c),
        }
    }
    out
}

/// Escape a protect path for use inside an rsync filter RULE ("P /x",
/// "- /x"). Same conditional escaping as the exclude list — an
/// unescaped `[` would turn the user's protected dir into a character
/// class and silently void the "never deleted" promise.
pub fn rsync_filter_pattern(path: &str) -> String {
    String::from_utf8(rsync_exclude_pattern(path.as_bytes()))
        .expect("escaping only inserts ASCII backslashes")
}

fn keep_file(rel: &Path, report: &mut WalkReport) {
    match rel.to_str() {
        Some(s) if !s.contains(['\n', '\r']) => report.kept.push(rel_bytes(rel)),
        Some(s) => report.bad_names.push(s.escape_default().to_string()),
        None => report
            .bad_names
            .push(rel.to_string_lossy().into_owned() + " (non-UTF8 name)"),
    }
}

/// A directory the walker is about to descend into: synced, therefore
/// ulak's to remember.
///
/// Recorded at the DESCENT, never for the scope root itself — a
/// footprint root that is gone from disk is docker's to recreate, not
/// ulak's to delete (the same rule that keeps a missing bind source
/// out of the filter rules). A bad name is dropped here rather than
/// reported, but it is no less fatal than a file's: the directory is
/// still walked, and every child inherits the bad byte in its
/// anchor-relative path, so `keep_file` sends all of them to
/// `bad_names` — which `sync::checked_plan` turns into a hard error that
/// stops the sync. Nothing under such a directory travels; only an
/// empty one gets through, unsynced and unmentioned.
fn keep_dir(rel: &Path, report: &mut WalkReport) {
    if let Some(s) = rel.to_str()
        && !s.contains(['\n', '\r'])
        && !s.is_empty()
    {
        report.kept_dirs.push(rel_bytes(rel));
    }
}

fn rel_bytes(rel: &Path) -> Vec<u8> {
    rel.as_os_str().as_encoded_bytes().to_vec()
}

/// Escape a literal path for use as an anchored rsync exclude pattern.
/// rsync treats backslash as an escape only when the pattern contains a
/// wildcard character, so escaping must be conditional.
pub fn rsync_exclude_pattern(rel: &[u8]) -> Vec<u8> {
    let mut out = vec![b'/'];
    if rel.iter().any(|b| matches!(b, b'*' | b'?' | b'[')) {
        for &b in rel {
            if matches!(b, b'*' | b'?' | b'[' | b'\\') {
                out.push(b'\\');
            }
            out.push(b);
        }
    } else {
        out.extend_from_slice(rel);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn cfg(exclude: &[&str], include: &[&str], protect: &[&str]) -> SyncCfg {
        SyncCfg {
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
            include: include.iter().map(|s| s.to_string()).collect(),
            protect: protect.iter().map(|s| s.to_string()).collect(),
            max_delete: crate::config::DeleteBudget::Limit(25),
            no_history: crate::config::NoHistory::Send,
        }
    }

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }

    /// The everyday walk: no build context in sight, so `.dockerignore`
    /// has nothing to say.
    fn plan(root: &Path, cfg: &SyncCfg) -> WalkReport {
        plan_in(root, root, cfg, &BuildFilter::default()).unwrap()
    }

    fn excludes_as_strings(r: &WalkReport) -> Vec<String> {
        let mut v: Vec<String> = r
            .excludes
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        v.sort();
        v
    }

    fn kept_as_strings(r: &WalkReport) -> Vec<String> {
        let mut v: Vec<String> = r
            .kept
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        v.sort();
        v
    }

    /// Docker never reads `.gitignore`: a bind mount shows the container
    /// every one of these files. So the walk keeps them all, at any
    /// depth and whatever the file says — a bare `*` included, which
    /// would have emptied the workspace had any of it been honoured.
    #[test]
    fn a_gitignore_has_no_say_in_what_travels() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "keep.txt");
        touch(root, ".env");
        touch(root, "node_modules/pkg/big.js");
        touch(root, "sub/target/out.bin");
        touch(root, "sub/src.rs");
        fs::write(root.join(".gitignore"), "node_modules/\n.env\n*\n").unwrap();
        fs::write(root.join("sub/.gitignore"), "target/\n").unwrap();

        let report = plan(root, &cfg(&[], &[], &[]));
        assert!(
            report.excludes.is_empty(),
            "{:?}",
            excludes_as_strings(&report)
        );
        assert_eq!(
            kept_as_strings(&report),
            vec![
                ".env",
                ".gitignore",
                "keep.txt",
                "node_modules/pkg/big.js",
                "sub/.gitignore",
                "sub/src.rs",
                "sub/target/out.bin",
            ]
        );
        assert!(report.bad_names.is_empty());
    }

    /// What stays local is declared in `[sync] exclude`, in gitignore's
    /// pattern syntax — and `[sync] include` takes a path back out of it.
    #[test]
    fn an_exclude_keeps_paths_local_and_an_include_takes_one_back() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "keep.txt");
        touch(root, ".env");
        touch(root, "node_modules/pkg/big.js");
        touch(root, "sub/target/out.bin");
        touch(root, "sub/src.rs");

        let report = plan(
            root,
            &cfg(&["node_modules/", ".env", "target/"], &[".env"], &[]),
        );
        assert_eq!(
            excludes_as_strings(&report),
            vec!["node_modules".to_string(), "sub/target".to_string()]
        );
        assert_eq!(
            kept_as_strings(&report),
            vec![".env", "keep.txt", "sub/src.rs"]
        );
    }

    #[test]
    fn an_exclude_negation_takes_a_file_back() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "logs/app.log");
        touch(root, "logs/KEEP.log");

        let report = plan(root, &cfg(&["*.log", "!KEEP.log"], &[], &[]));
        assert_eq!(
            excludes_as_strings(&report),
            vec!["logs/app.log".to_string()]
        );
        assert_eq!(kept_as_strings(&report), vec!["logs/KEEP.log"]);
    }

    /// The same question the walker asks inline, asked by name — because
    /// `sync::checked_plan` has to ask it too, about files that never
    /// pass through a walk at all (a compose file, an env file, a config
    /// named one by one). A claim on server-owned data is a promise ulak
    /// can only half keep: the filter rule stops the push, nothing stops
    /// the delete.
    #[test]
    fn a_protect_entry_covers_its_own_subtree_however_it_was_spelled() {
        for spelling in [
            vec!["data".to_string()],
            vec!["data/".to_string()],
            vec!["./data".to_string()],
            vec!["./data/".to_string()],
            // The gitignore habit, and the one that used to protect
            // NOTHING: `exclude` and `include` sit in the same table and
            // are gitignore patterns, where a leading slash means
            // exactly what it means here — anchored at the root. Left
            // in, it matched no anchor-relative path (none begins with a
            // slash), so the walker claimed the tree into the ledger and
            // rsync got `P //data`, which protects nothing either.
            vec!["/data".to_string()],
            vec!["/data/".to_string()],
            vec!["//data".to_string()],
        ] {
            assert!(protected(&spelling, "data"), "{spelling:?}");
            assert!(protected(&spelling, "data/config.json"), "{spelling:?}");
            assert!(protected(&spelling, "data/pg/base/1"), "{spelling:?}");
            // A prefix is not a parent: `data2` is somebody else's.
            assert!(!protected(&spelling, "data2"), "{spelling:?}");
            assert!(!protected(&spelling, "data2/x"), "{spelling:?}");
            assert!(!protected(&spelling, "code.py"), "{spelling:?}");
        }
        // An empty entry protects nothing rather than everything — the
        // filter the walker builds drops it for the same reason.
        assert!(!protected(&["".to_string(), "/".to_string()], "anything"));
        assert!(!protected(&[], "anything"));
    }

    #[test]
    fn protect_paths_are_never_visited() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "data/db.sqlite");
        touch(root, "code.py");

        let report = plan(root, &cfg(&[], &[], &["data/"]));
        // protect is not an exclude entry: rsync-level P/- rules own it
        assert!(excludes_as_strings(&report).is_empty());
        assert_eq!(report.kept_files(), 1);
    }

    /// The walk answers "what is in the workspace", and directories are in
    /// it — rsync creates one on the receiver for every push. Recording
    /// them is what lets an empty directory the user removed leave the
    /// server; without it, one sat there for good and the pull copied it
    /// into the repo, where ulak may delete nothing.
    #[test]
    fn directories_are_reported_so_they_can_be_retired() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "site/deep/page.html");
        std::fs::create_dir_all(root.join("site/hollow")).unwrap();
        touch(root, "data/db.sqlite");
        touch(root, "big/blob.bin");

        let report = plan(root, &cfg(&["big/"], &[], &["data/"]));
        let dirs: Vec<String> = report
            .kept_dirs
            .iter()
            .map(|d| String::from_utf8_lossy(d).into_owned())
            .collect();
        assert_eq!(
            dirs,
            vec!["site", "site/deep", "site/hollow"],
            "an empty directory counts; a protected one and an excluded one do not, \
             and the scope root is docker's to recreate, not ulak's to retire"
        );
    }

    #[test]
    fn config_excludes_and_builtins_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, ".git/HEAD");
        touch(root, ".DS_Store");
        touch(root, "big/blob.bin");
        touch(root, "app.js");

        let report = plan(root, &cfg(&["big/"], &[], &[]));
        assert_eq!(
            excludes_as_strings(&report),
            vec![
                ".DS_Store".to_string(),
                ".git".to_string(),
                "big".to_string()
            ]
        );
        assert_eq!(report.kept_files(), 1);
    }

    /// Ulak's own scratch never travels, whatever it is called this time.
    ///
    /// Both names carry a pid or the destination's own filename, so a
    /// literal match catches neither: a reconcile that runs while a
    /// `docker cp` is unpacking would otherwise push the half-extracted
    /// tree to the server, or delete it mid-extraction.
    #[test]
    fn ulaks_own_scratch_is_never_synced_under_any_of_its_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, ".ulak-cp-4711-0a1b2c3d/payload/inner.txt");
        touch(root, "dist/api.tar.ulak-partial");
        touch(root, "dist/api.tar");
        touch(root, "app.js");

        let report = plan(root, &cfg(&[], &[], &[]));
        assert_eq!(
            excludes_as_strings(&report),
            vec![
                ".ulak-cp-4711-0a1b2c3d".to_string(),
                "dist/api.tar.ulak-partial".to_string()
            ]
        );
        assert_eq!(report.kept_files(), 2, "app.js and the finished api.tar");
    }

    #[test]
    fn include_reaches_into_excluded_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "secrets/prod.env");
        touch(root, "secrets/junk.tmp");

        let report = plan(root, &cfg(&["secrets/"], &["secrets/prod.env"], &[]));
        assert_eq!(
            excludes_as_strings(&report),
            vec!["secrets/junk.tmp".to_string()]
        );
        assert_eq!(kept_as_strings(&report), vec!["secrets/prod.env"]);
    }

    /// A footprint root is judged like any directory below it: a mounted
    /// `./logs` the user excluded stays local (doctor already called it
    /// excluded), an include still reaches inside an excluded root, and a
    /// root under an excluded directory goes with it unless an include
    /// names it. Walked one root at a time, the answer is the one a single
    /// walk from the anchor gives — which is what a rescan relies on too.
    #[test]
    fn an_exclude_keeps_a_mounted_directory_itself_local() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for rel in [
            "logs/app.log",
            "src/main.rs",
            "secrets/prod.env",
            "secrets/junk.tmp",
            "vendor/lib/a.rs",
            "vendor/other/b.rs",
        ] {
            touch(root, rel);
        }
        let rules = cfg(
            &["logs/", "secrets/", "vendor/"],
            &["secrets/prod.env", "vendor/lib/"],
            &[],
        );
        let none = BuildFilter::default();
        let walk = |scope: &str| plan_in(root, &root.join(scope), &rules, &none).unwrap();

        let logs = walk("logs");
        assert_eq!(excludes_as_strings(&logs), vec!["logs".to_string()]);
        assert!(logs.kept.is_empty());
        assert!(is_ignored(root, &root.join("logs"), &rules, &none).unwrap());
        assert_eq!(kept_as_strings(&walk("src")), vec!["src/main.rs"]);
        let secrets = walk("secrets");
        assert_eq!(
            excludes_as_strings(&secrets),
            vec!["secrets/junk.tmp".to_string()]
        );
        assert_eq!(kept_as_strings(&secrets), vec!["secrets/prod.env"]);
        assert_eq!(
            kept_as_strings(&walk("vendor/lib")),
            vec!["vendor/lib/a.rs"]
        );
        assert_eq!(
            excludes_as_strings(&walk("vendor/other")),
            vec!["vendor/other".to_string()]
        );

        let mut roots = WalkReport::default();
        for scope in ["logs", "secrets", "src", "vendor/lib", "vendor/other"] {
            let part = walk(scope);
            roots.excludes.extend(part.excludes);
            roots.kept.extend(part.kept);
        }
        let whole = plan(root, &rules);
        assert_eq!(excludes_as_strings(&roots), excludes_as_strings(&whole));
        assert_eq!(kept_as_strings(&roots), kept_as_strings(&whole));

        // And `is_ignored`, which doctor and a rescan ask, gives the walk's
        // answer for every one of them: a directory an include reaches
        // into is not swallowed, what the include leaves behind in it is.
        for (rel, swallowed) in [
            ("logs", true),
            ("src", false),
            ("secrets", false),
            ("secrets/prod.env", false),
            ("secrets/junk.tmp", true),
            ("vendor", false),
            ("vendor/lib", false),
            ("vendor/lib/a.rs", false),
            ("vendor/other", true),
            ("vendor/other/b.rs", true),
        ] {
            assert_eq!(
                is_ignored(root, &root.join(rel), &rules, &none).unwrap(),
                swallowed,
                "{rel}"
            );
            let travels = whole.kept.iter().any(|k| {
                let k = String::from_utf8_lossy(k);
                k == rel || k.starts_with(&format!("{rel}/"))
            });
            assert_eq!(travels, !swallowed, "{rel} as the walk sees it");
        }
    }

    /// `[sync] protect` outranks an include that reaches into an excluded
    /// directory, as it outranks one everywhere else: a server-owned path
    /// is never entered, so it can never be claimed — from the anchor, from
    /// the excluded root, or from inside the protected path itself.
    #[test]
    fn protect_outranks_an_include_that_reaches_into_an_excluded_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "storage/db/live.json");
        touch(root, "storage/cache/a.json");
        touch(root, "storage/junk.bin");
        let rules = cfg(&["storage/"], &["storage/**/*.json"], &["storage/db/"]);
        let none = BuildFilter::default();

        let whole = plan(root, &rules);
        assert_eq!(kept_as_strings(&whole), vec!["storage/cache/a.json"]);
        assert_eq!(
            excludes_as_strings(&whole),
            vec!["storage/junk.bin".to_string()]
        );
        let storage = plan_in(root, &root.join("storage"), &rules, &none).unwrap();
        assert_eq!(kept_as_strings(&storage), vec!["storage/cache/a.json"]);
        let db = plan_in(root, &root.join("storage/db"), &rules, &none).unwrap();
        assert!(
            db.kept.is_empty() && db.excludes.is_empty(),
            "a protected root is neither walked nor named: {:?}",
            kept_as_strings(&db)
        );
    }

    #[test]
    fn wildcard_include_reaches_into_excluded_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "config/prod.env");
        touch(root, "config/junk.tmp");

        let report = plan(root, &cfg(&["config/"], &["config/*.env"], &[]));
        assert_eq!(
            excludes_as_strings(&report),
            vec!["config/junk.tmp".to_string()]
        );
    }

    #[test]
    fn bare_name_include_does_not_force_descend() {
        // ".env" (default) must NOT drag the walker into node_modules.
        assert!(!include_reaches_into(&[".env".into()], "node_modules"));
        // …but explicit deep globs and anchored paths do reach.
        assert!(include_reaches_into(
            &["**/prod.env".into()],
            "node_modules"
        ));
        assert!(include_reaches_into(&["config/*.env".into()], "config"));
        assert!(!include_reaches_into(&["config/*.env".into()], "dist"));
        assert!(include_reaches_into(
            &["/secrets/prod.env".into()],
            "secrets"
        ));
    }

    #[test]
    fn nested_git_dirs_are_excluded() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, ".git/HEAD");
        touch(root, "vendor/dep/.git/HEAD");
        touch(root, "vendor/dep/lib.rs");

        let report = plan(root, &cfg(&[], &[], &[]));
        assert_eq!(
            excludes_as_strings(&report),
            vec![".git".to_string(), "vendor/dep/.git".to_string()]
        );
    }

    fn context(root: &Path) -> crate::dockerignore::BuildContext {
        crate::dockerignore::BuildContext {
            root: root.to_path_buf(),
            dockerfile: None,
        }
    }

    /// The oversized workspace, reproduced small. Several services build
    /// from the repo root, so the anchor IS a build context and nothing
    /// narrows it — but the user already wrote down what the build
    /// reads. Without the `.dockerignore` stage every one of these
    /// files travels.
    #[test]
    fn a_build_contexts_dockerignore_narrows_what_travels() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "app/main.rs");
        touch(root, "junk/big.bin");
        touch(root, "stray.zip");
        fs::write(root.join(".dockerignore"), "*\n!app\n").unwrap();

        let build = BuildFilter::new(&[context(root)], &[]);
        let report = plan_in(root, root, &cfg(&[], &[], &[]), &build).unwrap();

        assert_eq!(
            excludes_as_strings(&report),
            vec!["junk".to_string(), "stray.zip".to_string()],
            "a dropped directory is ONE exclude line, so rsync never descends"
        );
        let kept: Vec<String> = report
            .kept
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        assert_eq!(
            kept,
            vec![".dockerignore".to_string(), "app/main.rs".to_string()],
            "the ignore file itself always travels — the server's build reads it"
        );
    }

    /// The broken mounts. Compose mounts a config file out of a repo
    /// no Dockerfile ever COPYs, so `.dockerignore` drops it and is
    /// RIGHT to — but a local bind mount does not consult
    /// `.dockerignore`, and filtering it would show the container an
    /// empty directory. The pin overrides, and the directory above it
    /// stays walkable.
    #[test]
    fn a_mounted_path_overrides_the_dockerignore() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "app/main.rs");
        touch(root, "app-core-be/listeners.yml");
        touch(root, "app-core-be/src/Bulk.java");
        fs::write(root.join(".dockerignore"), "*\n!app\n").unwrap();

        let mount = root.join("app-core-be/listeners.yml");
        let build = BuildFilter::new(&[context(root)], &[mount]);
        let report = plan_in(root, root, &cfg(&[], &[], &[]), &build).unwrap();

        assert_eq!(
            excludes_as_strings(&report),
            vec!["app-core-be/src".to_string()],
            "only the part of that repo nothing asked for is dropped"
        );
        let kept: Vec<String> = report
            .kept
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        assert_eq!(
            kept,
            vec![
                ".dockerignore".to_string(),
                "app/main.rs".to_string(),
                "app-core-be/listeners.yml".to_string(),
            ]
        );
        let dirs: Vec<String> = report
            .kept_dirs
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        assert_eq!(
            dirs,
            vec!["app".to_string(), "app-core-be".to_string()],
            "a dropped directory ulak still puts on the server has to be \
             in the ledger, or it becomes the empty shell nobody can clear"
        );
    }

    /// The ignore rules above `.dockerignore` are unchanged by it: an
    /// excluded path stays out even when a build context would keep it,
    /// because pushing an excluded `./data` over a live database is
    /// still the worse failure.
    #[test]
    fn an_exclude_still_outranks_the_build_context() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "app/main.rs");
        touch(root, "app/secrets.key");
        fs::write(root.join(".dockerignore"), "*\n!app\n").unwrap();

        let build = BuildFilter::new(&[context(root)], &[]);
        let report = plan_in(root, root, &cfg(&["*.key"], &[], &[]), &build).unwrap();
        assert!(
            excludes_as_strings(&report).contains(&"app/secrets.key".to_string()),
            "the exclude still wins: {:?}",
            excludes_as_strings(&report)
        );
    }

    /// `[sync] include` takes a path back from a `.dockerignore` the way
    /// it takes one back from an exclude: an anchored pattern reaches
    /// into the dropped directory, and only what it names travels.
    #[test]
    fn an_include_reaches_into_a_directory_the_dockerignore_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "app/main.rs");
        touch(root, "assets/keep/logo.png");
        touch(root, "assets/junk.bin");
        fs::write(root.join(".dockerignore"), "*\n!app\n").unwrap();
        let build = BuildFilter::new(&[context(root)], &[]);

        let report = plan_in(root, root, &cfg(&[], &[], &[]), &build).unwrap();
        assert_eq!(excludes_as_strings(&report), vec!["assets".to_string()]);

        let include = cfg(&[], &["assets/keep/logo.png"], &[]);
        let report = plan_in(root, root, &include, &build).unwrap();
        assert_eq!(
            excludes_as_strings(&report),
            vec!["assets/junk.bin".to_string()]
        );
        assert_eq!(
            kept_as_strings(&report),
            vec![".dockerignore", "app/main.rs", "assets/keep/logo.png"]
        );

        // A directory include takes its whole subtree back — its children
        // are not dropped again through the parent the `.dockerignore`
        // matches — and a walk that starts inside it, as a rescan does,
        // answers the same.
        touch(root, "assets/keep/deep/icon.svg");
        let whole = cfg(&[], &["assets/keep/"], &[]);
        let report = plan_in(root, root, &whole, &build).unwrap();
        assert_eq!(
            kept_as_strings(&report),
            vec![
                ".dockerignore",
                "app/main.rs",
                "assets/keep/deep/icon.svg",
                "assets/keep/logo.png"
            ]
        );
        let rescan = plan_in(root, &root.join("assets/keep/deep"), &whole, &build).unwrap();
        assert_eq!(kept_as_strings(&rescan), vec!["assets/keep/deep/icon.svg"]);

        // A rule that would drop something below the included directory
        // has no say there for `is_ignored` either, or a rescan skips what
        // the walk keeps and doctor calls it excluded.
        fs::write(root.join(".dockerignore"), "*\n!app\n!assets\n**/deep\n").unwrap();
        let build = BuildFilter::new(&[context(root)], &[]);
        let kept = kept_as_strings(&plan_in(root, root, &whole, &build).unwrap());
        assert!(
            kept.contains(&"assets/keep/deep/icon.svg".to_string()),
            "{kept:?}"
        );
        assert!(!is_ignored(root, &root.join("assets/keep/deep"), &whole, &build).unwrap());
    }

    /// A build context is what docker tars up: its `.dockerignore`
    /// decides, and a `.gitignore` beside it decides nothing. The build
    /// output a Dockerfile `COPY`s is gitignored in most repos — this is
    /// the file that used to be missing on the server alone.
    #[test]
    fn a_build_context_reads_its_dockerignore_and_never_the_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "app/main.rs");
        touch(root, "app/dist/bundle.js");
        touch(root, "junk/big.bin");
        fs::write(root.join(".gitignore"), "dist/\napp/\n").unwrap();
        fs::write(root.join(".dockerignore"), "*\n!app\n").unwrap();

        let build = BuildFilter::new(&[context(root)], &[]);
        let report = plan_in(root, root, &cfg(&[], &[], &[]), &build).unwrap();
        assert_eq!(
            excludes_as_strings(&report),
            vec![".gitignore".to_string(), "junk".to_string()],
            "the `.dockerignore` drops the `.gitignore` like any other top-level file"
        );
        assert_eq!(
            kept_as_strings(&report),
            vec![".dockerignore", "app/dist/bundle.js", "app/main.rs"]
        );
    }

    #[test]
    fn filter_pattern_escapes_wildcards() {
        assert_eq!(rsync_filter_pattern("data"), "/data");
        assert_eq!(rsync_filter_pattern("cache[old]"), "/cache\\[old]");
        assert_eq!(rsync_filter_pattern("rel*eases"), "/rel\\*eases");
    }

    #[test]
    fn newline_names_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "ok.txt");
        fs::write(root.join("bad\nname.txt"), b"x").unwrap();

        let report = plan(root, &cfg(&[], &[], &[]));
        assert_eq!(report.bad_names.len(), 1);
        assert!(report.bad_names[0].contains("\\n"));
    }

    #[test]
    fn wildcard_names_get_escaped_patterns() {
        assert_eq!(rsync_exclude_pattern(b"plain/path.txt"), b"/plain/path.txt");
        assert_eq!(
            rsync_exclude_pattern(b"we[ird*name?"),
            b"/we\\[ird\\*name\\?".to_vec()
        );
        // backslash in a name WITHOUT wildcards must stay untouched
        assert_eq!(
            rsync_exclude_pattern(b"back\\slash"),
            b"/back\\slash".to_vec()
        );
    }

    /// `protect = ["data/"]` and `exclude = ["/secret.txt"]` beside the
    /// compose file. Read from the anchor as written, a sibling mount that
    /// widened it to the parent sent `repo/data/db` and `repo/secret.txt`.
    /// Read through `rebase`, the project walks the same under either.
    #[test]
    fn a_rule_names_the_same_files_whatever_the_anchor() {
        let tmp = tempfile::tempdir().unwrap();
        let w = tmp.path();
        let repo = w.join("repo");
        for rel in [
            "repo/data/db",
            "repo/secret.txt",
            "repo/src/main.rs",
            "web/uploads/a.png",
            "web/index.html",
        ] {
            touch(w, rel);
        }
        let written = cfg(&["/secret.txt"], &[], &["data/", "../web/uploads/"]);
        let none = BuildFilter::default();

        let narrow = plan_in(&repo, &repo, &rebase(&written, &repo, &repo), &none).unwrap();
        assert_eq!(kept_as_strings(&narrow), ["src/main.rs"]);
        assert_eq!(excludes_as_strings(&narrow), ["secret.txt"]);

        let wide = rebase(&written, &repo, w);
        let (mut kept, mut excluded) = (Vec::new(), Vec::new());
        for scope in [repo.clone(), w.join("web")] {
            let part = plan_in(w, &scope, &wide, &none).unwrap();
            kept.extend(kept_as_strings(&part));
            excluded.extend(excludes_as_strings(&part));
        }
        kept.sort();
        assert_eq!(kept, ["repo/src/main.rs", "web/index.html"]);
        assert_eq!(excluded, ["repo/secret.txt"]);
    }

    #[test]
    fn rules_are_written_from_the_project_and_read_from_the_anchor() {
        let repo = Path::new("/w/repo");
        let w = Path::new("/w");
        let protect =
            |anchor: &Path, entries: &[&str]| rebase(&cfg(&[], &[], entries), repo, anchor).protect;
        let exclude =
            |anchor: &Path, lines: &[&str]| rebase(&cfg(lines, &[], &[]), repo, anchor).exclude;

        assert_eq!(
            protect(
                w,
                &["data/", "./data", "/data", "../web/uploads", "data/../logs"]
            ),
            [
                "repo/data",
                "repo/data",
                "repo/data",
                "web/uploads",
                "repo/logs"
            ]
        );
        assert!(
            protect(w, &[".", "..", "../..", "../../elsewhere"]).is_empty(),
            "the project itself, a folder above it and one outside the anchor name nothing that travels"
        );
        assert_eq!(
            protect(repo, &["data/", "../repo/data", "../web/uploads"]),
            ["data", "data"]
        );

        assert_eq!(
            exclude(
                w,
                &[
                    "node_modules",
                    "*.log",
                    "**/upload/",
                    "/**/tmp",
                    "# note",
                    "/secret.txt",
                    "web/dist/",
                    "!/keep.txt",
                    "./a/b",
                    "../web/dist/",
                ]
            ),
            [
                "node_modules",
                "*.log",
                "**/upload/",
                "/**/tmp",
                "# note",
                "/repo/secret.txt",
                "/repo/web/dist/",
                "!/repo/keep.txt",
                "/repo/a/b",
                "/web/dist/",
                "!/repo/",
            ]
        );
        // From the project's own root: a `../` that walks back in by name,
        // by a glob or through `**` still counts, one that leaves the
        // anchor names nothing. Each means what it means under the wider
        // anchor above, so no profile changes what a rule reaches.
        assert_eq!(
            exclude(
                repo,
                &[
                    "/secret.txt",
                    "../repo/x",
                    "../web/dist",
                    "../*/x",
                    "../re[a-p]o/y",
                    "../w*/z",
                    "../**/cache/",
                    "!../**/keep",
                    "../**/",
                ]
            ),
            ["/secret.txt", "/x", "/x", "/y", "/**/cache/", "!/**/keep"]
        );
        // A project directory ABOVE the anchor: what climbs into the
        // anchor keeps its meaning — doctor's spelling reads back — and
        // the rest names nothing that travels.
        let docker = Path::new("/w/repo/docker");
        assert_eq!(protect(docker, &["docker/pgdata/", "data/"]), ["pgdata"]);
        assert_eq!(as_written(repo, docker, "pgdata"), "docker/pgdata");
        assert_eq!(
            exclude(docker, &["/docker/cache/", "node_modules", "/secret.txt"]),
            ["/cache/", "node_modules"]
        );
        assert_eq!(
            exclude(docker, &["*/cache/", "**/tmp", "/*/**/logs"]),
            ["/cache/", "**/tmp", "/**/logs"]
        );
        // A project directory the anchor does not touch has nothing to
        // respell from, and doctor's spelling is the anchor's.
        let elsewhere = Path::new("/elsewhere/repo");
        let written = cfg(&["/secret.txt"], &["a/b"], &["data/"]);
        assert_eq!(rebase(&written, elsewhere, w), written);
        assert_eq!(as_written(elsewhere, w, "repo/data"), "repo/data");
    }

    /// The project folder, and every folder between it and a wider anchor,
    /// are where the rules are written from, never what they judge: a
    /// narrow anchor never asks about them, and a wide one — a profile
    /// mounted a sibling — must not either, or `exclude = ["api"]` drops a
    /// project called `api` whole. Everything inside it is judged alike.
    #[test]
    fn a_project_folder_is_never_judged_by_its_own_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let w = tmp.path();
        let api = w.join("api");
        touch(w, "api/main.rs");
        touch(w, "api/api/nested.rs");
        let written = cfg(&["api"], &[], &[]);
        let none = BuildFilter::default();

        let narrow = plan_in(&api, &api, &rebase(&written, &api, &api), &none).unwrap();
        assert_eq!(kept_as_strings(&narrow), vec!["main.rs"]);
        assert_eq!(excludes_as_strings(&narrow), vec!["api".to_string()]);

        let wide_rules = rebase(&written, &api, w);
        let wide = plan_in(w, &api, &wide_rules, &none).unwrap();
        assert_eq!(kept_as_strings(&wide), vec!["api/main.rs"]);
        assert_eq!(excludes_as_strings(&wide), vec!["api/api".to_string()]);
        assert!(!is_ignored(w, &api, &wide_rules, &none).unwrap());
        assert!(is_ignored(w, &api.join("api"), &wide_rules, &none).unwrap());
    }

    /// The project folder's own name goes in front of every anchored rule,
    /// so a `[` in it must stay a character — and an include must still
    /// see that it reaches into the excluded directory beneath it.
    #[test]
    fn a_folder_named_like_a_glob_is_respelled_literally() {
        let tmp = tempfile::tempdir().unwrap();
        let w = tmp.path();
        let repo = w.join("app[1]");
        for rel in [
            "app[1]/main.rs",
            "app[1]/secret.txt",
            "app[1]/secrets/prod.env",
            "app[1]/secrets/junk.tmp",
        ] {
            touch(w, rel);
        }
        let written = cfg(&["/secret.txt", "/secrets/"], &["/secrets/prod.env"], &[]);
        let report = plan_in(
            w,
            &repo,
            &rebase(&written, &repo, w),
            &BuildFilter::default(),
        )
        .unwrap();
        assert_eq!(
            kept_as_strings(&report),
            ["app[1]/main.rs", "app[1]/secrets/prod.env"]
        );
        assert_eq!(
            excludes_as_strings(&report),
            ["app[1]/secret.txt", "app[1]/secrets/junk.tmp"]
        );
    }

    #[test]
    fn what_doctor_suggests_reads_back_as_the_path_it_meant() {
        let repo = Path::new("/w/repo");
        let w = Path::new("/w");
        assert_eq!(as_written(repo, w, "repo/data"), "data");
        assert_eq!(as_written(repo, w, "web/uploads"), "../web/uploads");
        assert_eq!(as_written(Path::new("/w/a/b"), w, "c/d"), "../../c/d");
        for rel in ["repo/data", "web/uploads", "repo/a/b"] {
            let written = as_written(repo, w, rel);
            assert_eq!(rebase(&cfg(&[], &[], &[&written]), repo, w).protect, [rel]);
        }
    }
}
