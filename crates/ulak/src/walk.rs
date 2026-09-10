//! Local tree walk that turns gitignore semantics into a LITERAL,
//! NUL-delimitable exclude list for rsync.
//!
//! Why not translate gitignore patterns into rsync excludes? Their glob
//! dialects differ in metacharacters, anchoring and negation — a
//! design review confirmed silent divergence. Why not
//! --files-from? Measured: rsync ignores --delete for file lists (and
//! with -r it would re-enumerate ignored dirs itself). So: rsync walks
//! the full tree; we hand it the exact paths to skip.
//!
//! Priority per entry: protect > include (escape hatch) > config
//! exclude > gitignore chain > .dockerignore > keep.
//!
//! `.dockerignore` sits at the bottom because it is the narrowest claim
//! of the five: it speaks only for what a BUILD reads, and only inside a
//! build context. Everything above it — and every path the footprint
//! pins for another reason — overrides it. It lands in the same literal
//! exclude list as the gitignore chain, which is what makes a pruned
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
/// scan), while every pattern the user writes and every path ulak
/// prints is anchor-relative, because the anchor IS the workspace root.
///
/// The gitignore stack is seeded from the anchor down to `scope`, so a
/// repo-root `.gitignore` still governs a directory deep inside it.
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
    let mut gitignore_stack = seed_gitignores(anchor, rel_scope)?;
    let mut report = WalkReport::default();
    walk_dir(
        anchor,
        rel_scope,
        &exclude_matcher,
        &include_matcher,
        &protect,
        &cfg.include,
        build,
        &mut gitignore_stack,
        &mut report,
    )?;
    Ok(report)
}

/// Would the ignore rules swallow this path outright?
///
/// The footprint names paths the compose file asked for, but the ignore
/// rules still win (sync.rs documents why: pushing a gitignored `./data`
/// over a live database is the worse failure). So doctor has to be able
/// to say WHICH referenced path silently will not travel — otherwise the
/// server just grows an empty directory where a mount should be.
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

    let mut stack: Vec<Gitignore> = Vec::new();
    let mut dir = anchor.to_path_buf();
    let mut partial = std::path::PathBuf::new();
    let comps: Vec<_> = rel.components().collect();
    for (i, comp) in comps.iter().enumerate() {
        // A directory's .gitignore governs its children, so it joins the
        // stack before the child is judged.
        let gitignore_file = dir.join(".gitignore");
        if gitignore_file.is_file() {
            let mut b = GitignoreBuilder::new(&dir);
            b.add(&gitignore_file);
            stack.push(b.build()?);
        }
        partial.push(comp);
        dir.push(comp);
        let deeper_to_go = i + 1 < comps.len();
        let is_dir = deeper_to_go || dir.is_dir();
        let rel_str = partial.to_string_lossy();

        // Same precedence the walker uses: protect > include > exclude.
        // Not the same REACH, though: the walker rescues an anchored
        // [sync] include out of an excluded directory (see
        // `include_reaches_into`), and this loop has no equivalent — it
        // returns at the first excluded ancestor, before the include for
        // the deeper path is ever tested. So `secrets/prod.env` under a
        // gitignored `secrets/` is reported ignored here even though the
        // walk keeps it and it travels.
        if protect.iter().any(|p| protect_covers(p, &rel_str)) {
            return Ok(true);
        }
        if matches!(include.matched(&partial, is_dir), Match::Ignore(_)) {
            continue;
        }
        if matches!(exclude.matched(&partial, is_dir), Match::Ignore(_))
            || gitignore_verdict(&stack, anchor, &partial, is_dir)
        {
            return Ok(true);
        }
        // A build context can sit inside another one, and the outer
        // `.dockerignore` can drop it. Nothing else here is at risk: a
        // context is never judged by its own file, and everything the
        // stack needs for another reason is pinned.
        if matches!(
            build.verdict(&dir, is_dir),
            Verdict::Drop { descend: false }
        ) {
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
/// anchor-relative and already anchored — `data` names the tree at the
/// root and never `api/data` — so `/data` says what the entry meant
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

/// `.gitignore` files between the anchor and the scope, shallowest
/// first, so git's "deepest wins" ordering still holds inside `scope`.
fn seed_gitignores(anchor: &Path, rel_scope: &Path) -> Result<Vec<Gitignore>> {
    let mut stack = Vec::new();
    let mut dir = anchor.to_path_buf();
    for comp in rel_scope.components() {
        let file = dir.join(".gitignore");
        if file.is_file() {
            let mut b = GitignoreBuilder::new(&dir);
            b.add(&file);
            stack.push(b.build()?);
        }
        dir.push(comp);
    }
    Ok(stack)
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
    gitignore_stack: &mut Vec<Gitignore>,
    report: &mut WalkReport,
) -> Result<()> {
    let abs_dir = root.join(rel_dir);

    // Deeper .gitignore files take precedence over shallower ones.
    let gitignore_file = abs_dir.join(".gitignore");
    let pushed = if gitignore_file.is_file() {
        let mut b = GitignoreBuilder::new(&abs_dir);
        b.add(&gitignore_file);
        gitignore_stack.push(b.build()?);
        true
    } else {
        false
    };

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

        // 2. include escape hatch (e.g. .env, gitignored but required).
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
                    build,
                    gitignore_stack,
                    report,
                )?;
            } else {
                keep_file(&rel, report);
            }
            continue;
        }

        // 3. config + builtin excludes, then 4. the gitignore chain.
        let excluded = matches!(exclude.matched(&rel, is_dir), Match::Ignore(_))
            || gitignore_verdict(gitignore_stack, root, &rel, is_dir);

        if excluded {
            // Don't prune a dir that may hide a force-included path.
            if is_dir && include_reaches_into(include_lines, &rel_str) {
                walk_excluded_dir(
                    root,
                    &rel,
                    exclude,
                    include,
                    protect,
                    include_lines,
                    build,
                    gitignore_stack,
                    report,
                )?;
            } else {
                report.excludes.push(rel_bytes(&rel));
            }
            continue;
        }

        // 5. the build context's own `.dockerignore`. A DIRECTORY it
        // drops is still entered when something inside may yet be wanted
        // — docker's own tar walker makes the same exception, and the
        // repos compose mounts config out of depend on it. Entering
        // one is exactly the normal keep path: every child is judged
        // here on its own terms anyway.
        if let Verdict::Drop { descend } = build.verdict(&abs_dir.join(&name), is_dir)
            && !(is_dir && descend)
        {
            report.excludes.push(rel_bytes(&rel));
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
                gitignore_stack,
                report,
            )?;
        } else {
            keep_file(&rel, report);
        }
    }

    if pushed {
        gitignore_stack.pop();
    }
    Ok(())
}

/// Inside an excluded dir kept alive only for force-includes: everything
/// not include-matched is excluded individually.
#[allow(clippy::too_many_arguments)]
fn walk_excluded_dir(
    root: &Path,
    rel_dir: &Path,
    exclude: &Gitignore,
    include: &Gitignore,
    protect: &[String],
    include_lines: &[String],
    build: &BuildFilter,
    gitignore_stack: &mut Vec<Gitignore>,
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
                    build,
                    gitignore_stack,
                    report,
                )?;
            } else {
                keep_file(&rel, report);
            }
        } else if is_dir && include_reaches_into(include_lines, &rel_str) {
            walk_excluded_dir(
                root,
                &rel,
                exclude,
                include,
                protect,
                include_lines,
                build,
                gitignore_stack,
                report,
            )?;
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

/// Last matching gitignore wins, deepest file last (git semantics).
fn gitignore_verdict(stack: &[Gitignore], root: &Path, rel: &Path, is_dir: bool) -> bool {
    let abs = root.join(rel);
    let mut ignored = false;
    for gi in stack {
        match gi.matched(&abs, is_dir) {
            Match::Ignore(_) => ignored = true,
            Match::Whitelist(_) => ignored = false,
            Match::None => {}
        }
    }
    ignored
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
        let prefix = &body[..body.find(['*', '?', '[']).unwrap_or(body.len())];
        let dir_slash = format!("{rel_dir}/");
        prefix.starts_with(&dir_slash) || dir_slash.starts_with(prefix)
    })
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

    #[test]
    fn gitignore_chain_and_env_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "keep.txt");
        touch(root, ".env");
        touch(root, "node_modules/pkg/big.js");
        touch(root, "sub/target/out.bin");
        touch(root, "sub/src.rs");
        fs::write(root.join(".gitignore"), "node_modules/\n.env\n").unwrap();
        fs::write(root.join("sub/.gitignore"), "target/\n").unwrap();

        let report = plan(root, &cfg(&[], &[".env"], &[]));
        assert_eq!(
            excludes_as_strings(&report),
            vec!["node_modules".to_string(), "sub/target".to_string()]
        );
        // .env kept despite gitignore; keep.txt, sub/src.rs, .gitignores kept
        assert_eq!(report.kept_files(), 5);
        assert!(report.bad_names.is_empty());
    }

    #[test]
    fn deeper_gitignore_negation_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "logs/app.log");
        touch(root, "logs/KEEP.log");
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("logs/.gitignore"), "!KEEP.log\n").unwrap();

        let report = plan(root, &cfg(&[], &[], &[]));
        assert_eq!(
            excludes_as_strings(&report),
            vec!["logs/app.log".to_string()]
        );
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
        fs::write(root.join(".gitignore"), "secrets/\n").unwrap();

        let report = plan(root, &cfg(&[], &["secrets/prod.env"], &[]));
        assert_eq!(
            excludes_as_strings(&report),
            vec!["secrets/junk.tmp".to_string()]
        );
        assert_eq!(report.kept_files(), 2); // prod.env + root .gitignore
    }

    #[test]
    fn wildcard_include_reaches_into_excluded_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "config/prod.env");
        touch(root, "config/junk.tmp");
        fs::write(root.join(".gitignore"), "config/\n").unwrap();

        let report = plan(root, &cfg(&[], &["config/*.env"], &[]));
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

    /// The ignore rules above `.dockerignore` are unchanged by it: a
    /// gitignored path stays out even when a build context would keep it,
    /// because pushing a gitignored `./data` over a live database is
    /// still the worse failure.
    #[test]
    fn gitignore_still_outranks_the_build_context() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "app/main.rs");
        touch(root, "app/secrets.key");
        fs::write(root.join(".gitignore"), "*.key\n").unwrap();
        fs::write(root.join(".dockerignore"), "*\n!app\n").unwrap();

        let build = BuildFilter::new(&[context(root)], &[]);
        let report = plan_in(root, root, &cfg(&[], &[], &[]), &build).unwrap();
        assert!(
            excludes_as_strings(&report).contains(&"app/secrets.key".to_string()),
            "the gitignore chain still wins: {:?}",
            excludes_as_strings(&report)
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
}
