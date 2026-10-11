//! A bounded, read-only remote change index, independent of the writer.
//!
//! The helper registers watches before the initial reconcile. Events produced
//! during that reconcile are retained, closing the scan/subscribe gap. The
//! registration scan runs while sync.rs surveys both ends — read-only work
//! on both sides — and is waited for before the first byte is written. The
//! index is optional: it needs Linux inotify and Python 3.9 or later
//! (observer.py uses `str.removeprefix`), and any other host retains
//! periodic full reconciliation.
//! EOF, malformed input, overflow and watch loss invalidate the whole index.
//! See observer.py and inotify(7): directory watches are not recursive, and
//! mmap/hard-link writes need the helper's periodic metadata audit.
//!
//! A content digest plus mode and mtime distinguishes our own writes from
//! server output. A pull may acknowledge only an identical, stable local
//! observation; taking a fresh stamp alone would swallow an editor save made
//! during the transfer. The index never authorizes writes or deletion: sync.rs
//! applies its existing footprint, ignore and ledger rules to the candidates.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::proc::Budget;
use crate::selection::Stamp;
use crate::ssh::{Ssh, sh_quote};

const HELPER: &str = include_str!("observer.py");
const LIMIT: usize = 4096;
const MAX_LINE: usize = 65536;
// Asset observation is independent of the small-file writer's 1 MiB limit.
// A 3 MiB PNG must not invalidate every watcher after each ordinary save.
const MAX_FILE: u64 = 8 * 1024 * 1024;

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct Notice {
    ready: bool,
    pub path: String,
    digest: Option<String>,
    pub deleted: bool,
    mode: u32,
    modified: (i64, i64),
    error: Option<String>,
}

impl Notice {
    pub fn content(&self) -> Option<String> {
        self.digest
            .as_ref()
            .map(|digest| format!("f:{}:{digest}", self.mode))
    }

    pub fn matches(&self, path: &Path) -> Option<Stamp> {
        let expected = self.digest.as_ref()?;
        let file = File::open(path).ok()?;
        let before = file.metadata().ok()?;
        if !before.is_file()
            || before.len() > MAX_FILE
            || before.mode() & 0o7777 != self.mode
            || (before.mtime(), before.mtime_nsec()) != self.modified
        {
            return None;
        }
        let observed = Stamp::from_metadata(&before);
        if Stamp::read(path).as_ref() != Some(&observed) {
            return None;
        }
        let mut bytes = Vec::new();
        (&file).take(MAX_FILE + 1).read_to_end(&mut bytes).ok()?;
        if format!("{:x}", Sha256::digest(&bytes)) != *expected
            || Stamp::from_metadata(&file.metadata().ok()?) != observed
            || Stamp::read(path).as_ref() != Some(&observed)
        {
            return None;
        }
        Some(observed)
    }
}

#[derive(Default)]
struct Index {
    paths: BTreeMap<String, Notice>,
    failed: bool,
}

impl Index {
    fn insert(&mut self, notice: Notice) -> bool {
        let path = &notice.path;
        if notice.error.is_some()
            || notice.ready
            || path.is_empty()
            || path.split('/').any(|p| matches!(p, "" | "." | ".."))
            || path.contains(['\n', '\r', '\0'])
            || (!notice.deleted && notice.digest.is_none())
            || (!self.paths.contains_key(path) && self.paths.len() >= LIMIT)
        {
            self.failed = true;
            return false;
        }
        self.paths.insert(notice.path.clone(), notice);
        true
    }
}

pub struct Watcher {
    child: Child,
    command: Command,
    _input: ChildStdin,
    index: Arc<Mutex<Index>>,
}

fn read_notice(output: &mut impl BufRead) -> std::io::Result<Notice> {
    let mut line = Vec::new();
    output
        .take((MAX_LINE + 1) as u64)
        .read_until(b'\n', &mut line)?;
    if line.len() > MAX_LINE || !line.ends_with(b"\n") {
        return Err(std::io::Error::other("truncated remote change notice"));
    }
    serde_json::from_slice(&line).map_err(std::io::Error::other)
}

/// A helper whose registration scan is still running. `ready` turns it
/// into a `Watcher`; dropping it unasked kills the helper.
pub struct Pending {
    child: Option<Child>,
    command: Command,
    index: Arc<Mutex<Index>>,
    ready: mpsc::Receiver<(bool, ChildStdin)>,
}

impl Pending {
    pub fn ready(mut self, budget: &Budget) -> anyhow::Result<Watcher> {
        let Some(mut child) = self.child.take() else {
            return Err(unavailable());
        };
        match self.ready.recv_timeout(budget.remaining()) {
            Ok((true, input)) => Ok(Watcher {
                child,
                command: std::mem::replace(&mut self.command, Command::new("ssh")),
                _input: input,
                index: Arc::clone(&self.index),
            }),
            _ => {
                let _ = child.kill();
                let _ = crate::proc::wait_for(&mut child, crate::proc::PROBE);
                Err(unavailable())
            }
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = crate::proc::wait_for(&mut child, crate::proc::PROBE);
            crate::audit::record_command("ssh-observer", &self.command, None);
        }
    }
}

impl Watcher {
    /// Start the helper; its scan proceeds while the caller works.
    pub fn start(ssh: &Ssh, root: &str, scope: &serde_json::Value) -> anyhow::Result<Pending> {
        let mut scope = scope.clone();
        scope["max_file"] = serde_json::json!(MAX_FILE);
        let mut command = ssh.command();
        command.arg("--").arg(&ssh.dest).arg(format!(
            "python3 -u -c {} {} {}",
            sh_quote(&format!(
                "import sys; exec(sys.stdin.buffer.read({}))",
                HELPER.len()
            )),
            sh_quote(root),
            sh_quote(&scope.to_string()),
        ));
        Self::launch(command, HELPER.as_bytes().to_vec())
    }

    #[cfg(test)]
    fn spawn(command: Command, startup: Vec<u8>, budget: &Budget) -> anyhow::Result<Self> {
        Self::launch(command, startup)?.ready(budget)
    }

    fn launch(mut command: Command, startup: Vec<u8>) -> anyhow::Result<Pending> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn()?;
        crate::audit::record_command("ssh-observer", &command, None);
        let pipes = child.stdin.take().zip(child.stdout.take());
        let Some((mut input, output)) = pipes else {
            let _ = child.kill();
            let _ = crate::proc::wait_for(&mut child, crate::proc::PROBE);
            return Err(unavailable());
        };
        let index = Arc::new(Mutex::new(Index::default()));
        let shared = Arc::clone(&index);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut output = BufReader::new(output);
            let ready = input
                .write_all(&startup)
                .and_then(|_| input.flush())
                .and_then(|_| read_notice(&mut output))
                .is_ok_and(|n| n.ready && n.error.is_none());
            let _ = tx.send((ready, input));
            if ready {
                while let Ok(notice) = read_notice(&mut output) {
                    let Ok(mut state) = shared.lock() else { return };
                    if !state.insert(notice) {
                        return;
                    }
                }
            }
            if let Ok(mut state) = shared.lock() {
                state.failed = true;
            }
        });
        Ok(Pending {
            child: Some(child),
            command,
            index,
            ready: rx,
        })
    }

    pub fn healthy(&self) -> bool {
        self.index.lock().is_ok_and(|s| !s.failed)
    }

    pub fn pending(&self) -> bool {
        self.index
            .lock()
            .map(|s| s.failed || !s.paths.is_empty())
            .unwrap_or(true)
    }

    pub fn take(&mut self) -> Option<Vec<Notice>> {
        let mut state = self.index.lock().ok()?;
        if state.failed {
            return None;
        }
        let mut notices = Vec::new();
        while notices.len() < 64 {
            let Some((_, notice)) = state.paths.pop_first() else {
                break;
            };
            notices.push(notice);
        }
        Some(notices)
    }

    pub fn retain(&mut self, notices: Vec<Notice>) {
        if let Ok(mut state) = self.index.lock() {
            for notice in notices {
                // A newer event may already have arrived during the pull.
                if !state.paths.contains_key(&notice.path) {
                    state.insert(notice);
                }
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let code = crate::proc::wait_for(&mut self.child, crate::proc::PROBE)
            .ok()
            .flatten()
            .and_then(|s| s.code());
        crate::audit::record_command("ssh-observer", &self.command, code);
    }
}

fn unavailable() -> anyhow::Error {
    crate::ui::fail!("the server's change index is unavailable")
        .now("using periodic full reconciliation instead; the change index needs Linux and Python 3.9 or later on the server")
        .into_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(path: &str, data: &[u8]) -> Notice {
        Notice {
            path: path.into(),
            digest: Some(format!("{:x}", Sha256::digest(data))),
            ..Notice::default()
        }
    }

    /// Fast echo suppression cannot accept same-size bytes or a later edit
    /// merely because the filename and mtime still match the server notice.
    #[test]
    fn only_identical_stable_bytes_and_metadata_acknowledge_a_pull() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"first").unwrap();
        let m = std::fs::metadata(&path).unwrap();
        let mut n = notice("file", b"first");
        n.mode = m.mode() & 0o7777;
        n.modified = (m.mtime(), m.mtime_nsec());
        assert!(n.matches(&path).is_some());
        std::fs::write(&path, b"other").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(m.modified().unwrap())
            .unwrap();
        assert!(n.matches(&path).is_none());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("missing", &path).unwrap();
        assert!(n.matches(&path).is_none());
    }

    /// An overflow is a lost observation, never a valid empty queue.
    #[test]
    fn duplicate_notices_coalesce_but_overflow_invalidates_the_index() {
        let mut index = Index::default();
        for i in 0..LIMIT {
            assert!(index.insert(notice(&format!("file-{i}"), b"old")));
        }
        assert!(index.insert(notice("file-0", b"new")));
        assert_eq!(index.paths.len(), LIMIT);
        assert_eq!(
            index.paths["file-0"].digest,
            notice("file-0", b"new").digest
        );
        assert!(!index.insert(notice("one-too-many", b"new")));
        assert!(index.failed);
    }

    /// A malformed peer must not turn a relative files-from candidate into
    /// an absolute path or make an unsupported event disappear.
    #[test]
    fn unsafe_and_incomplete_notices_require_reconciliation() {
        for path in ["", "/abs", "../up", "a/../b", "a//b", "a\nb", "a\0b"] {
            let mut index = Index::default();
            assert!(!index.insert(notice(path, b"x")), "{path:?}");
            assert!(index.failed);
        }
        assert!(!Index::default().insert(Notice {
            path: "large".into(),
            ..Notice::default()
        }));
        assert!(read_notice(&mut &b"{\"path\":\"a\"}"[..]).is_err());
        assert!(read_notice(&mut vec![b'x'; MAX_LINE + 1].as_slice()).is_err());
    }

    /// A deletion has no content digest but must reach sync.rs, which decides
    /// whether the local file is restored remotely or its ledger is retired.
    #[test]
    fn deletion_notices_reach_the_sync_owner_without_a_digest() {
        let mut index = Index::default();
        assert!(index.insert(Notice {
            path: "src/removed".into(),
            deleted: true,
            ..Notice::default()
        }));
        assert!(!index.failed);
        assert!(index.paths["src/removed"].deleted);
    }

    /// Loss of a quiet SSH channel must wake recovery even with no files
    /// waiting in its queue; a successful handshake is not lasting health.
    #[test]
    fn eof_invalidates_an_initially_ready_watcher() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf '%s\\n' '{\"ready\":true}'; sleep 0.1"]);
        let watcher = Watcher::spawn(
            cmd,
            Vec::new(),
            &Budget::new(std::time::Duration::from_secs(2)),
        )
        .unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while watcher.healthy() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!watcher.healthy());
        assert!(watcher.pending());
    }
}
