//! The ledger: which paths ulak itself put in the workspace.
//!
//! Locally there is one filesystem, so "last writer wins" is the whole
//! story — nothing ever deletes a file because it is *absent* somewhere
//! else. A workspace has two copies, and `rsync --delete` invents exactly
//! that operation: it removes a remote file for the sole reason that the
//! local side does not have it. Docker has no equivalent, and it is what
//! silently ate a freshly generated migration (measured).
//!
//! One bit of memory fixes it. After every successful sync ulak
//! records the paths it synced; next time, a remote file is only
//! eligible for deletion if it is IN that list and gone from disk now.
//! Anything else was born on the server — the container's work — and is
//! left alone.
//!
//! A ledger belongs to one REMOTE copy, not merely one local checkout.
//! Its path is keyed by workspace plus SSH destination. When one shared
//! ledger served server A and server B, A's confirmed deletion retired B's
//! row too; B then looked server-born and its stale file was pulled home.
//! The workspace lock remains checkout-wide because both destinations can
//! still pull into the same local tree.
//!
//! Deliberately NOT a conflict engine: no hashes, no three-way merge, no
//! `.conflict` files. Local docker has none of that either.
//!
//! Directories are recorded too, with a TRAILING SLASH — walk.rs
//! guarantees file rows never end in one, so the two never blur. They
//! have to be in here for the same reason files do: rsync creates a
//! directory on whichever side receives, so ulak put it there, and
//! "ulak put it there" is the only licence it has to take it away.
//! Without the rows, an empty directory was undeletable in BOTH
//! directions — it sat on the server for good, and the pull copied it
//! into every repo that synced. A directory row is removed with `rmdir`
//! and never `rm -r`: one the container has since filled is no longer
//! an empty shell ulak left behind, and the failed removal is the
//! right answer.
//!
//! # One workspace, many writers
//!
//! `docker build`, `docker run` and `docker bake` all hash the SAME
//! workspace root into their workspace id (`Invocation::workspace` leaves
//! `compose_files` empty, so the identity path is the root itself), so
//! they share a remote directory and this one file. What they do not
//! share is a footprint: `docker build .` sends the whole tree, while
//! `docker run -v ./app:/app` sends one directory out of it.
//!
//! So the ledger MERGES. A writer adds what it put there and takes away
//! only what it actually removed; it never says anything about the rest
//! of the workspace, because it never looked. Writing it wholesale meant a
//! narrow `docker run` un-claimed everything a `docker build .` had put on
//! the server, with two consequences that were both permanent: those
//! files could never be retired again (nothing claimed them, so `doomed`
//! could never name them), and `pull_back`'s creating pass — which
//! excludes exactly the rows in here — was free to plant one back in the
//! repo after the user deleted it.
//!
//! # Why a row records its anchor
//!
//! A row is relative to an ANCHOR, and the anchor is not fixed per
//! workspace: `docker.rs::build_anchor` climbs to the common ancestor of
//! the workspace root and the build context, so `docker build ../sibling`
//! writes rows relative to the PARENT of the root, while every
//! `docker run` writes them relative to the root. The same row then names
//! two different local files, and "is it still on disk?" — the only
//! question that retires anything — has no single answer. A writer may
//! therefore retire only rows filed under its own anchor. The rest stay
//! claimed, which is the answer that loses nothing: a stale claim leaves
//! a file on the server, a wrong retirement deletes one that is still in
//! use.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::config::WorkspaceStateKey;

/// Entries are UTF-8 and newline-free by construction (walk.rs refuses
/// to sync anything else), so one path per line is lossless.
fn ledger_path(key: &WorkspaceStateKey) -> Option<PathBuf> {
    Some(crate::intent::workspace_state_dir_path(key)?.join("ledger"))
}

/// First line of a ledger that files its rows under an anchor. Its
/// ABSENCE is the version test, and it has to be: every ulak before this
/// wrote a bare list of rows, and those rows name real files that are
/// still on the server.
const MAGIC: &[u8] = b"#ulak-ledger 2";
/// What every versioned ledger's first line starts with, whatever
/// version it claims. Without this, "not exactly MAGIC" meant "v1", so a
/// file a NEWER ulak wrote — the shape a downgrade leaves behind — was
/// read as a flat row list whose rows were `@/Users/me/repo`,
/// `>site/index.html` and the magic line itself. The `>` then travels as
/// part of the path, so the pull is handed `>site/index.html` and the
/// REAL `site/index.html` is no longer excluded from the creating pass:
/// a file the user deleted is planted straight back. The next `store`
/// adopts the mangled rows and re-prefixes them, so it compounds.
const VERSION_PREFIX: &[u8] = b"#ulak-ledger ";
/// Line markers. A relative path may legally begin with `@`, so the two
/// kinds of line are told apart by a prefix rather than by hoping.
const ANCHOR: u8 = b'@';
const ROW: u8 = b'>';

#[derive(Default)]
struct Ledger {
    /// Rows, filed under the anchor they are relative to. The key is the
    /// anchor's raw bytes: it is only ever compared, never rebuilt into a
    /// path, so nothing has to survive a lossy conversion.
    by_anchor: BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
    /// Rows an older ulak wrote, which said nothing about what they were
    /// relative to. Adopted by the first writer that stores (see `store`),
    /// so this and `by_anchor` are never both filled: a file either has
    /// the magic line and is all sections, or has none and is all rows.
    unanchored: BTreeSet<Vec<u8>>,
}

impl Ledger {
    /// The rows this writer could have written itself — the only ones it
    /// may retire.
    fn rows_under(&self, anchor: &Path) -> impl Iterator<Item = &Vec<u8>> {
        self.by_anchor
            .get(anchor_key(anchor).as_slice())
            .into_iter()
            .flatten()
            .chain(&self.unanchored)
    }

    /// Every row, whatever it is relative to. What is on the server does
    /// not depend on which anchor a writer measured it from.
    fn all_rows(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.by_anchor.values().flatten().chain(&self.unanchored)
    }
}

fn anchor_key(anchor: &Path) -> Vec<u8> {
    anchor.as_os_str().as_encoded_bytes().to_vec()
}

fn read(key: &WorkspaceStateKey) -> Ledger {
    let Some(path) = ledger_path(key) else {
        return Ledger::default();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        // No ledger yet (first sync, or a wiped state dir): we do not
        // claim to have put ANYTHING there, so nothing is deletable.
        // Losing the ledger leaves stale files, never lost ones.
        return Ledger::default();
    };
    if version_of(&bytes) == Version::Unknown {
        // Refusing is the safe direction (stale files, never lost ones),
        // but it is not free: nothing already on the server can be
        // retired until this is sorted out, so it is said out loud
        // rather than absorbed.
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            crate::ui::warn(
                "this workspace's ledger was written by a newer Ulak and was not read — nothing already on the server can be retired by this version",
            );
            crate::ui::dim(&format!(
                "upgrade Ulak, or let this version claim the workspace again: rm {}",
                path.display()
            ));
        });
        return Ledger::default();
    }
    parse(&bytes)
}

/// Which ulak wrote this file — asked of the first line alone.
#[derive(Debug, PartialEq)]
enum Version {
    /// A bare list of rows: every ulak before the anchored format.
    Flat,
    /// The format this ulak reads and writes.
    Anchored,
    /// Versioned, but not a version this binary knows: a newer ulak the
    /// user has since downgraded from, or a file that acquired `\r\n`.
    /// Reinterpreting it is worse than refusing it.
    Unknown,
}

fn version_of(bytes: &[u8]) -> Version {
    let first = bytes.split(|b| *b == b'\n').next().unwrap_or_default();
    if first == MAGIC {
        Version::Anchored
    } else if first.starts_with(VERSION_PREFIX) {
        Version::Unknown
    } else {
        Version::Flat
    }
}

fn parse(bytes: &[u8]) -> Ledger {
    let mut out = Ledger::default();
    match version_of(bytes) {
        Version::Anchored => {}
        Version::Unknown => return out,
        Version::Flat => {
            out.unanchored = bytes
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty())
                .map(<[u8]>::to_vec)
                .collect();
            return out;
        }
    }
    let mut lines = bytes.split(|b| *b == b'\n');
    lines.next();
    let mut anchor: Option<Vec<u8>> = None;
    let mut rows: BTreeSet<Vec<u8>> = BTreeSet::new();
    for line in lines {
        match line.split_first() {
            Some((&ANCHOR, rest)) => {
                if let Some(previous) = anchor.take() {
                    out.by_anchor.entry(previous).or_default().append(&mut rows);
                }
                anchor = Some(unescape(rest));
            }
            Some((&ROW, rest)) if !rest.is_empty() => {
                rows.insert(rest.to_vec());
            }
            _ => {}
        }
    }
    if let Some(last) = anchor {
        out.by_anchor.entry(last).or_default().append(&mut rows);
    }
    out
}

fn render(ledger: &Ledger) -> Vec<u8> {
    let mut out = Vec::with_capacity(MAGIC.len() + 1);
    out.extend_from_slice(MAGIC);
    out.push(b'\n');
    for (anchor, rows) in &ledger.by_anchor {
        if rows.is_empty() {
            continue;
        }
        out.push(ANCHOR);
        out.extend_from_slice(&escape(anchor));
        out.push(b'\n');
        for row in rows {
            out.push(ROW);
            out.extend_from_slice(row);
            out.push(b'\n');
        }
    }
    out
}

/// Rows are newline-free by construction; an ANCHOR is not — it comes
/// from `common_ancestor` of whatever the user named, and a directory may
/// legally hold a newline in its name. One unescaped byte there would
/// turn the rest of the file into rows nobody filed.
fn escape(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for b in raw {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            _ => out.push(*b),
        }
    }
    out
}

fn unescape(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let mut bytes = line.iter();
    while let Some(b) = bytes.next() {
        if *b != b'\\' {
            out.push(*b);
            continue;
        }
        match bytes.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b'\\') => out.push(b'\\'),
            Some(other) => {
                out.push(b'\\');
                out.push(*other);
            }
            None => out.push(b'\\'),
        }
    }
    out
}

/// Record what this writer put in the workspace, and take back only what
/// it really removed.
///
/// MERGE, not replace, and that is the whole contract. Every writer sees
/// one shape of the workspace — the footprint it was given — and knows
/// nothing about the rest. A `docker run -v ./app:/app` that wrote the
/// ledger wholesale left the server holding a `docker build .`'s files
/// with nobody claiming them: undeletable forever, and free to be planted
/// back in the repo by `pull_back`'s creating pass.
///
/// `retired` and `retired_dirs` are the paths the server has ACTUALLY
/// lost — not the ones that were doomed. A `rmdir` the container's own
/// work refused is a directory that is still up there, and dropping its
/// row would make it exactly the empty shell nobody can clear. They are
/// removed from every anchor's section, because the path they name is a
/// path in the remote workspace: whichever writer claimed it, it is gone.
///
/// A refused or deferred deletion needs no special case any more: pass
/// nothing as retired and the rows simply stay, which is what "the files
/// are still up there" means.
pub fn store(
    key: &WorkspaceStateKey,
    anchor: &Path,
    claimed: &[Vec<u8>],
    retired: &[String],
    retired_dirs: &[String],
) {
    let Some(path) = ledger_path(key) else {
        return;
    };
    let mut ledger = read(key);
    // An older ulak's rows say nothing about what they are relative to,
    // and the manifest allows a workspace one anchor at a time — it
    // re-lays the server out when the anchor moves — so the writer
    // standing here is the one they belong to. Leaving them unanchored
    // instead would mean no writer could ever retire them, which is the
    // undeletable-file bug in a different costume.
    let adopted = std::mem::take(&mut ledger.unanchored);
    let mine = ledger.by_anchor.entry(anchor_key(anchor)).or_default();
    mine.extend(adopted);
    mine.extend(claimed.iter().cloned());

    let gone: BTreeSet<Vec<u8>> = retired
        .iter()
        .map(|p| p.clone().into_bytes())
        .chain(retired_dirs.iter().map(|d| dir_row(d.as_bytes())))
        .collect();
    if !gone.is_empty() {
        for rows in ledger.by_anchor.values_mut() {
            rows.retain(|row| !gone.contains(row));
        }
    }

    if let Some(dir) = path.parent() {
        let _ = crate::invocation::private_dir(dir);
    }
    let _ = write_atomic(&path, &render(&ledger));
}

/// Replace the file in one step, so a reader can never see half of it.
///
/// `invocation::write_private` truncates in place. `pull_back`'s creating
/// pass hands rsync every row in here as its exclude list, and
/// `passthrough::run` does that WITHOUT the per-workspace lock — so a
/// short read there is a row missing from the exclude list, and
/// `--ignore-existing` is then free to re-create the file the user just
/// deleted. A rename cannot be read halfway.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    let tmp = path.with_file_name(name);
    crate::invocation::write_private(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub fn forget(key: &WorkspaceStateKey) {
    if let Some(path) = ledger_path(key) {
        let _ = std::fs::remove_file(path);
    }
}

/// Paths the server should lose: ones ulak synced last time and
/// that are no longer on disk at all.
///
/// "No longer on disk" is stricter than "no longer synced" on purpose.
/// Newly EXCLUDED files (someone gitignored a directory) stay on the
/// server: a container may be using them, and the promise has always
/// been that excluding is not deleting.
///
/// Only rows filed under `anchor` are even asked the question. A row
/// written from a different anchor spells a different local path, so
/// `still_on_disk` would be answering about a file nobody named.
pub fn doomed(key: &WorkspaceStateKey, anchor: &Path, kept_now: &[Vec<u8>]) -> Vec<String> {
    let previous = read(key);
    let current: BTreeSet<&Vec<u8>> = kept_now.iter().collect();
    let mut listing_cache: BTreeMap<PathBuf, BTreeSet<std::ffi::OsString>> = BTreeMap::new();
    previous
        .rows_under(anchor)
        .filter(|rel| !rel.ends_with(b"/"))
        .filter(|rel| !current.contains(*rel))
        .filter_map(|rel| String::from_utf8(rel.clone()).ok())
        .filter(|rel| !still_on_disk(anchor, rel, &mut listing_cache))
        .collect()
}

/// The same question for directories, answered separately because the
/// answer drives a different command: `rmdir`, not `rm -f`.
///
/// Deepest first, so a whole removed subtree comes apart from the leaves
/// up in one pass. The returned paths carry NO trailing slash — the row
/// format's marker is not part of the path.
///
/// "Gone from disk" is the only test, exactly as it is for files: a
/// directory that is merely newly EXCLUDED is still here, and excluding
/// has never meant deleting.
pub fn doomed_dirs(key: &WorkspaceStateKey, anchor: &Path) -> Vec<String> {
    let previous = read(key);
    let mut listing_cache: BTreeMap<PathBuf, BTreeSet<std::ffi::OsString>> = BTreeMap::new();
    let mut gone: Vec<String> = previous
        .rows_under(anchor)
        .filter_map(|rel| {
            let rel = rel.strip_suffix(b"/")?;
            String::from_utf8(rel.to_vec()).ok()
        })
        .filter(|rel| !still_on_disk(anchor, rel, &mut listing_cache))
        .collect();
    gone.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    gone
}

/// Every FILE this workspace holds because ulak put it there.
///
/// The pull's whole licence rests on this list: a path in it may be
/// UPDATED here but never CREATED here, because "missing on this machine"
/// can only mean the user removed it. A path outside it is the
/// container's work and may be created freely.
///
/// Every anchor's rows, not just the caller's: the question is what is on
/// the SERVER, and a file is no less there for having been measured from
/// a different anchor.
///
/// Directory rows are deliberately left out. Excluding a directory stops
/// rsync descending into it, and what the container writes INSIDE one is
/// exactly what the pull exists to bring home. A directory whose files
/// are all excluded carries nothing and never travels anyway
/// (`--prune-empty-dirs`), so the shell of a directory the user removed
/// still cannot be planted here.
pub fn synced_files(key: &WorkspaceStateKey) -> Vec<Vec<u8>> {
    read(key)
        .all_rows()
        .filter(|rel| !rel.ends_with(b"/"))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// A directory row as the ledger spells it.
pub fn dir_row(rel: &[u8]) -> Vec<u8> {
    let mut row = rel.to_vec();
    row.push(b'/');
    row
}

/// Is this path REALLY still here, spelled exactly this way?
///
/// `Path::exists` is the obvious answer and the wrong one: macOS is
/// case-insensitive, so after `Foo.txt` → `foo.txt` it happily reports
/// that `Foo.txt` is still there — and the old spelling would survive
/// forever on the case-sensitive server. So the parent directory is
/// listed and the name compared byte for byte. Doomed sets are normally
/// empty, and the listing is cached per directory, so this costs nothing
/// in the common case.
fn still_on_disk(
    anchor: &Path,
    rel: &str,
    cache: &mut BTreeMap<PathBuf, BTreeSet<std::ffi::OsString>>,
) -> bool {
    let full = anchor.join(rel);
    let (Some(parent), Some(name)) = (full.parent(), full.file_name()) else {
        return false;
    };
    let names = cache.entry(parent.to_path_buf()).or_insert_with(|| {
        std::fs::read_dir(parent)
            .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default()
    });
    names.contains(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> WorkspaceStateKey {
        crate::config::WorkspaceKey::from_namespace("ledger-tests", Path::new(id))
            .unwrap()
            .state_key("test-server")
    }

    fn ledger_path(id: &str) -> Option<PathBuf> {
        super::ledger_path(&key(id))
    }

    fn read(id: &str) -> Ledger {
        super::read(&key(id))
    }

    fn store(
        id: &str,
        anchor: &Path,
        claimed: &[Vec<u8>],
        retired: &[String],
        retired_dirs: &[String],
    ) {
        super::store(&key(id), anchor, claimed, retired, retired_dirs);
    }

    fn forget(id: &str) {
        super::forget(&key(id));
    }

    fn doomed(id: &str, anchor: &Path, kept_now: &[Vec<u8>]) -> Vec<String> {
        super::doomed(&key(id), anchor, kept_now)
    }

    fn doomed_dirs(id: &str, anchor: &Path) -> Vec<String> {
        super::doomed_dirs(&key(id), anchor)
    }

    fn synced_files(id: &str) -> Vec<Vec<u8>> {
        super::synced_files(&key(id))
    }

    fn bytes(paths: &[&str]) -> Vec<Vec<u8>> {
        paths.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    /// The old wholesale write, for the tests that have to prove the
    /// merge is doing something: one anchor, one claim, nothing retired.
    fn claim(id: &str, anchor: &Path, paths: &[&str]) {
        store(id, anchor, &bytes(paths), &[], &[]);
    }

    fn rows(id: &str) -> BTreeSet<Vec<u8>> {
        read(id).all_rows().cloned().collect()
    }

    /// A deletion receipt belongs only to the server that issued it. The
    /// old workspace-only ledger let A retire B's row, after which B's stale
    /// copy looked container-born and the pull was allowed to recreate it.
    #[test]
    fn one_destination_cannot_retire_another_destinations_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        let workspace = crate::config::WorkspaceKey::from_namespace(
            "ledger-destinations",
            Path::new("/same/checkout/compose.yaml"),
        )
        .unwrap();
        let a = workspace.state_key("server-a");
        let b = workspace.state_key("server-b");
        std::fs::write(anchor.join("old.txt"), b"old").unwrap();
        let claimed = bytes(&["old.txt"]);
        super::store(&a, anchor, &claimed, &[], &[]);
        super::store(&b, anchor, &claimed, &[], &[]);

        std::fs::remove_file(anchor.join("old.txt")).unwrap();
        assert_eq!(super::doomed(&a, anchor, &[]), vec!["old.txt"]);
        assert_eq!(super::doomed(&b, anchor, &[]), vec!["old.txt"]);

        super::store(&a, anchor, &[], &["old.txt".to_string()], &[]);
        assert!(super::doomed(&a, anchor, &[]).is_empty());
        assert_eq!(
            super::doomed(&b, anchor, &[]),
            vec!["old.txt"],
            "server A's receipt must leave server B's ownership row intact"
        );

        super::forget(&a);
        super::forget(&b);
    }

    /// The whole point, in one test: a file the container created is not
    /// ours to delete; a file we sent and the user removed is.
    #[test]
    fn only_what_we_put_there_can_be_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        let id = format!("ledger-test-{}", std::process::id());

        // Last sync synced two files.
        std::fs::write(anchor.join("keep.rs"), b"x").unwrap();
        std::fs::write(anchor.join("gone.rs"), b"x").unwrap();
        claim(&id, anchor, &["keep.rs", "gone.rs"]);

        // The user deletes one; the container created a migration that
        // was never in the ledger.
        std::fs::remove_file(anchor.join("gone.rs")).unwrap();
        let now = bytes(&["keep.rs", "migrations/001.sql"]);

        assert_eq!(
            doomed(&id, anchor, &now),
            vec!["gone.rs"],
            "only the file we synced and the user removed"
        );
        forget(&id);
    }

    #[test]
    fn a_newly_excluded_file_is_not_deleted() {
        // It is still on disk — the user gitignored it, they did not
        // delete it, and a container may be reading it.
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        let id = format!("ledger-excl-{}", std::process::id());
        std::fs::write(anchor.join("build.log"), b"x").unwrap();
        claim(&id, anchor, &["build.log"]);

        assert!(doomed(&id, anchor, &[]).is_empty());
        forget(&id);
    }

    /// A reader can never see half a ledger.
    ///
    /// `passthrough::run` drops the per-workspace lock before it pulls,
    /// and `pull_back`'s creating pass hands rsync every row in here as
    /// its exclude list — so a ledger written by truncating in place can
    /// be read short by a passthrough running alongside, and a row
    /// missing from that list is a file `--ignore-existing` plants back
    /// in the repo after the user deleted it.
    ///
    /// Replacing the file by rename is what rules the short read out,
    /// and the INODE is what tells the two apart from outside: truncating
    /// in place keeps it, and a rename cannot — the replacement was
    /// allocated while the old file was still linked.
    #[test]
    fn a_stored_ledger_is_replaced_whole_and_never_truncated_in_place() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        let id = format!("ledger-atomic-{}", std::process::id());

        claim(&id, anchor, &["a.txt"]);
        let path = ledger_path(&id).expect("a state dir to write into");
        let first = std::fs::metadata(&path).unwrap().ino();

        claim(&id, anchor, &["b.txt"]);
        let second = std::fs::metadata(&path).unwrap().ino();
        assert_ne!(
            first, second,
            "the second store must land as a fresh file renamed over the old one, \
             not as a truncation a concurrent pull could read halfway"
        );

        // The detour through a temp file must not cost the 0600 the
        // ledger is written with — it names every path in the workspace.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the temp file carries the mode, not the rename"
        );

        let leftovers: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(&id) && n.contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a successful store leaves no scratch behind: {leftovers:?}"
        );

        forget(&id);
    }

    #[test]
    fn a_case_only_rename_retires_the_old_spelling() {
        // macOS says Foo.txt still exists after the rename to foo.txt;
        // the Linux server would then keep BOTH. Caught by the fixture
        // catalog, so the check compares names byte for byte.
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        let id = format!("ledger-case-{}", std::process::id());
        std::fs::write(anchor.join("Foo.txt"), b"x").unwrap();
        claim(&id, anchor, &["Foo.txt"]);

        std::fs::rename(anchor.join("Foo.txt"), anchor.join("foo.txt")).unwrap();
        assert_eq!(
            doomed(&id, anchor, &bytes(&["foo.txt"])),
            vec!["Foo.txt"],
            "the old spelling must be retired on the server"
        );
        forget(&id);
    }

    /// The hole that made `e2e_service` red every second run, in one
    /// test: ulak creates directories on the server (rsync does it for
    /// every push), so the ledger has to be able to say "I put that there
    /// and it is gone now". Without directory rows an empty directory sat
    /// on the server for good and the pull carried it into the repo.
    #[test]
    fn a_directory_ulak_put_there_can_also_be_retired() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        let id = format!("ledger-dirs-{}", std::process::id());

        std::fs::create_dir_all(anchor.join("site/stays")).unwrap();
        std::fs::create_dir_all(anchor.join("site/goes/deeper")).unwrap();
        std::fs::write(anchor.join("site/goes/deeper/x.txt"), b"x").unwrap();
        claim(
            &id,
            anchor,
            &[
                "site/",
                "site/goes/",
                "site/goes/deeper/",
                "site/goes/deeper/x.txt",
                "site/stays/",
            ],
        );

        // The user removes one subtree, whole.
        std::fs::remove_dir_all(anchor.join("site/goes")).unwrap();
        assert_eq!(
            doomed_dirs(&id, anchor),
            vec!["site/goes/deeper", "site/goes"],
            "deepest first, so a removed subtree comes apart from the leaves up"
        );
        // The file inside it is doomed by its own rule, and the directory
        // rows must not leak into that list — it drives `rm -f`.
        assert_eq!(
            doomed(&id, anchor, &[]),
            vec!["site/goes/deeper/x.txt"],
            "only files may reach the rm"
        );
        forget(&id);
    }

    /// What the pull's creating pass is handed, and the one row shape it
    /// must NOT be handed.
    ///
    /// Files: every one of them, so a file the user deleted can never be
    /// created here again — that is the whole fix for the lost deletion
    /// (`sync::pull_back`), and it does not care when the deletion
    /// landed. Directories: none of them, ever. rsync will not descend
    /// into an excluded directory, so excluding `site/` would leave the
    /// container's own work in it stranded on the server — the exact
    /// thing the down leg exists to bring home.
    #[test]
    fn the_pull_is_handed_files_to_refuse_and_never_a_directory() {
        let id = format!("ledger-synced-{}", std::process::id());
        claim(
            &id,
            Path::new("/repo"),
            &["site/", "site/index.html", "site/deep/", "site/deep/a.txt"],
        );
        assert_eq!(
            synced_files(&id),
            bytes(&["site/deep/a.txt", "site/index.html"]),
            "directory rows must not reach the pull's exclude list"
        );
        forget(&id);
    }

    #[test]
    fn a_newly_excluded_directory_is_not_deleted_either() {
        // Same promise as for files: excluding is not deleting. The
        // directory is still here, so a container may well be using it.
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        let id = format!("ledger-excl-dir-{}", std::process::id());
        std::fs::create_dir_all(anchor.join("build")).unwrap();
        claim(&id, anchor, &["build/"]);

        assert!(doomed_dirs(&id, anchor).is_empty());
        forget(&id);
    }

    #[test]
    fn without_a_ledger_nothing_is_deletable() {
        // First sync, or a wiped state dir: ulak has not claimed to
        // have put anything there, so it deletes nothing. Stale files are
        // recoverable; deleted ones are not.
        let tmp = tempfile::tempdir().unwrap();
        assert!(doomed("ledger-absent-xyz", tmp.path(), &[]).is_empty());
    }

    #[test]
    fn roundtrip_survives_odd_but_legal_names() {
        let id = format!("ledger-rt-{}", std::process::id());
        // The middle name is NFD on purpose (ü + n + i + U+0307): macOS
        // hands decomposed filenames back, and the ledger stores raw
        // bytes rather than normalizing, so a round trip has to prove it.
        // The last one starts with the byte that marks an anchor line —
        // a legal file name, and one a format without row markers would
        // read back as a section header.
        let paths = bytes(&["a b/c.txt", "üni̇code/file.rs", "we[ird]*.txt", "@sign.txt"]);
        store(&id, Path::new("/repo"), &paths, &[], &[]);
        assert_eq!(rows(&id), paths.iter().cloned().collect());
        forget(&id);
        assert!(rows(&id).is_empty());
    }

    /// The bug this format exists to end.
    ///
    /// `docker build .` and `docker run -v ./app:/app` share a workspace
    /// id, a remote directory and this file — but not a footprint. A
    /// wholesale write let the narrow one un-claim everything the wide
    /// one had put on the server, and both halves of that were permanent:
    /// nothing could retire those files afterwards, and the pull was free
    /// to plant one back after the user deleted it.
    #[test]
    fn a_narrow_run_cannot_unclaim_what_a_build_put_there() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        let id = format!("ledger-merge-{}", std::process::id());
        std::fs::create_dir_all(anchor.join("src")).unwrap();
        std::fs::create_dir_all(anchor.join("app")).unwrap();
        std::fs::write(anchor.join("src/main.rs"), b"x").unwrap();
        std::fs::write(anchor.join("app/index.js"), b"x").unwrap();

        // `docker build .` sent the whole tree…
        claim(
            &id,
            anchor,
            &["app/", "app/index.js", "src/", "src/main.rs"],
        );
        // …then `docker run -v ./app:/app` sent one directory out of it.
        claim(&id, anchor, &["app/", "app/index.js"]);

        assert!(
            synced_files(&id).contains(&b"src/main.rs".to_vec()),
            "the pull's creating pass excludes exactly this list; a row missing \
             from it is a file --ignore-existing may plant back in the repo"
        );

        // The user deletes the source file and builds again.
        std::fs::remove_file(anchor.join("src/main.rs")).unwrap();
        assert_eq!(
            doomed(&id, anchor, &bytes(&["app/index.js"])),
            vec!["src/main.rs"],
            "un-claimed is undeletable: nothing would ever retire this file again"
        );
        forget(&id);
    }

    /// Merging is not "never delete anything". A row leaves the ledger
    /// the moment the server really loses it — and only then.
    #[test]
    fn a_row_leaves_only_when_the_server_actually_lost_it() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        let id = format!("ledger-retire-{}", std::process::id());
        std::fs::create_dir_all(anchor.join("site")).unwrap();
        claim(&id, anchor, &["site/", "site/a.txt", "site/b.txt"]);

        // `rm -f` took a.txt; the rmdir of site/ was refused because the
        // container had written its own work into it.
        store(
            &id,
            anchor,
            &[],
            &["site/a.txt".to_string()],
            &[/* site/ survived */],
        );
        assert_eq!(
            rows(&id),
            bytes(&["site/", "site/b.txt"]).into_iter().collect(),
            "the removed file goes, the directory that refused to go stays claimed"
        );

        // …and staying claimed is what lets the next sync try again.
        std::fs::remove_dir(anchor.join("site")).unwrap();
        assert_eq!(doomed_dirs(&id, anchor), vec!["site"]);
        forget(&id);
    }

    /// `docker build ../sibling` files its rows against the PARENT of the
    /// workspace root; every `docker run` files them against the root.
    /// Same workspace id, same file — and the same row then means two
    /// different local paths, so the wrong writer answering "is it still
    /// on disk?" would delete a file the other one is still syncing.
    #[test]
    fn a_row_is_only_retired_by_a_writer_that_shares_its_anchor() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path();
        let inner = outer.join("repo");
        std::fs::create_dir_all(inner.join("src")).unwrap();
        std::fs::write(inner.join("src/main.rs"), b"x").unwrap();
        let id = format!("ledger-anchor-{}", std::process::id());

        // The build's anchor is the parent, so its row reads repo/src/….
        claim(&id, outer, &["repo/src/main.rs"]);
        // The run's anchor is the repo. `src/main.rs` is not on disk
        // relative to IT (the file is at repo/src/main.rs), but the row
        // belongs to another anchor and is not the run's to judge.
        assert!(
            doomed(&id, &inner, &[]).is_empty(),
            "a row measured from another anchor names a file this writer never saw"
        );
        // It is still on the server, so the pull must still refuse to
        // create it.
        assert_eq!(synced_files(&id), bytes(&["repo/src/main.rs"]));

        // The writer that DID file it can still retire it.
        std::fs::remove_file(inner.join("src/main.rs")).unwrap();
        assert_eq!(doomed(&id, outer, &[]), vec!["repo/src/main.rs"]);
        forget(&id);
    }

    /// A ledger an older ulak wrote has no anchors in it, and every row
    /// in it is a real file on the server. Refusing to judge them would
    /// freeze every existing workspace's deletions forever, so the first
    /// writer takes them on — which is exactly what the manifest already
    /// asserts, one anchor per workspace at a time.
    #[test]
    fn rows_from_a_ledger_without_anchors_are_adopted_not_orphaned() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        let id = format!("ledger-v1-{}", std::process::id());
        let path = ledger_path(&id).unwrap();
        crate::invocation::private_dir(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"keep.rs\ngone.rs\nsite/\n").unwrap();
        std::fs::write(anchor.join("keep.rs"), b"x").unwrap();

        assert_eq!(
            doomed(&id, anchor, &bytes(&["keep.rs"])),
            vec!["gone.rs"],
            "an upgrade must not make every file already up there undeletable"
        );
        assert_eq!(doomed_dirs(&id, anchor), vec!["site"]);

        store(
            &id,
            anchor,
            &bytes(&["keep.rs"]),
            &["gone.rs".to_string()],
            &[],
        );
        assert_eq!(
            rows(&id),
            bytes(&["keep.rs", "site/"]).into_iter().collect(),
            "and the rows come back filed under the anchor that claimed them"
        );
        forget(&id);
    }

    /// A version this binary does not know is refused, not reread as v1.
    ///
    /// "Not exactly the magic line" used to mean "an older ulak wrote
    /// it", and a newer one's file — what a downgrade leaves behind —
    /// went through the v1 door: every line became a row, markers and
    /// all. The `>` then travels as part of the path, so the pull's
    /// creating pass is handed `>site/index.html` while the REAL
    /// `site/index.html` is excluded by nothing and gets planted back in
    /// the repo after the user deleted it — the one failure this module
    /// exists to prevent. The next `store` adopts the mangled rows and
    /// re-prefixes them, so it compounds.
    #[test]
    fn a_ledger_from_a_newer_ulak_is_refused_rather_than_read_as_v1() {
        let id = format!("ledger-v3-{}", std::process::id());
        let path = ledger_path(&id).unwrap();
        crate::invocation::private_dir(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"#ulak-ledger 3\n@/repo\n>site/index.html\n").unwrap();

        assert!(
            synced_files(&id).is_empty(),
            "a row read out of a format we cannot parse is worse than no row: {:?}",
            synced_files(&id)
        );
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            doomed(&id, tmp.path(), &[]).is_empty() && doomed_dirs(&id, tmp.path()).is_empty(),
            "nothing may be deleted on the strength of a file we did not understand"
        );

        // The same door catches a ledger that acquired CRLF on its way
        // through something that rewrites text files: every row would
        // otherwise carry a trailing \r and name a file that is not
        // there.
        std::fs::write(&path, b"#ulak-ledger 2\r\n@/repo\r\n>site/index.html\r\n").unwrap();
        assert!(synced_files(&id).is_empty(), "{:?}", synced_files(&id));

        forget(&id);
    }

    /// The file's exact bytes, so the format cannot move by accident.
    ///
    /// MAGIC, ANCHOR and ROW are read by every ulak that comes after
    /// this one, off ledgers already on disk. Changing one of them
    /// silently turns every existing workspace into "a file we cannot
    /// read" — which is now refused rather than misparsed, but still
    /// means nothing already on the server can be retired. This test is
    /// the one thing that makes that a decision somebody takes.
    #[test]
    fn the_bytes_a_ledger_is_written_in_do_not_move_by_accident() {
        let mut ledger = Ledger::default();
        ledger.by_anchor.insert(
            anchor_key(Path::new("/repo")),
            bytes(&["site/", "site/index.html"]).into_iter().collect(),
        );
        assert_eq!(
            render(&ledger),
            b"#ulak-ledger 2\n@/repo\n>site/\n>site/index.html\n".to_vec(),
        );
        // And what we write is what we read: the version test above
        // means a mismatch here is no longer a silent reinterpretation,
        // it is a workspace that claims nothing.
        assert_eq!(version_of(&render(&ledger)), Version::Anchored);
    }

    #[test]
    fn an_anchor_with_a_newline_in_it_cannot_forge_a_row() {
        // A directory name may hold a newline; a row may not. The anchor
        // line is the only one that has to be escaped, and without it the
        // tail of the name would come back as rows nobody filed.
        let anchor = PathBuf::from("/od\nd\\path");
        let mut ledger = Ledger::default();
        ledger
            .by_anchor
            .insert(anchor_key(&anchor), bytes(&["a.txt"]).into_iter().collect());
        let back = parse(&render(&ledger));
        assert_eq!(
            back.rows_under(&anchor).cloned().collect::<Vec<_>>(),
            bytes(&["a.txt"])
        );
        assert!(back.unanchored.is_empty());
        assert_eq!(back.by_anchor.len(), 1);
    }
}
