//! A persistent SSH channel for small regular files: replaces, creates,
//! unlinks and reads that sync.rs selected.
//!
//! sync.rs alone selects paths, decides deletion and holds the workspace
//! lock. This transport owns no ignore rule and chooses no path: every
//! item is a leaf sync.rs named, guarded by the server copy sync.rs expects
//! there, and the receipt says what changed, what the server lost and what
//! was refused. A refused batch applied nothing. A read brings a small
//! file's bytes home in the same exchange; `land` writes them only over a
//! local copy that still carries the stamp sync.rs selected it under, or
//! into a name that is still absent — rsync's `--existing` and
//! `--ignore-existing`, decided at the instant of writing — through real
//! directories held open from the anchor down (dirfd.rs), creating the
//! ones the server made and never crossing a symlink, even one swapped in
//! while the file lands. Measured: an rsync spawn is ~6 round trips and a
//! fresh ssh session ~2, so a new file cost three of each; one exchange
//! here is one round trip. Large or unstable files, new directories and
//! every conflict still use rsync and the full survey. The helper runs on
//! the server's python3, which syncing needs anyway: the survey's
//! baseline.py requires Python 3.7 or later. Nothing is installed and no
//! daemon runs; the helper is sent on every connect and EOF ends it.
//! Private coordination files live beside the project, under the
//! workspace's private .stream/. They must not share rsync's partial
//! directory: a project file named stream.lock could otherwise collide
//! with the coordination lock.
//!
//! Each request is a bounded JSON header followed by exact-length raw bytes.
//! One worker owns both pipes; callers wait on a channel with a shared Budget,
//! never on pipe I/O. Timeout kills the SSH child, unblocking the worker.
//! Receipts name the batch and the applied paths, and only their matching
//! pre-read local observations may be acknowledged. A concurrent save remains
//! different and is sent again. Unknown/partial receipts require rsync repair.
//! Killing local SSH alone cannot revoke bytes already buffered remotely.
//! fence() retires helper leases under the same remote lock as atomic replace;
//! the round trip that prepares the workspace before every full push runs
//! the same script, and failed-batch repair calls it directly, so rsync
//! never writes behind a live lease.
//! Production batches carry the server fingerprint sync.rs expects: the
//! last common content on the warm path, the server copy as surveyed on a
//! full reconcile, whose selection already applied the common record and
//! `no_history` (`push_and_claim` says why the record there would refuse
//! every no-history send). A differing destination is refused, even if its
//! timestamp is older. A refused/uncertain batch must return to
//! reconciliation, never blind repair.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::proc::Budget;
use crate::selection::Stamp;
use crate::ssh::{Ssh, sh_quote};
use crate::ui::{self, fail};

const HELPER: &str = include_str!("stream.py");
const MAX_FILE: u64 = 1024 * 1024;
const MAX_BATCH: usize = 8 * MAX_FILE as usize;
const MAX_HEADER: usize = 65536;

const FENCE: &str = r#"import os,sys,fcntl
control=os.open(sys.argv[1],os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW)
try:
 lock=os.open('stream.lock',os.O_RDWR|os.O_NOFOLLOW,dir_fd=control)
 fcntl.flock(lock,fcntl.LOCK_EX)
 for name in os.listdir(control):
  if name.startswith('stream-') and name.endswith('.lease'):
   try: os.unlink(name,dir_fd=control)
   except FileNotFoundError: pass
finally:
 os.close(control)
"#;

fn control(workspace: &str) -> String {
    format!("{workspace}/.stream")
}

/// The one line that retires every helper lease, for the round trip that
/// prepares a workspace (`sync::ensure_workspace`) as much as for `fence`.
pub fn fence_script(workspace: &str) -> String {
    let root = control(workspace);
    format!(
        "if [ -e {marker} ]; then python3 -c {code} {root}; fi",
        marker = sh_quote(&format!("{root}/stream.lock")),
        code = sh_quote(FENCE),
        root = sh_quote(&root),
    )
}

pub fn fence(ssh: &Ssh, workspace: &str, budget: &Budget) -> Result<()> {
    let script = fence_script(workspace);
    let mut command = ssh.command();
    command.arg("--").arg(&ssh.dest).arg("sh -s");
    let out =
        crate::proc::run_bounded(&mut command, Some(script.into_bytes()), budget.remaining())?;
    crate::audit::record_command("ssh-stream-fence", &command, out.status.code());
    if out.timed_out || !out.status.success() {
        return Err(transport_error(
            "could not retire the server's previous transfer leases",
        ));
    }
    Ok(())
}

#[derive(Serialize)]
struct Entry {
    op: &'static str,
    path: String,
    size: usize,
    mode: u32,
    modified: (i64, i64),
    guarded: bool,
    expected: Option<String>,
    content: String,
}

pub struct Batch {
    entries: Vec<Entry>,
    bytes: Vec<u8>,
}

impl Batch {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            bytes: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The bytes sync.rs selected, read through one fd and compared to the
    /// stamp it selected them under (see `prepare`). `create` names a leaf
    /// the ledger does not claim yet; the helper refuses an existing one.
    pub fn add(
        &mut self,
        anchor: &Path,
        path: &Path,
        expected: &Stamp,
        create: bool,
    ) -> Option<()> {
        if self.entries.len() >= 64 {
            return None;
        }
        let name = crate::invocation::anchor_rel(anchor, path)?;
        let file = File::open(path).ok()?;
        let before = file.metadata().ok()?;
        if !before.is_file()
            || before.len() > MAX_FILE
            || Stamp::from_metadata(&before) != *expected
            || self.bytes.len() + before.len() as usize > MAX_BATCH
        {
            return None;
        }
        let start = self.bytes.len();
        (&file)
            .take(MAX_FILE + 1)
            .read_to_end(&mut self.bytes)
            .ok()?;
        if self.bytes.len() - start != before.len() as usize
            || Stamp::from_metadata(&file.metadata().ok()?) != *expected
            || Stamp::read(path).as_ref() != Some(expected)
        {
            return None;
        }
        self.entries.push(Entry {
            op: if create { "create" } else { "replace" },
            path: name,
            size: before.len() as usize,
            mode: before.mode() & 0o7777,
            modified: (before.mtime(), before.mtime_nsec()),
            guarded: false,
            expected: None,
            content: crate::baseline::regular(&self.bytes[start..], before.mode()),
        });
        Some(())
    }

    /// A small file the server holds that should come home: its bytes,
    /// mode and mtime arrive with the receipt.
    pub fn read(&mut self, name: String) -> Option<()> {
        if self.entries.len() >= 64 {
            return None;
        }
        self.entries.push(Entry {
            op: "read",
            path: name,
            size: 0,
            mode: 0,
            modified: (0, 0),
            guarded: false,
            expected: None,
            content: String::new(),
        });
        Some(())
    }

    /// A ledger row the server should lose, but only while its copy is
    /// still `expected` — the content both ends last agreed on. sync.rs
    /// passes None for a row without one, and the helper refuses it.
    pub fn unlink(&mut self, name: String, expected: Option<String>) -> Option<()> {
        if self.entries.len() >= 64 {
            return None;
        }
        self.entries.push(Entry {
            op: "unlink",
            path: name,
            size: 0,
            mode: 0,
            modified: (0, 0),
            guarded: true,
            expected,
            content: String::new(),
        });
        Some(())
    }

    /// Guard every replace with the server content the caller expects
    /// (the module header says which, per path). An unlink carries its own
    /// expectation from the caller and a create has none: the helper
    /// refuses an existing name outright.
    pub fn guard(&mut self, common: &crate::baseline::Contents) {
        for entry in self.entries.iter_mut().filter(|e| e.op == "replace") {
            entry.guarded = true;
            entry.expected = common.get(&entry.path).cloned().flatten();
        }
    }

    /// What the common-content record may learn from a full receipt: the
    /// bytes of every replace and create, and no content for an unlink.
    pub fn contents(&self) -> crate::baseline::Contents {
        self.entries
            .iter()
            .filter(|e| e.op != "read")
            .map(|e| {
                (
                    e.path.clone(),
                    (e.op != "unlink").then(|| e.content.clone()),
                )
            })
            .collect()
    }

    /// The names of the leaves this batch would put on the server.
    pub fn written(&self) -> Vec<Vec<u8>> {
        self.entries
            .iter()
            .filter(|e| e.op != "unlink" && e.op != "read")
            .map(|e| e.path.clone().into_bytes())
            .collect()
    }
}

/// A small file read from the server, exactly as the helper saw it.
pub struct Pulled {
    pub path: String,
    pub mode: u32,
    pub modified: (i64, i64),
    pub content: String,
    pub bytes: Vec<u8>,
    /// The modes of the server's directories above it, from the anchor
    /// down: `land` gives a directory it creates its server's mode.
    pub parents: Vec<u32>,
}

/// The directories one `land` created, set to the server's modes when it
/// returns, however it returns. Not as each is made: a mode without the
/// owner's write would block the staging directory and the file below it.
/// With this machine's umask instead, a directory a container made 0777
/// differed from the server's, and the next push tried to change root's
/// directory as a non-root SSH user: every sync failed. A mode that can't
/// be set keeps the umask's.
struct ServerModes(Vec<(crate::dirfd::Dir, u32)>);

impl Drop for ServerModes {
    fn drop(&mut self) {
        for (dir, mode) in &self.0 {
            let _ = dir.set_mode(*mode);
        }
    }
}

/// Write a pulled file into the workspace under the guard the caller
/// names: `expected` is the stamp the local copy must still carry (the
/// one sync.rs selected it under), or None for a name that must still be
/// absent. Either failing leaves the tree untouched and returns None — a
/// save landed meanwhile, or the file appeared — and the next notice
/// decides again. Bytes go through rsync's staging directory and land by
/// rename or, for a new name, by a link that refuses an existing one.
pub fn land(anchor: &Path, pulled: &Pulled, expected: Option<&Stamp>) -> Result<Option<Stamp>> {
    use crate::dirfd::{Dir, Entry};
    if crate::baseline::regular(&pulled.bytes, pulled.mode) != pulled.content {
        return Err(transport_error("pulled bytes differ from their receipt"));
    }
    let rel = Path::new(&pulled.path);
    if rel
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(transport_error("pulled path escapes the workspace"));
    }
    let (Some(parent_rel), Some(name)) = (rel.parent(), rel.file_name()) else {
        return Err(transport_error("pulled path escapes the workspace"));
    };
    let refuse = |why: &str| {
        ui::warn(&format!("sync: {} not brought home — {why}", pulled.path));
        Ok(None)
    };
    // Held open from the anchor down (see dirfd.rs): the directory checked
    // is the directory written, whatever is renamed meanwhile.
    let mut parent = match Dir::open(anchor) {
        Ok(dir) => dir,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
            return refuse("the project directory was replaced by a symlink");
        }
        Err(e) => return Err(e.into()),
    };
    let mut made = ServerModes(Vec::new());
    for (depth, component) in parent_rel.components().enumerate() {
        match parent.child(component.as_os_str())? {
            Some((dir, created)) => {
                if created && let Some(&mode) = pulled.parents.get(depth) {
                    made.0.push((dir.try_clone()?, mode));
                }
                parent = dir;
            }
            None => return refuse("a parent here is a symlink or not a directory"),
        }
    }
    let stamp = |dir: &Dir| -> Result<Option<Option<Stamp>>> {
        Ok(match dir.entry(name)? {
            Entry::Absent => Some(None),
            Entry::Present(Some(m)) => Some(Some(Stamp::from_metadata(&m))),
            Entry::Present(None) => None,
        })
    };
    let guard = |current: Option<Option<Stamp>>| match (expected, current) {
        (Some(e), Some(Some(c))) => *e == c,
        (None, Some(None)) => true,
        _ => false,
    };
    if !guard(stamp(&parent)?) {
        return Ok(None);
    }
    let staging_name = std::ffi::OsStr::new(".ulak-partial");
    let Some((staging, created_staging)) = parent.child(staging_name)? else {
        return refuse("the staging path beside it is not a directory");
    };
    let temporary = format!("pull-{}", crate::hashid::uuid_v4()?);
    let temporary = std::ffi::OsStr::new(&temporary);
    let landed = (|| -> Result<bool> {
        let mut file = staging.create(temporary, 0o600)?;
        std::io::Write::write_all(&mut file, &pulled.bytes)?;
        file.set_permissions(std::fs::Permissions::from_mode(pulled.mode))?;
        let (secs, nanos) = pulled.modified;
        let mtime = std::time::UNIX_EPOCH
            + std::time::Duration::new(secs.max(0) as u64, nanos.clamp(0, 999_999_999) as u32);
        file.set_modified(mtime)?;
        drop(file);
        if !guard(stamp(&parent)?) {
            return Ok(false);
        }
        if expected.is_some() {
            staging.rename(temporary, &parent, name)?;
        } else {
            staging.link(temporary, &parent, name)?;
        }
        Ok(true)
    })();
    let _ = staging.unlink(temporary);
    drop(staging);
    if created_staging {
        let _ = parent.rmdir(staging_name);
    }
    if !landed? {
        return Ok(None);
    }
    Ok(stamp(&parent)?.flatten())
}

/// What one exchange did on the server, by name. `refused` non-empty means
/// nothing at all was applied.
#[derive(Default)]
pub struct Applied {
    pub changed: Vec<String>,
    pub deleted: Vec<String>,
    pub refused: Vec<(String, String)>,
    pub pulled: Vec<Pulled>,
}

/// Read through one fd and compare its identity/times before and after.
/// Opening a replaced symlink cannot send its target: the fd must have the
/// regular inode that sync.rs selected before any bytes are read.
pub fn prepare(anchor: &Path, changed: &BTreeMap<PathBuf, Stamp>) -> Option<Batch> {
    if changed.is_empty() || changed.len() > 64 {
        return None;
    }
    let mut batch = Batch::new();
    for (path, expected) in changed {
        batch.add(anchor, path, expected, false)?;
    }
    Some(batch)
}

#[derive(Deserialize)]
#[serde(default)]
struct Receipt {
    version: u32,
    id: u64,
    changed: Vec<String>,
    deleted: Vec<String>,
    refused: Vec<Refusal>,
    reads: Vec<ReadHead>,
    error: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Refusal {
    path: String,
    reason: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ReadHead {
    path: String,
    size: usize,
    mode: u32,
    modified: (i64, i64),
    content: String,
    parents: Vec<u32>,
}

impl Default for Receipt {
    fn default() -> Self {
        Self {
            version: 0,
            id: 0,
            changed: Vec::new(),
            deleted: Vec::new(),
            refused: Vec::new(),
            reads: Vec::new(),
            error: Some("incomplete receipt".into()),
        }
    }
}

struct Exchange {
    payload: Vec<u8>,
    reply: Sender<std::io::Result<(Receipt, Vec<u8>)>>,
}

pub struct Client {
    child: Child,
    command: Command,
    requests: Sender<Exchange>,
    next: u64,
}

/// The receipt line, then exactly the bytes its reads announce.
fn receipt(reader: &mut impl BufRead) -> std::io::Result<(Receipt, Vec<u8>)> {
    let mut line = Vec::new();
    reader
        .take((MAX_HEADER + 1) as u64)
        .read_until(b'\n', &mut line)?;
    if line.len() > MAX_HEADER || !line.ends_with(b"\n") {
        return Err(std::io::Error::other(
            "incomplete or oversized sync receipt",
        ));
    }
    let receipt: Receipt = serde_json::from_slice(&line).map_err(std::io::Error::other)?;
    let total: usize = receipt.reads.iter().map(|r| r.size).sum();
    if total > MAX_BATCH || receipt.reads.iter().any(|r| r.size > MAX_FILE as usize) {
        return Err(std::io::Error::other("read payload exceeds protocol limit"));
    }
    let mut bytes = vec![0; total];
    reader.read_exact(&mut bytes)?;
    Ok((receipt, bytes))
}

impl Client {
    pub fn connect(ssh: &Ssh, root: &str, workspace: &str, budget: &Budget) -> Result<Self> {
        let bootstrap = format!("import sys; exec(sys.stdin.buffer.read({}))", HELPER.len());
        let mut command = ssh.command();
        command.arg("--").arg(&ssh.dest).arg(format!(
            "python3 -u -c {} {} {}",
            sh_quote(&bootstrap),
            sh_quote(root),
            sh_quote(&control(workspace))
        ));
        Self::spawn(command, HELPER.as_bytes().to_vec(), budget)
    }

    fn spawn(mut command: Command, startup: Vec<u8>, budget: &Budget) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|e| transport_error(&e.to_string()))?;
        crate::audit::record_command("ssh-stream", &command, None);
        let Some(mut input) = child.stdin.take() else {
            let _ = child.kill();
            let _ = crate::proc::wait_for(&mut child, crate::proc::PROBE);
            return Err(transport_error("missing stdin pipe"));
        };
        let Some(output) = child.stdout.take() else {
            let _ = child.kill();
            let _ = crate::proc::wait_for(&mut child, crate::proc::PROBE);
            return Err(transport_error("missing stdout pipe"));
        };
        let (requests, incoming): (Sender<Exchange>, Receiver<Exchange>) = mpsc::channel();
        thread::spawn(move || {
            let mut output = BufReader::new(output);
            for exchange in incoming {
                let answer = input
                    .write_all(&exchange.payload)
                    .and_then(|_| input.flush())
                    .and_then(|_| receipt(&mut output));
                let failed = answer.is_err();
                let _ = exchange.reply.send(answer);
                if failed {
                    break;
                }
            }
        });
        let mut client = Self {
            child,
            command,
            requests,
            next: 1,
        };
        let (hello, _) = client.exchange(startup, budget)?;
        if hello.version != 1 || hello.id != 0 || hello.error.is_some() {
            return Err(transport_error("helper did not accept protocol version 1"));
        }
        Ok(client)
    }

    fn exchange(&mut self, payload: Vec<u8>, budget: &Budget) -> Result<(Receipt, Vec<u8>)> {
        let (reply, answer) = mpsc::channel();
        self.requests
            .send(Exchange { payload, reply })
            .map_err(|e| transport_error(&e.to_string()))?;
        answer
            .recv_timeout(budget.remaining())
            .map_err(|e| transport_error(&e.to_string()))?
            .map_err(|e| transport_error(&e.to_string()))
    }

    pub fn send(&mut self, batch: Batch, budget: &Budget) -> Result<Applied> {
        let id = self.next;
        self.next += 1;
        let mut payload =
            serde_json::to_vec(&serde_json::json!({"id": id, "files": batch.entries}))?;
        if payload.len() >= MAX_HEADER {
            return Err(transport_error("selected paths exceed the header limit"));
        }
        payload.push(b'\n');
        payload.extend(batch.bytes);
        let (receipt, mut bytes) = self.exchange(payload, budget)?;
        if let Some(error) = &receipt.error {
            return Err(transport_error(error));
        }
        let named = |p: &String| batch.entries.iter().any(|e| &e.path == p);
        let asked = |p: &String| batch.entries.iter().any(|e| &e.path == p && e.op == "read");
        if receipt.version != 1
            || receipt.id != id
            || !receipt.changed.iter().all(named)
            || !receipt.deleted.iter().all(named)
            || !receipt.refused.iter().all(|r| named(&r.path))
            || !receipt.reads.iter().all(|r| asked(&r.path))
            || (!receipt.refused.is_empty()
                && !(receipt.changed.is_empty()
                    && receipt.deleted.is_empty()
                    && receipt.reads.is_empty()))
        {
            return Err(transport_error(
                "helper returned an unmatched or failed receipt",
            ));
        }
        let mut pulled = Vec::new();
        for head in receipt.reads {
            let rest = bytes.split_off(head.size.min(bytes.len()));
            pulled.push(Pulled {
                path: head.path,
                mode: head.mode,
                modified: head.modified,
                content: head.content,
                bytes: std::mem::replace(&mut bytes, rest),
                parents: head.parents,
            });
        }
        Ok(Applied {
            changed: receipt.changed,
            deleted: receipt.deleted,
            refused: receipt
                .refused
                .into_iter()
                .map(|r| (r.path, r.reason))
                .collect(),
            pulled,
        })
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let status = crate::proc::wait_for(&mut self.child, crate::proc::PROBE)
            .ok()
            .flatten()
            .and_then(|s| s.code());
        crate::audit::record_command("ssh-stream", &self.command, status);
    }
}

fn transport_error(detail: &str) -> anyhow::Error {
    fail!("persistent file transfer stopped: {detail}")
        .now("run ulak sync to reconcile the workspace with rsync")
        .into_err()
}

/// The real helper serving a local directory as if it were the server's
/// project, for tests in this module and in sync.rs that need the
/// protocol's actual answers rather than a stand-in.
#[cfg(test)]
pub(crate) fn local_helper(root: &Path) -> Client {
    let mut command = python();
    command
        .args(["-u", "-c", HELPER])
        .arg(root)
        .arg(root.join(".control"));
    Client::spawn(
        command,
        Vec::new(),
        &Budget::new(std::time::Duration::from_secs(3)),
    )
    .unwrap()
}

/// The real helper, except that it exits after applying a batch and
/// before answering: the receipt a lost connection would lose.
#[cfg(test)]
pub(crate) fn local_helper_losing_receipts(root: &Path) -> Client {
    let reply = "    reply({\"version\": 1, \"id\": request[\"id\"]";
    assert_eq!(
        HELPER.matches(reply).count(),
        1,
        "the helper's receipt line moved"
    );
    let script = HELPER.replace(reply, &format!("    os._exit(0)\n{reply}"));
    let mut command = python();
    command
        .args(["-u", "-c", &script])
        .arg(root)
        .arg(root.join(".control"));
    Client::spawn(
        command,
        Vec::new(),
        &Budget::new(std::time::Duration::from_secs(3)),
    )
    .unwrap()
}

/// The interpreter every helper test spawns (stream.py here, baseline.py
/// in baseline.rs and hashcache.rs), asserted present by name so a machine
/// without it fails red instead of skipping.
#[cfg(test)]
pub(crate) fn python() -> Command {
    static AVAILABLE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    AVAILABLE.get_or_init(|| {
        assert!(Command::new("python3").arg("--version").output()
            .is_ok_and(|out| out.status.success()),
            "server helper tests need python3: brew install python (macOS) or sudo apt-get install python3 (Debian/Ubuntu)");
    });
    Command::new("python3")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant};

    fn helper(root: &Path) -> Client {
        local_helper(root)
    }

    fn batch(root: &Path, names: &[&str]) -> Batch {
        let selected = names
            .iter()
            .map(|name| {
                let p = root.join(name);
                let stamp = Stamp::read(&p).unwrap();
                (p, stamp)
            })
            .collect();
        prepare(root, &selected).unwrap()
    }

    /// A delayed local save must not erase server output written since the
    /// common ancestor, even when the old and new payload sizes are equal.
    #[test]
    fn guarded_batches_preserve_remote_edits_and_accept_an_unchanged_ancestor() {
        for remote_changed in [false, true] {
            let local = tempfile::tempdir().unwrap();
            let remote = tempfile::tempdir().unwrap();
            let destination = remote.path().join("file");
            std::fs::write(&destination, b"base").unwrap();
            let ancestor = crate::baseline::fingerprint(&destination).unwrap();
            std::fs::write(local.path().join("file"), b"edit").unwrap();
            if remote_changed {
                std::fs::write(&destination, b"work").unwrap();
            }
            let mut pending = batch(local.path(), &["file"]);
            pending.guard(&BTreeMap::from([("file".into(), ancestor)]));
            let mut client = helper(remote.path());
            let result = client
                .send(pending, &Budget::new(Duration::from_secs(3)))
                .unwrap();
            assert_eq!(result.refused.is_empty(), !remote_changed);
            assert!(result.deleted.is_empty());
            assert_eq!(
                std::fs::read(&destination).unwrap(),
                if remote_changed { b"work" } else { b"edit" }
            );
        }
    }

    /// The receipt must follow complete bytes, mode and nanosecond times;
    /// repeating identical content must not replace the inode or emit an echo.
    #[test]
    fn repeated_writes_preserve_exact_bytes_and_do_not_replace_unchanged_files() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let names = ["empty", "utf8-ş ' [x]", "binary"];
        let payloads = [b"".as_slice(), b"new\ntext", &[0, 255, 0, 128]];
        for (name, data) in names.iter().zip(payloads) {
            std::fs::write(local.path().join(name), data).unwrap();
            std::fs::set_permissions(
                local.path().join(name),
                std::fs::Permissions::from_mode(0o640),
            )
            .unwrap();
            std::fs::write(remote.path().join(name), "old").unwrap();
        }
        let mut client = helper(remote.path());
        let got = client
            .send(
                batch(local.path(), &names),
                &Budget::new(Duration::from_secs(3)),
            )
            .unwrap();
        assert_eq!(got.changed.len(), names.len());
        let mut before = Vec::new();
        for name in names {
            let a = local.path().join(name);
            let b = remote.path().join(name);
            assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
            let a = a.metadata().unwrap();
            let b = b.metadata().unwrap();
            assert_eq!(a.mode() & 0o7777, b.mode() & 0o7777);
            assert_eq!((a.mtime(), a.mtime_nsec()), (b.mtime(), b.mtime_nsec()));
            before.push(b.ino());
        }
        assert!(
            client
                .send(
                    batch(local.path(), &names),
                    &Budget::new(Duration::from_secs(3))
                )
                .unwrap()
                .changed
                .is_empty()
        );
        assert_eq!(
            before,
            names
                .iter()
                .map(|n| remote.path().join(n).metadata().unwrap().ino())
                .collect::<Vec<_>>()
        );
        assert!(!remote.path().join(".ulak-partial").exists());
        let scratch: Vec<_> = std::fs::read_dir(remote.path().join(".control"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            scratch.len(),
            2,
            "only the lock and this channel's lease remain"
        );
    }

    /// A replaced parent or leaf must not redirect bytes into server-owned
    /// storage. Missing destinations go back through the ordinary sync path.
    #[test]
    fn missing_files_and_symlinked_parents_or_leaves_are_never_written() {
        for scenario in ["missing", "parent", "leaf", "staging"] {
            let local = tempfile::tempdir().unwrap();
            let remote = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::create_dir(local.path().join("dir")).unwrap();
            std::fs::write(local.path().join("dir/file"), "local").unwrap();
            std::fs::write(outside.path().join("file"), "protected").unwrap();
            if scenario == "parent" {
                symlink(outside.path(), remote.path().join("dir")).unwrap();
            } else {
                std::fs::create_dir(remote.path().join("dir")).unwrap();
                if scenario == "leaf" {
                    symlink(outside.path().join("file"), remote.path().join("dir/file")).unwrap();
                } else if scenario == "staging" {
                    std::fs::write(remote.path().join("dir/file"), "old").unwrap();
                    symlink(outside.path(), remote.path().join("dir/.ulak-partial")).unwrap();
                }
            }
            let mut client = helper(remote.path());
            let outcome = client.send(
                batch(local.path(), &["dir/file"]),
                &Budget::new(Duration::from_secs(3)),
            );
            // A missing or non-regular destination is refused by name and
            // the channel lives on; a symlinked parent or staging directory
            // is a protocol failure that ends it.
            match scenario {
                "missing" | "leaf" => {
                    let applied = outcome.unwrap_or_else(|e| panic!("{scenario}: {e}"));
                    assert!(applied.changed.is_empty(), "{scenario}");
                    assert_eq!(applied.refused.len(), 1, "{scenario}");
                }
                _ => assert!(outcome.is_err(), "{scenario}"),
            }
            assert_eq!(
                std::fs::read(outside.path().join("file")).unwrap(),
                b"protected"
            );
            if scenario == "missing" {
                assert!(!remote.path().join("dir/file").exists());
            }
            if scenario == "staging" {
                assert_eq!(
                    std::fs::read(remote.path().join("dir/file")).unwrap(),
                    b"old"
                );
            }
        }
    }

    /// Cache observations are not permission to read a replaced inode or
    /// allocate a huge file. Both cases must use the checksum rsync path.
    #[test]
    fn changed_observations_and_large_files_are_not_streamed() {
        let local = tempfile::tempdir().unwrap();
        let p = local.path().join("file");
        std::fs::write(&p, "first").unwrap();
        let selected = BTreeMap::from([(p.clone(), Stamp::read(&p).unwrap())]);
        std::fs::write(&p, "second").unwrap();
        assert!(prepare(local.path(), &selected).is_none());
        File::create(&p).unwrap().set_len(MAX_FILE + 1).unwrap();
        let selected = BTreeMap::from([(p.clone(), Stamp::read(&p).unwrap())]);
        assert!(prepare(local.path(), &selected).is_none());
    }

    /// Losing an ACK does not prove the remote worker stopped. Revocation
    /// must reject its next buffered write before rsync repairs the files.
    #[test]
    fn a_revoked_channel_cannot_overwrite_a_newer_repair() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(local.path().join("file"), "first").unwrap();
        std::fs::write(remote.path().join("file"), "old").unwrap();
        let mut client = helper(remote.path());
        client
            .send(
                batch(local.path(), &["file"]),
                &Budget::new(Duration::from_secs(3)),
            )
            .unwrap();
        let mut revoke = python();
        revoke
            .args(["-c", FENCE])
            .arg(remote.path().join(".control"));
        let out = crate::proc::run_bounded(&mut revoke, None, Duration::from_secs(3)).unwrap();
        assert!(out.status.success());
        std::fs::write(remote.path().join("file"), "repaired").unwrap();
        std::fs::write(local.path().join("file"), "stale channel").unwrap();
        assert!(
            client
                .send(
                    batch(local.path(), &["file"]),
                    &Budget::new(Duration::from_secs(3))
                )
                .is_err()
        );
        assert_eq!(
            std::fs::read(remote.path().join("file")).unwrap(),
            b"repaired"
        );
    }

    /// A save after capture belongs to the next batch; an ACK must not make
    /// the later local bytes look like the snapshot that actually travelled.
    #[test]
    fn a_save_after_capture_remains_distinct_from_the_transferred_snapshot() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let path = local.path().join("file");
        std::fs::write(&path, "first").unwrap();
        std::fs::write(remote.path().join("file"), "old").unwrap();
        let observed = Stamp::read(&path).unwrap();
        let captured = batch(local.path(), &["file"]);
        std::fs::write(&path, "second").unwrap();
        let mut client = helper(remote.path());
        client
            .send(captured, &Budget::new(Duration::from_secs(3)))
            .unwrap();
        assert_eq!(std::fs::read(remote.path().join("file")).unwrap(), b"first");
        assert_ne!(Stamp::read(&path).unwrap(), observed);
        client
            .send(
                batch(local.path(), &["file"]),
                &Budget::new(Duration::from_secs(3)),
            )
            .unwrap();
        assert_eq!(
            std::fs::read(remote.path().join("file")).unwrap(),
            b"second"
        );
    }

    /// A create lands only under an existing parent and never over an
    /// existing name; an unlink removes only the content both ends last
    /// agreed on and reports an absent name as already lost. One refusal
    /// applies NOTHING in its batch — the sibling replace is left alone —
    /// because sync.rs claims and retires on the receipt as a whole.
    #[test]
    fn creates_and_unlinks_apply_only_when_every_item_still_matches() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        std::fs::create_dir(local.path().join("dir")).unwrap();
        std::fs::create_dir(remote.path().join("dir")).unwrap();
        std::fs::write(local.path().join("dir/new.rs"), "pub fn new() {}").unwrap();
        std::fs::set_permissions(
            local.path().join("dir/new.rs"),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        std::fs::write(local.path().join("dir/kept.rs"), "edited").unwrap();
        std::fs::write(remote.path().join("dir/kept.rs"), "base").unwrap();
        std::fs::write(remote.path().join("dir/old.rs"), "retired").unwrap();
        std::fs::write(remote.path().join("dir/busy.rs"), "server work").unwrap();
        let base = crate::baseline::fingerprint(&remote.path().join("dir/kept.rs")).unwrap();
        let retired = crate::baseline::fingerprint(&remote.path().join("dir/old.rs")).unwrap();
        let stamp = |name: &str| Stamp::read(&local.path().join(name)).unwrap();
        let mut client = helper(remote.path());
        let budget = || Budget::new(Duration::from_secs(3));

        // The accepting case: a create, a guarded replace, an unlink of the
        // common content and an unlink of a name already gone, in one batch.
        let mut batch = Batch::new();
        batch
            .add(
                local.path(),
                &local.path().join("dir/new.rs"),
                &stamp("dir/new.rs"),
                true,
            )
            .unwrap();
        batch
            .add(
                local.path(),
                &local.path().join("dir/kept.rs"),
                &stamp("dir/kept.rs"),
                false,
            )
            .unwrap();
        batch.unlink("dir/old.rs".into(), retired.clone()).unwrap();
        batch
            .unlink("dir/gone.rs".into(), Some("f:644:never".into()))
            .unwrap();
        batch.guard(&BTreeMap::from([("dir/kept.rs".into(), base.clone())]));
        assert_eq!(
            batch.written(),
            [b"dir/new.rs".to_vec(), b"dir/kept.rs".to_vec()]
        );
        let applied = client.send(batch, &budget()).unwrap();
        assert!(applied.refused.is_empty(), "{:?}", applied.refused);
        assert_eq!(applied.changed, ["dir/new.rs", "dir/kept.rs"]);
        assert_eq!(applied.deleted, ["dir/old.rs", "dir/gone.rs"]);
        let created = remote.path().join("dir/new.rs");
        assert_eq!(std::fs::read(&created).unwrap(), b"pub fn new() {}");
        assert_eq!(created.metadata().unwrap().mode() & 0o7777, 0o640);
        assert_eq!(
            (
                created.metadata().unwrap().mtime(),
                created.metadata().unwrap().mtime_nsec()
            ),
            (
                stamp("dir/new.rs").last_change().0,
                created.metadata().unwrap().mtime_nsec()
            )
        );
        assert_eq!(
            std::fs::read(remote.path().join("dir/kept.rs")).unwrap(),
            b"edited"
        );
        assert!(!remote.path().join("dir/old.rs").exists());
        assert!(!remote.path().join("dir/.ulak-partial").exists());

        // A create over a name already holding the same bytes is the
        // claim of server output that came home: applied as a replace,
        // never refused. Different bytes under that name are a conflict.
        std::fs::write(local.path().join("dir/home.rs"), "generated").unwrap();
        std::fs::write(remote.path().join("dir/home.rs"), "generated").unwrap();
        let mut batch = Batch::new();
        batch
            .add(
                local.path(),
                &local.path().join("dir/home.rs"),
                &stamp("dir/home.rs"),
                true,
            )
            .unwrap();
        let applied = client.send(batch, &budget()).unwrap();
        assert!(applied.refused.is_empty(), "{:?}", applied.refused);
        assert_eq!(
            std::fs::read(remote.path().join("dir/home.rs")).unwrap(),
            b"generated"
        );
        let (a, b) = (
            local.path().join("dir/home.rs").metadata().unwrap(),
            remote.path().join("dir/home.rs").metadata().unwrap(),
        );
        assert_eq!((a.mtime(), a.mtime_nsec()), (b.mtime(), b.mtime_nsec()));

        // Refusals, each with the sibling replace that must NOT land:
        // creating over server work, unlinking server work, unlinking with
        // no known ancestor, and creating under a parent the server lost.
        std::fs::write(local.path().join("dir/busy.rs"), "mine").unwrap();
        std::fs::write(local.path().join("dir/kept.rs"), "edited again").unwrap();
        let edited = crate::baseline::fingerprint(&remote.path().join("dir/kept.rs")).unwrap();
        std::fs::create_dir(local.path().join("lost")).unwrap();
        std::fs::write(local.path().join("lost/f.rs"), "x").unwrap();
        type Build<'a> = Box<dyn Fn(&mut Batch) + 'a>;
        let cases: Vec<(&str, Build)> = vec![
            (
                "exists",
                Box::new(|b: &mut Batch| {
                    b.add(
                        local.path(),
                        &local.path().join("dir/busy.rs"),
                        &stamp("dir/busy.rs"),
                        true,
                    )
                    .unwrap();
                }),
            ),
            (
                "differs",
                Box::new(|b: &mut Batch| {
                    b.unlink("dir/busy.rs".into(), base.clone()).unwrap();
                }),
            ),
            (
                "unknown ancestor",
                Box::new(|b: &mut Batch| {
                    b.unlink("dir/busy.rs".into(), None).unwrap();
                }),
            ),
            (
                "missing parent",
                Box::new(|b: &mut Batch| {
                    b.add(
                        local.path(),
                        &local.path().join("lost/f.rs"),
                        &stamp("lost/f.rs"),
                        true,
                    )
                    .unwrap();
                }),
            ),
        ];
        for (reason, build) in cases {
            let mut batch = Batch::new();
            build(&mut batch);
            batch
                .add(
                    local.path(),
                    &local.path().join("dir/kept.rs"),
                    &stamp("dir/kept.rs"),
                    false,
                )
                .unwrap();
            batch.guard(&BTreeMap::from([("dir/kept.rs".into(), edited.clone())]));
            let applied = client.send(batch, &budget()).unwrap();
            assert!(
                applied.changed.is_empty() && applied.deleted.is_empty(),
                "{reason}"
            );
            assert_eq!(applied.refused.len(), 1, "{reason}");
            assert_eq!(applied.refused[0].1, reason);
            assert_eq!(
                std::fs::read(remote.path().join("dir/kept.rs")).unwrap(),
                b"edited",
                "{reason}: a refused batch must apply nothing"
            );
            assert_eq!(
                std::fs::read(remote.path().join("dir/busy.rs")).unwrap(),
                b"server work"
            );
        }
        assert!(!remote.path().join("lost").exists());
    }

    /// A read brings back exactly the bytes, mode and mtime the server holds,
    /// and `land` writes them only over the stamp the caller named or into
    /// a name still absent: a save that landed meanwhile, or a file that
    /// appeared, is never overwritten. A missing file refuses the batch.
    #[test]
    fn reads_come_home_only_over_the_stamp_they_were_selected_under() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        std::fs::create_dir(local.path().join("out")).unwrap();
        std::fs::create_dir(remote.path().join("out")).unwrap();
        std::fs::write(remote.path().join("out/gen.rs"), "generated").unwrap();
        std::fs::set_permissions(
            remote.path().join("out/gen.rs"),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        std::fs::write(remote.path().join("out/log.txt"), "server log").unwrap();
        std::fs::write(local.path().join("out/log.txt"), "old log").unwrap();
        let mut client = helper(remote.path());
        let budget = || Budget::new(Duration::from_secs(3));

        let mut batch = Batch::new();
        batch.read("out/gen.rs".into()).unwrap();
        batch.read("out/log.txt".into()).unwrap();
        let applied = client.send(batch, &budget()).unwrap();
        assert!(applied.refused.is_empty());
        assert_eq!(applied.pulled.len(), 2);
        let generated = applied
            .pulled
            .iter()
            .find(|p| p.path == "out/gen.rs")
            .unwrap();
        assert_eq!(generated.bytes, b"generated");
        assert_eq!(generated.mode, 0o640);
        assert_eq!(
            generated.content,
            crate::baseline::regular(b"generated", 0o640)
        );

        // A new name lands by link; landing it twice refuses the second.
        let stamp = land(local.path(), generated, None)
            .unwrap()
            .expect("absent name lands");
        assert_eq!(
            std::fs::read(local.path().join("out/gen.rs")).unwrap(),
            b"generated"
        );
        let m = local.path().join("out/gen.rs").metadata().unwrap();
        assert_eq!(m.mode() & 0o7777, 0o640);
        assert_eq!((m.mtime(), m.mtime_nsec()), generated.modified);
        assert_eq!(Stamp::read(&local.path().join("out/gen.rs")), Some(stamp));
        assert!(land(local.path(), generated, None).unwrap().is_none());

        // An existing file lands only over the stamp it was selected under.
        let log = applied
            .pulled
            .iter()
            .find(|p| p.path == "out/log.txt")
            .unwrap();
        let selected = Stamp::read(&local.path().join("out/log.txt")).unwrap();
        std::fs::write(local.path().join("out/log.txt"), "edited meanwhile").unwrap();
        assert!(land(local.path(), log, Some(&selected)).unwrap().is_none());
        assert_eq!(
            std::fs::read(local.path().join("out/log.txt")).unwrap(),
            b"edited meanwhile"
        );
        let current = Stamp::read(&local.path().join("out/log.txt")).unwrap();
        assert!(land(local.path(), log, Some(&current)).unwrap().is_some());
        assert_eq!(
            std::fs::read(local.path().join("out/log.txt")).unwrap(),
            b"server log"
        );
        assert!(!local.path().join("out/.ulak-partial").exists());

        let mut batch = Batch::new();
        batch.read("out/missing.rs".into()).unwrap();
        let applied = client.send(batch, &budget()).unwrap();
        assert_eq!(applied.refused.len(), 1);
        assert!(applied.pulled.is_empty());
    }

    /// Reads whose total passes the batch limit are refused as rsync's,
    /// not sent: the client rejects a receipt over the limit outright,
    /// which closed the channel and forced a full reconcile on every
    /// service tick for nine near-1 MiB server files. The two ends read
    /// one limit, pinned here.
    #[test]
    fn reads_past_the_batch_limit_are_refused_as_rsyncs_not_sent() {
        assert!(HELPER.contains("MAX_FILE = 1024 * 1024"));
        assert_eq!(MAX_FILE, 1024 * 1024);
        assert!(HELPER.contains("MAX_BATCH = 8 * MAX_FILE"));
        assert_eq!(MAX_BATCH, 8 * MAX_FILE as usize);
        let root = tempfile::tempdir().unwrap();
        let mut batch = Batch::new();
        for i in 0..9 {
            std::fs::write(
                root.path().join(format!("f{i}")),
                vec![b'x'; MAX_FILE as usize],
            )
            .unwrap();
            batch.read(format!("f{i}")).unwrap();
        }
        let applied = helper(root.path())
            .send(batch, &Budget::new(Duration::from_secs(10)))
            .unwrap();
        assert!(applied.pulled.is_empty(), "a refused batch reads nothing");
        assert_eq!(
            applied.refused,
            [("f8".to_string(), "needs rsync".to_string())]
        );

        // The accepting half: eight at the limit come home in one exchange.
        let mut batch = Batch::new();
        for i in 0..8 {
            batch.read(format!("f{i}")).unwrap();
        }
        let applied = helper(root.path())
            .send(batch, &Budget::new(Duration::from_secs(10)))
            .unwrap();
        assert!(applied.refused.is_empty());
        assert_eq!(applied.pulled.len(), 8);
    }

    /// A directory a pull creates takes the server's mode, as rsync gave
    /// it, and one already here keeps its own. A mode without the owner's
    /// write still lets the file land below it.
    #[test]
    fn a_directory_a_pull_creates_takes_the_servers_mode() {
        use std::os::unix::fs::PermissionsExt;
        let server = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let mode =
            |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let set = |p: &std::path::Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        let sealed = server.path().join("kept/work/sealed");
        std::fs::create_dir_all(&sealed).unwrap();
        std::fs::write(sealed.join("001.sql"), b"CREATE TABLE").unwrap();
        for (dir, m) in [
            ("kept", 0o750),
            ("kept/work", 0o777),
            ("kept/work/sealed", 0o555),
        ] {
            set(&server.path().join(dir), m);
        }
        std::fs::create_dir(local.path().join("kept")).unwrap();
        set(&local.path().join("kept"), 0o700);

        let mut batch = Batch::new();
        batch.read("kept/work/sealed/001.sql".into()).unwrap();
        let applied = helper(server.path())
            .send(batch, &Budget::new(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(applied.pulled[0].parents, [0o750, 0o777, 0o555]);
        assert!(
            land(local.path(), &applied.pulled[0], None)
                .unwrap()
                .is_some()
        );

        let here = local.path().join("kept/work/sealed");
        assert_eq!(
            std::fs::read(here.join("001.sql")).unwrap(),
            b"CREATE TABLE"
        );
        assert_eq!(
            mode(&local.path().join("kept")),
            0o700,
            "already here: its own"
        );
        assert_eq!(mode(&local.path().join("kept/work")), 0o777);
        assert_eq!(mode(&here), 0o555);
        assert!(!here.join(".ulak-partial").exists());
        set(&here, 0o755);
        set(&sealed, 0o755);
    }

    /// Landing goes through real directories only. A directory the server
    /// made is created here; a symlinked parent — the walk never follows
    /// one — is refused and nothing is written outside the workspace; a
    /// staging path that is not a directory refuses too. Measured: a
    /// `generated` symlink to another directory received the bytes, and a
    /// missing `generated/` aborted the whole pull.
    #[test]
    fn a_pull_lands_through_real_directories_only_and_creates_the_servers() {
        let local = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let pulled = |path: &str| Pulled {
            path: path.into(),
            mode: 0o644,
            modified: (1_700_000_000, 0),
            content: crate::baseline::regular(b"server generated code", 0o644),
            bytes: b"server generated code".to_vec(),
            parents: Vec::new(),
        };
        // The server made `generated/`; this machine lacks it.
        assert!(
            land(local.path(), &pulled("generated/new.rs"), None)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            std::fs::read(local.path().join("generated/new.rs")).unwrap(),
            b"server generated code"
        );
        assert!(!local.path().join("generated/.ulak-partial").exists());

        // A symlinked parent: refused, and the target directory untouched.
        symlink(outside.path(), local.path().join("linked")).unwrap();
        assert!(
            land(local.path(), &pulled("linked/new.rs"), None)
                .unwrap()
                .is_none()
        );
        assert!(!outside.path().join("new.rs").exists());

        // A staging path that is a symlink beside the target: refused too.
        std::fs::create_dir(local.path().join("plain")).unwrap();
        symlink(outside.path(), local.path().join("plain/.ulak-partial")).unwrap();
        assert!(
            land(local.path(), &pulled("plain/new.rs"), None)
                .unwrap()
                .is_none()
        );
        assert!(!outside.path().join("new.rs").exists());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());

        // A project directory swapped for a symlink after it was chosen:
        // nothing lands through it.
        let base = tempfile::tempdir().unwrap();
        let anchor = base.path().join("project");
        symlink(outside.path(), &anchor).unwrap();
        assert!(
            land(&anchor, &pulled("generated.rs"), None)
                .unwrap()
                .is_none()
        );
        assert!(!outside.path().join("generated.rs").exists());

        // A path that leaves the workspace never lands anywhere.
        assert!(land(local.path(), &pulled("../escape.rs"), None).is_err());
        assert!(!local.path().parent().unwrap().join("escape.rs").exists());
    }

    /// Swaps two names atomically, so a directory and a symlink trade
    /// places with no instant where either is missing.
    #[cfg(target_os = "macos")]
    fn swap(a: &std::ffi::CStr, b: &std::ffi::CStr) -> std::io::Result<()> {
        // SAFETY: two valid NUL-terminated paths.
        match unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) } {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    #[cfg(target_os = "linux")]
    fn swap(a: &std::ffi::CStr, b: &std::ffi::CStr) -> std::io::Result<()> {
        // SAFETY: two valid NUL-terminated paths relative to the cwd fd.
        match unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                a.as_ptr(),
                libc::AT_FDCWD,
                b.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        } {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    /// A parent swapped for a symlink WHILE a pull lands never receives
    /// the bytes outside the workspace: `land` holds each directory open
    /// instead of re-resolving its path. Measured against the pathname
    /// version: a swap every 150 µs put `file-120.rs` outside within the
    /// first few hundred lands.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_parent_swapped_for_a_symlink_mid_landing_never_leaks_the_bytes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let local = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let parent = local.path().join("generated");
        let parked = local.path().join("parked");
        std::fs::create_dir_all(parent.join(".ulak-partial")).unwrap();
        std::fs::create_dir(outside.path().join(".ulak-partial")).unwrap();
        symlink(outside.path(), &parked).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let swaps = Arc::new(AtomicUsize::new(0));
        let changer = {
            let (running, swaps) = (running.clone(), swaps.clone());
            let a = std::ffi::CString::new(parent.as_os_str().as_encoded_bytes()).unwrap();
            let b = std::ffi::CString::new(parked.as_os_str().as_encoded_bytes()).unwrap();
            std::thread::spawn(move || {
                while running.load(Ordering::Relaxed) {
                    swap(&a, &b).expect("an atomic swap of a directory and a symlink");
                    swaps.fetch_add(1, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_micros(150));
                }
            })
        };
        let bytes = vec![b'R'; 4096];
        let content = crate::baseline::regular(&bytes, 0o644);
        let mut landed = 0;
        for i in 0..2000 {
            let pulled = Pulled {
                path: format!("generated/file-{i}.rs"),
                mode: 0o644,
                modified: (1_700_000_000, 0),
                content: content.clone(),
                bytes: bytes.clone(),
                parents: Vec::new(),
            };
            if matches!(land(local.path(), &pulled, None), Ok(Some(_))) {
                landed += 1;
            }
        }
        running.store(false, Ordering::Relaxed);
        changer.join().unwrap();
        // Both halves must have happened, or the race was never run.
        assert!(
            swaps.load(Ordering::Relaxed) > 100,
            "the parent was hardly swapped"
        );
        assert!(
            landed > 0,
            "no pull landed at all: the accepting half never ran"
        );
        let leaked: Vec<_> = std::fs::read_dir(outside.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != ".ulak-partial")
            .collect();
        assert!(
            leaked.is_empty(),
            "pulled bytes landed outside the workspace: {leaked:?}"
        );
        assert!(
            std::fs::read_dir(outside.path().join(".ulak-partial"))
                .unwrap()
                .next()
                .is_none(),
            "nothing was even staged outside the workspace"
        );
    }

    /// An incomplete sender or silent helper cannot hold the workspace lock
    /// forever. The same deadline covers writing and receipt collection.
    #[test]
    fn a_silent_helper_is_killed_at_the_exchange_deadline() {
        let mut command = python();
        command.args(["-c", "import time; time.sleep(60)"]);
        let start = Instant::now();
        assert!(
            Client::spawn(command, Vec::new(), &Budget::new(Duration::from_millis(50))).is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
