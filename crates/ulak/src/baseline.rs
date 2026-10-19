//! Last common content, separate from the ledger's deletion authority.
//!
//! Times cannot distinguish an unsent edit from container output after a
//! restart. Only content observed equal on both ends advances this record.
//! A copy that moved away from the recorded common content is a conflict,
//! never an invitation to choose the other copy. Divergence with no record
//! at all is held by a pull, and a push follows `[sync] no_history`
//! (`may_push`; config.rs says why the default sends). Records belong to
//! destination AND anchor, like receipts. Two records ride along that are
//! not common content, and `may_push`/`may_pull` never read them: sends
//! whose receipt was lost (`State::unconfirmed`), and files coming back
//! because the server's edit won over a local deletion
//! (`State::returning`). A `Record` is `#[serde(default)]`, so one written
//! before a field existed reads that field as empty.
//!
//! A compact snapshot plus atomic numbered deltas avoids rewriting tens of
//! thousands of digests on every save. Snapshot publication precedes
//! retiring its deltas; sequence numbers make an interrupted compaction
//! safe. The workspace lock serializes writers. Corrupt state fails closed
//! instead of forgetting the common ancestor. This module neither selects
//! paths nor authorizes deletion.
//!
//! Fingerprints are read here and on the server by baseline.py, one
//! request per comparison. A full survey hands both ends a digest cache
//! (hashcache.rs owns when a recorded digest may answer) and asks by
//! difference: per-directory digests of the local fingerprints go up and
//! only the directories that disagree come down, so a quiet restart
//! receives almost nothing instead of a fingerprint for every file. The
//! small post-transfer confirmations ask for a plain list, answered through
//! the same caches under the same stamp rule. Only `remote` and `verdicts`
//! pass no cache and hash every file they name; their callers (the warm
//! path, deletion) ask about a few names. baseline.py refuses a python3
//! older than 3.7 before it reads anything, so a failed comparison names
//! that requirement.
//!
//! The deletion judgement asks with `verdicts`, and nothing else does. It
//! answers every doomed row in one request (`Copies`): the server's copy,
//! a structural verdict for a path the server restructured (a parent that
//! is no longer a real directory, a leaf that is neither a regular file
//! nor a symlink), or a hold for a row that cannot be read right now (a
//! file changing while it is read, a permission refusal, any other error).
//! baseline.py owns that classification; sync.rs owns what each verdict
//! does to the claim. The request fails whole only when the helper does
//! not answer it, which is an SSH failure like any other. Every other
//! request fails whole on any unreadable name: a survey or a confirmation
//! must not decide around a name it could not read, while a deletion
//! must be able to leave one row alone and judge the rest.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub type Contents = BTreeMap<String, Option<String>>;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Record {
    version: u32,
    sequence: u64,
    entries: Contents,
    /// Sends whose outcome is unknown: Some(content) ADDS one to the
    /// name's set, None settles them all. A record without the field
    /// reads it as empty (`#[serde(default)]`); its `entries` still settle
    /// their names on replay.
    unconfirmed: Contents,
    /// A snapshot's full sets, which `unconfirmed` cannot hold: added on
    /// replay like its Some values.
    unconfirmed_sets: BTreeMap<String, Vec<String>>,
    /// `State::returning`: true starts a name's return, false ends it. A
    /// snapshot lists every name still returning; a record without the
    /// field changes none.
    returning: BTreeMap<String, bool>,
}

pub struct State {
    root: PathBuf,
    sequence: u64,
    pub entries: Contents,
    /// Every content a channel exchange tried to put on the server under
    /// a name without learning whether it landed — a SET, because a retry
    /// whose receipt is lost again may be the one that arrived. It is NOT
    /// common content: `may_push`/`may_pull` never see it, because nothing
    /// proves both ends ever held it. It answers one question only, at
    /// deletion time — is the server's copy one ulak sent
    /// (`sync::never_landed`) — and any agreement recorded for the name
    /// settles it.
    unconfirmed: BTreeMap<String, std::collections::BTreeSet<String>>,
    /// Names whose claim `sync::retire_moved` retired so the server's edit
    /// could come home. The pull that lands one claims it again
    /// (`sync::claim_returned`), so a second local deletion is judged like
    /// any other. Kept here rather than in memory because that pull is
    /// often a later command's: a passthrough's `pull_back`, or the next
    /// sync after an error. Like `unconfirmed`, never common content.
    ///
    /// A return ends when the name is claimed (by that landing or by any
    /// push), when both ends are recorded without it (`record` with None,
    /// as a confirmed removal is), when the server is seen without it (a
    /// pull's listing, `sync::returns_the_server_lost`, or a deletion
    /// notice in `sync::pull_changes`), or when a deletion judgement stops
    /// tracking the name (`sync::retire_never_landed`,
    /// `sync::retire_restructured`). A mark left behind would claim a
    /// same-named file the server makes later, and a claim is a licence
    /// to delete it.
    returning: std::collections::BTreeSet<String>,
}

impl State {
    pub fn load(key: &crate::config::WorkspaceStateKey, anchor: &Path) -> Result<Self> {
        let root = crate::intent::workspace_state_dir_path(key)
            .ok_or_else(|| failure("the state directory is unavailable"))?
            .join("common")
            .join(format!(
                "{:x}",
                Sha256::digest(anchor.as_os_str().as_encoded_bytes())
            ));
        Self::read(root)
    }

    fn read(root: PathBuf) -> Result<Self> {
        let mut state = Self {
            root,
            sequence: 0,
            entries: Contents::new(),
            unconfirmed: BTreeMap::new(),
            returning: std::collections::BTreeSet::new(),
        };
        let snapshot = state.root.join("snapshot.json");
        if snapshot.exists() {
            let record = read_record(&snapshot)?;
            state.sequence = record.sequence;
            state.entries = record.entries;
            state.apply_unconfirmed(record.unconfirmed, record.unconfirmed_sets);
            state.apply_returning(record.returning);
        }
        let mut deltas = Vec::new();
        match std::fs::read_dir(&state.root) {
            Ok(dir) => {
                for entry in dir {
                    let path = entry?.path();
                    if path.extension().is_some_and(|e| e == "json") && path != snapshot {
                        deltas.push(path);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        deltas.sort();
        for path in deltas {
            let record = read_record(&path)?;
            if record.sequence <= state.sequence {
                continue;
            }
            if record.sequence != state.sequence + 1 {
                return Err(failure("a common-content receipt is missing"));
            }
            state.sequence = record.sequence;
            for name in record.entries.keys() {
                state.unconfirmed.remove(name);
            }
            state.entries.extend(record.entries);
            state.apply_unconfirmed(record.unconfirmed, record.unconfirmed_sets);
            state.apply_returning(record.returning);
        }
        Ok(state)
    }

    fn apply_returning(&mut self, delta: BTreeMap<String, bool>) {
        for (name, returning) in delta {
            if returning {
                self.returning.insert(name);
            } else {
                self.returning.remove(&name);
            }
        }
    }

    fn apply_unconfirmed(&mut self, delta: Contents, sets: BTreeMap<String, Vec<String>>) {
        for (name, sent) in delta {
            match sent {
                Some(sent) => {
                    self.unconfirmed.entry(name).or_default().insert(sent);
                }
                None => {
                    self.unconfirmed.remove(&name);
                }
            }
        }
        for (name, sent) in sets {
            self.unconfirmed.entry(name).or_default().extend(sent);
        }
    }

    fn write(
        &mut self,
        entries: Contents,
        unconfirmed: Contents,
        returning: BTreeMap<String, bool>,
    ) -> Result<()> {
        let sequence = self.sequence + 1;
        let record = Record {
            version: 1,
            sequence,
            entries,
            unconfirmed,
            unconfirmed_sets: BTreeMap::new(),
            returning,
        };
        write_record(&self.root.join(format!("{sequence:020}.json")), &record)?;
        self.sequence = sequence;
        for name in record.entries.keys() {
            self.unconfirmed.remove(name);
        }
        self.entries.extend(record.entries);
        self.apply_unconfirmed(record.unconfirmed, BTreeMap::new());
        self.apply_returning(record.returning);
        Ok(())
    }

    /// Whether `path` is on its way back (`returning` above).
    pub fn returning(&self, path: &str) -> bool {
        self.returning.contains(path)
    }

    /// Every name on its way back.
    pub fn returning_names(&self) -> std::collections::BTreeSet<String> {
        self.returning.clone()
    }

    /// Start the return of names whose claim is about to be retired so the
    /// server's copy can come home. Written BEFORE the claim is retired:
    /// an interruption between the two leaves a mark on a claimed name,
    /// which no landing reads, instead of a retired claim with no mark.
    pub fn mark_returning(&mut self, names: &[String]) -> Result<()> {
        let started: BTreeMap<String, bool> = names
            .iter()
            .filter(|p| !self.returning.contains(*p))
            .map(|p| (p.clone(), true))
            .collect();
        if started.is_empty() {
            return Ok(());
        }
        self.write(Contents::new(), Contents::new(), started)
    }

    /// End the return of whichever of `names` is returning. Ledger rows
    /// are accepted as they are; a name that is not UTF-8 was never marked.
    pub fn end_return<N: AsRef<[u8]>>(&mut self, names: impl IntoIterator<Item = N>) -> Result<()> {
        // Every push hands over its whole claim, and nearly always
        // nothing is returning.
        if self.returning.is_empty() {
            return Ok(());
        }
        let ended: BTreeMap<String, bool> = names
            .into_iter()
            .filter_map(|n| std::str::from_utf8(n.as_ref()).ok().map(str::to_owned))
            .filter(|p| self.returning.contains(p))
            .map(|p| (p, false))
            .collect();
        if ended.is_empty() {
            return Ok(());
        }
        self.write(Contents::new(), Contents::new(), ended)
    }

    /// Every content ulak tried to send under `path` without learning
    /// whether it landed.
    pub fn unconfirmed(&self, path: &str) -> Option<&std::collections::BTreeSet<String>> {
        self.unconfirmed.get(path)
    }

    /// Record sends before their outcome is known; each adds to its
    /// name's set.
    pub fn record_unconfirmed(&mut self, sent: BTreeMap<String, String>) -> Result<()> {
        if sent.is_empty() {
            return Ok(());
        }
        self.write(
            Contents::new(),
            sent.into_iter().map(|(p, c)| (p, Some(c))).collect(),
            BTreeMap::new(),
        )
    }

    /// Forget unconfirmed sends the server has since answered for.
    pub fn settle(&mut self, names: &[String]) -> Result<()> {
        let settled: Contents = names
            .iter()
            .filter(|p| self.unconfirmed.contains_key(*p))
            .map(|p| (p.clone(), None))
            .collect();
        if settled.is_empty() {
            return Ok(());
        }
        self.write(Contents::new(), settled, BTreeMap::new())
    }

    pub fn get(&self, path: &str) -> Option<&String> {
        self.entries.get(path).and_then(Option::as_ref)
    }

    pub fn record(&mut self, entries: Contents) -> Result<()> {
        let settled: Contents = entries
            .keys()
            .filter(|p| self.unconfirmed.contains_key(*p))
            .map(|p| (p.clone(), None))
            .collect();
        // Absent on both ends: nothing is left to come home.
        let ended: BTreeMap<String, bool> = entries
            .iter()
            .filter(|(p, v)| v.is_none() && self.returning.contains(*p))
            .map(|(p, _)| (p.clone(), false))
            .collect();
        let entries: Contents = entries
            .into_iter()
            .filter(|(p, v)| self.entries.get(p) != Some(v))
            .collect();
        if entries.is_empty() && settled.is_empty() && ended.is_empty() {
            return Ok(());
        }
        self.write(entries, settled, ended)
    }

    pub fn compact(&self) -> Result<()> {
        write_record(
            &self.root.join("snapshot.json"),
            &Record {
                version: 1,
                sequence: self.sequence,
                entries: self.entries.clone(),
                unconfirmed: Contents::new(),
                unconfirmed_sets: self
                    .unconfirmed
                    .iter()
                    .map(|(p, c)| (p.clone(), c.iter().cloned().collect()))
                    .collect(),
                returning: self.returning.iter().map(|p| (p.clone(), true)).collect(),
            },
        )?;
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok())
                .is_some_and(|n| n <= self.sequence)
                && path.extension().is_some_and(|e| e == "json")
            {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

fn read_record(path: &Path) -> Result<Record> {
    let record: Record = serde_json::from_slice(&std::fs::read(path)?)
        .with_context(|| failure("a common-content receipt cannot be read"))?;
    if record.version != 1 {
        return Err(failure("unsupported common-content receipt version"));
    }
    Ok(record)
}

fn write_record(path: &Path, record: &Record) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("tmp.{}", std::process::id()));
    crate::invocation::write_private(&temp, &serde_json::to_vec(record)?)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

fn failure(detail: &str) -> anyhow::Error {
    crate::ui::fail!("cannot establish common sync content: {detail}")
        .now("preserve both copies and restore the destination's common-content state before retrying")
        .into_err()
}

pub fn regular(bytes: &[u8], mode: u32) -> String {
    format!("f:{}:{:x}", mode & 0o7777, Sha256::digest(bytes))
}

pub fn fingerprint(path: &Path) -> Result<Option<String>> {
    Ok(stamped(path)?.map(|(value, _)| value))
}

/// The fingerprint together with the stamp the read was verified against:
/// what hashcache.rs may record, because that stamp is the one the bytes
/// were proven stable under.
pub fn stamped(path: &Path) -> Result<Option<(String, crate::selection::Stamp)>> {
    let before = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let value = if before.file_type().is_symlink() {
        format!(
            "l:{:x}",
            Sha256::digest(std::fs::read_link(path)?.as_os_str().as_encoded_bytes())
        )
    } else if before.is_file() {
        let mut file = std::fs::File::open(path)?;
        if crate::selection::Stamp::from_metadata(&file.metadata()?)
            != crate::selection::Stamp::from_metadata(&before)
        {
            return Err(failure("a selected file changed while opening it"));
        }
        let mut hash = Sha256::new();
        let mut buf = [0; 65536];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
        }
        format!("f:{}:{:x}", before.mode() & 0o7777, hash.finalize())
    } else {
        return Err(failure(
            "a selected leaf is no longer a regular file or symlink",
        ));
    };
    let stamp = crate::selection::Stamp::from_metadata(&before);
    if crate::selection::Stamp::read(path).as_ref() != Some(&stamp) {
        return Err(failure(
            "a selected file changed while hashing it; retry the sync",
        ));
    }
    Ok(Some((value, stamp)))
}

/// A synced directory's own fingerprint: its mode, or None when it is not
/// a directory here. Directories are never recorded as common content;
/// the survey compares them only to know which ones the push must name.
pub fn directories(anchor: &Path, names: &[Vec<u8>]) -> Result<Contents> {
    names
        .iter()
        .map(|p| {
            let name = std::str::from_utf8(p)?;
            let value = match std::fs::symlink_metadata(anchor.join(name)) {
                Ok(m) if m.is_dir() => Some(format!("d:{}", m.mode() & 0o7777)),
                Ok(_) => None,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            Ok((name.to_owned(), value))
        })
        .collect()
}

pub fn local(anchor: &Path, names: &[Vec<u8>]) -> Result<Contents> {
    names
        .iter()
        .map(|p| {
            let name = std::str::from_utf8(p)?;
            Ok((name.to_owned(), fingerprint(&anchor.join(name))?))
        })
        .collect()
}

/// The server's digest cache, beside the project like the stream's
/// coordination files and never inside it, so it can neither travel nor
/// collide with a project path.
pub fn remote_cache(project: &crate::config::Project) -> String {
    format!(
        "{}/.baseline/fingerprints.json",
        project.remote_workspace_root()
    )
}

pub fn remote(
    ssh: &crate::ssh::Ssh,
    root: &str,
    names: &[Vec<u8>],
    budget: &crate::proc::Budget,
) -> Result<Contents> {
    remote_run(remote_command(ssh, root, None), names, budget.remaining())
}

/// What the deletion judgement learned of the server's copies of the
/// doomed rows, from baseline.py's verdict request. Every row asked about
/// is in exactly one of the three.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Copies {
    /// The server's copy of each row it read; None where it has none.
    pub remote: Contents,
    /// Rows whose path the server restructured, each with why: a parent
    /// that is no longer a real directory, or a leaf that is neither a
    /// regular file nor a symlink. No byte was read through such a path.
    pub structural: BTreeMap<String, String>,
    /// Rows that cannot be read right now, each with why.
    pub held: BTreeMap<String, Held>,
}

/// Why a doomed row could not be read this time.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Held {
    pub why: String,
    /// The server refused the SSH user access: a file the server owns,
    /// which `[sync] protect` is for.
    pub denied: bool,
}

/// The deletion judgement's one request: a verdict for every doomed row
/// (`Copies`). It fails only when baseline.py does not answer, never
/// because of one row.
pub fn verdicts(
    ssh: &crate::ssh::Ssh,
    root: &str,
    names: &[String],
    budget: &crate::proc::Budget,
) -> Result<Copies> {
    verdicts_run(remote_command(ssh, root, None), names, budget.remaining())
}

fn verdicts_run(
    mut command: Command,
    names: &[String],
    limit: std::time::Duration,
) -> Result<Copies> {
    if names.is_empty() {
        return Ok(Copies::default());
    }
    let request = serde_json::json!({ "verdicts": names });
    let out = crate::proc::run_bounded(&mut command, Some(serde_json::to_vec(&request)?), limit)?;
    crate::audit::record_command("ssh-common", &command, out.status.code());
    answered(&out)?;
    let copies: Copies = serde_json::from_slice(&out.stdout)?;
    let mut judged: Vec<&str> = copies
        .remote
        .keys()
        .chain(copies.structural.keys())
        .chain(copies.held.keys())
        .map(String::as_str)
        .collect();
    judged.sort_unstable();
    let asked: std::collections::BTreeSet<&str> = names.iter().map(String::as_str).collect();
    if !judged.iter().copied().eq(asked.iter().copied()) {
        return Err(failure(
            "the server returned an incomplete content inventory",
        ));
    }
    Ok(copies)
}

/// The error every refused comparison gives, carrying the server's reason.
fn refused(reason: &str) -> anyhow::Error {
    crate::ui::fail!("could not compare the server's content before sync: {reason}")
        .now("run ulak doctor, which checks SSH and the server's python3 (syncing needs Python 3.7 or later), then retry; neither copy is chosen on a failed comparison")
        .into_err()
}

/// `remote`, answered by baseline.py run here over `root`: the server's
/// half of a comparison without a server.
#[cfg(test)]
pub(crate) fn local_remote(root: &Path, names: &[Vec<u8>]) -> Result<Contents> {
    remote_run(
        local_command(root),
        names,
        std::time::Duration::from_secs(5),
    )
}

/// `verdicts`, answered the same way as `local_remote`.
#[cfg(test)]
pub(crate) fn local_verdicts(root: &Path, names: &[String]) -> Result<Copies> {
    local_verdicts_after("", root, names)
}

/// `local_verdicts` with `prelude` run first in the same interpreter, so a
/// test can change what the helper meets, such as a file that changes
/// while it is read, at a moment no outside process could hit reliably.
#[cfg(test)]
pub(crate) fn local_verdicts_after(prelude: &str, root: &Path, names: &[String]) -> Result<Copies> {
    let mut command = crate::stream::python();
    command
        .arg("-c")
        .arg(format!("{prelude}\n{}", include_str!("baseline.py")))
        .arg(root);
    verdicts_run(command, names, std::time::Duration::from_secs(5))
}

/// A `local_verdicts_after` prelude under which every block baseline.py
/// hashes from `path` appends a byte to it, so the file changes during
/// the read as a file being written does.
#[cfg(test)]
pub(crate) fn rewritten_while_read(path: &Path) -> String {
    r#"
import hashlib
_sha256 = hashlib.sha256
class _Rewriting(object):
    def __init__(self, *data):
        self.inner = _sha256(*data)
    def update(self, data):
        if data.startswith(MARK):
            with open(TARGET, 'ab') as target:
                target.write(b'!')
        self.inner.update(data)
    def hexdigest(self):
        return self.inner.hexdigest()
hashlib.sha256 = _Rewriting
"#
    .replace("MARK", &format!("{:?}.encode()", REWRITTEN))
    .replace(
        "TARGET",
        &format!("{:?}", path.to_str().unwrap_or_default()),
    )
}

/// What a file must begin with for `rewritten_while_read` to rewrite it.
#[cfg(test)]
pub(crate) const REWRITTEN: &str = "rewritten:";

#[cfg(test)]
fn local_command(root: &Path) -> Command {
    let mut command = crate::stream::python();
    command.args(["-c", include_str!("baseline.py")]).arg(root);
    command
}

/// One directory's digest, identical on both ends (baseline.py
/// `digest_of`): SHA-256 over `name\0fingerprint\n` in the given order,
/// `-` for a missing file.
pub fn digest_of<'a>(pairs: impl IntoIterator<Item = (&'a str, Option<&'a String>)>) -> String {
    let mut hash = Sha256::new();
    for (name, fingerprint) in pairs {
        hash.update(name.as_bytes());
        hash.update(b"\0");
        hash.update(fingerprint.map_or("-", String::as_str).as_bytes());
        hash.update(b"\n");
    }
    format!("{:x}", hash.finalize())
}

/// The server's half of a full survey, answered by difference: only the
/// directories whose digest of the client's fingerprints differs come
/// back, with every name of that directory, plus directories whose mode
/// differs. `Answer::merge` turns that into the full remote inventory.
#[derive(Default, Deserialize)]
#[serde(default)]
pub struct Answer {
    differs: BTreeMap<String, Contents>,
    dirmodes: Contents,
}

impl Answer {
    /// The remote fingerprint of every local name: the server's where it
    /// answered, the local one where the digests agreed.
    pub fn merge(self, local: &Contents, local_dirs: &Contents) -> Result<(Contents, Contents)> {
        let mut remote = Contents::new();
        for (name, value) in local {
            let (dir, base) = split(name);
            let answered = match self.differs.get(dir) {
                Some(entries) => Some(entries.get(base).ok_or_else(|| {
                    failure("the server returned an incomplete content inventory")
                })?),
                None => None,
            };
            remote.insert(
                name.clone(),
                answered.cloned().unwrap_or_else(|| value.clone()),
            );
        }
        let mut dirs = local_dirs.clone();
        for (dir, value) in self.dirmodes {
            if !dirs.contains_key(&dir) {
                return Err(failure(
                    "the server answered for a directory it was not asked about",
                ));
            }
            dirs.insert(dir, value);
        }
        Ok((remote, dirs))
    }
}

fn split(name: &str) -> (&str, &str) {
    name.rsplit_once('/').unwrap_or(("", name))
}

/// The full survey: every local fingerprint, grouped by directory with a
/// digest each, so the server answers only where the two ends disagree.
pub fn survey_in_background(
    ssh: &crate::ssh::Ssh,
    root: &str,
    cache: &str,
    local: &Contents,
    local_dirs: &Contents,
    budget: &crate::proc::Budget,
) -> std::thread::JoinHandle<Result<Answer>> {
    let mut dirs: BTreeMap<&str, Vec<(&str, Option<&String>)>> = BTreeMap::new();
    for (name, value) in local {
        let (dir, base) = split(name);
        dirs.entry(dir).or_default().push((base, value.as_ref()));
    }
    let request = serde_json::json!({
        "dirs": dirs.iter().map(|(dir, pairs)| {
            (dir.to_string(), serde_json::json!({
                "names": pairs.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
                "digest": digest_of(pairs.iter().copied()),
            }))
        }).collect::<serde_json::Map<String, serde_json::Value>>(),
        "dirmodes": local_dirs,
    });
    let mut command = remote_command(ssh, root, Some(cache));
    let payload = serde_json::to_vec(&request);
    let limit = budget.remaining();
    std::thread::spawn(move || {
        let out = crate::proc::run_bounded(&mut command, Some(payload?), limit)?;
        crate::audit::record_command("ssh-common", &command, out.status.code());
        answered(&out)?;
        Ok(serde_json::from_slice(&out.stdout)?)
    })
}

/// The confirmation's request: a plain list of names, answered in full.
pub fn remote_in_background(
    ssh: &crate::ssh::Ssh,
    root: &str,
    cache: &str,
    names: &[Vec<u8>],
    budget: &crate::proc::Budget,
) -> std::thread::JoinHandle<Result<Contents>> {
    let command = remote_command(ssh, root, Some(cache));
    let names = names.to_vec();
    let limit = budget.remaining();
    std::thread::spawn(move || remote_run(command, &names, limit))
}

fn remote_command(ssh: &crate::ssh::Ssh, root: &str, cache: Option<&str>) -> Command {
    let mut command = ssh.command();
    command.arg("--").arg(&ssh.dest).arg(format!(
        "python3 -c {} {}{}",
        crate::ssh::sh_quote(include_str!("baseline.py")),
        crate::ssh::sh_quote(root),
        cache
            .map(|c| format!(" {}", crate::ssh::sh_quote(c)))
            .unwrap_or_default()
    ));
    command
}

fn remote_run(
    mut command: Command,
    names: &[Vec<u8>],
    limit: std::time::Duration,
) -> Result<Contents> {
    if names.is_empty() {
        return Ok(Contents::new());
    }
    let names: Vec<_> = names
        .iter()
        .map(|p| std::str::from_utf8(p))
        .collect::<std::result::Result<_, _>>()?;
    let out = crate::proc::run_bounded(&mut command, Some(serde_json::to_vec(&names)?), limit)?;
    crate::audit::record_command("ssh-common", &command, out.status.code());
    answered(&out)?;
    let values: Contents = serde_json::from_slice(&out.stdout)?;
    if names.iter().any(|p| !values.contains_key(*p)) || values.len() != names.len() {
        return Err(failure(
            "the server returned an incomplete content inventory",
        ));
    }
    Ok(values)
}

/// Whether baseline.py answered; one answer for every request.
fn answered(out: &crate::proc::Bounded) -> Result<()> {
    if !out.timed_out && out.status.success() {
        return Ok(());
    }
    Err(refused(&reason(out)))
}

/// Why a request was not answered. The server's last stderr line rides
/// along, because the causes read alike otherwise: baseline.py's own
/// refusal of a python3 older than 3.7, an interpreter's traceback, or
/// ssh's reason for not connecting.
fn reason(out: &crate::proc::Bounded) -> String {
    if out.timed_out {
        return "the server did not answer in time".to_owned();
    }
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("the server gave no reason")
        .to_owned()
}

/// Missing remote content may be restored; a differing existing copy needs
/// proof that it is still the common ancestor. Divergence with no ancestor
/// on record follows `[sync] no_history`: sent, as before content records
/// existed, or held — config.rs says why the default is to send.
pub fn may_push(
    no_history: crate::config::NoHistory,
    base: Option<&String>,
    local: Option<&String>,
    remote: Option<&String>,
) -> bool {
    remote.is_none()
        || local == remote
        || (base.is_some() && remote == base)
        || (base.is_none() && local.is_some() && no_history == crate::config::NoHistory::Send)
}

pub fn may_pull(base: Option<&String>, local: Option<&String>, remote: Option<&String>) -> bool {
    local == remote || (base.is_some() && local == base)
}

pub fn equal(local: &Contents, remote: &Contents) -> Contents {
    local
        .iter()
        .filter(|(p, v)| remote.get(*p) == Some(*v))
        .map(|(p, v)| (p.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A lost receipt's send survives a restart and a compaction, stays
    /// out of the common content the push and pull rules read, and is
    /// settled by any agreement recorded for the name.
    #[test]
    fn an_unconfirmed_send_is_remembered_apart_until_something_settles_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("common");
        let mut state = State::read(root.clone()).unwrap();
        state
            .record_unconfirmed(BTreeMap::from([
                ("a.rs".to_string(), "f:420:aa".to_string()),
                ("b.rs".to_string(), "f:420:bb".to_string()),
            ]))
            .unwrap();
        // A retry of b.rs whose receipt is lost again adds to the set.
        state
            .record_unconfirmed(BTreeMap::from([(
                "b.rs".to_string(),
                "f:420:b2".to_string(),
            )]))
            .unwrap();
        let state2 = State::read(root.clone()).unwrap();
        assert!(state2.unconfirmed("a.rs").unwrap().contains("f:420:aa"));
        assert!(state2.get("a.rs").is_none(), "never common content");
        state2.compact().unwrap();
        let mut state = State::read(root.clone()).unwrap();
        assert_eq!(
            state
                .unconfirmed("b.rs")
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            ["f:420:b2", "f:420:bb"],
            "both attempts survive a compaction"
        );
        // Agreement on a.rs settles it, even when the record is unchanged.
        state
            .record(Contents::from([(
                "a.rs".to_string(),
                Some("f:420:aa".to_string()),
            )]))
            .unwrap();
        state.settle(&["b.rs".to_string()]).unwrap();
        let state = State::read(root).unwrap();
        assert!(state.unconfirmed("a.rs").is_none());
        assert!(state.unconfirmed("b.rs").is_none());
        assert_eq!(state.get("a.rs").map(String::as_str), Some("f:420:aa"));
    }

    /// A file on its way back stays marked across a restart and a
    /// compaction, because the pull that lands it is often a later
    /// command's; kept in memory, the mark reached only `ulak sync`, and a
    /// file spared under `ulak docker compose` came home unclaimed. It ends
    /// only when told to, or when both ends are recorded without the file,
    /// and never becomes common content.
    #[test]
    fn a_return_survives_a_restart_and_a_compaction_until_something_ends_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("common");
        let mut state = State::read(root.clone()).unwrap();
        let names = ["a.rs", "b.rs", "c.rs", "d.rs"].map(String::from);
        state.mark_returning(&names).unwrap();
        let sequence = state.sequence;
        state.mark_returning(&names[..1]).unwrap();
        assert_eq!(state.sequence, sequence, "a second mark writes nothing");

        let state = State::read(root.clone()).unwrap();
        state.compact().unwrap();
        let mut state = State::read(root.clone()).unwrap();
        for name in &names {
            assert!(state.returning(name), "{name} lost its mark");
            assert!(state.get(name).is_none(), "{name}: never common content");
        }

        state
            .end_return([b"a.rs".to_vec(), b"dir/".to_vec()])
            .unwrap();
        state
            .record(Contents::from([
                ("b.rs".into(), Some("f:420:b".into())),
                ("c.rs".into(), None),
            ]))
            .unwrap();
        // A delta written before the field existed carries no returns.
        let next = state.sequence + 1;
        std::fs::write(
            root.join(format!("{next:020}.json")),
            format!(r#"{{"version":1,"sequence":{next},"entries":{{"e.rs":"f:420:e"}}}}"#),
        )
        .unwrap();
        let state = State::read(root).unwrap();
        assert!(!state.returning("a.rs"), "ended by name");
        assert!(
            state.returning("b.rs"),
            "agreed content alone does not end it"
        );
        assert!(
            !state.returning("c.rs"),
            "absent on both ends: nothing to bring home"
        );
        assert!(state.returning("d.rs"));
        assert_eq!(state.get("e.rs").map(String::as_str), Some("f:420:e"));
    }

    /// The server and client must agree on binary data, modes and symlink
    /// bytes; a symlinked ancestor must not expose an unrelated directory.
    #[test]
    fn server_fingerprints_match_local_content_and_refuse_symlinked_parents() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("binary ş"), [0, 255, 10, 128]).unwrap();
        std::os::unix::fs::symlink("binary ş", dir.path().join("link")).unwrap();
        let names = [
            b"binary \xc5\x9f".to_vec(),
            b"link".to_vec(),
            b"missing".to_vec(),
        ];
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let sub = std::process::Command::new("python3")
            .args(["-c", include_str!("baseline.py")])
            .arg(dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write as _;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(br#"["sub", "link"]"#)?;
                child.wait_with_output()
            })
            .expect("install python3 to exercise the remote content protocol");
        let dirs: Contents = serde_json::from_slice(&sub.stdout).unwrap();
        assert_eq!(
            dirs.get("sub"),
            directories(dir.path(), &[b"sub".to_vec()])
                .unwrap()
                .get("sub"),
            "a directory answers with its mode on both ends"
        );
        assert!(dirs["sub"].as_deref().unwrap().starts_with("d:"));
        assert_eq!(
            directories(dir.path(), &[b"link".to_vec()]).unwrap()["link"],
            None
        );
        let mut command = std::process::Command::new("python3");
        command
            .args(["-c", include_str!("baseline.py")])
            .arg(dir.path());
        let out = crate::proc::run_bounded(
            &mut command,
            Some(serde_json::to_vec(&["binary ş", "link", "missing"]).unwrap()),
            std::time::Duration::from_secs(5),
        )
        .expect("install python3 to exercise the remote content protocol");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let remote: Contents = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(remote.len(), 3);
        assert_eq!(remote, local(dir.path(), &names).unwrap());

        // The server's digest cache, under hashcache.rs's rule. A planted
        // entry whose stamp matches and whose hashing moment lies past the
        // slack is served without reading; a size change is a miss; a
        // corrupt cache is an empty one. The file is private.
        let cache = dir.path().join("state/.baseline/fingerprints.json");
        let mut cached = std::process::Command::new("python3");
        cached
            .args(["-c", include_str!("baseline.py")])
            .arg(dir.path())
            .arg(&cache);
        let ask = |cmd: &mut std::process::Command| -> Contents {
            let out = crate::proc::run_bounded(
                cmd,
                Some(serde_json::to_vec(&["binary ş", "link", "missing"]).unwrap()),
                std::time::Duration::from_secs(5),
            )
            .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice(&out.stdout).unwrap()
        };
        assert_eq!(ask(&mut cached), remote);
        assert_eq!(
            std::fs::metadata(&cache).unwrap().mode() & 0o777,
            0o600,
            "the cache names every synced path"
        );
        let mut file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        let entry = &mut file["entries"]["binary ş"];
        assert_eq!(entry.as_array().map(Vec::len), Some(8));
        entry[6] = serde_json::json!(entry[6].as_i64().unwrap() + 60_000_000_000i64);
        entry[7] = serde_json::json!("f:644:planted");
        assert!(file["entries"]["missing"].is_null());
        std::fs::write(&cache, serde_json::to_vec(&file).unwrap()).unwrap();
        assert_eq!(
            ask(&mut cached)
                .get("binary ş")
                .cloned()
                .flatten()
                .as_deref(),
            Some("f:644:planted"),
            "a trusted entry answers without opening the file"
        );
        std::fs::write(dir.path().join("binary ş"), [0, 255, 10, 128, 7]).unwrap();
        assert_eq!(
            ask(&mut cached).get("binary ş"),
            local(dir.path(), &names[..1]).unwrap().get("binary ş")
        );
        std::fs::write(&cache, b"{").unwrap();
        assert_eq!(ask(&mut cached), local(dir.path(), &names).unwrap());

        // The survey by difference: a directory whose digest matches is
        // absent from the answer; one that differs comes back whole; a
        // directory's mode is answered only when it differs; and the same
        // digest function runs on both ends.
        std::fs::create_dir_all(dir.path().join("same")).unwrap();
        std::fs::create_dir_all(dir.path().join("diff")).unwrap();
        std::fs::write(dir.path().join("same/a"), b"a").unwrap();
        std::fs::write(dir.path().join("diff/b"), b"server").unwrap();
        std::fs::write(dir.path().join("diff/c"), b"c").unwrap();
        let mine = |bytes: &[u8]| Some(regular(bytes, 0o644));
        let local_files: Contents = [
            ("same/a".to_owned(), mine(b"a")),
            ("diff/b".to_owned(), mine(b"local")),
            ("diff/c".to_owned(), mine(b"c")),
            ("diff/gone".to_owned(), None),
        ]
        .into_iter()
        .collect();
        let local_dirs: Contents = [
            ("same".to_owned(), Some("d:493".into())),
            ("diff".to_owned(), Some("d:448".into())),
        ]
        .into_iter()
        .collect();
        let dir_of = |d: &str| {
            let pairs: Vec<_> = local_files
                .iter()
                .filter(|(n, _)| split(n).0 == d)
                .map(|(n, v)| (split(n).1, v.as_ref()))
                .collect();
            serde_json::json!({
                "names": pairs.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
                "digest": digest_of(pairs.iter().copied()),
            })
        };
        let request = serde_json::json!({
            "dirs": {"same": dir_of("same"), "diff": dir_of("diff")},
            "dirmodes": local_dirs,
        });
        let mut survey = std::process::Command::new("python3");
        survey
            .args(["-c", include_str!("baseline.py")])
            .arg(dir.path());
        let out = crate::proc::run_bounded(
            &mut survey,
            Some(serde_json::to_vec(&request).unwrap()),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let answer: Answer = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(answer.differs.keys().collect::<Vec<_>>(), ["diff"]);
        assert_eq!(answer.differs["diff"].len(), 3);
        assert_eq!(answer.differs["diff"]["gone"], None);
        assert_eq!(answer.dirmodes.len(), 1, "{:?}", answer.dirmodes);
        let (remote, dirs) = answer.merge(&local_files, &local_dirs).unwrap();
        assert_eq!(remote["same/a"], mine(b"a"));
        assert_eq!(remote["diff/b"], mine(b"server"));
        assert_eq!(remote["diff/c"], mine(b"c"));
        assert_eq!(remote["diff/gone"], None);
        assert_eq!(dirs["same"], Some("d:493".into()));
        assert_ne!(dirs["diff"], Some("d:448".into()));

        std::fs::write(outside.path().join("secret"), b"private").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        let out = crate::proc::run_bounded(
            &mut command,
            Some(br#"["escape/secret"]"#.to_vec()),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
    }

    /// A confirmation that asks about two names keeps the digests a full
    /// survey recorded for every other name; a full survey drops only the
    /// names its directories no longer hold. Measured: every list-mode
    /// call rewrote the cache with just the names it asked about, so the
    /// next restart re-hashed the whole server tree.
    #[test]
    fn a_partial_query_keeps_the_servers_digest_cache_and_a_survey_prunes_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(dir.path().join("src").join(name), name).unwrap();
        }
        let cache = dir.path().join("state/fingerprints.json");
        let run = |request: serde_json::Value| {
            let mut command = std::process::Command::new("python3");
            command
                .args(["-c", include_str!("baseline.py")])
                .arg(dir.path())
                .arg(&cache);
            let out = crate::proc::run_bounded(
                &mut command,
                Some(serde_json::to_vec(&request).unwrap()),
                std::time::Duration::from_secs(5),
            )
            .expect("install python3 to exercise the remote content protocol");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let cached = || -> Vec<String> {
            let file: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
            file["entries"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };
        let survey =
            || serde_json::json!({"dirs": {"src": {"names": ["a", "b", "c"], "digest": ""}}});
        run(survey());
        assert_eq!(cached(), ["src/a", "src/b", "src/c"]);
        run(serde_json::json!(["src/a"]));
        assert_eq!(
            cached(),
            ["src/a", "src/b", "src/c"],
            "a partial query merges"
        );
        std::fs::remove_file(dir.path().join("src/c")).unwrap();
        run(serde_json::json!({"dirs": {"src": {"names": ["a", "b"], "digest": ""}}}));
        assert_eq!(cached(), ["src/a", "src/b"], "a surveyed directory prunes");
    }

    /// Restart and clock order must not turn independent remote work into
    /// permission to overwrite it, including unknown preexisting contents.
    #[test]
    fn only_the_unchanged_side_may_be_overwritten() {
        use crate::config::NoHistory::{Hold, Send};
        let a = "base".to_owned();
        let l = "local".to_owned();
        let r = "remote".to_owned();
        for no_history in [Hold, Send] {
            assert!(may_push(no_history, Some(&a), Some(&l), Some(&a)));
            assert!(!may_pull(Some(&a), Some(&l), Some(&a)));
            assert!(!may_push(no_history, Some(&a), Some(&a), Some(&r)));
            assert!(may_pull(Some(&a), Some(&a), Some(&r)));
            // A recorded ancestor keeps the conflict rule whatever the setting.
            assert!(!may_push(no_history, Some(&a), Some(&l), Some(&r)));
            for base in [None, Some(&a)] {
                assert!(!may_pull(base, Some(&l), Some(&r)));
                assert!(may_push(no_history, base, Some(&l), None));
                assert!(may_push(no_history, base, Some(&l), Some(&l)));
            }
        }
        // No ancestor on record: the setting decides, and a copy gone from
        // disk is never "sent".
        assert!(!may_push(Hold, None, Some(&l), Some(&r)));
        assert!(may_push(Send, None, Some(&l), Some(&r)));
        assert!(!may_push(Send, None, None, Some(&r)));
    }

    /// An interrupted snapshot cleanup must not replay old deltas over a
    /// newer common ancestor; truncated receipts must stop recovery.
    #[test]
    fn restart_replays_atomic_deltas_and_compaction_keeps_the_latest_content() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::read(dir.path().to_owned()).unwrap();
        state
            .record(BTreeMap::from([("a".into(), Some("old".into()))]))
            .unwrap();
        let old = std::fs::read(dir.path().join("00000000000000000001.json")).unwrap();
        state
            .record(BTreeMap::from([("a".into(), Some("new".into()))]))
            .unwrap();
        state.compact().unwrap();
        std::fs::write(dir.path().join("00000000000000000001.json"), old).unwrap();
        assert_eq!(
            State::read(dir.path().to_owned()).unwrap().get("a"),
            Some(&"new".into())
        );
        assert_eq!(
            std::fs::metadata(dir.path().join("snapshot.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::write(dir.path().join("00000000000000000003.json"), b"{").unwrap();
        assert!(State::read(dir.path().to_owned()).is_err());
    }

    /// baseline.py needs Python 3.7 (`time.time_ns`). Before its own check
    /// an older python3 failed inside the script, and the sync told a
    /// server that had Python 3 to install Python 3. The refusal is one
    /// stderr line naming the version found, the error carries that line
    /// with the step that checks it, and the same script under 3.7
    /// answers.
    #[test]
    fn an_old_python3_is_refused_by_version_and_the_error_says_what_to_run() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"a").unwrap();
        let pretend = |version: &str| {
            let mut command = crate::stream::python();
            command
                .arg("-c")
                .arg(format!(
                    "import sys\nsys.version_info = {version}\n{}",
                    include_str!("baseline.py")
                ))
                .arg(dir.path());
            command
        };
        let old = "(3, 6, 15, 'final', 0)";
        let limit = std::time::Duration::from_secs(5);

        let out =
            crate::proc::run_bounded(&mut pretend(old), Some(br#"["a"]"#.to_vec()), limit).unwrap();
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "ulak needs Python 3.7 or later on the server; python3 there is 3.6\n"
        );

        let err = remote_run(pretend(old), &[b"a".to_vec()], limit).unwrap_err();
        let shown = crate::ui::flatten(&err);
        for needed in [
            "python3 there is 3.6",
            "run ulak doctor",
            "Python 3.7 or later",
            "neither copy is chosen",
        ] {
            assert!(shown.contains(needed), "the error lost {needed:?}: {shown}");
        }

        let answer = remote_run(pretend("(3, 7, 0, 'final', 0)"), &[b"a".to_vec()], limit).unwrap();
        assert_eq!(answer, local(dir.path(), &[b"a".to_vec()]).unwrap());
    }

    /// The deletion judgement's request answers every doomed row in one
    /// round trip, whatever the server did to it. baseline.py used to fail
    /// the whole request on one row it could not read, so a single symlinked
    /// parent sent the round to a one-by-one fallback that stopped every
    /// sync past 64 rows, and every error, a file being written included,
    /// was taken for a restructured path and untracked for good. A path the
    /// server restructured is a structural verdict, and nothing under a
    /// symlinked parent is read. A row that cannot be read right now is
    /// held: a file that changes while it is read, and one the SSH user may
    /// not read, which is marked as a permission refusal. The rest are read
    /// as usual.
    #[test]
    fn each_doomed_row_gets_a_copy_a_structural_verdict_or_a_hold_in_one_request() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a"), b"a").unwrap();
        std::os::unix::fs::symlink("a", root.join("link")).unwrap();
        std::fs::write(root.join("plain"), b"a file").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(outside.path().join("secret"), b"private").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();
        let fifo = std::process::Command::new("mkfifo")
            .arg(root.join("pipe"))
            .status()
            .expect("mkfifo, from coreutils, to make a FIFO on the test server");
        assert!(fifo.success(), "mkfifo failed");
        std::fs::write(
            root.join("busy"),
            format!("{REWRITTEN} a file being written"),
        )
        .unwrap();
        let locked = root.join("locked");
        std::fs::write(&locked, b"the server's own").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            std::fs::File::open(&locked).is_err(),
            "a mode-000 file is readable here: run the tests as a user other than root"
        );
        let busy = rewritten_while_read(&root.join("busy"));
        let rows = [
            "../outside",
            "a",
            "busy",
            "escape/secret",
            "gone/x",
            "link",
            "locked",
            "missing",
            "pipe",
            "plain/b.txt",
            "sub",
        ]
        .map(String::from);
        let copies = local_verdicts_after(&busy, root, &rows).unwrap();

        let read = local(root, &[b"a".to_vec(), b"link".to_vec()]).unwrap();
        assert_eq!(
            copies.remote,
            Contents::from([
                ("a".into(), read["a"].clone()),
                ("gone/x".into(), None),
                ("link".into(), read["link"].clone()),
                ("missing".into(), None),
            ])
        );
        let structural: Vec<(&str, &str)> = copies
            .structural
            .iter()
            .map(|(p, why)| (p.as_str(), why.as_str()))
            .collect();
        assert_eq!(
            structural,
            [
                ("../outside", "../outside is not a path inside the project"),
                ("escape/secret", "escape is a symlink"),
                ("pipe", "pipe is a special file"),
                ("plain/b.txt", "plain is a file"),
                ("sub", "sub is a directory"),
            ]
        );
        let held: Vec<(&str, &str, bool)> = copies
            .held
            .iter()
            .map(|(p, h)| (p.as_str(), h.why.as_str(), h.denied))
            .collect();
        assert_eq!(
            held,
            [
                ("busy", "it changed while it was read", false),
                ("locked", "the SSH user may not read it", true),
            ]
        );
        let secret = local(outside.path(), &[b"secret".to_vec()]).unwrap()["secret"]
            .clone()
            .unwrap();
        assert!(
            !format!("{copies:?}").contains(&secret),
            "a byte was read through the symlinked parent"
        );

        // Accepting: once the file is still, the same row is read.
        let calm = local_verdicts(root, &["busy".to_string()]).unwrap();
        assert!(calm.held.is_empty(), "{calm:?}");
        assert_eq!(
            calm.remote["busy"],
            local(root, &[b"busy".to_vec()]).unwrap()["busy"]
        );
    }

    /// The verdict request fails whole only when the helper does not
    /// answer it: a missing project directory, ssh's own failure (255), a
    /// missing python3 (127), a timeout, or an answer that leaves a row
    /// out. Any of those is an SSH failure like any other and names the
    /// server's reason; none of them is a verdict on a row.
    #[test]
    fn a_verdict_request_fails_whole_only_when_the_helper_does_not_answer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plain"), b"a file").unwrap();
        let rows = ["plain/b.txt".to_string()];
        let answered = local_verdicts(dir.path(), &rows).unwrap();
        assert_eq!(answered.structural.len(), 1, "{answered:?}");

        let missing = local_verdicts(&dir.path().join("no-such-workspace"), &rows)
            .expect_err("a missing project directory is not a verdict");
        assert!(
            crate::ui::flatten(&missing).contains("could not compare"),
            "{}",
            crate::ui::flatten(&missing)
        );
        let limit = std::time::Duration::from_secs(5);
        for code in [255, 127] {
            let mut failing = Command::new("sh");
            failing.args([
                "-c",
                &format!("cat >/dev/null; echo 'ssh: connect to host: refused' >&2; exit {code}"),
            ]);
            let error = verdicts_run(failing, &rows, limit)
                .err()
                .unwrap_or_else(|| panic!("exit {code} was taken for the helper's answer"));
            let shown = crate::ui::flatten(&error);
            assert!(shown.contains("connect to host"), "{code}: {shown}");
        }
        let mut stalled = Command::new("sh");
        stalled.args(["-c", "sleep 10"]);
        let error = verdicts_run(stalled, &rows, std::time::Duration::from_millis(200))
            .expect_err("a timeout is not the helper's answer");
        assert!(crate::ui::flatten(&error).contains("did not answer in time"));
        let mut partial = Command::new("sh");
        partial.args([
            "-c",
            r#"cat >/dev/null; echo '{"remote": {}, "structural": {}, "held": {}}'"#,
        ]);
        let error = verdicts_run(partial, &rows, limit)
            .expect_err("an answer without the row is not a verdict");
        assert!(crate::ui::flatten(&error).contains("incomplete"));
    }

    /// An interpreter compiles the whole script before running its first
    /// line, so the version refusal is reached only if every line parses
    /// on the interpreters it refuses. `SLACK_NS = 2_000_000_000` was a
    /// SyntaxError before Python 3.6, so 3.5 never printed the reason.
    /// The script travels as an argv word, and a non-ASCII byte there can
    /// fail to decode on Python 3.6 or older under the C locale.
    #[test]
    fn baseline_py_parses_on_the_old_pythons_it_refuses() {
        let source = include_str!("baseline.py");
        assert!(
            source.is_ascii(),
            "baseline.py must stay ASCII: {:?}",
            source.lines().find(|line| !line.is_ascii())
        );
        let out = newer_syntax_in(source);
        assert!(
            out.status.success(),
            "baseline.py uses syntax the pythons it refuses cannot parse:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The check above must see each construct on whichever python3 runs
    /// it, CI's included. Without the token pass, 3.12 and later let an
    /// f-string through and 3.9 lets `:=` and `/` through.
    #[test]
    fn the_old_syntax_check_names_each_construct_on_this_python3() {
        for (snippet, named) in [
            ("x = f'{1}'\n", "an f-string"),
            ("x = Rf'{1}'\n", "an f-string"),
            ("x = 2_000\n", "an underscore in a number"),
            ("if (x := 1):\n    pass\n", "an assignment expression"),
            ("def g(a, /):\n    pass\n", "a positional-only parameter"),
        ] {
            let out = newer_syntax_in(snippet);
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(!out.status.success(), "{snippet:?} passed: {said}");
            // 3.7 cannot parse `:=` or `/` at all, and says so instead.
            assert!(
                said.contains(&format!("line 1: {named}"))
                    || said.contains("line 1: syntax this python3 cannot parse"),
                "{snippet:?} was not named as {named:?}: {said}{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let plain = newer_syntax_in("x = '%d' % 2000\n\ndef g(a, *rest):\n    return rest\n");
        assert!(
            plain.status.success(),
            "3.4 syntax was refused: {}{}",
            String::from_utf8_lossy(&plain.stdout),
            String::from_utf8_lossy(&plain.stderr)
        );
    }

    pub(crate) fn newer_syntax_in(source: &str) -> crate::proc::Bounded {
        let mut command = crate::stream::python();
        command.args(["-c", NEWER_SYNTAX]);
        crate::proc::run_bounded(
            &mut command,
            Some(source.as_bytes().to_vec()),
            std::time::Duration::from_secs(5),
        )
        .unwrap()
    }

    /// Names, with its line, syntax the pythons baseline.py refuses cannot
    /// parse, and exits 1 if there is any; runs on any python3 from 3.7.
    /// `ast.parse(source, feature_version=(3, 4))` alone depends on the
    /// interpreter that runs it. Measured with `f"{1}"`, `(x := 1)` and
    /// `def g(a, /)`: 3.9.6 rejects the f-string and accepts the other two;
    /// 3.13.9 and 3.14.6 reject the other two and accept the f-string,
    /// which entered the grammar in 3.12 (PEP 701), the python3 CI runs.
    /// 3.7 has no `feature_version` at all. So f-strings, `:=` and
    /// `1_000` are looked for by token, positional-only parameters in the
    /// tree, and the parse still runs where it exists. Unpacking in
    /// displays and calls (`{**a}`, 3.5) passes every version's parse and
    /// is not looked for: it breaks only 3.4.
    const NEWER_SYNTAX: &str = r#"
import ast, io, re, sys, tokenize
source = sys.stdin.read()
found = []
for token in tokenize.generate_tokens(io.StringIO(source).readline):
    kind = tokenize.tok_name[token.type]
    prefix = re.match('[A-Za-z]*', token.string).group().lower()
    if kind == 'FSTRING_START' or (kind == 'STRING' and 'f' in prefix):
        found.append((token.start[0], 'an f-string (3.6)'))
    elif kind == 'NUMBER' and '_' in token.string:
        found.append((token.start[0], 'an underscore in a number (3.6)'))
    elif kind == 'OP' and token.string == ':=':
        found.append((token.start[0], 'an assignment expression, := (3.8)'))
try:
    tree = ast.parse(source)
except SyntaxError as error:
    found.append((error.lineno or 0, 'syntax this python3 cannot parse: %s' % error.msg))
else:
    for node in ast.walk(tree):
        if isinstance(node, ast.arguments) and getattr(node, 'posonlyargs', None):
            found.append((node.posonlyargs[0].lineno, 'a positional-only parameter, / (3.8)'))
if sys.version_info >= (3, 8):
    try:
        ast.parse(source, feature_version=(3, 4))
    except SyntaxError as error:
        found.append((error.lineno or 0, 'syntax Python 3.4 cannot parse: %s' % error.msg))
for line, what in sorted(found):
    print('line %d: %s' % (line, what))
sys.exit(1 if found else 0)
"#;
}
