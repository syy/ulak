//! A recorded digest may stand in for reading a file only while the file's
//! identity stamp still matches and the file was already old when hashed.
//!
//! A full reconcile fingerprints every selected leaf on both ends
//! (baseline.rs). Hashing every selected file costs orders of magnitude
//! more than the lstat walk that finds them, and a restart used to pay it
//! twice. Rereading a file whose device, inode, size, mode and
//! nanosecond mtime and ctime are unchanged proves nothing the stamp did
//! not; git's index and rsync's quick check rest on the same fact. The one
//! hole is a write landing in the same clock granule as the change that was
//! hashed — Linux stamps files from a coarse clock and some filesystems
//! keep whole seconds — so an entry is trusted only when the hashing moment
//! lies `SLACK` past the file's last change. A younger entry is recorded
//! but rehashed next time; git calls these racily clean. Nothing here is a
//! fact of its own: a cache that cannot be read is an empty cache, never
//! an error, and a mismatched stamp is simply a miss.
//!
//! Keys are the walk's anchor-relative UTF-8 names, under one file per
//! anchor (`workspaces/<id>/fingerprints/<anchor hash>.json`, checkout-wide
//! like the identity state beside it, since the files it describes are the
//! same for every destination): a lossy rendering of a path could serve
//! one file's digest for another. Entries not asked about in a session are
//! dropped on save, so the file tracks the current selection. Writers hold
//! the workspace lock, or an unlocked pull loses only its own additions to
//! a concurrent rename. The stamp's ctime comes from the filesystem's clock
//! and the hashing moment from this process's: a network mount with a
//! skewed server clock can trust a stale entry, the same trust rsync's
//! quick check extends there. baseline.py keeps the server's twin under the
//! workspace's private .baseline/ with the same rule; the two must not
//! diverge, and a test here runs baseline.py's own rule to keep them one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::baseline::Contents;
use crate::selection::Stamp;

/// How far past a file's last change the hashing moment must lie before
/// the stamp alone may answer for the bytes.
pub const SLACK: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    stamp: Stamp,
    /// When the hashing began, in Unix seconds and nanoseconds. Taken
    /// BEFORE the read: a write racing the read moves the ctime past it,
    /// and a write inside the same granule leaves this entry untrusted.
    hashed: (i64, i64),
    value: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct File {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

pub struct Cache {
    path: Option<PathBuf>,
    known: BTreeMap<String, Entry>,
    fresh: BTreeMap<String, Entry>,
}

impl Cache {
    pub fn open(key: &crate::config::WorkspaceStateKey, anchor: &Path) -> Cache {
        let path = crate::intent::workspace_dir_path(key.workspace_id()).map(|dir| {
            dir.join("fingerprints").join(format!(
                "{:x}.json",
                Sha256::digest(anchor.as_os_str().as_encoded_bytes())
            ))
        });
        Self::open_at(path)
    }

    fn open_at(path: Option<PathBuf>) -> Cache {
        let known = path
            .as_deref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<File>(&bytes).ok())
            .filter(|file| file.version == 1)
            .map(|file| file.entries)
            .unwrap_or_default();
        Cache {
            path,
            known,
            fresh: BTreeMap::new(),
        }
    }

    /// The fingerprint baseline.rs would compute, read from disk only when
    /// the recorded stamp no longer answers for it.
    pub fn fingerprint(&mut self, anchor: &Path, name: &str) -> Result<Option<String>> {
        let path = anchor.join(name);
        let hashed = unix_now();
        if let Some(current) = Stamp::read(&path)
            && let Some(entry) = self.known.get(name)
            && entry.stamp == current
            && trusted(entry)
        {
            self.fresh.insert(name.to_owned(), entry.clone());
            return Ok(Some(entry.value.clone()));
        }
        let found = crate::baseline::stamped(&path)?;
        if let Some((value, stamp)) = &found {
            self.fresh.insert(
                name.to_owned(),
                Entry {
                    stamp: stamp.clone(),
                    hashed,
                    value: value.clone(),
                },
            );
        }
        Ok(found.map(|(value, _)| value))
    }

    pub fn local(&mut self, anchor: &Path, names: &[Vec<u8>]) -> Result<Contents> {
        names
            .iter()
            .map(|p| {
                let name = std::str::from_utf8(p)?;
                Ok((name.to_owned(), self.fingerprint(anchor, name)?))
            })
            .collect()
    }

    /// The stamp each name was seen under in this session — the very stamp
    /// its fingerprint was read or trusted at, so a session that records
    /// these as its pre-transfer observations spends no second `lstat`.
    pub fn stamps(&self) -> impl Iterator<Item = (&str, &Stamp)> {
        self.fresh.iter().map(|(name, e)| (name.as_str(), &e.stamp))
    }

    /// The stamp one name was seen under in this session, if it was.
    pub fn stamp_of(&self, name: &str) -> Option<&Stamp> {
        self.fresh.get(name).map(|e| &e.stamp)
    }

    /// Persist what this session looked up. A failure is swallowed: the
    /// cache must never break the comparison it accelerates, and the next
    /// session merely hashes again.
    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        if self.fresh == self.known {
            return;
        }
        let file = File {
            version: 1,
            entries: self.fresh.clone(),
        };
        let Ok(bytes) = serde_json::to_vec(&file) else {
            return;
        };
        let Some(parent) = path.parent() else { return };
        if crate::invocation::private_dir(parent).is_err() {
            return;
        }
        let temp = path.with_extension(format!("tmp.{}", std::process::id()));
        if crate::invocation::write_private(&temp, &bytes).is_ok()
            && std::fs::rename(&temp, path).is_err()
        {
            let _ = std::fs::remove_file(&temp);
        }
    }
}

fn trusted(entry: &Entry) -> bool {
    let (secs, nanos) = entry.stamp.last_change();
    let changed = i128::from(secs) * 1_000_000_000 + i128::from(nanos);
    let hashed = i128::from(entry.hashed.0) * 1_000_000_000 + i128::from(entry.hashed.1);
    hashed - changed >= SLACK.as_nanos() as i128
}

fn unix_now() -> (i64, i64) {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, i64::from(d.subsec_nanos())),
        Err(_) => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn entry_for(path: &Path, value: &str, hashed: (i64, i64)) -> Entry {
        Entry {
            stamp: Stamp::read(path).unwrap(),
            hashed,
            value: value.to_owned(),
        }
    }

    /// The trust rule itself: an unchanged stamp answers only once the
    /// hashing moment lies SLACK past the file's last change. A file
    /// hashed in the same second it was written is recorded and rehashed,
    /// which is what closes the coarse-clock hole.
    #[test]
    fn a_stamp_answers_only_for_a_file_that_was_already_old_when_hashed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"one").unwrap();
        let (secs, nanos) = Stamp::read(&file).unwrap().last_change();
        assert!(!trusted(&entry_for(&file, "x", (secs, nanos))));
        assert!(!trusted(&entry_for(&file, "x", (secs + 1, nanos))));
        assert!(trusted(&entry_for(&file, "x", (secs + 2, nanos))));
        // An entry recorded from a clock that ran behind the filesystem's
        // is never trusted, whatever its stamp says.
        assert!(!trusted(&entry_for(&file, "x", (secs - 5, nanos))));
    }

    /// The accepting case, proven by a value the disk does not hold: a
    /// trusted entry is served without reading the file, and any change
    /// to the stamp — here the size — makes the cache read again.
    #[test]
    fn a_trusted_entry_is_served_without_reading_and_a_changed_stamp_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("anchor");
        std::fs::create_dir_all(anchor.join("src")).unwrap();
        let file = anchor.join("src/lib.rs");
        std::fs::write(&file, b"fn a() {}").unwrap();
        let store = dir.path().join("state/fingerprints/x.json");
        let (secs, nanos) = Stamp::read(&file).unwrap().last_change();

        let mut cache = Cache::open_at(Some(store.clone()));
        cache.known.insert(
            "src/lib.rs".into(),
            entry_for(&file, "f:644:planted", (secs + 10, nanos)),
        );
        assert_eq!(
            cache.fingerprint(&anchor, "src/lib.rs").unwrap().as_deref(),
            Some("f:644:planted"),
            "a trusted entry must answer without opening the file"
        );

        std::fs::write(&file, b"fn a() {} // longer").unwrap();
        let real = crate::baseline::fingerprint(&file).unwrap();
        assert_eq!(cache.fingerprint(&anchor, "src/lib.rs").unwrap(), real);
        assert_eq!(cache.fingerprint(&anchor, "src/missing.rs").unwrap(), None);
    }

    /// baseline.py keeps the server's twin of this cache under the same
    /// rule, in code this crate cannot share. Pinned by reading its
    /// `SLACK_NS` and running its own `stamp` and `trusted` on a file whose
    /// mtime lies years before its ctime: its stamp must be exactly the
    /// fields a `Stamp` holds (device, inode, size, mode, mtime, ctime),
    /// the times in nanoseconds and in the order `trusted` indexes, and
    /// both rules must refuse one nanosecond short of SLACK past the later
    /// time and trust at it. A rule keyed on the mtime alone, or `>` for
    /// `>=`, disagrees at one of those moments.
    #[test]
    fn the_servers_digest_cache_trusts_exactly_what_this_one_does() {
        let source = include_str!("baseline.py");
        let server_slack: i64 = source
            .lines()
            .find_map(|line| line.strip_prefix("SLACK_NS = "))
            .expect("baseline.py defines SLACK_NS at the top level")
            .trim()
            .parse()
            .expect("SLACK_NS is a plain integer literal");
        assert_eq!(
            i128::from(server_slack),
            SLACK.as_nanos() as i128,
            "baseline.py's SLACK_NS and hashcache.rs's SLACK must be one threshold"
        );
        let function = |name: &str| {
            source
                .split("\n\n\n")
                .map(str::trim)
                .find(|block| block.starts_with(&format!("def {name}(")))
                .unwrap_or_else(|| panic!("baseline.py lost `def {name}`"))
                .to_owned()
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"contents").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::new(1_000_000_000, 123))
            .unwrap();
        let stamp = Stamp::read(&path).unwrap();
        let fields = serde_json::to_value(&stamp).unwrap();
        let names: std::collections::BTreeSet<&str> = fields
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            names,
            ["changed", "dev", "ino", "mode", "modified", "size"].into(),
            "Stamp changed its fields; baseline.py's `stamp` must change with it"
        );
        let ns = |key: &str| {
            fields[key][0].as_i64().unwrap() * 1_000_000_000 + fields[key][1].as_i64().unwrap()
        };
        assert!(
            ns("modified") < ns("changed"),
            "the probe needs mtime before ctime"
        );
        let (secs, nanos) = stamp.last_change();
        let last = secs * 1_000_000_000 + nanos;
        let moments = [last - 1, last + server_slack - 1, last + server_slack];

        let program = format!(
            "import json, os, sys\nSLACK_NS = {server_slack}\n{}\n{}\n\
             current = stamp(os.lstat(sys.argv[1]))\n\
             print(json.dumps({{'stamp': current, 'trusted': \
             [trusted(current + [h, 'v'], current) for h in json.loads(sys.argv[2])]}}))\n",
            function("stamp"),
            function("trusted"),
        );
        let mut command = crate::stream::python();
        command
            .arg("-c")
            .arg(&program)
            .arg(&path)
            .arg(serde_json::to_string(&moments).unwrap());
        let out = crate::proc::run_bounded(&mut command, None, std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let server: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            server["stamp"],
            serde_json::json!([
                fields["dev"],
                fields["ino"],
                fields["size"],
                fields["mode"],
                ns("modified"),
                ns("changed"),
            ]),
            "baseline.py's stamp must cover exactly the fields Stamp compares"
        );
        let here: Vec<bool> = moments
            .iter()
            .map(|&h| {
                trusted(&Entry {
                    stamp: stamp.clone(),
                    hashed: (h.div_euclid(1_000_000_000), h.rem_euclid(1_000_000_000)),
                    value: "v".into(),
                })
            })
            .collect();
        assert_eq!(here, [false, false, true]);
        assert_eq!(
            server["trusted"],
            serde_json::json!(here),
            "both caches must trust a recorded digest at the same moment"
        );
    }

    /// What survives a save: only the names this session asked about, at
    /// 0600, replayable by the next session; a corrupt file is an empty
    /// cache and not a refusal to sync.
    #[test]
    fn a_save_keeps_the_session_selection_privately_and_corruption_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("anchor");
        std::fs::create_dir_all(&anchor).unwrap();
        std::fs::write(anchor.join("kept"), b"k").unwrap();
        std::fs::write(anchor.join("dropped"), b"d").unwrap();
        let store = dir.path().join("state/fingerprints/x.json");

        let mut cache = Cache::open_at(Some(store.clone()));
        cache
            .local(&anchor, &[b"kept".to_vec(), b"dropped".to_vec()])
            .unwrap();
        cache.save();
        assert_eq!(
            std::fs::metadata(&store).unwrap().mode() & 0o777,
            0o600,
            "the cache names every synced path"
        );

        let mut again = Cache::open_at(Some(store.clone()));
        assert_eq!(again.known.len(), 2);
        again.local(&anchor, &[b"kept".to_vec()]).unwrap();
        again.save();
        let third = Cache::open_at(Some(store.clone()));
        assert_eq!(third.known.keys().collect::<Vec<_>>(), ["kept"]);

        std::fs::write(&store, b"{").unwrap();
        let mut broken = Cache::open_at(Some(store));
        assert!(broken.known.is_empty());
        assert_eq!(
            broken.fingerprint(&anchor, "kept").unwrap(),
            crate::baseline::fingerprint(&anchor.join("kept")).unwrap()
        );
    }
}
