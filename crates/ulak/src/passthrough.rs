//! Explicit Compose passthrough below `ulak docker compose`.
//! Compose's whole argv surface is forwarded without becoming ulak's
//! root command surface.
//!
//! Contract: state-changing commands auto-sync first; read-only ones
//! skip the sync and go straight through. The Docker namespace removes
//! name collisions with ulak's own management commands.
//! Every invocation gets the resolved project identity injected as the
//! FIRST `-p`; a compose-global -p the user adds later simply wins
//! (pflag last-occurrence semantics). "Resolved" is docker's own
//! cascade and not a computation of ours — see `compose::project_name`.
//!
//! An `up` declaration is settled from Docker's path labels, never its exit
//! status alone. Success can leave every old container untouched
//! (`--no-recreate`) or preserve old orphans beside newly-created services;
//! failure can still create part of a stack. The new workspace becomes the
//! lifecycle owner only when every existing container names it.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::ops::Range;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::compose;
use crate::composepaths::{self, Direction, LocalPath, Shape};
use crate::config::Project;
use crate::footprint::{Entry, Footprint, Why};
use crate::lockfile::WorkspaceLock;
use crate::ssh::{Ssh, sh_quote};
use crate::sync;
use crate::ui::{self, fail};

/// Compose subcommands that do not need the workspace refreshed before
/// they run — they skip the auto-sync, and the pull back after it, for
/// speed. `down` is here despite changing server state: all it needs
/// are the compose files, which are already on the server.
const READ_ONLY: &[&str] = &[
    "config", "down", "events", "images", "logs", "ls", "port", "ps", "stats", "top", "version",
    "wait",
];

/// Commands where compose itself READS the workspace tree (build contexts,
/// bind sources at create time): the per-workspace lock is held until they
/// finish so watch can never hand them a half-synced tree.
///
/// `cp` joined this list when its local end started travelling: compose
/// opens that file on the server while the copy runs, so a sync landing
/// in the middle would hand it half a file. It returns in under a second,
/// so holding the lock costs nothing a user can feel.
const TREE_READERS: &[&str] = &[
    "build", "cp", "create", "pull", "push", "restart", "run", "start", "up",
];

/// Compose subcommands whose `-o/--output` names a FILE and whose
/// natural output is stdout — so the flag never reaches the server at
/// all: compose writes the bytes, ssh carries them, and they land here.
/// Measured against Compose v5.1.2.
///
/// `bridge convert` is deliberately absent even though it also spells
/// its output `-o`: it writes a directory TREE, and no stream carries
/// one.
const STREAMED_OUTPUT: &[&str] = &["config", "export"];

pub fn run(globals: &[String], argv: Vec<OsString>) -> Result<ExitCode> {
    // Root project selectors join the globals inside `docker compose`,
    // in that order — the same thing compose itself would see.
    let mut args = globals.to_vec();
    args.extend(decode_args(argv)?);
    // The invocation owns the argument split: compose globals are
    // captured (and re-emitted remapped), the rest stays verbatim.
    let crate::config::Bound {
        project,
        ssh,
        dest,
        mut footprint,
    } = crate::config::bind(&args)?;

    let subcommand = project.inv.subcommand.clone().unwrap_or_default();
    let subcommand = subcommand.as_str();

    // What the SUBCOMMAND's own argv names on this machine.
    //
    // The compose MODEL is already synced, which is why forwarding argv
    // verbatim is right for thirty of these commands. It is wrong for
    // the handful that name a path themselves, because no model mentions
    // those paths and so nothing carries them: measured, `compose run -v
    // ./data:/data` mounted an empty directory the server's docker had
    // just created, with no error and no output.
    let mut sub_args = project.inv.args.clone();
    let found = composepaths::scan(&project.inv.cwd, &sub_args, project.inv.subcommand_at)?;
    // `config -o` and `export -o` are answered by a stream, so no path
    // the flag names joins the footprint — nothing about them travels,
    // whether they land here or are simply overruled by a later `-o`.
    let spots = composepaths::output_spots(&sub_args, project.inv.subcommand_at);
    let streamed = streamed_output(subcommand, &found, &spots);
    let travelling: Vec<LocalPath> = found
        .iter()
        .filter(|p| !spots.iter().any(|(index, _)| *index == p.index))
        .cloned()
        .collect();
    carry(&mut footprint, &travelling, &mut sub_args, subcommand);
    let stack_id = crate::intent::stack_id(&dest, &project.compose_identity());
    // Before the work, what the service has been unable to do. These are
    // the commands somebody types all day; `ulak status` is the one
    // they type when they already suspect something, which is too late
    // to be the only place the answer lives. Kept, not printed and
    // forgotten: whether this half spoke decides whether the settled
    // half below may.
    let complained = crate::agent::stack_complaint(&stack_id, subcommand);
    if let Some(said) = &complained {
        ui::warn(said);
    }
    // Even `ps` and `down` parse the compose files, so the skip is only
    // safe once this workspace has been populated at least once.
    let state = project.state_key(&dest);
    let read_only = READ_ONLY.contains(&subcommand) && crate::invocation::ever_synced(&state);

    // Docker's own rule, written down: `up` means live until `down`.
    // It is recorded BEFORE the command runs, because a stack that is
    // coming up should be maintained from the first moment rather than
    // from whenever compose happens to finish — and because a command
    // that dies halfway leaves a half-started stack that still needs
    // looking after. `was_live` is what lets a FAILED `up` put the flag
    // back: a stack that never came up at all must not look live
    // forever.
    let wanted = match subcommand {
        "up" => Some(true),
        "down" => Some(false),
        _ => None,
    };
    let mut intent = wanted.map(|live| IntentGuard::declare(&project, &dest, live));

    let mut lock = None;
    if !read_only {
        let guard = WorkspaceLock::acquire(project.workspace_id())?;
        // Two budgets, not one: the compose command runs between the two
        // legs and may legitimately take hours (a build, a migration),
        // so a shared deadline would charge the pull for the push's time
        // plus everything in between.
        let report = sync::run_sync(
            &project,
            &footprint,
            &ssh,
            &sync::SyncOptions {
                dry_run: false,
                max_delete_override: None,
                quiet: true,
                over_budget: sync::OverBudget::Ask,
            },
            &crate::proc::Budget::new(crate::proc::RECONCILE),
        )?;
        if report.pushed + report.deleted > 0 {
            ui::dim(&format!(
                "workspace synced ({} pushed, {} deleted)",
                report.pushed, report.deleted
            ));
        }
        lock = Some(guard);
    }
    // Interactive commands (exec, a shell) may run for hours; only
    // tree-reading commands keep watch/sync blocked that long.
    if !TREE_READERS.contains(&subcommand) {
        lock = None;
    }

    // `config -o` and `export -o` are answered by a STREAM, not by a
    // path. Compose writes to stdout when no file is named, so the flag
    // is taken back out of argv and ssh's stdout lands here — which is
    // why `config` can stay in READ_ONLY: nothing was written on the
    // server, so there is nothing for a pull to go and fetch. It also
    // spares `export` the round trip it used to pay, pushing a 60 MB tar
    // up the workspace and dragging it back down.
    if STREAMED_OUTPUT.contains(&subcommand) {
        // Every span still points at its own bytes: `carry` was handed
        // none of these paths, and a rewrite only ever replaces a range
        // inside a word, so no index it touched has moved.
        //
        // With no `-o` at all, nothing is taken out and nothing lands:
        // compose already writes to stdout and ours is the user's. It
        // still comes through the landing, because a bare `compose
        // export` hands a TAR to whatever is there — and through ssh
        // compose only ever sees a pipe, so it cannot refuse the
        // terminal on its own.
        let stripped = strip_output(&sub_args, &streamed.strip);
        // NEVER a TTY: `export` is a tar stream, and a pty would
        // translate newlines and corrupt every archive that crossed it.
        let (cmd, redacted) = compose_ssh_command(&ssh, &project, &dest, &stripped, Some(false))?;
        let named = format!("compose {subcommand}");
        let on_terminal = match subcommand {
            "export" => crate::bridge::OnTerminal::Refuse { command: &named },
            // `config` prints YAML somebody reads, and `… config | less`
            // has to keep working.
            _ => crate::bridge::OnTerminal::Print,
        };
        // The stack itself did not change, but the picture the service
        // holds of it is stale for the same reason every other command
        // rings this bell.
        crate::intent::nudge(&stack_id);
        drop(lock);
        return crate::bridge::land(
            cmd,
            &redacted,
            streamed.landing.as_ref().map(|out| out.path.as_path()),
            on_terminal,
        );
    }

    let status = exec_remote_compose(&ssh, &project, &dest, &sub_args)?;

    // A command that failed did not do what it said. Roll the flag back
    // to whatever it was: a stack that never came up must not be left
    // looking live, or the service would maintain a workspace that has
    // nothing behind it — forever, since the intent has no TTL.
    //
    // Settled HERE rather than left to the guard's own `Drop`, so the
    // doorbell below rings on the answer that stuck.
    if let Some(guard) = intent.take() {
        guard.settle(declaration_holds(&status, subcommand, &project, &ssh));
    }

    // The heartbeat warning again, now that the declaration has settled.
    // The check above this command read the PREVIOUS declaration, so a
    // fresh `up` — the command that sends the user to a dead port — was
    // structurally silent; `agent::stack_complaint_after` owns that
    // story. Suppressed when the before-half already spoke: one command,
    // one sentence.
    if complained.is_none()
        && let Some(said) = crate::agent::stack_complaint_after(&stack_id)
    {
        ui::warn(&said);
    }

    // Ring the service's doorbell. Whatever that command was, the picture
    // the service holds of this stack is now out of date, and it has no
    // other way to find out than to ask on its own cadence — which is at
    // its slowest precisely when a stack has just come up (see
    // `intent::nudge`; measured at 82 seconds on a real project).
    //
    // Rung for EVERY command, on purpose. "Which compose verbs change a
    // stack" is a list, and a list is a thing that can be wrong — while
    // one needless `docker ps` round costs 0.27 s and answers for every
    // project on that server at once. Rung BEFORE the pull below so the
    // ports can be coming home while it runs: the probe and the tunnels
    // do not want the lock this still holds.
    crate::intent::nudge(&stack_id);

    // The command may well have WRITTEN something — a migration, a
    // lockfile, generated code. Locally that file would simply be in
    // your directory; here it has to travel back, and right now is
    // exactly when we know to look. Never fatal: the command's own exit
    // code is what the user asked for.
    if !read_only
        // No cutoff here: the push was before the command, and whatever
        // the command wrote — a migration, a lockfile, generated code —
        // is exactly what has to come home, however recently it landed.
        && let Err(e) = sync::pull_back(
            &project,
            &footprint,
            &ssh,
            true,
            &crate::proc::Budget::new(crate::proc::RECONCILE),
            None,
        )
    {
        ui::render_error(&e);
    }
    drop(lock);

    // Compose's exit code IS our exit code (scripts depend on it).
    //
    // Known gap, deferred rather than missed: ssh's own 255 arrives
    // at this same match indistinguishable from a compose 255, so a
    // dead link is handed back as the command's verdict.
    // `ssh::run_script` retries on exactly that code; there is no
    // retry here. Seen once in six full-suite runs against a real
    // server, cause unknown — measured, it was NOT the server's
    // connection quota: multiplexing means ulak almost never opens
    // the kind of connection that quota governs. The retry cannot
    // simply be lifted from ssh.rs, which is why this is still open:
    // passthrough streams its output live, and replaying half a
    // streamed `up --build` is not safe (a `down`, `ps` or `logs`
    // would be). The cheaper door, still on the table, changes no
    // behaviour: say "this 255 came from ssh, not from your command".
    // The signal half of "Compose's exit code is Ulak's exit code" is
    // `docker::status_code`'s answer, not a second opinion. A child no
    // exit code came back from is 128 plus the signal's number, the way
    // every shell and CI runner fills it in. This arm was a flat 130 —
    // SIGINT's number told about every signal — so `ulak docker compose
    // logs api | head -5`, which closes the pipe under a child ssh that
    // still has SIGPIPE at SIG_DFL, reported that the operator had
    // pressed Ctrl-C, while the very same pipeline through `ulak docker
    // logs -f api` already answered 141.
    crate::docker::status_code(status)
}

/// The declared intent, and the value to put back if the command it was
/// declared for never reaches its own verdict.
///
/// The flag is written BEFORE the sync, on purpose — a stack that is
/// coming up should be looked after from the first moment, not from
/// whenever compose happens to finish. What that left open was every
/// way out in between, and there are three, each a `?`:
/// `WorkspaceLock::acquire`, `sync::run_sync` and `exec_remote_compose`.
/// Only a compose that actually ran reached the rollback, so an `up`
/// that died at the sync — an unreachable server, a deletion budget the
/// user declined at the prompt — left the workspace declared live with
/// nothing behind it. The intent has no TTL by design, so `status` said
/// "live" for good, `agent::stack_complaint` nagged on every later
/// compose command in that project, and the service kept a job and its
/// ssh probe alive for a stack that had never existed.
struct IntentGuard {
    desired: crate::intent::Desired,
    previous: Option<crate::intent::Desired>,
    armed: bool,
}

impl IntentGuard {
    fn declare(project: &Project, dest: &str, live: bool) -> Self {
        let desired = crate::intent::Desired::of(project, dest, &project.compose_identity(), live);
        let previous = crate::intent::declare(&desired);
        IntentGuard {
            desired,
            previous,
            armed: true,
        }
    }

    /// The command answered for itself: a declaration that still holds
    /// keeps what was declared, one that does not puts back what was
    /// there before. See `declaration_holds` for why that is not the
    /// same question as "did compose exit 0".
    fn settle(mut self, held: bool) {
        self.armed = false;
        if !held {
            self.restore();
            return;
        }
        // The declaration was written before the workspace lock, so an
        // offline retirement of this very server can take it away while
        // compose is still running. A command that HELD is the truth about
        // the stack — an `up` that succeeded is running, a `down` that
        // succeeded is idle — so it is put back rather than left to
        // whichever side raced; a running stack with no declaration is one
        // the service neither tends nor tunnels. Nothing else removes a
        // live declaration: `clean` refuses while one stands.
        if crate::intent::read_desired(&self.desired.stack_id()).is_none() {
            let _ = crate::intent::declare(&self.desired);
        }
    }

    fn restore(&self) {
        let restored = self.previous.clone().unwrap_or_else(|| {
            // A first declaration has no earlier invocation to recover,
            // but keeping its idle record is intentional: fleet/status
            // can still show the stack that was attempted, while the
            // service ignores it because it is not live.
            let mut idle = self.desired.clone();
            idle.live = false;
            idle.updated_unix = crate::intent::now_unix();
            idle
        });
        crate::intent::restore(&self.desired.stack_id(), &restored);
    }
}

impl Drop for IntentGuard {
    fn drop(&mut self) {
        if self.armed {
            self.restore();
        }
    }
}

/// Does the declaration made before the command still hold?
///
/// `status.success()` was the whole answer, and for `down` it still is.
/// For `up`, lifecycle success and process success are different axes.
/// `--wait` may fail after creating containers, while `--no-recreate` may
/// succeed after leaving every old container on another workspace. Orphans
/// can leave old and new containers mixed under one project name.
///
/// Nothing said so, either, which is what makes this worth a round trip
/// rather than a doc note: `agent::stack_complaint` returns `None`
/// for a workspace that is not declared live, so every later `ps`,
/// `logs` and `exec` in that project stayed quiet about the tunnels
/// too. The same rollback fires on a Ctrl-C'd attached `up`, where
/// compose leaves the containers it created behind.
///
/// So every `up` asks Docker instead of guessing. A container counts only
/// when its Compose path labels name THIS invocation's workspace, and all
/// existing containers must agree. Compose's own exit code still reaches
/// the user untouched; this decision only says which declaration the
/// background service may trust.
fn declaration_holds(
    status: &std::process::ExitStatus,
    subcommand: &str,
    project: &Project,
    ssh: &Ssh,
) -> bool {
    // Only `up` declares something Docker's container labels can settle.
    // A failed `down` really does leave the stack where it was, which is
    // exactly what putting the flag back already says.
    if subcommand != "up" {
        return status.success();
    }
    match compose::stack_containers(project, ssh) {
        Ok(labels) => {
            let use_of_workspace =
                compose::workspace_use(&labels, &project.remote_workspace_root());
            match use_of_workspace {
                compose::WorkspaceUse::All => {
                    if !status.success() {
                        ui::warn(&format!(
                            "compose reported failure, but all {} container(s) of this stack use the new workspace — Ulak is keeping it live, so sync and forwarded ports stay",
                            labels.len()
                        ));
                        ui::dim(&format!(
                            "    see them: {}",
                            crate::management::compose_command_for_identity(
                                project,
                                &project.compose_identity(),
                                &["ps", "-a"],
                            )
                        ));
                        ui::dim(&format!(
                            "    take them down: {}",
                            crate::management::compose_command_for_identity(
                                project,
                                &project.compose_identity(),
                                &["down"],
                            )
                        ));
                    }
                }
                compose::WorkspaceUse::Other => {
                    // Says what Ulak DID, not what it kept: an independent
                    // client meeting this stack for the first time has no
                    // earlier declaration, and that is exactly the
                    // scenario this branch exists for.
                    ui::warn(
                        "compose finished without moving this project's existing containers: they all use a different workspace — Ulak did not declare this one live, so it will not sync the wrong bytes",
                    );
                    let up = crate::management::compose_command_for_identity(
                        project,
                        &project.compose_identity(),
                        &["up", "-d", "--force-recreate", "--remove-orphans"],
                    );
                    ui::dim(&format!("    move the whole stack here: {up}"));
                }
                compose::WorkspaceUse::Mixed => {
                    ui::warn(
                        "this Docker project is split across old and new workspaces — Ulak did not assign the mixed stack to this workspace",
                    );
                    let up = crate::management::compose_command_for_identity(
                        project,
                        &project.compose_identity(),
                        &["up", "-d", "--force-recreate", "--remove-orphans"],
                    );
                    ui::dim(&format!("    make every container agree: {up}"));
                }
                compose::WorkspaceUse::Absent => {
                    if status.success() {
                        ui::warn(
                            "compose reported success but left no containers for this project — Ulak did not leave it declared live",
                        );
                    }
                }
            }
            up_declaration_holds(use_of_workspace)
        }
        Err(e) => {
            ui::warn(&format!(
                "Ulak could not verify which workspace this project's containers use, so it did not declare this one live: {}",
                ui::flatten(&e)
            ));
            // Not a dead end: compose already did its work, and the
            // declaration is the only thing missing. Saying so matters
            // because an undeclared stack is silent — the service will
            // not sync it and `ps`/`logs` will not mention its tunnels.
            let up = crate::management::compose_command_for_identity(
                project,
                &project.compose_identity(),
                &["up", "-d"],
            );
            ui::dim(&format!(
                "    the containers are untouched; declare them again: {up}"
            ));
            false
        }
    }
}

fn up_declaration_holds(use_of_workspace: compose::WorkspaceUse) -> bool {
    use_of_workspace == compose::WorkspaceUse::All
}

fn decode_args(argv: Vec<OsString>) -> Result<Vec<String>> {
    argv.into_iter()
        .map(|a| {
            a.into_string().map_err(|bad| {
                fail!("argument {:?} is not valid UTF-8", bad)
                    .now("re-run with UTF-8 arguments")
                    .into_err()
            })
        })
        .collect()
}

// ─── the paths a subcommand's own argv names ────────────────────────

/// Where a streamed subcommand's output lands on THIS machine, and the
/// `-o` occurrences argv has to give back for it to.
#[derive(Debug, Default)]
struct Streamed {
    /// The file the stream goes into, or `None` when compose's own
    /// reading of the last `-o` is already the right one.
    landing: Option<LocalPath>,
    /// Every occurrence to take out, as argv index and value span.
    strip: Vec<(usize, Range<usize>)>,
}

/// pflag reads a repeated string flag to its LAST occurrence, and this
/// read the FIRST — the same bug `bridge::take_flag` was measured into
/// shape for `docker save`, one route over and never carried across.
///
/// Measured on Compose v5.1.2: `compose config -o first.yml -o
/// second.yml` exits 0 having written second.yml, and first.yml is
/// never created. Taking the first left `-o second.yml` in the argv the
/// server ran, so three things happened at once and none of them said
/// so: compose wrote the rendered model to second.yml on the SERVER,
/// remote stdout carried nothing, and `land` renamed that nothing over
/// the user's existing first.yml with ssh exiting 0. Same for `export`,
/// where the nothing replaces a tar.
///
/// The last occurrence is not always a path this machine can land —
/// `composepaths::local` makes none out of `-`, a `~` path or an empty
/// value. Those stay in argv exactly as typed, so compose keeps
/// whatever answer it has for them and nothing lands here; the
/// occurrences they overruled still come out, or the server would open
/// one of those instead.
fn streamed_output(
    subcommand: &str,
    found: &[LocalPath],
    spots: &[(usize, Range<usize>)],
) -> Streamed {
    if !STREAMED_OUTPUT.contains(&subcommand) {
        return Streamed::default();
    }
    let Some((last, earlier)) = spots.split_last() else {
        return Streamed::default();
    };
    match found
        .iter()
        .find(|p| p.index == last.0 && p.direction == Direction::Write)
    {
        Some(landing) => Streamed {
            landing: Some(landing.clone()),
            strip: spots.to_vec(),
        },
        None => Streamed {
            landing: None,
            strip: earlier.to_vec(),
        },
    }
}

/// Take the `-o`/`--output` occurrences back out of argv, so compose
/// writes its answer to stdout and `bridge::land` can put it where the
/// user asked.
///
/// The span covers the VALUE alone. When it starts at 0 the value is its
/// own word and the flag is the word before it; otherwise flag and value
/// share one word (`--output=x`, `-ox`, `-o=x`) and only that word goes.
///
/// Highest index first, because taking a word out moves every index
/// after it — and `spots` arrives in argv order.
fn strip_output(args: &[String], at: &[(usize, Range<usize>)]) -> Vec<String> {
    let mut out = args.to_vec();
    let mut spots = at.to_vec();
    spots.sort_by_key(|(index, _)| std::cmp::Reverse(*index));
    for (index, span) in spots {
        if span.start == 0 {
            out.drain(index - 1..=index);
        } else {
            out.remove(index);
        }
    }
    out
}

/// Put every argv-named path that can travel into the footprint, and
/// respell it the way the server will read it.
///
/// Two paths do NOT get an entry, and each says so out loud rather than
/// being carried badly:
///
///   * one outside the workspace anchor — the server's own filesystem,
///     which ulak does not mirror;
///   * one that is not here yet and that the server may WRITE into (see
///     `may_be_written` for the two shapes that are). `filter_rules`
///     gives a path that does not exist no rsync rule at all, and that
///     gate is load-bearing rather than an oversight: the rule chain is
///     identical in both directions, so a rule that carried `./out` home
///     would carry a database home with it — a real supabase stack names
///     `./volumes/db/data` without it existing locally. The rule stays;
///     only the silence goes. `runspec.rs::missing_sources_stayed_there`
///     answers the identical case for `docker run`, and its doc explains
///     at length why creating the path here first is worse, not better.
///
/// The second warning is keyed on `whole_anchor` because a project whose
/// compose mounts `.` has no narrowing at all and DOES get such a file
/// home — measured, both `cp` directions already work there. Firing
/// unconditionally would be a false alarm on exactly the projects where
/// nothing is wrong, which is the habit that made the old `cp` warning
/// worth deleting.
fn carry(footprint: &mut Footprint, found: &[LocalPath], args: &mut [String], subcommand: &str) {
    let anchor = footprint.anchor.clone();
    let whole = footprint.whole_anchor;
    let mut carried: Vec<LocalPath> = Vec::new();
    for path in found {
        if !path.path.starts_with(&anchor) {
            ui::warn(&format!(
                "{} {} is outside {} — compose will use the server's own path, not this one",
                path.flag,
                path.path.display(),
                anchor.display()
            ));
            continue;
        }
        let exists = path.path.exists();
        if !exists && may_be_written(path) && !whole {
            ui::warn(&format!(
                "{} is not here, so compose will make it on the SERVER — and nothing written into it comes back",
                path.path.display()
            ));
            ui::dim("    create it here and run again");
        }
        // Respelled even when it gets no entry: where it lands on the
        // server is still the mirror of where it was asked for, so a
        // user who creates the file and runs again gets the same place.
        carried.push(path.clone());
        if !exists {
            continue;
        }
        // Pinned, not merely entered. A `.dockerignore` speaks for a
        // BUILD, and none of these flags is one: a `-v` bind source, an
        // `--env-from-file`, an `--ssh` key are the "needed for another
        // reason" case `dockerignore.rs`'s module doc names — the same
        // rule `footprint::pinned_paths` already applies to every
        // non-`Why::Build` path the compose MODEL names, and
        // `carry_build_inputs` to every path `docker build`'s own argv
        // names. This was the one argv scanner that did not.
        //
        // Unpinned, a project that builds from `.` with the commonplace
        // `*.pem` line lost the key `--ssh` named, and the remote build
        // stopped at a file sitting on this machine's disk. The entry
        // pushed below cannot rescue it: the walk's excludes are rank 3
        // in `sync::filter_args` and the footprint's `+` rules rank 4,
        // and under a whole anchor there are no `+` rules at all.
        //
        // Before the dedup below, because a path the model already
        // carries as a build CONTEXT still needs the exemption this
        // reference gives it — what a container is shown at run time is
        // not narrowed by the ignore file of a build sharing its root.
        footprint.pinned.push(path.path.clone());
        let is_dir = match path.shape {
            Shape::Dir => true,
            Shape::File => false,
            Shape::Either => path.path.is_dir(),
        };
        if footprint
            .entries
            .iter()
            .any(|e| e.local == path.path && e.is_dir == is_dir)
        {
            continue;
        }
        footprint.whole_anchor |= is_dir && path.path == anchor;
        footprint.entries.push(Entry {
            local: path.path.clone(),
            is_dir,
            exists,
            empty: is_dir && std::fs::read_dir(&path.path).is_ok_and(|mut d| d.next().is_none()),
            why: why(path.flag),
            service: format!("compose {subcommand}"),
            writable: path.direction == Direction::Write,
        });
    }
    footprint.entries.sort_by(|a, b| a.local.cmp(&b.local));
    // Sorted and deduped the way every other producer of this list
    // leaves it (`docker.rs` does it at both of its call sites), so
    // `BuildFilter`'s prefix scan reads one shape wherever the paths
    // came from.
    footprint.pinned.sort();
    footprint.pinned.dedup();
    composepaths::rewrite(args, &carried, |p| anchor_relative(&anchor, &p.path));
}

/// A path the SERVER may write into, so its absence here is worth
/// saying out loud.
///
/// Two shapes reach it, and only the first used to. An explicit
/// `Direction::Write` is `cp`'s DEST_PATH. The other is a bind SOURCE,
/// and keying the warning on the direction alone made that one
/// unreachable: a `-v` resolves as a `Direction::Read`, because in
/// `composepaths::resolve` the direction answers "may I canonicalize
/// the last component", not "who writes into it". A bind mount is
/// read-write to docker unless the value says otherwise, and its whole
/// point is often the write.
///
/// So the ordinary backup shape — `compose run -v ./backup:/backup web
/// export /backup/dump.json`, where the CONTAINER writes the file —
/// said nothing at all: docker made an empty directory on the server,
/// the container filled it, and this machine showed no sign of either.
///
/// `runspec.rs::sources_left_on_the_server` is the same rule for
/// `docker run` and reads it off `Why::Volume`. This reads it off the
/// same `why` table below, so one flag cannot get two answers.
fn may_be_written(path: &LocalPath) -> bool {
    path.direction == Direction::Write || why(path.flag) == Why::Volume
}

/// The kind of thing this is, in the vocabulary `ulak doctor` and the
/// manifest already speak. `SRC_PATH`, `--output`, `--templates` and
/// `PATH` are none of a volume, an env file or a secret; `Config` is the
/// closest the enum has to "a local file this command named by hand",
/// and a truer variant would mean editing footprint.rs.
fn why(flag: &str) -> Why {
    match flag {
        "-v" | "--volume" => Why::Volume,
        "--env-from-file" => Why::EnvFile,
        "--ssh" => Why::Secret,
        _ => Why::Config,
    }
}

/// How the server should spell `target`, standing where compose will
/// stand.
///
/// Anchored to the workspace ROOT, not to the local cwd, because
/// `compose::remote_prefix` cds to `project.remote_dir()` — which is why
/// `ulak docker compose cp ./x web:/y` run from a subdirectory used to
/// name a file one directory too high, silently and on the wrong
/// machine.
///
/// This deliberately does NOT match `docker.rs::remote_workdir`, which
/// mirrors the local cwd. Both are right for their own command: docker
/// resolves a relative path against the process's directory, so the
/// mirror of the cwd is where a `docker run` must stand, while compose
/// resolves against the PROJECT directory, which is the anchor. Making
/// these two agree would break whichever one was changed.
///
/// The leading `./` is not decoration: compose reads a bare `app:/app`
/// as a NAMED VOLUME, exactly as docker does. Same reasoning, and the
/// same trap, as `runspec.rs`'s `workdir_relative`.
fn anchor_relative(anchor: &Path, target: &Path) -> String {
    match target.strip_prefix(anchor) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
        Ok(rel) => format!("./{}", rel.display()),
        // Unreachable: `carry` only ever hands this a path under the
        // anchor. Spelled out in full rather than unwrapped, because a
        // panic here would be a panic in the middle of somebody's build.
        Err(_) => target.display().to_string(),
    }
}

/// Build the ssh invocation for one remote compose command (identity
/// injected, args quoted, audit-redacted twin returned alongside).
/// `tty`: None = auto (both ends are terminals), Some(true) = force -t
/// (interactive shells), Some(false) = NEVER -t (background children —
/// a backgrounded `ssh -t` puts the shared terminal in raw mode and
/// steals Ctrl-C from the whole process group).
pub fn compose_ssh_command(
    ssh: &Ssh,
    project: &Project,
    dest: &str,
    args: &[String],
    tty: Option<bool>,
) -> Result<(std::process::Command, String)> {
    let mut remote = compose::remote_prefix(project)?;
    let mut remote_redacted = remote.clone();
    for (a, safe) in args.iter().zip(redact_kv(args)) {
        remote.push(' ');
        remote.push_str(&sh_quote(a));
        remote_redacted.push(' ');
        remote_redacted.push_str(&sh_quote(&safe));
    }
    let mut cmd = ssh.command();
    let want_tty =
        tty.unwrap_or_else(|| std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
    if want_tty {
        cmd.arg("-t");
    }
    cmd.arg("--").arg(dest).arg(&remote);
    Ok((cmd, remote_redacted))
}

fn exec_remote_compose(
    ssh: &Ssh,
    project: &Project,
    dest: &str,
    args: &[String],
) -> Result<std::process::ExitStatus> {
    // The `cp` warning that used to stand here fired on every `cp` and
    // said the local path was wrong. Measured, it was true for a project
    // whose compose mounts nothing and FALSE for one that mounts `.` —
    // where both directions already worked. A warning that is right for
    // the wrong reason on half the projects is one users learn to scroll
    // past, so it is gone. `carry` now warns about the two cases that
    // are actually broken, and only when they are.

    // The identity comes from the ONE captured invocation, so `up`,
    // `down` and `ulak status` can no longer disagree about which
    // project they mean. A user `-p` was captured before the
    // subcommand, so `exec -T db psql -p 5432` still carries a
    // container flag, not a project name.
    // TTY policy: a real TTY on both ends gets -t (logs -f colors, exec
    // shells); pipes stay binary-safe (`exec -T db psql < dump.sql`).
    let (mut cmd, remote_redacted) = compose_ssh_command(ssh, project, dest, args, None)?;
    let status = cmd.status().context("cannot spawn ssh for compose")?;
    record_ssh_audit(&cmd, &remote_redacted, status.code());
    Ok(status)
}

/// Audit one ssh invocation, swapping the raw remote command for its
/// redacted twin. EVERY compose-carrying ssh must pass through here —
/// the trail's promise is "no unanswered `what did ulak run?`".
pub fn record_ssh_audit(cmd: &std::process::Command, redacted_remote: &str, exit: Option<i32>) {
    let mut argv = vec![cmd.get_program().to_string_lossy().into_owned()];
    argv.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
    if let Some(last) = argv.last_mut() {
        *last = redacted_remote.to_string();
    }
    crate::audit::record("ssh", &argv, exit);
}

/// One argv, audit-safe.
///
/// Two rules, and the second is why this takes the flag list as an
/// argument instead of hard-coding it: `-p` is a password to `docker
/// login` and a published port to `docker run`. A redactor that cannot
/// tell those apart either leaks the password or writes a trail that
/// does not say which port was published — so the answer comes from the
/// command's own catalog entry, not from a global list of scary words.
pub fn redact_argv(args: &[String], secret_flags: &[&str]) -> Vec<String> {
    let kv = redact_kv(args);
    let mut out = Vec::with_capacity(args.len());
    let mut hide_next = false;
    for (a, safe) in args.iter().zip(kv) {
        if std::mem::take(&mut hide_next) {
            out.push("***".into());
            continue;
        }
        match a.split_once('=') {
            Some((flag, _)) if secret_flags.contains(&flag) => out.push(format!("{flag}=***")),
            _ if secret_flags.contains(&a.as_str()) => {
                hide_next = true;
                out.push(a.clone());
            }
            // pflag lets a short flag swallow the rest of its own word:
            // `-phunter2` is `-p hunter2`. Only two-character flags can
            // do this, and missing it wrote the password verbatim into
            // the trail while the spaced and `=` spellings were clean.
            _ => match attached_short(a, secret_flags) {
                Some(flag) => out.push(format!("{flag}***")),
                None => out.push(safe),
            },
        }
    }
    out
}

/// `-phunter2` → `Some("-p")`. A long flag cannot attach a value without
/// `=`, so only two-character flags are ever this shape.
pub fn attached_short<'a>(arg: &str, flags: &[&'a str]) -> Option<&'a str> {
    flags
        .iter()
        .copied()
        .find(|f| f.len() == 2 && f.starts_with('-') && arg.len() > 2 && arg.starts_with(f))
}

/// Flags whose VALUE is itself a `KEY=VALUE` pair, so the payload sits
/// one `=` deeper than the flag's own and the key to the left of it is
/// whatever docker accepts there — a dotted label as readily as an
/// environment variable.
///
/// `audit.rs` keeps the same list for the last gate. Both need one:
/// this reads a single argv element, that reads the whole assembled
/// remote command line as one string, and neither can hand its answer
/// to the other.
const KV_FLAGS: &[&str] = &[
    "-e",
    "--env",
    "-l",
    "--label",
    "--build-arg",
    "--annotation",
];

/// Every argv element with its `KEY=VALUE` payload emptied out, in
/// order, so a caller can quote them into a command line that is safe
/// to PRINT.
///
/// A list rather than a per-word call because the vouching is stateful:
/// `--label` says the NEXT word is a pair, and a caller redacting word
/// by word cannot see that.
fn redact_kv(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut pair_follows = false;
    for a in args {
        let vouched = pair_follows;
        pair_follows = KV_FLAGS.contains(&a.as_str());
        out.push(redact_env_arg(a, vouched));
    }
    out
}

/// One argv element with any `KEY=VALUE` payload emptied out.
///
/// pflag spells the same assignment five ways and docker 29.4 honours
/// all five: `--env K=V`, `--env=K=V`, `-e K=V`, `-eK=V`, and `-iteK=V`
/// inside a bundle. Only the two with a space in them used to be caught
/// here, so `--env=DB_PASSWORD=hunter2` reached the redacted twin in
/// full. Nothing leaked, because every caller today hands that twin
/// straight to `audit::record`, whose own redaction does catch it — but
/// a twin that is safe only for as long as nobody prints it is not a
/// redaction, it is a trap for the first `--dry-run` echo or error
/// message that names it.
///
/// `vouched` means the PREVIOUS word was a `KV_FLAGS` flag, so however
/// this word's key looks, docker read it as one. Without that word of
/// honour the key has to look like an environment variable, and that is
/// what keeps the rest legible: `--filter=P` is the question being
/// asked, not a payload.
fn redact_env_arg(a: &str, vouched: bool) -> String {
    // `--env=KEY=VALUE`: the flag vouches for the rest of its own word.
    if let Some((flag, pair)) = a.split_once('=')
        && KV_FLAGS.contains(&flag)
    {
        return format!("{flag}={}", hide_value(pair, true));
    }
    // A shorthand swallows the rest of its own word, and a BUNDLE of
    // them does too: `-eK=V` and `-iteK=V` both reach the daemon with K
    // set (measured, docker 29.4). Neither is parsed — everything up to
    // the first `=` is kept, which leaves the flags AND the key readable
    // and drops only the value.
    //
    // `attached_short` would name the `-e` in the first shape and cannot
    // see the second without that command's whole short-flag table, and
    // naming it would not change a character of the answer: the key
    // survives either way, so splitting at the flag and splitting at the
    // first `=` write the same word. Being broad instead costs nothing
    // legible — spelled apart, `-o type=local` already came through as
    // `type=***`, so all that changes is that writing a flag attached no
    // longer keeps a value that writing it spaced has always dropped.
    // It is also the rule `audit.rs` already applies at the last gate,
    // so the twin and the trail now say the same thing.
    if a.starts_with('-') && !a.starts_with("--") {
        return hide_value(a, true);
    }
    hide_value(a, vouched)
}

/// Drop the VALUE of a `KEY=VALUE` word, keeping the key — `-e ***`
/// answers nothing about which variable the container was handed.
fn hide_value(word: &str, vouched: bool) -> String {
    match word.split_once('=') {
        Some((key, _))
            if !key.is_empty()
                && (vouched || key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')) =>
        {
            format!("{key}=***")
        }
        _ => word.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The declaration is written before the workspace lock, so a
    /// `clean --forget-destination` for this server can remove it while
    /// compose is still running. A command that then succeeds is the
    /// truth about the stack and must leave a declaration behind, or the
    /// stack runs with no service and no tunnels. The refusing half: a
    /// command that did NOT hold still restores nothing over a retirement.
    #[test]
    fn a_command_that_held_puts_back_a_declaration_retired_underneath_it() {
        let desired = crate::intent::Desired {
            schema: crate::intent::SCHEMA,
            live: true,
            workspace_id: "checkout-a".into(),
            workspace_namespace: "client-a".into(),
            destination: format!("raced-server-{}", std::process::id()),
            identity: "api".into(),
            cwd: "/checkout/a".into(),
            argv_globals: Vec::new(),
            compose_env: std::collections::BTreeMap::new(),
            updated_unix: 1,
        };
        let id = desired.stack_id();

        let guard = IntentGuard {
            desired: desired.clone(),
            previous: crate::intent::declare(&desired),
            armed: true,
        };
        std::fs::remove_dir_all(crate::intent::stack_dir_path(&id).unwrap()).unwrap();
        guard.settle(true);
        assert!(
            crate::intent::read_desired(&id).is_some_and(|d| d.live),
            "an up that succeeded leaves its declaration"
        );

        let guard = IntentGuard {
            desired: desired.clone(),
            previous: None,
            armed: true,
        };
        std::fs::remove_dir_all(crate::intent::stack_dir_path(&id).unwrap()).unwrap();
        guard.settle(false);
        assert!(
            crate::intent::read_desired(&id).is_none(),
            "an up that failed restores nothing over a retirement"
        );
    }

    /// Both exit statuses can lie about lifecycle ownership: successful
    /// `--no-recreate` can leave the old workspace in place, while a failed
    /// `--wait` can leave every container on the new one. Only unanimous
    /// Docker path labels settle the declaration.
    #[test]
    fn an_up_declaration_requires_every_container_to_use_the_new_workspace() {
        assert!(up_declaration_holds(compose::WorkspaceUse::All));
        for not_ours in [
            compose::WorkspaceUse::Absent,
            compose::WorkspaceUse::Other,
            compose::WorkspaceUse::Mixed,
        ] {
            assert!(!up_declaration_holds(not_ours), "{not_ours:?}");
        }
    }

    /// Compose's exit code is answered in exactly one place.
    ///
    /// Every other route ends in `docker::status_code`, whose
    /// `shell_code` reports a signal death as 128 plus the signal — the
    /// number a shell would have filled in. This one arm was a flat 130,
    /// SIGINT's number told about every signal, so `ulak docker compose
    /// logs api | head -5` claimed a Ctrl-C where `ulak docker logs -f
    /// api` already answered 141 for the same pipeline. A second opinion
    /// on this question is the bug, so what is pinned is that there is
    /// only one.
    #[test]
    fn the_compose_route_answers_its_exit_code_where_every_other_route_does() {
        let mine = include_str!("passthrough.rs");
        assert!(
            mine.contains("crate::docker::status_code(status)"),
            "the compose route grew its own exit-code arithmetic again"
        );
        // Spelled at runtime so this test does not match itself, the
        // same way `sync.rs`'s one-way-up guard spells its needle.
        let flat = format!("ExitCode::from({})", 130);
        assert!(
            !mine.contains(&flat),
            "a flat 130 is SIGINT's number told about every signal"
        );
    }

    /// A `compose up --wait` waits for a whole stack to become healthy,
    /// which on a hardened install is minutes — and nothing here may put
    /// a clock on it.
    ///
    /// The two sync legs each carry their own `proc::Budget`, on purpose
    /// two rather than one, and the command that runs between them is
    /// charged for neither. So the compose child runs on a bare
    /// `status()`. That reads like an omission next to every other
    /// subprocess in this codebase, which is exactly why it is pinned:
    /// the tempting "fix" is to wrap it in `proc::run_bounded` like the
    /// rest, and the result would be an install cut off in its third
    /// minute with the stack half up and no way to say why.
    ///
    /// `e2e_flags::an_up_that_waits_longer_than_a_budget_is_not_cut_off`
    /// is the same rule against a real server; this one costs nothing
    /// and fails at the moment the code changes rather than the moment
    /// somebody runs the suite.
    #[test]
    fn the_compose_child_runs_under_no_clock_of_ulaks_own() {
        let mine = include_str!("passthrough.rs");
        let at = mine
            .find("fn exec_remote_compose(")
            .expect("this test is named after that function");
        let body = &mine[at..];
        let end = body.find("\n}\n").expect("a function has an end");
        let body = &body[..end];
        assert!(
            body.contains(".status()"),
            "the compose child stopped running on a bare status()"
        );
        for clock in ["run_bounded", "Budget", "timeout"] {
            assert!(
                !body.contains(clock),
                "a {clock} appeared around the compose child — a stack coming up \
                 legitimately takes as long as it takes, and the two sync legs are \
                 budgeted separately for exactly that reason"
            );
        }
    }

    /// A path the SUBCOMMAND named is pinned, not merely entered.
    ///
    /// A `.dockerignore` speaks for a build, and none of these flags is
    /// one. `footprint::pinned_paths` already exempts every
    /// non-`Why::Build` path the compose MODEL names and
    /// `carry_build_inputs` every path `docker build`'s argv names; this
    /// was the one argv scanner that did not, so a project building from
    /// `.` with the commonplace `*.pem` line lost the key `--ssh` had
    /// just pointed at — and the remote build then stopped on a file
    /// sitting right here.
    ///
    /// The entry alone cannot carry it: the walk's excludes outrank the
    /// footprint's `+` rules in `sync::filter_args`, and under a whole
    /// anchor there are no `+` rules at all.
    #[test]
    fn a_path_the_subcommand_named_overrides_a_build_that_ignores_it() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = temp.path().canonicalize().unwrap();
        std::fs::write(anchor.join("deploy.pem"), "KEY").unwrap();
        std::fs::create_dir(anchor.join("data")).unwrap();

        let mut fp = Footprint {
            anchor: anchor.clone(),
            entries: Vec::new(),
            server_refs: Vec::new(),
            whole_anchor: false,
            contexts: Vec::new(),
            pinned: Vec::new(),
            model_json: String::new(),
        };
        // The key `--ssh` names on a build and the bind source `-v`
        // names on a run: the two shapes, each on the subcommand that
        // really owns the flag.
        for (subcommand, spelled, want) in [
            (
                "build",
                vec!["build", "--ssh", "default=./deploy.pem"],
                "deploy.pem",
            ),
            ("run", vec!["run", "-v", "./data:/data", "web"], "data"),
        ] {
            let mut args: Vec<String> = spelled.iter().map(|s| s.to_string()).collect();
            let found = composepaths::scan(&anchor, &args, Some(0)).unwrap();
            assert_eq!(found.len(), 1, "{spelled:?} is what this test is built on");

            carry(&mut fp, &found, &mut args, subcommand);
            assert!(
                fp.pinned.iter().any(|p| p.ends_with(want)),
                "{want} was entered but not pinned, so a build's ignore file can \
                 still drop it: {:?}",
                fp.pinned
            );
        }
        assert!(
            fp.pinned.windows(2).all(|w| w[0] < w[1]),
            "sorted and deduped: {:?}",
            fp.pinned
        );
    }

    /// The warning for a path that is not here was gated on
    /// `Direction::Write`, and `composepaths` fixes a `-v` at
    /// `Direction::Read` — so for a bind SOURCE the branch was
    /// unreachable code that read like a working safeguard.
    ///
    /// The shape it exists for is the ordinary containerized backup:
    /// `compose run -v ./backup:/backup web export /backup/dump.json`
    /// on a machine where `./backup` has not been made yet. Docker
    /// creates the source itself, on the SERVER; the container writes
    /// the dump into it; the developer's disk never hears about it.
    ///
    /// Asserted on the RULE rather than on the terminal, for the reason
    /// `runspec.rs::sources_left_on_the_server` is split out: a test of
    /// the message could only rebuild the condition by hand and then
    /// keep agreeing with its own copy of it.
    #[test]
    fn a_bind_source_that_is_not_here_yet_is_one_the_server_will_write_into() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();

        let cases: &[(&[&str], bool, &str)] = &[
            (
                &["run", "-v", "./backup:/backup", "web"],
                true,
                "a bind source is read-write to docker",
            ),
            (
                &["run", "--volume=./backup:/backup", "web"],
                true,
                "the same flag spelled long",
            ),
            (
                &["cp", "web:/etc/hosts", "./out/hosts"],
                true,
                "cp's DEST_PATH was already covered",
            ),
            (
                &["run", "--env-from-file", "./missing.env", "web"],
                false,
                "compose READS this one — a missing file is compose's own error to report",
            ),
        ];
        for (spelled, want, why_) in cases {
            let args: Vec<String> = spelled.iter().map(|s| s.to_string()).collect();
            let found = composepaths::scan(&cwd, &args, Some(0)).unwrap();
            assert_eq!(found.len(), 1, "{spelled:?} is what this test is built on");
            assert_eq!(may_be_written(&found[0]), *want, "{spelled:?}: {why_}");
        }
    }

    #[test]
    fn read_only_set_is_sorted_and_known() {
        // BTree-ish sanity so additions stay reviewable.
        let mut sorted = READ_ONLY.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, READ_ONLY);
        assert!(READ_ONLY.contains(&"logs"));
        assert!(!READ_ONLY.contains(&"up"));
        assert!(!READ_ONLY.contains(&"exec"));
    }

    #[test]
    fn env_args_are_redacted_before_quoting() {
        assert_eq!(
            redact_env_arg("DB_PASSWORD=s3cret", false),
            "DB_PASSWORD=***"
        );
        assert_eq!(redact_env_arg("--flag=value", false), "--flag=value"); // key has '-'
        assert_eq!(redact_env_arg("plain", false), "plain");
        // A flag that said "a pair follows" makes any key a key.
        assert_eq!(
            redact_env_arg("com.acme.token=s3cret", true),
            "com.acme.token=***"
        );
    }

    /// pflag spells one env var five ways and docker 29.4 honours all
    /// five. The twin `compose_ssh_command` and `Remote::spell` hand
    /// back exists to be SAFE TO PRINT — a `--dry-run` echo, a
    /// `ui::info`, an error message — so it has to be clean in every
    /// spelling, not only in the two that also survive the audit
    /// trail's own second gate. `--env=K=V` and `-eK=V` did not.
    #[test]
    fn the_redacted_twin_holds_no_secret_in_any_spelling() {
        let spellings: &[(&[&str], &str)] = &[
            (
                &["run", "--env", "DB_PASSWORD=s3cret", "web"],
                "DB_PASSWORD",
            ),
            (&["run", "--env=DB_PASSWORD=s3cret", "web"], "DB_PASSWORD"),
            (&["run", "-e", "DB_PASSWORD=s3cret", "web"], "DB_PASSWORD"),
            (&["run", "-eDB_PASSWORD=s3cret", "web"], "DB_PASSWORD"),
            (&["run", "-ite", "DB_PASSWORD=s3cret", "web"], "DB_PASSWORD"),
            (&["run", "-iteDB_PASSWORD=s3cret", "web"], "DB_PASSWORD"),
            // The same payload under the other flags that carry one —
            // a dotted key only ever survived because of its charset.
            (
                &["build", "--label", "com.acme.token=s3cret", "."],
                "com.acme.token",
            ),
            (
                &["build", "--label=com.acme.token=s3cret", "."],
                "com.acme.token",
            ),
            (&["build", "-lcom.acme.token=s3cret", "."], "com.acme.token"),
            (&["build", "--build-arg=NPM_TOKEN=s3cret", "."], "NPM_TOKEN"),
        ];
        for (args, key) in spellings {
            let twin = redact_argv(&v(args), &[]).join(" ");
            assert!(!twin.contains("s3cret"), "{args:?} leaked: {twin}");
            assert!(twin.contains(key), "{args:?} lost the key: {twin}");
        }
    }

    /// What must stay readable. A twin that redacts the question as well
    /// as the payload answers nothing, and these are the words a reader
    /// needs: which filter, which port, which mount.
    #[test]
    fn the_shape_of_a_command_survives_the_twin() {
        let twin = redact_argv(
            &v(&[
                "run",
                "--filter=dangling",
                "-p8080:80",
                "-v/data:/data",
                "--mount=type=bind,source=/a,target=/b",
            ]),
            &[],
        );
        assert_eq!(
            twin,
            v(&[
                "run",
                "--filter=dangling",
                "-p8080:80",
                "-v/data:/data",
                "--mount=type=bind,source=/a,target=/b",
            ])
        );
    }

    #[test]
    fn non_utf8_args_are_rejected_kindly() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0x66, 0xff]);
        let err = decode_args(vec![bad]).unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tree_readers_and_read_only_stay_sorted_and_disjoint_where_it_matters() {
        let mut sorted = TREE_READERS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, TREE_READERS);
        // `cp` reads the workspace while it copies, so it holds the lock.
        assert!(TREE_READERS.contains(&"cp"));
        // …and it is not read-only: its bytes have to travel first.
        assert!(!READ_ONLY.contains(&"cp"));
    }

    #[test]
    fn only_config_and_export_answer_with_a_stream() {
        // `bridge convert` also spells its output `-o`, but writes a
        // directory tree — no stream carries one, so it must stay out.
        assert!(STREAMED_OUTPUT.contains(&"config"));
        assert!(STREAMED_OUTPUT.contains(&"export"));
        assert!(!STREAMED_OUTPUT.contains(&"bridge"));
    }

    #[test]
    fn a_streamed_output_flag_is_taken_back_out_of_argv() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        for spelling in [
            v(&["config", "-o", "rendered.yaml"]),
            v(&["config", "--output", "rendered.yaml"]),
            v(&["config", "--output=rendered.yaml"]),
            v(&["config", "-orendered.yaml"]),
        ] {
            let out = streamed(&cwd, "config", &spelling);
            let landing = out.landing.expect("config -o is a stream");
            assert_eq!(landing.path, cwd.join("rendered.yaml"), "{spelling:?}");
            assert_eq!(
                strip_output(&spelling, &out.strip),
                v(&["config"]),
                "the flag must not reach the server, or compose writes it there: {spelling:?}"
            );
        }
    }

    /// What `run` does with one command's argv, without a server.
    fn streamed(cwd: &Path, subcommand: &str, args: &[String]) -> Streamed {
        let found = composepaths::scan(cwd, args, Some(0)).unwrap();
        let spots = composepaths::output_spots(args, Some(0));
        streamed_output(subcommand, &found, &spots)
    }

    #[test]
    fn a_subcommand_with_no_output_flag_is_not_a_stream() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        // Compose already writes to stdout, and passthrough already
        // inherits it — routing this through the landing would only
        // refuse the terminal the user is deliberately printing to.
        let out = streamed(&cwd, "config", &v(&["config"]));
        assert!(out.landing.is_none());
        assert!(out.strip.is_empty());
        // `-o -` is the same answer said out loud, and compose says it
        // to itself: the flag stays, so nothing has to be taken out.
        let out = streamed(&cwd, "config", &v(&["config", "-o", "-"]));
        assert!(out.landing.is_none());
        assert!(out.strip.is_empty());
    }

    /// pflag reads a repeated string flag to its LAST occurrence. Taking
    /// the first left the second in argv for the server to open, so
    /// compose wrote the model to second.yml THERE, remote stdout
    /// carried nothing, and `land` renamed that nothing over the user's
    /// existing first.yml with ssh exiting 0. `bridge::take_flag` was
    /// measured into shape for the same bug on `docker save`; this route
    /// never got it.
    #[test]
    fn a_repeated_output_flag_is_read_to_the_last_one_and_none_are_forwarded() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        for spelling in [
            v(&["config", "-o", "first.yml", "-o", "second.yml"]),
            v(&["config", "-o", "first.yml", "--output=second.yml"]),
            v(&["config", "-ofirst.yml", "-o", "second.yml"]),
            v(&["config", "--output", "first.yml", "-osecond.yml"]),
            // Three, because "the last" and "the second" are the same
            // answer for two and this has to be the former.
            v(&["config", "-o", "a.yml", "-o", "b.yml", "-o", "second.yml"]),
        ] {
            let out = streamed(&cwd, "config", &spelling);
            let landing = out.landing.as_ref().expect("a repeated -o still lands");
            assert_eq!(landing.path, cwd.join("second.yml"), "{spelling:?}");
            assert_eq!(
                strip_output(&spelling, &out.strip),
                v(&["config"]),
                "an -o survived into argv, so the server opens it: {spelling:?}"
            );
        }
    }

    /// The last `-o` decides, and `composepaths::local` does not make a
    /// path out of every value: `-` and a `~` path are left for compose
    /// to answer for itself. Nothing lands here for those — but the file
    /// they overruled must still come out, or the server would open that
    /// one and write the whole answer where nobody asked for it.
    ///
    /// What compose does with those two is its own business and stays
    /// its own: the word is forwarded exactly as typed. Measured on
    /// v5.1.2 it is not what the name suggests — `config -o -` writes a
    /// FILE called `-`, and `config -o '~/x.yaml'` fails with `open
    /// ~/x.yaml: no such file or directory` — which is a reading this
    /// route already had before repeats were read at all, and is not
    /// what this test is about.
    #[test]
    fn a_last_output_flag_this_machine_cannot_land_takes_only_the_others_out() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();

        let args = v(&["config", "-o", "rendered.yaml", "-o", "-"]);
        let out = streamed(&cwd, "config", &args);
        assert!(out.landing.is_none());
        assert_eq!(strip_output(&args, &out.strip), v(&["config", "-o", "-"]));

        let args = v(&["config", "-o", "rendered.yaml", "-o", "~/there.yaml"]);
        let out = streamed(&cwd, "config", &args);
        assert!(out.landing.is_none());
        assert_eq!(
            strip_output(&args, &out.strip),
            v(&["config", "-o", "~/there.yaml"])
        );

        // The other way round the last one IS a file here, so it lands
        // and the one before it goes.
        let args = v(&["config", "-o", "-", "-o", "rendered.yaml"]);
        let out = streamed(&cwd, "config", &args);
        assert_eq!(
            out.landing.as_ref().map(|p| p.path.clone()),
            Some(cwd.join("rendered.yaml"))
        );
        assert_eq!(strip_output(&args, &out.strip), v(&["config"]));
    }

    #[test]
    fn a_path_is_respelled_against_the_anchor_not_the_directory_it_was_typed_in() {
        let anchor = Path::new("/w/proj");
        // The bug this fixes: typed in proj/sub, `./subfile.txt` used to
        // reach the server as `./subfile.txt` and resolve one directory
        // too high, because compose runs from the anchor.
        assert_eq!(
            anchor_relative(anchor, Path::new("/w/proj/sub/subfile.txt")),
            "./sub/subfile.txt"
        );
        assert_eq!(anchor_relative(anchor, Path::new("/w/proj/data")), "./data");
        // The anchor itself is `.`, not `./`.
        assert_eq!(anchor_relative(anchor, Path::new("/w/proj")), ".");
    }

    #[test]
    fn a_respelled_bind_source_never_comes_out_as_a_named_volume() {
        // `app:/app` is a NAMED VOLUME to compose, so dropping the `./`
        // would trade a wrong directory for silent daemon state — the
        // same trap runspec.rs's `workdir_relative` documents.
        let spelled = anchor_relative(Path::new("/w/proj"), Path::new("/w/proj/app"));
        assert!(
            spelled.starts_with("./"),
            "{spelled} would be a volume name"
        );
    }

    #[test]
    fn each_carried_flag_keeps_the_vocabulary_doctor_already_speaks() {
        assert_eq!(why("-v"), Why::Volume);
        assert_eq!(why("--volume"), Why::Volume);
        assert_eq!(why("--env-from-file"), Why::EnvFile);
        assert_eq!(why("--ssh"), Why::Secret);
        // Everything a command names by hand — cp's two positionals,
        // --output, --templates, PATH — has no truer variant than this.
        assert_eq!(why("SRC_PATH"), Why::Config);
        assert_eq!(why("--templates"), Why::Config);
    }
}
