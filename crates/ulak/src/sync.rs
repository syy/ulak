//! The sync engine: one rsync profile, no fallbacks.
//!
//! Profile: one rsync from the ANCHOR with --delay-updates
//! --partial-dir, a NUL-delimited literal exclude list computed by
//! walk.rs, and filter rules for protect and the footprint.
//!
//! Rule order is the whole design (rsync takes the FIRST match):
//!   0. partials  — rsync's own half-written blobs, ours to exclude
//!   1. held back — paths the pull must not carry home (see pull_back)
//!   2. protect   — server-owned data: never sent, never deleted
//!   3. ignores   — .gitignore/[sync] exclude, expanded to literal paths
//!   4. footprint — the only paths allowed to travel, plus their parents
//!   5. `- *`     — everything else, which rsync then never descends into
//!
//! Rule 5 is what makes a small stack inside a multi-gigabyte monorepo
//! cost a 3 MB scan. Rule 3 beating rule 4 is deliberate: pushing a
//! gitignored `./data` over a live database is the worse of the two
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
//! counter, the push window and the ledger itself — is keyed by workspace
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
    let started = std::time::SystemTime::now();
    let deadline = crate::proc::Budget::new(crate::proc::RECONCILE);
    let mut report = run_sync(project, fp, ssh, opts, &deadline)?;
    // A workspace runs both ways, because a local bind mount does: whatever
    // the stack produced comes home in the same breath it went up.
    // `started` is the line between "the server may already know this"
    // and "you typed it just now, and it is yours" — see `pull_back`.
    if !opts.dry_run {
        let back = pull_back(project, fp, ssh, opts.quiet, &deadline, Some(started))?;
        report.pulled += back.pulled;
    }
    Ok(report)
}

pub fn run_sync(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    opts: &SyncOptions,
    deadline: &crate::proc::Budget,
) -> Result<SyncReport> {
    let rsync_bin = local_rsync()?;
    let plan = checked_plan(project, fp)?;
    let state = project.state_key(&ssh.dest);
    let budget = opts
        .max_delete_override
        .unwrap_or(project.config.sync.max_delete);
    // Computed BEFORE the push, from the ledger alone: no remote listing,
    // no second rsync pass, and the answer cannot change under us.
    let mut doomed = ledger::doomed(&state, &fp.anchor, &plan.kept);
    // Directories go the same way, by their own command. They are kept
    // out of the BUDGET on purpose: the budget guards against destroying
    // work, and `rmdir` cannot — it refuses a directory that still holds
    // anything. A branch switch that retires forty directories is not the
    // kind of surprise the threshold exists to stop.
    let mut doomed_dirs = ledger::doomed_dirs(&state, &fp.anchor);

    if opts.dry_run {
        let events = rsync_pass(
            &rsync_bin,
            project,
            fp,
            ssh,
            &plan.excludes,
            &["--dry-run"],
            deadline,
        )?
        .or_fail()?;
        let mut report = render_events(&events, opts.quiet);
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
        summarize(&report, opts.quiet, true, budget);
        if !budget.allows(doomed.len()) {
            ui::warn(&format!(
                "{} deletions exceed the budget of {budget} — a real sync will ask first",
                doomed.len()
            ));
        }
        return Ok(report);
    }

    on_layout(
        ensure_workspace(ssh, project)?,
        &state,
        &mut doomed,
        &mut doomed_dirs,
    );
    let events = push_and_claim(&rsync_bin, project, fp, ssh, &plan, &state, deadline)?;
    let mut report = render_events(&events, opts.quiet);

    if doomed.is_empty() && doomed_dirs.is_empty() {
        crate::invocation::record_pending_deletions(&state, 0);
        summarize(&report, opts.quiet, false, budget);
        return Ok(report);
    }

    if !budget.allows(doomed.len()) {
        // No terminal to ask in (the daemon): push, report, carry on.
        // Nothing is retired, so the rows stay exactly as they are —
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
    }

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
fn push_and_claim(
    rsync_bin: &PathBuf,
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    plan: &walk::WalkReport,
    state: &crate::config::WorkspaceStateKey,
    deadline: &crate::proc::Budget,
) -> Result<Vec<Event>> {
    let last = crate::invocation::last_push_window(state);
    let from = std::time::SystemTime::now();
    let mut pass = rsync_pass(rsync_bin, project, fp, ssh, &plan.excludes, &[], deadline)?;
    // From the PREVIOUS push's window, never this one's: a second that is
    // still running can still take another save, so re-offering inside it
    // settles nothing.
    let hidden = last
        .map(|w| hidden_by_the_quick_check(&fp.anchor, &plan.kept, w))
        .unwrap_or_default();
    let settled = hidden.is_empty()
        || match offer_again(rsync_bin, project, ssh, fp, &hidden, deadline) {
            Ok(again) => {
                pass.also(again);
                true
            }
            Err(e) => {
                pass.failed.get_or_insert(e);
                false
            }
        };
    // Recorded before the failure is raised, like the claim below: this
    // pass has had files open, and the NEXT one has to ask about those
    // seconds. A catch-up that did not run keeps the old window's start —
    // dropping it would strand those files for good.
    crate::invocation::record_push_window(
        state,
        match settled {
            true => from,
            false => last.map_or(from, |(f, _)| f),
        },
        crate::invocation::now_secs(),
    );
    // Claimed BEFORE the failure is raised, and this order is the whole
    // point. A pass that exits 23 — one unreadable file in a tree that
    // otherwise arrived — has still put files on the server. Returning
    // the error first left every one of them with nobody claiming it:
    // `doomed` could never name them, so deleting one here left it up
    // there forever, and `pull_back`'s creating pass was free to plant
    // it back in the repo. That is the same hole `claim` was written to
    // close, one exit code over.
    let claimed = claim(&plan.kept, &plan.kept_dirs, &pass.events);
    ledger::store(state, &fp.anchor, &claimed, &[], &[]);
    pass.or_fail()
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
/// gitignored noise (a container's node_modules) never comes down. What
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
/// `--ignore-existing` and hands rsync every FILE row the ledger holds
/// as an exclude: it can only ever create, and only what ulak never
/// put there.
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
/// exclude list from the ledger instead.
///
/// That second half is only safe under the per-workspace lock, which is
/// where `reconcile_once` reads it. `passthrough::run` does NOT hold it:
/// it drops the lock before every compose subcommand that is in
/// neither `READ_ONLY` nor `TREE_READERS` (`exec`, `stop`, `kill`,
/// `rm`, `cp`, …) and pulls afterwards, unlocked — so the service can
/// be inside `reconcile_once` at that moment, rewriting the same ledger
/// file. A read that came back SHORT used to be the danger there: rows
/// missing from pass two's exclude list are rows `--ignore-existing` is
/// free to create, re-creating the file the user just deleted.
/// `ledger::store` now writes through a rename, so what this read gets is
/// always one whole ledger — the one before that write or the one after,
/// never half of either. What is still open is the other order: the
/// unlocked writer's own read-modify-write can lose the concurrent
/// writer's rows, because nothing serialises the two. Closing that means
/// holding the per-workspace lock across the passthrough pull.
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
pub fn pull_back(
    project: &Project,
    fp: &Footprint,
    ssh: &Ssh,
    quiet: bool,
    deadline: &crate::proc::Budget,
    since: Option<std::time::SystemTime>,
) -> Result<SyncReport> {
    let rsync_bin = local_rsync()?;
    let plan = checked_plan(project, fp)?;
    let state = project.state_key(&ssh.dest);
    // Every hold is lifted to the highest ancestor that is missing here.
    // `touched_since` names a file that existed when it was stat-ed, and
    // the directory holding it can be gone by the time rsync looks — a
    // rename lands in that window on every second run of the service
    // suite. Excluding just the file would let the footprint's
    // `+ dir/***` create the empty parent HERE.
    let mut held_back: Vec<String> = since
        .map(|since| touched_since(&fp.anchor, &plan.kept, since))
        .unwrap_or_default()
        .iter()
        .map(|rel| hold_at_the_highest_gap(&fp.anchor, rel))
        .collect();
    held_back.sort();
    held_back.dedup();

    // Pass one: refresh what is already here. `--existing` is the whole
    // guarantee — it creates nothing at all, so no deletion can be
    // undone by it, whenever the deletion happens to land.
    let mut events = rsync_pass_dir(
        &rsync_bin,
        project,
        fp,
        ssh,
        &plan.excludes,
        &held_back,
        &["--update", "--existing", "--prune-empty-dirs"],
        Direction::Down,
        deadline,
    )?
    .or_fail()?;
    // Pass two: bring home what the stack invented. It can only create
    // (`--ignore-existing`), and every path ulak itself put in the
    // workspace is excluded, so the only thing it can create is the
    // container's own work. The exclude list rides the same stdin channel
    // as the ignores, which already outranks the footprint's includes —
    // so no filter rule had to move.
    let mut unclaimed = plan.excludes.clone();
    unclaimed.extend(ledger::synced_files(&state));
    let invented = rsync_pass_dir(
        &rsync_bin,
        project,
        fp,
        ssh,
        &unclaimed,
        &held_back,
        &["--ignore-existing", "--prune-empty-dirs"],
        Direction::Down,
        deadline,
    )?
    .or_fail()?;
    say_what_slipped_past_the_ignores(project, fp, &invented);
    events.extend(invented);

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
/// The ranking is not the bug and does not move — it is what stops a
/// gitignored `./data` from being pushed over a live database, which is
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
    let slipped = slipped_past_the_ignores(&project.config.sync, fp, invented);
    if slipped.is_empty() {
        return;
    }
    ui::warn(&format!(
        "the stack made {} on the server and it came home, though this project's ignore rules cover it — nothing here could exclude a path that did not exist yet",
        slipped.join(", ")
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

/// The push's mirror of `touched_since`: the saves that rsync's quick
/// check cannot see, and the only ones worth asking about twice.
///
/// rsync decides what to send from size and mtime-TO-THE-SECOND, and the
/// copy on the server carries the mtime the local file had when it was
/// read. So a same-size save made in that same second is indistinguishable
/// from what is already up there, and stays that way for good: rsync
/// exits 0 having sent nothing, every pass.
///
/// The question is not "what changed" — the quick check answers that
/// correctly for everything else. It is two, and a file has to answer
/// both:
///
/// 1. **Written in a second the last push was reading in?** If not, the
///    server's copy carries a different second and the quick check
///    catches it unaided. Asked in whole seconds, because that is what
///    rsync compares.
/// 2. **Written after that push began?** If not, the push read this
///    exact content and the server HAS it. Asked as instants, and it is
///    what keeps the answer small — measured before it existed: the
///    second sync of the ten-thousand file workspace re-offered every
///    file and reported thousands of changes for a no-op.
///
/// Together they are the causal condition: a save can hide only if it
/// landed after the push began (or it would BE the copy) and inside a
/// second the push was reading in (or rsync would see the difference).
/// Question 2 assumes mtimes finer than a second — APFS, ext4, xfs and
/// btrfs all qualify; on one that does not, the hole stays as it was.
///
/// Cost: one stat per synced path, the same one `touched_since` makes on
/// the way down, plus one rsync over a handful of names when the answer
/// is not empty. That is what keeps `--checksum` off the main pass.
fn hidden_by_the_quick_check(
    anchor: &std::path::Path,
    kept: &[Vec<u8>],
    window: (std::time::SystemTime, u64),
) -> Vec<String> {
    let (from, to) = window;
    kept.iter()
        .filter_map(|rel| std::str::from_utf8(rel).ok())
        .filter(|rel| {
            std::fs::symlink_metadata(anchor.join(rel))
                .and_then(|m| m.modified())
                .ok()
                .is_some_and(|m| {
                    m >= from
                        && m.duration_since(std::time::UNIX_EPOCH)
                            .is_ok_and(|d| d.as_secs() <= to)
                })
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
fn removal_script(remote_dir: &str, doomed: &[String], doomed_dirs: &[String]) -> String {
    // "" and /* keep the rm inside the workspace; the four `..` shapes
    // are the traversal, spelled as components so a name that merely
    // CONTAINS two dots is not caught with them.
    const OUTSIDE: &str = "\"\"|/*|..|../*|*/../*|*/..";
    let mut script = format!("cd {} || exit 1\nn=0\n", sh_quote(remote_dir));
    script.push_str(&set_args(doomed));
    script.push_str(&format!(
        "for p in \"$@\"; do\n\
         case \"$p\" in {OUTSIDE}) printf 'ULAK_KEPT_FILE %s\\n' \"$p\"; continue ;; esac\n\
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
    let events = push_and_claim(&rsync_bin, project, fp, ssh, &plan, &state, &deadline)?;
    let pending = !ledger::doomed(&state, &fp.anchor, &plan.kept).is_empty();
    let report = render_events(&events, true);
    Ok((report, pending))
}

/// Ignore rules, expanded to literal paths, for the footprint's
/// directories only — walking the anchor itself is exactly the scan the
/// footprint model exists to avoid.
fn checked_plan(project: &Project, fp: &Footprint) -> Result<walk::WalkReport> {
    let mut plan = walk::WalkReport::default();
    // Narrows each build context by its own `.dockerignore`, and nothing
    // else — see dockerignore.rs. It lands in the SAME literal exclude
    // list as the gitignore chain, which sync.rs already ranks ahead of
    // the footprint's includes, so no filter rule has to change.
    let build = fp.build_filter();
    for dir in fp.sync_dirs() {
        let part = walk::plan_in(&fp.anchor, &dir, &project.config.sync, &build)?;
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
            if crate::walk::protected(&project.config.sync.protect, &rel) {
                continue;
            }
            plan.kept.push(rel.into_bytes());
        }
    }
    plan.kept.sort();
    plan.kept.dedup();
    plan.kept_dirs.sort();
    plan.kept_dirs.dedup();
    if !plan.bad_names.is_empty() {
        let sample = plan.bad_names.iter().take(5).cloned().collect::<Vec<_>>();
        return Err(fail!(
            "{} file name(s) cannot be synced safely (newlines or non-UTF8 bytes in the name): {}",
            plan.bad_names.len(),
            sample.join(", ")
        )
        .now("rename those files, or add them to [sync] exclude in ulak.toml")
        .into_err());
    }
    Ok(plan)
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
    extra: &[&str],
    deadline: &crate::proc::Budget,
) -> Result<Pass> {
    // Nothing is held back going UP: a path that is not on disk here has
    // nothing to send, and the push removes it on the server itself.
    rsync_pass_dir(
        rsync_bin,
        project,
        fp,
        ssh,
        excludes,
        &[],
        extra,
        Direction::Up,
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
    cmd.args(filter_args(
        &project.config.sync.protect,
        held_back,
        fp.filter_rules(),
    ));
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
    let payload: Vec<u8> = excludes
        .iter()
        .flat_map(|e| {
            let mut v = walk::rsync_exclude_pattern(e);
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
    /// Fold a follow-up pass into this one. A path both carried is ONE
    /// change: counting it twice would report "2 change(s)" for one save.
    fn also(&mut self, other: Pass) {
        for e in other.events {
            if !self.events.contains(&e) {
                self.events.push(e);
            }
        }
        if let Some(e) = other.failed {
            self.failed.get_or_insert(e);
        }
    }

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
    let script = format!(
        "umask 077 && mkdir -p {dir} && chmod 700 {b} {ns} {ws} {dir} && \
         if [ -f {m} ]; then cat {m}; else printf '%s' {json} > {m} && cat {m}; fi",
        dir = sh_quote(&remote_dir),
        b = sh_quote(base),
        ns = sh_quote(&namespace_dir),
        ws = sh_quote(&workspace_dir),
        m = sh_quote(&manifest_path),
        json = sh_quote(&candidate_json),
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
    let protect = &project.config.sync.protect;
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
        const DOORS: &[&str] = &[
            "run_sync(",
            "reconcile_once(",
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
        // Eight doors today, across seven files; the floor is under that
        // so a refactor that merges two of them is not a false alarm.
        assert!(checked >= 6, "expected several call sites, found {checked}");

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
        // No `[sync]` rules at all: the `.gitignore` above is the only
        // thing speaking, which is the shape of an ordinary project.
        let cfg = crate::config::SyncCfg {
            exclude: Vec::new(),
            include: Vec::new(),
            protect: Vec::new(),
            max_delete: crate::config::DeleteBudget::Limit(0),
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
    /// time on every machine. Two claims: ulak's push flags lose the
    /// edit, and `--checksum` recovers it. If some future rsync closes
    /// this on its own, the first half is what will say so.
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
             now says v5, rsync has changed and `offer_again` can go"
        );

        // And the catch-up's one flag, which is the whole fix.
        push(&["--checksum"]);
        assert_eq!(
            std::fs::read_to_string(to.join("index.html")).unwrap(),
            "<h1>service-edit-v5</h1>",
            "--checksum is what the second pass carries, and it has to be \
             enough on its own"
        );
    }

    /// Which files the catch-up asks about, and which it does not.
    /// Dropping either half is a real bug with no symptom a type could
    /// catch: without the second, saves the push never looked at are
    /// re-offered forever; without the first, every file a push sent is
    /// re-offered on the next one — measured, that turned the
    /// ten-thousand file no-op sync into thousands of changes.
    #[test]
    fn a_save_is_asked_about_only_if_it_landed_mid_push_and_shares_its_second() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path();
        std::fs::create_dir_all(anchor.join("site")).unwrap();
        let at = |name: &str, secs: u64, nanos: u32| {
            write_at(&anchor.join(name), b"x", secs, nanos);
            name.as_bytes().to_vec()
        };
        // A push that began half a second into 100 and was still reading
        // in 102.
        let window = (
            std::time::UNIX_EPOCH + std::time::Duration::new(100, 500_000_000),
            102,
        );
        let kept = vec![
            // Written in one of the push's seconds, but BEFORE it began:
            // what the push sent is what is on disk, so nothing is
            // hiding. This is the ten-thousand file case.
            at("site/already-sent.txt", 100, 100_000_000),
            at("site/mid-push.txt", 100, 900_000_000),
            at("site/during.txt", 101, 0),
            at("site/last-second.txt", 102, 0),
            // After the push stopped reading: the server's copy carries
            // an earlier second, so the quick check catches this one.
            at("site/after.txt", 103, 0),
            // Gone from disk between the walk and the question. Naming it
            // to rsync would only earn a "file has vanished".
            b"site/deleted.txt".to_vec(),
        ];

        let asked = hidden_by_the_quick_check(anchor, &kept, window);
        assert_eq!(
            asked,
            vec![
                "site/mid-push.txt".to_string(),
                "site/during.txt".to_string(),
                "site/last-second.txt".to_string(),
            ],
            "a save can hide only if it landed after the push began and \
             inside a second the push was still reading in"
        );
    }

    /// The catch-up is not optional, and it is not the main pass. Two
    /// things a later edit could quietly undo, and neither would fail a
    /// behavioural test on macOS, where the hole never opens — so the
    /// shape is pinned here rather than trusted to a green run.
    #[test]
    fn the_catch_up_skips_the_quick_check_and_the_main_pass_does_not() {
        let mine = include_str!("sync.rs");
        let body = |name: &str| {
            let at = mine.find(name).unwrap_or_else(|| panic!("{name} exists"));
            &mine[at..][..mine[at..].find("\n}\n").expect("it ends")]
        };

        let catch_up = body("fn offer_again");
        for flag in ["--checksum", "--files-from=-", "--from0"] {
            assert!(
                catch_up.contains(flag),
                "the catch-up pass has to carry {flag}: without it the \
                 hidden save is judged by size and second all over again"
            );
        }

        let main = body("fn rsync_pass_dir");
        for flag in ["--ignore-times", "--checksum"] {
            assert!(
                !main.contains(flag),
                "{flag} is on the MAIN pass, which is the cost this design \
                 exists to avoid — a ten-thousand file workspace pays it on \
                 every reconcile and the service gets turned off"
            );
        }

        let push = body("fn push_and_claim");
        assert!(
            push.contains("offer_again") && push.contains("record_push_window"),
            "a push that does not re-offer, or does not say which seconds it \
             read in, leaves the same save hiding for good"
        );
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
