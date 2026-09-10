//! Per-workspace manifest: the identity contract.
//!
//! The namespace plus path hash in the workspace layout are LOCATORS; the
//! manifest carries them alongside the permanent identity (random UUID)
//! and a deterministic workload identity. Every sync verifies it — two projects can
//! never silently share one workspace, and a workspace belonging to another
//! project (a hash collision, or the same compose file under a different
//! `--project-directory`) is refused instead of overwritten. A MOVED
//! checkout is not detected: the workspace id is the hash of the client
//! namespace plus first compose file's absolute path, so the move lands
//! in a fresh workspace and the old one is simply orphaned. A retry for the
//! selected project carries its exact invocation. A foreign owner's cleanup
//! stays checkout-relative because this module does not know that project's
//! selectors and must not pretend the current project's invocation is theirs.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Project;
use crate::ui::fail;

pub const MANIFEST_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub uuid: String,
    #[serde(default)]
    pub namespace: String,
    pub identity: String,
    /// The workspace (or Compose project) directory this workspace serves.
    pub local_root: String,
    /// Layout base: workspace-relative paths equal anchor-relative paths.
    /// Stored so a widening footprint (a new sibling mount) is DETECTED
    /// instead of silently re-laying-out the workspace.
    #[serde(default)]
    pub anchor: String,
    pub host: String,
    pub created_unix: u64,
}

impl Manifest {
    /// The manifest we would write if the workspace were new.
    ///
    /// The UUID is handed in rather than minted here: it has to be the
    /// one THIS machine remembers for this workspace, or the comparison it
    /// exists for — "is the workspace on the server still mine?" — would
    /// compare a fresh random number against itself and always agree.
    pub fn candidate(project: &Project, dest: &str, uuid: String) -> Result<Manifest> {
        Ok(Manifest {
            version: MANIFEST_VERSION,
            uuid,
            namespace: project.workspace_namespace().to_string(),
            identity: project.compose_identity(),
            local_root: project.inv.project_dir.to_string_lossy().into_owned(),
            anchor: project.anchor.to_string_lossy().into_owned(),
            host: dest.to_string(),
            created_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        })
    }

    /// The existing manifest must belong to US. A mismatch means a hash
    /// collision or a foreign workspace — mutating it would destroy
    /// someone else's project.
    ///
    /// The CHECKOUT is what owns a workspace, so `local_root` is the
    /// whole question. `identity` used to be half of it and could not
    /// stay: a compose project name is docker's answer to one command,
    /// not a property of a directory, so it legitimately differs between
    /// two commands on the same tree. Measured — `ulak docker compose -p
    /// foo up -d` writes the manifest as `foo`, and the `ulak sync`
    /// after it (a ulak command, with no `-p` to carry) computes the
    /// directory's own name and was refused entry to its own workspace.
    /// Requiring them to agree also forbade something docker allows
    /// outright: two projects from one checkout, `-p a` and `-p b`, over
    /// the same files.
    ///
    /// It stays in the manifest, and in the message below, because it
    /// answers the question a human reading this error actually has —
    /// which stack is in there.
    pub fn verify_owner(&self, candidate: &Manifest, clean: &str, sync: &str) -> Result<()> {
        if self.namespace == candidate.namespace && self.local_root == candidate.local_root {
            return Ok(());
        }
        Err(fail!(
            "the workspace directory on the server belongs to a DIFFERENT project: \
             {} (checked out at {})",
            self.identity,
            self.local_root
        )
        .now("if that project is gone, remove its workspace there: ulak clean (run from ITS checkout), or delete the workspace dir by hand")
        .now(format!("if you moved this checkout, the old workspace is orphaned — run {clean}, then {sync}; a fresh workspace will be created"))
        .into_err())
    }
}

pub fn parse(bytes: &[u8], clean: &str, sync: &str) -> Result<Manifest> {
    serde_json::from_slice(bytes).map_err(|e| {
        fail!("the workspace manifest on the server is unreadable: {e}")
            .now(format!("recreate the workspace: {clean}, then {sync}"))
            .into_err()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(identity: &str, root: &str) -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            uuid: "u".into(),
            namespace: "client-a".into(),
            identity: identity.into(),
            local_root: root.into(),
            anchor: root.into(),
            host: "h".into(),
            created_unix: 0,
        }
    }

    #[test]
    fn a_manifest_refuses_a_different_checkout_or_namespace() {
        let verify = |owner: Manifest, candidate: Manifest| {
            owner.verify_owner(&candidate, "ulak clean", "ulak sync")
        };
        assert!(verify(m("a-1", "/x"), m("a-1", "/x")).is_ok());
        let err = verify(m("a-1", "/x"), m("a-2", "/y")).unwrap_err();
        assert!(err.to_string().contains("DIFFERENT project"));
        // A different CHECKOUT is a different owner…
        let err = verify(m("same", "/x"), m("same", "/y")).unwrap_err();
        assert!(err.to_string().contains("DIFFERENT project"));
        // …and a different project name over the same checkout is not.
        // `ulak sync` after `compose -p foo up -d` is that case, and so
        // is the `-p a` / `-p b` pair docker allows from one directory.
        assert!(verify(m("foo", "/x"), m("stack", "/x")).is_ok());

        let mut other_namespace = m("foo", "/x");
        other_namespace.namespace = "client-b".into();
        assert!(verify(m("foo", "/x"), other_namespace).is_err());
    }

    #[test]
    fn a_manifest_round_trip_preserves_identity() {
        let text = serde_json::to_string(&m("a-1", "/x")).unwrap();
        let back = parse(text.as_bytes(), "ulak clean", "ulak sync").unwrap();
        assert_eq!(back.identity, "a-1");
        assert!(parse(b"not json", "ulak clean", "ulak sync").is_err());
    }
}
