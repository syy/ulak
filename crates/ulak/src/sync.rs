//! The sync engine: one selection policy, bulk rsync and small-file streaming.
//!
//! Profile: one rsync from the ANCHOR with --delay-updates
//! --partial-dir. Bulk pushes and existing-file pulls use the literal
//! excludes from walk.rs; creation pulls select unclaimed leaves through
//! selection.rs. Protect and footprint rules apply to both pull passes.
//!
//! Rule order is the whole design (rsync takes the FIRST match):
//!   0. partials  — rsync's own half-written blobs, ours to exclude
//!   1. held back — paths the pull must not carry home (see pull_back)
//!   2. protect   — server-owned data: never sent, never deleted
//!   3. ignores   — [sync] exclude/.dockerignore, expanded to literal paths
//!   4. footprint — the only paths allowed to travel, plus their parents
//!   5. `- *`     — everything else, which rsync then never descends into
//!
//! Rule 5 is what makes a small stack inside a multi-gigabyte monorepo
//! cost a 3 MB scan. Rule 3 beating rule 4 is deliberate: pushing an
//! excluded `./data` over a live database is the worse of the two
//! failures, and doctor reports any footprint path the ignores swallowed.
//!
//! The workspace runs BOTH ways, because a local bind mount does: your
//! edits go up, and whatever the container writes comes back down. The
//! one thing that is not symmetric is deletion — see ledger.rs. rsync
//! therefore never carries `--delete` here; the doomed set is computed
//! locally from the ledger and removed by an explicit, visible `rm`
//! (and, for the directories it leaves behind, an explicit `rmdir`).
//!
//! One sync sees ONE footprint, and several of them share a workspace:
//! `docker build .` and `docker run -v ./app:/app` hash the same root.
//! So a sync only ever tells the ledger two things — what it put there,
//! and what the server confirmed it lost — and never that the rest of the
//! workspace stopped existing, which it has no way of knowing.
//!
//! It says those two things to ONE DESTINATION'S ledger. Every receipt
//! this engine writes — the ever-synced mark, the pending-deletion
//! counter and the ledger itself — is keyed by workspace
//! AND ssh destination (`config::WorkspaceStateKey`), because the same
//! checkout syncing to two servers has two independent remote copies:
//! server A confirming a deletion says nothing about the file still
//! sitting on server B, and retiring B's claim on A's receipt is what
//! makes B's next pull carry that file home as if the server had made it.
//! The `WorkspaceLock` stays checkout-wide on purpose — both destinations
//! still pull into the same local tree, and that race is a different one.
//! `ledger.rs` owns the retirement rules themselves.
//!
//! The two directions share the profile, and the way DOWN departs from
//! it in one measured way: it is TWO passes, because a remote file the
//! ledger claims and one it does not have opposite rights here — the
//! first may be updated but never created, the second may be created
//! freely. `--existing` and `--ignore-existing` are how rsync is told
//! which is which, and rsync answers the existence question at the
//! instant of transfer, which is the only instant whose answer is true.
//! See `pull_back` for the deletions that were lost before it did.
//! The updating pass also carries `--update`, so an edit in flight
//! always beats the server's copy; the creating one has nothing to
//! overwrite and does not need it. Both carry `--prune-empty-dirs`, so
//! the pull can never plant a directory that ulak — which deletes
//! nothing local — could not take back.
//!
//! Do not give the existing-file pass the entire tree via --files-from.
//! rsync 3.5.0 builds an implied include list for a remote sender and
//! checks each received name against it (flist.c: recv_file_entry).
//! Reproduce with tens of thousands of unchanged files: rsync -rltpz
//! --from0 --no-recursive --files-from=- --update --existing
//! --prune-empty-dirs host:tree/ tree/, feeding every leaf on stdin.
//! Sampling the receiver lands in check_filter/rule_matches; ordinary
//! recursive traversal avoids this all-pairs check. --existing supplies
//! the receiving-side safety, so enumerating local leaves adds cost
//! without adding that guarantee.
//!
//! Warm sessions retain the selected local paths and their inode, size,
//! mode, nanosecond mtime and ctime. Watcher notices carry paths; unchanged
//! observations cost no transfer. Small regular files use the persistent
//! stream.rs channel — replaces of known leaves, and for bounded structural
//! change (new or removed leaves under known directories, which walk.rs
//! rescans alone and `rescanned_plan` splices into the last full plan)
//! creates and guarded unlinks in the same exchange, with the ledger
//! claiming before the exchange and retiring on its receipt. Large
//! or unstable files, unknown ancestors and failed batches use the
//! checksum-selected rsync path and the removal script. Only an
//! acknowledged batch updates the pre-transfer observations. One exchange
//! is one round trip; stream.rs owns why that beats an rsync.
//! Directory or rule changes, restarts, ledger writes and missed events require a full
//! reconcile. With a healthy remote index, the idle deadline audits local
//! metadata on a background thread instead of blocking saves behind a full
//! rsync. observer.rs audits remote metadata independently too. Missing
//! capabilities and invalidated indexes retain full reconciliation.
//! Content ancestors live in baseline.rs, independently of the warm index.
//! Full pushes hold differing remote edits, pulls hold differing local edits;
//! divergence with no agreed content is held by a pull and follows
//! `[sync] no_history` on a push (`baseline::may_push`). Warm streaming checks
//! the expected remote ancestor before replacement. Metadata is a scheduling
//! hint, never permission to overwrite a different remote version.
//! Snapshots precede transfers:
//! a save made during one remains different and must be sent again.
//! A missed watcher event or restart leaves such a save to the next full
//! survey, which compares content rather than seconds; no push window or
//! catch-up pass is kept, because a content survey needs neither.
//!
//! A full reconcile is ONE survey: one walk, and one fingerprint of every
//! selected leaf on each end, read through the digest caches hashcache.rs
//! governs. The survey is also the transfer selection. rsync is handed
//! exactly the differing names — under `--checksum`, so a same-size save
//! in the same second cannot hide — plus the synced directories missing
//! or re-moded on the server, and traverses the tree only when more than
//! `EXACT_PASS_LIMIT` leaves differ. A restart used to hash both ends
//! twice and run two `--checksum` traversals; one survey now feeds push
//! and pull. The push folds what it confirmed back into the survey, so
//! the pull reads nothing a second time.
//! The server's file inventory for the creating pull is listed while this
//! machine hashes; the server's fingerprints come after the local hashing,
//! because they are asked for by difference against its digests
//! (`Survey::take`). Synced directories are surveyed too, so a push names
//! only the ones missing or re-moded; and the previous transfer leases are
//! retired in the round trip that prepares the workspace. The differing
//! leaves themselves, when small and under existing directories, go over
//! the persistent channel too — creates and guarded replaces up, reads
//! landed under the survey's stamps down — and the channel then stays
//! open for the warm session. A quiet restart is therefore one survey and
//! no rsync; a busy one is one exchange each way.
//! A PUSH batch the helper refuses applied nothing, and what happens next
//! is decided by the reason, never by handing the same selection to rsync:
//! `RSYNC_CAN` names the limits of the channel that rsync covers, every
//! other reason is the server having moved since the survey and holds the
//! leaf as a conflict (`split_refusal`). A refused pull read falls back to
//! the `--existing` / `--ignore-existing` passes by design: a read writes
//! nothing on the server, and those passes decide at the instant of
//! writing, as the channel's landing does. The ledger claims before the
//! exchange, and `fresh_claims` says which of those claims to retire when
//! a refusal proves nothing was applied — a claim on a server-born file is
//! a licence to delete it later. A LOST receipt proves nothing either
//! way, so the claims stand beside the sent content, recorded as
//! unconfirmed before every exchange on both paths
//! (`unconfirmed_sends`, `baseline::State::unconfirmed`); before a claimed row is
//! deleted, `never_landed` compares that record with the server's copy
//! and retires the claim instead when the send never arrived. A replace
//! of a claimed leaf is recorded only once its receipt is lost, and only
//! when the name has agreed content (`attempted`): an acknowledged save
//! writes no extra state, and a claim with no agreed content stays the
//! ledger's word rather than turning into a send that never landed.
//!
//! A claimed row deleted here whose server copy changed since the last
//! agreed content (`moved_on_the_server`) is not deleted and does not stop
//! the sync: the server's edit wins. A server copy ulak itself sent
//! without a receipt is not such a change. `retire_moved` marks the file
//! as returning (`baseline::State::returning`), retires the claim and
//! names the file, and every other deletion goes ahead. Whichever pull
//! lands the file next claims it again and ends the return
//! (`claim_returned`): this sync's, the `pull_back` after a passthrough,
//! build or run, or the service's `pull_changes`. A second local deletion
//! is then judged like any other. The mark is durable because that pull
//! is often a later command's. Handed from push to pull in memory it
//! reached only `ulak sync`, and under `ulak docker compose` the file came
//! home unclaimed, so each later deletion of it was silently undone. Every
//! claim a push makes ends a return too, and so does a pull that sees the
//! server without the file (`returns_the_server_lost`);
//! `baseline::State::returning` lists what else does.
//!
//! The judgements read the server and delete nothing, so `push` makes
//! them before the budget counts the rows (`judge_doomed`). It makes them
//! again right before the removal when a person confirmed an over-budget
//! list: the prompt has no time limit, and the removal script's `rm -f`
//! checks nothing. One request reads every doomed row (`read_doomed`), and
//! baseline.py answers each with its copy or with why it cannot be judged
//! (`baseline::Copies`), so no row stops the others. A row whose path the
//! server restructured, with a parent that is no longer a real directory
//! or a leaf that is neither a regular file nor a symlink, stays on the
//! server, untracked from then on (`retire_restructured`): ulak does not
//! delete through such a path. A row that cannot be read right now, such
//! as a file being written or one the SSH user may not read, keeps its
//! claim and is judged again by the next sync (`hold_unreadable`); the
//! claim keeps it out of the pull's creating pass. Neither stops the sync,
//! and no directory row above either kind is removed that round
//! (`spare_dirs_above`). The removal script keeps any row with a symlink
//! on its way as well (`removal_script`). A read the helper never answers
//! is an SSH failure like any other. A dry run makes the same split
//! without retiring anything (`preview_deletions`). A `[sync] protect`
//! path is never doomed, even when it was claimed before it was protected
//! (`doomed_files`).
//! A removal the server confirms leaves no agreed content behind
//! (`forget_removed`). The warm path's rsync fallback reads its doomed
//! rows after its upload, right before the removal, and steps aside to the
//! full reconcile for a row the server changed, whether that read or the
//! helper's refused unlink finds it.
//!
//! Deletion budget: the doomed set is known exactly and up front, so
//! within budget it is applied, and over budget it is either confirmed
//! (a terminal) or reported and left pending (the background daemon).
//! Human retry commands are rendered from the same `Project` the transfer
//! used. The first explicit sync can fail before Desired exists; dropping a
//! nonstandard `-f`, or dropping `--dry-run`, would make its way out either
//! unusable or unexpectedly mutating.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result};

use crate::config::Project;
use crate::footprint::Footprint;
use crate::ledger;
use crate::ssh::{Ssh, sh_quote};
use crate::ui::{self, fail};
use crate::walk;

pub struct SyncOptions {
    pub dry_run: bool,
    pub max_delete_override: Option<crate::config::DeleteBudget>,
    pub quiet: bool,
    pub over_budget: OverBudget,
}

/// What to do when a sync wants more deletions than the budget allows.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum OverBudget {
    /// Show the list, ask, and fail with the ways forward if refused.
    #[default]
    Ask,
    /// Push the content, leave the deletions, report the count and carry
    /// on. The service always takes this door: it has no terminal to ask
    /// in, and a branch switch must not make a deletion budget the
    /// reason a workspace stops being maintained.
    Report,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub pushed: usize,
    pub deleted: usize,
    /// Files that came BACK from the server (container output).
    pub pulled: usize,
    /// Server-side deletions that were listed but NOT applied. Anything
    /// above zero means the workspace carries files the project no longer
    /// has — recorded so status and doctor can say so instead of letting
    /// it drift quietly.
    pub pending_deletions: usize,
}

/// A successfully reconciled selection, held only by the service that
/// observed it. No cache on disk is trusted after a restart. A foreground
/// writer changes the ledger stamp and invalidates the selection too.
#[derive(Default)]
pub struct Session {
    signature: String,
    observed: std::collections::BTreeMap<PathBuf, crate::selection::Stamp>,
    ledger: Option<crate::selection::Stamp>,
    pending_deletions: usize,
    children: std::collections::BTreeMap<PathBuf, std::collections::BTreeSet<std::ffi::OsString>>,
    owned: std::collections::BTreeSet<PathBuf>,
    stream: Option<crate::stream::Client>,
    stream_unavailable: bool,
    watcher: Option<crate::observer::Watcher>,
    /// The last full walk's answer, with every directory rescanned since
    /// spliced in (`rescanned_plan`).
    plan: walk::WalkReport,
    audit: Option<std::thread::JoinHandle<std::collections::BTreeSet<PathBuf>>>,
    deferred: std::collections::BTreeSet<PathBuf>,
    common: Option<crate::baseline::State>,
}

impl Session {
    pub fn indexed(&self) -> bool {
        self.watcher.as_ref().is_some_and(|w| w.healthy())
    }

    pub fn pending(&self) -> bool {
        self.watcher.as_ref().is_some_and(|w| w.pending())
            || self.audit.as_ref().is_some_and(|a| a.is_finished())
            || !self.deferred.is_empty()
    }

    /// Audit the known selection without holding up the transfer worker.
    /// Directory stamps detect structural changes; those still use the owner
    /// walk. The snapshot precedes the scan, so racing saves remain dirty.
    pub fn audit(&mut self) {
        if self.audit.is_none() {
            let observed = self.observed.clone();
            let children = self.children.clone();
            self.audit = Some(std::thread::spawn(move || {
                observed
                    .into_iter()
                    .filter_map(|(path, previous)| {
                        if children.get(&path).is_some_and(|before| {
                            directory_children(&path).as_ref() == Some(before)
                        }) {
                            return None;
                        }
                        (crate::selection::Stamp::read(&path).as_ref() != Some(&previous))
                            .then_some(path)
                    })
                    .collect()
            }));
        }
    }
}

/// The ledger file's stamp. A session's selection holds only while the
/// ledger is the one the session itself last wrote or read; a foreground
/// writer changes it.
fn ledger_stamp(state: &crate::config::WorkspaceStateKey) -> Option<crate::selection::Stamp> {
    crate::intent::workspace_state_dir_path(state)
        .and_then(|p| crate::selection::Stamp::read(&p.join("ledger")))
}

/// Accelerate known leaves and bounded structural batches. Capturing BEFORE the transfer is
/// essential: observing after it would acknowledge an edit that arrived
/// while rsync was reading the previous version. Such edits retain a
/// different stamp and the queued watcher notice sends them next.
pub fn reconcile_changes(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    opts: &SyncOptions,
    session: &mut Session,
    paths: Option<&std::collections::BTreeSet<PathBuf>>,
) -> Result<SyncReport> {
    if opts.dry_run {
        *session = Session::default();
        return reconcile_once(project, fp, ssh, opts);
    }
    let signature = format!(
        "{}:{:?}:{}",
        ssh.dest,
        project.config.sync,
        serde_json::to_string(fp)?
    );
    let state = project.state_key(&ssh.dest);
    let mut paths = paths.cloned();
    if let Some(paths) = &mut paths {
        paths.append(&mut session.deferred);
    }
    if session.audit.as_ref().is_some_and(|a| a.is_finished())
        && let Some(audit) = session.audit.take()
    {
        match (audit.join(), &mut paths) {
            (Ok(changed), Some(paths)) => paths.extend(changed),
            (Err(_), _) => paths = None,
            _ => {}
        }
    }
    if session.watcher.as_ref().is_some_and(|w| !w.healthy()) {
        paths = None;
    }
    let candidates = paths
        .as_ref()
        .filter(|_| session.signature == signature && session.ledger == ledger_stamp(&state))
        .and_then(|paths| changed_leaves(&fp.anchor, session, paths));
    if let Some(changed) = candidates {
        if changed.is_empty() {
            return pull_changes(project, fp, ssh, opts, session);
        }
        let names: Vec<String> = changed
            .keys()
            .filter_map(|p| crate::invocation::anchor_rel(&fp.anchor, p))
            .collect();
        let deadline = crate::proc::Budget::new(crate::proc::RECONCILE);
        let result = transfer_changed(project, fp, ssh, session, &changed).and_then(|events| {
            events.map(Ok).unwrap_or_else(|| {
                let rsync = local_rsync()?;
                let selected = names
                    .iter()
                    .map(|p| p.as_bytes().to_vec())
                    .collect::<Vec<_>>();
                let Some(mut common) = session.common.take() else {
                    return Err(content_conflict("the common-content state is unavailable"));
                };
                let local = crate::baseline::local(&fp.anchor, &selected)?;
                let remote =
                    crate::baseline::remote(ssh, &project.remote_dir(), &selected, &deadline)?;
                if local.iter().any(|(p, l)| {
                    !crate::baseline::may_push(
                        project.config.sync.no_history,
                        common.get(p),
                        l.as_ref(),
                        remote.get(p).and_then(Option::as_ref),
                    )
                }) {
                    return Err(content_conflict("a selected file changed on the server"));
                }
                say_what_no_history_sends(local.keys(), &common.entries, &local, &remote);
                let events = offer_again(&rsync, project, ssh, fp, &names, &deadline)?.or_fail()?;
                remember_common(&mut common, ssh, project, fp, &selected, &deadline)?;
                session.common = Some(common);
                Ok(events)
            })
        });
        match result {
            Ok(events) => {
                session.observed.extend(changed);
                let mut report = render_events(&events, opts.quiet);
                report.pending_deletions = session.pending_deletions;
                // Continuous typing must not starve unrelated server output.
                // At most one bounded reverse batch follows each local batch.
                let reverse = pull_changes(project, fp, ssh, opts, session)?;
                report.pulled += reverse.pulled;
                return Ok(report);
            }
            Err(e) => {
                *session = Session::default();
                return Err(e);
            }
        }
    }
    if session.signature == signature
        && session.ledger == ledger_stamp(&state)
        && let Some(paths) = paths.as_ref()
        && let Some(report) = reconcile_structure(project, fp, ssh, opts, session, paths)?
    {
        return Ok(report);
    }
    *session = Session::default();
    let plan = checked_plan(project, fp)?;
    let (roots, pruned) = inventory_scope(&project.sync_rules().protect, fp, &plan.excludes);
    // Subscribe before reconciling: output written during the full transfer
    // must survive into the next turn. The helper's registration scan runs
    // while the survey does — both read-only — and is waited for before
    // the transfer writes. This read-only helper has no writer lease, so
    // fencing the full push cannot revoke its observation window.
    let pending = crate::observer::Watcher::start(
        ssh,
        &project.remote_dir(),
        &serde_json::json!({"roots": roots, "pruned": pruned}),
    )
    .map_err(|error| ui::dim(&ui::flatten(&error)))
    .ok();
    let deadline = crate::proc::Budget::new(crate::proc::RECONCILE);
    let started = std::time::SystemTime::now();
    let (mut survey, doomed, doomed_dirs) = prepare(project, fp, ssh, plan, true, &deadline)?;
    // The pre-transfer observations are the stamps the survey read the
    // fingerprints under; directories are stamped here.
    let mut observed: std::collections::BTreeMap<_, _> = survey
        .cache
        .stamps()
        .map(|(name, stamp)| (fp.anchor.join(name), stamp.clone()))
        .collect();
    for dir in survey
        .plan
        .kept_dirs
        .iter()
        .filter_map(|p| std::str::from_utf8(p).ok())
        .map(|p| fp.anchor.join(p))
        .chain(fp.watch_dirs())
    {
        if let Some(stamp) = crate::selection::Stamp::read(&dir) {
            observed.insert(dir, stamp);
        }
    }
    let children = observed
        .keys()
        .filter(|p| p.is_dir())
        .filter_map(|p| directory_children(p).map(|c| (p.clone(), c)))
        .collect();
    let watcher = pending.and_then(|pending| {
        pending
            .ready(&crate::proc::Budget::new(crate::proc::STREAM))
            .map_err(|error| ui::dim(&ui::flatten(&error)))
            .ok()
    });
    // The channel the transfer opens stays open for the session.
    let mut channel = None;
    let report = transfer(
        project,
        fp,
        ssh,
        opts,
        &mut survey,
        doomed,
        doomed_dirs,
        started,
        &deadline,
        &mut channel,
    )?;
    survey.finish()?;
    let owned = ledger::synced_files(&state)
        .iter()
        .filter_map(|p| std::str::from_utf8(p).ok())
        .map(|p| fp.anchor.join(p))
        .collect();
    *session = Session {
        signature,
        observed,
        ledger: ledger_stamp(&state),
        pending_deletions: report.pending_deletions,
        children,
        owned,
        stream: channel,
        stream_unavailable: false,
        watcher,
        plan: survey.plan,
        audit: None,
        deferred: std::collections::BTreeSet::new(),
        common: Some(survey.common),
    };
    Ok(report)
}

/// Reuse the owner walk for selection, but send only its changed leaves —
/// over the persistent channel when every item fits it, else by selected
/// rsync and the removal script. This bounds files-from filters and keeps
/// directory creation/removal and rule changes on the conservative full
/// path. New files still acquire ledger claims before the transfer can
/// fail, so a partial transfer leaves nothing unclaimed. A doomed row the
/// server changed since the last agreed content sends the rest of the
/// batch to the full path too, which keeps the server's copy
/// (`retire_moved`); the rsync path reads the doomed rows for that only
/// after its upload, right before the removal.
fn reconcile_structure(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    opts: &SyncOptions,
    session: &mut Session,
    paths: &std::collections::BTreeSet<PathBuf>,
) -> Result<Option<SyncReport>> {
    if session.common.is_none()
        || paths.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".dockerignore") || n == "ulak.toml")
        })
    {
        return Ok(None);
    }
    let Some((plan, scopes)) = rescanned_plan(&project.sync_rules(), fp, &session.plan, paths)?
    else {
        return Ok(None);
    };
    let state = project.state_key(&ssh.dest);
    let mut doomed = doomed_files(project, &state, &fp.anchor, &plan.kept);
    if !doomed_directories(project, &state, &fp.anchor).is_empty()
        || !opts
            .max_delete_override
            .unwrap_or(project.config.sync.max_delete)
            .allows(doomed.len())
    {
        return Ok(None);
    }
    let within = |path: &std::path::Path| scopes.iter().any(|s| path.starts_with(s));
    // Fresh observations for the rescanned directories only; every other
    // observation stands, and nothing replaces the session's until the
    // transfer has succeeded — a refreshed stamp before a failed transfer
    // would make the save look already sent.
    let mut observed = std::collections::BTreeMap::new();
    let mut selected = Vec::new();
    for raw in &plan.kept {
        let name = std::str::from_utf8(raw)?;
        let path = fp.anchor.join(name);
        if !within(&path) {
            continue;
        }
        let Some(stamp) = crate::selection::Stamp::read(&path) else {
            return Ok(None);
        };
        if session.observed.get(&path) != Some(&stamp) {
            if !std::fs::symlink_metadata(&path)?.is_file()
                || !path
                    .parent()
                    .is_some_and(|p| session.children.contains_key(p))
            {
                return Ok(None);
            }
            selected.push(raw.clone());
        }
        observed.insert(path, stamp);
    }
    if selected.len() + doomed.len() > 128 {
        return Ok(None);
    }
    // Capture the directory inventory before network work as well. A new
    // sibling arriving during a transfer must remain visible to the audit.
    let mut children = std::collections::BTreeMap::new();
    for path in plan
        .kept_dirs
        .iter()
        .filter_map(|p| std::str::from_utf8(p).ok())
        .map(|p| fp.anchor.join(p))
        .filter(|p| within(p))
        .chain(fp.watch_dirs())
    {
        let Some(stamp) = crate::selection::Stamp::read(&path) else {
            return Ok(None);
        };
        let Some(names) = directory_children(&path) else {
            return Ok(None);
        };
        observed.insert(path.clone(), stamp);
        children.insert(path, names);
    }
    let deadline = crate::proc::Budget::new(crate::proc::RECONCILE);
    let Some(mut common) = session.common.take() else {
        return Ok(None);
    };
    let mut report = SyncReport::default();
    let streamed = match stream_structure(
        project,
        fp,
        ssh,
        session,
        &mut common,
        &state,
        &selected,
        &doomed,
        &observed,
        opts.quiet,
    ) {
        Ok(Structure::Applied(streamed)) => Some(streamed),
        Ok(Structure::Rsync) => None,
        Ok(Structure::Full) => {
            session.common = Some(common);
            return Ok(None);
        }
        Err(e) => {
            session.common = Some(common);
            return Err(e);
        }
    };
    let rsync_needed = streamed.is_none();
    if let Some(streamed) = streamed {
        report = streamed;
    } else {
        let local = crate::baseline::local(&fp.anchor, &selected)?;
        // A read that fails decides nothing here, and nothing has been
        // sent yet: the full reconcile reads again.
        let Ok(remote) = crate::baseline::remote(ssh, &project.remote_dir(), &selected, &deadline)
        else {
            session.common = Some(common);
            return Ok(None);
        };
        if local.iter().any(|(p, l)| {
            !crate::baseline::may_push(
                project.config.sync.no_history,
                common.get(p),
                l.as_ref(),
                remote[p].as_ref(),
            )
        }) {
            return Err(content_conflict(
                "a changed path has different server content",
            ));
        }
        say_what_no_history_sends(local.keys(), &common.entries, &local, &remote);
    }
    if rsync_needed && !selected.is_empty() {
        let rsync = local_rsync()?;
        let pass = rsync_pass_dir(
            &rsync,
            project,
            fp,
            ssh,
            &plan.excludes,
            &[],
            &["--checksum"],
            Direction::Up,
            Some(&selected),
            &deadline,
        )?;
        let claimed = claim(&selected, &[], &pass.events);
        ledger::store(&state, &fp.anchor, &claimed, &[], &[]);
        common.end_return(&claimed)?;
        report = render_events(&pass.or_fail()?, opts.quiet);
        remember_common(&mut common, ssh, project, fp, &selected, &deadline)?;
    }
    if rsync_needed && !doomed.is_empty() {
        // Judged only now, after the upload and right before the removal
        // script's `rm -f`, which compares nothing: a server rewrite the
        // upload set off, such as a watcher regenerating the file, meets a
        // read taken after it, as in the full push. A row the server
        // changed since the last agreed content is the full reconcile's,
        // whose pull brings the server's copy home (`retire_moved`), so
        // the batch steps aside before this judgement writes anything.
        // What went up is claimed and recorded by then, so stepping aside
        // repeats none of it. A read the helper never answers is an error
        // like any SSH failure.
        let copies = read_doomed(ssh, project, &doomed, &deadline)?;
        if !split_moved(doomed.clone(), &common, &copies.remote)
            .1
            .is_empty()
        {
            session.common = Some(common);
            return Ok(None);
        }
        doomed = set_aside(
            &state,
            &fp.anchor,
            &project.inv.project_dir,
            &mut common,
            &copies,
            doomed,
        )?;
        doomed = retire_never_landed(&state, &fp.anchor, &mut common, &copies.remote, doomed)?;
        if !doomed.is_empty() {
            let removal = delete_remote(ssh, project, &doomed, &[])?;
            ledger::store(&state, &fp.anchor, &[], &removal.files, &[]);
            forget_removed(&mut common, &removal.files)?;
            report.deleted = removal.count;
            report.pending_deletions = removal.kept_files;
        }
    }
    session.observed.retain(|p, _| !within(p));
    session.observed.extend(observed);
    session.children.retain(|p, _| !within(p));
    session.children.extend(children);
    session.owned = ledger::synced_files(&state)
        .iter()
        .filter_map(|p| std::str::from_utf8(p).ok())
        .map(|p| fp.anchor.join(p))
        .collect();
    session.ledger = ledger_stamp(&state);
    session.plan = plan;
    session.common = Some(common);
    session.pending_deletions = report.pending_deletions;
    crate::invocation::record_pending_deletions(&state, report.pending_deletions);
    report.pulled += pull_changes(project, fp, ssh, opts, session)?.pulled;
    Ok(Some(report))
}

/// Bound reverse work to a small candidate batch. The same rsync filter
/// ranking and receiving-time existence guards as pull_back still decide
/// what can land. Local dirty observations take priority over remote output.
fn pull_changes(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    opts: &SyncOptions,
    session: &mut Session,
) -> Result<SyncReport> {
    let notices = match session.watcher.as_mut() {
        Some(watcher) => watcher.take(),
        None => Some(Vec::new()),
    };
    let Some(notices) = notices else {
        return reconcile_changes(project, fp, ssh, opts, session, None);
    };
    let excluded = crate::selection::Excludes::new(session.plan.excludes.clone());
    let mut existing = Vec::new();
    let mut invented = Vec::new();
    let mut acknowledge = Vec::new();
    let mut deferred = Vec::new();
    for notice in notices {
        if excluded.contains(notice.path.as_bytes()) {
            continue;
        }
        if notice.deleted {
            let path = fp.anchor.join(&notice.path);
            if !session.owned.contains(&path) && !path.try_exists()? {
                // An acknowledged ledger deletion echoes through inotify.
                // It cannot require another full reconciliation. A
                // returning file the server deleted has nothing left to
                // come home, and its mark would claim the next same-named
                // file the server makes (`returns_the_server_lost`).
                if let Some(common) = session.common.as_mut() {
                    common.end_return([notice.path.as_str()])?;
                }
                continue;
            }
            // The full engine restores locally present files and settles
            // ledger-owned deletions. The notice never authorizes a local rm.
            return reconcile_changes(project, fp, ssh, opts, session, None);
        }
        let path = fp.anchor.join(&notice.path);
        let current = crate::selection::Stamp::read(&path);
        if session.observed.get(&path) != current.as_ref() {
            session.deferred.insert(path);
            deferred.push(notice);
            continue;
        }
        if session.observed.contains_key(&path) {
            if notice.matches(&path).is_some() {
                continue;
            }
            let local = crate::baseline::fingerprint(&path)?;
            let remote = notice.content();
            let base = session.common.as_ref().and_then(|s| s.get(&notice.path));
            if !crate::baseline::may_pull(base, local.as_ref(), remote.as_ref()) {
                ui::warn(&format!(
                    "sync conflict: {} — both copies are preserved",
                    notice.path
                ));
                continue;
            }
            existing.push(notice.path.as_bytes().to_vec());
            acknowledge.push(notice);
        } else if !session.owned.contains(&path) && current.is_none() {
            invented.push(notice.path.as_bytes().to_vec());
            // A newly created local path must go through walk.rs before it
            // enters the warm selection: an ignore may apply only now that
            // the path exists. Its local watcher notice triggers that walk.
        }
    }
    if let Some(watcher) = session.watcher.as_mut() {
        watcher.retain(deferred);
    }
    let mut report = SyncReport {
        pending_deletions: session.pending_deletions,
        ..SyncReport::default()
    };
    if existing.is_empty() && invented.is_empty() {
        return Ok(report);
    }
    // Small files come home over the persistent channel in one exchange,
    // landed under the same guards the two rsync passes carry (`land`);
    // anything the channel refuses — a large file, a missing one — takes
    // those passes as before.
    let (mut events, new_events) =
        match stream_pull(project, fp, ssh, session, &existing, &invented)? {
            Some(landed) => landed,
            None => {
                let rsync = local_rsync()?;
                let budget = crate::proc::Budget::new(crate::proc::RECONCILE);
                let refreshed = rsync_pass_dir(
                    &rsync,
                    project,
                    fp,
                    ssh,
                    &session.plan.excludes,
                    &[],
                    &["--checksum", "--update", "--existing", "--prune-empty-dirs"],
                    Direction::Down,
                    Some(&existing),
                    &budget,
                )?
                .or_fail()?;
                let created = rsync_pass_dir(
                    &rsync,
                    project,
                    fp,
                    ssh,
                    &session.plan.excludes,
                    &[],
                    &["--ignore-existing", "--prune-empty-dirs"],
                    Direction::Down,
                    Some(&invented),
                    &budget,
                )?
                .or_fail()?;
                (refreshed, created)
            }
        };
    say_what_slipped_past_the_ignores(project, fp, &new_events);
    // A returning file lands here when no full pull brought it home first.
    // Nothing above recorded what a created file holds, so the agreed
    // content is read before the claim, as the full pull's confirmation
    // does; claimed on the old record, the next local deletion would read
    // as the server's edit and bring the file back again.
    if let Some(common) = session.common.as_mut() {
        let landed = landed_back(common, &new_events);
        if !landed.is_empty() {
            let rows: Vec<Vec<u8>> = landed.iter().map(|p| p.as_bytes().to_vec()).collect();
            let budget = crate::proc::Budget::new(crate::proc::RECONCILE);
            remember_common(common, ssh, project, fp, &rows, &budget)?;
            let state = project.state_key(&ssh.dest);
            claim_returned(&state, &fp.anchor, common, &landed)?;
            session
                .owned
                .extend(landed.iter().map(|p| fp.anchor.join(p)));
            session.ledger = ledger_stamp(&state);
        }
    }
    events.extend(new_events);
    for notice in acknowledge {
        let path = fp.anchor.join(&notice.path);
        if let Some(stamp) = notice.matches(&path) {
            if let Some(common) = session.common.as_mut() {
                common.record(std::collections::BTreeMap::from([(
                    notice.path.clone(),
                    notice.content(),
                )]))?;
            }
            session.observed.insert(path, stamp);
        }
    }
    report.pulled = events
        .iter()
        .filter(|e| matches!(e, Event::Push(p) if !p.ends_with('/')))
        .count();
    Ok(report)
}

/// What the channel made of a structural batch.
enum Structure {
    /// The helper applied the whole batch.
    Applied(SyncReport),
    /// Not a batch for the channel — a large or unstable file, too many
    /// items, no helper — so the rsync path takes it as before.
    Rsync,
    /// The helper refused to unlink a doomed row: the server's copy is not
    /// the last agreed content. Nothing was applied, and the full
    /// reconcile decides the row (`retire_moved`).
    Full,
}

/// The structural batch over the persistent channel: creates for leaves
/// the ledger does not claim, guarded replaces for the ones it does, and
/// guarded unlinks for the doomed rows — one exchange, one round trip,
/// where rsync plus the checks around it cost three sessions. The helper
/// verifies every item before applying any, so a refusal applies nothing.
/// A refused unlink sends the batch to the full reconcile, where the
/// server's edit wins over the deletion; any other refusal is a conflict
/// for the full survey to hold.
#[allow(clippy::too_many_arguments)]
fn stream_structure(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    session: &mut Session,
    common: &mut crate::baseline::State,
    state: &crate::config::WorkspaceStateKey,
    selected: &[Vec<u8>],
    doomed: &[String],
    observed: &std::collections::BTreeMap<PathBuf, crate::selection::Stamp>,
    quiet: bool,
) -> Result<Structure> {
    if session.stream_unavailable {
        return Ok(Structure::Rsync);
    }
    let mut batch = crate::stream::Batch::new();
    for raw in selected {
        let name = std::str::from_utf8(raw)?;
        let path = fp.anchor.join(name);
        let Some(stamp) = observed.get(&path) else {
            return Ok(Structure::Rsync);
        };
        let create = !session.owned.contains(&path);
        if batch.add(&fp.anchor, &path, stamp, create).is_none() {
            return Ok(Structure::Rsync);
        }
    }
    for name in doomed {
        // The helper unlinks only a copy it can compare to the last agreed
        // content; a row without one takes the rsync path, where
        // `never_landed` and `moved_on_the_server` judge it before the
        // removal script runs.
        let Some(expected) = common.get(name).cloned() else {
            return Ok(Structure::Rsync);
        };
        if batch.unlink(name.clone(), Some(expected)).is_none() {
            return Ok(Structure::Rsync);
        }
    }
    if batch.is_empty() {
        return Ok(Structure::Applied(SyncReport::default()));
    }
    batch.guard(&common.entries);
    let accepted = batch.contents();
    let written = batch.written();
    // Claimed BEFORE the exchange, beside an unconfirmed record of what
    // is sent (`fresh_claims`, `unconfirmed_sends`); a refusal retires
    // both, a lost receipt leaves both standing.
    let fresh = fresh_claims(&written, |name| {
        session.owned.contains(&fp.anchor.join(name))
    });
    // The protective record first: a failed or interrupted write must
    // never leave a claim standing without it (measured).
    let sends = unconfirmed_sends(&fresh, common, &accepted);
    common.record_unconfirmed(sends.clone())?;
    ledger::store(state, &fp.anchor, &written, &[], &[]);
    let budget = crate::proc::Budget::new(crate::proc::STREAM);
    let result =
        open_stream(project, ssh, session, &budget).and_then(|client| client.send(batch, &budget));
    match result {
        Ok(applied) if applied.refused.is_empty() => {
            common.record(accepted)?;
            // Claimed for good now, which ends any return; a refusal
            // below retires a fresh claim and leaves its return standing.
            common.end_return(&written)?;
            ledger::store(state, &fp.anchor, &[], &applied.deleted, &[]);
            let events: Vec<Event> = applied.changed.into_iter().map(Event::Push).collect();
            let mut report = render_events(&events, quiet);
            report.deleted = applied.deleted.len();
            if !quiet {
                for p in &applied.deleted {
                    ui::dim(&format!("- {p}"));
                }
            }
            Ok(Structure::Applied(report))
        }
        Ok(applied) => {
            ledger::store(state, &fp.anchor, &[], &fresh, &[]);
            common.settle(&fresh)?;
            // A doomed row the SSH user may not read is the rsync
            // fallback's to hold (`set_aside`), which deletes the rest of
            // the batch's rows; stepping aside to the full path instead
            // would repeat on every structural batch while it stays so.
            if applied
                .refused
                .iter()
                .any(|(p, why)| doomed.contains(p) && why == "unreadable")
            {
                return Ok(Structure::Rsync);
            }
            if applied.refused.iter().any(|(p, _)| doomed.contains(p)) {
                return Ok(Structure::Full);
            }
            Err(content_conflict(&format!(
                "the server's copy of {} {}",
                applied.refused[0].0, applied.refused[0].1
            )))
        }
        Err(error) => {
            // No receipt: the helper may have applied the batch before the
            // answer was lost, so the fresh claims STAND — retiring them
            // would make an upload look server-born and pull it back after
            // a local delete — and so does their unconfirmed record, which
            // the deletion rule compares with the server's copy
            // (`never_landed`) before a claim is acted on. The replaces
            // join that record now (`attempted`).
            let replaced = attempted(common, &accepted)
                .into_iter()
                .filter(|(p, _)| !sends.contains_key(p))
                .collect();
            let recorded = common.record_unconfirmed(replaced);
            revoke_stream(project, ssh, session)?;
            recorded?;
            Err(error)
        }
    }
}

/// The way down over the channel: one read exchange for the existing and
/// the invented candidates, each landed only over the stamp it was
/// observed under or into a name still absent (stream::land). None means
/// the channel is not for this batch — unavailable, too many names, or a
/// refusal such as a file past the helper's size — and the rsync passes
/// decide as before; nothing was written either way.
fn stream_pull(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    session: &mut Session,
    existing: &[Vec<u8>],
    invented: &[Vec<u8>],
) -> Result<Option<(Vec<Event>, Vec<Event>)>> {
    if session.stream_unavailable {
        return Ok(None);
    }
    let mut batch = crate::stream::Batch::new();
    for raw in existing.iter().chain(invented) {
        if batch.read(String::from_utf8(raw.clone())?).is_none() {
            return Ok(None);
        }
    }
    let budget = crate::proc::Budget::new(crate::proc::STREAM);
    let result =
        open_stream(project, ssh, session, &budget).and_then(|client| client.send(batch, &budget));
    match result {
        Ok(applied) if applied.refused.is_empty() => {
            let mut refreshed = Vec::new();
            let mut created = Vec::new();
            for pulled in &applied.pulled {
                let path = fp.anchor.join(&pulled.path);
                let known = existing
                    .iter()
                    .any(|e| e.as_slice() == pulled.path.as_bytes());
                let expected = session.observed.get(&path);
                if known != expected.is_some() {
                    continue;
                }
                if crate::stream::land(&fp.anchor, pulled, expected)?.is_some() {
                    let events = if known { &mut refreshed } else { &mut created };
                    events.push(Event::Push(pulled.path.clone()));
                }
            }
            Ok(Some((refreshed, created)))
        }
        Ok(_) => Ok(None),
        Err(error) => {
            revoke_stream(project, ssh, session)?;
            Err(error)
        }
    }
}

/// The session's channel, opened on first use.
fn open_stream<'a>(
    project: &Project,
    ssh: &Ssh,
    session: &'a mut Session,
    budget: &crate::proc::Budget,
) -> Result<&'a mut crate::stream::Client> {
    if session.stream.is_none() {
        session.stream = Some(crate::stream::Client::connect(
            ssh,
            &project.remote_dir(),
            &project.remote_workspace_root(),
            budget,
        )?);
    }
    session.stream.as_mut().ok_or_else(|| {
        fail!("the persistent transfer channel was not opened")
            .now("run ulak sync to reconcile with rsync")
            .into_err()
    })
}

/// After a transport failure the channel is closed and its remote lease
/// retired, so bytes it may still be applying cannot land behind the
/// rsync repair that follows. A lost ACK is not permission for an
/// unconditional repair either: the next full survey decides.
fn revoke_stream(project: &Project, ssh: &Ssh, session: &mut Session) -> Result<()> {
    session.stream = None;
    session.stream_unavailable = true;
    crate::stream::fence(
        ssh,
        &project.remote_workspace_root(),
        &crate::proc::Budget::new(crate::proc::STREAM),
    )
}

/// The stream has no policy of its own: every candidate is an already
/// selected and claimed leaf. A failed/partial batch is repaired by the
/// checksum pass, after revoking its remote lease so buffered bytes cannot
/// race the repair even if killing local SSH leaves the remote child alive.
fn transfer_changed(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    session: &mut Session,
    changed: &std::collections::BTreeMap<PathBuf, crate::selection::Stamp>,
) -> Result<Option<Vec<Event>>> {
    if session.stream_unavailable {
        return Ok(None);
    }
    let Some(mut batch) = crate::stream::prepare(&fp.anchor, changed) else {
        return Ok(None);
    };
    let Some(common) = session.common.as_ref() else {
        return Err(fail!("the common-content state is unavailable")
            .now("run ulak sync before retrying the transfer")
            .into_err());
    };
    batch.guard(&common.entries);
    let accepted = batch.contents();
    let budget = crate::proc::Budget::new(crate::proc::STREAM);
    let result =
        open_stream(project, ssh, session, &budget).and_then(|client| client.send(batch, &budget));
    match result {
        Ok(applied) if applied.refused.is_empty() => {
            if let Some(common) = session.common.as_mut() {
                common.record(accepted)?;
            }
            Ok(Some(applied.changed.into_iter().map(Event::Push).collect()))
        }
        // The helper applied nothing: the server's copy is not the one both
        // ends last agreed on. That is a conflict for the full survey to
        // hold, not a transport failure, so the channel is not revoked.
        Ok(applied) => Err(content_conflict(&format!(
            "the server's copy of {} {}",
            applied.refused[0].0, applied.refused[0].1
        ))),
        Err(error) => {
            // The helper may have applied the replaces before the answer
            // was lost (`attempted`).
            let recorded = session.common.as_mut().map_or(Ok(()), |common| {
                let replaced = attempted(common, &accepted);
                common.record_unconfirmed(replaced)
            });
            revoke_stream(project, ssh, session)?;
            recorded?;
            Err(error)
        }
    }
}

fn changed_leaves(
    anchor: &std::path::Path,
    session: &Session,
    paths: &std::collections::BTreeSet<PathBuf>,
) -> Option<std::collections::BTreeMap<PathBuf, crate::selection::Stamp>> {
    let mut changed = std::collections::BTreeMap::new();
    for path in paths {
        if !session.observed.contains_key(path)
            && !session.owned.contains(path)
            && std::fs::symlink_metadata(path)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            // Editors rename a temporary sibling over a known file. A
            // vanished, never-owned sibling has no bytes or deletion to
            // send; the final filename's own notice remains in the batch.
            continue;
        }
        let previous = session.observed.get(path)?;
        let current = crate::selection::Stamp::read(path)?;
        if let Some(before) = session.children.get(path) {
            // Some watcher backends report a directory instead of its
            // changed child. A stable directory mtime says nothing about
            // writes inside existing files. Only a batch that also names
            // descendants can use the atomic-save shortcut.
            if paths.iter().any(|p| p != path && p.starts_with(path))
                && directory_children(path).as_ref() == Some(before)
            {
                continue;
            }
            return None;
        }
        if previous == &current {
            continue;
        }
        let name = path.file_name()?.to_str()?;
        if name.ends_with(".dockerignore")
            || name == "ulak.toml"
            || !path.starts_with(anchor)
            || !std::fs::symlink_metadata(path).ok()?.is_file()
        {
            return None;
        }
        changed.insert(path.clone(), current);
    }
    Some(changed)
}

fn directory_children(
    path: &std::path::Path,
) -> Option<std::collections::BTreeSet<std::ffi::OsString>> {
    std::fs::read_dir(path)
        .ok()?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<_>>()
        .ok()
}

/// THE engine, and the only one.
///
/// `ulak sync` runs it and the service runs it — the same function,
/// so "two engines that quietly drift apart" is not a risk this codebase
/// can carry. Both legs run inside the caller's per-workspace lock: the
/// service takes that lock with `try_acquire` and skips the tick when a
/// human holds it, the CLI waits. `watch` used to run its idle-tick
/// `pull_back` OUTSIDE the lock, so a compose build could read the tree
/// while files were still landing in it.
///
/// The whole reconcile shares ONE budget. Bounding each rsync leg
/// separately let a dead link cost ~32 minutes of held lock, which is
/// also the longest a human `ulak docker compose up` could have waited behind it.
pub fn reconcile_once(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    opts: &SyncOptions,
) -> Result<SyncReport> {
    let deadline = crate::proc::Budget::new(crate::proc::RECONCILE);
    if opts.dry_run {
        return run_sync(project, fp, ssh, opts, &deadline);
    }
    let plan = checked_plan(project, fp)?;
    let (report, mut survey) = reconcile_full(project, fp, ssh, opts, plan, &deadline)?;
    survey.finish()?;
    Ok(report)
}

/// Both legs over one survey, handed back so a session can keep its
/// common-content state and excludes without reading either again.
fn reconcile_full(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    opts: &SyncOptions,
    plan: walk::WalkReport,
    deadline: &crate::proc::Budget,
) -> Result<(SyncReport, Survey)> {
    let started = std::time::SystemTime::now();
    let (mut survey, doomed, doomed_dirs) = prepare(project, fp, ssh, plan, true, deadline)?;
    let report = transfer(
        project,
        fp,
        ssh,
        opts,
        &mut survey,
        doomed,
        doomed_dirs,
        started,
        deadline,
        &mut None,
    )?;
    Ok((report, survey))
}

/// Both legs of a prepared survey. `started` predates the survey: it is
/// the line between "the server may already know this" and "you typed it
/// just now, and it is yours" — see `pull`.
#[allow(clippy::too_many_arguments)]
fn transfer(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    opts: &SyncOptions,
    survey: &mut Survey,
    doomed: Vec<String>,
    doomed_dirs: Vec<String>,
    started: std::time::SystemTime,
    deadline: &crate::proc::Budget,
    channel: &mut Option<crate::stream::Client>,
) -> Result<SyncReport> {
    let mut report = push(
        project,
        fp,
        ssh,
        opts,
        survey,
        doomed,
        doomed_dirs,
        deadline,
        channel,
    )?;
    // A workspace runs both ways, because a local bind mount does: whatever
    // the stack produced comes home in the same breath it went up.
    let back = pull(
        project,
        fp,
        ssh,
        opts.quiet,
        survey,
        deadline,
        Some(started),
        channel,
    )?;
    report.pulled += back.pulled;
    Ok(report)
}

/// What one exchange over the survey's channel came to. `Refused` is the
/// helper's answer with nothing applied and every reason by name: the
/// caller must not simply hand the same selection to rsync, because a
/// refusal for content means the server moved since the survey and rsync
/// would write over that. `Unavailable` is a channel that could not be
/// opened, so nothing was sent. `Lost` is a transport that failed after
/// the batch was handed over, so the helper may have applied it; the
/// lease is retired so bytes still buffered remotely cannot land behind
/// the rsync that follows, and when that retirement cannot reach the
/// server `channel_send` fails instead. Either way the rsync pass takes
/// the batch as before.
enum Sent {
    Applied(crate::stream::Applied),
    Refused(Vec<(String, String)>),
    Unavailable,
    Lost,
}

/// One exchange over the survey's channel, opened on first use and handed
/// to the warm session afterwards.
fn channel_send(
    project: &Project,
    ssh: &Ssh,
    channel: &mut Option<crate::stream::Client>,
    batch: crate::stream::Batch,
) -> Result<Sent> {
    let budget = crate::proc::Budget::new(crate::proc::STREAM);
    if channel.is_none() {
        match crate::stream::Client::connect(
            ssh,
            &project.remote_dir(),
            &project.remote_workspace_root(),
            &budget,
        ) {
            Ok(client) => *channel = Some(client),
            Err(error) => {
                ui::dim(&ui::flatten(&error));
                return Ok(Sent::Unavailable);
            }
        }
    }
    let Some(client) = channel.as_mut() else {
        return Ok(Sent::Unavailable);
    };
    match client.send(batch, &budget) {
        Ok(applied) if applied.refused.is_empty() => Ok(Sent::Applied(applied)),
        Ok(applied) => Ok(Sent::Refused(applied.refused)),
        Err(error) => {
            ui::dim(&ui::flatten(&error));
            *channel = None;
            crate::stream::fence(ssh, &project.remote_workspace_root(), &budget)?;
            Ok(Sent::Lost)
        }
    }
}

/// The helper's refusals that name a limit of the channel, not a move on
/// the server: a create under a parent directory the server lacks — the
/// footprint's own roots are not surveyed as directories, so a first
/// population reaches the channel this way, and rsync's creating pass
/// makes the parents as it always has. `stream.py`'s `verify` answers it
/// for a create only, and compares a large server copy before it answers
/// anything, so no push refusal here can be a move on the server in
/// disguise. "needs rsync" is a pull's answer and is deliberately absent:
/// on a push it would mean the helper skipped that comparison.
const RSYNC_CAN: &[&str] = &["missing parent"];

/// What a refused push batch leaves for rsync, and what it holds. The
/// helper refuses a whole batch at once; only a reason in `RSYNC_CAN` is
/// an invitation to rsync. Any other reason — the server's copy differs
/// from the survey's, a name appeared, a file vanished — says the server
/// moved in the window since the survey, and rsync sending the stale
/// selection would write over that; those leaves are held, like any
/// conflict, for the next survey to judge.
fn split_refusal(
    selected: &[String],
    refused: &[(String, String)],
) -> (Vec<String>, Vec<(String, String)>) {
    let held: Vec<(String, String)> = refused
        .iter()
        .filter(|(_, reason)| !RSYNC_CAN.contains(&reason.as_str()))
        .cloned()
        .collect();
    let rsync = selected
        .iter()
        .filter(|p| !held.iter().any(|(h, _)| h == *p))
        .cloned()
        .collect();
    (rsync, held)
}

/// The claims a batch would make for the first time. Claimed BEFORE the
/// exchange like every push, so a create the helper applied without a
/// receipt is never left unclaimed — and retired again when a refusal
/// proves the exchange applied nothing, because a claim on a file the
/// server held all along is a licence to delete it later:
/// `moved_on_the_server` has no record to consult for it. Measured: a
/// refused create, then a local delete, doomed the server's own
/// file even under no_history = hold. A lost receipt is not such proof;
/// see `never_landed`.
fn fresh_claims(written: &[Vec<u8>], claimed: impl Fn(&str) -> bool) -> Vec<String> {
    written
        .iter()
        .filter_map(|w| std::str::from_utf8(w).ok())
        .filter(|name| !claimed(name))
        .map(str::to_owned)
        .collect()
}

/// One full comparison of the selection, taken once per reconcile and
/// shared by the push and the pull. Its digests are the transfer
/// selection: the fingerprints already say which leaves differ, so rsync
/// is handed exactly those names instead of hashing the tree a second
/// time under `--checksum`. Wider than `EXACT_PASS_LIMIT`, the traversing
/// pass takes over — the module header records why a long files-from list
/// is the wrong shape for rsync.
struct Survey {
    plan: walk::WalkReport,
    local: crate::baseline::Contents,
    remote: crate::baseline::Contents,
    /// The synced directories' modes, `d:<mode>` or None, on each end.
    /// Never common content: only the push reads them, to name what is
    /// missing or re-moded on the server.
    local_dirs: crate::baseline::Contents,
    remote_dirs: crate::baseline::Contents,
    /// The server's file inventory for the creating pull, taken alongside
    /// the fingerprints when a pull will follow. What the push creates in
    /// the meantime is claimed by then and filtered out by the ledger.
    listing: Option<Vec<u8>>,
    /// The names returning (`baseline::State::returning`) when `listing`
    /// was asked for. A listing shows the server without a returning file
    /// only for a mark made before it (`returns_the_server_lost`); the
    /// push's judgement that marks a file reads the server after it.
    returning_when_listed: std::collections::BTreeSet<String>,
    common: crate::baseline::State,
    cache: crate::hashcache::Cache,
}

/// Above this many exact names, one traversing rsync is cheaper than the
/// receiver's all-pairs implied-include check (module header).
const EXACT_PASS_LIMIT: usize = 4096;

impl Survey {
    fn take(
        project: &Project,
        fp: &Footprint,
        ssh: &Ssh,
        plan: walk::WalkReport,
        for_pull: bool,
        deadline: &crate::proc::Budget,
    ) -> Result<Survey> {
        let state = project.state_key(&ssh.dest);
        let common = crate::baseline::State::load(&state, &fp.anchor)?;
        let returning_when_listed = common.returning_names();
        let mut cache = crate::hashcache::Cache::open(&state, &fp.anchor);
        // The inventory listing runs while this machine hashes; the server's
        // fingerprints are then asked for by difference — per-directory
        // digests up, only the disagreeing directories down (baseline.rs
        // `survey_in_background`). The cache is written before any answer
        // can fail: a file that changed mid-hash costs one retry, not the
        // selection's digests.
        let listing =
            for_pull.then(|| inventory_in_background(project, fp, ssh, &plan.excludes, deadline));
        let local = cache.local(&fp.anchor, &plan.kept);
        cache.save();
        let local = local?;
        let local_dirs = crate::baseline::directories(&fp.anchor, &plan.kept_dirs)?;
        let answer = crate::baseline::survey_in_background(
            ssh,
            &project.remote_dir(),
            &crate::baseline::remote_cache(project),
            &local,
            &local_dirs,
            deadline,
        );
        let answer = answer.join().map_err(|_| {
            fail!("the server content comparison did not finish")
                .now("run ulak sync again; neither copy was chosen")
                .into_err()
        })??;
        let listing = listing
            .map(|handle| {
                handle.join().map_err(|_| {
                    fail!("the server's file inventory did not finish")
                        .now("run ulak sync again")
                        .into_err()
                })?
            })
            .transpose()?;
        let (remote, remote_dirs) = answer.merge(&local, &local_dirs)?;
        Ok(Survey {
            plan,
            local,
            remote,
            local_dirs,
            remote_dirs,
            listing,
            returning_when_listed,
            common,
            cache,
        })
    }

    /// What a transfer just made equal, re-read on both ends and folded
    /// back in, so the leg that follows sees the tree as it now is. Read
    /// through the caches like the survey: a first population confirms
    /// every file it sent, and the server's copies are new either way.
    fn confirm(
        &mut self,
        ssh: &Ssh,
        project: &Project,
        fp: &Footprint,
        names: &[Vec<u8>],
        deadline: &crate::proc::Budget,
    ) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let remote = crate::baseline::remote_in_background(
            ssh,
            &project.remote_dir(),
            &crate::baseline::remote_cache(project),
            names,
            deadline,
        );
        let local = self.cache.local(&fp.anchor, names);
        let (local, remote) = (local?, gathered(remote)?);
        self.common
            .record(crate::baseline::equal(&local, &remote))?;
        self.local.extend(local);
        self.remote.extend(remote);
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.cache.save();
        self.common.compact()
    }
}

/// The server's half of a survey, collected after this machine's. A thread
/// that panicked is reported like a failed comparison: neither copy is
/// chosen.
fn gathered(
    remote: std::thread::JoinHandle<Result<crate::baseline::Contents>>,
) -> Result<crate::baseline::Contents> {
    remote.join().map_err(|_| {
        fail!("the server content comparison did not finish")
            .now("run ulak sync again; neither copy was chosen")
            .into_err()
    })?
}

/// Which surveyed leaves a push may send: every one whose content differs
/// and whose server copy is absent or still the common ancestor — or,
/// under `[sync] no_history = "send"`, has no ancestor on record. `held`
/// are the rest — differing on both ends with proof, or no permission,
/// that the server moved — so neither copy is chosen. A leaf gone from
/// disk since the walk is neither: there is nothing to send and nothing
/// to hold.
fn push_selection(
    no_history: crate::config::NoHistory,
    base: &crate::baseline::Contents,
    local: &crate::baseline::Contents,
    remote: &crate::baseline::Contents,
) -> (Vec<String>, Vec<String>) {
    let mut held = Vec::new();
    let mut selected = Vec::new();
    for (p, l) in local {
        let r = remote.get(p).and_then(Option::as_ref);
        if !crate::baseline::may_push(
            no_history,
            base.get(p).and_then(Option::as_ref),
            l.as_ref(),
            r,
        ) {
            held.push(p.clone());
        } else if l.is_some() && l.as_ref() != r {
            selected.push(p.clone());
        }
    }
    (held, selected)
}

/// The synced directories a push must still name: absent on the server,
/// or carrying a different mode. rsync creates a named directory with its
/// mode and without recursing, so an unchanged one costs nothing to omit.
fn dir_selection(
    local: &crate::baseline::Contents,
    remote: &crate::baseline::Contents,
) -> Vec<String> {
    local
        .iter()
        .filter(|(p, l)| l.is_some() && l.as_ref() != remote.get(*p).and_then(Option::as_ref))
        .map(|(p, _)| p.clone())
        .collect()
}

/// The mirror for the way down: server output may land only on a local
/// copy that still IS the common ancestor; an unsent edit is held.
fn pull_selection(
    base: &crate::baseline::Contents,
    local: &crate::baseline::Contents,
    remote: &crate::baseline::Contents,
) -> (Vec<String>, Vec<String>) {
    let mut held = Vec::new();
    let mut selected = Vec::new();
    for (p, l) in local {
        let r = remote.get(p).and_then(Option::as_ref);
        if !crate::baseline::may_pull(base.get(p).and_then(Option::as_ref), l.as_ref(), r) {
            held.push(p.clone());
        } else if r.is_some() && l.as_ref() != r {
            selected.push(p.clone());
        }
    }
    (held, selected)
}

pub fn run_sync(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    opts: &SyncOptions,
    deadline: &crate::proc::Budget,
) -> Result<SyncReport> {
    let plan = checked_plan(project, fp)?;
    if opts.dry_run {
        return dry_run(project, fp, ssh, opts, &plan, deadline);
    }
    let (mut survey, doomed, doomed_dirs) = prepare(project, fp, ssh, plan, false, deadline)?;
    let report = push(
        project,
        fp,
        ssh,
        opts,
        &mut survey,
        doomed,
        doomed_dirs,
        deadline,
        &mut None,
    )?;
    survey.finish()?;
    Ok(report)
}

/// What a real sync would do, told without a survey: a traversing
/// `--dry-run` itemizes what rsync's quick check would send, and the
/// ledger names what would be removed — less the rows a real sync would
/// keep, judged over a read of their server copies that retires nothing
/// (`preview_deletions`). The over-budget error sends people here to
/// review the list, so it must not name a file the sync would keep.
fn dry_run(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    opts: &SyncOptions,
    plan: &walk::WalkReport,
    deadline: &crate::proc::Budget,
) -> Result<SyncReport> {
    let rsync_bin = local_rsync()?;
    let state = project.state_key(&ssh.dest);
    let budget = opts
        .max_delete_override
        .unwrap_or(project.config.sync.max_delete);
    let doomed = doomed_files(project, &state, &fp.anchor, &plan.kept);
    let doomed_dirs = doomed_directories(project, &state, &fp.anchor);
    let events = rsync_pass(
        &rsync_bin,
        project,
        fp,
        ssh,
        &plan.excludes,
        &[],
        &["--dry-run"],
        deadline,
    )?
    .or_fail()?;
    let mut report = render_events(&events, opts.quiet);
    let (doomed, doomed_dirs, kept) = if doomed.is_empty() {
        (doomed, doomed_dirs, Vec::new())
    } else {
        let judged = crate::baseline::State::load(&state, &fp.anchor).and_then(|common| {
            let copies = read_doomed(ssh, project, &doomed, deadline)?;
            Ok((common, copies))
        });
        match judged {
            Ok((common, copies)) => {
                let (doomed, kept) = preview_deletions(doomed, &common, &copies);
                (doomed, spare_dirs_above(doomed_dirs, &copies), kept)
            }
            Err(error) => {
                ui::warn(&format!(
                    "could not read the server's copies of the files a sync would delete, so every deletion is listed; a real sync stops on the same error: {}",
                    ui::flatten(&error)
                ));
                (doomed, doomed_dirs, Vec::new())
            }
        }
    };
    for p in &doomed {
        report.deleted += 1;
        if !opts.quiet {
            ui::dim(&format!("- {p}"));
        }
    }
    for d in &doomed_dirs {
        report.deleted += 1;
        if !opts.quiet {
            ui::dim(&format!("- {d}/"));
        }
    }
    if !opts.quiet {
        for line in &kept {
            ui::dim(line);
        }
    }
    summarize(&report, opts.quiet, true, budget);
    if !budget.allows(doomed.len()) {
        ui::warn(&format!(
            "{} deletions exceed the budget of {budget} — a real sync will ask first",
            doomed.len()
        ));
    }
    Ok(report)
}

/// The ledger's doomed files (`ledger::doomed`) less every `[sync] protect`
/// path: every place that asks what a sync would delete asks here. A
/// protected path never enters the ledger, but a row claimed before its
/// path was protected stays claimed, and protect means never deleted.
/// Without this, the protect entry `held_warning` advises for a file the
/// server owns changed nothing: the row stayed doomed, held and named on
/// every sync, and deleted as soon as it could be read.
fn doomed_files(
    project: &Project,
    state: &crate::config::WorkspaceStateKey,
    anchor: &std::path::Path,
    kept: &[Vec<u8>],
) -> Vec<String> {
    let protect = project.sync_rules().protect;
    let mut doomed = ledger::doomed(state, anchor, kept);
    doomed.retain(|row| !walk::protected(&protect, row));
    doomed
}

/// `ledger::doomed_dirs` less every `[sync] protect` path, as
/// `doomed_files` says.
fn doomed_directories(
    project: &Project,
    state: &crate::config::WorkspaceStateKey,
    anchor: &std::path::Path,
) -> Vec<String> {
    let protect = project.sync_rules().protect;
    let mut doomed = ledger::doomed_dirs(state, anchor);
    doomed.retain(|row| !walk::protected(&protect, row));
    doomed
}

/// What a push needs settled before the survey is worth taking: the
/// doomed set from the ledger alone, and a workspace that exists on the
/// server. baseline.py opens the project directory, so a first sync must
/// create it first — and a re-layout empties it, which the survey then
/// reads as it is rather than remembers as it was.
fn prepare(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    plan: walk::WalkReport,
    for_pull: bool,
    deadline: &crate::proc::Budget,
) -> Result<(Survey, Vec<String>, Vec<String>)> {
    let state = project.state_key(&ssh.dest);
    // Computed BEFORE the push, from the ledger alone: no remote listing,
    // no second rsync pass, and the answer cannot change under us.
    let mut doomed = doomed_files(project, &state, &fp.anchor, &plan.kept);
    // Directories go the same way, by their own command. They are kept
    // out of the BUDGET on purpose: the budget guards against destroying
    // work, and `rmdir` cannot — it refuses a directory that still holds
    // anything. A branch switch that retires forty directories is not the
    // kind of surprise the threshold exists to stop.
    let mut doomed_dirs = doomed_directories(project, &state, &fp.anchor);
    on_layout(
        ensure_workspace(ssh, project)?,
        &state,
        &mut doomed,
        &mut doomed_dirs,
    );
    let survey = Survey::take(project, fp, ssh, plan, for_pull, deadline)?;
    Ok((survey, doomed, doomed_dirs))
}

/// The way up, and the removals it licenses.
#[allow(clippy::too_many_arguments)]
fn push(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    opts: &SyncOptions,
    survey: &mut Survey,
    doomed: Vec<String>,
    mut doomed_dirs: Vec<String>,
    deadline: &crate::proc::Budget,
    channel: &mut Option<crate::stream::Client>,
) -> Result<SyncReport> {
    let rsync_bin = local_rsync()?;
    let state = project.state_key(&ssh.dest);
    let budget = opts
        .max_delete_override
        .unwrap_or(project.config.sync.max_delete);
    let events = push_and_claim(
        &rsync_bin, project, fp, ssh, survey, &state, deadline, channel,
    )?;
    let mut report = render_events(&events, opts.quiet);
    // Judged BEFORE the budget counts the rows: the judgements only read
    // the server and retire claims, so they are safe even when the
    // deletions then wait over budget, and a row the sync keeps must not
    // be counted, prompted for or recorded as pending.
    let copies = read_doomed(ssh, project, &doomed, deadline)?;
    let mut doomed = judge_doomed(
        &state,
        &fp.anchor,
        &project.inv.project_dir,
        &mut survey.common,
        &copies,
        doomed,
    )?;
    doomed_dirs = spare_dirs_above(doomed_dirs, &copies);
    if doomed.is_empty() && doomed_dirs.is_empty() {
        crate::invocation::record_pending_deletions(&state, 0);
        summarize(&report, opts.quiet, false, budget);
        return Ok(report);
    }

    if !budget.allows(doomed.len()) {
        // No terminal to ask in (the daemon): push, report, carry on.
        // Nothing else is retired, so the rows stay exactly as they are —
        // which is what "those files are still up there" means, and what
        // lets the next sync name them again.
        if opts.over_budget == OverBudget::Report {
            report.pending_deletions = doomed.len();
            crate::invocation::record_pending_deletions(&state, doomed.len());
            summarize(&report, opts.quiet, false, budget);
            return Ok(report);
        }
        if !confirm_deletions(&doomed, budget)? {
            crate::invocation::record_pending_deletions(&state, doomed.len());
            let review = crate::management::root_command(project, &["sync", "--dry-run"]);
            let budget_arg = format!("--max-delete={}", doomed.len());
            let delete = crate::management::root_command(project, &["sync", &budget_arg]);
            return Err(fail!(
                "sync stopped: {} server-side deletions exceed the budget of {budget} \
                 (files were pushed; nothing was deleted)",
                doomed.len()
            )
            .now(format!("review the list: {review}"))
            .now(format!("delete them: {delete}"))
            .now("or mark server-owned paths as protect in ulak.toml")
            .into_err());
        }
        // The prompt waited on a person with no time limit, so the read
        // judged above may be minutes old, and the removal script's
        // `rm -f` compares nothing. A server edit made while the list was
        // open is spared like any other: read and judge again, now.
        let copies = read_doomed(ssh, project, &doomed, deadline)?;
        doomed = judge_doomed(
            &state,
            &fp.anchor,
            &project.inv.project_dir,
            &mut survey.common,
            &copies,
            doomed,
        )?;
        doomed_dirs = spare_dirs_above(doomed_dirs, &copies);
    }

    // Listed after both judgements, so a row whose deletion is not
    // applied is never shown as removed.
    if !opts.quiet {
        for p in &doomed {
            ui::dim(&format!("- {p}"));
        }
        // Spelled with the slash they are: "- site/old" and "- site/old/"
        // are different removals, and the second one is the line that
        // says a directory went.
        for d in &doomed_dirs {
            ui::dim(&format!("- {d}/"));
        }
    }
    let removal = delete_remote(ssh, project, &doomed, &doomed_dirs)?;
    forget_removed(&mut survey.common, &removal.files)?;
    report.deleted += removal.count;
    summarize(&report, opts.quiet, false, budget);
    // Only what the server REALLY lost is un-claimed. A `rmdir` the
    // container's own work refused leaves a directory standing, and a row
    // dropped for it is the empty shell nobody can clear afterwards.
    ledger::store(&state, &fp.anchor, &[], &removal.files, &removal.dirs);
    // What the server KEPT is a deletion this sync listed and did not
    // apply, and the number is the only way `status` and `doctor` learn
    // of it. Writing 0 here said "clean" while the workspace held files
    // the project had deleted — for good, since every later sync lists
    // them again and reports the same zero.
    report.pending_deletions = removal.kept_files;
    crate::invocation::record_pending_deletions(&state, removal.kept_files);
    Ok(report)
}

/// The server's copies of the doomed rows, read for the deletion
/// judgements in one request (`baseline::verdicts`): a copy, a structural
/// verdict or a hold for every row. It fails only when the helper does not
/// answer, and that is an error like any SSH failure: nothing is judged,
/// deferred or recorded as pending.
fn read_doomed(
    ssh: &Ssh,
    project: &Project,
    doomed: &[String],
    deadline: &crate::proc::Budget,
) -> Result<crate::baseline::Copies> {
    crate::baseline::verdicts(ssh, &project.remote_dir(), doomed, deadline)
}

/// Every deletion judgement over `copies`, the server's copies of `doomed`
/// as the caller just read them (`read_doomed`): `set_aside`, then
/// `retire_never_landed`, then `retire_moved`. The rows left are deleted;
/// no row is removed without a judgement over its server copy.
fn judge_doomed(
    state: &crate::config::WorkspaceStateKey,
    anchor: &std::path::Path,
    project_dir: &std::path::Path,
    common: &mut crate::baseline::State,
    copies: &crate::baseline::Copies,
    doomed: Vec<String>,
) -> Result<Vec<String>> {
    let doomed = set_aside(state, anchor, project_dir, common, copies, doomed)?;
    let doomed = retire_never_landed(state, anchor, common, &copies.remote, doomed)?;
    retire_moved(state, anchor, common, &copies.remote, doomed)
}

/// `doomed` split by baseline.py's verdicts: the rows whose server copy
/// was read, the rows the server restructured, and the rows held this
/// round.
fn split_verdicts(
    doomed: Vec<String>,
    copies: &crate::baseline::Copies,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (mut read, mut restructured, mut held) = (Vec::new(), Vec::new(), Vec::new());
    for row in doomed {
        if copies.structural.contains_key(&row) {
            restructured.push(row);
        } else if copies.held.contains_key(&row) {
            held.push(row);
        } else {
            read.push(row);
        }
    }
    (read, restructured, held)
}

/// `doomed` without the rows baseline.py could not judge, none of which is
/// deleted: `retire_restructured` stops tracking the rows the server
/// restructured, and `hold_unreadable` keeps the claim of the rows that
/// cannot be read right now. Neither stops the sync.
fn set_aside(
    state: &crate::config::WorkspaceStateKey,
    anchor: &std::path::Path,
    project_dir: &std::path::Path,
    common: &mut crate::baseline::State,
    copies: &crate::baseline::Copies,
    doomed: Vec<String>,
) -> Result<Vec<String>> {
    let (doomed, restructured, held) = split_verdicts(doomed, copies);
    retire_restructured(state, anchor, common, copies, &restructured)?;
    hold_unreadable(copies, &held, |rel| {
        walk::as_written(project_dir, anchor, rel)
    });
    Ok(doomed)
}

/// The rows whose path the server restructured: a parent is no longer a
/// real directory, or the leaf is neither a regular file nor a symlink.
/// Such a row can never be judged, and ulak does not delete through a
/// path the server restructured, so it stays on the server and leaves
/// ulak's tracking: its claim is retired, its unconfirmed sends are
/// settled, and any return ends first, since a mark on an unclaimed name
/// would claim a later file. No return is marked, because no pull lands a
/// file through that path. One warning names them all, each with the
/// server's reason.
fn retire_restructured(
    state: &crate::config::WorkspaceStateKey,
    anchor: &std::path::Path,
    common: &mut crate::baseline::State,
    copies: &crate::baseline::Copies,
    restructured: &[String],
) -> Result<()> {
    if restructured.is_empty() {
        return Ok(());
    }
    common.end_return(restructured)?;
    common.settle(restructured)?;
    ledger::store(state, anchor, &[], restructured, &[]);
    ui::warn(&restructured_warning(copies, restructured));
    Ok(())
}

/// The warning `retire_restructured` gives, split from the printing for
/// the reason `slipped_past_the_ignores` is.
fn restructured_warning(copies: &crate::baseline::Copies, restructured: &[String]) -> String {
    let rows: Vec<String> = restructured
        .iter()
        .map(|p| format!("{p} ({})", copies.structural[p]))
        .collect();
    let them = if restructured.len() == 1 {
        "it"
    } else {
        "them"
    };
    format!(
        "the server changed the structure of the path to {}, so ulak left {them} there and stopped tracking {them}; your deletion was not applied",
        rows.join(", ")
    )
}

/// The rows baseline.py could not read this time: a file changing while
/// it was read, one the SSH user may not read, or any other error. Their
/// deletion waits and their claim stays, so they never enter the pull's
/// creating pass, and the next sync judges them again. Retiring them as
/// restructured instead turned a file caught mid-write into one ulak no
/// longer tracked, and a file the SSH user may not read into a
/// creating-pass candidate that failed every later pull.
fn hold_unreadable(
    copies: &crate::baseline::Copies,
    held: &[String],
    as_written: impl Fn(&str) -> String,
) {
    if let Some(warning) = held_warning(copies, held, as_written) {
        ui::warn(&warning);
    }
}

/// The warning `hold_unreadable` gives, split from the printing for the
/// reason `slipped_past_the_ignores` is. A file the SSH user may not read
/// is the server's own, and `[sync] protect` is how to say so; protected
/// rows are never doomed (`doomed_files`), so that ends the warning. The
/// entry is offered `as_written` — from the project directory, as
/// `ulak.toml` reads it — or a wider anchor's spelling would protect
/// nothing.
fn held_warning(
    copies: &crate::baseline::Copies,
    held: &[String],
    as_written: impl Fn(&str) -> String,
) -> Option<String> {
    if held.is_empty() {
        return None;
    }
    let rows: Vec<String> = held
        .iter()
        .map(|p| format!("{p} ({})", copies.held[p].why))
        .collect();
    let them = if held.len() == 1 { "it" } else { "them" };
    let mut warning = format!(
        "ulak could not read the server's copy of {} and left {them} there; your deletion waits, and the next sync tries again",
        rows.join(", ")
    );
    let denied: Vec<String> = held
        .iter()
        .filter(|p| copies.held[*p].denied)
        .map(|p| as_written(p))
        .collect();
    if !denied.is_empty() {
        warning.push_str(&format!(
            "; if the server owns {}, add {} to [sync] protect in ulak.toml",
            denied.join(", "),
            if denied.len() == 1 { "it" } else { "them" }
        ));
    }
    Some(warning)
}

/// `doomed_dirs` less every directory row above a row baseline.py could
/// not judge this round. What such a directory holds is not ulak's to
/// judge now, and `rmdir -p` resolves every component of the path it is
/// given: a row under a parent the server turned into a symlink removed
/// an empty directory from wherever the symlink pointed.
fn spare_dirs_above(doomed_dirs: Vec<String>, copies: &crate::baseline::Copies) -> Vec<String> {
    doomed_dirs
        .into_iter()
        .filter(|dir| {
            !copies
                .structural
                .keys()
                .chain(copies.held.keys())
                .any(|row| walk::protect_covers(dir, row))
        })
        .collect()
}

/// The Relaid answer, acted on in one place because two push paths ask
/// the question and getting it wrong is silent in both.
///
/// A re-layout removes and remakes the whole workspace directory, so
/// every row in the ledger names a file that is no longer up there —
/// including the ones this sync was about to delete. The rows do not
/// fall out on their own any more (`ledger::store` merges), and leaving
/// them would hand `pull_back`'s creating pass an exclude list covering
/// the server's own work: it would never come home, permanently. This
/// forget is also the only thing that keeps ONE ledger from holding two
/// anchors, after which `synced_files` — deliberately anchor-blind —
/// feeds `/parent`-relative rows to an rsync rooted at `/parent/repo`,
/// where they exclude nothing at all.
fn on_layout(
    layout: Layout,
    state: &crate::config::WorkspaceStateKey,
    doomed: &mut Vec<String>,
    doomed_dirs: &mut Vec<String>,
) {
    if layout != Layout::Relaid {
        return;
    }
    ledger::forget(state);
    doomed.clear();
    doomed_dirs.clear();
}

/// One push, and the claim it owes the ledger, in the one function that
/// does both.
///
/// Every byte that lands on the server has to be claimed in the same
/// breath, and the claim cannot be a separate step a caller might skip.
/// Two callers already skipped it. Doctor's pre-flight (`push_no_delete`)
/// pushed a whole workspace and told the ledger nothing — and since
/// `init` sends the user straight to `ulak doctor`, that was routinely
/// the FIRST population of a workspace. `run_sync` dropped the claim
/// whenever the removal round failed after the transfer, because its
/// only `store` sat past the `?`. Both are the same silent loss: an
/// unclaimed file can never be retired (nothing `doomed` can name it),
/// and `pull_back`'s creating pass is free to plant it back in the repo
/// after the user deletes it.
#[allow(clippy::too_many_arguments)]
fn push_and_claim(
    rsync_bin: &PathBuf,
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    survey: &mut Survey,
    state: &crate::config::WorkspaceStateKey,
    deadline: &crate::proc::Budget,
    channel: &mut Option<crate::stream::Client>,
) -> Result<Vec<Event>> {
    let (mut held, selected) = push_selection(
        project.config.sync.no_history,
        &survey.common.entries,
        &survey.local,
        &survey.remote,
    );
    say_what_no_history_sends(
        &selected,
        &survey.common.entries,
        &survey.local,
        &survey.remote,
    );
    for p in &held {
        if !crate::baseline::may_pull(
            survey.common.get(p),
            survey.local.get(p).and_then(Option::as_ref),
            survey.remote.get(p).and_then(Option::as_ref),
        ) {
            ui::warn(&format!(
                "sync conflict: {p} — both copies are preserved; make their contents agree to resume syncing this file"
            ));
        }
    }
    survey
        .common
        .record(crate::baseline::equal(&survey.local, &survey.remote))?;
    let mut confirmed = false;
    let pass = if selected.len() > EXACT_PASS_LIMIT {
        // The traversal judges by size and second, which cannot tell a
        // selected leaf from its same-size server copy; `--checksum` can,
        // and is owed only when some selected leaf HAS a server copy. A
        // first population has none, and the sender's checksum of every
        // file would be its only cost.
        let existing = selected
            .iter()
            .any(|p| survey.remote.get(p).is_some_and(Option::is_some));
        rsync_pass(
            rsync_bin,
            project,
            fp,
            ssh,
            &survey.plan.excludes,
            &held,
            if existing { &["--checksum"] } else { &[] },
            deadline,
        )?
    } else {
        // The survey said which leaves and which directories differ. Small
        // leaves under existing directories go over the channel in one
        // exchange, claimed first and recorded from the receipt. Otherwise
        // rsync creates a named directory with its mode and without
        // recursing, and the missing parents of a named file (pinned by
        // `a_directory_named_to_a_selected_pass_is_created`); nothing
        // selected means no rsync at all.
        let dirs = dir_selection(&survey.local_dirs, &survey.remote_dirs);
        let mut for_rsync = selected.clone();
        let mut streamed = None;
        if dirs.is_empty() && !selected.is_empty() {
            let mut batch = crate::stream::Batch::new();
            // Under the stamp the file carries now, not the survey's: the
            // batch records exactly the bytes it sends, so a save since the
            // survey goes as it is. Left to rsync, that save reached the
            // server as a version no record named, and every later sync
            // held the file as the server's edit.
            let fits = selected.iter().all(|p| {
                let create = !survey.remote.get(p).is_some_and(Option::is_some);
                let path = fp.anchor.join(p);
                crate::selection::Stamp::read(&path)
                    .is_some_and(|stamp| batch.add(&fp.anchor, &path, &stamp, create).is_some())
            });
            if fits {
                // Guarded on the server's copy AS SURVEYED, not on the
                // common record: `push_selection` has already applied the
                // record's rule (and `no_history`) to decide these may go
                // over the server's copy, so the exchange has only to
                // catch the server moving since. Guarding on the record
                // refused every no-history send as "differs" — measured:
                // a wiped runner's second job never reached the server.
                batch.guard(&survey.remote);
                let accepted = batch.contents();
                let written = batch.written();
                let already = ledger::synced_files(state);
                let fresh = fresh_claims(&written, |name| {
                    already.iter().any(|row| row.as_slice() == name.as_bytes())
                });
                // Claimed BEFORE the exchange, beside an unconfirmed record
                // of what is sent (`fresh_claims`, `unconfirmed_sends`).
                // The protective record first: a failed or interrupted
                // write must never leave a claim without it.
                let sends = unconfirmed_sends(&fresh, &survey.common, &accepted);
                survey.common.record_unconfirmed(sends.clone())?;
                ledger::store(state, &fp.anchor, &written, &[], &[]);
                let sent = channel_send(project, ssh, channel, batch);
                // A lost exchange — `Lost`, or its fence failing — may have
                // applied the replaces too (`attempted`); rsync, or the
                // error, comes after the record.
                if matches!(sent, Ok(Sent::Lost) | Err(_)) {
                    let replaced = attempted(&survey.common, &accepted)
                        .into_iter()
                        .filter(|(p, _)| !sends.contains_key(p))
                        .collect();
                    survey.common.record_unconfirmed(replaced)?;
                }
                match sent? {
                    Sent::Applied(applied) => streamed = Some((applied, accepted)),
                    Sent::Refused(refused) => {
                        // Nothing landed. A create's missing parent goes
                        // to rsync; the rest moved on the server since the
                        // survey and are held, their fresh claims retired.
                        let (rest, kept_back) = split_refusal(&selected, &refused);
                        let retire: Vec<String> = kept_back
                            .iter()
                            .map(|(p, _)| p.clone())
                            .filter(|p| fresh.contains(p))
                            .collect();
                        ledger::store(state, &fp.anchor, &[], &retire, &[]);
                        survey.common.settle(&retire)?;
                        for (p, reason) in &kept_back {
                            ui::warn(&format!(
                                "sync conflict: {p} — the server's copy of it {reason} since the survey; both copies are preserved and the next sync decides again"
                            ));
                            held.push(p.clone());
                        }
                        for_rsync = rest;
                    }
                    Sent::Unavailable | Sent::Lost => {}
                }
            }
        }
        match streamed {
            Some((applied, accepted)) => {
                survey.common.record(accepted.clone())?;
                survey.remote.extend(accepted);
                confirmed = true;
                Pass {
                    events: applied.changed.into_iter().map(Event::Push).collect(),
                    failed: None,
                }
            }
            None => {
                let mut names: Vec<Vec<u8>> =
                    for_rsync.iter().map(|p| p.as_bytes().to_vec()).collect();
                names.extend(dirs.into_iter().map(String::into_bytes));
                rsync_pass_dir(
                    rsync_bin,
                    project,
                    fp,
                    ssh,
                    &survey.plan.excludes,
                    &[],
                    &["--checksum"],
                    Direction::Up,
                    Some(&names),
                    deadline,
                )?
            }
        }
    };
    // Claimed BEFORE the failure is raised, and this order is the whole
    // point. A pass that exits 23 — one unreadable file in a tree that
    // otherwise arrived — has still put files on the server. Returning
    // the error first left every one of them with nobody claiming it:
    // `doomed` could never name them, so deleting one here left it up
    // there forever, and `pull_back`'s creating pass was free to plant
    // it back in the repo. That is the same hole `claim` was written to
    // close, one exit code over.
    let kept: Vec<_> = survey
        .plan
        .kept
        .iter()
        .filter(|p| !held.iter().any(|h| h.as_bytes() == p.as_slice()))
        .cloned()
        .collect();
    let claimed = claim(&kept, &survey.plan.kept_dirs, &pass.events);
    ledger::store(state, &fp.anchor, &claimed, &[], &[]);
    // A claim ends a return (module header). The claim made before the
    // exchange does not end one: a refusal retires that claim again, and
    // the name must then still be returning.
    survey.common.end_return(&claimed)?;
    let events = pass.or_fail()?;
    if !confirmed {
        survey.confirm(ssh, project, fp, &event_paths(&events), deadline)?;
    }
    Ok(events)
}

/// Whether a ledger-claimed file that is gone locally may still be
/// removed from the server: only a KNOWN and differing last-agreed content
/// says the server changed it since — container output the deletion
/// would destroy. With no record at all the ledger's claim stands on its
/// own, as it did before content records existed: a claimed file from an
/// older ulak, or one whose confirmation was lost to a crash or to a
/// delete that landed before it, must not become undeletable forever.
/// Measured: a file created and deleted within the confirmation round
/// trip left the service refusing the same deletion on every tick.
///
/// A server copy among ulak's own unconfirmed sends (`sent`, the evidence
/// `never_landed` reads) is not a change either: a lost receipt leaves
/// the record at the older content while the server holds the upload,
/// and calling that the server's edit would bring the deleted file back.
///
/// A row this names is not an error: the server's edit wins over the
/// deletion (`retire_moved`), and the pull brings that copy home.
fn moved_on_the_server(
    common: Option<&String>,
    sent: Option<&std::collections::BTreeSet<String>>,
    remote: Option<&String>,
) -> bool {
    matches!((common, remote), (Some(c), Some(r)) if c != r && !sent.is_some_and(|s| s.contains(r)))
}

/// `doomed` split by `moved_on_the_server`: the rows whose deletion
/// stands, and the rows the server changed since the last agreed content.
fn split_moved(
    doomed: Vec<String>,
    common: &crate::baseline::State,
    remote: &crate::baseline::Contents,
) -> (Vec<String>, Vec<String>) {
    doomed.into_iter().partition(|p| {
        !moved_on_the_server(
            common.get(p),
            common.unconfirmed(p),
            remote.get(p).and_then(Option::as_ref),
        )
    })
}

/// `doomed` without the rows the server changed since the last agreed
/// content. Their deletion is not applied: each is marked returning
/// before its claim is retired, like `retire_never_landed` retires one,
/// so the pull's creating pass takes the server's copy as server-born and
/// brings it home, and `claim_returned` claims it again once it has. Every
/// other deletion goes ahead. An error here would stop every later sync
/// the same way, until someone reconciled the file by hand on one end.
fn retire_moved(
    state: &crate::config::WorkspaceStateKey,
    anchor: &std::path::Path,
    common: &mut crate::baseline::State,
    remote: &crate::baseline::Contents,
    doomed: Vec<String>,
) -> Result<Vec<String>> {
    let (doomed, moved) = split_moved(doomed, common, remote);
    if !moved.is_empty() {
        common.mark_returning(&moved)?;
        ledger::store(state, anchor, &[], &moved, &[]);
        common.settle(&moved)?;
        for p in &moved {
            ui::warn(&format!(
                "{p} changed on the server after it was last synced, so your deletion was not applied; the server's copy comes back"
            ));
        }
    }
    Ok(doomed)
}

/// The returning files (`baseline::State::returning`) a creating pass
/// just landed. A returning name that did not land — held back, refused,
/// or recreated here meanwhile — stays returning for a later pull.
fn landed_back(common: &crate::baseline::State, created: &[Event]) -> Vec<String> {
    created
        .iter()
        .filter_map(|e| match e {
            Event::Push(p) if common.returning(p) => Some(p.clone()),
            _ => None,
        })
        .collect()
}

/// The returning files (`baseline::State::returning`) a pull's listing
/// shows the server no longer holds: `marked` before the listing was asked
/// for, inside what it covers — under one of its roots, outside what it
/// prunes and what the pull excludes — yet absent from it, and neither
/// claimed nor here. Nothing is left to come home under such a name. Its
/// mark left standing claimed the next same-named file the server made,
/// as it landed, and deleting that file here then removed the server's
/// own file from the server. `unclaimed` is the listing as
/// `selection::unclaimed` read it; an unclaimed, unexcluded name is in it
/// exactly when the server listed it.
fn returns_the_server_lost(
    marked: &std::collections::BTreeSet<String>,
    unclaimed: &[Vec<u8>],
    claimed: &[Vec<u8>],
    excluded: &crate::selection::Excludes,
    (roots, pruned): &(
        std::collections::BTreeSet<String>,
        std::collections::BTreeSet<String>,
    ),
    anchor: &std::path::Path,
) -> Vec<String> {
    let listed_under = |root: &str, name: &str| {
        root == "." || walk::protect_covers(root.strip_prefix("./").unwrap_or(root), name)
    };
    marked
        .iter()
        .filter(|name| {
            let raw = name.as_bytes();
            roots.iter().any(|root| listed_under(root, name))
                && !pruned.iter().any(|p| walk::protect_covers(p, name))
                && !excluded.contains(raw)
                && !unclaimed.iter().any(|u| u == raw)
                && !claimed.iter().any(|c| c == raw)
                && std::fs::symlink_metadata(anchor.join(name))
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        })
        .cloned()
        .collect()
}

/// Claim the returning files a pull landed and end their return. Without
/// the claim the file is server-born from then on: deleting it here a
/// second time dooms nothing, and every pull brings it back. The claim
/// comes first: an interruption between the two leaves a mark on a
/// claimed name, which no landing reads, where the other order leaves the
/// landed file unclaimed with no mark to claim it by.
fn claim_returned(
    state: &crate::config::WorkspaceStateKey,
    anchor: &std::path::Path,
    common: &mut crate::baseline::State,
    landed: &[String],
) -> Result<()> {
    if landed.is_empty() {
        return Ok(());
    }
    let rows: Vec<Vec<u8>> = landed.iter().map(|p| p.as_bytes().to_vec()).collect();
    ledger::store(state, anchor, &rows, &[], &[]);
    common.end_return(landed)
}

/// What a removal the server confirmed does to the common record: the
/// names go to None, as the channel's unlink records them
/// (`stream::Batch::contents`), which also settles their unconfirmed
/// sends and ends any return. Settling alone kept the last agreed
/// content of a file that no longer exists, and a later upload under the
/// name whose receipt was lost then read as the server's change
/// (`moved_on_the_server`).
fn forget_removed(common: &mut crate::baseline::State, removed: &[String]) -> Result<()> {
    let gone = removed
        .iter()
        .filter(|p| {
            common.get(p).is_some() || common.unconfirmed(p).is_some() || common.returning(p)
        })
        .map(|p| (p.clone(), None))
        .collect();
    common.record(gone)
}

/// What a dry run says of the doomed rows: those a real sync would
/// delete, and a line for each one it would keep, split by the same
/// judgements as `judge_doomed` without retiring anything — a dry run
/// writes nothing. Split from the printing for the reason
/// `slipped_past_the_ignores` is.
fn preview_deletions(
    doomed: Vec<String>,
    common: &crate::baseline::State,
    copies: &crate::baseline::Copies,
) -> (Vec<String>, Vec<String>) {
    let (doomed, restructured, held) = split_verdicts(doomed, copies);
    let (foreign, doomed) = split_never_landed(doomed, common, &copies.remote);
    let (doomed, moved) = split_moved(doomed, common, &copies.remote);
    let kept = restructured
        .iter()
        .map(|p| format!("kept: {p} (the server changed the structure of its path: {}; ulak leaves it there and stops tracking it)", copies.structural[p]))
        .chain(held.iter().map(|p| {
            format!("kept: {p} (the server's copy cannot be read right now: {}; the next sync tries again)", copies.held[p].why)
        }))
        .chain(foreign.iter().map(|p| {
            format!("kept: {p} (ulak's own send of it never landed; the server's file comes home)")
        }))
        .chain(moved.iter().map(|p| {
            format!("kept: {p} (changed on the server since it was last synced; the server's copy comes home)")
        }))
        .collect();
    (doomed, kept)
}

/// Whether a claimed row stands only on a send whose receipt was lost
/// (`baseline::State::unconfirmed`) and the server holds something else
/// under the name: the send never landed, the claim was never true, and
/// the file there is the server's own. Deleting it on the claim's word
/// would destroy server work; `retire_never_landed` retires the claim
/// instead and leaves the file for a pull to bring home.
fn never_landed(
    common: Option<&String>,
    sent: Option<&std::collections::BTreeSet<String>>,
    remote: Option<&String>,
) -> bool {
    common.is_none() && matches!((sent, remote), (Some(s), Some(r)) if !s.contains(r))
}

/// What a batch would put under the names it claims fresh, recorded as
/// unconfirmed BEFORE the exchange: a receipt lost to the network, a
/// fence that cannot reach the server, or a process killed mid-exchange
/// all leave the claim standing with the record beside it. A definite
/// answer settles it — a refusal at once, an applied batch by recording
/// the agreed content, an rsync fallback by the survey's confirmation.
///
/// A name counts when its claim is fresh or when an earlier send of it is
/// still unconfirmed: a retry whose receipt is lost again may be the copy
/// that arrived (measured — the second upload was judged
/// server-born and would have come back after a local delete).
fn unconfirmed_sends(
    fresh: &[String],
    common: &crate::baseline::State,
    accepted: &crate::baseline::Contents,
) -> std::collections::BTreeMap<String, String> {
    accepted
        .iter()
        .filter(|(p, _)| fresh.contains(p) || common.unconfirmed(p).is_some())
        .filter_map(|(p, c)| Some((p.clone(), c.clone()?)))
        .collect()
}

/// Every content a batch tried to put on the server, for the record a
/// LOST receipt leaves. `unconfirmed_sends` records fresh claims before
/// the exchange; a replace of a claimed leaf is recorded only here, after
/// the loss, so an acknowledged save writes no extra state. Without it
/// the record keeps the older content while the server may hold the
/// upload, and a local delete before the next survey reads that upload
/// as the server's change (`moved_on_the_server`) and brings it back.
///
/// Only a name with agreed content counts, the one case
/// `moved_on_the_server` reads the record for. A claim with no agreed
/// content stands on the ledger's word; recording a lost replace of it
/// made `never_landed` read the claim as a send that never arrived, keep
/// the server's copy and bring a deleted file back unclaimed.
fn attempted(
    common: &crate::baseline::State,
    accepted: &crate::baseline::Contents,
) -> std::collections::BTreeMap<String, String> {
    accepted
        .iter()
        .filter(|(p, _)| common.get(p).is_some())
        .filter_map(|(p, c)| Some((p.clone(), c.clone()?)))
        .collect()
}

/// `doomed` split by `never_landed`: the rows it names first, then the
/// rows whose deletion stands.
fn split_never_landed(
    doomed: Vec<String>,
    common: &crate::baseline::State,
    remote: &crate::baseline::Contents,
) -> (Vec<String>, Vec<String>) {
    doomed.into_iter().partition(|p| {
        never_landed(
            common.get(p),
            common.unconfirmed(p),
            remote.get(p).and_then(Option::as_ref),
        )
    })
}

/// `doomed` without the rows `never_landed` names, whose claims are
/// retired and whose unconfirmed sends are settled here. The file there
/// is the server's own, so it comes home unclaimed and any return under
/// the name ends.
fn retire_never_landed(
    state: &crate::config::WorkspaceStateKey,
    anchor: &std::path::Path,
    common: &mut crate::baseline::State,
    remote: &crate::baseline::Contents,
    doomed: Vec<String>,
) -> Result<Vec<String>> {
    let (foreign, doomed) = split_never_landed(doomed, common, remote);
    if !foreign.is_empty() {
        ledger::store(state, anchor, &[], &foreign, &[]);
        common.settle(&foreign)?;
        common.end_return(&foreign)?;
        for p in &foreign {
            ui::dim(&format!(
                "{p} stays on the server: ulak's own send of it never landed there"
            ));
        }
    }
    Ok(doomed)
}

/// The leaves a push sends over a differing server copy with no common
/// content on record — what `[sync] no_history = "send"` decided. Split
/// from the warning for the reason `slipped_past_the_ignores` is.
fn sent_without_history<'a>(
    names: impl IntoIterator<Item = &'a String>,
    base: &crate::baseline::Contents,
    local: &crate::baseline::Contents,
    remote: &crate::baseline::Contents,
) -> Vec<&'a String> {
    names
        .into_iter()
        .filter(|p| {
            base.get(*p).and_then(Option::as_ref).is_none()
                && local.get(*p).is_some_and(Option::is_some)
                && remote.get(*p).is_some_and(Option::is_some)
                && remote.get(*p) != local.get(*p)
        })
        .collect()
}

/// Said out loud, per file, on every path that sends: the server held
/// different bytes with no shared history, and this machine's copy is
/// going over them.
fn say_what_no_history_sends<'a>(
    names: impl IntoIterator<Item = &'a String>,
    base: &crate::baseline::Contents,
    local: &crate::baseline::Contents,
    remote: &crate::baseline::Contents,
) {
    for p in sent_without_history(names, base, local, remote) {
        ui::warn(&format!(
            "sync: {p} — no shared history; the local copy is sent over the server's (set [sync] no_history = \"hold\" to keep both instead)"
        ));
    }
}

fn content_conflict(detail: &str) -> anyhow::Error {
    fail!("sync preserved differing copies: {detail}")
        .now("compare the local and server files, preserve the desired edits on both sides, then run ulak sync")
        .into_err()
}

fn event_paths(events: &[Event]) -> Vec<Vec<u8>> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Push(p) if !p.ends_with('/') => Some(p.as_bytes().to_vec()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn remember_common(
    common: &mut crate::baseline::State,
    ssh: &Ssh,
    project: &Project,
    fp: &Footprint,
    names: &[Vec<u8>],
    deadline: &crate::proc::Budget,
) -> Result<()> {
    let remote = crate::baseline::remote(ssh, &project.remote_dir(), names, deadline)?;
    let local = crate::baseline::local(&fp.anchor, names)?;
    common.record(crate::baseline::equal(&local, &remote))
}

/// What the ledger must claim after a push: the walk's answer PLUS
/// everything rsync actually sent.
///
/// The two are not the same set, and the gap is a real hole. `plan.kept`
/// comes from a local walk that happens BEFORE the transfer; rsync then
/// scans the tree again when it runs, with a remote round-trip in
/// between. A file created inside that window travels to the server and
/// is missing from the walk's answer — so the ledger never claims it,
/// which the ledger reads as "born on the server". Deleting it locally
/// then leaves it up there FOREVER, silently.
///
/// Measured on my-server: a file saved while watch was waiting on the
/// per-workspace lock (a `ulak docker compose up` held it) landed on the server and
/// became undeletable. Claiming what actually travelled closes it.
///
/// Directories come from the WALK only, never from rsync's itemize
/// rows. The rows name what CHANGED in this transfer, and the ledger is
/// a snapshot of what is in the workspace — a directory that did not change
/// would drop out of the ledger on the next sync and look deleted. The
/// walk answers the snapshot question; the rows only ever add.
fn claim(kept: &[Vec<u8>], kept_dirs: &[Vec<u8>], events: &[Event]) -> Vec<Vec<u8>> {
    let mut all = kept.to_vec();
    all.extend(events.iter().filter_map(|e| match e {
        Event::Push(p) if !p.ends_with('/') => Some(p.clone().into_bytes()),
        _ => None,
    }));
    all.extend(kept_dirs.iter().map(|d| ledger::dir_row(d)));
    all.sort();
    all.dedup();
    all
}

/// The other half of the workspace: whatever the stack produced on the
/// server comes home. Never deletes anything local — under no
/// circumstance does ulak remove a file from the user's machine.
///
/// The same filter chain as the push, so protected data stays put and
/// excluded noise (a container's node_modules) comes down at most once,
/// before the walk can name it (`say_what_slipped_past_the_ignores`). What
/// the push does not need is the SPLIT: down here every remote file is
/// one of two things, and the two have opposite rights.
///
/// **A path the ledger claims** is one ulak put there. It may be
/// UPDATED here — the container rewrote a lockfile, a formatter went
/// over the source — but it may never be CREATED here, because on this
/// machine "missing" can only mean the user removed it.
///
/// **A path the ledger does not claim** was born on the server. It may
/// be created freely; bringing it home is the entire reason this leg
/// exists.
///
/// So the pull is two rsync passes, and that shape is the fix. Pass one
/// carries `--existing`: it refreshes what is here and creates nothing,
/// not even a directory (measured). Pass two carries
/// `--ignore-existing` and receives only remote leaves absent from the
/// ledger. Membership is evaluated by selection.rs, not an ordered rsync
/// filter list: a large ledger otherwise makes every remote candidate
/// visit thousands of exclusion rules. Only the creation pass uses
/// non-recursive `--files-from`; the existing-file pass traverses the
/// footprint with its ordinary filters (see the module header).
///
/// **Why it had to be structural.** This used to be one pass with a hold
/// list — the paths the ledger claims that are gone from disk, worked
/// out just before the transfer. That list cannot win the race and never
/// could: ulak answers "is it on disk?" when it builds the list, and
/// rsync answers it again when the file list finally arrives from the
/// server, hundreds of milliseconds later. A deletion landing in between
/// is invisible to the hold, so rsync finds the file missing here,
/// present there, and CREATES it. ulak deletes nothing local, so the
/// resurrected copy stays, the next walk syncs it, `doomed` can never
/// name it again — and the deletion is lost for good. Measured on
/// my-server: `e2e_service::phase5` failing 3 full-suite runs out of
/// 7, with the service's own log reading `1 pushed, 0 deleted,
/// 1 came back`. The same sentence sync.rs already wrote for empty
/// directories, now applied to files: a hold computed a moment before
/// the transfer cannot win that race, so the rule is structural instead.
/// Neither pass asks what is on disk right now. `--existing` puts that
/// question to rsync at the instant of transfer, and pass two takes its
/// candidate set from the ledger instead.
///
/// That second half is only safe under the per-workspace lock, and so is
/// the ledger write a pull makes itself (`claim_returned`). Every caller
/// holds it: `reconcile_once` and the service reconcile under it, and a
/// command that gave it up while it ran takes it again before `pull_back`
/// (`passthrough::run` after `exec`, `stop` and the like;
/// `runspec::pull_home` after a foreground run). Unlocked, a service tick
/// could be inside `reconcile_once` at that moment, rewriting the same
/// ledger file, and either writer's read-modify-write could lose the
/// other's rows. `nothing_reaches_the_ledger_without_the_workspace_lock`
/// pins it.
///
/// `--prune-empty-dirs` stays on both, for the directory half of the
/// same rule: a directory whose every file was filtered out does not
/// travel, so the shell of one the user renamed away cannot be planted
/// here, where nothing may delete it.
///
/// One hold list survives, and it is about a different failure.
/// **What you have just written.** rsync compares mtimes with ONE-SECOND
/// resolution, and `--update` only spares a file that is strictly newer
/// here. A save landing in the same second as the push is therefore a
/// tie — and rsync breaks a tie in the sender's favour, so the server's
/// older copy overwrites the edit still warm in your editor. Measured: a
/// rename-over save 200 ms after a push was silently reverted, and
/// because both sides then agreed, it stayed reverted.
///
/// ulak breaks that tie the other way, always. `since` is the moment
/// the reconcile began; anything modified at or after it is work the
/// server cannot possibly know about yet. A file the container rewrote
/// is untouched by this — its local copy is older than the reconcile —
/// so codegen still comes home. `None` means there is no such moment:
/// the passthrough pulls AFTER a command whose whole purpose was to
/// write, and a cutoff there would discard exactly what it went to get.
///
/// The cost is one stat per synced path, on inodes the walk has just
/// touched, plus the second pass: measured on a mid-sized workspace,
/// 0.66 s became 1.47 s.
#[allow(clippy::too_many_arguments)]
fn pull(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    quiet: bool,
    survey: &mut Survey,
    deadline: &crate::proc::Budget,
    since: Option<std::time::SystemTime>,
    channel: &mut Option<crate::stream::Client>,
) -> Result<SyncReport> {
    let rsync_bin = local_rsync()?;
    let state = project.state_key(&ssh.dest);
    // Every hold is lifted to the highest ancestor that is missing here.
    // `touched_since` names a file that existed when it was stat-ed, and
    // the directory holding it can be gone by the time rsync looks — a
    // rename lands in that window on every second run of the service
    // suite. Excluding just the file would let the footprint's
    // `+ dir/***` create the empty parent HERE.
    let mut held_back: Vec<String> = since
        .map(|since| touched_since(&fp.anchor, &survey.plan.kept, since))
        .unwrap_or_default()
        .iter()
        .map(|rel| hold_at_the_highest_gap(&fp.anchor, rel))
        .collect();
    let (held, selected) = pull_selection(&survey.common.entries, &survey.local, &survey.remote);
    held_back.extend(held);
    survey
        .common
        .record(crate::baseline::equal(&survey.local, &survey.remote))?;
    held_back.sort();
    held_back.dedup();

    // Pass one: refresh what is already here. `--existing` is the whole
    // guarantee — it creates nothing at all, so no deletion can be
    // undone by it, whenever the deletion happens to land. The survey
    // names the leaves the server rewrote; the tree is traversed only
    // when that list is long.
    let refresh = ["--checksum", "--update", "--existing", "--prune-empty-dirs"];
    let excluded = crate::selection::Excludes::new(
        survey
            .plan
            .excludes
            .iter()
            .cloned()
            .chain(held_back.iter().map(|p| p.as_bytes().to_vec())),
    );
    let names: Vec<Vec<u8>> = selected.iter().map(|p| p.as_bytes().to_vec()).collect();
    // Small leaves come home over the channel, landed only over the stamp
    // the survey read them under; the rest, or a refused batch, by rsync.
    let readable: Vec<&Vec<u8>> = names.iter().filter(|n| !excluded.contains(n)).collect();
    let mut streamed = None;
    if !readable.is_empty() && readable.len() <= 64 {
        let mut batch = crate::stream::Batch::new();
        if readable
            .iter()
            .all(|n| String::from_utf8(n.to_vec()).is_ok_and(|n| batch.read(n).is_some()))
            && let Sent::Applied(applied) = channel_send(project, ssh, channel, batch)?
        {
            let mut landed = Vec::new();
            let mut agreed = crate::baseline::Contents::new();
            for pulled in &applied.pulled {
                let Some(expected) = survey.cache.stamp_of(&pulled.path) else {
                    continue;
                };
                if crate::stream::land(&fp.anchor, pulled, Some(expected))?.is_some() {
                    landed.push(Event::Push(pulled.path.clone()));
                    agreed.insert(pulled.path.clone(), Some(pulled.content.clone()));
                }
            }
            survey.common.record(agreed.clone())?;
            survey.local.extend(agreed.clone());
            survey.remote.extend(agreed);
            streamed = Some(landed);
        }
    }
    let refreshed_by_rsync = streamed.is_none();
    let mut events = match streamed {
        Some(landed) => landed,
        None => rsync_pass_dir(
            &rsync_bin,
            project,
            fp,
            ssh,
            &survey.plan.excludes,
            &held_back,
            &refresh,
            Direction::Down,
            (selected.len() <= EXACT_PASS_LIMIT).then_some(names.as_slice()),
            deadline,
        )?
        .or_fail()?,
    };
    // Ownership is checked once per candidate, before rsync sees the list.
    // `--ignore-existing` still checks the receiving path at transfer time.
    let (listing, marked) = match survey.listing.take() {
        Some(listing) => (listing, std::mem::take(&mut survey.returning_when_listed)),
        None => {
            let marked = survey.common.returning_names();
            let listing = remote_files(project, fp, ssh, &survey.plan.excludes, deadline)?;
            (listing, marked)
        }
    };
    let claimed = ledger::synced_files(&state);
    let unclaimed = crate::selection::unclaimed(&listing, &claimed, &excluded)?;
    // Nearly always nothing is returning, and the scope stats every exclude.
    if !marked.is_empty() {
        let lost = returns_the_server_lost(
            &marked,
            &unclaimed,
            &claimed,
            &excluded,
            &inventory_scope(&project.sync_rules().protect, fp, &survey.plan.excludes),
            &fp.anchor,
        );
        survey.common.end_return(&lost)?;
    }
    // Server-born leaves come home the same way: read over the channel and
    // landed only into a name still absent here — `--ignore-existing`
    // decided at the instant of writing — or by rsync for what the
    // channel will not carry.
    let mut created = None;
    if !unclaimed.is_empty() && unclaimed.len() <= 64 {
        let mut batch = crate::stream::Batch::new();
        if unclaimed
            .iter()
            .all(|n| String::from_utf8(n.clone()).is_ok_and(|n| batch.read(n).is_some()))
            && let Sent::Applied(applied) = channel_send(project, ssh, channel, batch)?
        {
            let mut landed = Vec::new();
            let mut agreed = crate::baseline::Contents::new();
            for pulled in &applied.pulled {
                if crate::stream::land(&fp.anchor, pulled, None)?.is_some() {
                    landed.push(Event::Push(pulled.path.clone()));
                    agreed.insert(pulled.path.clone(), Some(pulled.content.clone()));
                }
            }
            survey.common.record(agreed.clone())?;
            survey.local.extend(agreed.clone());
            survey.remote.extend(agreed);
            created = Some(landed);
        }
    }
    let created_by_rsync = created.is_none();
    let invented = match created {
        Some(landed) => landed,
        None => rsync_pass_dir(
            &rsync_bin,
            project,
            fp,
            ssh,
            &survey.plan.excludes,
            &held_back,
            &["--ignore-existing", "--prune-empty-dirs"],
            Direction::Down,
            Some(&unclaimed),
            deadline,
        )?
        .or_fail()?,
    };
    say_what_slipped_past_the_ignores(project, fp, &invented);
    // The channel's receipts already said what landed; only what rsync
    // moved is read again on both ends.
    let mut confirm = Vec::new();
    if created_by_rsync {
        confirm.extend(event_paths(&invented));
    }
    if refreshed_by_rsync {
        confirm.extend(event_paths(&events));
    }
    let landed = landed_back(&survey.common, &invented);
    events.extend(invented);
    survey.confirm(ssh, project, fp, &confirm, deadline)?;
    // Claimed again only now, beside the content just recorded for them.
    // A confirmation that fails leaves them returning: the next push
    // claims what is still here, and the next pull what is not.
    claim_returned(&state, &fp.anchor, &mut survey.common, &landed)?;

    let mut report = SyncReport::default();
    for e in &events {
        // Directory rows are not files the stack produced; counting them
        // made "2 file(s) came back" out of one file and its parent.
        if let Event::Push(p) = e
            && !p.ends_with('/')
        {
            report.pulled += 1;
            if !quiet {
                ui::dim(&format!("↓ {p}"));
            }
        }
    }
    if report.pulled > 0 && !quiet {
        // The files are ours from now on: they are on disk, so the next
        // walk syncs them and the next ledger claims them.
        ui::dim(&format!(
            "{} file(s) the stack produced came back from the server",
            report.pulled
        ));
    }
    Ok(report)
}

/// The standalone way down, for a caller that has no survey in hand: the
/// passthrough pulls after a command whose whole purpose was to write.
pub fn pull_back(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    quiet: bool,
    deadline: &crate::proc::Budget,
    since: Option<std::time::SystemTime>,
) -> Result<SyncReport> {
    let plan = checked_plan(project, fp)?;
    let mut survey = Survey::take(project, fp, ssh, plan, true, deadline)?;
    let report = pull(
        project,
        fp,
        ssh,
        quiet,
        &mut survey,
        deadline,
        since,
        &mut None,
    )?;
    survey.finish()?;
    Ok(report)
}

/// What the stack made on the server that this project's ignore rules
/// cover — said out loud on the one trip those rules cannot stop.
///
/// The ignore list is built by WALKING the local tree, so it is a list
/// of real paths (`walk.rs`). A path born on the server and not here
/// yet cannot be on it, so pass two brings that path home, once. From
/// the second run on it IS here, the walk names it, and the exclude at
/// rank 3 of `filter_args` outranks the footprint's `+` rules at rank
/// 4: it never travels again, in either direction.
///
/// The ranking is not the bug and does not move — it is what stops an
/// excluded `./data` from being pushed over a live database, which is
/// the worse failure by a distance. The bug is that the one trip it
/// does make is silent, and it is the trip that matters: a container's
/// data directory lands on the developer's disk without being asked
/// for, and the answer to that is `[sync] protect`, which nobody
/// reaches for because nobody was told.
///
/// Said exactly once per path by construction: pass two carries
/// `--ignore-existing`, so a path that has come home is one it will
/// never create again.
///
/// `doctor::ignored_entries` puts the same question to the footprint's
/// own entries and answers it in the report. This one puts it to what
/// just came down the wire.
fn say_what_slipped_past_the_ignores(project: &Project, fp: &Footprint, invented: &[Event]) {
    let slipped = slipped_past_the_ignores(&project.sync_rules(), fp, invented);
    if slipped.is_empty() {
        return;
    }
    // Named the way ulak.toml would name them, since that is where the
    // advice below sends them.
    let written: Vec<String> = slipped
        .iter()
        .map(|p| walk::as_written(&project.inv.project_dir, &fp.anchor, p))
        .collect();
    ui::warn(&format!(
        "the stack made {} on the server and it came home, though this project's ignore rules cover it — nothing here could exclude a path that did not exist yet",
        written.join(", ")
    ));
    ui::dim("    to keep it on the server from now on: add it to [sync] protect in ulak.toml");
}

/// The paths the warning above names, outermost only. Split out for the
/// reason `runspec::sources_left_on_the_server` is: a test of a function
/// that returns nothing and writes to a terminal can do no more than
/// rebuild the rule by hand and then agree with its own copy of it.
fn slipped_past_the_ignores(
    cfg: &crate::config::SyncCfg,
    fp: &Footprint,
    invented: &[Event],
) -> Vec<String> {
    if invented.is_empty() {
        return Vec::new();
    }
    // Read from disk, so not built until there is something to judge.
    let build = fp.build_filter();
    let mut slipped: Vec<String> = invented
        .iter()
        .filter_map(|e| match e {
            Event::Push(p) => Some(p.as_str()),
            Event::Delete(_) => None,
        })
        // A directory row and its files say one thing; the directory is
        // the one worth protecting, and trimming the slash is what lets
        // the two collapse onto each other below.
        .map(|p| p.trim_end_matches('/'))
        .filter(|p| walk::is_ignored(&fp.anchor, &fp.anchor.join(p), cfg, &build).unwrap_or(false))
        .map(str::to_string)
        .collect();
    slipped.sort();
    slipped.dedup();
    // Outermost only: naming `data/` and then every file under it is one
    // sentence said a hundred times, and `[sync] protect` wants the
    // directory anyway.
    let outermost = slipped.clone();
    slipped.retain(|p| {
        !outermost
            .iter()
            .any(|o| o != p && p.starts_with(&format!("{o}/")))
    });
    slipped
}

/// Synced paths you have written at or after `since`.
///
/// The one-second slack is not caution, it is the tie itself: mtimes are
/// whole seconds on the wire, so a file stamped in the same second the
/// reconcile started cannot be ordered against the server's copy. Ties
/// go to the local file, every time — ulak does not destroy work on
/// this machine, and an editor buffer is precisely the work a user
/// notices losing.
fn touched_since(
    anchor: &std::path::Path,
    kept: &[Vec<u8>],
    since: std::time::SystemTime,
) -> Vec<String> {
    let cutoff = since
        .checked_sub(std::time::Duration::from_secs(1))
        .unwrap_or(since);
    kept.iter()
        .filter_map(|rel| std::str::from_utf8(rel).ok())
        .filter(|rel| {
            std::fs::symlink_metadata(anchor.join(rel))
                .and_then(|m| m.modified())
                .is_ok_and(|m| m >= cutoff)
        })
        .map(str::to_string)
        .collect()
}

/// What to hold back, expressed so rsync cannot sneak the parent in.
///
/// Excluding `site/gone/inner.txt` still lets the footprint's
/// `+ /site/***` create an EMPTY `site/gone/` here — and an empty
/// directory is then pushed straight back up, recreating on the server
/// exactly what the deletion was removing. Measured: a renamed directory
/// came home as an empty shell and ping-ponged with the server forever,
/// because each side kept restoring it for the other.
///
/// So a path whose parent is not here either is held back at the highest
/// ancestor that is missing. Everything that reaches here came from the
/// walk of THIS machine, so a directory the container invented is
/// untouched by it — that one is the creating pass's business, and the
/// pass has its own rule for empty shells (`--prune-empty-dirs`).
fn hold_at_the_highest_gap(anchor: &std::path::Path, rel: &str) -> String {
    let mut held = rel.to_string();
    let mut cursor = std::path::Path::new(rel);
    while let Some(parent) = cursor.parent() {
        if parent.as_os_str().is_empty() || anchor.join(parent).exists() {
            break;
        }
        // The trailing slash is what makes rsync refuse to descend.
        held = format!("{}/", parent.display());
        cursor = parent;
    }
    held
}

/// Remove exactly the doomed paths — an explicit `rm` rather than
/// `rsync --delete`, so what gets deleted is a decision ulak made and
/// can show you, not a side effect of what rsync happened to find.
/// Every path here is UTF-8 and newline-free (walk.rs guarantees it), so
/// one line of the script's answer names exactly one of them — see
/// `removal_script` for how the paths get there, which is the part a
/// filename was once able to interfere with.
///
/// Two passes, because a directory is not a file. Files go first, so a
/// directory emptied by this very call can then go; directories are
/// removed with `rmdir` and NEVER `rm -r` — one the container has since
/// filled is no longer the empty shell ulak left behind, and the
/// refusal is the correct answer, not a failure. `-p` then takes the
/// ancestors it can, which is what retires a whole removed subtree.
///
/// The count is files and directories together: both are removals the
/// user is entitled to see in the summary.
///
/// The script also names what it could NOT remove, and that half is what
/// the ledger reads. `rmdir` refusing a directory the container has since
/// filled is the correct answer, not a failure — but the row has to stay
/// claimed for it, or the next sync has no licence to try again and the
/// directory stands on the server for good. Same for the paths the safety
/// `case` skips: nothing happened to them, so nothing may be un-claimed.
fn delete_remote(
    ssh: &Ssh,
    project: &Project,
    doomed: &[String],
    doomed_dirs: &[String],
) -> Result<Removal> {
    let script = removal_script(&project.remote_dir(), doomed, doomed_dirs);
    let out = ssh.run_checked(&script, "removing what you deleted")?;
    Ok(parse_removal(
        &String::from_utf8_lossy(&out.stdout),
        doomed,
        doomed_dirs,
    ))
}

/// The removal round as a script no FILENAME can escape.
///
/// The two lists used to travel as the bodies of quoted heredocs, and a
/// file named exactly `ULAK_DOOMED` — a legal name walk.rs syncs without
/// a murmur, and one that sorts ahead of most rows — ended the heredoc
/// early. Every remaining doomed path was then read by the remote shell
/// as a COMMAND, with the workspace as its cwd: the deletions after it
/// silently did not happen (they retry forever, and nothing says so),
/// and a row spelling `scripts/deploy.sh` ran. There is no terminator to
/// hit now — the paths arrive as single-quoted positional parameters,
/// which the shell reads as data whatever they say.
///
/// The `case` guard stays for what quoting does not answer: a row that
/// names something OUTSIDE the workspace. It rejects only a `..` PATH
/// COMPONENT, never the substring — `v1..v2.patch` and `2024..2025.csv`
/// are legal names, and a substring test left them doomed on every sync
/// forever while the summary claimed the workspace was clean.
///
/// `linked` keeps a row when any directory on its way is now a symlink,
/// checked right before its `rm -f` or `rmdir -p`: both resolve every
/// component, so a row under a parent the server replaced with a symlink
/// reached through it, and `rmdir -p` removed an empty directory from
/// wherever it pointed. The judgement already leaves such rows alone
/// (`spare_dirs_above`); this holds even when the server changes the path
/// after the judgement, or a directory row outlives the file row that
/// revealed the symlink.
fn removal_script(remote_dir: &str, doomed: &[String], doomed_dirs: &[String]) -> String {
    // "" and /* keep the rm inside the workspace; the four `..` shapes
    // are the traversal, spelled as components so a name that merely
    // CONTAINS two dots is not caught with them.
    const OUTSIDE: &str = "\"\"|/*|..|../*|*/../*|*/..";
    let mut script = format!("cd {} || exit 1\nn=0\n", sh_quote(remote_dir));
    // Every proper prefix of $1, deepest first; the row's own last
    // component is left out, since `rm -f` removes a symlink leaf itself.
    script.push_str(
        "linked() {\n\
         q=$1\n\
         while :; do\n\
         case $q in */*) q=${q%/*} ;; *) return 1 ;; esac\n\
         [ -L \"$q\" ] && return 0\n\
         done\n\
         }\n",
    );
    script.push_str(&set_args(doomed));
    script.push_str(&format!(
        "for p in \"$@\"; do\n\
         case \"$p\" in {OUTSIDE}) printf 'ULAK_KEPT_FILE %s\\n' \"$p\"; continue ;; esac\n\
         if linked \"$p\"; then printf 'ULAK_KEPT_FILE %s\\n' \"$p\"; continue; fi\n\
         if rm -f -- \"$p\"; then n=$((n+1)); else printf 'ULAK_KEPT_FILE %s\\n' \"$p\"; fi\n\
         d=${{p%/*}}\n\
         [ \"$d\" != \"$p\" ] && rmdir -p -- \"$d\" 2>/dev/null\n\
         done\n"
    ));
    script.push_str(&set_args(doomed_dirs));
    // A directory the file loop's `rmdir -p` already took is GONE, not
    // kept — so "still a directory" is the test, never the exit code.
    script.push_str(&format!(
        "for p in \"$@\"; do\n\
         case \"$p\" in {OUTSIDE}) printf 'ULAK_KEPT_DIR %s\\n' \"$p\"; continue ;; esac\n\
         if linked \"$p\"; then printf 'ULAK_KEPT_DIR %s\\n' \"$p\"; continue; fi\n\
         if rmdir -p -- \"$p\" 2>/dev/null; then n=$((n+1))\n\
         elif [ -d \"$p\" ]; then printf 'ULAK_KEPT_DIR %s\\n' \"$p\"\n\
         fi\n\
         done\n"
    ));
    script.push_str("printf 'DELETED %s\\n' \"$n\"\n");
    script
}

/// One list, as the shell's positional parameters — one path per line so
/// a workspace retiring ten thousand rows does not arrive as a single
/// megabyte-long line.
fn set_args(paths: &[String]) -> String {
    let mut out = String::from("set --");
    for p in paths {
        out.push_str(" \\\n");
        out.push_str(&quoted_row(p));
    }
    out.push('\n');
    out
}

/// A doomed row as one shell word, with no exceptions.
///
/// `ssh::sh_quote` deliberately leaves a leading `~/` UNQUOTED, because
/// `-v ~/data:/app` is how a user names the server's home and the tilde
/// has to survive to the far shell. A doomed row is a path inside the
/// workspace and must never expand to anything else: a directory really
/// may be named `~`, and that hole would aim the `rm` at the remote home
/// directory instead. Quoted whole, it cannot.
fn quoted_row(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

/// What a removal round actually achieved.
struct Removal {
    /// Files and directories together, for the summary line.
    count: usize,
    /// The doomed FILES the server really lost.
    files: Vec<String>,
    /// The doomed DIRECTORIES the server really lost.
    dirs: Vec<String>,
    /// Doomed FILES the server still holds: listed for deletion, not
    /// applied. `status` and `doctor` read this number, so reporting it
    /// as zero is the workspace quietly drifting while both say it is
    /// clean. Directories are deliberately not counted — a `rmdir` the
    /// container's own work refused is the right answer, not a deletion
    /// anyone is waiting for.
    kept_files: usize,
}

/// Read the script's answer: the count, and — by subtraction from what it
/// says it KEPT — the paths that really went. Every ledger row is
/// newline-free (walk.rs), so a marker plus the path is one unambiguous
/// line.
///
/// The trailing `DELETED` line is the receipt, and without it nothing is
/// un-claimed. Subtracting from a truncated answer would read silence as
/// "all of them went" and hand the workspace a set of files with nobody
/// claiming them — the failure this whole module is here to prevent, in
/// the one place that has real licence to remove a row.
fn parse_removal(stdout: &str, doomed: &[String], doomed_dirs: &[String]) -> Removal {
    let mut finished = None;
    let mut kept_files: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut kept_dirs: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("ULAK_KEPT_FILE ") {
            kept_files.insert(rest);
        } else if let Some(rest) = line.strip_prefix("ULAK_KEPT_DIR ") {
            kept_dirs.insert(rest);
        } else if let Some(rest) = line.strip_prefix("DELETED ") {
            finished = Some(rest.trim().parse().unwrap_or(0));
        }
    }
    let Some(count) = finished else {
        return Removal {
            count: 0,
            files: Vec::new(),
            dirs: Vec::new(),
            // No receipt is no confirmation, so every doomed file is
            // still up there as far as anyone here knows.
            kept_files: doomed.len(),
        };
    };
    let files: Vec<String> = doomed
        .iter()
        .filter(|p| !kept_files.contains(p.as_str()))
        .cloned()
        .collect();
    Removal {
        count,
        kept_files: doomed.len() - files.len(),
        files,
        dirs: doomed_dirs
            .iter()
            .filter(|d| !kept_dirs.contains(d.as_str()))
            .cloned()
            .collect(),
    }
}

/// Doctor's pre-flight push: transfers content, NEVER deletes anything.
/// The bool reports whether server-side deletions are pending.
///
/// This one takes the workspace lock ITSELF, which no other engine entry
/// point does. Its caller is a whole-program pre-flight rather than a
/// sync — `doctor::run` holds no lock at any point — and the two things
/// that happen here are a ledger wipe (`on_layout`) and a ledger write.
/// Run against a service mid-reconcile, the wipe lands under it and the
/// service's own `store` then re-reads an empty file and writes back
/// only its own footprint, leaving everything else on the server
/// unclaimed: undeletable, and fair game for the pull's creating pass.
///
/// So a CALLER must not hold the lock as well. `WorkspaceLock` is an
/// OS file lock taken on a fresh handle every time, and a second handle
/// in the same process blocks against the first — the deadlock would be
/// this function waiting on its own caller. The guard test
/// `nothing_reaches_the_ledger_without_the_workspace_lock` refuses both
/// halves of that: this lock going missing, and a caller adding one.
pub fn push_no_delete(project: &Project, fp: &Footprint, ssh: &Ssh) -> Result<(SyncReport, bool)> {
    let rsync_bin = local_rsync()?;
    let plan = checked_plan(project, fp)?;
    let id = project.workspace_id();
    let state = project.state_key(&ssh.dest);
    let _lock = crate::lockfile::WorkspaceLock::acquire(id)?;
    // Same reason as in `run_sync`: the directory those rows describe is
    // gone, so keeping them would report deletions that cannot happen
    // and hide the server's own work from the pull.
    on_layout(
        ensure_workspace(ssh, project)?,
        &state,
        &mut Vec::new(),
        &mut Vec::new(),
    );
    let deadline = crate::proc::Budget::new(crate::proc::RECONCILE);
    // Doctor's push is a real push: files land on the server, and until
    // this claimed them the ledger said the workspace was empty. `init`
    // points the user at `ulak doctor`, so that was the ordinary first
    // population of a workspace — every file of it unclaimed, hence
    // undeletable, hence free to come back after the user deleted it.
    let mut survey = Survey::take(project, fp, ssh, plan, false, &deadline)?;
    let events = push_and_claim(
        &rsync_bin,
        project,
        fp,
        ssh,
        &mut survey,
        &state,
        &deadline,
        &mut None,
    )?;
    survey.finish()?;
    let pending = !doomed_files(project, &state, &fp.anchor, &survey.plan.kept).is_empty();
    let report = render_events(&events, true);
    Ok((report, pending))
}

/// Ignore rules, expanded to literal paths, for the footprint's
/// directories only — walking the anchor itself is exactly the scan the
/// footprint model exists to avoid.
fn checked_plan(project: &Project, fp: &Footprint) -> Result<walk::WalkReport> {
    let mut plan = walk::WalkReport::default();
    let rules = project.sync_rules();
    // Narrows each build context by its own `.dockerignore`, and nothing
    // else — see dockerignore.rs. It lands in the SAME literal exclude
    // list as `[sync] exclude`, which sync.rs already ranks ahead of the
    // footprint's includes, so no filter rule has to change.
    let build = fp.build_filter();
    for dir in fp.sync_dirs() {
        let part = walk::plan_in(&fp.anchor, &dir, &rules, &build)?;
        plan.excludes.extend(part.excludes);
        plan.bad_names.extend(part.bad_names);
        plan.kept.extend(part.kept);
        plan.kept_dirs.extend(part.kept_dirs);
    }
    // Individually-named files (the compose files, env files, a config)
    // are synced by their own filter rule, not by any directory walk —
    // so without this they would never enter the ledger, and deleting one
    // locally would leave it on the server FOREVER. Measured: a removed
    // compose.override.yaml kept overriding the stack.
    let excluded = crate::selection::Excludes::new(plan.excludes.iter().cloned());
    for e in &fp.entries {
        if e.is_dir || !e.exists {
            continue;
        }
        if let Some(rel) = crate::invocation::anchor_rel(&fp.anchor, &e.local) {
            // Every one of them EXCEPT a protected one, and this loop is
            // the only door such a row could come through: `walk_dir`
            // never enters a protect path, so a claim on server-owned
            // data can only be made here.
            //
            // What it cost: `protect` means never pushed AND never
            // deleted, so the file on the server is the only copy. A
            // claim on it is a promise ulak can keep in exactly one
            // direction — the rank-2 filter rule stops the push
            // (measured against rsync 3.4.4 with ulak's own argv order:
            // only the unprotected file travelled), but nothing stops
            // the DELETE. Once the stale local copy went, `doomed`
            // filters on `still_on_disk` alone, the row became doomed,
            // and `rm -f -- 'data/config.json'` ran against the server's
            // only copy. One file is under the default `max_delete` of
            // 25, so there was no prompt either.
            if crate::walk::protected(&rules.protect, &rel) {
                continue;
            }
            // Nor one an exclude keeps local, such as an `env_file:` in a
            // mounted folder the user excluded: rsync's literal exclude
            // refuses it and the pull's inventory never lists it, so a
            // claim only the channel honoured sent it by one transport and
            // not the other. Doctor names it (`ignored_entries`).
            if excluded.contains(rel.as_bytes()) {
                continue;
            }
            plan.kept.push(rel.into_bytes());
        }
    }
    plan.kept.sort();
    plan.kept.dedup();
    plan.kept_dirs.sort();
    plan.kept_dirs.dedup();
    refuse_bad_names(&plan)?;
    Ok(plan)
}

fn refuse_bad_names(plan: &walk::WalkReport) -> Result<()> {
    if plan.bad_names.is_empty() {
        return Ok(());
    }
    let sample = plan.bad_names.iter().take(5).cloned().collect::<Vec<_>>();
    Err(fail!(
        "{} file name(s) cannot be synced safely (newlines or non-UTF8 bytes in the name): {}",
        plan.bad_names.len(),
        sample.join(", ")
    )
    .now("rename those files, or add them to [sync] exclude in ulak.toml")
    .into_err())
}

/// The walker's answer for the directories the notices name, spliced into
/// the last full plan. The walker stays the only selection policy: each
/// scope is walked with the same matchers, entered as the walk from the
/// anchor entered it (`walk::plan_in`), and every previous entry under it
/// is replaced by that walk. A notice names a leaf, whose parent is the
/// scope, or a directory the previous plan already kept — some watcher
/// backends report the directory rather than its changed child — which is
/// rescanned itself. A directory the plan does not know is reported
/// through its parent, whose rescan either excludes it (rsync's own
/// `.ulak-partial` staging, a builtin the literal list cannot name before
/// it exists) or lists it, after which its leaves have no known parent and
/// `reconcile_structure` sends them to the full path. A leaf whose parent
/// is gone is reported through its nearest surviving ancestor: rsync's
/// staging directory vanishes with its temporary before the notice is
/// read, and a kept directory that really went is missing from that
/// rescan, which the ledger's doomed directories then send to the full
/// path. A leaf under a directory the walk excludes — by the literal list
/// it produced, or by the rules themselves for a directory born since, such
/// as rsync's staging — is not walked at all. A full walk and stamp of a
/// large tree on every structural change cost far more than rescanning the
/// one directory a notice names. Pinned by
/// `a_rescanned_directory_answers_as_the_full_walk_would`.
fn rescanned_plan(
    cfg: &crate::config::SyncCfg,
    fp: &Footprint,
    previous: &walk::WalkReport,
    paths: &std::collections::BTreeSet<PathBuf>,
) -> Result<Option<(walk::WalkReport, Vec<PathBuf>)>> {
    let sync_dirs = fp.sync_dirs();
    let build = fp.build_filter();
    let excluded = crate::selection::Excludes::new(previous.excludes.iter().cloned());
    let known = |dir: &std::path::Path| {
        sync_dirs.iter().any(|d| d == dir)
            || crate::invocation::anchor_rel(&fp.anchor, dir)
                .is_some_and(|rel| previous.kept_dirs.contains(&rel.into_bytes()))
    };
    let mut scopes = std::collections::BTreeSet::new();
    for path in paths {
        let scope = if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) && known(path) {
            path.as_path()
        } else {
            let mut parent = path.parent();
            while let Some(dir) = parent
                && !dir.is_dir()
            {
                parent = dir.parent();
            }
            let Some(parent) = parent.filter(|p| p.starts_with(&fp.anchor)) else {
                return Ok(None);
            };
            parent
        };
        if !sync_dirs.iter().any(|d| scope.starts_with(d)) {
            continue;
        }
        let Some(rel) = crate::invocation::anchor_rel(&fp.anchor, scope) else {
            return Ok(None);
        };
        if rel != "." && excluded.contains(rel.as_bytes()) {
            continue;
        }
        // A directory the previous walk never saw is judged by the rules
        // it would have applied; a kept one was judged then, include
        // rescues and all. One an include reaches into is not ignored, and
        // `plan_in` enters it as the walk from the anchor did.
        if !known(scope) && walk::is_ignored(&fp.anchor, scope, cfg, &build)? {
            continue;
        }
        scopes.insert(scope.to_path_buf());
    }
    let scopes: Vec<PathBuf> = scopes
        .iter()
        .filter(|s| !scopes.iter().any(|o| o != *s && s.starts_with(o)))
        .cloned()
        .collect();
    let within = |raw: &Vec<u8>| {
        std::str::from_utf8(raw).is_ok_and(|r| {
            let p = fp.anchor.join(r);
            scopes.iter().any(|s| p.starts_with(s))
        })
    };
    let mut plan = walk::WalkReport {
        excludes: previous
            .excludes
            .iter()
            .filter(|r| !within(r))
            .cloned()
            .collect(),
        bad_names: Vec::new(),
        kept: previous
            .kept
            .iter()
            .filter(|r| !within(r))
            .cloned()
            .collect(),
        kept_dirs: previous
            .kept_dirs
            .iter()
            .filter(|r| !within(r))
            .cloned()
            .collect(),
    };
    for scope in &scopes {
        let part = walk::plan_in(&fp.anchor, scope, cfg, &build)?;
        plan.excludes.extend(part.excludes);
        plan.bad_names.extend(part.bad_names);
        plan.kept.extend(part.kept);
        plan.kept_dirs.extend(part.kept_dirs);
        // The walker lists a directory's children, never the directory
        // itself; a scope the previous plan kept stays kept.
        if let Some(rel) = crate::invocation::anchor_rel(&fp.anchor, scope)
            && previous.kept_dirs.contains(&rel.as_bytes().to_vec())
        {
            plan.kept_dirs.push(rel.into_bytes());
        }
    }
    plan.kept.sort();
    plan.kept.dedup();
    plan.kept_dirs.sort();
    plan.kept_dirs.dedup();
    plan.excludes.sort();
    plan.excludes.dedup();
    refuse_bad_names(&plan)?;
    Ok(Some((plan, scopes)))
}

/// Enumerate only footprint roots, without following symlinks. `find`
/// supplies names, never ownership or deletion decisions. The byte index
/// in selection.rs makes those decisions locally. Directory exclusions
/// are pruned remotely so a protected database or ignored dependency tree
/// is not scanned merely to discover that none of it may come home.
fn remote_files(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    excludes: &[Vec<u8>],
    deadline: &crate::proc::Budget,
) -> Result<Vec<u8>> {
    let (cmd, script) = inventory_command(project, fp, ssh, excludes);
    inventory_run(cmd, script, ssh.dest.clone(), deadline)
}

/// The same listing, started beside the survey's hashing (see `Survey`).
fn inventory_in_background(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    excludes: &[Vec<u8>],
    deadline: &crate::proc::Budget,
) -> std::thread::JoinHandle<Result<Vec<u8>>> {
    let (cmd, script) = inventory_command(project, fp, ssh, excludes);
    let dest = ssh.dest.clone();
    let limit = deadline.remaining().min(crate::proc::SCRIPT);
    std::thread::spawn(move || inventory_run(cmd, script, dest, &crate::proc::Budget::new(limit)))
}

fn inventory_command(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    excludes: &[Vec<u8>],
) -> (Command, String) {
    let script = inventory_script(
        &project.remote_dir(),
        &project.sync_rules().protect,
        fp,
        excludes,
    );
    let mut cmd = ssh.command();
    cmd.arg("--").arg(&ssh.dest).arg("sh -s");
    (cmd, script)
}

fn inventory_run(
    mut cmd: Command,
    script: String,
    dest: String,
    deadline: &crate::proc::Budget,
) -> Result<Vec<u8>> {
    let out = crate::proc::run_bounded(
        &mut cmd,
        Some(script.into_bytes()),
        deadline.remaining().min(crate::proc::SCRIPT),
    )?;
    crate::audit::record_command("ssh", &cmd, out.status.code());
    if out.timed_out || !out.status.success() {
        return Err(fail!(
            "cannot list sync candidates on {}: {}",
            dest,
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .now(format!(
            "check the connection: ssh {} true",
            sh_quote(&dest)
        ))
        .now("rerun ulak sync when the server is available")
        .into_err());
    }
    Ok(out.stdout)
}

fn inventory_script(
    remote_dir: &str,
    protect: &[String],
    fp: &Footprint,
    excludes: &[Vec<u8>],
) -> String {
    let (roots, pruned) = inventory_scope(protect, fp, excludes);
    // find -path has glob syntax even when its argv word is shell-quoted.
    let literal = |p: &str| -> String {
        p.chars()
            .flat_map(|c| {
                if "\\*?[]".contains(c) {
                    vec!['\\', c]
                } else {
                    vec![c]
                }
            })
            .collect()
    };
    let mut prune = "-name .ulak-partial".to_string();
    for p in pruned {
        prune.push_str(&format!(
            " -o -path {}",
            sh_quote(&literal(&format!("./{p}")))
        ));
    }
    let mut script = format!("cd {} || exit\n", sh_quote(remote_dir));
    for root in roots {
        let root = sh_quote(&root);
        script.push_str(&format!("if [ -e {root} ] || [ -L {root} ]; then find {root} \\( {prune} \\) -prune -o ! -type d -print0 || exit; fi\n"));
    }
    script
}

fn inventory_scope(
    protect: &[String],
    fp: &Footprint,
    excludes: &[Vec<u8>],
) -> (
    std::collections::BTreeSet<String>,
    std::collections::BTreeSet<String>,
) {
    use std::collections::BTreeSet;
    let mut roots: BTreeSet<String> = fp
        .sync_dirs()
        .iter()
        .filter_map(|p| crate::invocation::anchor_rel(&fp.anchor, p))
        .map(|p| {
            if p == "." {
                ".".to_string()
            } else {
                format!("./{p}")
            }
        })
        .collect();
    if fp.whole_anchor {
        roots.insert(".".to_string());
    }
    for e in &fp.entries {
        if !e.is_dir
            && e.exists
            && let Some(p) = crate::invocation::anchor_rel(&fp.anchor, &e.local)
        {
            roots.insert(format!("./{p}"));
        }
    }
    if roots.contains(".") {
        roots = BTreeSet::from([".".to_string()]);
    }
    let mut pruned: BTreeSet<String> = protect
        .iter()
        .filter_map(|p| walk::protect_entry(p))
        .collect();
    for p in excludes {
        if let Ok(p) = std::str::from_utf8(p)
            && fp.anchor.join(p).is_dir()
        {
            pruned.insert(p.to_string());
        }
    }
    roots.retain(|root| {
        !pruned
            .iter()
            .any(|p| walk::protect_covers(p, root.strip_prefix("./").unwrap_or(root)))
    });
    (roots, pruned)
}

fn confirm_deletions(doomed: &[String], budget: crate::config::DeleteBudget) -> Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(false);
    }
    ui::warn(&format!(
        "this sync wants to delete {} files on the server (budget: {budget})",
        doomed.len()
    ));
    for p in doomed.iter().take(20) {
        ui::dim(&format!("- {p}"));
    }
    if doomed.len() > 20 {
        ui::dim(&format!("… and {} more", doomed.len() - 20));
    }
    // Esc/Ctrl-C means "no" — the caller's budget error then explains
    // every way forward; an abort must not lose that guidance.
    match inquire::Confirm::new("delete these files on the server?")
        .with_default(false)
        .prompt()
    {
        Ok(answer) => Ok(answer),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(false),
        Err(e) => Err(e).context("confirmation prompt failed"),
    }
}

// ─── rsync invocation ───────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Event {
    Push(String),
    Delete(String),
}

/// Which way the bytes move. The filter chain is identical both ways —
/// protected data and ignored noise are as unwelcome coming down as
/// going up.
#[derive(Clone, Copy, PartialEq)]
enum Direction {
    Up,
    Down,
}

#[allow(clippy::too_many_arguments)]
fn rsync_pass(
    rsync_bin: &PathBuf,
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    excludes: &[Vec<u8>],
    held_back: &[String],
    extra: &[&str],
    deadline: &crate::proc::Budget,
) -> Result<Pass> {
    // Differing remote edits are held before footprint includes, just as
    // unsent local edits are held on the return trip.
    rsync_pass_dir(
        rsync_bin,
        project,
        fp,
        ssh,
        excludes,
        held_back,
        extra,
        Direction::Up,
        None,
        deadline,
    )
}

/// The filter chain, emitted in the order rsync reads it. rsync takes
/// the FIRST match, so this order is not a style choice — it is the
/// design, and it is built in one place so it can be asserted as a list.
fn filter_args(protect: &[String], held_back: &[String], footprint: Vec<String>) -> Vec<String> {
    // 0. rsync's own half-written blobs. rsync adds this exclude ITSELF
    // — but at the END of the filter list, where the footprint's
    // `+ dir/***` has already won. Measured: 8.1 MB of a half-sent
    // 30 MB file came home into the user's repo on the next pull_back.
    // First rank is what makes that impossible, in both directions.
    let mut args = vec!["--filter=- .ulak-partial/".to_string()];
    // 1. held back — see `pull_back`. Empty on the way up; on the way
    // down these are the paths that must not come home at all.
    for t in held_back {
        args.push(format!("--filter=- {}", walk::rsync_filter_pattern(t)));
    }
    // 2. protect
    for p in protect.iter().filter_map(|p| walk::protect_entry(p)) {
        let p = p.as_str();
        // P = never delete on the receiver; - = never send. The path is
        // escaped: a literal "[" or "*" in a protect entry must not
        // become a glob and silently void the protection.
        let pattern = walk::rsync_filter_pattern(p);
        args.push(format!("--filter=P {pattern}"));
        args.push(format!("--filter=- {pattern}"));
    }
    // 3. ignores (fed on stdin, so the flag's POSITION is the rule order)
    args.push("--exclude-from=-".to_string());
    // 4. + 5. footprint, then the wall
    args.extend(footprint.into_iter().map(|r| format!("--filter={r}")));
    args
}

#[allow(clippy::too_many_arguments)]
fn rsync_pass_dir(
    rsync_bin: &PathBuf,
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    excludes: &[Vec<u8>],
    held_back: &[String],
    extra: &[&str],
    dir: Direction,
    selected: Option<&[Vec<u8>]>,
    deadline: &crate::proc::Budget,
) -> Result<Pass> {
    let mut cmd = Command::new(rsync_bin);
    cmd.args([
        "--recursive",
        "--links",
        "--perms",
        "--times",
        "--compress",
        "--delay-updates",
        "--partial-dir=.ulak-partial",
        "--itemize-changes",
        "--from0",
        // I/O deadline for the transfer itself. NOT --contimeout: that
        // one only governs an rsync:// daemon connection and does
        // exactly nothing over an ssh transport. Measured: on a dead
        // link ulak's own rsync argv sat for 241 seconds and never
        // came back on its own.
        "--timeout=60",
    ]);
    let excluded = crate::selection::Excludes::new(
        excludes
            .iter()
            .cloned()
            .chain(held_back.iter().map(|p| p.as_bytes().to_vec())),
    );
    let selected = selected.map(|paths| {
        paths
            .iter()
            .filter(|p| !excluded.contains(p))
            .collect::<Vec<_>>()
    });
    if selected.as_ref().is_some_and(Vec::is_empty) {
        return Ok(Pass {
            events: Vec::new(),
            failed: None,
        });
    }
    let mut filters = filter_args(
        &project.sync_rules().protect,
        if selected.is_some() { &[] } else { held_back },
        fp.filter_rules(),
    );
    if selected.is_some() {
        // Exact leaves only. Recursing a selected directory would reopen
        // every ignored child that the walker already refused.
        cmd.args(["--no-recursive", "--files-from=-", "--ignore-missing-args"]);
        filters.retain(|arg| arg != "--exclude-from=-");
    }
    cmd.args(filters);
    cmd.args(extra);
    cmd.arg("-e").arg(ssh.rsync_transport());
    let local = format!("{}/", fp.anchor.display());
    let remote = format!("{}:{}/", ssh.dest, project.remote_dir());
    cmd.arg("--");
    match dir {
        Direction::Up => cmd.arg(&local).arg(&remote),
        Direction::Down => cmd.arg(&remote).arg(&local),
    };
    // The NUL-delimited literal exclude list travels on stdin, so the
    // flag's POSITION above is what fixes its rank in the filter chain.
    let payload: Vec<u8> = match selected {
        Some(paths) => paths
            .iter()
            .flat_map(|p| p.iter().copied().chain([0]))
            .collect(),
        None => excludes
            .iter()
            .flat_map(|e| {
                let mut v = walk::rsync_exclude_pattern(e);
                v.push(0);
                v
            })
            .collect(),
    };
    let out = crate::proc::run_bounded(&mut cmd, Some(payload), deadline.remaining())?;
    crate::audit::record_command("rsync", &cmd, out.status.code());
    if out.timed_out {
        let sync = crate::management::root_command(project, &["sync"]);
        return Err(fail!(
            "the sync to {} ran past its {}-minute budget and was stopped",
            ssh.dest,
            crate::proc::RECONCILE.as_secs() / 60
        )
        .now(format!(
            "check the link: ssh -o ConnectTimeout=10 {} true",
            ssh.dest
        ))
        .now(format!(
            "nothing was deleted; rerun when the connection is back: {sync}"
        ))
        .into_err());
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let events = parse_itemized(&stdout);

    // The rows come back WITH the verdict rather than instead of it.
    // rsync exits non-zero for reasons that still moved bytes — 23 is
    // "partial transfer due to error", which is one unreadable file in a
    // tree that otherwise arrived — and the caller that claims has to
    // see what landed before it is handed the failure. See
    // `push_and_claim`.
    let failed = match out.status.code() {
        Some(0) => None,
        // 24: files vanished mid-transfer (normal while editing)
        Some(24) => {
            ui::warn(
                "some files vanished while syncing (edited mid-flight) — resync will catch up",
            );
            None
        }
        code => {
            let sync = crate::management::root_command(project, &["sync"]);
            let doctor = crate::management::root_command(project, &["doctor"]);
            Some(map_rsync_failure(
                code,
                &String::from_utf8_lossy(&out.stderr),
                ssh,
                &sync,
                &doctor,
            ))
        }
    };
    Ok(Pass { events, failed })
}

/// Offer these exact files again, judged by their contents this time.
///
/// `--checksum` and not `--ignore-times`, which closes the hole just as
/// well: under `--ignore-times` every offered path is itemized as sent
/// whether anything moved or not, so an offer that found nothing counts
/// as a change. `--checksum` itemizes only the differing file, as
/// `>fc.......`. Same cost over a list this short.
///
/// A second rsync rather than a flag on the first: the flag cannot apply
/// to some paths only, and stdin is taken — the main pass reads its
/// exclude list there, this one its file list.
///
/// No filter chain and no `--recursive`. Every path came out of
/// `plan.kept`, already narrowed by the footprint, the ignores and the
/// build contexts, with `protect` paths never in it.
fn offer_again(
    rsync_bin: &PathBuf,
    project: &Project,
    ssh: &Ssh,
    fp: &Footprint,
    paths: &[String],
    deadline: &crate::proc::Budget,
) -> Result<Pass> {
    let mut cmd = Command::new(rsync_bin);
    cmd.args([
        "--links",
        "--perms",
        "--times",
        "--compress",
        "--delay-updates",
        "--partial-dir=.ulak-partial",
        "--itemize-changes",
        "--from0",
        "--timeout=60",
        "--checksum",
        "--files-from=-",
    ]);
    cmd.arg("-e").arg(ssh.rsync_transport());
    cmd.arg("--");
    cmd.arg(format!("{}/", fp.anchor.display()));
    cmd.arg(format!("{}:{}/", ssh.dest, project.remote_dir()));
    let payload: Vec<u8> = paths
        .iter()
        .flat_map(|p| {
            let mut v = p.clone().into_bytes();
            v.push(0);
            v
        })
        .collect();
    let out = crate::proc::run_bounded(&mut cmd, Some(payload), deadline.remaining())?;
    crate::audit::record_command("rsync", &cmd, out.status.code());
    if out.timed_out {
        let sync = crate::management::root_command(project, &["sync"]);
        return Err(fail!(
            "the sync to {} ran past its {}-minute budget and was stopped",
            ssh.dest,
            crate::proc::RECONCILE.as_secs() / 60
        )
        .now(format!(
            "nothing was deleted; rerun when the connection is back: {sync}"
        ))
        .into_err());
    }
    let events = parse_itemized(&String::from_utf8_lossy(&out.stdout));
    let failed = match out.status.code() {
        // 24 is a file that vanished between the list and the transfer,
        // which for a list this short means the save was undone while we
        // were offering it. Nothing is owed.
        Some(0) | Some(24) => None,
        code => {
            let sync = crate::management::root_command(project, &["sync"]);
            let doctor = crate::management::root_command(project, &["doctor"]);
            Some(map_rsync_failure(
                code,
                &String::from_utf8_lossy(&out.stderr),
                ssh,
                &sync,
                &doctor,
            ))
        }
    };
    Ok(Pass { events, failed })
}

/// What one rsync pass achieved, and — separately — whether it ended
/// badly. The two are not alternatives: a pass can move most of a tree
/// and still exit non-zero.
struct Pass {
    events: Vec<Event>,
    failed: Option<anyhow::Error>,
}

impl Pass {
    /// The rows, for a caller that has nothing to record and only needs
    /// the failure raised.
    fn or_fail(self) -> Result<Vec<Event>> {
        match self.failed {
            Some(e) => Err(e),
            None => Ok(self.events),
        }
    }
}

/// The %i itemize field is exactly 11 chars (protocol >= 30, guaranteed
/// by the rsync >= 3.2 floor) plus one separator space — fixed-width
/// splitting keeps names with leading spaces intact.
fn parse_itemized(stdout: &str) -> Vec<Event> {
    let mut events = Vec::new();
    for line in stdout.lines() {
        let (Some(flags), Some(path)) = (line.get(..12), line.get(12..)) else {
            continue;
        };
        if path.is_empty() || !flags.ends_with(' ') {
            continue;
        }
        if flags.starts_with("*deleting") {
            events.push(Event::Delete(path.trim_end_matches('/').to_string()));
            continue;
        }
        let f = flags.as_bytes();
        // '<' SENT to the remote — ulak always pushes, so this is the
        // common case and missing it made every sync report "0 changes"
        // (and watch look asleep). '>' received, 'c' created locally on
        // the receiver (dirs, symlinks), 'h' hard-linked.
        if matches!(f[0], b'<' | b'>' | b'c' | b'h') {
            // Symlink lines read "name -> target"; keep just the name.
            let path = if f[1] == b'L' {
                path.split(" -> ").next().unwrap_or(path)
            } else {
                path
            };
            events.push(Event::Push(path.to_string()));
        }
    }
    events
}

/// List what moved. The SUMMARY is deliberately separate: deletions are
/// decided after the push (from the ledger), so summarising here would
/// announce "0 deletions" and then print the deletions underneath it.
fn render_events(events: &[Event], quiet: bool) -> SyncReport {
    let mut report = SyncReport::default();
    for e in events {
        match e {
            Event::Push(p) => {
                report.pushed += 1;
                if !quiet {
                    ui::dim(&format!("+ {p}"));
                }
            }
            Event::Delete(p) => {
                report.deleted += 1;
                if !quiet {
                    ui::dim(&format!("- {p}"));
                }
            }
        }
    }
    report
}

fn summarize(report: &SyncReport, quiet: bool, dry_run: bool, budget: crate::config::DeleteBudget) {
    if quiet {
        return;
    }
    let verb = if dry_run { "would sync" } else { "synced" };
    let mut line = format!(
        "{verb} {} change(s), {} deletion(s)",
        report.pushed, report.deleted
    );
    if report.pulled > 0 {
        line.push_str(&format!(", {} from the server", report.pulled));
    }
    ui::ok(&line);
    if let Some(note) = budget_note(budget) {
        ui::dim(&note);
    }
}

/// Lifting the guard is a decision, and a decision nobody is shown is one
/// nobody can review — the whole point of naming it rather than writing a
/// number large enough never to be hit. A budget that still holds says
/// nothing: it is the default, and every sync reporting it would be noise.
fn budget_note(budget: crate::config::DeleteBudget) -> Option<String> {
    budget
        .is_unlimited()
        .then(|| format!("deletion budget: {}", crate::config::UNLIMITED))
}

/// What `ensure_workspace` found on the server, as far as the ledger is
/// concerned. A re-layout empties the workspace directory, and a claim
/// about a file that is no longer there is worse than no claim at all —
/// it is what the pull would refuse to bring home.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Layout {
    /// The workspace on the server is the one we left there.
    Kept,
    /// The anchor moved, so the directory was removed and remade empty.
    Relaid,
}

/// The same preparation `run_sync` does first, for a command that has
/// nothing to push but is still going to touch the tree on the server.
///
/// `run_sync` was the only way in, so a run that named no local path
/// skipped it — and with it the one check that says the workspace
/// directory is ours. `docker run --cidfile x.cid` then went straight
/// to `mkdir -p` and `rm -f` inside a directory no manifest had been
/// read from.
pub fn claim_workspace(ssh: &Ssh, project: &Project) -> Result<()> {
    ensure_workspace(ssh, project)?;
    Ok(())
}

/// Create the 0700 workspace tree AND settle the identity contract in one
/// round: write our manifest if none exists, then read whichever one is
/// there and verify it belongs to us before anything mutates.
fn ensure_workspace(ssh: &Ssh, project: &Project) -> Result<Layout> {
    let remote_dir = project.remote_dir();
    let base = ".ulak/workspaces";
    let namespace_dir = project.remote_namespace_root();
    let workspace_dir = project.remote_workspace_root();
    let manifest_path = project.remote_manifest_path();
    let id = project.workspace_id();
    let state = project.state_key(&ssh.dest);
    // The UUID this machine already minted, or one it would mint. It is
    // NOT persisted yet: which of the two is ours depends on what the
    // server turns out to be holding, and that is only known below.
    // A declared identity stands in for this machine's memory. It is not
    // a bypass: `verify_owner` below still refuses a workspace belonging
    // to another project. It only answers the one question the server
    // cannot — "is that one mine?" — which otherwise stops a second CI
    // run dead, since the fresh state directory has no claim to compare.
    let declared = crate::config::declared_workspace_uuid()?;
    let remembered = crate::invocation::recorded_uuid(id);
    let identity = declared_identity(declared.as_deref(), remembered.as_deref());
    if let Some(fresh) = identity.record {
        crate::invocation::record_uuid(id, fresh);
    }
    let recorded = identity.known;
    let mine = match recorded {
        Some(u) => u.to_string(),
        None => crate::hashid::uuid_v4()?,
    };
    let candidate = crate::manifest::Manifest::candidate(project, &ssh.dest, mine.clone())?;
    let candidate_json = serde_json::to_string(&candidate).context("cannot serialize manifest")?;

    // No `--` separator: busybox tools lack it, and these paths are ours
    // (always ".ulak/workspaces/..." — never dash-led).
    // The previous transfer leases are retired in this same round trip:
    // every full push passes through here, and rsync must never write
    // behind a helper that is still applying an earlier batch (stream.rs).
    let script = format!(
        "umask 077 && mkdir -p {dir} && chmod 700 {b} {ns} {ws} {dir} && \
         if [ -f {m} ]; then cat {m}; else printf '%s' {json} > {m} && cat {m}; fi && {fence}",
        dir = sh_quote(&remote_dir),
        b = sh_quote(base),
        ns = sh_quote(&namespace_dir),
        ws = sh_quote(&workspace_dir),
        m = sh_quote(&manifest_path),
        json = sh_quote(&candidate_json),
        fence = crate::stream::fence_script(&workspace_dir),
    );
    let out = ssh.run_checked(&script, "preparing the workspace")?;
    let clean = crate::management::root_command(project, &["clean"]);
    let sync = crate::management::root_command(project, &["sync"]);
    let existing = crate::manifest::parse(&out.stdout, &clean, &sync)?;
    existing.verify_owner(&candidate, &clean, &sync)?;
    if recorded.is_none() {
        // Read BEFORE mark_synced below, or every first sync would look
        // like an upgrade of a workspace this machine already owned.
        let ours_before = crate::invocation::ever_synced(&state);
        let settled = match settle_uuid(&mine, &existing.uuid, ours_before) {
            Owner::Ours => mine.clone(),
            Owner::Adopt => existing.uuid.clone(),
            Owner::Ambiguous => adopt_or_refuse(&existing, &ssh.dest, &mine, id, project)?,
        };
        crate::invocation::record_uuid(id, &settled);
    }
    let mut layout = Layout::Kept;
    if !existing.anchor.is_empty() && existing.anchor != candidate.anchor {
        // Anchors only ever widen — `invocation::workspace_anchor` is
        // monotone — so a server anchor that CONTAINS this one is never
        // a layout this invocation should be making. It means the record
        // it widened against was not current, and there are two ways to
        // get there: the local state directory is gone (so there was
        // nothing to widen against), or another ulak settled a wider
        // anchor in between — the record is read before the workspace
        // lock is taken and written under it, so two commands started
        // together both read the old one.
        //
        // Relaying out here would `rm -rf` the wider layout the other
        // side had just pushed, `protect` and all, or — with no terminal
        // to ask in — refuse in a way that reads like the workspace is
        // broken. Neither is true: the server is right and this
        // invocation's paths are simply computed against the wrong base.
        // Writing the server's anchor down is the whole repair; the same
        // command run again widens to it and agrees.
        if server_holds_the_wider_anchor(&existing.anchor, &candidate.anchor) {
            crate::invocation::record_anchor(id, std::path::Path::new(&existing.anchor));
            return Err(anchor_moved_under_us(&existing.anchor));
        }
        relayout(ssh, project, &existing.anchor, &candidate, &candidate_json)?;
        layout = Layout::Relaid;
    }
    // What the server is laid out under, now that this round has settled
    // it. Read back by `invocation::workspace_anchor`, so the next
    // command — which may compute a NARROWER anchor for the same
    // workspace — widens to this one instead of asking for a relayout
    // that would undo this one's.
    crate::invocation::record_anchor(id, &project.anchor);
    crate::invocation::mark_synced(&state);
    Ok(layout)
}

/// What a declaration means for this run: the uuid to proceed under, and
/// the claim this machine still has to write down.
#[derive(Debug, PartialEq, Eq)]
struct Identity<'a> {
    /// `None` is a workspace this machine can name no uuid for — one has
    /// to be minted and then settled against the server.
    known: Option<&'a str>,
    /// `None` is nothing to write: the state directory already says this.
    record: Option<&'a str>,
}

/// The declaration is this machine's memory when it has none, and it is
/// written down as well as used — a runner whose state DOES survive then
/// keeps reaching the same workspace once the variable is taken back out
/// of the pipeline.
///
/// Writing it down again unchanged is not free: `record_uuid` truncates
/// before it writes, so every needless rewrite is a window in which
/// another ulak reads no claim at all — and a claim read as absent is
/// what sends a machine to the ownership question below.
fn declared_identity<'a>(declared: Option<&'a str>, recorded: Option<&'a str>) -> Identity<'a> {
    let Some(declared) = declared else {
        return Identity {
            known: recorded,
            record: None,
        };
    };
    Identity {
        known: Some(declared),
        record: (recorded != Some(declared)).then_some(declared),
    }
}

/// Who the workspace on the server belongs to, for a machine that has no
/// record of it.
#[derive(Debug, PartialEq)]
enum Owner {
    /// The server carries the uuid we just proposed: we created this
    /// workspace moments ago.
    Ours,
    /// The server's manifest predates the local record and this machine
    /// HAS synced this workspace before — a v0.3 workspace of our own. Adopt
    /// it rather than declaring our own project foreign to itself.
    Adopt,
    /// The server carries a stranger's uuid and this machine has never
    /// synced the workspace. Nothing on the server can settle this: two
    /// machines sharing a username and a checkout path produce an
    /// identical identity AND an identical local_root, which is exactly
    /// why `verify_owner` waves them both through. Only a human knows.
    Ambiguous,
}

/// `synced_before` is the only discriminator that exists here, and it is
/// not always present — a machine whose state directory was reset has
/// lost it while its workspace on the server lives on. That case used to
/// resolve silently to "not mine", which locked the workspace away behind a
/// message whose only way out was `ulak clean`, i.e. destroying it.
/// Measured on a real project: a v0.3 workspace this very machine had
/// created was declared foreign, and the honest answer — "that is mine,
/// take it back" — did not exist anywhere in the product.
fn settle_uuid(mine: &str, on_server: &str, synced_before: bool) -> Owner {
    if on_server == mine {
        Owner::Ours
    } else if synced_before {
        Owner::Adopt
    } else {
        Owner::Ambiguous
    }
}

/// Ask the human standing here, or stop and say why.
///
/// The decision is recorded once and forever after, so it must never be
/// made by a process nobody is watching: a service with no terminal that
/// guessed "not mine" would settle the question before its owner ever
/// saw it — which is precisely how a workspace ends up unreachable except
/// by deleting it.
fn adopt_or_refuse(
    existing: &crate::manifest::Manifest,
    dest: &str,
    mine: &str,
    id: &str,
    project: &Project,
) -> Result<String> {
    use std::io::IsTerminal;
    let sync = crate::management::root_command(project, &["sync"]);
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(fail!(
            "the workspace for this project already exists on {dest}, created by an Ulak installation that left no record on this machine"
        )
        .now(format!(
            "run this in a terminal, where it can ask you once: {sync}"
        ))
        .into_err());
    }
    ui::warn(&format!(
        "this project already has a workspace on {dest}, and this machine has no record of creating it"
    ));
    ui::dim(&format!(
        "it says it was made for {} — that is either THIS machine before its state was reset, or a second machine using the same checkout path",
        existing.local_root
    ));
    ui::dim("adopting means this machine maintains it; refusing means Ulak leaves it alone");
    if ui::confirm("Is that workspace yours? Adopt it")? {
        Ok(existing.uuid.clone())
    } else {
        ui::warn("left alone — Ulak will not sync or tunnel that workspace from here");
        if let Some(claim) = crate::intent::workspace_dir_path(id).map(|d| d.join("uuid")) {
            ui::dim(&format!(
                "to change your mind: rm {}   then: {sync}",
                claim.display(),
            ));
        }
        Ok(mine.to_string())
    }
}

/// The footprint reached outside its old base, so the anchor moved and
/// every synced path now sits at a different depth. No silent move.
/// Doing it quietly would leave a full shadow copy of the old layout
/// behind — the workspace would double in size and `compose` would keep
/// reading the stale half.
fn relayout(
    ssh: &Ssh,
    project: &Project,
    old_anchor: &str,
    candidate: &crate::manifest::Manifest,
    candidate_json: &str,
) -> Result<()> {
    ui::warn(&format!(
        "this project now reaches outside its old workspace base:\n  was: {old_anchor}\n  now: {}",
        candidate.anchor
    ));
    ui::dim("the server copy has to be laid out again — local files are not touched");
    // Which was the whole of what this prompt used to say, and alone it
    // is misleading in the one direction that costs something. The
    // `rm -rf` below empties the entire remote workspace, and `protect`
    // names precisely the paths that live there and NOWHERE else: a
    // database directory the container writes and ulak deliberately
    // never pushes, never deletes and never pulls home. "Local files are
    // not touched" is true of those too, and useless — there is no local
    // copy to be touched. Naming them is the difference between a
    // question somebody can answer and one they can only agree to.
    // Where the server holds them now: under the layout being replaced.
    let protect = walk::rebase(
        &project.config.sync,
        &project.inv.project_dir,
        std::path::Path::new(old_anchor),
    )
    .protect;
    if !protect.is_empty() {
        ui::warn(&format!(
            "server-owned data is emptied with it, and nothing here holds a copy: {}",
            protect.join(", ")
        ));
        ui::dim(&format!(
            "copy it off first if you need it, e.g.: scp -r {}:{}/{} .",
            ssh.dest,
            project.remote_dir_shown(),
            protect[0]
        ));
    }
    if !ui::confirm("re-lay out the workspace on the server?")? {
        let sync = crate::management::root_command(project, &["sync"]);
        return Err(relayout_refused(&sync));
    }
    let script = relayout_script(
        &project.remote_dir(),
        &project.remote_manifest_path(),
        candidate_json,
    );
    ssh.run_checked(&script, "re-laying out the workspace")?;
    ui::ok("workspace re-laid out — the next sync fills it");
    Ok(())
}

/// Saying no to the re-layout stops the sync, and stopping is only half
/// an answer.
///
/// Whoever refuses is standing at a terminal with a question they did
/// not expect, and both ways out are real: run it again and say yes, or
/// keep the old layout by making the reference relative to the project
/// again. Without the second one, "no" reads as a dead end and the only
/// way anybody finds out otherwise is by reading this file.
/// Does the server's anchor CONTAIN the one this invocation computed?
///
/// Split out so the decision can be tested without a server, the same
/// reason `invocation::widen` is split from the read around it. Both are
/// ancestors of the same project root — the workspace id hashes that
/// root, so one anchor cannot be a stranger to the other — which is what
/// makes "contains" the only question worth asking.
fn server_holds_the_wider_anchor(server: &str, ours: &str) -> bool {
    !server.is_empty() && std::path::Path::new(ours).starts_with(std::path::Path::new(server))
}

/// The server is laid out under a wider base than this command worked
/// out for itself, so its paths would land beside the tree instead of
/// in it. Nothing is broken and nothing needs re-laying out.
fn anchor_moved_under_us(server_anchor: &str) -> anyhow::Error {
    fail!(
        "this workspace is laid out under {server_anchor} on the server, which is wider than this command worked out on its own"
    )
    .now("run the same command again — that base is written down now, and the next run uses it")
    .into_err()
}

fn relayout_refused(sync: &str) -> anyhow::Error {
    fail!(
        "sync stopped: the workspace still uses the old layout, so the new reference would be missing on the server"
    )
    .now(format!("rerun and confirm: {sync}"))
    .now("or keep the old layout by making the reference relative to the project again")
    .into_err()
}

/// The one `rm -rf` ulak ever aims at a workspace, built where it can be
/// read as a whole and asserted on. It empties the workspace DIRECTORY
/// and immediately writes the new manifest back, so a link that dies
/// mid-round leaves a workspace whose manifest still describes it —
/// never an anchor-less directory the next run would have to guess at.
fn relayout_script(remote_dir: &str, manifest_path: &str, candidate_json: &str) -> String {
    format!(
        "umask 077 && rm -rf {dir} && mkdir -p {dir} && chmod 700 {dir} && printf '%s' {json} > {m}",
        dir = sh_quote(remote_dir),
        m = sh_quote(manifest_path),
        json = sh_quote(candidate_json),
    )
}

fn map_rsync_failure(
    code: Option<i32>,
    stderr: &str,
    ssh: &Ssh,
    sync: &str,
    doctor: &str,
) -> anyhow::Error {
    let stderr = stderr.trim();
    let brief: String = stderr.lines().take(4).collect::<Vec<_>>().join("\n  ");
    if stderr.contains("command not found") || stderr.contains("No such file") && code == Some(127)
    {
        return fail!("the server has no usable rsync")
            .now(format!(
                "install it: ssh {} 'apt-get install -y rsync'  (or your distro's equivalent)",
                ssh.dest
            ))
            .now(format!("then verify with: {doctor}"))
            .into_err();
    }
    // 30: rsync's own --timeout=60 fired. The transfer was alive when it
    // started, so this is a link that went away mid-flight, not a setup
    // problem — say that instead of sending the user to doctor.
    if code == Some(30) {
        return fail!(
            "the transfer to {} stalled for 60 seconds and rsync gave up",
            ssh.dest
        )
        .now(format!(
            "check the link: ssh -o ConnectTimeout=10 {} true",
            ssh.dest
        ))
        .now(format!(
            "nothing was deleted; rerun when the connection is back: {sync}"
        ))
        .into_err();
    }
    if code == Some(12) || stderr.contains("protocol") {
        return fail!("rsync protocol error talking to {}:\n  {brief}", ssh.dest)
            .now(format!(
                "check the remote rsync version (need >= 3.2): {doctor}"
            ))
            .into_err();
    }
    if stderr.contains("No space left") {
        return fail!("the server disk filled up mid-sync:\n  {brief}")
            .now(format!(
                "free space, e.g.: ssh {} 'docker system df && docker system prune'",
                ssh.dest
            ))
            .into_err();
    }
    fail!(
        "rsync failed (exit {}):\n  {brief}",
        code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
    )
    .now(format!("test the connection: ssh {} true", ssh.dest))
    .now(format!("run the preflight checks: {doctor}"))
    .into_err()
}

// ─── local rsync discovery ──────────────────────────────────────────────

/// ulak prefers Homebrew's rsync EXPLICITLY: on macOS, bare `rsync`
/// can resolve to Apple's openrsync (protocol 29 — unusable), and PATH
/// order differs per shell. Design decision, not an accident.
const RSYNC_CANDIDATES: &[&str] = &["/opt/homebrew/bin/rsync", "/usr/local/bin/rsync", "rsync"];

pub fn local_rsync() -> Result<PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            RSYNC_CANDIDATES.iter().find_map(|c| {
                rsync_version(c)
                    .filter(|v| *v >= (3, 2))
                    .map(|_| PathBuf::from(c))
            })
        })
        .clone()
        .ok_or_else(|| {
            fail!("no rsync >= 3.2 found on this machine (Apple's openrsync is not enough)")
                .now("macOS: brew install rsync")
                .now("linux: apt-get install rsync  (or your distro's equivalent)")
                .into_err()
        })
}

pub fn rsync_version(bin: &str) -> Option<(u32, u32)> {
    let mut cmd = Command::new(bin);
    cmd.arg("--version");
    let out = crate::proc::run_bounded(&mut cmd, None, crate::proc::PROBE).ok()?;
    if !out.status.success() || out.timed_out {
        return None;
    }
    parse_rsync_version(&String::from_utf8_lossy(&out.stdout))
}

pub fn parse_rsync_version(text: &str) -> Option<(u32, u32)> {
    // "rsync  version 3.4.4  protocol version 32"
    // openrsync: "openrsync: protocol version 29\nrsync version 2.6.9 ..."
    let line = text.lines().find(|l| l.contains("version"))?;
    if line.contains("openrsync") {
        return Some((0, 0));
    }
    let ver = line
        .split_whitespace()
        .skip_while(|w| *w != "version")
        .nth(1)?;
    let mut it = ver.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    Some((it.next()?, it.next().unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Atomic saves change the containing directory's mtime. After the leaf
    /// was acknowledged, that alone must not schedule another full-tree sync.
    /// A genuinely new child still invalidates the cached selection.
    #[test]
    fn background_audits_distinguish_acknowledged_saves_from_new_children() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"before").unwrap();
        let mut session = Session::default();
        session.observed.insert(
            dir.path().into(),
            crate::selection::Stamp::read(dir.path()).unwrap(),
        );
        session
            .children
            .insert(dir.path().into(), directory_children(dir.path()).unwrap());
        let temporary = dir.path().join("temporary");
        std::fs::write(&temporary, b"after").unwrap();
        std::fs::rename(&temporary, &path).unwrap();
        session
            .observed
            .insert(path.clone(), crate::selection::Stamp::read(&path).unwrap());
        session.audit();
        assert!(session.audit.take().unwrap().join().unwrap().is_empty());
        std::fs::write(dir.path().join("new"), b"born locally").unwrap();
        session.audit();
        assert_eq!(
            session.audit.take().unwrap().join().unwrap(),
            std::collections::BTreeSet::from([dir.path().to_path_buf()])
        );
        std::fs::write(&path, b"not acknowledged").unwrap();
        session.audit();
        assert!(
            session
                .audit
                .take()
                .unwrap()
                .join()
                .unwrap()
                .contains(&path)
        );
    }

    fn inventory_footprint(anchor: &std::path::Path) -> Footprint {
        Footprint {
            anchor: anchor.to_path_buf(),
            entries: Vec::new(),
            server_refs: Vec::new(),
            whole_anchor: true,
            contexts: Vec::new(),
            pinned: Vec::new(),
            model_json: String::new(),
        }
    }

    /// Enumerating an anchor used to spell it `./.`, which made every
    /// generated name fail the relative-path validation. Pruning also has
    /// to quote find's glob dialect, independently of shell quoting.
    #[test]
    fn inventory_names_are_relative_and_protected_globs_remain_literal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (path, bytes) in [
            ("kept/x", "ok"),
            ("data[*]/secret", "secret"),
            ("data1/safe", "yes"),
            (".ulak-partial/blob", "partial"),
        ] {
            let p = root.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        std::os::unix::fs::symlink("kept", root.join("link")).unwrap();
        let fp = inventory_footprint(root);
        let script = inventory_script(root.to_str().unwrap(), &["data[*]".into()], &fp, &[]);
        let mut cmd = Command::new("sh");
        cmd.arg("-s");
        let out =
            crate::proc::run_bounded(&mut cmd, Some(script.into_bytes()), crate::proc::SCRIPT)
                .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let names =
            crate::selection::unclaimed(&out.stdout, &[], &crate::selection::Excludes::new([]))
                .unwrap();
        assert_eq!(
            names,
            [b"data1/safe".to_vec(), b"kept/x".to_vec(), b"link".to_vec()]
        );
    }

    /// A byte-identical metadata observation may be acknowledged, but a
    /// same-length save or a rename preserving mtime must remain pending.
    #[test]
    fn warm_selection_detects_same_size_saves_and_rename_over_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file");
        std::fs::write(&path, b"one").unwrap();
        let original_time = std::fs::metadata(&path).unwrap().modified().unwrap();
        let observed = std::collections::BTreeMap::from([(
            path.clone(),
            crate::selection::Stamp::read(&path).unwrap(),
        )]);
        let paths = std::collections::BTreeSet::from([path.clone()]);
        assert!(
            changed_leaves(
                tmp.path(),
                &Session {
                    observed: observed.clone(),
                    ..Session::default()
                },
                &paths
            )
            .unwrap()
            .is_empty()
        );
        std::fs::write(&path, b"two").unwrap();
        assert_eq!(
            changed_leaves(
                tmp.path(),
                &Session {
                    observed: observed.clone(),
                    ..Session::default()
                },
                &paths
            )
            .unwrap()
            .len(),
            1
        );
        let staged = tmp.path().join("staged");
        std::fs::write(&staged, b"new").unwrap();
        std::fs::File::open(&staged)
            .unwrap()
            .set_modified(original_time)
            .unwrap();
        std::fs::rename(staged, &path).unwrap();
        assert_eq!(
            changed_leaves(
                tmp.path(),
                &Session {
                    observed: observed.clone(),
                    ..Session::default()
                },
                &paths
            )
            .unwrap()
            .len(),
            1
        );
        std::fs::remove_file(&path).unwrap();
        assert!(
            changed_leaves(
                tmp.path(),
                &Session {
                    observed: observed.clone(),
                    ..Session::default()
                },
                &paths
            )
            .is_none()
        );
    }

    /// Both accepting and invalidating transitions matter: a cached
    /// known file must never let a new path or an ignore edit bypass the
    /// walker that decides whether it may travel.
    #[test]
    fn warm_selection_reconciles_new_paths_and_changed_ignore_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let edit = |name: &str| {
            let path = tmp.path().join(name);
            std::fs::write(&path, b"old").unwrap();
            let observed = std::collections::BTreeMap::from([(
                path.clone(),
                crate::selection::Stamp::read(&path).unwrap(),
            )]);
            std::fs::write(&path, b"new-rule").unwrap();
            changed_leaves(
                tmp.path(),
                &Session {
                    observed,
                    ..Session::default()
                },
                &std::collections::BTreeSet::from([path]),
            )
        };
        for rules in [".dockerignore", "Dockerfile.dockerignore", "ulak.toml"] {
            assert!(
                edit(rules).is_none(),
                "{rules} must send the save to a walk"
            );
        }
        // The walker never reads a `.gitignore`, so an edit to one moves
        // no rule: it is an ordinary save, sent like any other.
        let saved = edit(".gitignore").expect("a .gitignore edit is an ordinary save");
        assert_eq!(
            saved.keys().collect::<Vec<_>>(),
            [&tmp.path().join(".gitignore")]
        );

        let new = tmp.path().join("new");
        std::fs::write(&new, b"new").unwrap();
        assert!(
            changed_leaves(
                tmp.path(),
                &Session::default(),
                &std::collections::BTreeSet::from([new])
            )
            .is_none()
        );
    }

    /// An atomic editor save reports the temporary sibling and its parent
    /// as well as the file. Treating every absent unknown path as a delete
    /// forced a full scan on every ordinary save.
    #[test]
    fn an_atomic_save_selects_the_file_but_a_new_sibling_requires_a_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let file = root.join("file");
        let staging = root.join("temporary");
        std::fs::write(&file, b"before").unwrap();
        let session = Session {
            observed: [root.to_path_buf(), file.clone()]
                .into_iter()
                .map(|p| {
                    let stamp = crate::selection::Stamp::read(&p).unwrap();
                    (p, stamp)
                })
                .collect(),
            children: std::collections::BTreeMap::from([(
                root.to_path_buf(),
                directory_children(root).unwrap(),
            )]),
            owned: std::collections::BTreeSet::from([file.clone()]),
            ..Session::default()
        };
        std::fs::write(&staging, b"after").unwrap();
        std::fs::rename(&staging, &file).unwrap();
        let events =
            std::collections::BTreeSet::from([root.to_path_buf(), file.clone(), staging.clone()]);
        let changed = changed_leaves(root, &session, &events).unwrap();
        assert_eq!(changed.keys().collect::<Vec<_>>(), [&file]);
        assert!(
            changed_leaves(
                root,
                &session,
                &std::collections::BTreeSet::from([root.to_path_buf()])
            )
            .is_none(),
            "a directory-only notification is not proof that its contents are unchanged"
        );
        std::fs::write(&staging, b"a real new file").unwrap();
        assert!(changed_leaves(root, &session, &events).is_none());
        std::fs::remove_file(&file).unwrap();
        assert!(
            changed_leaves(root, &session, &std::collections::BTreeSet::from([file])).is_none()
        );
    }

    /// Run a removal round the way the server runs it: `sh -s` with the
    /// script on stdin (ssh.rs:script_attempt), in a real directory.
    ///
    /// The script is the only part of a deletion that a filename gets to
    /// influence, so it is tested against a shell rather than by reading
    /// the text back — a claim about what `sh` does with a name is worth
    /// exactly as much as the shell's answer to it.
    fn run_removal(
        dir: &std::path::Path,
        home: &std::path::Path,
        doomed: &[&str],
        doomed_dirs: &[&str],
    ) -> String {
        let own = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let script = removal_script(&dir.to_string_lossy(), &own(doomed), &own(doomed_dirs));
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-s").env("HOME", home);
        let out = crate::proc::run_bounded(&mut cmd, Some(script.into_bytes()), crate::proc::PROBE)
            .expect("a shell to run the removal script");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn touch(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"x").unwrap();
    }

    /// A filename is data, and the payload has to carry it as data.
    ///
    /// The doomed lists used to travel as the bodies of quoted heredocs.
    /// A file named exactly `ULAK_DOOMED` — legal, synced without a
    /// murmur by walk.rs, and sorted ahead of most rows by the BTreeSet
    /// the list comes from — ended the heredoc on its own line. Every
    /// remaining doomed path was then read by the far shell as a COMMAND
    /// with the workspace as its cwd: the deletions after it silently
    /// did not happen, and a row spelling a script ran instead.
    #[test]
    fn no_filename_can_end_the_removal_payload_or_become_a_command() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // `touch pwned` is a legal filename. Read as a command it makes
        // a file called `pwned`, which is the whole proof: nothing in a
        // doomed list may ever be executed.
        for name in ["ULAK_DOOMED", "touch pwned", "after.txt"] {
            touch(&dir.join(name));
        }
        let out = run_removal(
            dir,
            dir,
            &["ULAK_DOOMED", "touch pwned", "after.txt"],
            &["ULAK_DOOMED_DIRS"],
        );

        assert!(
            !dir.join("pwned").exists(),
            "a doomed row was executed as a command: {out}"
        );
        for name in ["ULAK_DOOMED", "touch pwned", "after.txt"] {
            assert!(
                !dir.join(name).exists(),
                "{name} survived the removal round: {out}"
            );
        }
        assert!(out.contains("DELETED 3"), "wrong receipt: {out}");
        assert!(
            !out.contains("ULAK_KEPT"),
            "nothing here should have been kept: {out}"
        );
    }

    /// Two dots in a name are not a way out of the workspace.
    ///
    /// The guard was `*..*`, a substring test: `v1..v2.patch` and
    /// `2024..2025.csv` are legal names that sync fine, and deleting one
    /// locally left it doomed on every sync forever — reported as kept,
    /// counted as zero pending, so `status` and `doctor` both called the
    /// workspace clean while it held a file the project had removed.
    /// What the guard is actually for is a `..` PATH COMPONENT, and that
    /// still has to be refused whole.
    #[test]
    fn a_legal_name_with_two_dots_is_retired_and_a_real_traversal_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        touch(&ws.join("v1..v2.patch"));
        touch(&ws.join("2024..2025.csv"));
        touch(&ws.join("sneak.txt"));
        touch(&tmp.path().join("outside.txt"));

        let out = run_removal(
            &ws,
            tmp.path(),
            &[
                "v1..v2.patch",
                "2024..2025.csv",
                "../outside.txt",
                "sub/../sneak.txt",
                "/etc/passwd",
                "..",
            ],
            &[],
        );

        assert!(
            !ws.join("v1..v2.patch").exists() && !ws.join("2024..2025.csv").exists(),
            "a legal name with two dots must be retirable: {out}"
        );
        assert!(
            tmp.path().join("outside.txt").exists() && ws.join("sneak.txt").exists(),
            "a `..` component must never reach the rm: {out}"
        );
        assert!(tmp.path().join("workspace").exists(), "`..` itself ran");
        for kept in [
            "ULAK_KEPT_FILE ../outside.txt",
            "ULAK_KEPT_FILE sub/../sneak.txt",
            "ULAK_KEPT_FILE /etc/passwd",
            "ULAK_KEPT_FILE ..",
        ] {
            assert!(out.contains(kept), "missing {kept} in: {out}");
        }
        assert!(out.contains("DELETED 2"), "wrong receipt: {out}");
    }

    /// A row that reads like a home path is still a path in the
    /// workspace.
    ///
    /// `ssh::sh_quote` leaves a leading `~/` unquoted on purpose, so
    /// `-v ~/data:/app` still names the server's home. A workspace may
    /// legally hold a directory called `~`, and quoting these rows that
    /// way would aim the `rm` at the remote home directory instead —
    /// where the `case` guard would refuse it, leaving the real file
    /// undeletable forever.
    #[test]
    fn a_row_that_looks_like_a_home_path_stays_inside_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        touch(&home.join("notes.txt"));
        touch(&ws.join("~/notes.txt"));

        let out = run_removal(&ws, &home, &["~/notes.txt"], &[]);
        assert!(
            home.join("notes.txt").exists(),
            "the removal escaped into $HOME: {out}"
        );
        assert!(
            !ws.join("~/notes.txt").exists(),
            "the workspace's own file was not retired: {out}"
        );
        assert!(out.contains("DELETED 1"), "wrong receipt: {out}");
    }

    /// The ledger's read-modify-write is only safe because the caller
    /// holds the workspace lock, and nothing said so out loud.
    ///
    /// `store` merges now, which means it READS the ledger it is about
    /// to replace. Two syncs of one workspace interleaving there lose
    /// whichever rows the loser had just claimed — the un-claiming bug
    /// this merge was written to fix, arriving by another door. Every
    /// path in does hold `WorkspaceLock` today; this is what stops the
    /// next call site from being the one that does not.
    ///
    /// It used to look for one spelling — `sync::run_sync(` — inside six
    /// files somebody typed out by hand, two of which contained no call
    /// at all. Both halves of that were how doctor's pre-flight came to
    /// wipe and rewrite the ledger with no lock anywhere: `doctor.rs` was
    /// not on the list, and `push_no_delete` was not the spelling. So the
    /// files come from the directory now, and the doors are named by what
    /// they reach rather than by which function is fashionable to call.
    /// `main.rs` reads oddly and is right: a `--dry-run` skips the lock
    /// precisely because a dry run returns before any write.
    #[test]
    fn nothing_reaches_the_ledger_without_the_workspace_lock() {
        // Every .rs in src, read from the DIRECTORY. The list used to be
        // written by hand, and a hand-written list stops at the file
        // nobody thought to add: doctor.rs was not on it, which is how
        // its pre-flight came to wipe and rewrite the ledger with no
        // lock at all and this guard say nothing.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources: Vec<(String, String)> = std::fs::read_dir(&src)
            .expect("src/ to be readable")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "rs"))
            .map(|p| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    std::fs::read_to_string(&p).unwrap_or_default(),
                )
            })
            .collect();
        sources.sort();
        assert!(sources.len() > 20, "src/ went missing: {}", sources.len());

        // Every door that reaches a ledger write, not just the one
        // spelling anybody remembered. `push_no_delete` is handled on
        // its own below — it takes the lock ITSELF.
        //
        // `invocation::forget_workspace_destination(` earns its place the
        // hard way: `clean` used to reach the ledger through
        // `ledger::forget(`, and moving to a per-destination state
        // directory took the ledger out from under this guard silently —
        // the call still deleted the ledger, the table just no longer
        // named it. A door is only pinned once it is written down here.
        //
        // `pull_back(` joined when the pull began claiming the files it
        // brings home after the server's edit won (`claim_returned`).
        const DOORS: &[&str] = &[
            "run_sync(",
            "reconcile_once(",
            "reconcile_changes(",
            "pull_back(",
            "ledger::store(",
            "ledger::forget(",
            "invocation::forget_workspace_destination(",
        ];
        let mut checked = 0;
        for (file, text) in &sources {
            // sync.rs and ledger.rs are the implementation: the rule is
            // about their CALLERS, and run_sync's own doc says so.
            if file == "sync.rs" || file == "ledger.rs" {
                continue;
            }
            for door in DOORS {
                for (at, _) in text.match_indices(door) {
                    if in_a_comment(text, at) {
                        continue;
                    }
                    let head = enclosing_fn(text, at);
                    let before = &text[head..at];
                    let locked = before
                        .match_indices("WorkspaceLock::")
                        .filter(|(i, _)| {
                            before[*i..].starts_with("WorkspaceLock::acquire")
                                || before[*i..].starts_with("WorkspaceLock::try_acquire")
                        })
                        // `let _ = WorkspaceLock::acquire(..)` drops the
                        // guard on the same line and locks nothing.
                        .any(|(i, _)| !line_at(before, i).contains("let _ ="));
                    assert!(
                        locked,
                        "{file}: a `{door}` call reaches the ledger without taking \
                         the workspace lock first"
                    );
                    checked += 1;
                }
            }
        }
        // A guard that stopped finding call sites would pass forever.
        // Thirteen call sites today, across seven files, five of them
        // `pull_back`; the floor is under that so a refactor that merges
        // two of them is not a false alarm, and over what is left without
        // the pull's.
        assert!(
            checked >= 10,
            "expected several call sites, found {checked}"
        );

        // The one exception, and both halves of it. `push_no_delete`
        // takes the lock itself because its only caller is a whole-
        // program pre-flight that holds none…
        let mine = include_str!("sync.rs");
        let at = mine
            .find("pub fn push_no_delete(")
            .expect("push_no_delete to still be here");
        let body = &mine[at..at + mine[at..].find("\n}\n").unwrap_or(0)];
        assert!(
            body.contains("WorkspaceLock::acquire"),
            "push_no_delete stopped taking the workspace lock, and doctor holds none"
        );
        // …and a caller must therefore NOT take it too: the lock is an
        // OS file lock on a fresh handle, and a second handle in this
        // same process blocks against the first — forever.
        for (file, text) in &sources {
            if file == "sync.rs" {
                continue;
            }
            for (at, _) in text.match_indices("push_no_delete(") {
                if in_a_comment(text, at) {
                    continue;
                }
                assert!(
                    !text[enclosing_fn(text, at)..at].contains("WorkspaceLock::"),
                    "{file}: push_no_delete takes the workspace lock itself — holding it \
                     here as well deadlocks the two against each other"
                );
            }
        }
    }

    /// A push that moves bytes can only be written one way.
    ///
    /// Doctor's pre-flight was a second push written beside the engine's,
    /// and what it left out was the claim: every file it sent landed on
    /// the server with nobody claiming it — so `doomed` could never name
    /// it, and the pull's creating pass was free to plant it back in the
    /// repo after the user deleted it. `init` points the user straight
    /// at `ulak doctor`, which made that the ordinary FIRST population
    /// of a workspace. There is one function now that pushes and claims
    /// in the same breath, and this is what stops a third one being
    /// written next to it.
    #[test]
    fn a_push_that_moves_bytes_can_only_be_written_one_way() {
        let mine = include_str!("sync.rs");
        // Spelled at runtime so this test does not match itself.
        let needle = format!("{}(", "rsync_pass");
        let mut checked = 0;
        for (at, _) in mine.match_indices(&needle) {
            if mine[..at].trim_end().ends_with("fn") || in_a_comment(mine, at) {
                continue;
            }
            let args = &mine[at..at + mine[at..].find(")?").unwrap_or(0)];
            // A dry run moves nothing, so it owes the ledger nothing.
            if args.contains("--dry-run") {
                continue;
            }
            assert!(
                mine[enclosing_fn(mine, at)..at].contains("fn push_and_claim"),
                "a push outside push_and_claim: whatever it sends lands on the server \
                 with nobody claiming it"
            );
            checked += 1;
        }
        assert_eq!(checked, 1, "expected exactly one way up, found {checked}");
    }

    /// Nothing under `protect` may enter the ledger.
    ///
    /// `protect` means never pushed AND never deleted, so the copy on the
    /// server is the only copy. A ledger row is a licence to `rm -f`, and
    /// the rank-2 filter rule that stops the push does nothing about the
    /// removal round — so one claim on server-owned data is one database
    /// directory the next stale local copy takes with it.
    ///
    /// `walk_dir` never enters a protect path, which leaves exactly one
    /// door: `checked_plan`'s loop over the individually-named entries,
    /// which no walk ever sees. Pinned on the source because the function
    /// needs a bound `Project` and a server to run.
    #[test]
    fn nothing_server_owned_can_be_claimed_by_the_files_named_one_by_one() {
        let mine = include_str!("sync.rs");
        let at = mine
            .find("fn checked_plan")
            .expect("checked_plan is the only door");
        let body = &mine[at..][..mine[at..].find("\n}\n").expect("it ends")];
        let loop_at = body
            .find("for e in &fp.entries")
            .expect("the named-entry loop");
        assert!(
            body[loop_at..].contains("walk::protected"),
            "checked_plan claims individually-named entries without asking whether \
             they are server-owned, and a claimed protect path is one `rm -f` away"
        );
    }

    /// And a push that ended BADLY still claims what it managed to send.
    ///
    /// The one way up returns the itemized rows and the verdict
    /// separately, because they are not alternatives: rsync exits 23 —
    /// "partial transfer due to error" — for a single unreadable file in
    /// a tree that otherwise arrived, and every file that did arrive is
    /// now on the server. Raising the failure before the claim left all
    /// of them with nobody claiming them, which is the same hole `claim`
    /// exists to close: `doomed` cannot name an unclaimed file, so
    /// deleting one here left it up there forever, and `pull_back`'s
    /// creating pass was free to plant it back in the repo.
    ///
    /// The fix is an ORDER, so the order is what is pinned — a later
    /// edit that moves the raise back above the claim reopens it without
    /// changing a single behaviour a type could catch.
    #[test]
    fn a_push_that_ended_badly_still_claims_what_it_sent() {
        let mine = include_str!("sync.rs");
        let at = mine
            .find("fn push_and_claim")
            .expect("there is one way up and this is its name");
        let body = &mine[at..];
        let body = &body[..body.find("\n}\n").expect("the function ends")];
        let claim = body
            .find("ledger::store")
            .expect("the one way up claims what it sent");
        let raise = body
            .find(".or_fail()")
            .expect("the one way up can still fail");
        assert!(
            claim < raise,
            "push_and_claim raises the rsync failure before it claims, so everything \
             a partial transfer DID put on the server is left unclaimed"
        );
        // The channel's way up keeps the same order: a create the helper
        // applied without a receipt would otherwise be a file nobody can retire.
        let at = mine
            .find("fn stream_structure")
            .expect("the channel's way up");
        let body = &mine[at..][..mine[at..].find("\n}\n").expect("it ends")];
        assert!(
            body.find("ledger::store").expect("it claims") < body.find(".send(").expect("it sends"),
            "stream_structure sends before it claims"
        );
    }

    /// The start of the function an offset sits in: the nearest
    /// preceding line that declares one, at whatever indentation. A
    /// `rfind("\nfn ")` misses every method in an impl block and falls
    /// back to the top of the file, where any lock anywhere would do.
    fn enclosing_fn(text: &str, at: usize) -> usize {
        text[..at]
            .match_indices('\n')
            .filter(|(i, _)| {
                let head = text[i + 1..].lines().next().unwrap_or("").trim_start();
                head.starts_with("fn ")
                    || head.starts_with("pub fn ")
                    || head.starts_with("pub(crate) fn ")
                    || head.starts_with("async fn ")
            })
            .map(|(i, _)| i)
            .next_back()
            .unwrap_or(0)
    }

    fn line_at(text: &str, at: usize) -> &str {
        let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
        let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
        &text[start..end]
    }

    /// Doc comments name these functions constantly (service.rs's module
    /// header calls `sync::reconcile_once` by name), and a mention is
    /// not a call site.
    fn in_a_comment(text: &str, at: usize) -> bool {
        let line = line_at(text, at).trim_start();
        line.starts_with("//") || line.starts_with("*") || line.starts_with("/*")
    }

    #[test]
    fn itemize_parsing() {
        // Real push output: rsync marks files it SENDS with '<'. (The
        // '>' shape here is the pull direction, kept so both are
        // covered — reading it as the only transfer marker was a
        // measured bug: every sync reported zero changes.)
        let out = "<f+++++++++ new.txt\n\
                   <f.st......  leading-space.txt\n\
                   >f+++++++++ pulled.txt\n\
                   cd+++++++++ sub/\n\
                   cL+++++++++ link -> ../target\n\
                   .d..t...... touched-dir/\n\
                   *deleting   gone.txt\n\
                   *deleting   olddir/\n";
        let events = parse_itemized(out);
        let pushes: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Push(p) => Some(p.as_str()),
                _ => None,
            })
            .collect();
        let deletes: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Delete(p) => Some(p.as_str()),
                _ => None,
            })
            .collect();
        // .d attr-only line skipped; symlink keeps its name, not target;
        // fixed-width split preserves the leading space in the name.
        assert_eq!(
            pushes,
            vec![
                "new.txt",
                " leading-space.txt",
                "pulled.txt",
                "sub/",
                "link"
            ]
        );
        assert_eq!(deletes, vec!["gone.txt", "olddir"]);
    }

    /// The one trip the ignore rules cannot stop, and it used to be
    /// silent.
    ///
    /// The exclude list is built by WALKING this machine, so it holds
    /// real paths. A directory a container created on the server is on
    /// no such list, so pass two brings it home — once. From the next
    /// run it is here, the walk names it, and rank 3 of `filter_args`
    /// outranks the footprint's `+` rules at rank 4, so it never moves
    /// again in either direction. The ranking stays; only the silence
    /// goes, because the trip it does make is the one that puts a
    /// container's data directory on somebody's laptop unasked.
    #[test]
    fn what_the_stack_invented_under_an_ignored_path_is_named_when_it_arrives() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = temp.path().canonicalize().unwrap();
        std::fs::write(anchor.join(".gitignore"), "pgdata/\n").unwrap();
        std::fs::create_dir_all(anchor.join("pgdata/base")).unwrap();
        std::fs::write(anchor.join("pgdata/base/1"), "").unwrap();
        std::fs::write(anchor.join("app.log"), "").unwrap();

        let fp = Footprint {
            anchor: anchor.clone(),
            entries: Vec::new(),
            server_refs: Vec::new(),
            whole_anchor: true,
            contexts: Vec::new(),
            pinned: Vec::new(),
            model_json: String::new(),
        };
        // `[sync] exclude` is the only rule speaking. The `.gitignore`
        // above says the same thing and is never read — the last
        // assertion holds it to that.
        let mut cfg = crate::config::SyncCfg {
            exclude: vec!["pgdata/".into()],
            include: Vec::new(),
            protect: Vec::new(),
            max_delete: crate::config::DeleteBudget::Limit(0),
            no_history: crate::config::NoHistory::Send,
        };

        let invented = vec![
            Event::Push("pgdata/".into()),
            Event::Push("pgdata/base/".into()),
            Event::Push("pgdata/base/1".into()),
            // Not ignored: ordinary generated work, and bringing it home
            // is the whole point of the pass.
            Event::Push("app.log".into()),
            Event::Delete("pgdata/old".into()),
        ];
        assert_eq!(
            slipped_past_the_ignores(&cfg, &fp, &invented),
            vec!["pgdata".to_string()],
            "the outermost ignored path is the one worth naming, and a deletion is not \
             something that arrived"
        );

        // Nothing invented, nothing said — the ordinary run, and the one
        // that must not pay for a `.dockerignore` read.
        assert!(slipped_past_the_ignores(&cfg, &fp, &[]).is_empty());
        assert!(
            slipped_past_the_ignores(&cfg, &fp, &[Event::Push("app.log".into())]).is_empty(),
            "a file no rule covers is not a leak"
        );

        // Without the exclude nothing covers `pgdata`: it comes home like
        // any other output, whatever the `.gitignore` says.
        cfg.exclude.clear();
        assert!(slipped_past_the_ignores(&cfg, &fp, &invented).is_empty());
    }

    #[test]
    fn the_ledger_claims_what_rsync_sent_not_only_what_the_walk_saw() {
        // The walk runs before the transfer, with a remote round-trip in
        // between. A file born in that window is on the server and, if
        // the ledger is built from the walk alone, is never claimed —
        // so deleting it locally leaves it up there forever. Measured
        // on my-server with a save that landed while watch waited on
        // the lock.
        let walked = vec![b"site/index.html".to_vec()];
        let walked_dirs = vec![b"site".to_vec()];
        let sent = vec![
            Event::Push("site/index.html".into()),
            Event::Push("site/scratch.txt".into()), // born mid-sync
            Event::Push("site/".into()),            // a directory row
            Event::Delete("site/old.txt".into()),
        ];
        let claimed = claim(&walked, &walked_dirs, &sent);
        assert_eq!(
            claimed,
            vec![
                b"site/".to_vec(),
                b"site/index.html".to_vec(),
                b"site/scratch.txt".to_vec()
            ],
            "deletions stay out, the new file comes in — and the directory is \
             claimed from the WALK, spelled with the slash that tells it apart"
        );
    }

    #[test]
    fn a_server_anchor_that_contains_ours_is_a_stale_read_not_a_relayout() {
        // Anchors only widen and both are ancestors of the same root, so
        // "does the server's contain ours?" is the whole question. Yes
        // means this invocation widened against a record that was not
        // current — the state directory is gone, or another ulak settled
        // a wider anchor between this one's read and its lock — and
        // `rm -rf`-ing the server for that would destroy the wider
        // layout the other side had just pushed.
        assert!(server_holds_the_wider_anchor("/w", "/w/repo"));
        assert!(server_holds_the_wider_anchor("/w", "/w/repo/deep"));
        // A genuine widening, which is what relayout is actually for.
        assert!(!server_holds_the_wider_anchor("/w/repo", "/w"));
        // Component-wise: a shared prefix of NAME is not containment.
        assert!(!server_holds_the_wider_anchor("/w/rep", "/w/repo"));
        // A server holding no anchor makes no claim about anything.
        assert!(!server_holds_the_wider_anchor("", "/w/repo"));
    }

    #[test]
    fn a_protect_entry_reaches_rsync_anchored_once_however_it_was_spelled() {
        // `walk::rsync_exclude_pattern` prepends the anchoring slash
        // itself, so an entry carrying its own arrived as `P //data` —
        // and that matches nothing. Measured against rsync with ulak's
        // own rule order: the stale local copy overwrote the server's
        // live one, which is the single thing `protect` exists to stop.
        for spelling in ["data", "data/", "./data", "/data", "/data/", "//data"] {
            let args = filter_args(&[spelling.to_string()], &[], vec![]);
            assert!(
                args.contains(&"--filter=P /data".to_string())
                    && args.contains(&"--filter=- /data".to_string()),
                "{spelling:?} produced {args:?}"
            );
        }
        // An entry that names nothing still protects nothing, rather
        // than protecting everything.
        for nothing in ["", "/", ".", "./", "//"] {
            let args = filter_args(&[nothing.to_string()], &[], vec![]);
            assert!(
                !args.iter().any(|a| a.starts_with("--filter=P ")),
                "{nothing:?} produced {args:?}"
            );
        }
    }

    #[test]
    fn partial_blobs_are_excluded_before_anything_can_include_them() {
        // Measured: an interrupted transfer leaves
        // `.ulak-partial/` on the server, the footprint's
        // `+ site/***` matches it first, and the next pull_back
        // carries half a blob into the user's repo. rsync's own
        // exclude for the partial dir sits at the END of the
        // chain, so it never gets a say. Rank 0 or the bug is back.
        let args = filter_args(
            &["data/".to_string()],
            &["site/renamed-dir-a/inner.txt".to_string()],
            vec!["+ /site/".into(), "+ /site/***".into(), "- *".into()],
        );
        assert_eq!(args[0], "--filter=- .ulak-partial/");
        let partial = 0;
        // A path you deleted must not ride the footprint's `+ site/***`
        // back home. Measured: a directory renamed while a reconcile was
        // in flight came back and, since ulak never deletes locally,
        // could never leave again.
        let held_back = args
            .iter()
            .position(|a| a == "--filter=- /site/renamed-dir-a/inner.txt")
            .expect("held-back rule");
        let protect = args
            .iter()
            .position(|a| a.starts_with("--filter=P "))
            .expect("protect rule");
        let ignores = args
            .iter()
            .position(|a| a == "--exclude-from=-")
            .expect("ignore list");
        let footprint = args
            .iter()
            .position(|a| a == "--filter=+ /site/***")
            .expect("footprint rule");
        let wall = args
            .iter()
            .position(|a| a == "--filter=- *")
            .expect("the wall");
        assert!(
            partial < held_back
                && held_back < protect
                && protect < ignores
                && ignores < footprint
                && footprint < wall,
            "filter order broke: {args:?}"
        );
    }

    #[test]
    fn clients_sharing_a_namespace_keep_their_own_uuid_and_an_upgrade_adopts() {
        // Automatic namespaces keep ordinary clients apart. Two clients
        // can deliberately configure the same namespace, though, and if
        // their checkout paths also match the manifest identity and
        // local_root are IDENTICAL — so only the UUID can tell the claims
        // apart. With two services running around the clock, the cost of
        // getting this wrong is a permanent delete/push ping-pong.
        // Nothing on the server can settle this one, so it is NOT
        // decided here: it goes to whoever is standing at the terminal,
        // and to nobody at all when there is no terminal. Measured on a
        // real project: deciding it silently locked a workspace this very
        // machine had created behind a message whose only way out was to
        // destroy it.
        assert_eq!(settle_uuid("mine", "theirs", false), Owner::Ambiguous);

        // The workspace we just created carries the uuid we proposed.
        assert_eq!(settle_uuid("mine", "mine", false), Owner::Ours);

        // A v0.3 workspace of OUR OWN: the server's manifest predates the
        // local record, and refusing it would declare our own project
        // foreign to itself on the first upgrade.
        assert_eq!(settle_uuid("mine", "theirs", true), Owner::Adopt);
    }

    /// The run a declaration exists for is the one with no memory of its
    /// own: the SECOND job on a wiped runner, whose fresh state directory
    /// would otherwise mint a uuid, fail to match the one the server kept
    /// from the first job, and be refused — on a machine with no terminal
    /// to answer the ownership question in.
    ///
    /// Repeating a claim that is already written down is not a write:
    /// `record_uuid` truncates before it writes, so a rewrite on every
    /// command is a window in which a concurrent ulak reads no claim at
    /// all, which is exactly the state that asks the ownership question.
    #[test]
    fn a_declared_identity_is_recorded_once_and_not_rewritten_while_it_holds() {
        assert_eq!(
            declared_identity(Some("D-1"), None),
            Identity {
                known: Some("D-1"),
                record: Some("D-1"),
            },
            "the wiped runner: the declaration is the only memory there is"
        );
        assert_eq!(
            declared_identity(Some("D-1"), Some("D-1")),
            Identity {
                known: Some("D-1"),
                record: None,
            },
            "already written down — saying it again only truncates the claim"
        );
        assert_eq!(
            declared_identity(Some("D-2"), Some("D-1")),
            Identity {
                known: Some("D-2"),
                record: Some("D-2"),
            },
            "the pipeline names the workspace, not a record left by an older one"
        );
        assert_eq!(
            declared_identity(None, Some("U-1")),
            Identity {
                known: Some("U-1"),
                record: None,
            },
            "nothing declared: this machine's own claim stands, untouched"
        );
        assert_eq!(
            declared_identity(None, None),
            Identity {
                known: None,
                record: None,
            },
            "a first sync mints its own uuid, and claims nothing before the server answers"
        );
    }

    #[test]
    fn a_deleted_directory_is_held_back_whole_not_file_by_file() {
        // Measured: excluding only `site/gone/inner.txt` still let the
        // footprint's `+ /site/***` create an EMPTY `site/gone/` here,
        // which the next push sent straight back — so the directory the
        // user renamed away reappeared on the server on every cycle and
        // never left.
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        std::fs::create_dir_all(anchor.join("site")).unwrap();
        std::fs::write(anchor.join("site/here.txt"), b"x").unwrap();

        // The directory is gone from disk: hold the whole subtree.
        assert_eq!(
            hold_at_the_highest_gap(anchor, "site/gone/inner.txt"),
            "site/gone/"
        );
        // Only the file is gone: hold just the file, so a sibling the
        // container writes still comes home.
        assert_eq!(
            hold_at_the_highest_gap(anchor, "site/here.txt"),
            "site/here.txt"
        );
        // Several levels missing: hold at the top of the gap.
        assert_eq!(hold_at_the_highest_gap(anchor, "site/a/b/c.txt"), "site/a/");
    }

    /// Stamp a file with an exact whole second, so a test about
    /// seconds does not have to race one.
    fn write_at(path: &std::path::Path, body: &[u8], secs: u64, nanos: u32) {
        std::fs::write(path, body).unwrap();
        let when = std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    /// The hole itself, put to the real rsync rather than described.
    /// Both saves are stamped with the SAME second on purpose, which is
    /// the only thing a burst ever does by accident, so it runs every
    /// time on every machine. Two claims: rsync's own quick check loses
    /// the edit, and `--checksum` recovers it — which is why the exact
    /// passes carry it over the survey's differing names. If some future
    /// rsync closes this on its own, the first half is what will say so.
    #[test]
    fn a_same_size_save_inside_one_second_is_invisible_to_the_quick_check() {
        let rsync = local_rsync().expect("rsync >= 3.2 — ulak states the same requirement");
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        let page = from.join("index.html");
        // One whole second, chosen and not waited for. Both saves are 24
        // bytes, which is what the burst wrote too.
        let second = 1_760_000_000;
        write_at(&page, b"<h1>service-edit-v3</h1>", second, 0);

        // ulak's push, minus what a local run has no use for.
        let push = |extra: &[&str]| {
            let mut cmd = Command::new(&rsync);
            cmd.args([
                "--recursive",
                "--links",
                "--perms",
                "--times",
                "--delay-updates",
                "--partial-dir=.ulak-partial",
                "--itemize-changes",
            ]);
            cmd.args(extra);
            cmd.arg("--")
                .arg(format!("{}/", from.display()))
                .arg(format!("{}/", to.display()));
            let out = crate::proc::run_bounded(&mut cmd, None, crate::proc::PROBE).unwrap();
            assert_eq!(out.status.code(), Some(0), "rsync itself must not fail");
        };

        push(&[]);
        assert_eq!(
            std::fs::read_to_string(to.join("index.html")).unwrap(),
            "<h1>service-edit-v3</h1>",
            "the first push is the ordinary case and has to work"
        );

        // The save the user would never see arrive: same size, same
        // second, different bytes.
        write_at(&page, b"<h1>service-edit-v5</h1>", second, 0);
        push(&[]);
        assert_eq!(
            std::fs::read_to_string(to.join("index.html")).unwrap(),
            "<h1>service-edit-v3</h1>",
            "rsync's quick check is size + mtime-to-the-second: if this \
             now says v5, rsync has changed and the exact passes can drop \
             `--checksum`"
        );

        // And the exact passes' one flag, which is the whole fix.
        push(&["--checksum"]);
        assert_eq!(
            std::fs::read_to_string(to.join("index.html")).unwrap(),
            "<h1>service-edit-v5</h1>",
            "--checksum is what the exact passes carry, and it has to be \
             enough on its own"
        );
    }

    /// The full push judges by content, and only the exact passes carry
    /// `--checksum`. Two things a later edit could quietly undo, and
    /// neither would fail a behavioural test on macOS, where the hole never
    /// opens — so the shape is pinned here rather than trusted to a green
    /// run: a traversal under `--checksum` on every reconcile is the cost
    /// the survey exists to avoid, and a selected pass without it lets a
    /// same-size save in the same second stay hidden for good.
    #[test]
    fn the_survey_selects_by_content_and_only_the_exact_passes_checksum() {
        let mine = include_str!("sync.rs");
        let body = |name: &str| {
            let at = mine.find(name).unwrap_or_else(|| panic!("{name} exists"));
            &mine[at..][..mine[at..].find("\n}\n").expect("it ends")]
        };

        let push = body("fn push_and_claim");
        assert!(
            push.contains("push_selection(") && push.contains("dir_selection("),
            "the push must take its names from the survey, never from a traversal alone"
        );
        for flag in ["--checksum", "--files-from=-", "--from0"] {
            assert!(
                body("fn offer_again").contains(flag),
                "the warm catch-up pass has to carry {flag}"
            );
        }
        assert!(
            body("fn pull(").contains("--checksum") && body("fn pull(").contains("pull_selection("),
            "the exact pull judges the selected names by content"
        );

        let main = body("fn rsync_pass_dir");
        for flag in ["--ignore-times", "--checksum"] {
            assert!(
                !main.contains(flag),
                "{flag} is on the MAIN pass, which is the cost this design \
                 exists to avoid — a ten-thousand file workspace pays it on \
                 every reconcile and the service gets turned off"
            );
        }
        // Spelled at runtime so this test does not match itself.
        for gone in [
            format!("hidden_by_{}", "the_quick_check"),
            format!("push_{}", "window"),
        ] {
            assert!(
                !mine.contains(&gone),
                "{gone} came back: a content survey needs no catch-up"
            );
        }
    }

    /// The survey decides the transfer. A push sends what differs and may
    /// be sent, never a leaf whose server copy moved; a pull takes server
    /// output onto a local copy that is still the common ancestor, never
    /// over an edit. Both hold unknown divergence, and neither names a
    /// leaf that vanished between the walk and the hash.
    #[test]
    fn the_survey_selects_exactly_the_differing_leaves_each_side_may_take() {
        let v = |s: &str| Some(s.to_owned());
        let rows = [
            // (name, base, local, remote)
            ("same", v("a"), v("a"), v("a")),
            ("edited", v("a"), v("b"), v("a")),
            ("new", None, v("b"), None),
            ("generated", v("a"), v("a"), v("c")),
            ("conflict", v("a"), v("b"), v("c")),
            ("vanished", v("a"), None, v("a")),
            ("unknown", None, v("b"), v("c")),
            ("restore", v("a"), v("a"), None),
        ];
        let column = |index: usize| {
            rows.iter()
                .map(|(name, base, local, remote)| {
                    let value = [base, local, remote][index].clone();
                    ((*name).to_owned(), value)
                })
                .collect::<crate::baseline::Contents>()
        };
        let base = column(0);
        let local = column(1);
        let remote = column(2);

        let (held, selected) =
            push_selection(crate::config::NoHistory::Hold, &base, &local, &remote);
        assert_eq!(selected, ["edited", "new", "restore"]);
        assert_eq!(held, ["conflict", "generated", "unknown"]);
        // The default sends what has no history; a recorded conflict stays held.
        let (held, selected) =
            push_selection(crate::config::NoHistory::Send, &base, &local, &remote);
        assert_eq!(selected, ["edited", "new", "restore", "unknown"]);
        assert_eq!(held, ["conflict", "generated"]);

        let (held, selected) = pull_selection(&base, &local, &remote);
        assert_eq!(selected, ["generated"]);
        assert_eq!(held, ["conflict", "edited", "new", "unknown", "vanished"]);

        // Directories: named only when absent or re-moded on the server.
        let dirs = |rows: &[(&str, Option<&str>)]| {
            rows.iter()
                .map(|(p, v)| ((*p).to_owned(), v.map(str::to_owned)))
                .collect::<crate::baseline::Contents>()
        };
        let local_dirs = dirs(&[
            ("same", Some("d:755")),
            ("remoded", Some("d:750")),
            ("missing", Some("d:755")),
            ("gone", None),
        ]);
        let remote_dirs = dirs(&[
            ("same", Some("d:755")),
            ("remoded", Some("d:755")),
            ("missing", None),
            ("gone", Some("d:755")),
        ]);
        assert_eq!(
            dir_selection(&local_dirs, &remote_dirs),
            ["missing", "remoded"]
        );
    }

    /// rsync 3.5.0, measured: a directory named in `--files-from` under
    /// `--no-recursive` is created on the receiver with its mode and
    /// WITHOUT its contents, and the missing parents of a named file are
    /// created too. The exact push relies on both — every synced
    /// directory rides along with the differing leaves — so a future
    /// rsync that skips named directories must fail here, not in a
    /// workspace where an empty directory quietly never arrives.
    #[test]
    fn a_directory_named_to_a_selected_pass_is_created_without_recursing() {
        use std::os::unix::fs::PermissionsExt;
        let rsync = local_rsync().expect("rsync >= 3.2 — ulak states the same requirement");
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(from.join("site/empty")).unwrap();
        std::fs::create_dir_all(from.join("site/deep/er")).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("site/deep/er/f.txt"), b"f").unwrap();
        std::fs::write(from.join("site/deep/er/skip.txt"), b"s").unwrap();
        std::fs::set_permissions(
            from.join("site/empty"),
            std::fs::Permissions::from_mode(0o750),
        )
        .unwrap();

        let mut cmd = Command::new(&rsync);
        cmd.args([
            "--recursive",
            "--links",
            "--perms",
            "--times",
            "--itemize-changes",
            "--from0",
            "--no-recursive",
            "--files-from=-",
            "--ignore-missing-args",
            "--checksum",
        ]);
        cmd.arg("--")
            .arg(format!("{}/", from.display()))
            .arg(format!("{}/", to.display()));
        let out = crate::proc::run_bounded(
            &mut cmd,
            Some(b"site/empty\0site/deep/er/f.txt\0".to_vec()),
            crate::proc::PROBE,
        )
        .unwrap();
        assert_eq!(out.status.code(), Some(0), "rsync itself must not fail");
        assert_eq!(
            std::fs::metadata(to.join("site/empty"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750,
            "a named directory arrives with its mode"
        );
        assert_eq!(std::fs::read(to.join("site/deep/er/f.txt")).unwrap(), b"f");
        assert!(
            !to.join("site/deep/er/skip.txt").exists(),
            "naming a directory must not recurse into it"
        );
    }

    /// Which ledger-claimed, locally deleted files the server may lose. A
    /// copy the server changed since the last agreed content is kept; one
    /// with no agreed content at all — an older ulak's claim, a lost
    /// confirmation — is retired on the ledger's word, as before content
    /// records existed. Measured: refusing it left a file created and
    /// deleted within one confirmation round trip undeletable for good.
    /// A server copy ulak itself sent without a receipt is not a change:
    /// calling it one warned that the server had edited the file and
    /// brought the user's own deleted upload back.
    #[test]
    fn only_a_known_change_on_the_server_keeps_a_deleted_file_there() {
        let a = "f:420:a".to_owned();
        let b = "f:420:b".to_owned();
        let sent_b = std::collections::BTreeSet::from([b.clone()]);
        let sent_a = std::collections::BTreeSet::from([a.clone()]);
        assert!(moved_on_the_server(Some(&a), None, Some(&b)));
        assert!(
            moved_on_the_server(Some(&a), Some(&sent_a), Some(&b)),
            "a send of other bytes does not explain the server's copy"
        );
        assert!(
            !moved_on_the_server(Some(&a), Some(&sent_b), Some(&b)),
            "the server holds ulak's own unconfirmed upload"
        );
        assert!(!moved_on_the_server(Some(&a), None, Some(&a)));
        assert!(!moved_on_the_server(None, None, Some(&b)));
        assert!(!moved_on_the_server(Some(&a), None, None));
        assert!(!moved_on_the_server(None, None, None));
    }

    /// The deletion flow splits by that one rule: a row the server changed
    /// since the last agreed content is spared, and every other row —
    /// unchanged, never agreed on, already gone from the server, or holding
    /// ulak's own upload whose receipt was lost — is still deleted. One
    /// spared row used to stop the whole sync.
    #[test]
    fn only_the_rows_the_server_changed_are_spared_and_the_rest_are_deleted() {
        let local = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let state = project.state_key(&ssh.dest);
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let (a, b, c) = (
            "f:420:a".to_string(),
            "f:420:b".to_string(),
            "f:420:c".to_string(),
        );
        common
            .record(crate::baseline::Contents::from([
                ("edited.rs".into(), Some(a.clone())),
                ("gone.rs".into(), Some(a.clone())),
                ("same.rs".into(), Some(a.clone())),
                ("uploaded.rs".into(), Some(a.clone())),
            ]))
            .unwrap();
        common
            .record_unconfirmed(std::collections::BTreeMap::from([(
                "uploaded.rs".to_string(),
                c.clone(),
            )]))
            .unwrap();
        let remote = crate::baseline::Contents::from([
            ("edited.rs".into(), Some(b.clone())),
            ("gone.rs".into(), None),
            ("same.rs".into(), Some(a.clone())),
            ("unagreed.rs".into(), Some(b.clone())),
            ("uploaded.rs".into(), Some(c.clone())),
        ]);
        let doomed = [
            "edited.rs",
            "gone.rs",
            "same.rs",
            "unagreed.rs",
            "uploaded.rs",
        ]
        .map(String::from)
        .to_vec();
        let (deleted, spared) = split_moved(doomed, &common, &remote);
        assert_eq!(
            deleted,
            ["gone.rs", "same.rs", "unagreed.rs", "uploaded.rs"]
        );
        assert_eq!(spared, ["edited.rs"]);
    }

    /// A notice under a directory the rules drop but an include reaches
    /// into is rescanned as the full walk would walk it: the file the
    /// include rescues is in the plan, its excluded neighbour is not. A
    /// notice under one nothing reaches into still changes nothing.
    #[test]
    fn a_notice_an_include_rescues_is_rescanned_as_the_full_walk_would() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        let write = |rel: &str| {
            let p = anchor.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, rel).unwrap();
            p
        };
        for rel in ["app/a.rs", "secrets/keep/prod.env", "secrets/junk.tmp"] {
            write(rel);
        }
        let cfg = crate::config::SyncCfg {
            exclude: vec!["secrets/".into(), "node_modules/".into()],
            include: vec!["secrets/keep/*.env".into()],
            protect: vec![],
            max_delete: crate::config::DeleteBudget::Limit(25),
            no_history: crate::config::NoHistory::Send,
        };
        let fp = inventory_footprint(anchor);
        let previous = walk::plan_in(anchor, anchor, &cfg, &fp.build_filter()).unwrap();
        assert!(previous.kept.contains(&b"secrets/keep/prod.env".to_vec()));

        let rescued = write("secrets/keep/stage.env");
        write("secrets/keep/junk.tmp");
        let (plan, scopes) = rescanned_plan(&cfg, &fp, &previous, &[rescued].into_iter().collect())
            .unwrap()
            .expect("a rescan answers it");
        assert_eq!(scopes, [anchor.join("secrets/keep")]);
        let full = walk::plan_in(anchor, anchor, &cfg, &fp.build_filter()).unwrap();
        assert!(
            plan.kept.contains(&b"secrets/keep/stage.env".to_vec()),
            "a file the include rescues must not wait for the next full walk"
        );
        assert_eq!(plan.kept, full.kept);
        assert_eq!(plan.kept_dirs, full.kept_dirs);
        assert_eq!(plan.excludes, full.excludes);
        let noise = write("node_modules/pkg/x.js");
        let (same, scopes) = rescanned_plan(&cfg, &fp, &previous, &[noise].into_iter().collect())
            .unwrap()
            .expect("nothing reaches into node_modules");
        assert!(scopes.is_empty());
        assert_eq!(same.kept, previous.kept);
    }

    /// Rescanning the directories the notices name and splicing the walk's
    /// answer into the previous plan must give exactly the full walk's
    /// answer: the same kept leaves, kept directories and literal excludes,
    /// under the same ignore rules. A notice naming a directory, or a leaf
    /// whose parent is gone, is not rescanned but sent to the full path; a
    /// leaf under an excluded directory changes nothing.
    #[test]
    fn a_rescanned_directory_answers_as_the_full_walk_would() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        let write = |rel: &str| {
            let p = anchor.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, rel).unwrap();
            p
        };
        for rel in [
            "app/src/a.rs",
            "app/src/b.rs",
            "app/src/old.log",
            "app/build/out.o",
            "lib/c.rs",
            "docs/d.md",
        ] {
            write(rel);
        }
        let cfg = crate::config::SyncCfg {
            exclude: vec!["*.log".into(), "build/".into()],
            include: vec![],
            protect: vec![],
            max_delete: crate::config::DeleteBudget::Limit(25),
            no_history: crate::config::NoHistory::Send,
        };
        let fp = inventory_footprint(anchor);
        let full = || {
            let mut plan = walk::plan_in(anchor, anchor, &cfg, &fp.build_filter()).unwrap();
            plan.kept.sort();
            plan.kept_dirs.sort();
            plan.excludes.sort();
            plan
        };
        let previous = full();
        assert!(previous.kept.contains(&b"app/src/a.rs".to_vec()));
        assert!(previous.excludes.contains(&b"app/build".to_vec()));

        // Leaves appear, vanish and turn out ignored in two directories.
        let new = write("app/src/new.rs");
        let log = write("app/src/new.log");
        let gone = anchor.join("lib/c.rs");
        std::fs::remove_file(&gone).unwrap();
        let paths = [new, log, gone].into_iter().collect();
        let (plan, scopes) = rescanned_plan(&cfg, &fp, &previous, &paths)
            .unwrap()
            .expect("leaves under known directories are rescanned");
        assert_eq!(scopes, [anchor.join("app/src"), anchor.join("lib")]);
        let expected = full();
        assert_eq!(plan.kept, expected.kept);
        assert_eq!(plan.kept_dirs, expected.kept_dirs);
        assert_eq!(plan.excludes, expected.excludes);
        assert!(plan.kept.contains(&b"app/src/new.rs".to_vec()));
        assert!(!plan.kept.contains(&b"lib/c.rs".to_vec()));
        assert!(plan.excludes.contains(&b"app/src/new.log".to_vec()));

        // A leaf under an excluded directory is not walked; nothing changes.
        let ignored = write("app/build/more.o");
        let (same, scopes) = rescanned_plan(&cfg, &fp, &previous, &[ignored].into_iter().collect())
            .unwrap()
            .unwrap();
        assert!(scopes.is_empty());
        assert_eq!(same.kept, previous.kept);

        // A known directory reported instead of its changed child is
        // rescanned itself; an unknown one is reported through its parent,
        // where the walker's builtin excludes still apply; a leaf whose
        // parent is gone belongs to the full path.
        let (again, scopes) = rescanned_plan(
            &cfg,
            &fp,
            &previous,
            &[anchor.join("app/src")].into_iter().collect(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(scopes, [anchor.join("app/src")]);
        let mut only_app_src = previous.kept.clone();
        only_app_src.push(b"app/src/new.rs".to_vec());
        only_app_src.sort();
        assert_eq!(
            again.kept, only_app_src,
            "a rescan of app/src learns its new leaf and nothing about lib/"
        );
        std::fs::create_dir(anchor.join("app/src/.ulak-partial")).unwrap();
        std::fs::write(anchor.join("app/src/.ulak-partial/tmp"), "half").unwrap();
        let (staged, scopes) = rescanned_plan(
            &cfg,
            &fp,
            &previous,
            &[anchor.join("app/src/.ulak-partial")].into_iter().collect(),
        )
        .unwrap()
        .expect("rsync's staging directory is excluded by the parent's rescan");
        assert_eq!(scopes, [anchor.join("app/src")]);
        assert_eq!(staged.kept, only_app_src);
        assert!(staged.excludes.contains(&b"app/src/.ulak-partial".to_vec()));
        std::fs::create_dir(anchor.join("app/src/sub")).unwrap();
        std::fs::write(anchor.join("app/src/sub/deep.rs"), "x").unwrap();
        let (grown, _) = rescanned_plan(
            &cfg,
            &fp,
            &previous,
            &[anchor.join("app/src/sub")].into_iter().collect(),
        )
        .unwrap()
        .unwrap();
        assert!(
            grown.kept_dirs.contains(&b"app/src/sub".to_vec())
                && grown.kept.contains(&b"app/src/sub/deep.rs".to_vec()),
            "a new directory is listed by the parent's rescan; its leaves have no known parent yet"
        );
        // A leaf inside a directory the rules exclude but the literal list
        // could not name yet (rsync's staging) is never walked; once that
        // directory has vanished with its temporary, the notice is read
        // through the nearest surviving ancestor.
        let temporary = [anchor.join("app/src/.ulak-partial/gone.tmp")]
            .into_iter()
            .collect();
        let (untouched, scopes) = rescanned_plan(&cfg, &fp, &previous, &temporary)
            .unwrap()
            .unwrap();
        assert!(scopes.is_empty());
        assert_eq!(untouched.kept, previous.kept);
        std::fs::remove_dir_all(anchor.join("app/src/.ulak-partial")).unwrap();
        let (_, scopes) = rescanned_plan(&cfg, &fp, &previous, &temporary)
            .unwrap()
            .unwrap();
        assert_eq!(scopes, [anchor.join("app/src")]);
    }

    /// A refused push batch is not an invitation to rsync the same
    /// selection: only a create's missing parent is. A leaf
    /// the server changed
    /// since the survey is held, and a claim made fresh for a leaf the
    /// exchange never sent is retired — otherwise a later local delete
    /// would license removing the server's own file.
    #[test]
    fn a_refused_batch_holds_what_moved_and_retires_what_it_never_sent() {
        let selected: Vec<String> = ["a.rs", "app/first.rs", "b.rs", "big.bin", "new.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let refused = vec![
            // A first population: the root `app/` is not on the server yet.
            ("app/first.rs".to_string(), "missing parent".to_string()),
            ("b.rs".to_string(), "differs".to_string()),
            ("big.bin".to_string(), "needs rsync".to_string()),
            ("new.rs".to_string(), "exists".to_string()),
        ];
        let (rsync, held) = split_refusal(&selected, &refused);
        assert_eq!(rsync, ["a.rs", "app/first.rs"]);
        assert_eq!(
            held.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            ["b.rs", "big.bin", "new.rs"]
        );

        let written = [b"a.rs".to_vec(), b"new.rs".to_vec()];
        let fresh = fresh_claims(&written, |name| name == "a.rs");
        assert_eq!(
            fresh,
            ["new.rs"],
            "only a claim the ledger did not hold before is fresh"
        );

        // Pinned on the source: both exchanges retire fresh claims when
        // nothing was applied, and do so before the error is raised.
        let mine = include_str!("sync.rs");
        for name in ["fn stream_structure", "fn push_and_claim"] {
            let at = mine.find(name).unwrap();
            let body = &mine[at..][..mine[at..].find("\n}\n").unwrap()];
            assert!(
                body.contains("fresh_claims(") && body.matches("ledger::store(").count() >= 2,
                "{name} must claim before the exchange and retire the fresh claims after a refusal"
            );
        }
    }

    /// A file the footprint names one by one is claimed like any other —
    /// unless an exclude keeps the folder it sits in local, where rsync's
    /// literal exclude and the pull's inventory would never carry it and
    /// only the channel would have.
    #[test]
    fn a_named_file_under_an_excluded_folder_is_not_claimed() {
        let local = tempfile::tempdir().unwrap();
        let (mut project, _ssh) = helper_project(local.path());
        let anchor = local.path().to_path_buf();
        std::fs::create_dir_all(anchor.join("config")).unwrap();
        std::fs::write(anchor.join("config/app.env"), "A=1\n").unwrap();
        std::fs::write(anchor.join("config/other.conf"), "x\n").unwrap();
        let entry = |rel: &str, is_dir: bool, why: crate::footprint::Why| crate::footprint::Entry {
            local: anchor.join(rel),
            is_dir,
            exists: true,
            empty: false,
            why,
            service: "app".into(),
            writable: false,
        };
        let fp = Footprint {
            anchor: anchor.clone(),
            entries: vec![
                entry("config", true, crate::footprint::Why::Volume),
                entry("config/app.env", false, crate::footprint::Why::EnvFile),
            ],
            server_refs: Vec::new(),
            whole_anchor: false,
            contexts: Vec::new(),
            pinned: Vec::new(),
            model_json: String::new(),
        };

        let kept = checked_plan(&project, &fp).unwrap().kept;
        assert!(kept.contains(&b"config/app.env".to_vec()), "{kept:?}");

        project.config.sync.exclude = vec!["config/".into()];
        let plan = checked_plan(&project, &fp).unwrap();
        assert!(plan.kept.is_empty(), "{:?}", plan.kept);
        assert_eq!(plan.excludes, [b"config".to_vec()]);
    }

    /// A project whose anchor is `local`, bound to a destination that is
    /// never contacted: the exchange under test goes to the real helper
    /// serving `remote` instead.
    fn helper_project(local: &std::path::Path) -> (Project, Ssh) {
        std::fs::write(local.join("compose.yaml"), "services: {}\n").unwrap();
        let inv =
            crate::invocation::Invocation::capture_in(local, &[], Default::default()).unwrap();
        let project = Project {
            config_home: local.to_path_buf(),
            anchor: local.to_path_buf(),
            name: "channel".into(),
            identity: "channel".into(),
            stack_pin: None,
            config: Default::default(),
            workspace_key: crate::config::WorkspaceKey::from_namespace("channel", local).unwrap(),
            inv,
        };
        let ssh = Ssh::new_in("unused", &Default::default(), false, local).unwrap();
        (project, ssh)
    }

    /// The helper refusing a replace because the server's copy moved since
    /// the survey is a conflict, not an invitation: the answer names the
    /// refusal, and the server's new work is still there afterwards.
    /// Measured: the refusal collapsed into the rsync fallback,
    /// which sent the stale selection and wrote over the server's change.
    #[test]
    fn a_content_refusal_over_the_channel_is_a_conflict_not_an_rsync_fallback() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let rel = "file.rs";
        let rp = remote.path().join(rel);
        std::fs::write(&rp, b"common ancestor").unwrap();
        let ancestor = crate::baseline::fingerprint(&rp).unwrap();
        let lp = local.path().join(rel);
        std::fs::write(&lp, b"local edit").unwrap();
        let surveyed = std::collections::BTreeMap::from([(rel.to_string(), ancestor)]);
        let replace = || {
            let mut batch = crate::stream::Batch::new();
            batch
                .add(
                    local.path(),
                    &lp,
                    &crate::selection::Stamp::read(&lp).unwrap(),
                    false,
                )
                .unwrap();
            // The cold push guards on the survey's view of the server, so
            // a file with no common record still goes (no_history = send).
            batch.guard(&surveyed);
            batch
        };
        let mut channel = Some(crate::stream::local_helper(remote.path()));
        // The accepting case first: the server still holds what was surveyed.
        let sent = channel_send(&project, &ssh, &mut channel, replace()).unwrap();
        assert!(
            matches!(sent, Sent::Applied(_)),
            "an unmoved server copy is replaced"
        );
        assert_eq!(std::fs::read(&rp).unwrap(), b"local edit");
        // Then the server writes between the survey and the exchange.
        std::fs::write(&rp, b"new server work").unwrap();
        let sent = channel_send(&project, &ssh, &mut channel, replace()).unwrap();
        let Sent::Refused(refused) = sent else {
            panic!("a content refusal must come back by name, never as the rsync fallback");
        };
        assert_eq!(refused[0].0, rel);
        assert_ne!(refused[0].1, "needs rsync");
        assert_eq!(std::fs::read(&rp).unwrap(), b"new server work");
        let selected = vec![rel.to_string()];
        let (rsync, held) = split_refusal(&selected, &refused);
        assert!(rsync.is_empty(), "nothing of this batch may go to rsync");
        assert_eq!(held[0].0, rel);
    }

    /// A server copy past the channel's size limit is compared before the
    /// helper answers. Unchanged since the survey, the small local bytes
    /// replace it over the channel with no rsync at all; grown past the
    /// limit since the survey, it is a server that moved and is held.
    /// Measured: the helper answered "needs rsync" before
    /// comparing, and the stale selection went to rsync over the change.
    #[test]
    fn a_large_server_copy_is_compared_before_the_channel_answers() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let (rp, lp) = (remote.path().join("file.rs"), local.path().join("file.rs"));
        let send = |surveyed: &crate::baseline::Contents| {
            let mut batch = crate::stream::Batch::new();
            batch
                .add(
                    local.path(),
                    &lp,
                    &crate::selection::Stamp::read(&lp).unwrap(),
                    false,
                )
                .unwrap();
            batch.guard(surveyed);
            batch
        };
        let mut channel = Some(crate::stream::local_helper(remote.path()));

        // Accepting: a large server copy, unchanged since it was surveyed.
        std::fs::write(&rp, vec![b'S'; 1024 * 1024 + 1]).unwrap();
        let surveyed = std::collections::BTreeMap::from([(
            "file.rs".to_string(),
            crate::baseline::fingerprint(&rp).unwrap(),
        )]);
        std::fs::write(&lp, b"local edit").unwrap();
        let sent = channel_send(&project, &ssh, &mut channel, send(&surveyed)).unwrap();
        assert!(
            matches!(sent, Sent::Applied(_)),
            "an unmoved large copy is replaced"
        );
        assert_eq!(std::fs::read(&rp).unwrap(), b"local edit");

        // Refusing: small when surveyed, grown past the limit since.
        let surveyed = std::collections::BTreeMap::from([(
            "file.rs".to_string(),
            crate::baseline::fingerprint(&rp).unwrap(),
        )]);
        let remote_work = vec![b'R'; 1024 * 1024 + 1];
        std::fs::write(&rp, &remote_work).unwrap();
        std::fs::write(&lp, b"a second local edit").unwrap();
        let Sent::Refused(refused) =
            channel_send(&project, &ssh, &mut channel, send(&surveyed)).unwrap()
        else {
            panic!("a server copy that moved must be refused");
        };
        assert_eq!(std::fs::read(&rp).unwrap(), remote_work);
        assert_eq!(refused[0].1, "differs", "compared, not waved on by size");
        let (rsync, held) = split_refusal(&["file.rs".to_string()], &refused);
        assert!(
            rsync.is_empty(),
            "the move must not reach rsync: {refused:?}"
        );
        assert_eq!(held[0].0, "file.rs");
    }

    /// A lost receipt proves nothing about what the helper applied, so the
    /// fresh claim stands and the send is kept as unconfirmed. Deleting
    /// the local copy then retires the server's copy — the upload — rather
    /// than leaving it unclaimed for a pull to bring back. Measured: the
    /// claim was retired on the transport error, and the
    /// locally deleted upload became an unclaimed pull candidate.
    #[test]
    fn a_lost_receipt_keeps_the_claim_and_remembers_what_was_sent() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let (project, mut ssh) = helper_project(local.path());
        ssh.offline();
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let lp = local.path().join("new.rs");
        std::fs::write(&lp, b"local new file").unwrap();
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let mut session = Session {
            stream: Some(crate::stream::local_helper_losing_receipts(remote.path())),
            ..Session::default()
        };
        let observed = std::collections::BTreeMap::from([(
            lp.clone(),
            crate::selection::Stamp::read(&lp).unwrap(),
        )]);
        let result = stream_structure(
            &project,
            &inventory_footprint(local.path()),
            &ssh,
            &mut session,
            &mut common,
            &state,
            &[b"new.rs".to_vec()],
            &[],
            &observed,
            true,
        );
        assert!(result.is_err(), "no receipt is an error");
        assert_eq!(
            std::fs::read(remote.path().join("new.rs")).unwrap(),
            b"local new file",
            "the helper applied the create before its receipt was lost"
        );
        std::fs::remove_file(&lp).unwrap();
        let unclaimed = crate::selection::unclaimed(
            b"new.rs\0",
            &ledger::synced_files(&state),
            &crate::selection::Excludes::new([]),
        )
        .unwrap();
        assert!(
            unclaimed.is_empty(),
            "the upload must not look server-born: {unclaimed:?}"
        );
        let sent = common.unconfirmed("new.rs").cloned();
        assert!(sent.is_some(), "what was sent is remembered");
        assert!(common.get("new.rs").is_none(), "and is not common content");

        // The deletion that follows: the server holds what was sent, so
        // the row is doomed like any other.
        let server = crate::baseline::fingerprint(&remote.path().join("new.rs")).unwrap();
        let doomed = ledger::doomed(&state, local.path(), &[]);
        assert_eq!(doomed, ["new.rs"]);
        let remote_view = crate::baseline::Contents::from([("new.rs".to_string(), server)]);
        let kept =
            retire_never_landed(&state, local.path(), &mut common, &remote_view, doomed).unwrap();
        assert_eq!(kept, ["new.rs"], "the upload is ulak's to delete");
    }

    /// The restart's push keeps the same record as the warm path. Here the
    /// server makes its own file after the survey, the helper refuses the
    /// create, and the refusal is lost with the network down: the claim
    /// stands, and after a restart and a local delete the server's file is
    /// still not ulak's to delete — the unconfirmed record, written before
    /// the exchange, says what ulak sent, and the server holds something
    /// else. Measured: the cold path recorded nothing, and the
    /// deletion was authorized even with no_history = hold.
    #[test]
    fn a_lost_refusal_on_the_restart_push_never_licenses_deleting_server_work() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let (mut project, mut ssh) = helper_project(local.path());
        project.config.sync.no_history = crate::config::NoHistory::Hold;
        ssh.offline();
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let lp = local.path().join("new.rs");
        std::fs::write(&lp, b"local new file").unwrap();
        let mut cache = crate::hashcache::Cache::open(&state, local.path());
        let local_content = cache.fingerprint(local.path(), "new.rs").unwrap();
        let mut survey = Survey {
            plan: walk::WalkReport {
                kept: vec![b"new.rs".to_vec()],
                ..Default::default()
            },
            local: crate::baseline::Contents::from([("new.rs".into(), local_content)]),
            remote: crate::baseline::Contents::from([("new.rs".into(), None)]),
            local_dirs: Default::default(),
            remote_dirs: Default::default(),
            listing: None,
            returning_when_listed: Default::default(),
            common: crate::baseline::State::load(&state, local.path()).unwrap(),
            cache,
        };
        let rp = remote.path().join("new.rs");
        std::fs::write(&rp, b"server's own work").unwrap();
        let mut channel = Some(crate::stream::local_helper_losing_receipts(remote.path()));
        let result = push_and_claim(
            &PathBuf::from("rsync-not-invoked"),
            &project,
            &inventory_footprint(local.path()),
            &ssh,
            &mut survey,
            &state,
            &crate::proc::Budget::new(crate::proc::RECONCILE),
            &mut channel,
        );
        assert!(result.is_err(), "the fence cannot reach an offline server");
        assert_eq!(std::fs::read(&rp).unwrap(), b"server's own work");
        std::fs::remove_file(&lp).unwrap();
        // A restart reloads durable state and takes the real deletion path.
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        assert!(
            common.unconfirmed("new.rs").is_some(),
            "recorded before the exchange"
        );
        let remote_view = crate::baseline::Contents::from([(
            "new.rs".into(),
            crate::baseline::fingerprint(&rp).unwrap(),
        )]);
        let doomed = ledger::doomed(&state, local.path(), &[]);
        assert_eq!(doomed, ["new.rs"], "the claim stands until it is judged");
        let doomed =
            retire_never_landed(&state, local.path(), &mut common, &remote_view, doomed).unwrap();
        assert!(doomed.is_empty(), "the server's own file is never deleted");
    }

    /// A survey of one local file, `new.rs`, against the server copy the
    /// caller names, as the restart's push would take it.
    fn one_file_survey(project: &Project, ssh: &Ssh, remote: Option<String>) -> Survey {
        let state = project.state_key(&ssh.dest);
        let mut cache = crate::hashcache::Cache::open(&state, &project.anchor);
        let local = cache.fingerprint(&project.anchor, "new.rs").unwrap();
        Survey {
            plan: walk::WalkReport {
                kept: vec![b"new.rs".to_vec()],
                ..Default::default()
            },
            local: crate::baseline::Contents::from([("new.rs".into(), local)]),
            remote: crate::baseline::Contents::from([("new.rs".into(), remote)]),
            local_dirs: Default::default(),
            remote_dirs: Default::default(),
            listing: None,
            returning_when_listed: Default::default(),
            common: crate::baseline::State::load(&state, &project.anchor).unwrap(),
            cache,
        }
    }

    /// The protective record is written before the claim, so a record
    /// that cannot be written leaves no claim behind — not a claim that,
    /// once storage recovers and the local copy is deleted, licenses
    /// deleting a server file ulak never wrote. Measured with a
    /// read-only common-content directory and a writable ledger; here a
    /// FILE stands where the record's directory belongs, which fails the
    /// write for root as well.
    #[test]
    fn a_protective_record_that_cannot_be_written_leaves_no_claim() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let (mut project, mut ssh) = helper_project(local.path());
        project.config.sync.no_history = crate::config::NoHistory::Hold;
        ssh.offline();
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let lp = local.path().join("new.rs");
        std::fs::write(&lp, b"local new file").unwrap();
        let mut survey = one_file_survey(&project, &ssh, None);
        survey
            .common
            .record(crate::baseline::Contents::from([(
                "unrelated.rs".into(),
                None,
            )]))
            .unwrap();
        let common_dir = crate::intent::workspace_state_dir_path(&state)
            .unwrap()
            .join("common");
        let journal = std::fs::read_dir(common_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let parked = journal.with_extension("parked");
        std::fs::rename(&journal, &parked).unwrap();
        std::fs::write(&journal, b"not a directory").unwrap();
        let rp = remote.path().join("new.rs");
        std::fs::write(&rp, b"server's own work").unwrap();
        let result = push_and_claim(
            &PathBuf::from("rsync-not-invoked"),
            &project,
            &inventory_footprint(local.path()),
            &ssh,
            &mut survey,
            &state,
            &crate::proc::Budget::new(crate::proc::RECONCILE),
            &mut Some(crate::stream::local_helper(remote.path())),
        );
        std::fs::remove_file(&journal).unwrap();
        std::fs::rename(&parked, &journal).unwrap();
        assert!(result.is_err(), "the record could not be written");
        assert_eq!(std::fs::read(&rp).unwrap(), b"server's own work");
        std::fs::remove_file(&lp).unwrap();
        assert!(
            ledger::doomed(&state, local.path(), &[]).is_empty(),
            "no claim was made without its record"
        );
    }

    /// A save between the survey and the send goes over the channel as it
    /// is now, and is recorded as what was sent. Sent by rsync instead, it
    /// put a version on the server that no record named, and every later
    /// sync took it for the server's own edit. Measured in CI: five saves
    /// 50 ms apart left the server on the third for good.
    #[test]
    fn a_save_after_the_survey_is_sent_as_it_is_now_and_recorded() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let (project, mut ssh) = helper_project(local.path());
        ssh.offline();
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let lp = local.path().join("new.rs");
        let rp = remote.path().join("new.rs");
        std::fs::write(&rp, b"agreed").unwrap();
        let agreed = crate::baseline::fingerprint(&rp).unwrap();
        std::fs::write(&lp, b"save 1").unwrap();
        let mut survey = one_file_survey(&project, &ssh, agreed.clone());
        survey
            .common
            .record(crate::baseline::Contents::from([("new.rs".into(), agreed)]))
            .unwrap();
        std::fs::write(&lp, b"save number two").unwrap();
        push_and_claim(
            &PathBuf::from("rsync-not-invoked"),
            &project,
            &inventory_footprint(local.path()),
            &ssh,
            &mut survey,
            &state,
            &crate::proc::Budget::new(crate::proc::RECONCILE),
            &mut Some(crate::stream::local_helper(remote.path())),
        )
        .expect("the channel carries the save, no rsync");
        assert_eq!(std::fs::read(&rp).unwrap(), b"save number two");
        let sent = crate::baseline::fingerprint(&lp).unwrap();
        assert_eq!(survey.common.get("new.rs"), sent.as_ref());
        assert_eq!(
            crate::baseline::State::load(&state, local.path())
                .unwrap()
                .get("new.rs"),
            sent.as_ref(),
            "the record names what the server holds"
        );
    }

    /// A retry whose receipt is lost again adds its content to the record
    /// instead of leaving the first attempt's: the second upload is the
    /// one on the server, and it must still be ulak's to delete, not a
    /// server-born file a pull would bring back.
    #[test]
    fn a_retry_whose_receipt_is_lost_again_is_recorded_too() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let (project, mut ssh) = helper_project(local.path());
        ssh.offline();
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let lp = local.path().join("new.rs");
        let rp = remote.path().join("new.rs");
        for bytes in [b"first upload".as_slice(), b"second upload".as_slice()] {
            std::fs::write(&lp, bytes).unwrap();
            let surveyed = crate::baseline::fingerprint(&rp).unwrap();
            let mut survey = one_file_survey(&project, &ssh, surveyed);
            let result = push_and_claim(
                &PathBuf::from("rsync-not-invoked"),
                &project,
                &inventory_footprint(local.path()),
                &ssh,
                &mut survey,
                &state,
                &crate::proc::Budget::new(crate::proc::RECONCILE),
                &mut Some(crate::stream::local_helper_losing_receipts(remote.path())),
            );
            assert!(result.is_err());
            assert_eq!(std::fs::read(&rp).unwrap(), bytes, "the helper applied it");
        }
        std::fs::remove_file(&lp).unwrap();
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let remote_view = crate::baseline::Contents::from([(
            "new.rs".into(),
            crate::baseline::fingerprint(&rp).unwrap(),
        )]);
        let doomed = ledger::doomed(&state, local.path(), &[]);
        let kept =
            retire_never_landed(&state, local.path(), &mut common, &remote_view, doomed).unwrap();
        assert_eq!(kept, ["new.rs"], "the second upload is ulak's to delete");
        let unclaimed = crate::selection::unclaimed(
            b"new.rs\0",
            &ledger::synced_files(&state),
            &crate::selection::Excludes::new([]),
        )
        .unwrap();
        assert!(unclaimed.is_empty(), "and never a pull candidate");
    }

    /// The other outcome of a lost receipt: the send never landed and the
    /// server holds its own file under the name. That file is not deleted
    /// on the unconfirmed claim's word; the claim is retired instead.
    #[test]
    fn an_unconfirmed_send_the_server_never_received_dooms_nothing() {
        let a = "f:420:aa".to_string();
        let b = "f:420:bb".to_string();
        let sent = std::collections::BTreeSet::from([a.clone()]);
        let both = std::collections::BTreeSet::from([a.clone(), b.clone()]);
        assert!(never_landed(None, Some(&sent), Some(&b)));
        assert!(
            !never_landed(None, Some(&sent), Some(&a)),
            "ours: deletable"
        );
        assert!(
            !never_landed(None, Some(&both), Some(&b)),
            "any attempt is ours"
        );
        assert!(
            !never_landed(None, Some(&sent), None),
            "gone: nothing to protect"
        );
        assert!(
            !never_landed(None, None, Some(&b)),
            "no send on record: the ledger's word"
        );
        assert!(
            !never_landed(Some(&b), Some(&sent), Some(&b)),
            "agreement outranks a guess"
        );

        let local = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        ledger::store(&state, local.path(), &[b"theirs.rs".to_vec()], &[], &[]);
        common
            .record_unconfirmed(std::collections::BTreeMap::from([(
                "theirs.rs".to_string(),
                a.clone(),
            )]))
            .unwrap();
        let remote_view =
            crate::baseline::Contents::from([("theirs.rs".to_string(), Some(b.clone()))]);
        let doomed = ledger::doomed(&state, local.path(), &[]);
        assert_eq!(doomed, ["theirs.rs"], "the claim dooms it until judged");
        let kept =
            retire_never_landed(&state, local.path(), &mut common, &remote_view, doomed).unwrap();
        assert!(kept.is_empty(), "the server's own file is never deleted");
        assert!(
            ledger::doomed(&state, local.path(), &[]).is_empty(),
            "claim retired"
        );
        assert!(
            common.unconfirmed("theirs.rs").is_none(),
            "and the send settled"
        );
    }

    /// A create the helper refused because the server already held a
    /// different file leaves no ledger claim behind: otherwise deleting
    /// the local copy later would doom the server's own file, even under
    /// no_history = hold. Measured with exactly this sequence.
    #[test]
    fn a_refused_create_leaves_no_claim_that_could_doom_the_servers_file() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let (mut project, ssh) = helper_project(local.path());
        project.config.sync.no_history = crate::config::NoHistory::Hold;
        let rel = "file.rs";
        let rp = remote.path().join(rel);
        std::fs::write(&rp, b"server's own file").unwrap();
        let lp = local.path().join(rel);
        std::fs::write(&lp, b"local edit").unwrap();
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let mut session = Session {
            stream: Some(crate::stream::local_helper(remote.path())),
            ..Session::default()
        };
        let fp = inventory_footprint(local.path());
        let observed = std::collections::BTreeMap::from([(
            lp.clone(),
            crate::selection::Stamp::read(&lp).unwrap(),
        )]);
        let result = stream_structure(
            &project,
            &fp,
            &ssh,
            &mut session,
            &mut common,
            &state,
            &[rel.as_bytes().to_vec()],
            &[],
            &observed,
            true,
        );
        assert!(
            result.is_err(),
            "the create is refused: the server holds different bytes"
        );
        assert_eq!(std::fs::read(&rp).unwrap(), b"server's own file");
        assert!(common.get(rel).is_none());
        std::fs::remove_file(&lp).unwrap();
        let doomed = ledger::doomed(&state, local.path(), &[]);
        assert!(
            !doomed.contains(&rel.to_string()),
            "a claim the exchange never honoured must not survive it: {doomed:?}"
        );
    }

    /// A doomed row the SSH user may not read is refused as "unreadable"
    /// and handed to the rsync fallback, which holds it and deletes the
    /// rest; the channel survives. Measured: the helper raised on the
    /// permission error, the channel ended, and every structural batch that
    /// named the row failed its service tick until the file became readable.
    #[test]
    fn a_doomed_row_the_ssh_user_cannot_read_goes_to_the_rsync_fallback_not_a_dead_channel() {
        use std::os::unix::fs::PermissionsExt;
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let fp = inventory_footprint(local.path());
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let locked = remote.path().join("master.key");
        std::fs::write(&locked, b"agreed").unwrap();
        common
            .record(crate::baseline::Contents::from([(
                "master.key".into(),
                crate::baseline::fingerprint(&locked).unwrap(),
            )]))
            .unwrap();
        ledger::store(&state, local.path(), &[b"master.key".to_vec()], &[], &[]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut session = Session {
            stream: Some(crate::stream::local_helper(remote.path())),
            ..Session::default()
        };
        let answer = stream_structure(
            &project,
            &fp,
            &ssh,
            &mut session,
            &mut common,
            &state,
            &[],
            &["master.key".to_string()],
            &Default::default(),
            true,
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            matches!(answer, Ok(Structure::Rsync)),
            "the row goes to the rsync fallback, not an error"
        );
        assert!(session.stream.is_some(), "the channel survives");
        assert_eq!(std::fs::read(&locked).unwrap(), b"agreed");
        assert_eq!(ledger::doomed(&state, local.path(), &[]), ["master.key"]);
    }

    /// A claimed file deleted here whose server copy changed since the last
    /// agreed content no longer stops the sync. Over the channel the helper
    /// refuses that unlink and the warm path steps aside with nothing
    /// applied; the full path then retires the claim, so the pull's
    /// creating pass sees a server-born file and brings the server's edit
    /// home. A row the server left alone is still deleted. Both paths used
    /// to return a conflict error, on every sync, until someone reconciled
    /// the file by hand.
    #[test]
    fn a_deleted_file_the_server_changed_comes_back_instead_of_stopping_the_sync() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let fp = inventory_footprint(local.path());
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        // Both files were synced, then deleted here: claimed, agreed on,
        // and absent from this machine.
        let mut agreed = crate::baseline::Contents::new();
        for name in ["edited.rs", "untouched.rs"] {
            let rp = remote.path().join(name);
            std::fs::write(&rp, b"agreed").unwrap();
            agreed.insert(name.into(), crate::baseline::fingerprint(&rp).unwrap());
            ledger::store(&state, local.path(), &[name.as_bytes().to_vec()], &[], &[]);
        }
        common.record(agreed).unwrap();
        // The server then edits one of them.
        let edited = remote.path().join("edited.rs");
        std::fs::write(&edited, b"server edit").unwrap();
        let mut session = Session {
            stream: Some(crate::stream::local_helper(remote.path())),
            ..Session::default()
        };
        let mut unlink = |common: &mut crate::baseline::State, name: &str| {
            stream_structure(
                &project,
                &fp,
                &ssh,
                &mut session,
                common,
                &state,
                &[],
                &[name.to_string()],
                &Default::default(),
                true,
            )
            .unwrap()
        };

        // Accepting: the untouched server copy is deleted over the channel.
        let applied = unlink(&mut common, "untouched.rs");
        assert!(matches!(applied, Structure::Applied(ref r) if r.deleted == 1));
        assert!(!remote.path().join("untouched.rs").exists());
        // Refusing: the edited copy stays and nothing fails; the warm path
        // steps aside with the claim untouched, for the full path to judge.
        let refused = unlink(&mut common, "edited.rs");
        assert!(matches!(refused, Structure::Full));
        assert_eq!(std::fs::read(&edited).unwrap(), b"server edit");
        let doomed = ledger::doomed(&state, local.path(), &[]);
        assert_eq!(doomed, ["edited.rs"]);

        // The full path's judgement, over the server's copy as it reads it.
        common
            .record_unconfirmed(std::collections::BTreeMap::from([(
                "edited.rs".into(),
                "f:420:lost".into(),
            )]))
            .unwrap();
        assert!(common.unconfirmed("edited.rs").is_some());
        let remote_view = crate::baseline::Contents::from([(
            "edited.rs".into(),
            crate::baseline::fingerprint(&edited).unwrap(),
        )]);
        let doomed = retire_moved(&state, local.path(), &mut common, &remote_view, doomed).unwrap();
        assert!(doomed.is_empty(), "the server's edit is never deleted");
        assert!(
            crate::baseline::State::load(&state, local.path())
                .unwrap()
                .returning("edited.rs"),
            "and it is marked for the pull that lands it, whichever that is"
        );
        assert_eq!(std::fs::read(&edited).unwrap(), b"server edit");
        assert!(
            ledger::doomed(&state, local.path(), &[]).is_empty(),
            "the claim is retired"
        );
        assert!(
            common.unconfirmed("edited.rs").is_none(),
            "and any unconfirmed send under the name settled"
        );
        let unclaimed = crate::selection::unclaimed(
            b"edited.rs\0",
            &ledger::synced_files(&state),
            &crate::selection::Excludes::new([]),
        )
        .unwrap();
        assert_eq!(
            unclaimed,
            [b"edited.rs".to_vec()],
            "the pull's creating pass brings the server's copy home"
        );

        // Pinned on the source: the full push judges the rows before the
        // budget counts them — a spared row was counted, prompted for and
        // recorded as pending, and a service over budget never retired its
        // claim — and before it lists or removes the rest. A moved row
        // never stops it.
        let mine = include_str!("sync.rs");
        let at = mine.find("\nfn push(").expect("the full push");
        let body = &mine[at..][..mine[at..].find("\n}\n").expect("it ends")];
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("the full push lost `{needle}`"))
        };
        for later in [
            "budget.allows(",
            "report.pending_deletions = doomed.len()",
            "confirm_deletions(",
            "ui::dim(&format!(\"- {p}\"))",
            "delete_remote(",
        ] {
            assert!(
                at("judge_doomed(") < at(later),
                "the full push must judge the rows before `{later}`"
            );
        }
        assert!(!body.contains("content_conflict("));
        let at = mine.find("\nfn judge_doomed(").expect("the judgement");
        let judge = &mine[at..][..mine[at..].find("\n}\n").expect("it ends")];
        for judged in ["retire_never_landed(", "retire_moved("] {
            assert!(judge.contains(judged), "the judgement lost `{judged}`");
        }
    }

    /// A file `retire_moved` spared is marked returning in the
    /// common-content state, so whichever pull lands it claims it again:
    /// this sync's, a passthrough's `pull_back`, or the service's warm
    /// pull, in this process or a later one. The mark used to be a list
    /// handed from push to pull in memory, which only `ulak sync` did:
    /// after `ulak docker compose up` the file landed unclaimed, the
    /// second local deletion doomed nothing, and every command's pull
    /// brought it back without a word. Only a returning name that landed
    /// is claimed; one held back stays marked for a later pull. The e2e
    /// test `a_deleted_file_the_server_changed_comes_back_and_the_sync_goes_on`
    /// drives the second deletion through `ulak sync` and through a
    /// passthrough.
    #[test]
    fn a_returning_file_any_later_pull_lands_is_claimed_again_so_a_second_deletion_goes_through() {
        let local = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let (a, b) = ("f:420:a".to_string(), "f:420:b".to_string());
        // Synced and agreed on, deleted here, then edited on the server.
        let mut agreed = crate::baseline::Contents::new();
        for name in ["edited.rs", "held.rs"] {
            ledger::store(&state, local.path(), &[name.as_bytes().to_vec()], &[], &[]);
            agreed.insert(name.into(), Some(a.clone()));
        }
        common.record(agreed).unwrap();
        let remote = crate::baseline::Contents::from([
            ("edited.rs".into(), Some(b.clone())),
            ("held.rs".into(), Some(b.clone())),
        ]);
        let doomed = ledger::doomed(&state, local.path(), &[]);
        let doomed = retire_moved(&state, local.path(), &mut common, &remote, doomed).unwrap();
        assert!(doomed.is_empty());
        drop(common);

        // A later command's pull, which holds nothing of the push in
        // memory: its creating pass lands edited.rs and a file the server
        // made, holds held.rs back, records what landed and claims.
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let created = [
            Event::Push("edited.rs".into()),
            Event::Push("theirs.rs".into()),
            Event::Push("dir/".into()),
        ];
        let landed = landed_back(&common, &created);
        assert_eq!(landed, ["edited.rs"], "only a returning name that landed");
        common
            .record(crate::baseline::Contents::from([(
                "edited.rs".into(),
                Some(b.clone()),
            )]))
            .unwrap();
        claim_returned(&state, local.path(), &mut common, &landed).unwrap();
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        assert!(!common.returning("edited.rs"), "its return ended");
        assert!(
            common.returning("held.rs"),
            "one that did not land stays marked"
        );
        let unclaimed = crate::selection::unclaimed(
            b"edited.rs\0held.rs\0theirs.rs\0",
            &ledger::synced_files(&state),
            &crate::selection::Excludes::new([]),
        )
        .unwrap();
        assert_eq!(
            unclaimed,
            [b"held.rs".to_vec(), b"theirs.rs".to_vec()],
            "the landed file is claimed again, and nothing else is"
        );

        // Deleted here again: doomed, and deleted this time.
        let doomed = ledger::doomed(&state, local.path(), &[]);
        assert_eq!(doomed, ["edited.rs"], "the claim is back");
        let doomed = retire_moved(&state, local.path(), &mut common, &remote, doomed).unwrap();
        assert_eq!(
            doomed,
            ["edited.rs"],
            "the server's copy is the one both ends agreed on"
        );

        // Pinned on the source: both pulls that land created files claim
        // the returning ones after recording what landed, and no list of
        // spared rows travels from the push any more.
        let mine = include_str!("sync.rs");
        let body = |head: &str| {
            let at = mine.find(head).unwrap_or_else(|| panic!("{head} is gone"));
            &mine[at..][..mine[at..].find("\n}\n").expect("it ends")]
        };
        for (head, recorded) in [
            ("\nfn pull(", "survey.confirm("),
            ("\nfn pull_changes(", "remember_common("),
        ] {
            let body = body(head);
            let find = |needle: &str| {
                body.find(needle)
                    .unwrap_or_else(|| panic!("{head} lost `{needle}`"))
            };
            assert!(find("landed_back(") < find("claim_returned("), "{head}");
            assert!(
                find(recorded) < find("claim_returned("),
                "{head} claims before it records what landed"
            );
        }
        assert!(body("\npub fn pull_back(").contains("pull("));
        assert!(!body("\nfn transfer(").contains("spared"));
    }

    /// Every claim a push makes ends a return, and so do the two ways a
    /// name stops being ulak's: a removal the server confirmed, and a
    /// claim `never_landed` finds was never true. A mark that outlived
    /// them would claim a same-named file the server makes later, and a
    /// claim is a licence to delete it.
    #[test]
    fn a_return_ends_when_the_name_is_claimed_removed_or_found_to_be_the_servers() {
        let local = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let (a, b) = ("f:420:a".to_string(), "f:420:b".to_string());
        common
            .record(crate::baseline::Contents::from([(
                "removed.rs".into(),
                Some(a.clone()),
            )]))
            .unwrap();
        let names = ["removed.rs", "foreign.rs", "kept.rs"].map(String::from);
        common.mark_returning(&names).unwrap();

        forget_removed(&mut common, &["removed.rs".to_string()]).unwrap();
        // Claimed with a lost send and no agreed content, while the
        // server holds something else.
        ledger::store(&state, local.path(), &[b"foreign.rs".to_vec()], &[], &[]);
        common
            .record_unconfirmed(std::collections::BTreeMap::from([(
                "foreign.rs".to_string(),
                a.clone(),
            )]))
            .unwrap();
        let remote = crate::baseline::Contents::from([("foreign.rs".into(), Some(b.clone()))]);
        let doomed = retire_never_landed(
            &state,
            local.path(),
            &mut common,
            &remote,
            vec!["foreign.rs".to_string()],
        )
        .unwrap();
        assert!(doomed.is_empty());
        let common = crate::baseline::State::load(&state, local.path()).unwrap();
        assert!(!common.returning("removed.rs"), "removed on the server");
        assert!(!common.returning("foreign.rs"), "the server's own file");
        assert!(common.returning("kept.rs"), "nothing ended the third");

        // Pinned on the source: every function that claims rows in the
        // ledger also ends their returns.
        let mine = include_str!("sync.rs");
        let code = &mine[..mine.find("\n#[cfg(test)]").expect("the tests")];
        let mut claims = 0;
        for (at, _) in code.match_indices("ledger::store(") {
            if in_a_comment(code, at) {
                continue;
            }
            let args = &code[at + "ledger::store(".len()..];
            let mut depth = 0;
            let mut commas = Vec::new();
            for (i, c) in args.char_indices() {
                match c {
                    '(' | '[' => depth += 1,
                    ')' | ']' if depth == 0 => {
                        commas.push(i);
                        break;
                    }
                    ')' | ']' => depth -= 1,
                    ',' if depth == 0 => commas.push(i),
                    _ => {}
                }
            }
            let claimed = args[commas[1] + 1..commas[2]].trim();
            if claimed == "&[]" {
                continue;
            }
            let head = enclosing_fn(code, at);
            let body = &code[head..][..code[head..].find("\n}\n").expect("it ends")];
            assert!(
                body.contains("end_return("),
                "a claim of `{claimed}` that leaves any return standing:\n{}",
                body.lines().nth(1).unwrap_or_default()
            );
            claims += 1;
        }
        assert!(claims >= 4, "expected the claim sites, found {claims}");
    }

    /// A return ends once the server is seen without the file. A mark
    /// whose file the server lost before any pull landed it used to stand
    /// for good: nothing else ended it, so the next file of that name the
    /// server made was claimed as it landed, and deleting it here then
    /// removed the server's own file from the server. A listing ends only
    /// the marks it can answer: made before it was asked for, for a name it
    /// covers, and absent here. A later file of that name lands unclaimed,
    /// as server work does. A deletion notice ends a return on the warm
    /// path too.
    #[test]
    fn a_return_ends_when_the_server_is_seen_without_the_file_so_a_later_one_lands_unclaimed() {
        let local = tempfile::tempdir().unwrap();
        let server = tempfile::tempdir().unwrap();
        let (mut project, ssh) = helper_project(local.path());
        project.config.sync.protect = vec!["data".into()];
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let fp = inventory_footprint(local.path());
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        // Synced and agreed on, deleted here, then edited on the server:
        // the deletion is not applied, and the file is marked returning.
        let rp = server.path().join("gen.rs");
        std::fs::write(&rp, b"agreed").unwrap();
        common
            .record(crate::baseline::Contents::from([(
                "gen.rs".into(),
                crate::baseline::fingerprint(&rp).unwrap(),
            )]))
            .unwrap();
        ledger::store(&state, local.path(), &[b"gen.rs".to_vec()], &[], &[]);
        std::fs::write(&rp, b"server edit").unwrap();
        let remote = crate::baseline::local_remote(server.path(), &[b"gen.rs".to_vec()]).unwrap();
        let doomed = ledger::doomed(&state, local.path(), &[]);
        assert!(
            retire_moved(&state, local.path(), &mut common, &remote, doomed)
                .unwrap()
                .is_empty()
        );
        // Marks the listing cannot answer: under a protected path, a file
        // this machine holds, and one made after the listing was asked for.
        std::fs::write(local.path().join("here.rs"), b"mine").unwrap();
        common
            .mark_returning(&["data/db".into(), "here.rs".into()])
            .unwrap();
        let listed = common.returning_names();
        common.mark_returning(&["late.rs".into()]).unwrap();
        // The server loses gen.rs before any pull lands it.
        std::fs::remove_file(&rp).unwrap();
        drop(common);

        let pull_with = |returning_when_listed| {
            let script = inventory_script(
                server.path().to_str().unwrap(),
                &project.config.sync.protect,
                &fp,
                &[],
            );
            let mut sh = Command::new("sh");
            sh.arg("-s");
            let listed =
                crate::proc::run_bounded(&mut sh, Some(script.into_bytes()), crate::proc::SCRIPT)
                    .unwrap();
            assert!(listed.status.success());
            let mut survey = Survey {
                plan: walk::WalkReport {
                    // The test helper keeps its control files in the
                    // project; the real one keeps them beside it.
                    excludes: vec![b".control".to_vec()],
                    ..Default::default()
                },
                local: Default::default(),
                remote: Default::default(),
                local_dirs: Default::default(),
                remote_dirs: Default::default(),
                listing: Some(listed.stdout),
                returning_when_listed,
                common: crate::baseline::State::load(&state, local.path()).unwrap(),
                cache: crate::hashcache::Cache::open(&state, local.path()),
            };
            let mut channel = Some(crate::stream::local_helper(server.path()));
            let budget = crate::proc::Budget::new(crate::proc::RECONCILE);
            pull(
                &project,
                &fp,
                &ssh,
                true,
                &mut survey,
                &budget,
                None,
                &mut channel,
            )
            .unwrap();
            survey.finish().unwrap();
        };
        pull_with(listed);
        let common = crate::baseline::State::load(&state, local.path()).unwrap();
        assert!(
            !common.returning("gen.rs"),
            "the server was seen without it"
        );
        for kept in ["data/db", "here.rs", "late.rs"] {
            assert!(
                common.returning(kept),
                "{kept}: the listing cannot answer it"
            );
        }

        // Later the server makes a gen.rs of its own.
        std::fs::write(&rp, b"the server's own work").unwrap();
        pull_with(common.returning_names());
        let lp = local.path().join("gen.rs");
        assert_eq!(std::fs::read(&lp).unwrap(), b"the server's own work");
        assert!(
            !ledger::synced_files(&state).contains(&b"gen.rs".to_vec()),
            "server work lands unclaimed"
        );
        std::fs::remove_file(&lp).unwrap();
        assert!(
            ledger::doomed(&state, local.path(), &[]).is_empty(),
            "deleting it here dooms nothing on the server"
        );

        // Pinned on the source: the warm pull ends a return on the
        // server's deletion notice for it before it moves on.
        let mine = include_str!("sync.rs");
        let at = mine.find("\nfn pull_changes(").expect("the warm pull");
        let body = &mine[at..][..mine[at..].find("\n}\n").expect("it ends")];
        let deleted = body
            .find("if notice.deleted {")
            .expect("the deletion notice");
        let skipped = deleted + body[deleted..].find("continue;").expect("it moves on");
        assert!(
            body[deleted..skipped].contains(".end_return("),
            "a deletion notice for a returning file must end its return"
        );
    }

    /// A deletion a person confirmed at the over-budget prompt is judged
    /// again right before the removal. The prompt waits with no time
    /// limit and the removal script's `rm -f` compares nothing, so a server
    /// edit made while the list was open met a judgement taken before it,
    /// and was deleted. Over a fresh read the edited row is spared and
    /// marked returning, and the untouched one still goes.
    #[test]
    fn a_deletion_confirmed_at_the_prompt_is_judged_again_right_before_the_removal() {
        let local = tempfile::tempdir().unwrap();
        let server = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let mut agreed = crate::baseline::Contents::new();
        for name in ["migration.py", "old.py"] {
            let rp = server.path().join(name);
            std::fs::write(&rp, b"agreed").unwrap();
            agreed.insert(name.into(), crate::baseline::fingerprint(&rp).unwrap());
            ledger::store(&state, local.path(), &[name.as_bytes().to_vec()], &[], &[]);
        }
        common.record(agreed).unwrap();
        let judged = |common: &mut crate::baseline::State, doomed: Vec<String>| {
            let copies = crate::baseline::local_verdicts(server.path(), &doomed).unwrap();
            judge_doomed(&state, local.path(), local.path(), common, &copies, doomed).unwrap()
        };

        // Before the prompt, nothing has moved.
        let doomed = judged(&mut common, ledger::doomed(&state, local.path(), &[]));
        assert_eq!(doomed, ["migration.py", "old.py"]);
        // While the list is open, the stack regenerates one of them.
        std::fs::write(server.path().join("migration.py"), b"regenerated").unwrap();
        let doomed = judged(&mut common, doomed);
        assert_eq!(doomed, ["old.py"], "the regenerated file is not removed");
        assert!(common.returning("migration.py"));
        assert_eq!(
            ledger::doomed(&state, local.path(), &[]),
            ["old.py"],
            "and its claim is retired"
        );

        // Pinned on the source: after the prompt, the push judges again
        // before it removes anything.
        let mine = include_str!("sync.rs");
        let at = mine.find("\nfn push(").expect("the full push");
        let body = &mine[at..][..mine[at..].find("\n}\n").expect("it ends")];
        let prompt = body.find("confirm_deletions(").expect("the prompt");
        let again = body[prompt..]
            .find("judge_doomed(")
            .map(|i| prompt + i)
            .expect("a confirmed list is judged again");
        assert!(again < body.find("delete_remote(").expect("the removal"));
    }

    /// One read judges every doomed row, and no row stops the others. A
    /// row whose path the server restructured (a parent turned into a
    /// symlink or a file, a leaf turned into a FIFO) stays on the server
    /// and leaves ulak's tracking, named in one warning. A row that cannot
    /// be read right now (a file being written, one the SSH user may not
    /// read) is held: not deleted, still claimed, so the pull's creating
    /// pass never offers it, and named with its reason. Every other row is
    /// judged and deleted as usual, and no directory row above a row left
    /// alone is removed. Every such row used to count as restructured: a
    /// file caught mid-write lost its claim for good, a file the SSH user
    /// could not read became a creating-pass candidate that failed every
    /// later pull, and `rmdir -p` of a directory row under the symlinked
    /// parent removed an empty directory from the volume it pointed to. A
    /// dry run previews the same split without retiring anything.
    #[test]
    fn a_restructured_row_is_untracked_an_unreadable_one_is_held_and_the_rest_are_deleted() {
        use std::os::unix::fs::PermissionsExt;
        let local = tempfile::tempdir().unwrap();
        let server = tempfile::tempdir().unwrap();
        let volume = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let fp = inventory_footprint(local.path());
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let rp = |name: &str| server.path().join(name);
        // Synced once and agreed on, then deleted here, with the directory
        // rows the sync claimed on the way.
        let rows = [
            "busy.bin",
            "data/a.txt",
            "edited.txt",
            "gone.txt",
            "locked.key",
            "pipe",
            "plain/b.txt",
        ];
        for name in ["edited.txt", "gone.txt"] {
            std::fs::write(rp(name), b"agreed").unwrap();
        }
        let agreed = crate::baseline::fingerprint(&rp("gone.txt")).unwrap();
        for name in rows {
            ledger::store(&state, local.path(), &[name.as_bytes().to_vec()], &[], &[]);
            common
                .record(crate::baseline::Contents::from([(
                    name.to_string(),
                    agreed.clone(),
                )]))
                .unwrap();
        }
        ledger::store(
            &state,
            local.path(),
            &[b"data/".to_vec(), b"data/cache/".to_vec()],
            &[],
            &[],
        );
        // Then the server changes: `data/` becomes a symlink to a volume
        // holding an empty `cache/`, `plain` a file, `pipe` a FIFO; a
        // watcher rewrites `busy.bin` as it is read, `locked.key` is made
        // unreadable to the SSH user, and `edited.txt` is edited.
        std::fs::write(volume.path().join("a.txt"), b"volume data").unwrap();
        std::fs::create_dir(volume.path().join("cache")).unwrap();
        std::os::unix::fs::symlink(volume.path(), rp("data")).unwrap();
        std::fs::write(rp("plain"), b"a file now").unwrap();
        let fifo = std::process::Command::new("mkfifo")
            .arg(rp("pipe"))
            .status()
            .expect("mkfifo, from coreutils, to make a FIFO on the test server");
        assert!(fifo.success(), "mkfifo failed");
        std::fs::write(
            rp("busy.bin"),
            format!("{} generated", crate::baseline::REWRITTEN),
        )
        .unwrap();
        std::fs::write(rp("locked.key"), b"the server's own key").unwrap();
        std::fs::set_permissions(rp("locked.key"), std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            std::fs::File::open(rp("locked.key")).is_err(),
            "a mode-000 file is readable here: run the tests as a user other than root"
        );
        std::fs::write(rp("edited.txt"), b"server edit").unwrap();
        let doomed = ledger::doomed(&state, local.path(), &[]);
        assert_eq!(doomed, rows);
        let doomed_dirs = ledger::doomed_dirs(&state, local.path());
        assert_eq!(doomed_dirs, ["data/cache", "data"]);

        let copies = crate::baseline::local_verdicts_after(
            &crate::baseline::rewritten_while_read(&rp("busy.bin")),
            server.path(),
            &doomed,
        )
        .unwrap();
        let restructured = ["data/a.txt", "pipe", "plain/b.txt"].map(String::from);
        let held = ["busy.bin", "locked.key"].map(String::from);
        assert_eq!(
            copies.structural.keys().collect::<Vec<_>>(),
            restructured.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            copies.held.keys().collect::<Vec<_>>(),
            held.iter().collect::<Vec<_>>()
        );

        // A dry run previews the same split and retires nothing.
        let (deleted, kept) = preview_deletions(doomed.clone(), &common, &copies);
        assert_eq!(deleted, ["gone.txt"]);
        let kept_as = |row: &str, why: &str| {
            kept.iter()
                .any(|l| l.starts_with(&format!("kept: {row} ")) && l.contains(why))
        };
        for row in &restructured {
            assert!(
                kept_as(row, "changed the structure of its path"),
                "{row}: {kept:?}"
            );
        }
        for row in &held {
            assert!(kept_as(row, "cannot be read right now"), "{row}: {kept:?}");
        }
        assert!(kept_as("edited.txt", "changed on the server"), "{kept:?}");
        assert_eq!(
            ledger::doomed(&state, local.path(), &[]),
            rows,
            "a preview retires nothing"
        );

        // One warning names the restructured rows, each with its reason.
        let warning = restructured_warning(&copies, &restructured);
        for needed in [
            "data/a.txt (data is a symlink)",
            "pipe (pipe is a special file)",
            "plain/b.txt (plain is a file)",
            "stopped tracking them",
        ] {
            assert!(warning.contains(needed), "lost {needed:?}: {warning}");
        }
        // One names the held rows with theirs, and offers `[sync] protect`
        // for the one the SSH user may not read, and only for it.
        let warning =
            held_warning(&copies, &held, str::to_string).expect("the held rows are named");
        for needed in [
            "busy.bin (it changed while it was read)",
            "locked.key (the SSH user may not read it)",
            "the next sync tries again",
            "if the server owns locked.key, add it to [sync] protect in ulak.toml",
        ] {
            assert!(warning.contains(needed), "lost {needed:?}: {warning}");
        }
        let warning =
            held_warning(&copies, &held[..1], str::to_string).expect("the held row is named");
        assert!(!warning.contains("protect"), "{warning}");
        assert!(held_warning(&copies, &[], str::to_string).is_none());
        // The entry it offers is spelled the way ulak.toml reads it, from
        // the project directory, whatever the anchor is.
        let warning = held_warning(&copies, &held, |rel| format!("../web/{rel}"))
            .expect("the held rows are named");
        assert!(
            warning.contains("if the server owns ../web/locked.key, add it to [sync] protect"),
            "{warning}"
        );

        // The judgement: only the unrelated row's deletion stands.
        let doomed = judge_doomed(
            &state,
            local.path(),
            local.path(),
            &mut common,
            &copies,
            doomed,
        )
        .unwrap();
        assert_eq!(doomed, ["gone.txt"], "the unrelated row is still deleted");
        assert_eq!(
            ledger::doomed(&state, local.path(), &[]),
            ["busy.bin", "gone.txt", "locked.key"],
            "the restructured rows are no longer tracked, and the held ones still are"
        );
        let reloaded = crate::baseline::State::load(&state, local.path()).unwrap();
        for row in restructured.iter().chain(&held) {
            assert!(!reloaded.returning(row), "{row}: nothing is coming back");
        }
        assert!(
            reloaded.returning("edited.txt"),
            "the server's edit comes back"
        );

        // The pull's creating pass offers the server's own files and never
        // a held row, whose claim still stands.
        let script = inventory_script(server.path().to_str().unwrap(), &[], &fp, &[]);
        let mut sh = Command::new("sh");
        sh.arg("-s");
        let listed =
            crate::proc::run_bounded(&mut sh, Some(script.into_bytes()), crate::proc::SCRIPT)
                .unwrap();
        assert!(listed.status.success());
        let candidates = crate::selection::unclaimed(
            &listed.stdout,
            &ledger::synced_files(&state),
            &crate::selection::Excludes::new([]),
        )
        .unwrap();
        for row in &held {
            assert!(
                !candidates.contains(&row.as_bytes().to_vec()),
                "{row} is offered to the creating pass"
            );
        }
        for own in ["edited.txt", "plain"] {
            assert!(
                candidates.contains(&own.as_bytes().to_vec()),
                "{own}: the server's own file is not offered"
            );
        }

        // No directory row above a row left alone is removed. What the
        // push hands the removal script:
        let doomed_dirs = spare_dirs_above(doomed_dirs, &copies);
        assert_eq!(doomed_dirs, ["data/cache"]);
        let out = run_removal(server.path(), home.path(), &["gone.txt"], &["data/cache"]);
        assert!(out.contains("DELETED 1"), "{out}");
        assert!(!rp("gone.txt").exists());
        assert_eq!(
            std::fs::read(volume.path().join("a.txt")).unwrap(),
            b"volume data",
            "nothing is deleted through the symlink"
        );
        assert!(
            volume.path().join("cache").is_dir(),
            "no directory is removed through the symlink: {out}"
        );
        let removal = parse_removal(&out, &doomed, &["data/cache".to_string()]);
        assert!(removal.dirs.is_empty(), "the directory row stays claimed");
        assert_eq!(std::fs::read(rp("plain")).unwrap(), b"a file now");
        assert_eq!(std::fs::read(rp("edited.txt")).unwrap(), b"server edit");
        for row in ["busy.bin", "locked.key", "pipe"] {
            assert!(rp(row).exists(), "{row} was removed");
        }

        // Pinned on the source: both judgements in the push read the rows
        // this way, a failed read returns before anything is listed,
        // prompted for or recorded as pending, and each judgement spares
        // the directories before the removal.
        let mine = include_str!("sync.rs");
        let at = mine.find("\nfn push(").expect("the full push");
        let body = &mine[at..][..mine[at..].find("\n}\n").expect("it ends")];
        let judged = body.matches("judge_doomed(").count();
        assert_eq!(judged, 2, "both judgements");
        assert_eq!(
            body.matches("read_doomed(ssh, project, &doomed, deadline)?")
                .count(),
            judged,
            "every judgement reads the rows first, and stops on a failed read"
        );
        assert_eq!(
            body.matches("doomed_dirs = spare_dirs_above(doomed_dirs, &copies);")
                .count(),
            judged,
            "every judgement spares the directories above what it left alone"
        );
        assert!(body.rfind("spare_dirs_above(").unwrap() < body.find("delete_remote(").unwrap());
        let at = mine.find("\nfn judge_doomed(").expect("the judgement");
        let judge = &mine[at..][..mine[at..].find("\n}\n").expect("it ends")];
        assert!(
            judge
                .find("set_aside(")
                .expect("unjudged rows are set aside")
                < judge
                    .find("retire_never_landed(")
                    .expect("then the rest are judged")
        );
    }

    /// More rows than the old one-by-one fallback read, all under one
    /// parent the server turned into a symlink, are judged in the same
    /// single request as any round, and the sync goes on. Past 64 such
    /// rows the round used to stop every sync, pull and compose command
    /// with advice that could not work for rows under that parent.
    #[test]
    fn more_than_64_rows_under_a_restructured_parent_no_longer_stop_the_sync() {
        let local = tempfile::tempdir().unwrap();
        let server = tempfile::tempdir().unwrap();
        let volume = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
        let state = project.state_key(&ssh.dest);
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let under: Vec<String> = (0..100).map(|i| format!("data/{i:03}.txt")).collect();
        for row in &under {
            std::fs::write(volume.path().join(&row[5..]), b"volume data").unwrap();
        }
        std::os::unix::fs::symlink(volume.path(), server.path().join("data")).unwrap();
        std::fs::write(server.path().join("gone.txt"), b"agreed").unwrap();
        let rows: Vec<Vec<u8>> = under
            .iter()
            .map(String::as_str)
            .chain(["gone.txt"])
            .map(|p| p.as_bytes().to_vec())
            .collect();
        ledger::store(&state, local.path(), &rows, &[], &[]);
        let doomed = ledger::doomed(&state, local.path(), &[]);
        assert_eq!(doomed.len(), 101);

        let copies = crate::baseline::local_verdicts(server.path(), &doomed).unwrap();
        assert_eq!(copies.structural.len(), 100);
        let doomed = judge_doomed(
            &state,
            local.path(),
            local.path(),
            &mut common,
            &copies,
            doomed,
        )
        .unwrap();
        assert_eq!(doomed, ["gone.txt"]);
        assert_eq!(ledger::doomed(&state, local.path(), &[]), ["gone.txt"]);
        let out = run_removal(server.path(), home.path(), &["gone.txt"], &[]);
        assert!(out.contains("DELETED 1"), "{out}");
        assert_eq!(std::fs::read_dir(volume.path()).unwrap().count(), 100);

        // Pinned on the source: the read is one request, whatever its size.
        let mine = include_str!("sync.rs");
        let at = mine.find("\nfn read_doomed(").expect("the doomed read");
        let body = &mine[at..][..mine[at..].find("\n}\n").expect("it ends")];
        assert_eq!(body.matches("crate::baseline::verdicts(").count(), 1);
        assert!(!body.contains("for ") && !body.contains("loop"), "{body}");
    }

    /// The removal script keeps any row with a symlink on its way, checked
    /// right before its `rm -f` or `rmdir -p`, even for a row the judgement
    /// never saw restructured. Both commands resolve every component, and a
    /// directory row under a parent the server replaced with a symlink
    /// removed an empty directory from wherever the symlink pointed. A
    /// symlink LEAF is still removed itself, and a kept row stays claimed.
    #[test]
    fn the_removal_script_keeps_every_row_with_a_symlink_on_its_way() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        let volume = tmp.path().join("volume");
        touch(&volume.join("a.txt"));
        std::fs::create_dir_all(volume.join("cache")).unwrap();
        touch(&ws.join("real/b.txt"));
        std::fs::create_dir_all(ws.join("real/empty")).unwrap();
        std::os::unix::fs::symlink(&volume, ws.join("data")).unwrap();
        std::os::unix::fs::symlink(&volume, ws.join("real/link")).unwrap();
        std::os::unix::fs::symlink(&volume, ws.join("leaf")).unwrap();
        let files = ["data/a.txt", "leaf", "real/b.txt", "real/link/a.txt"];
        let dirs = ["data/cache", "real/empty", "real/link/cache"];

        let out = run_removal(&ws, tmp.path(), &files, &dirs);
        assert!(
            volume.join("a.txt").exists(),
            "a file went through a symlink: {out}"
        );
        assert!(
            volume.join("cache").is_dir(),
            "a directory went through a symlink: {out}"
        );
        assert!(
            std::fs::symlink_metadata(ws.join("leaf")).is_err(),
            "a symlink leaf is removed itself: {out}"
        );
        assert!(
            !ws.join("real/b.txt").exists() && !ws.join("real/empty").exists(),
            "{out}"
        );
        for kept in [
            "ULAK_KEPT_FILE data/a.txt",
            "ULAK_KEPT_FILE real/link/a.txt",
            "ULAK_KEPT_DIR data/cache",
            "ULAK_KEPT_DIR real/link/cache",
        ] {
            assert!(out.lines().any(|l| l == kept), "{kept} missing: {out}");
        }
        let own = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let removal = parse_removal(&out, &own(&files), &own(&dirs));
        assert_eq!(removal.files, ["leaf", "real/b.txt"]);
        assert_eq!(removal.dirs, ["real/empty"]);
        assert_eq!(removal.kept_files, 2);
    }

    /// A directory row is spared while any row under it is left alone this
    /// round, restructured or held, and only such a directory: a prefix
    /// that is not a parent (`datax` beside `data`) still goes.
    #[test]
    fn a_directory_above_a_row_left_alone_is_spared_that_round() {
        let copies = crate::baseline::Copies {
            structural: std::collections::BTreeMap::from([(
                "data/cache/a.txt".to_string(),
                "data is a symlink".to_string(),
            )]),
            held: std::collections::BTreeMap::from([(
                "keep/k".to_string(),
                crate::baseline::Held::default(),
            )]),
            ..Default::default()
        };
        let dirs = ["data/cache", "data", "datax", "keep", "other"].map(String::from);
        assert_eq!(spare_dirs_above(dirs.to_vec(), &copies), ["datax", "other"]);
        assert_eq!(
            spare_dirs_above(dirs.to_vec(), &Default::default()),
            dirs,
            "nothing left alone spares nothing"
        );
    }

    /// A claimed row under a `[sync] protect` path is never doomed, even
    /// one claimed before the path was protected. The hold warning for a
    /// file the SSH user may not read advises exactly that entry, and
    /// without this it changed nothing: the row stayed doomed, was held
    /// and named on every sync, and was deleted as soon as it could be
    /// read. Every place that asks what a sync would delete asks here.
    #[test]
    fn a_path_protected_after_it_was_claimed_is_never_doomed() {
        let local = tempfile::tempdir().unwrap();
        let (mut project, ssh) = helper_project(local.path());
        let state = project.state_key(&ssh.dest);
        ledger::store(
            &state,
            local.path(),
            &[
                b"config/".to_vec(),
                b"config/master.key".to_vec(),
                b"gone.txt".to_vec(),
            ],
            &[],
            &[],
        );
        assert_eq!(
            doomed_files(&project, &state, local.path(), &[]),
            ["config/master.key", "gone.txt"]
        );
        assert_eq!(
            doomed_directories(&project, &state, local.path()),
            ["config"]
        );
        project.config.sync.protect = vec!["/config/".into()];
        assert_eq!(
            doomed_files(&project, &state, local.path(), &[]),
            ["gone.txt"]
        );
        assert!(doomed_directories(&project, &state, local.path()).is_empty());

        // Pinned on the source: nothing else in the engine asks the ledger.
        let mine = include_str!("sync.rs");
        let code = &mine[..mine.find("\n#[cfg(test)]").expect("the tests")];
        let mut asked = 0;
        for (needle, owner) in [
            ("ledger::doomed(", "fn doomed_files("),
            ("ledger::doomed_dirs(", "fn doomed_directories("),
        ] {
            for (at, _) in code.match_indices(needle) {
                if in_a_comment(code, at) {
                    continue;
                }
                let head = enclosing_fn(code, at);
                assert!(
                    code[head..].trim_start().starts_with(owner),
                    "`{needle}` outside `{owner}`: {}",
                    line_at(code, at)
                );
                asked += 1;
            }
        }
        assert_eq!(asked, 2);
    }

    /// A lost replace is recorded as ulak's own send only under a name
    /// with agreed content, the one case `moved_on_the_server` reads the
    /// record for. A claim with no agreed content, such as one made before
    /// content records existed, stays the ledger's word. Recording its
    /// replace made `never_landed` read the claim as a send that never
    /// arrived: the deletion was not applied, and the server's copy came
    /// home unclaimed. Driven on all three channel paths, with the server
    /// still holding its own copy, as when the batch never arrived.
    #[test]
    fn a_lost_replace_of_a_claim_with_no_agreed_content_stays_the_ledgers_word() {
        let local = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let state = project.state_key(&ssh.dest);
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let (a, l) = ("f:420:a".to_string(), "f:420:l".to_string());
        common
            .record(crate::baseline::Contents::from([(
                "agreed.rs".into(),
                Some(a.clone()),
            )]))
            .unwrap();
        let accepted = crate::baseline::Contents::from([
            ("agreed.rs".into(), Some(l.clone())),
            ("unagreed.rs".into(), Some(l.clone())),
            ("unlinked.rs".into(), None),
        ]);
        assert_eq!(
            attempted(&common, &accepted),
            std::collections::BTreeMap::from([("agreed.rs".to_string(), l.clone())])
        );
        drop((local, common));

        for path in ["transfer_changed", "stream_structure", "push_and_claim"] {
            let local = tempfile::tempdir().unwrap();
            let remote = tempfile::tempdir().unwrap();
            let (project, mut ssh) = helper_project(local.path());
            ssh.offline();
            let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
            let state = project.state_key(&ssh.dest);
            let fp = inventory_footprint(local.path());
            let (lp, rp) = (
                local.path().join("config.yml"),
                remote.path().join("config.yml"),
            );
            // Claimed, with no agreed content on record; the copies differ.
            std::fs::write(&rp, b"server copy").unwrap();
            let server = crate::baseline::fingerprint(&rp).unwrap();
            ledger::store(&state, local.path(), &[b"config.yml".to_vec()], &[], &[]);
            std::fs::write(&lp, b"local copy").unwrap();
            let stamp = crate::selection::Stamp::read(&lp).unwrap();
            let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
            let losing = || crate::stream::local_helper_losing_receipts(remote.path());
            let failed = match path {
                "transfer_changed" => {
                    let mut session = Session {
                        stream: Some(losing()),
                        common: Some(common),
                        ..Session::default()
                    };
                    let changed = std::collections::BTreeMap::from([(lp.clone(), stamp)]);
                    transfer_changed(&project, &fp, &ssh, &mut session, &changed).is_err()
                }
                "stream_structure" => {
                    let mut session = Session {
                        stream: Some(losing()),
                        owned: std::collections::BTreeSet::from([lp.clone()]),
                        ..Session::default()
                    };
                    let observed = std::collections::BTreeMap::from([(lp.clone(), stamp)]);
                    stream_structure(
                        &project,
                        &fp,
                        &ssh,
                        &mut session,
                        &mut common,
                        &state,
                        &[b"config.yml".to_vec()],
                        &[],
                        &observed,
                        true,
                    )
                    .is_err()
                }
                _ => {
                    let mut survey = Survey {
                        plan: walk::WalkReport {
                            kept: vec![b"config.yml".to_vec()],
                            ..Default::default()
                        },
                        local: crate::baseline::Contents::from([(
                            "config.yml".into(),
                            crate::baseline::fingerprint(&lp).unwrap(),
                        )]),
                        remote: crate::baseline::Contents::from([(
                            "config.yml".into(),
                            server.clone(),
                        )]),
                        local_dirs: Default::default(),
                        remote_dirs: Default::default(),
                        listing: None,
                        returning_when_listed: Default::default(),
                        common,
                        cache: crate::hashcache::Cache::open(&state, local.path()),
                    };
                    let failed = push_and_claim(
                        &PathBuf::from("rsync-not-invoked"),
                        &project,
                        &fp,
                        &ssh,
                        &mut survey,
                        &state,
                        &crate::proc::Budget::new(crate::proc::RECONCILE),
                        &mut Some(losing()),
                    )
                    .is_err();
                    // The cold push guards on the surveyed copy, so this
                    // helper applied the replace before it went silent;
                    // the batch that never arrived leaves the server's.
                    std::fs::write(&rp, b"server copy").unwrap();
                    failed
                }
            };
            assert!(failed, "{path}: no receipt is an error");
            assert_eq!(std::fs::read(&rp).unwrap(), b"server copy", "{path}");
            let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
            assert!(
                common.unconfirmed("config.yml").is_none(),
                "{path}: a replace under no agreed content is recorded as a send"
            );
            // Deleted here: the ledger's word stands, and the row goes.
            std::fs::remove_file(&lp).unwrap();
            let doomed = ledger::doomed(&state, local.path(), &[]);
            let remote_view = crate::baseline::Contents::from([("config.yml".into(), server)]);
            let kept = retire_never_landed(&state, local.path(), &mut common, &remote_view, doomed)
                .unwrap();
            assert_eq!(kept, ["config.yml"], "{path}");
        }
    }

    /// The warm path's rsync fallback reads and judges its doomed rows after
    /// the upload and right before the removal script, and steps aside for
    /// a row the server changed since the last agreed content before it
    /// writes anything: that script is an unconditional `rm -f`, and only
    /// the full reconcile's pull brings the server's edit home
    /// (`retire_moved`). The rows baseline.py could not judge are left
    /// alone here as in the full push (`set_aside`), and a read the helper
    /// never answers is an error, not a deferral. The fallback used to
    /// judge the rows before the upload, so a file a watcher regenerated
    /// because of that upload was removed by the `rm -f` that followed.
    /// The selected files are still read before the upload, for
    /// `may_push`. Nothing drives the fallback in a unit test — it needs
    /// rsync and a server — so the order is pinned on the source, like the
    /// full push's.
    #[test]
    fn the_warm_rsync_fallback_judges_its_doomed_rows_after_the_upload_and_before_the_removal() {
        let mine = include_str!("sync.rs");
        let at = mine
            .find("\nfn reconcile_structure(")
            .expect("the warm structural path");
        let body = &mine[at..][..mine[at..].find("\n}\n").expect("it ends")];
        let code = |needle: &str| {
            body.match_indices(needle)
                .map(|(i, _)| i)
                .find(|i| !in_a_comment(body, *i))
                .unwrap_or_else(|| panic!("the warm structural path lost `{needle}`"))
        };
        let pushable = code("may_push(");
        let upload = code("rsync_pass_dir(");
        let remembered = code("remember_common(");
        let read = code("read_doomed(ssh, project, &doomed, &deadline)?");
        let moved = code("split_moved(");
        let aside = code("set_aside(");
        let never = code("retire_never_landed(");
        let removal = code("delete_remote(");
        assert!(
            body[..upload].contains("crate::baseline::remote(") && pushable < upload,
            "the selected files are read and checked before the upload"
        );
        for (earlier, later, what) in [
            (upload, read, "the doomed rows are read after the upload"),
            (remembered, read, "and after the upload is recorded"),
            (read, moved, "then a moved row sends the batch away"),
            (moved, aside, "before anything is written"),
            (aside, never, "the unjudged rows are left alone"),
            (never, removal, "before the removal"),
        ] {
            assert!(earlier < later, "{what}");
        }
        let stepped: Vec<usize> = body[read..removal]
            .match_indices("return Ok(None)")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            stepped.len(),
            1,
            "a moved row must send the batch to the full reconcile, not merely be noticed"
        );
    }

    /// A removal the server confirmed leaves no agreed content behind, and
    /// settles any unconfirmed send under the name, as the channel's
    /// unlink does. Settling alone kept the last agreed content of the
    /// deleted file: a later upload under the name whose receipt was lost
    /// then read as the server's change, and deleting it brought it back.
    #[test]
    fn a_removal_the_server_confirmed_leaves_no_agreed_content_behind() {
        let local = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let state = project.state_key(&ssh.dest);
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let (a, c) = ("f:420:a".to_string(), "f:420:c".to_string());
        common
            .record(crate::baseline::Contents::from([
                ("f.rs".into(), Some(a.clone())),
                ("other.rs".into(), Some(a.clone())),
            ]))
            .unwrap();
        common
            .record_unconfirmed(std::collections::BTreeMap::from([(
                "g.rs".to_string(),
                c.clone(),
            )]))
            .unwrap();
        let removed = ["f.rs", "g.rs", "never.rs"].map(String::from);
        forget_removed(&mut common, &removed).unwrap();
        let reloaded = crate::baseline::State::load(&state, local.path()).unwrap();
        for state in [&common, &reloaded] {
            assert!(state.get("f.rs").is_none(), "no ancestor outlives the file");
            assert!(state.unconfirmed("g.rs").is_none(), "the send is settled");
            assert_eq!(state.get("other.rs"), Some(&a), "the rest stands");
            assert!(
                !state.entries.contains_key("never.rs"),
                "a name with nothing on record gains nothing"
            );
        }

        // Pinned on the source: both removal rounds record what the server
        // confirmed losing this way, after the round.
        let mine = include_str!("sync.rs");
        for head in ["\nfn push(", "\nfn reconcile_structure("] {
            let at = mine.find(head).expect("a removal round");
            let body = &mine[at..][..mine[at..].find("\n}\n").expect("it ends")];
            assert!(
                body.find("delete_remote(").expect("it removes")
                    < body.find("forget_removed(").unwrap_or(0),
                "{head}: a confirmed removal must leave no agreed content behind"
            );
        }
    }

    /// A replace of a claimed leaf whose receipt is lost is recorded as
    /// ulak's own unconfirmed send, on all three channel paths. Without it
    /// the record kept the older content while the server held the
    /// upload, so deleting the file here before the next survey read the
    /// upload as the server's change, warned that the server had edited
    /// it, and brought the deleted file back.
    #[test]
    fn a_replace_whose_receipt_is_lost_stays_ulaks_own_to_delete() {
        for path in ["transfer_changed", "stream_structure", "push_and_claim"] {
            let local = tempfile::tempdir().unwrap();
            let remote = tempfile::tempdir().unwrap();
            let (project, mut ssh) = helper_project(local.path());
            ssh.offline();
            let _lock = crate::lockfile::WorkspaceLock::acquire(project.workspace_id()).unwrap();
            let state = project.state_key(&ssh.dest);
            let fp = inventory_footprint(local.path());
            let (lp, rp) = (local.path().join("new.rs"), remote.path().join("new.rs"));
            // Synced and agreed on, claimed, then saved here.
            std::fs::write(&rp, b"agreed").unwrap();
            let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
            common
                .record(crate::baseline::Contents::from([(
                    "new.rs".into(),
                    crate::baseline::fingerprint(&rp).unwrap(),
                )]))
                .unwrap();
            ledger::store(&state, local.path(), &[b"new.rs".to_vec()], &[], &[]);
            std::fs::write(&lp, b"local save").unwrap();
            let stamp = crate::selection::Stamp::read(&lp).unwrap();
            let losing = || crate::stream::local_helper_losing_receipts(remote.path());
            let failed = match path {
                "transfer_changed" => {
                    let mut session = Session {
                        stream: Some(losing()),
                        common: Some(common),
                        ..Session::default()
                    };
                    let changed = std::collections::BTreeMap::from([(lp.clone(), stamp)]);
                    transfer_changed(&project, &fp, &ssh, &mut session, &changed).is_err()
                }
                "stream_structure" => {
                    let mut session = Session {
                        stream: Some(losing()),
                        owned: std::collections::BTreeSet::from([lp.clone()]),
                        ..Session::default()
                    };
                    let observed = std::collections::BTreeMap::from([(lp.clone(), stamp)]);
                    stream_structure(
                        &project,
                        &fp,
                        &ssh,
                        &mut session,
                        &mut common,
                        &state,
                        &[b"new.rs".to_vec()],
                        &[],
                        &observed,
                        true,
                    )
                    .is_err()
                }
                _ => {
                    let surveyed = crate::baseline::fingerprint(&rp).unwrap();
                    let mut survey = one_file_survey(&project, &ssh, surveyed);
                    push_and_claim(
                        &PathBuf::from("rsync-not-invoked"),
                        &project,
                        &fp,
                        &ssh,
                        &mut survey,
                        &state,
                        &crate::proc::Budget::new(crate::proc::RECONCILE),
                        &mut Some(losing()),
                    )
                    .is_err()
                }
            };
            assert!(failed, "{path}: no receipt is an error");
            assert_eq!(
                std::fs::read(&rp).unwrap(),
                b"local save",
                "{path}: the helper applied the replace before its receipt was lost"
            );
            // Deleted here before any survey confirms the upload.
            std::fs::remove_file(&lp).unwrap();
            let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
            let server = crate::baseline::fingerprint(&rp).unwrap();
            assert!(
                common
                    .unconfirmed("new.rs")
                    .zip(server.as_ref())
                    .is_some_and(|(sent, server)| sent.contains(server)),
                "{path}: the upload is remembered as ulak's own send"
            );
            let remote_view = crate::baseline::Contents::from([("new.rs".into(), server)]);
            let doomed = ledger::doomed(&state, local.path(), &[]);
            assert_eq!(doomed, ["new.rs"], "{path}");
            let doomed =
                retire_moved(&state, local.path(), &mut common, &remote_view, doomed).unwrap();
            assert_eq!(
                doomed,
                ["new.rs"],
                "{path}: ulak's own upload is deleted, not brought back as the server's edit"
            );
            assert!(!common.returning("new.rs"), "{path}");
        }
    }

    /// A dry run lists the rows a real sync would keep apart from its
    /// deletions and leaves them out of the count. The over-budget error
    /// sends people to `ulak sync --dry-run` to review the list, and that
    /// list named a file the sync would bring home instead. It judges over
    /// the state it reads and retires nothing.
    #[test]
    fn a_dry_run_lists_the_rows_a_sync_would_keep_apart_from_its_deletions() {
        let local = tempfile::tempdir().unwrap();
        let (project, ssh) = helper_project(local.path());
        let state = project.state_key(&ssh.dest);
        let mut common = crate::baseline::State::load(&state, local.path()).unwrap();
        let [a, b, c, d] = ["a", "b", "c", "d"].map(|x| format!("f:420:{x}"));
        common
            .record(crate::baseline::Contents::from([
                ("edited.rs".into(), Some(a.clone())),
                ("gone.rs".into(), Some(a.clone())),
            ]))
            .unwrap();
        common
            .record_unconfirmed(std::collections::BTreeMap::from([(
                "theirs.rs".to_string(),
                c.clone(),
            )]))
            .unwrap();
        let remote = crate::baseline::Contents::from([
            ("edited.rs".into(), Some(b.clone())),
            ("gone.rs".into(), Some(a.clone())),
            ("theirs.rs".into(), Some(d.clone())),
            ("unagreed.rs".into(), Some(b.clone())),
        ]);
        let doomed = ["edited.rs", "gone.rs", "theirs.rs", "unagreed.rs"]
            .map(String::from)
            .to_vec();
        let copies = crate::baseline::Copies {
            remote,
            ..Default::default()
        };
        let (deleted, kept) = preview_deletions(doomed, &common, &copies);
        assert_eq!(deleted, ["gone.rs", "unagreed.rs"]);
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert!(
            kept.iter()
                .any(|l| l.starts_with("kept: edited.rs ") && l.contains("changed on the server")),
            "{kept:?}"
        );
        assert!(
            kept.iter()
                .any(|l| l.starts_with("kept: theirs.rs ") && l.contains("never landed")),
            "{kept:?}"
        );
        let reloaded = crate::baseline::State::load(&state, local.path()).unwrap();
        assert!(
            reloaded.unconfirmed("theirs.rs").is_some(),
            "nothing was settled"
        );

        // Pinned on the source: the dry run counts only what the judgement
        // leaves, and still lists everything when it cannot judge.
        let mine = include_str!("sync.rs");
        let at = mine.find("\nfn dry_run(").expect("the dry run");
        let body = &mine[at..][..mine[at..].find("\n}\n").expect("it ends")];
        assert!(
            body.find("preview_deletions(").expect("the dry run judges")
                < body.find("report.deleted += 1").expect("and counts"),
        );
        assert!(!body.contains("retire_"), "a dry run retires nothing");
    }

    /// `[sync] no_history = "send"` names each file it sends over a
    /// differing server copy, on every path that sends: the full push and
    /// both warm rsync fallbacks. Only the full push used to say so; the
    /// warm fallbacks sent over the server's bytes without a word.
    #[test]
    fn no_history_send_names_each_file_it_sends_over_on_every_path() {
        let (a, b) = ("f:420:a".to_string(), "f:420:b".to_string());
        let names = ["agreed.rs", "gone.rs", "new.rs", "over.rs", "same.rs"]
            .map(String::from)
            .to_vec();
        let base = crate::baseline::Contents::from([("agreed.rs".into(), Some(b.clone()))]);
        let local = crate::baseline::Contents::from([
            ("agreed.rs".into(), Some(a.clone())),
            ("gone.rs".into(), None),
            ("new.rs".into(), Some(a.clone())),
            ("over.rs".into(), Some(a.clone())),
            ("same.rs".into(), Some(a.clone())),
        ]);
        let remote = crate::baseline::Contents::from([
            ("agreed.rs".into(), Some(b.clone())),
            ("gone.rs".into(), Some(b.clone())),
            ("new.rs".into(), None),
            ("over.rs".into(), Some(b.clone())),
            ("same.rs".into(), Some(a.clone())),
        ]);
        // Named: over.rs alone. The others have a common record, nothing
        // local to send, no server copy, or the same bytes on both ends.
        assert_eq!(
            sent_without_history(&names, &base, &local, &remote),
            [&"over.rs".to_string()]
        );

        // Every function that decides a push by `may_push`, directly or
        // through `push_selection`, and then sends, says so.
        let mine = include_str!("sync.rs");
        let code = &mine[..mine.find("\n#[cfg(test)]").expect("the tests")];
        let mut checked = 0;
        for needle in [format!("{}(", "may_push"), format!("{}(", "push_selection")] {
            for (at, _) in code.match_indices(&needle) {
                if in_a_comment(code, at) || code[..at].trim_end().ends_with("fn") {
                    continue;
                }
                let head = enclosing_fn(code, at);
                let body = &code[head..][..code[head..].find("\n}\n").expect("it ends")];
                // The selection itself sends nothing; its caller is checked.
                if body.trim_start().starts_with("fn push_selection(") {
                    continue;
                }
                assert!(
                    body.contains("say_what_no_history_sends("),
                    "a push decided by may_push sends without naming the no-history files:\n{}",
                    body.lines().nth(1).unwrap_or_default()
                );
                checked += 1;
            }
        }
        assert!(checked >= 3, "expected three pushes, found {checked}");
    }

    /// What the ledger is allowed to forget after a removal round.
    ///
    /// `rmdir` refusing a directory the container has since filled is the
    /// right answer, not a failure — but the row has to survive it, or
    /// nothing may ever try again and the directory stands on the server
    /// for good. The count stays what the server counted: a directory the
    /// file loop's `rmdir -p` already took is gone without being counted
    /// twice.
    #[test]
    fn only_what_the_server_confirmed_losing_leaves_the_ledger() {
        let doomed = vec!["site/a.txt".to_string(), "site/b.txt".to_string()];
        let doomed_dirs = vec!["site/full".to_string(), "site/empty".to_string()];
        let out = "ULAK_KEPT_FILE site/b.txt\n\
                   ULAK_KEPT_DIR site/full\n\
                   DELETED 2\n";
        let removal = parse_removal(out, &doomed, &doomed_dirs);
        assert_eq!(removal.count, 2);
        assert_eq!(removal.files, vec!["site/a.txt"]);
        assert_eq!(removal.dirs, vec!["site/empty"]);
        // The kept FILE is a deletion this sync listed and did not
        // apply, and this number is the only way status and doctor hear
        // about it. The kept DIRECTORY is not: a `rmdir` the container's
        // work refused is the right answer, and counting it would nag
        // about a deletion no `--max-delete` can ever apply.
        assert_eq!(removal.kept_files, 1);

        // Nothing kept: everything doomed really went.
        let removal = parse_removal("DELETED 4\n", &doomed, &doomed_dirs);
        assert_eq!(removal.files, doomed);
        assert_eq!(removal.dirs, doomed_dirs);
        assert_eq!(removal.kept_files, 0);

        // The output was cut off before the receipt. Reading silence as
        // "all of them went" would leave those files on the server with
        // nobody claiming them — undeletable, and free to come home.
        let removal = parse_removal("ULAK_KEPT_FILE site/b.txt\n", &doomed, &doomed_dirs);
        assert_eq!(removal.count, 0);
        assert!(
            removal.files.is_empty() && removal.dirs.is_empty(),
            "no receipt is not a confirmation"
        );
        assert_eq!(
            removal.kept_files, 2,
            "no receipt means every doomed file is still up there"
        );
    }

    /// A re-layout starts the ledger over, and nothing else may.
    ///
    /// `relayout` removes and remakes the whole workspace directory, so
    /// every row describes a file that is no longer there. Rows do not
    /// fall out on their own any more (`store` merges), and two things
    /// follow from keeping them, both permanent: `pull_back`'s creating
    /// pass is handed an exclude list covering the server's OWN work, so
    /// it never comes home; and one ledger ends up holding two anchors,
    /// after which the anchor-blind `synced_files` feeds `/parent`-
    /// relative rows to an rsync rooted at `/parent/repo`, where they
    /// exclude nothing at all.
    #[test]
    fn a_re_laid_out_workspace_starts_the_ledger_over() {
        let id = format!("sync-relaid-{}", std::process::id());
        let state =
            crate::config::WorkspaceKey::from_namespace("sync-relaid", std::path::Path::new(&id))
                .unwrap()
                .state_key("test-server");
        let anchor = std::path::Path::new("/repo");
        ledger::store(&state, anchor, &[b"site/index.html".to_vec()], &[], &[]);
        let mut doomed = vec!["site/old.txt".to_string()];
        let mut doomed_dirs = vec!["site/old".to_string()];

        // The workspace we left there is the workspace we found.
        on_layout(Layout::Kept, &state, &mut doomed, &mut doomed_dirs);
        assert_eq!(
            ledger::synced_files(&state),
            vec![b"site/index.html".to_vec()]
        );
        assert_eq!(doomed.len(), 1);
        assert_eq!(doomed_dirs.len(), 1);

        on_layout(Layout::Relaid, &state, &mut doomed, &mut doomed_dirs);
        assert!(
            ledger::synced_files(&state).is_empty(),
            "a row that survives a re-layout hides the server's own work from the pull"
        );
        assert!(
            doomed.is_empty() && doomed_dirs.is_empty(),
            "the paths this sync was about to delete went with the directory"
        );
        ledger::forget(&state);
    }

    /// The one `rm -rf` ulak aims at a workspace.
    /// The declaration has to be consulted BEFORE this machine's own
    /// record, or a runner whose state directory is wiped every job would
    /// mint a fresh uuid, fail to match the one the server kept from the
    /// last job, and be refused with "run this in a terminal" — on a
    /// machine that has none. Pinned to the source because the ordering
    /// is the whole fix and nothing else in the function shows it.
    /// A lifted guard has to be visible in the run that used it — that is
    /// the whole reason it is a name and not a large number. A budget
    /// still in force says nothing: it is the default, and reporting it
    /// every sync would be noise.
    #[test]
    fn only_a_lifted_deletion_budget_is_announced() {
        use crate::config::DeleteBudget;
        assert_eq!(
            budget_note(DeleteBudget::Unlimited).as_deref(),
            Some("deletion budget: unlimited")
        );
        assert_eq!(budget_note(DeleteBudget::Limit(25)), None);
        assert_eq!(budget_note(DeleteBudget::Limit(0)), None);
    }

    #[test]
    fn a_declared_identity_is_read_before_this_machines_own_record() {
        let mine = include_str!("sync.rs");
        let at = mine
            .find("fn ensure_workspace(")
            .expect("ensure_workspace to still be here");
        let body = &mine[at..at + mine[at..].find("\n}\n").unwrap_or(0)];
        let declared = body
            .find("declared_workspace_uuid")
            .expect("ensure_workspace to consult the declared identity");
        let recorded = body
            .find("recorded_uuid")
            .expect("ensure_workspace to still read this machine's record");
        assert!(
            declared < recorded,
            "the declared identity must be read before the local record"
        );
        // And it must not reach for a fresh uuid when one was declared.
        let minted = body.find("uuid_v4").expect("the mint to still be here");
        assert!(
            declared < minted,
            "a declared identity must pre-empt minting"
        );
    }

    #[test]
    fn a_re_layout_empties_the_workspace_and_writes_the_manifest_back() {
        let script = relayout_script(
            ".ulak/workspaces/alice/abc/my app",
            ".ulak/workspaces/alice/abc/manifest.json",
            "{\"uuid\":\"u\"}",
        );
        // A name with a space in it must not become two arguments —
        // `rm -rf .ulak/workspaces/alice/abc/my app` would take the whole
        // hash directory and then some.
        assert!(
            script.contains("rm -rf '.ulak/workspaces/alice/abc/my app'"),
            "{script}"
        );
        assert!(script.contains("umask 077") && script.contains("chmod 700"));
        // The manifest is written back in the SAME round: a link that
        // dies here must not leave a workspace whose manifest describes
        // the layout that was just removed.
        assert!(
            script.contains(
                "printf '%s' '{\"uuid\":\"u\"}' > .ulak/workspaces/alice/abc/manifest.json"
            ),
            "{script}"
        );

        // And saying no stops the sync with both ways out, not just the
        // obvious one — the user who refuses is the one least likely to
        // know that keeping the old layout is a choice they still have.
        let refused = crate::ui::flatten(&relayout_refused("ulak sync"));
        assert!(
            refused.contains("rerun and confirm: ulak sync")
                && refused.contains("relative to the project again"),
            "{refused}"
        );
    }

    /// An error that only says no is a dead end, and rsync's exit codes
    /// are where a user is least able to work out the next step alone.
    /// Every branch here names one, and it has to be the RIGHT one:
    /// "run doctor" is wrong guidance for a link that died mid-transfer,
    /// and "install rsync" is wrong for a disk that filled up.
    #[test]
    fn every_rsync_failure_says_what_to_do_next() {
        let ssh = Ssh::new("my-server").expect("an ssh handle");
        let said = |code: Option<i32>, stderr: &str| {
            crate::ui::flatten(&map_rsync_failure(
                code,
                stderr,
                &ssh,
                "ulak sync",
                "ulak doctor",
            ))
        };

        let missing = said(Some(127), "bash: line 1: rsync: command not found\n");
        assert!(
            missing.contains("no usable rsync") && missing.contains("apt-get install -y rsync"),
            "{missing}"
        );
        assert!(missing.contains("ulak doctor"), "{missing}");

        // 30 is rsync's own --timeout=60. The transfer was alive when it
        // started, so this is a link that went away mid-flight — sending
        // the user to doctor would send them looking for a setup problem
        // that is not there.
        let stalled = said(Some(30), "io timeout after 60 seconds\n");
        assert!(
            stalled.contains("stalled for 60 seconds") && stalled.contains("ConnectTimeout=10"),
            "{stalled}"
        );
        assert!(
            stalled.contains("nothing was deleted"),
            "the one thing a user needs to know before rerunning: {stalled}"
        );
        assert!(
            !stalled.contains("no usable rsync"),
            "a dead link is not a missing binary: {stalled}"
        );

        let protocol = said(Some(12), "rsync: protocol incompatibility\n");
        assert!(
            protocol.contains("protocol error") && protocol.contains(">= 3.2"),
            "{protocol}"
        );
        // Version-skew is announced by the message, not the code, on a
        // server that answers with something else entirely.
        let by_text = said(Some(1), "unknown protocol version\n");
        assert!(by_text.contains("protocol error"), "{by_text}");

        let full = said(
            Some(11),
            "rsync: write failed: No space left on device (28)\n",
        );
        assert!(
            full.contains("disk filled up") && full.contains("docker system df"),
            "{full}"
        );

        // Anything else still has to hand over a next step and the
        // stderr that caused it — this is the branch a user reaches with
        // an exit code nobody has seen before.
        let other = said(Some(23), "rsync: some files could not be transferred\n");
        assert!(
            other.contains("exit 23")
                && other.contains("some files could not be transferred")
                && other.contains("ssh my-server true")
                && other.contains("ulak doctor"),
            "{other}"
        );
        // A signal death has no code at all, and "exit ?" is still an
        // answer; an unwrap here would abort the sync with a panic.
        let signalled = said(None, "");
        assert!(signalled.contains("exit ?"), "{signalled}");
    }

    #[test]
    fn version_parsing() {
        assert_eq!(
            parse_rsync_version("rsync  version 3.4.4  protocol version 32"),
            Some((3, 4))
        );
        assert_eq!(
            parse_rsync_version("rsync  version v3.2.7  protocol version 31"),
            Some((0, 2)) // "v3" fails to parse major — rejected as too old
        );
        assert_eq!(
            parse_rsync_version("openrsync: protocol version 29"),
            Some((0, 0))
        );
        assert_eq!(parse_rsync_version("no such thing"), None);
    }
}
