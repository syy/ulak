//! The file contract between the CLI and the service.
//!
//! Two JSON files per Docker stack, with exactly one writer each:
//!
//!   `stacks/<id>/desired.json`   the CLI writes, the service reads
//!   `stacks/<id>/status.json`    the service writes, the CLI reads
//!
//! `stacks/<id>` is keyed by SSH destination plus Docker Compose project
//! identity. That is Docker's namespace: two `-p` names from one checkout
//! are two stacks, while the same name on the same daemon is one stack
//! even when two checkouts address it. `nudge`, the one-shot doorbell the
//! CLI leaves and the service consumes, and `tunnels.pid` from
//! `forward.rs` share this directory. An unreadable `desired.json` is
//! parked here as `desired.bad`.
//!
//! File transport has a different owner and therefore a different key.
//! `workspaces/<id>` is the namespace-aware path hash from `WorkspaceKey`.
//! Its `uuid` and `anchor` describe this client's claim on the transported
//! workspace itself. Receipts about one remote copy live below
//! `destinations/<destination-id>`: `ledger`, `synced`, `pending` and
//! `pushed`. Two servers already have separate remote filesystems, but
//! without this local partition a deletion receipt from one retired the
//! other server's ownership row and let its stale copy come home again.
//! Several Docker stacks may deliberately share one transported footprint.
//! Folding the stack and workspace namespaces together made a bare `down`
//! retire an explicit `-p` stack Docker had left running, and made a second
//! `-p` stack overwrite the first one's service and tunnels.
//!
//! One writer per JSON file is what makes a lock unnecessary here. What
//! is still necessary is that a reader can never see half a file, so
//! every write lands in a sibling temp and is renamed into place:
//! rename within a directory is atomic, and a reader either sees the
//! whole old file or the whole new one.
//!
//! Both files open with `"schema": 3`. Unknown fields are ignored, so a
//! newer writer cannot break an older reader. A file that will not parse
//! at all is moved aside to `.bad` and the reader carries on with
//! nothing — "the intent file is corrupt" must never be the reason a
//! stack stops being maintained, and a corpse kept on disk is worth more
//! than one silently overwritten.
//!
//! `desired.json` carries the serialized invocation plus four facts the
//! service must NOT derive later: the destination daemon, Docker project
//! identity, client namespace and sync workspace selected when the user
//! declared the stack.
//! Config and compose files may change while that stack is still running;
//! re-answering any of those would silently switch the background job to a
//! different Docker stack or directory. Everything else stays derived:
//! no port list (the compose model answers that) and no anchor (the
//! footprint answers that). The service rebuilds the invocation with
//! `Invocation::rebuild_in`, the same constructor a typed `-f` goes
//! through, and validates it against the pinned workspace before acting.
//! Ulak's root management commands may use that same validated declaration;
//! the Docker command path never does.
//!
//! A declaration leaves this directory two ways. `clean` removes it together
//! with the bytes on the server. `clean --forget-destination` removes it
//! ALONE, for a server that no longer exists: `retire_declarations` takes
//! the declaration file and touches nothing under `workspaces/`, because
//! the receipts there describe bytes that are still wherever they were sent.
//!
//! One window is accepted rather than locked away: a retirement reads a
//! declaration and then removes the file, and a DIFFERENT checkout — its
//! own workspace lock, the same stack id — could replace that file between
//! the two syscalls and then finish an `up` before the removal lands. That
//! takes two checkouts on one stack, a server the user just declared dead
//! answering an `up`, and the retiring process stalled for the whole of
//! that `up` between a read and an unlink. What it costs when it happens:
//! that `up` ends with a running stack and no declaration, so the service
//! neither tends nor tunnels it until the user runs `up` once more. A
//! per-stack lock across every writer of `desired.json` is what closing it
//! would cost, and that is the trade taken.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::WorkspaceStateKey;
use crate::invocation::Invocation;

pub const SCHEMA: u32 = 3;

/// What the user asked for. Written by whichever CLI command changed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Desired {
    pub schema: u32,
    /// Docker's rule, deliberately: `up` means live until `down`. No
    /// TTL, no ceiling, no adopt command — the answer to "these pile up"
    /// is that `status` shows them, not that ulak forgets them.
    #[serde(default)]
    pub live: bool,
    /// The transported footprint this declaration used. Pinned so
    /// `clean` can refuse while ANY stack over these bytes is live, even
    /// if the compose files have since moved or stopped parsing.
    #[serde(default)]
    pub workspace_id: String,
    /// The client namespace above `workspace_id` on the server. It is
    /// pinned with the id: a later config edit must not move a live job.
    #[serde(default)]
    pub workspace_namespace: String,
    /// The Docker daemon this stack lives on. A later config edit must
    /// not move an already-running stack's background job elsewhere.
    #[serde(default)]
    pub destination: String,
    /// Docker Compose's resolved project name at declaration time.
    #[serde(default)]
    pub identity: String,
    /// Where the user stood. Compose resolves relative paths from it, so
    /// it is part of the invocation, not decoration.
    #[serde(default)]
    pub cwd: String,
    /// The compose globals in compose's own argv shape.
    #[serde(default)]
    pub argv_globals: Vec<String>,
    /// The `COMPOSE_*` variables that were in force at the time.
    #[serde(default)]
    pub compose_env: BTreeMap<String, String>,
    #[serde(default)]
    pub updated_unix: u64,
}

/// What the service last saw. Written by the per-destination worker
/// (`service::Dest::publish`) — when a probe falls due, when a workspace
/// was just tended, and after a failed connect — and read by the CLI.
/// One writer, so the two sides cannot end up looking at different
/// truths.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub schema: u32,
    #[serde(default)]
    pub updated_unix: u64,
    /// `up`, `down` or `blocked` — the last thing a probe established.
    #[serde(default)]
    pub connection: String,
    /// When `connection` last changed, so status can say "for 3 hours"
    /// instead of repeating a bare word.
    #[serde(default)]
    pub since_unix: u64,
    #[serde(default)]
    pub tunnels: Vec<TunnelState>,
    #[serde(default)]
    pub last_sync_unix: u64,
    /// Files the service brought back into the repo while nobody was
    /// looking. The one number a user cannot reconstruct afterwards.
    #[serde(default)]
    pub pulled_total: u64,
    /// What the service could not do, in one sentence.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelState {
    pub port: u32,
    #[serde(default)]
    pub service: String,
    /// False means the port is taken on this machine: the honest half of
    /// the tunnel report, and the reason `forward` stopped exiting 0.
    #[serde(default)]
    pub open: bool,
    /// Whether a running container was behind this tunnel's service at
    /// the last probe — the THIRD state. An open tunnel is bound the
    /// moment the stack is declared (deliberately: nothing else keeps
    /// the local port safe until the service starts), so "open" alone
    /// reported a never-started service exactly like a running one, and
    /// the user went debugging a server-side service they had never
    /// brought up. `service::publish` stamps this from the probe's
    /// answer. `None` means the writer did not know (an older service's
    /// file) and must render as it always did — which is also why this
    /// is a new `#[serde(default)]` field and SCHEMA stays put: the
    /// reader refuses files from a NEWER schema, so bumping it would
    /// make an older `ulak status` show no tunnels at all.
    #[serde(default)]
    pub service_running: Option<bool>,
}

// ─── paths ──────────────────────────────────────────────────────────

/// The per-workspace directory. Checkout-wide identity and layout state live
/// here; destination-specific sync receipts live one level further down.
pub fn workspace_dir(workspace_id: &str) -> Option<PathBuf> {
    let root = crate::invocation::state_dir()?.join("workspaces");
    workspace_dir_in(&root, workspace_id)
}

/// Where the per-workspace directory WOULD be, without creating it.
///
/// Asking a question must not leave a thing behind. `recorded_uuid` is a
/// read, and it is asked about ids that may not be ours at all — through
/// the creating variant, every such question became a directory that
/// state cleanup then had to carry forever. Measured on this machine: 22
/// of them, one per `cargo test` run, in the developer's real state dir.
pub fn workspace_dir_path(workspace_id: &str) -> Option<PathBuf> {
    Some(
        crate::invocation::state_dir()?
            .join("workspaces")
            .join(workspace_id),
    )
}

/// The same thing with the root handed in, so the tests never have to
/// reach for a process-wide environment variable — which, with the test
/// harness running them in one process, would race every other test that
/// touches the state directory.
fn workspace_dir_in(root: &Path, workspace_id: &str) -> Option<PathBuf> {
    let dir = root.join(workspace_id);
    crate::invocation::private_dir(&dir).ok()?;
    Some(dir)
}

/// The receipt directory for one workspace copy on one SSH destination.
pub(crate) fn workspace_state_dir(key: &WorkspaceStateKey) -> Option<PathBuf> {
    let dir = workspace_state_dir_path(key)?;
    crate::invocation::private_dir(&dir).ok()?;
    Some(dir)
}

/// Where destination-specific receipts WOULD be, without creating it.
pub(crate) fn workspace_state_dir_path(key: &WorkspaceStateKey) -> Option<PathBuf> {
    Some(
        workspace_dir_path(key.workspace_id())?
            .join("destinations")
            .join(key.destination_id()),
    )
}

/// The per-stack directory. Its opaque id is `stack_id`: the daemon and
/// Docker Compose project namespace, not the local checkout.
pub fn stack_dir(stack_id: &str) -> Option<PathBuf> {
    let root = crate::invocation::state_dir()?.join("stacks");
    stack_dir_in(&root, stack_id)
}

/// Where the per-stack directory WOULD be, without creating it.
pub fn stack_dir_path(stack_id: &str) -> Option<PathBuf> {
    Some(
        crate::invocation::state_dir()?
            .join("stacks")
            .join(stack_id),
    )
}

fn stack_dir_in(root: &Path, stack_id: &str) -> Option<PathBuf> {
    let dir = root.join(stack_id);
    crate::invocation::private_dir(&dir).ok()?;
    Some(dir)
}

/// Docker's stack namespace is one Compose project name on one daemon.
/// NUL is an unambiguous separator: neither an SSH destination nor a
/// Compose project name can contain it.
pub fn stack_id(destination: &str, identity: &str) -> String {
    let mut key = Vec::with_capacity(destination.len() + identity.len() + 1);
    key.extend_from_slice(destination.as_bytes());
    key.push(0);
    key.extend_from_slice(identity.as_bytes());
    crate::hashid::fnv1a128_hex(&key)
}

pub const SYNCED: &str = "synced";
const DESIRED: &str = "desired.json";
const STATUS: &str = "status.json";
const NUDGE: &str = "nudge";

/// Leave a nudge for the service: somebody just ran a compose command
/// here, so whatever the service believes about this stack is stale.
///
/// This is a doorbell, and without it there is none. The service can only
/// DISCOVER that a stack came up, by asking — and it asks a destination
/// whose every stack is down once every two minutes, which is exactly the
/// window a stack comes up in. So the slow cadence is chosen *because*
/// nothing is running, and the moment something starts running is the
/// moment least likely to be noticed. Measured on a real project: `up`
/// returned 82 seconds before the ports came home.
///
/// A file, because that is the entire channel between the two halves —
/// there is no socket to ring, by design. Not a field in `desired.json`:
/// the intent did not change (a `restart` changes nothing about what the
/// user wants), and that file's stamp has one-second resolution, so a
/// command that finished inside a second would move nothing at all.
pub fn nudge(stack_id: &str) {
    // A command against a stack with no declaration has no background
    // job to wake. Creating a directory for every one-off `-p … ps`
    // would turn questions into permanent catalog entries.
    if read_desired(stack_id).is_none() {
        return;
    }
    if let Some(dir) = stack_dir(stack_id) {
        nudge_in(&dir);
    }
}

/// Take the nudge, if there is one.
///
/// Consuming it is the whole design: a nudge that stayed would turn one
/// typed command into a permanently fast cadence, which is the cost
/// `SILENT_PROBE_EVERY` exists to avoid. The read side deliberately does
/// NOT create the workspace directory — asking a question must not leave a
/// thing behind.
///
/// Racing a second `nudge` can only cost one extra probe (0.27 s for the
/// whole destination, measured on Docker 29.6.2 over a warm control
/// socket), never a missed one: the file is written
/// after the command, and the command is what changed the world.
pub fn take_nudge(stack_id: &str) -> bool {
    stack_dir_path(stack_id).is_some_and(|d| take_nudge_in(&d))
}

/// The two halves with the directory handed in, so the tests never reach
/// for a process-wide state directory — and so they can never write into
/// the developer's own.
fn nudge_in(dir: &Path) {
    let _ = crate::invocation::write_private(&dir.join(NUDGE), b"");
}

fn take_nudge_in(dir: &Path) -> bool {
    std::fs::remove_file(dir.join(NUDGE)).is_ok()
}

/// Every Docker stack this machine keeps lifecycle state for.
pub fn stack_ids() -> Vec<String> {
    let Some(root) = crate::invocation::state_dir().map(|s| s.join("stacks")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort();
    ids
}

/// The stacks and what each was last asked to be. A stack with no
/// readable intent is left out: "there is nothing to maintain here" is
/// the same answer a fresh machine gives.
pub fn catalog() -> Vec<(String, Desired)> {
    stack_ids()
        .into_iter()
        .filter_map(|id| {
            let d = read_desired(&id)?;
            Some((id, d))
        })
        .collect()
}

// ─── read / write ───────────────────────────────────────────────────

pub fn read_desired(stack_id: &str) -> Option<Desired> {
    read(&stack_dir_path(stack_id)?.join(DESIRED))
}

pub fn read_status(stack_id: &str) -> Option<Status> {
    read(&stack_dir_path(stack_id)?.join(STATUS))
}

pub fn write_desired(stack_id: &str, desired: &Desired) -> std::io::Result<()> {
    let dir = stack_dir(stack_id).ok_or_else(|| {
        std::io::Error::other("HOME is not set, so ulak has nowhere to keep its state")
    })?;
    write_atomic(&dir.join(DESIRED), desired)
}

/// The service's half of the contract.
pub fn write_status(stack_id: &str, status: &Status) -> std::io::Result<()> {
    let dir = stack_dir(stack_id).ok_or_else(|| {
        std::io::Error::other("HOME is not set, so ulak has nowhere to keep its state")
    })?;
    write_atomic(&dir.join(STATUS), status)
}

/// Forget every remote copy's sync state and every stack that referred to
/// the checkout.
///
/// Test-only: ordinary `clean` removes one destination through the
/// narrower function below, and nothing in production discards a whole
/// checkout's claim. Kept as the contrast those tests measure the narrow
/// delete against.
#[cfg(test)]
pub fn forget_workspace(workspace_id: &str) {
    if let Some(dir) =
        crate::invocation::state_dir().map(|s| s.join("workspaces").join(workspace_id))
    {
        let _ = std::fs::remove_dir_all(&dir);
    }
    for (stack_id, desired) in catalog() {
        if desired.workspace_id == workspace_id
            && let Some(dir) = stack_dir_path(&stack_id)
        {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// What one offline retirement removed, and what it could not.
///
/// `#[must_use]`: the receipt is the only thing that tells a user which
/// declarations went and which removals failed, and a declaration still on
/// disk still wedges every root command.
#[derive(Debug, Default, PartialEq, Eq)]
#[must_use]
pub struct Retired {
    /// One per declaration removed, sorted by identity — it reaches output.
    pub stacks: Vec<RetiredStack>,
    pub failures: Vec<Trouble>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RetiredStack {
    pub identity: String,
    /// Still declared live when it went. The service was keeping this
    /// stack and now stops — the half worth saying out loud.
    pub live: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Trouble {
    pub path: PathBuf,
    pub why: String,
}

/// Retire every declaration one checkout made on one destination, without
/// contacting it.
///
/// DECLARATIONS ONLY. Nothing under `workspaces/<id>/destinations/<dest>` —
/// not the ledger, not `synced`, `pushed` or `pending` — and neither `uuid`
/// nor `anchor`. The wedge this ends is made entirely of
/// `stacks/<id>/desired.json`: `management::locate`'s declaration rung, the
/// service worker and `Fleet::reload` read that file and nothing under
/// `workspaces/`. The ledger stays because the bytes it describes stay: a
/// ledger retired while its remote copy still exists would be the first
/// such state in this product, and `pull_back`'s creating pass — which
/// excludes exactly the rows in that ledger — would plant every file the
/// user had deleted locally straight back into the checkout if the server
/// ever answered again. `clean` remains the one thing that retires a
/// ledger, because it is the one thing that removes those bytes.
///
/// The declaration FILE, not the directory. `tunnels.pid` stays where it
/// is: the service learns of a retirement at its next catalog read, and a
/// service killed inside that window leaves an ssh tunnel child behind
/// whose only record is that file — `forward::sweep_orphans` finds it
/// there at the next start, and a `remove_dir_all` would have made the
/// orphan permanent, holding a localhost port against every later stack.
/// The directory itself goes only once nothing is left in it; one that
/// stays has no `desired.json`, which `catalog` skips for good.
pub fn retire_declarations(workspace_id: &str, destination: &str) -> Retired {
    let Some(root) = crate::invocation::state_dir().map(|dir| dir.join("stacks")) else {
        return Retired::default();
    };
    retire_declarations_in(&root, workspace_id, destination)
}

/// The same with the stacks root handed in, so tests never touch the
/// process-wide state root every other unit test in this binary shares.
fn retire_declarations_in(root: &Path, workspace_id: &str, destination: &str) -> Retired {
    let mut report = Retired::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        return report;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        // Read, never recompute: `read` is the one place that decides what
        // "unreadable" means, and a declaration it sets aside as .bad is
        // nobody's to retire.
        let Some(desired) = read::<Desired>(&dir.join(DESIRED)) else {
            continue;
        };
        if desired.workspace_id != workspace_id || desired.destination != destination {
            continue;
        }
        match std::fs::remove_file(dir.join(DESIRED)) {
            Ok(()) => {
                // A nudge is a doorbell for a stack that no longer exists,
                // and the status is the service's report about it; neither
                // is a recovery record. The pid file is, and it is kept.
                let _ = std::fs::remove_file(dir.join(NUDGE));
                let _ = std::fs::remove_file(dir.join(STATUS));
                let _ = std::fs::remove_dir(&dir);
                report.stacks.push(RetiredStack {
                    identity: desired.identity,
                    live: desired.live,
                });
            }
            Err(e) => report.failures.push(Trouble {
                path: dir.join(DESIRED),
                why: e.to_string(),
            }),
        }
    }
    report.stacks.sort_by(|a, b| a.identity.cmp(&b.identity));
    report.failures.sort_by(|a, b| a.path.cmp(&b.path));
    report
}

/// Forget only the local receipts and retired lifecycle records belonging to
/// one remote filesystem copy.
///
/// `clean` removes one destination's remote workspace. The UUID and anchor
/// stay checkout-wide, and another destination's ledger must survive even
/// when it currently has no live stack; otherwise its stale files can be
/// mistaken for server-born data on the next pull.
pub fn forget_workspace_destination(key: &WorkspaceStateKey, destination: &str) {
    if let Some(dir) = workspace_state_dir_path(key) {
        let _ = std::fs::remove_dir_all(&dir);
        // `destinations/` empties only when the LAST remote copy is gone,
        // so `remove_dir` succeeding is the signal — and then the UUID
        // and anchor underneath it own nothing. Keeping them is not
        // harmless: `settle_uuid` only asks who owns a workspace when no
        // UUID is recorded, so a retained claim silently skips the
        // ambiguity check the next sync owes a deliberately shared
        // namespace, and the checkout would adopt another machine's
        // workspace instead of refusing it.
        if let Some(parent) = dir.parent()
            && std::fs::remove_dir(parent).is_ok()
            && let Some(workspace) = workspace_dir_path(key.workspace_id())
        {
            let _ = std::fs::remove_dir_all(workspace);
        }
    }
    for (stack_id, desired) in catalog() {
        if desired.workspace_id == key.workspace_id()
            && desired.destination == destination
            && let Some(dir) = stack_dir_path(&stack_id)
        {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Parse, or set the file aside and report nothing.
///
/// A truncated or hand-edited file is not an emergency and must not be
/// one: the caller's whole answer is "there is no intent here", which is
/// the same answer a fresh machine gives. It is moved rather than
/// deleted so the evidence survives the incident.
fn read<T: serde::de::DeserializeOwned + HasSchema>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice::<T>(&bytes) {
        // A file from a NEWER ulak is not corrupt, it is just not
        // ours to read. Leave it exactly where it is.
        Ok(v) if v.schema() > SCHEMA => None,
        Ok(v) => Some(v),
        Err(_) => {
            let _ = std::fs::rename(path, path.with_extension("bad"));
            crate::ui::warn(&format!(
                "{} could not be read and was set aside as .bad",
                path.display()
            ));
            None
        }
    }
}

/// Write through a sibling temp + rename, so a reader never sees a half
/// file. The temp carries the pid: two writers would be a bug (one
/// writer per file is the whole reason there is no lock here), but if it
/// ever happens they must not corrupt each other's temp.
fn write_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    crate::invocation::write_private(&tmp, &json)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// So the reader can refuse a file from a newer ulak without knowing
/// which of the two shapes it is holding.
trait HasSchema {
    fn schema(&self) -> u32;
}
impl HasSchema for Desired {
    fn schema(&self) -> u32 {
        self.schema
    }
}
impl HasSchema for Status {
    fn schema(&self) -> u32 {
        self.schema
    }
}

// ─── the intent itself ──────────────────────────────────────────────

impl Desired {
    /// Serialize the RESOLVED invocation, not the argv the user typed —
    /// see `Invocation::globals_argv`. What goes in is exactly what
    /// `Desired::rebuild` takes back out, and the round trip is the point.
    pub fn of(
        project: &crate::config::Project,
        destination: &str,
        identity: &str,
        live: bool,
    ) -> Desired {
        let inv = &project.inv;
        Desired {
            schema: SCHEMA,
            live,
            workspace_id: project.workspace_id().to_string(),
            workspace_namespace: project.workspace_namespace().to_string(),
            destination: destination.to_string(),
            identity: identity.to_string(),
            cwd: inv.cwd.to_string_lossy().into_owned(),
            argv_globals: inv.globals_argv(),
            compose_env: inv.compose_env.clone(),
            updated_unix: now_unix(),
        }
    }

    /// Rebuild the invocation this file describes. The service and the
    /// CLI go through the same constructor, so no second way to derive a
    /// path, a port or an identity can grow anywhere in the codebase.
    ///
    /// Untrusted input by contract: the file is plain JSON on disk
    /// that a service with the user's ssh keys will act on, so its
    /// paths get the same checking a typed `-f` gets — which is what
    /// going through `Invocation::rebuild_in` buys: the same `build()` a
    /// typed `-f` runs. Invocation construction is stateless; only this
    /// stack-owned declaration persists. The service and Ulak's root
    /// management commands may recover it, but it never becomes input to a
    /// later Docker command.
    fn rebuild(&self) -> anyhow::Result<Invocation> {
        Invocation::rebuild_in(
            Path::new(&self.cwd),
            &self.argv_globals,
            self.compose_env.clone(),
        )
    }

    /// Rebuild and bind an on-disk declaration to the stack directory
    /// where it was found. The state file is untrusted input used by a
    /// service holding SSH keys, so every caller goes through this one
    /// validation rather than independently trusting its paths or host.
    pub(crate) fn rebuild_for_stack(&self, found_as: &str) -> anyhow::Result<Rebuilt> {
        let inv = self.rebuild()?;
        let workspace_key = match crate::config::WorkspaceKey::from_namespace(
            &self.workspace_namespace,
            inv.workspace_identity_path(),
        ) {
            Ok(key) => key,
            Err(_) => return Err(invalid_declaration(found_as)),
        };
        if self.workspace_id.is_empty()
            || self.workspace_namespace.is_empty()
            // Not `is_empty`: the destination reaches OpenSSH and rsync as
            // an argv word. Checked HERE rather than where it is used so the
            // remedy names the stack file it came from — `config`'s own
            // refusal would offer to edit a TOML layer that never held it.
            || crate::config::validate_ssh_dest(&self.destination).is_err()
            || self.identity.is_empty()
            || self.stack_id() != found_as
            || workspace_key.id() != self.workspace_id
        {
            return Err(invalid_declaration(found_as));
        }
        Ok(Rebuilt { inv, workspace_key })
    }

    /// Turn one already-validated declaration into the historical project
    /// both the background service and root management commands must use.
    pub(crate) fn project_from_rebuilt(
        &self,
        rebuilt: Rebuilt,
    ) -> anyhow::Result<crate::config::Project> {
        let mut project =
            crate::config::Project::locate_from_pinned(rebuilt.inv, rebuilt.workspace_key)?;
        project.pin_existing_stack(&self.destination, &self.identity);
        Ok(project)
    }

    /// Validate and rebuild in one step for callers that do not need to
    /// inspect the invocation while choosing among declarations.
    pub(crate) fn project_for_stack(
        &self,
        found_as: &str,
    ) -> anyhow::Result<crate::config::Project> {
        self.project_from_rebuilt(self.rebuild_for_stack(found_as)?)
    }

    pub fn stack_id(&self) -> String {
        stack_id(&self.destination, &self.identity)
    }
}

fn invalid_declaration(found_as: &str) -> anyhow::Error {
    crate::ui::fail!(
        "the file that says what to keep alive here does not match its stack or sync workspace"
    )
    .now(format!(
        "declare it again from the project itself: rm -rf {}, then ulak docker compose up -d",
        stack_dir_path(found_as)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| found_as.to_string())
    ))
    .into_err()
}

#[derive(Debug)]
pub(crate) struct Rebuilt {
    pub(crate) inv: Invocation,
    pub(crate) workspace_key: crate::config::WorkspaceKey,
}

/// Record what the user asked for, and hand back the whole declaration
/// that was there before.
///
/// The WHOLE value is load-bearing now that one Docker stack can be
/// addressed from different checkouts. A failed `up` from checkout B
/// must restore checkout A's workspace, files and destination, not just
/// copy A's `live` bit onto B's declaration. Otherwise the service
/// abandons the bytes the still-running stack actually uses.
pub fn declare(desired: &Desired) -> Option<Desired> {
    let id = desired.stack_id();
    let previous = read_desired(&id);
    let _ = write_desired(&id, desired);
    previous
}

/// Put one whole declaration back after a command failed.
///
/// Only over a declaration that is still there. `declare` wrote one before
/// the command ran, so its absence means a `clean` or an offline retirement
/// removed the stack underneath this command — and writing the old
/// declaration back would resurrect the exact wedge the retirement ended,
/// under a receipt that said it was over. `stack_dir_path` rather than
/// `stack_dir` for the same reason: a rollback must not recreate the
/// directory it is asking about.
pub fn restore(stack_id: &str, desired: &Desired) {
    let Some(dir) = stack_dir_path(stack_id) else {
        return;
    };
    if dir.join(DESIRED).is_file() {
        restore_in(&dir, desired);
    }
}

fn restore_in(dir: &Path, desired: &Desired) {
    let _ = write_atomic(&dir.join(DESIRED), desired);
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stack directory under a temp root — no environment variable is
    /// touched, so these run beside every other test instead of racing
    /// them for a process-wide XDG_STATE_HOME.
    fn sandbox() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = stack_dir_in(&tmp.path().join("stacks"), "m").unwrap();
        (tmp, dir)
    }

    /// The doorbell rings ONCE. Both halves of that matter and both were
    /// reasoned about, not guessed:
    ///   * it has to survive being left — the service may read the
    ///     catalog a moment before the command finishes, or not be
    ///     running at all;
    ///   * it has to be spent when read — a nudge that stayed would turn
    ///     one typed command into a permanently fast probe cadence, which
    ///     is the whole cost `SILENT_PROBE_EVERY` exists to avoid.
    #[test]
    fn a_nudge_is_taken_exactly_once() {
        let (_tmp, dir) = sandbox();
        assert!(!take_nudge_in(&dir), "no nudge yet, so nothing to take");

        nudge_in(&dir);
        assert!(dir.join(NUDGE).is_file());
        assert!(take_nudge_in(&dir), "the nudge is there and is taken");
        assert!(
            !take_nudge_in(&dir),
            "and it is GONE — otherwise every catalog read would probe again, forever"
        );

        // Ringing twice before anyone answers is still one answer.
        nudge_in(&dir);
        nudge_in(&dir);
        assert!(take_nudge_in(&dir));
        assert!(!take_nudge_in(&dir));
    }

    fn stacks_root() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("stacks");
        std::fs::create_dir_all(&root).unwrap();
        (tmp, root)
    }

    fn declare_in(root: &Path, workspace: &str, destination: &str, identity: &str, live: bool) {
        let desired = Desired {
            schema: SCHEMA,
            live,
            workspace_id: workspace.into(),
            workspace_namespace: "client-a".into(),
            destination: destination.into(),
            identity: identity.into(),
            cwd: "/checkout/a".into(),
            argv_globals: vec!["-f".into(), "a.yaml".into()],
            compose_env: BTreeMap::new(),
            updated_unix: 1,
        };
        let dir = stack_dir_in(root, &desired.stack_id()).unwrap();
        write_atomic(&dir.join(DESIRED), &desired).unwrap();
    }

    /// The accepting case, which is the whole feature: a server that no
    /// longer exists is retired from this machine alone. Written first,
    /// because the refusals are the easy half — the user who reported this
    /// could reach every refusal in the product and none of them ended.
    #[test]
    fn retiring_a_destination_removes_its_declarations_and_leaves_every_other_one() {
        let (_tmp, root) = stacks_root();
        declare_in(&root, "checkout-a", "dead-server", "api", true);
        declare_in(&root, "checkout-a", "dead-server", "web", false);
        declare_in(&root, "checkout-a", "live-server", "api", true);
        declare_in(&root, "checkout-b", "dead-server", "other", true);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 4);

        let report = retire_declarations_in(&root, "checkout-a", "dead-server");

        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(
            report.stacks,
            vec![
                RetiredStack {
                    identity: "api".into(),
                    live: true
                },
                RetiredStack {
                    identity: "web".into(),
                    live: false
                },
            ],
            "sorted by identity, and the live bit survives into the receipt"
        );
        // The two that must survive: another server, and another checkout
        // on the same dead one. Both are somebody else's answer to give.
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        let left = retire_declarations_in(&root, "checkout-a", "live-server");
        assert_eq!(left.stacks.len(), 1, "the live server's declaration stayed");
        let other = retire_declarations_in(&root, "checkout-b", "dead-server");
        assert_eq!(
            other.stacks.len(),
            1,
            "the other checkout's declaration stayed"
        );
    }

    /// A live tunnel's pid file is the one record an orphan sweep has, so
    /// retiring the declaration must leave it — and the directory that
    /// holds it — in place. Everything else about the stack goes.
    #[test]
    fn retiring_a_declaration_keeps_the_tunnel_pid_record_for_the_orphan_sweep() {
        let (_tmp, root) = stacks_root();
        declare_in(&root, "checkout-a", "dead-server", "api", true);
        let dir = std::fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(dir.join("tunnels.pid"), b"4242\n").unwrap();
        std::fs::write(dir.join(STATUS), b"{}").unwrap();
        std::fs::write(dir.join(NUDGE), b"").unwrap();

        let report = retire_declarations_in(&root, "checkout-a", "dead-server");
        assert_eq!(report.stacks.len(), 1);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!dir.join(DESIRED).exists(), "the declaration is gone");
        assert!(!dir.join(STATUS).exists() && !dir.join(NUDGE).exists());
        assert_eq!(
            std::fs::read(dir.join("tunnels.pid")).unwrap(),
            b"4242\n",
            "the pid record survives for forward::sweep_orphans"
        );
        assert!(
            read::<Desired>(&dir.join(DESIRED)).is_none(),
            "and the leftover directory is no declaration"
        );
    }

    /// A different spelling is a different host, and silence would read as
    /// success. `declared_on` refuses first in production; this pins the
    /// half that would otherwise delete the wrong thing.
    #[test]
    fn a_destination_that_matches_nothing_retires_nothing() {
        let (_tmp, root) = stacks_root();
        declare_in(&root, "checkout-a", "dead-server", "api", true);

        let report = retire_declarations_in(&root, "checkout-a", "dead-server ");
        assert!(
            report.stacks.is_empty(),
            "a trailing space is a different host"
        );
        assert!(report.failures.is_empty());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }

    /// A stacks root that never existed is the fresh-machine answer, and a
    /// plain file in it is not a declaration: neither may be counted as a
    /// retirement, because the receipt is what tells the user the wedge is
    /// over.
    #[test]
    fn litter_in_the_stacks_root_is_neither_retired_nor_a_failure() {
        let (_tmp, root) = stacks_root();
        declare_in(&root, "checkout-a", "dead-server", "api", true);
        std::fs::write(root.join("not-a-directory"), b"x").unwrap();
        std::fs::create_dir(root.join("no-desired-inside")).unwrap();

        let report = retire_declarations_in(&root, "checkout-a", "dead-server");
        assert_eq!(report.stacks.len(), 1);
        assert!(report.failures.is_empty());
        assert!(root.join("not-a-directory").is_file());
        assert!(root.join("no-desired-inside").is_dir());

        let missing = retire_declarations_in(&root.join("never-made"), "checkout-a", "dead-server");
        assert_eq!(missing, Retired::default());
    }

    /// A declaration `up` would write for a real project directory, so
    /// every OTHER check in `rebuild_for_stack` passes and a refusal can
    /// only come from the field under test.
    fn declaration_for(project_dir: &Path, destination: &str) -> Desired {
        std::fs::write(project_dir.join("compose.yaml"), "services: {}\n").unwrap();
        let inv = Invocation::capture_in(project_dir, &[], BTreeMap::new()).unwrap();
        let project = crate::config::Project {
            config_home: project_dir.to_path_buf(),
            anchor: project_dir.to_path_buf(),
            name: "stack".into(),
            identity: "api".into(),
            stack_pin: None,
            config: crate::config::Config::default(),
            workspace_key: crate::config::WorkspaceKey::from_namespace(
                "test-client",
                inv.workspace_identity_path(),
            )
            .unwrap(),
            inv,
        };
        Desired::of(&project, destination, "api", true)
    }

    /// The declaration is untrusted input a process holding SSH keys acts
    /// on. A destination that could not have come through `config` is
    /// refused where the remedy can name the file it came from. The
    /// accepting control comes first: a first draft of this test used a
    /// made-up workspace id, so the refusal fired from the wrong arm and
    /// the destination check was never exercised at all.
    #[test]
    fn a_declaration_whose_destination_could_never_reach_ssh_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().canonicalize().unwrap();

        let fine = declaration_for(&project, "deploy@my-server");
        fine.rebuild_for_stack(&fine.stack_id())
            .expect("an ordinary destination rebuilds");

        let hostile = declaration_for(&project, "-oProxyCommand=touch /tmp/pwned");
        let err = hostile
            .rebuild_for_stack(&hostile.stack_id())
            .expect_err("a destination beginning with - must never reach OpenSSH");
        let said = crate::ui::flatten(&err);
        assert!(
            said.contains("does not match its stack or sync workspace"),
            "{said}"
        );
    }

    /// A rollback that recreates a declaration somebody retired underneath
    /// the command puts the wedge straight back, under a receipt that said
    /// it was over. The accepting half: a declaration still there is put
    /// back whole, which is the rollback's entire job.
    #[test]
    fn a_rollback_never_resurrects_a_declaration_that_was_retired_underneath_it() {
        let previous = Desired {
            schema: SCHEMA,
            live: true,
            workspace_id: "checkout-a".into(),
            workspace_namespace: "client-a".into(),
            destination: "server".into(),
            identity: "api".into(),
            cwd: "/checkout/a".into(),
            argv_globals: vec!["-f".into(), "a.yaml".into()],
            compose_env: BTreeMap::new(),
            updated_unix: 1,
        };
        let id = format!("rollback-{}", std::process::id());
        let mut attempted = previous.clone();
        attempted.live = false;
        write_desired(&id, &attempted).unwrap();

        restore(&id, &previous);
        assert!(
            read_desired(&id).is_some_and(|d| d.live),
            "a declaration still on disk is put back whole"
        );

        std::fs::remove_dir_all(stack_dir_path(&id).unwrap()).unwrap();
        restore(&id, &previous);
        assert!(
            read_desired(&id).is_none() && !stack_dir_path(&id).unwrap().exists(),
            "a retired declaration stays retired, and its directory is not recreated"
        );
    }

    /// One Docker project may already be live from checkout A when an
    /// `up` from checkout B fails before changing it. Restoring only the
    /// old `live` bit pins the surviving stack to B's files; restoring
    /// the whole declaration keeps its original workspace and argv.
    #[test]
    fn rollback_restores_the_whole_previous_stack_declaration() {
        let (_tmp, dir) = sandbox();
        let old = Desired {
            schema: SCHEMA,
            live: true,
            workspace_id: "checkout-a".into(),
            workspace_namespace: "client-a".into(),
            destination: "server".into(),
            identity: "api".into(),
            cwd: "/checkout/a".into(),
            argv_globals: vec!["-f".into(), "a.yaml".into()],
            compose_env: BTreeMap::new(),
            updated_unix: 1,
        };
        let new = Desired {
            workspace_id: "checkout-b".into(),
            cwd: "/checkout/b".into(),
            argv_globals: vec!["-f".into(), "b.yaml".into()],
            updated_unix: 2,
            ..old.clone()
        };
        let path = dir.join(DESIRED);
        write_atomic(&path, &old).unwrap();

        let previous: Desired = read(&path).unwrap();
        write_atomic(&path, &new).unwrap();
        restore_in(&dir, &previous);
        assert_eq!(read::<Desired>(&path), Some(old));
    }

    #[test]
    fn a_corrupt_file_is_set_aside_and_the_reader_survives() {
        // The failure this prevents: a truncated intent file (a full
        // disk, a kill mid-write) making every command exit 1. The
        // answer must be the one a fresh machine gives — "there is no
        // intent here" — with the evidence kept rather than overwritten.
        let (_tmp, dir) = sandbox();
        let path = dir.join(DESIRED);
        std::fs::write(&path, b"{\"schema\": 1, \"cwd\"").unwrap();

        assert!(
            read::<Desired>(&path).is_none(),
            "a corrupt file must read as absent"
        );
        assert!(
            dir.join("desired.bad").is_file(),
            "the evidence must be kept, not deleted"
        );
        assert!(
            !path.is_file(),
            "the corpse must not stay in the way of the next write"
        );
    }

    #[test]
    fn a_file_from_a_newer_ulak_is_left_alone() {
        // Not corruption: an older binary must not eat a newer file, and
        // must not pretend to understand it either.
        let (_tmp, dir) = sandbox();
        let path = dir.join(DESIRED);
        std::fs::write(&path, br#"{"schema": 99, "cwd": "/x", "live": true}"#).unwrap();

        assert!(read::<Desired>(&path).is_none());
        assert!(
            path.is_file(),
            "a newer file must be left exactly where it is"
        );
    }

    #[test]
    fn unknown_fields_do_not_break_an_older_reader() {
        let (_tmp, dir) = sandbox();
        let path = dir.join(DESIRED);
        std::fs::write(
            &path,
            br#"{"schema": 2, "cwd": "/x", "live": true, "invented_later": [1,2,3]}"#,
        )
        .unwrap();

        let d: Desired = read(&path).expect("a field we do not know must be ignored");
        assert!(d.live);
        assert_eq!(d.cwd, "/x");
    }

    #[test]
    fn files_are_private_and_land_whole() {
        let (_tmp, dir) = sandbox();
        let path = dir.join(DESIRED);
        let d = Desired {
            schema: SCHEMA,
            live: true,
            workspace_id: "w".into(),
            workspace_namespace: "client-a".into(),
            destination: "server".into(),
            identity: "demo".into(),
            cwd: "/x".into(),
            argv_globals: vec!["-f".into(), "a.yaml".into()],
            compose_env: BTreeMap::new(),
            updated_unix: 7,
        };
        write_atomic(&path, &d).unwrap();

        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the intent names the user's paths — it is nobody else's business"
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // No temp left behind to be mistaken for state later.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "rename left a temp behind: {leftovers:?}"
        );

        let back: Desired = read(&path).unwrap();
        assert!(back.live);
        assert_eq!(back.argv_globals, vec!["-f", "a.yaml"]);
    }

    #[test]
    fn an_invocation_survives_the_round_trip_intact() {
        // The whole reason desired.json is a serialized INVOCATION and
        // not a pile of derived facts: the service must rebuild exactly
        // what the CLI saw. If this drifts, the service and the CLI
        // disagree about which project they mean — the prototype's bug
        // that made `down` report success while stopping nothing, except
        // now with something running 24/7 behind it.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let proj = root.join("stack");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("compose.base.yaml"), "services: {}\n").unwrap();
        std::fs::write(root.join("shared.env"), "K=V\n").unwrap();

        let argv: Vec<String> = [
            "-f",
            "compose.base.yaml",
            "-p",
            "demo",
            "--profile",
            "dev",
            "--env-file",
            "../shared.env",
            "up",
            "-d",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut env = BTreeMap::new();
        env.insert("COMPOSE_BAKE".to_string(), "true".to_string());
        let original = Invocation::capture_in(&proj, &argv, env).unwrap();
        let project = crate::config::Project {
            config_home: proj.clone(),
            anchor: proj.clone(),
            name: "stack".into(),
            identity: "demo".into(),
            stack_pin: None,
            config: crate::config::Config::default(),
            workspace_key: crate::config::WorkspaceKey::from_namespace(
                "test-client",
                original.workspace_identity_path(),
            )
            .unwrap(),
            inv: original.clone(),
        };

        // Through the file and back.
        let path = stack_dir_in(&root.join("stacks"), "m")
            .unwrap()
            .join(DESIRED);
        let stored = Desired::of(&project, "my-server", "demo", true);
        write_atomic(&path, &stored).unwrap();
        let rebuilt: Desired = read(&path).unwrap();
        assert!(rebuilt.live);
        let inv = rebuilt.rebuild().unwrap();

        assert_eq!(inv.cwd, original.cwd);
        assert_eq!(inv.compose_files, original.compose_files);
        assert_eq!(inv.project_dir, original.project_dir);
        assert_eq!(inv.project_name, original.project_name);
        assert_eq!(inv.env_files, original.env_files);
        assert_eq!(inv.profiles, original.profiles);
        assert_eq!(inv.compose_env, original.compose_env);
        // Same workspace, same compose project — the two identities the
        // service and the CLI must never disagree about.
        assert_eq!(
            inv.workspace_identity_path(),
            original.workspace_identity_path()
        );
        assert_eq!(inv.compose_identity(), original.compose_identity());
        assert_eq!(rebuilt.workspace_id, project.workspace_id());
        assert_eq!(rebuilt.workspace_namespace, "test-client");
        assert_eq!(rebuilt.destination, "my-server");
        assert_eq!(rebuilt.identity, "demo");
        assert!(rebuilt.rebuild_for_stack(&rebuilt.stack_id()).is_ok());
        assert!(
            rebuilt.rebuild_for_stack("a-different-stack-id").is_err(),
            "moving an intent file must not redirect a service holding SSH keys"
        );
        let mut unsafe_namespace = rebuilt.clone();
        unsafe_namespace.workspace_namespace = "../elsewhere".into();
        let err = unsafe_namespace
            .rebuild_for_stack(&unsafe_namespace.stack_id())
            .unwrap_err();
        assert!(
            err.to_string().contains("does not match"),
            "an unsafe path segment must be treated as a corrupt declaration: {err}"
        );
        // The SUBCOMMAND is deliberately not carried: the intent is
        // "this stack should be live", never "replay `up -d` at me".
        assert!(
            inv.args.is_empty(),
            "the file must not carry a command to run"
        );
    }

    /// Docker names a stack by daemon and Compose project, not by the
    /// directory from which the command happened to be typed. This is
    /// the separation that lets one checkout run `-p a` and `-p b`
    /// without one declaration or tunnel replacing the other.
    #[test]
    fn stack_ids_follow_dockers_daemon_and_project_namespace() {
        assert_ne!(stack_id("server", "a"), stack_id("server", "b"));
        assert_ne!(stack_id("server-a", "demo"), stack_id("server-b", "demo"));
        assert_eq!(stack_id("server", "demo"), stack_id("server", "demo"));
    }

    #[test]
    fn the_service_half_of_the_contract_round_trips_too() {
        let (_tmp, dir) = sandbox();
        let path = dir.join(STATUS);
        let st = Status {
            schema: SCHEMA,
            updated_unix: 100,
            connection: "up".into(),
            since_unix: 90,
            tunnels: vec![
                TunnelState {
                    port: 8080,
                    service: "web".into(),
                    open: true,
                    service_running: Some(true),
                },
                TunnelState {
                    port: 5432,
                    service: "db".into(),
                    open: false,
                    service_running: Some(false),
                },
            ],
            last_sync_unix: 99,
            pulled_total: 3,
            note: Some("5432 is taken on this machine".into()),
        };
        write_atomic(&path, &st).unwrap();

        let back: Status = read(&path).unwrap();
        assert_eq!(back.connection, "up");
        assert_eq!(back.tunnels.len(), 2);
        assert!(!back.tunnels[1].open);
        assert_eq!(back.tunnels[0].service_running, Some(true));
        assert_eq!(back.tunnels[1].service_running, Some(false));
        assert_eq!(back.pulled_total, 3);
        assert_eq!(back.note.as_deref(), Some("5432 is taken on this machine"));
    }

    /// A status written by an OLDER service says nothing about which
    /// services are alive, and the reader must know the difference
    /// between "not known" and "known absent": defaulting the missing
    /// field to `false` would make every port of a mixed-version machine
    /// read "no container" while the stack runs fine.
    #[test]
    fn a_tunnel_report_without_the_liveness_field_reads_as_unknown() {
        let (_tmp, dir) = sandbox();
        let path = dir.join(STATUS);
        std::fs::write(
            &path,
            br#"{"schema": 3, "tunnels": [{"port": 8080, "service": "web", "open": true}]}"#,
        )
        .unwrap();
        let st: Status = read(&path).expect("an older writer's file must parse");
        assert_eq!(st.tunnels[0].service_running, None);
    }
}
