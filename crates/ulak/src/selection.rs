//! Exact path selection for a pull. Ownership is a set membership question,
//! not tens of thousands of ordered rsync glob rules. The ledger supplies FILE
//! rows: rejecting a claimed file must not reject server-born siblings.
//!
//! A selection is only a candidate list. `--existing` / `--ignore-existing`
//! still decide whether a receiving path may be written at transfer time;
//! an earlier filesystem snapshot cannot replace that decision.

use std::collections::BTreeSet;

/// Identity as well as nanosecond times: an editor's rename-over save
/// must remain visible even if it preserves the old length and mtime.
/// Serialized as hashcache.rs's key to a recorded digest, which is why
/// every field is here and none is derived.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Stamp {
    pub fn read(path: &std::path::Path) -> Option<Self> {
        let m = std::fs::symlink_metadata(path).ok()?;
        Some(Self::from_metadata(&m))
    }

    pub fn from_metadata(m: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mode: m.mode(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        }
    }

    /// The later of the file's mtime and ctime: the moment its bytes or
    /// its inode last changed, whichever a writer could not avoid moving.
    pub fn last_change(&self) -> (i64, i64) {
        self.modified.max(self.changed)
    }
}

/// Literal excludes cover a path and descendants. Unlike an rsync rule
/// list, lookup visits only the components of the candidate path.
pub struct Excludes(BTreeSet<Vec<u8>>);

impl Excludes {
    pub fn new(paths: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self(
            paths
                .into_iter()
                .map(|mut p| {
                    while p.ends_with(b"/") {
                        p.pop();
                    }
                    p
                })
                .collect(),
        )
    }

    pub fn contains(&self, path: &[u8]) -> bool {
        self.0.contains(path)
            || path
                .iter()
                .enumerate()
                .any(|(i, b)| *b == b'/' && self.0.contains(&path[..i]))
    }
}

/// Listing output is NUL-delimited. No decoding happens until after raw
/// byte excludes, so an ignored non-UTF8 filename remains harmless.
pub fn unclaimed(
    listing: &[u8],
    claimed: &[Vec<u8>],
    excludes: &Excludes,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let owned: BTreeSet<&[u8]> = claimed.iter().map(Vec::as_slice).collect();
    let mut selected = BTreeSet::new();
    if !listing.is_empty() && !listing.ends_with(&[0]) {
        return Err(
            crate::ui::fail!("the server's file inventory was truncated")
                .now("check the SSH connection and run ulak sync again")
                .into_err(),
        );
    }
    for raw in listing.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = raw.strip_prefix(b"./").unwrap_or(raw);
        if excludes.contains(path) || owned.contains(path) {
            continue;
        }
        if path.starts_with(b"/")
            || path
                .split(|b| *b == b'/')
                .any(|c| c.is_empty() || c == b"." || c == b"..")
            || path.contains(&b'\n')
            || path.contains(&b'\r')
            || std::str::from_utf8(path).is_err()
        {
            return Err(crate::ui::fail!("the server has a file name that cannot be synced safely")
                .now("rename files with newlines or non-UTF8 names, or protect their directory in ulak.toml").into_err());
        }
        selected.insert(path.to_vec());
    }
    Ok(selected.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ownership is per file: a generated sibling must still come home,
    /// including names that look like filter syntax.
    #[test]
    fn claimed_files_do_not_hide_generated_siblings() {
        let listing = b"./src/a\0./src/new\0./src/[a]*\0./src/new\0./cache/db\0./cache2/db\0";
        let got = unclaimed(
            listing,
            &[b"src/a".to_vec()],
            &Excludes::new([b"cache/".to_vec()]),
        )
        .unwrap();
        assert_eq!(
            got,
            [
                b"cache2/db".to_vec(),
                b"src/[a]*".to_vec(),
                b"src/new".to_vec()
            ]
        );
    }

    /// Refusing unsafe names after exclusions keeps ignored byte names
    /// usable without ever converting them to replacement characters.
    #[test]
    fn ignored_byte_names_are_filtered_before_decoding() {
        let excludes = Excludes::new([b"ignored".to_vec()]);
        assert_eq!(
            unclaimed(b"./ignored/\xff\0./ok\0", &[], &excludes).unwrap(),
            [b"ok".to_vec()]
        );
        for bad in [
            b"./bad\xff\0".as_slice(),
            b"../escape\0",
            b"/absolute\0",
            b"./incomplete",
            b"a\nb\0",
        ] {
            assert!(unclaimed(bad, &[], &excludes).is_err(), "{bad:?}");
        }
    }
}
