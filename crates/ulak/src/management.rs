//! Project selection for Ulak's own root management commands.
//!
//! Docker Compose starts every invocation from argv, environment and file
//! discovery. `invocation.rs` owns that rule and this module never changes
//! it. A root command asks a different question once Ulak has declared a
//! stack: status, doctor, sync and clean must address the exact Compose
//! files, project name, destination and sync workspace that Ulak is already
//! maintaining. `Desired` is the one complete, validated answer.
//!
//! A stack NAME typed after the command (`ulak status shop`) outranks all of
//! the rungs below: it names one declaration on this machine by its Compose
//! project name, from any directory, and `name@destination` settles a name
//! two servers share. Typed on the command line, it also outranks
//! `COMPOSE_*` from the environment, which is Docker's own precedence:
//! flags win over the environment. Typed Compose globals are a second answer
//! to the same question, so they are refused rather than merged.
//!
//! Without a name, selection has three rungs, in this order:
//!   1. globals or owned `COMPOSE_*` input supplied for this command;
//!   2. the most specific validated Desired declaration for this local
//!      Ulak workspace and cwd;
//!   3. ordinary Docker discovery, but never across a nearer Ulak config
//!      boundary.
//!
//! Several declarations at the same specificity are `Context::Several`, and
//! no rung guesses between them. What that means is each command's own
//! answer. `status` describes them all side by side. `doctor` and `sync`
//! ask in a terminal (`Choose::Ask`). `clean` removes a workspace, so it
//! never takes a choice from an Enter key (`Choose::Named`). Every refusal
//! lists each choice as its short, named command. A declaration is rebuilt
//! through `Desired::rebuild_for_stack`; this module never parses its argv
//! or trusts its paths a second way.
//!
//! This module also owns pasteable command rendering. A root command for a
//! declared project is spelled by name (`ulak sync shop --dry-run`): the
//! name selects the declaration, and with it the files, project name and
//! server it pinned. Spelling every absolute `-f` and `--project-directory`
//! instead made each hint a line nobody would retype. Everything else
//! printed into the shell that ran the command is spelled for that shell:
//! the shortest globals that `Invocation::capture_in` reads back to the same
//! invocation from its directory and environment, measured candidate by
//! candidate (`here_globals_in`), and every selector in absolute form when
//! nothing shorter agrees. Text the service keeps is read later and from
//! anywhere, so it keeps the absolute form (`compose_command`). Either way
//! the historical identity rides along as `-p`, and every root-command
//! argument travels with the line so a `--dry-run` cannot turn into a
//! mutating retry. `--raw` asks for the absolute form everywhere instead
//! (`Spelling::Raw`): no names and no shortening, and the several-stacks
//! refusal goes back to its full lines, each labelled by a shell comment so
//! it still pastes as it stands. It is a choice made once per process, by
//! `main`; the `_in` renderers take it as an argument so tests never share it.
//!
//! A declaration outranks a later `host` edit, and this module is where that
//! is said: `warn_on_destination_drift` speaks once, at the one site a
//! declaration is chosen, and every other surface reads
//! `Project::destination()` rather than comparing the two again. The way
//! out for a server that no longer exists is `clean --forget-destination`,
//! selected by DESTINATION through `declared_on`: today's config no longer
//! names that server, so nothing that starts from the config could reach the
//! declaration. A named root command needs no aiming, because the name
//! carries the declared server. Every other pasteable command rendered from
//! a project passes through `render_project` or `render_here`, and both
//! put `env ULAK_HOST=<declared>` in front when a pinned project's config no
//! longer answers the same server (`aim_for`): the globals alone bind a
//! paste to the config, which for a drifted checkout is a different stack
//! than the line describes.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context as _, Result};

use crate::config::{self, Project, Workspace};
use crate::intent::{self, Desired, Rebuilt};
use crate::invocation::Invocation;
use crate::ssh::sh_quote;
use crate::ui::{self, fail};

pub enum Context {
    Project(Box<Project>),
    /// More than one declaration answers this directory equally well.
    Several(Several),
    Workspace(Box<Workspace>),
    Outside,
}

/// How a command that acts on ONE stack settles `Context::Several`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choose {
    /// Ask in a terminal. Without one, or when the question is dismissed,
    /// refuse and list the named commands.
    Ask,
    /// Always refuse and list the named commands. For a command that
    /// removes something: the name is typed, never taken from an Enter key.
    Named,
}

struct Candidate {
    stack_id: String,
    desired: Desired,
    rebuilt: Rebuilt,
    score: usize,
}

/// Locate the context for one of Ulak's own root commands.
///
/// `stack` is the name typed after the command, if any. It outranks every
/// other rung; see the module header.
pub fn locate(
    globals: &[String],
    command: &str,
    command_args: &[String],
    stack: Option<&str>,
) -> Result<Context> {
    if let Some(spec) = stack {
        return named(globals, command, command_args, spec)
            .map(|project| Context::Project(Box::new(project)));
    }
    if !globals.is_empty() || crate::invocation::compose_environment_is_explicit() {
        return Project::locate(globals).map(|project| Context::Project(Box::new(project)));
    }

    let cwd = current_dir()?;
    let boundary = config::workspace_home(&cwd);

    let mut candidates = declared_candidates(&cwd, boundary.as_deref());
    if let Some(best) = candidates.iter().map(|candidate| candidate.score).max() {
        candidates.retain(|candidate| candidate.score == best);
        if candidates.len() > 1 {
            return Ok(Context::Several(Several::new(cwd, candidates)));
        }
        // `best` came from this vector and `retain` keeps every row with
        // that score, so this branch has exactly one element.
        let candidate = candidates.remove(0);
        return adopt(&candidate.desired, candidate.rebuilt)
            .map(|project| Context::Project(Box::new(project)));
    }

    // Build Docker's discovered answer without activating its audit trail.
    // A nearer Ulak config may still veto it: activating first would bind the
    // process-wide OnceLock to the ancestor this command is about to reject.
    match Invocation::capture(&[]) {
        Ok(inv) => {
            let project = Project::locate_from(inv)?;
            let discovered_boundary = config::workspace_home(&project.inv.project_dir);
            if boundary.is_some() && discovered_boundary != boundary {
                return workspace_or_outside(boundary);
            }
            activate(&project);
            Ok(Context::Project(Box::new(project)))
        }
        Err(e) if config::is_projectless(&e) => workspace_or_outside(boundary),
        Err(e) => Err(e),
    }
}

/// The project-only half used by commands that have no workspace form.
pub fn project(
    globals: &[String],
    command: &str,
    command_args: &[String],
    stack: Option<&str>,
    how: Choose,
) -> Result<Project> {
    match locate(globals, command, command_args, stack)? {
        Context::Project(project) => Ok(*project),
        Context::Several(several) => several.choose(command, command_args, how),
        Context::Workspace(workspace) => Err(fail!(
            "no Compose project is selected in the Ulak workspace {}",
            workspace.root.display()
        )
        .now(format!(
            "name its files explicitly: {}",
            explicit_example(command, command_args)
        ))
        .into_err()),
        Context::Outside => Err(fail!("no Compose project or Ulak workspace here")
            .now("cd into the project, or create a workspace: ulak init <ssh-host>")
            .now(format!(
                "name its files explicitly: {}",
                explicit_example(command, command_args)
            ))
            .into_err()),
    }
}

/// How the pasteable commands this process prints are spelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spelling {
    /// By stack name, or the shortest globals that read back in this shell.
    Short,
    /// `--raw`: every selector absolute and no stack names, the spelling
    /// that reads the same from any directory without a declaration to
    /// look up. Long, so it is asked for rather than shown by default.
    Raw,
}

static SPELLING: OnceLock<Spelling> = OnceLock::new();

/// Spell every command this process prints in full. Set once, by `main`,
/// for `--raw`. Tests call the `_in` renderers instead: a process-wide
/// switch set by one test would leak into every test beside it.
pub fn spell_raw() {
    SPELLING.get_or_init(|| Spelling::Raw);
}

pub fn spelling() -> Spelling {
    SPELLING.get().copied().unwrap_or(Spelling::Short)
}

/// An exact root command for this invocation, safe to paste into a shell.
///
/// By name for a declared project, which reads the same from any
/// directory: see the module header. A project with no declaration is
/// only ever rendered by the command a user just typed, so it is spelled
/// for that shell, never bare: a bare root command would first consult the
/// declarations and could select a different stack. A pinned project
/// whose declaration has since been removed keeps the full spelling,
/// because the service renders those and its text is read elsewhere.
pub fn root_command(project: &Project, tail: &[&str]) -> String {
    root_command_in(spelling(), project, tail)
}

fn root_command_in(spelling: Spelling, project: &Project, tail: &[&str]) -> String {
    let tail = words(tail);
    if spelling == Spelling::Raw {
        return render_project(&["ulak"], project, &tail);
    }
    match (declared_selector(project), tail.split_first()) {
        (Some(selector), Some((command, args))) => named_command(command, args, &selector),
        _ if project.stack_pin.is_some() => render_project(&["ulak"], project, &tail),
        _ => render_here(
            &["ulak"],
            project,
            project.compose_model_project_name(),
            &tail,
            Bare::Refused,
        ),
    }
}

/// What to type after a root command to reach one declared stack: its
/// Compose project name, or `name@destination` when another server in
/// `catalog` has a stack of the same name. `resolve` reads it back by the
/// same rule.
pub(crate) fn selector_in(catalog: &[(String, Desired)], desired: &Desired) -> String {
    let shared = catalog
        .iter()
        .filter(|(_, other)| other.identity == desired.identity)
        .count();
    if shared > 1 {
        format!("{}@{}", desired.identity, desired.destination)
    } else {
        desired.identity.clone()
    }
}

fn declared_selector(project: &Project) -> Option<String> {
    let pin = project.stack_pin.as_ref()?;
    let catalog = intent::catalog();
    let (_, desired) = catalog.iter().find(|(_, desired)| {
        desired.identity == pin.identity && desired.destination == pin.destination
    })?;
    Some(selector_in(&catalog, desired))
}

/// `ulak <command> <selector> <args…>`, shell-quoted.
fn named_command(command: &str, command_args: &[String], selector: &str) -> String {
    let mut words = vec![
        "ulak".to_string(),
        command.to_string(),
        selector.to_string(),
    ];
    words.extend_from_slice(command_args);
    shell_join(&words)
}

/// An exact Docker Compose command for this invocation, safe to paste from
/// any directory. For text read later and elsewhere, such as the service's
/// notes; a line printed into the shell that ran this command uses
/// `compose_command_here`.
pub fn compose_command(project: &Project, tail: &[&str]) -> String {
    render_project(&["ulak", "docker", "compose"], project, &words(tail))
}

/// `compose_command`, spelled for the shell that ran this command: the
/// shortest globals that read back to the same invocation from its
/// directory and environment (`here_globals_in`).
pub fn compose_command_here(project: &Project, tail: &[&str]) -> String {
    if spelling() == Spelling::Raw {
        return compose_command(project, tail);
    }
    render_here(
        &["ulak", "docker", "compose"],
        project,
        project.compose_model_project_name(),
        &words(tail),
        Bare::Allowed,
    )
}

/// `compose_command_here` for a known resolved identity over the selected
/// files. The server model, historical lifecycle state or Docker labels may
/// know `-p` more precisely than today's local inputs; spelling it here
/// keeps that answer attached to the invocation that can act on it.
pub fn compose_command_here_as(project: &Project, identity: &str, tail: &[&str]) -> String {
    if spelling() == Spelling::Raw {
        let line = render(
            &["ulak", "docker", "compose"],
            &project.inv,
            Some(identity),
            &words(tail),
        );
        return aim_for(project, line);
    }
    render_here(
        &["ulak", "docker", "compose"],
        project,
        Some(identity),
        &words(tail),
        Bare::Allowed,
    )
}

fn words(tail: &[&str]) -> Vec<String> {
    tail.iter().map(|word| (*word).to_string()).collect()
}

/// Say once, here, that a declaration outranks a later config edit.
///
/// ONE caller, `adopt`, where every way of choosing a declaration for a
/// root command ends: by scope, by name or from the picker. `init` passes
/// through here too and addresses no server, so the headline is about the
/// checkout, not about "this command".
fn warn_on_destination_drift(project: &Project) {
    let Ok(destination) = project.destination() else {
        return;
    };
    // `Unconfigured` is not a disagreement — nothing else claims to know.
    let Some(config::DestinationDrift::Configured(configured)) = destination.drift else {
        return;
    };
    let mut lines = drift_lines(&destination.dest, &configured).into_iter();
    if let Some(headline) = lines.next() {
        ui::warn(&headline);
    }
    for line in lines {
        ui::dim(&line);
    }
}

/// The words, apart from the printing: headline first, then the dim lines
/// under it. Built here so a test can read them without a tty.
fn drift_lines(pinned: &str, configured: &str) -> Vec<String> {
    vec![
        format!(
            "a stack here is declared on {pinned}, and this checkout now configures {configured} — a declaration outranks a later config edit"
        ),
        format!("until it is retired, Ulak's own commands here keep addressing {pinned}"),
        format!("if {pinned} is gone for good: {}", forget_command(pinned)),
    ]
}

/// The retirement step an SSH failure owes when a declaration made on this
/// machine is what sent the command to that server.
///
/// `ssh.rs` holds a destination string and nothing else, so it cannot know
/// that the host it failed to reach is one a declaration pins rather than
/// one the user typed today. Without this, its `exit 255` offers "check the
/// connection by hand" and "add it to ~/.ssh/config" — both dead ends for a
/// server that has been destroyed, which is exactly the pair the reported
/// bug ended on.
pub(crate) fn retirement_step(destination: &str) -> Option<String> {
    intent::catalog()
        .iter()
        .any(|(_, desired)| desired.destination == destination)
        .then(|| {
            format!(
                "if {destination} is gone for good, retire what was declared on it: {}",
                forget_command(destination)
            )
        })
}

/// The exact command that retires a declaration whose server is gone.
pub(crate) fn forget_command(destination: &str) -> String {
    format!("ulak clean --forget-destination {}", sh_quote(destination))
}

/// The same command, spelled so it reaches a server the config no longer
/// names. `render` emits only Compose globals — never a host — so a pasted
/// command binds to whatever the config says now, which for a drifted
/// checkout is a DIFFERENT Docker stack than the one the line describes.
///
/// `env VAR=… cmd` rather than the bare `VAR=… cmd`: Ulak ships fish
/// completions, and fish rejects the bare form outright, which would turn
/// every aimed line into a dead end there. `env` reads the same in sh,
/// bash, zsh and fish.
pub(crate) fn aimed_at(destination: &str, command: &str) -> String {
    match config::env_var_for("host") {
        Some(var) => format!("env {var}={} {command}", sh_quote(destination)),
        None => command.to_string(),
    }
}

/// Aim one rendered line when this project's config has drifted from its
/// pin. `Unconfigured` counts: the config names nothing, so an unaimed
/// paste would refuse with "no server is configured" and offer to edit a
/// file — one more dead end for the checkout this exists for.
fn aim_for(project: &Project, line: String) -> String {
    match project.destination() {
        Ok(destination) if destination.drift.is_some() => aimed_at(&destination.dest, &line),
        _ => line,
    }
}

/// What this directory's bare root commands would address, for a report
/// that must not select among them.
pub(crate) fn declared_here() -> Vec<(String, String)> {
    let Ok(cwd) = std::env::current_dir().and_then(|cwd| cwd.canonicalize()) else {
        return Vec::new();
    };
    let boundary = config::workspace_home(&cwd);
    let mut out: Vec<(String, String)> = declarations_in_scope(&cwd, boundary.as_deref())
        .into_iter()
        .map(|desired| (desired.identity, desired.destination))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Every declaration made from this directory or a directory it scopes,
/// WITHOUT rebuilding its Compose invocation.
///
/// `declared_candidates` rebuilds each declaration through `Invocation`,
/// which is right for a command about to act on a stack — and wrong for the
/// one command whose whole point is a server that is gone: by then the old
/// `-f` file may be gone too, and a declaration that cannot rebuild was
/// silently dropped, so the offline escape answered "nothing declared" and
/// offered itself again. Only the two fields a retirement needs are read,
/// and both are checked before anything is done with them: the workspace
/// id names a lock file, so it must be the hex token `WorkspaceKey` mints
/// and nothing that could be a path; the destination must be one `config`
/// would have accepted.
fn declarations_in_scope(cwd: &Path, boundary: Option<&Path>) -> Vec<intent::Desired> {
    intent::catalog()
        .into_iter()
        .map(|(_, desired)| desired)
        .filter(|desired| {
            !desired.workspace_id.is_empty()
                && desired.workspace_id.chars().all(|c| c.is_ascii_hexdigit())
                && config::validate_ssh_dest(&desired.destination).is_ok()
                && in_scope(cwd, boundary, Path::new(&desired.cwd))
        })
        .collect()
}

/// Whether a declaration made from `declared_from` belongs to a command
/// typed in `cwd`: inside the same Ulak workspace, and in scope by
/// `scope_of`.
fn in_scope(cwd: &Path, boundary: Option<&Path>, declared_from: &Path) -> bool {
    (boundary.is_none() || config::workspace_home(declared_from).as_deref() == boundary)
        && scope_of(cwd, boundary, declared_from).is_some()
}

/// The sync workspaces whose declarations on `destination` a root command
/// typed here may retire.
///
/// Selection by DESTINATION, deliberately, and never by today's config: the
/// whole point is to reach a declaration whose server no config layer names
/// any more. Nor by Docker discovery — a discovered project answers "where
/// does my work go", and the question here is "what did this checkout leave
/// on that server". Nor through a rebuilt invocation — see
/// `declarations_in_scope`.
///
/// Several, when one directory declared the same server through different
/// first `-f` files, since the sync workspace follows that file. A
/// retirement by destination owes them all in one go: refusing and pointing
/// at "the checkout that declared it" pointed straight back at this very
/// directory, which is a dead end wearing a next step.
pub(crate) fn declared_on(destination: &str) -> Result<Vec<String>> {
    let cwd = current_dir()?;
    let boundary = config::workspace_home(&cwd);
    let declared = declarations_in_scope(&cwd, boundary.as_deref());
    // No best-score filter: `retire_declarations` is keyed by workspace and
    // destination, so every match in one workspace asks for the same
    // removal. Keeping only the deepest scope would leave a second
    // declaration on the same dead server behind, under a receipt that said
    // everything was retired.
    let mut workspaces: Vec<String> = declared
        .iter()
        .filter(|desired| desired.destination == destination)
        .map(|desired| desired.workspace_id.clone())
        .collect();
    if workspaces.is_empty() {
        return Err(nothing_declared_on(destination, &cwd, &declared));
    }
    // Sorted, so two overlapping forgets take their locks in one order.
    workspaces.sort();
    workspaces.dedup();
    Ok(workspaces)
}

/// Nothing here was declared on that server — say what WAS, so a mistyped
/// host is not handed another dead end.
fn nothing_declared_on(
    destination: &str,
    cwd: &Path,
    declared: &[intent::Desired],
) -> anyhow::Error {
    let mut known: Vec<&str> = declared
        .iter()
        .map(|desired| desired.destination.as_str())
        .collect();
    known.sort_unstable();
    known.dedup();
    let mut error = fail!(
        "nothing declared under {} was declared on {destination}",
        cwd.display()
    );
    if known.is_empty() {
        error = error
            .now("this checkout has declared no stack at all, so there is nothing to retire")
            .now("a workspace left on a server that still answers is removed with: ulak clean");
    } else {
        error = error.now(format!("declared here: {}", known.join(", ")));
        for dest in &known {
            error = error.now(format!(
                "retire one of those instead: {}",
                forget_command(dest)
            ));
        }
    }
    // The ssh failure that pointed here reads the whole machine's catalog,
    // so the declaration may belong to another checkout: name the directory
    // it was made from, or the hint would end in this refusal.
    let mut elsewhere: Vec<String> = intent::catalog()
        .into_iter()
        .filter(|(_, desired)| desired.destination == destination)
        .map(|(_, desired)| desired.cwd)
        .collect();
    elsewhere.sort();
    elsewhere.dedup();
    for declared_from in elsewhere {
        error = error.now(format!(
            "or from the checkout that declared on it: (cd {} && {})",
            sh_quote(&declared_from),
            forget_command(destination)
        ));
    }
    error.into_err()
}

fn declared_candidates(cwd: &Path, boundary: Option<&Path>) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (id, desired) in intent::catalog() {
        let Ok(rebuilt) = desired.rebuild_for_stack(&id) else {
            continue;
        };
        if let Some(boundary) = boundary
            && config::workspace_home(&rebuilt.inv.project_dir).as_deref() != Some(boundary)
        {
            continue;
        }
        let Some(score) = scope_score(cwd, boundary, &rebuilt.inv) else {
            continue;
        };
        out.push(Candidate {
            stack_id: id,
            desired,
            rebuilt,
            score,
        });
    }
    out
}

/// More deeply nested declaration scopes win. `Invocation::cwd` owns the
/// scope because that is where the declaration was made; `project_dir` is
/// only Compose's relative-path base and may be a broad shared ancestor.
/// A command above a declaration but still inside its Ulak workspace may use
/// a sole descendant; a child may never drift sideways into a sibling.
fn scope_score(cwd: &Path, boundary: Option<&Path>, inv: &Invocation) -> Option<usize> {
    scope_of(cwd, boundary, &inv.cwd)
}

/// The same rule over the directory a declaration was made from, for the
/// one caller that has no rebuilt invocation to read it from.
fn scope_of(cwd: &Path, boundary: Option<&Path>, declared_from: &Path) -> Option<usize> {
    if cwd.starts_with(declared_from) {
        return Some(declared_from.components().count());
    }
    let boundary = boundary?;
    (cwd.starts_with(boundary) && declared_from.starts_with(cwd)).then_some(0)
}

/// Turn the chosen declaration into the project a root command runs on.
/// Every way of choosing one ends here, so the drift warning speaks once.
fn adopt(desired: &Desired, rebuilt: Rebuilt) -> Result<Project> {
    let project = desired.project_from_rebuilt(rebuilt)?;
    warn_on_destination_drift(&project);
    activate(&project);
    Ok(project)
}

fn current_dir() -> Result<PathBuf> {
    std::env::current_dir()
        .context("cannot read current directory")?
        .canonicalize()
        .context("cannot canonicalize current directory")
}

// ─── a stack by name ─────────────────────────────────────────────────

/// The declaration a typed stack name selects, from any directory.
fn named(
    globals: &[String],
    command: &str,
    command_args: &[String],
    spec: &str,
) -> Result<Project> {
    if !globals.is_empty() {
        let mut by_files = vec!["ulak".to_string()];
        by_files.extend_from_slice(globals);
        by_files.extend(command_line(command, command_args));
        return Err(fail!(
            "`{spec}` and `{}` both choose the project",
            shell_join(globals)
        )
        .now(format!(
            "by its name: {}",
            named_command(command, command_args, spec)
        ))
        .now(format!("or by its files: {}", shell_join(&by_files)))
        .into_err());
    }
    let cwd = current_dir()?;
    let boundary = config::workspace_home(&cwd);
    let catalog = intent::catalog();
    let (stack_id, desired) = resolve(
        &catalog,
        spec,
        &cwd,
        boundary.as_deref(),
        command,
        command_args,
    )?;
    let rebuilt = desired
        .rebuild_for_stack(stack_id)
        .map_err(|error| unreadable(desired, error, command, command_args))?;
    adopt(desired, rebuilt)
}

/// A named stack whose checkout cannot be read any more. The rebuild's own
/// reason names a path relative to a directory the user never typed, or
/// none at all, and the fallback step was `ulak doctor`, which meets the
/// same wall. Say which stack and where it was declared from, and keep
/// whatever steps the reason did have.
///
/// A moved checkout is a new workspace. A live stack is declared again from
/// there, so the service follows it; an idle one is only run from there.
/// `clean` gets neither: from the new place it would address the new,
/// empty workspace and report the old one gone.
fn unreadable(
    desired: &Desired,
    error: anyhow::Error,
    command: &str,
    command_args: &[String],
) -> anyhow::Error {
    let name = &desired.identity;
    let from = &desired.cwd;
    let mut steps = Vec::new();
    let mut wrapped = if !Path::new(from).is_dir() {
        if desired.live {
            steps.push(format!(
                "if the checkout moved, declare it from there: cd into it, then ulak docker compose -p {} up -d",
                sh_quote(name)
            ));
        } else if command != "clean" {
            let mut words = vec!["ulak".to_string()];
            words.extend(command_line(command, command_args));
            steps.push(format!(
                "if the checkout moved, cd into it, then: {}",
                shell_join(&words)
            ));
        }
        fail!("{name} was declared from {from}, which no longer exists")
    } else if let Some(reason) = error.downcast_ref::<ui::Fail>() {
        steps.clone_from(&reason.now);
        fail!("{name} was declared from {from}, but {}", reason.msg)
    } else {
        fail!("{name} was declared from {from}, but {error:#}")
    };
    steps.push("every stack on this machine: ulak status".to_string());
    for step in steps {
        wrapped = wrapped.now(step);
    }
    wrapped.into_err()
}

/// Read a typed `name` or `name@destination` back against the catalog, by
/// the rule `selector_in` writes it with.
///
/// Split at the FIRST `@`: a Compose project name cannot contain one, and a
/// destination such as `deploy@10.0.0.1` may. A name two servers share is
/// settled by the checkout it is typed in, when exactly one of them was
/// declared there; anywhere else it needs its server.
fn resolve<'a>(
    catalog: &'a [(String, Desired)],
    spec: &str,
    cwd: &Path,
    boundary: Option<&Path>,
    command: &str,
    command_args: &[String],
) -> Result<(&'a str, &'a Desired)> {
    let (name, server) = match spec.split_once('@') {
        Some((name, server)) => (name, Some(server)),
        None => (spec, None),
    };
    if name.is_empty() || server.is_some_and(str::is_empty) {
        return Err(fail!("`{spec}` is not a stack name")
            .now(format!(
                "name a stack by its Compose project name: {}",
                named_command(command, command_args, "my-app")
            ))
            .now(format!(
                "and add its server when two servers share the name: {}",
                named_command(command, command_args, "my-app@my-server")
            ))
            .into_err());
    }
    let same_name: Vec<&(String, Desired)> = catalog
        .iter()
        .filter(|(_, desired)| desired.identity == name)
        .collect();
    let matching: Vec<&(String, Desired)> = same_name
        .iter()
        .copied()
        .filter(|(_, desired)| server.is_none_or(|server| desired.destination == server))
        .collect();
    let chosen = match matching.as_slice() {
        [] => None,
        [one] => Some(*one),
        shared => {
            let here: Vec<&(String, Desired)> = shared
                .iter()
                .copied()
                .filter(|(_, desired)| in_scope(cwd, boundary, Path::new(&desired.cwd)))
                .collect();
            match here.as_slice() {
                [one] => Some(*one),
                _ => {
                    let mut error = fail!("{name} is declared on {} servers", shared.len());
                    for (_, desired) in shared {
                        error = error.now(named_command(
                            command,
                            command_args,
                            &format!("{name}@{}", desired.destination),
                        ));
                    }
                    return Err(error.into_err());
                }
            }
        }
    };
    if let Some((stack_id, desired)) = chosen {
        return Ok((stack_id.as_str(), desired));
    }
    if let Some(server) = server.filter(|_| !same_name.is_empty()) {
        let mut error = fail!("{name} is not declared on {server}");
        for (_, desired) in &same_name {
            error = error.now(format!(
                "it is on {}: {}",
                desired.destination,
                named_command(command, command_args, &selector_in(catalog, desired))
            ));
        }
        return Err(error.into_err());
    }
    Err(unknown(catalog, name, cwd, boundary, command, command_args))
}

/// No declaration has that name: offer the near miss, then what exists.
fn unknown(
    catalog: &[(String, Desired)],
    name: &str,
    cwd: &Path,
    boundary: Option<&Path>,
    command: &str,
    command_args: &[String],
) -> anyhow::Error {
    let mut error = fail!("no stack named {name} is declared on this machine");
    let mut declared: Vec<&str> = catalog
        .iter()
        .map(|(_, desired)| desired.identity.as_str())
        .collect();
    declared.sort_unstable();
    declared.dedup();
    if let Some(close) = closest(name, &declared) {
        for (at, selector) in spelled(catalog, close, cwd, boundary).iter().enumerate() {
            let lead = if at == 0 { "did you mean" } else { "or" };
            error = error.now(format!(
                "{lead}: {}",
                named_command(command, command_args, selector)
            ));
        }
    }
    let names = |here: bool| {
        let mut names: Vec<String> = catalog
            .iter()
            .filter(|(_, desired)| !here || in_scope(cwd, boundary, Path::new(&desired.cwd)))
            .map(|(_, desired)| selector_in(catalog, desired))
            .collect();
        names.sort();
        names.dedup();
        names
    };
    let here = names(true);
    if !here.is_empty() {
        error = error.now(format!("declared here: {}", some_of(&here)));
    } else if !catalog.is_empty() {
        error = error.now(format!(
            "declared on this machine: {}",
            some_of(&names(false))
        ));
    } else {
        error = error.now(
            "nothing is declared on this machine yet: ulak docker compose up -d declares a stack",
        );
    }
    error.into_err()
}

/// The one declared name `typed` most likely meant: the only name it
/// begins, else the only name nearest it within a few edits. Over NAMES,
/// not declarations: a name two servers share is one answer, and judged
/// per declaration it tied with itself and suggested nothing.
fn closest<'a>(typed: &str, names: &[&'a str]) -> Option<&'a str> {
    let mut prefixed = names.iter().filter(|name| name.starts_with(typed));
    if let (Some(only), None) = (prefixed.next(), prefixed.next()) {
        return Some(only);
    }
    let limit = 2.max(typed.chars().count() / 3);
    let scored: Vec<(usize, &str)> = names
        .iter()
        .map(|name| (edits(typed, name), *name))
        .filter(|(distance, _)| *distance <= limit)
        .collect();
    let best = scored.iter().map(|(distance, _)| *distance).min()?;
    let mut nearest = scored.iter().filter(|(distance, _)| *distance == best);
    match (nearest.next(), nearest.next()) {
        (Some((_, only)), None) => Some(only),
        _ => None,
    }
}

/// How to type a declared `name` so `resolve` selects it from here: the
/// name alone when it is one declaration, or the one declared in this
/// checkout; once per server otherwise.
fn spelled(
    catalog: &[(String, Desired)],
    name: &str,
    cwd: &Path,
    boundary: Option<&Path>,
) -> Vec<String> {
    let servers: Vec<&Desired> = catalog
        .iter()
        .map(|(_, desired)| desired)
        .filter(|desired| desired.identity == name)
        .collect();
    let here = servers
        .iter()
        .filter(|desired| in_scope(cwd, boundary, Path::new(&desired.cwd)))
        .count();
    if servers.len() == 1 || here == 1 {
        return vec![name.to_string()];
    }
    servers
        .iter()
        .map(|desired| format!("{name}@{}", desired.destination))
        .collect()
}

/// Levenshtein distance, over chars.
fn edits(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(ca != *cb));
            diagonal = above;
        }
    }
    row[b.len()]
}

/// A list a refusal can print on one line, however many stacks there are.
fn some_of(names: &[String]) -> String {
    const SHOWN: usize = 8;
    if names.len() <= SHOWN {
        return names.join(", ");
    }
    format!(
        "{}, and {} more",
        names[..SHOWN].join(", "),
        names.len() - SHOWN
    )
}

// ─── several declarations here ───────────────────────────────────────

/// Every declaration that answers this directory equally well.
pub struct Several {
    cwd: PathBuf,
    /// Sorted by name, then server, so every listing reads the same.
    candidates: Vec<Candidate>,
    /// Read once, so every selector is spelled against the same catalog.
    catalog: Vec<(String, Desired)>,
}

/// One of them, as a listing shows it.
pub struct Listed<'a> {
    pub stack_id: &'a str,
    pub desired: &'a Desired,
    /// What to type after a root command to reach it.
    pub selector: String,
    /// Its Compose files, relative to this directory when below it.
    pub files: String,
    rebuilt: &'a Rebuilt,
}

impl Listed<'_> {
    /// This stack's whole invocation in full, for `--raw`.
    pub fn in_full(&self, tail: &[&str]) -> String {
        in_full(self.desired, self.rebuilt, tail)
    }

    /// The root command that runs `command` on this stack.
    fn spelled(&self, spelling: Spelling, command: &str, command_args: &[String]) -> String {
        match spelling {
            Spelling::Short => named_command(command, command_args, &self.selector),
            Spelling::Raw => {
                let line = command_line(command, command_args);
                let tail: Vec<&str> = line.iter().map(String::as_str).collect();
                self.in_full(&tail)
            }
        }
    }
}

/// A declaration's whole invocation in full, for `--raw`. Through the
/// pinned project, so `render_project` aims it exactly as it would a
/// chosen declaration: explicit globals select by config, and unaimed, two
/// choices on different servers would paste into the same stack.
pub(crate) fn in_full(desired: &Desired, rebuilt: &Rebuilt, tail: &[&str]) -> String {
    let copy = Rebuilt {
        inv: rebuilt.inv.clone(),
        workspace_key: rebuilt.workspace_key.clone(),
    };
    match desired.project_from_rebuilt(copy) {
        Ok(project) => render_project(&["ulak"], &project, &words(tail)),
        Err(_) => render(
            &["ulak"],
            &rebuilt.inv,
            Some(&desired.identity),
            &words(tail),
        ),
    }
}

impl Several {
    fn new(cwd: PathBuf, mut candidates: Vec<Candidate>) -> Several {
        candidates.sort_by(|a, b| {
            (&a.desired.identity, &a.desired.destination)
                .cmp(&(&b.desired.identity, &b.desired.destination))
        });
        Several {
            cwd,
            candidates,
            catalog: intent::catalog(),
        }
    }

    pub fn listed(&self) -> Vec<Listed<'_>> {
        self.candidates
            .iter()
            .map(|candidate| Listed {
                stack_id: &candidate.stack_id,
                desired: &candidate.desired,
                selector: selector_in(&self.catalog, &candidate.desired),
                files: files_shown(&self.cwd, &candidate.rebuilt.inv),
                rebuilt: &candidate.rebuilt,
            })
            .collect()
    }

    /// Settle on one stack the way `how` allows, or refuse with each
    /// choice spelled as its named command.
    pub fn choose(
        mut self,
        command: &str,
        command_args: &[String],
        how: Choose,
    ) -> Result<Project> {
        if how == Choose::Ask {
            let labels = pick_labels(&self.listed());
            if let Some(index) = ui::pick(&format!("Which stack {}?", the_one(command)), &labels)?
                && index < self.candidates.len()
            {
                let skip = self.listed()[index].spelled(spelling(), command, command_args);
                let candidate = self.candidates.swap_remove(index);
                let project = adopt(&candidate.desired, candidate.rebuilt)?;
                ui::dim(&format!("skip this question next time: {skip}"));
                return Ok(project);
            }
        }
        Err(self.refusal(spelling(), command, command_args))
    }

    /// Each choice as its own command. `--raw` prints the old full lines,
    /// labelled by a shell comment so each still pastes as it stands.
    fn refusal(&self, spelling: Spelling, command: &str, command_args: &[String]) -> anyhow::Error {
        let mut error = fail!(
            "{} stacks are declared here — name the one {}",
            self.candidates.len(),
            the_one(command)
        );
        for listed in self.listed() {
            let line = listed.spelled(spelling, command, command_args);
            error = error.now(match spelling {
                Spelling::Short => line,
                Spelling::Raw => format!(
                    "{line}  # {} on {}",
                    listed.desired.identity, listed.desired.destination
                ),
            });
        }
        let state = match spelling {
            Spelling::Short => "each one's state: ulak status",
            Spelling::Raw => "each one's state: ulak status --raw",
        };
        error.now(state).into_err()
    }

    /// Where `init` writes when every stack here shares one settings home.
    pub fn config_home(&self) -> Result<PathBuf> {
        let mut homes = Vec::new();
        for candidate in &self.candidates {
            let rebuilt = Rebuilt {
                inv: candidate.rebuilt.inv.clone(),
                workspace_key: candidate.rebuilt.workspace_key.clone(),
            };
            homes.push(candidate.desired.project_from_rebuilt(rebuilt)?.config_home);
        }
        homes.sort();
        homes.dedup();
        match homes.as_slice() {
            [home] => Ok(home.clone()),
            _ => {
                let mut error = fail!(
                    "the {} stacks declared here keep their settings in different directories",
                    self.candidates.len()
                );
                for home in &homes {
                    error = error.now(format!(
                        "initialize one: cd {} && ulak init",
                        sh_quote(&home.display().to_string())
                    ));
                }
                Err(error.into_err())
            }
        }
    }
}

/// What a command does with the one stack it needs, completing both
/// "Which stack …?" and "name the one …".
fn the_one(command: &str) -> &'static str {
    match command {
        "doctor" => "to check",
        "sync" => "to sync",
        "clean" => "whose workspace to remove",
        _ => "to use",
    }
}

/// The picker's rows: name, state, server and files, in aligned columns.
fn pick_labels(listed: &[Listed<'_>]) -> Vec<String> {
    let name = listed
        .iter()
        .map(|l| l.selector.chars().count())
        .max()
        .unwrap_or(0);
    let server = listed
        .iter()
        .map(|l| l.desired.destination.chars().count())
        .max()
        .unwrap_or(0);
    listed
        .iter()
        .map(|l| {
            let state = if l.desired.live { "live" } else { "idle" };
            format!(
                "{:<name$}   {state}   {:<server$}   {}",
                l.selector, l.desired.destination, l.files
            )
            .trim_end()
            .to_string()
        })
        .collect()
}

/// A declaration's Compose files as someone standing in `cwd` would name
/// them.
fn files_shown(cwd: &Path, inv: &Invocation) -> String {
    inv.compose_files
        .iter()
        .map(|file| file.strip_prefix(cwd).unwrap_or(file).display().to_string())
        .collect::<Vec<_>>()
        .join(" + ")
}

fn workspace_or_outside(boundary: Option<PathBuf>) -> Result<Context> {
    match boundary {
        Some(_) => Workspace::locate().map(|workspace| Context::Workspace(Box::new(workspace))),
        None => Ok(Context::Outside),
    }
}

fn activate(project: &Project) {
    crate::audit::set_context(project.workspace_id());
}

fn explicit_example(command: &str, command_args: &[String]) -> String {
    let mut words = vec!["ulak".into(), "-f".into(), "compose.dev.yaml".into()];
    words.extend(command_line(command, command_args));
    shell_join(&words)
}

fn command_line(command: &str, command_args: &[String]) -> Vec<String> {
    let mut tail = Vec::with_capacity(command_args.len() + 1);
    tail.push(command.to_string());
    tail.extend_from_slice(command_args);
    tail
}

/// Every pasteable command rendered from a project, and therefore the one
/// site that aims it — see `aim_for`.
fn render_project(prefix: &[&str], project: &Project, tail: &[String]) -> String {
    aim_for(
        project,
        render(
            prefix,
            &project.inv,
            project.compose_model_project_name(),
            tail,
        ),
    )
}

fn render(prefix: &[&str], inv: &Invocation, identity: Option<&str>, tail: &[String]) -> String {
    let mut words: Vec<String> = prefix.iter().map(|word| (*word).to_string()).collect();
    words.extend(exact_globals(inv, identity));
    words.extend_from_slice(tail);
    shell_join(&words)
}

/// Every global, absolute: the spelling that reads the same from any
/// directory.
fn exact_globals(inv: &Invocation, identity: Option<&str>) -> Vec<String> {
    let mut exact = inv.clone();
    if let Some(identity) = identity {
        exact.project_name = Some(identity.to_string());
    }
    exact.globals_argv()
}

/// Whether a rendered command may carry no globals at all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bare {
    /// `ulak docker compose …`: argv, environment and discovery are the
    /// whole of Docker's answer.
    Allowed,
    /// `ulak <root command>`: without globals, the declarations are
    /// consulted before discovery.
    Refused,
}

/// `render`, spelled for the shell that ran this command; see
/// `here_globals_in`.
fn render_here(
    prefix: &[&str],
    project: &Project,
    identity: Option<&str>,
    tail: &[String],
    bare: Bare,
) -> String {
    let globals = match std::env::current_dir() {
        Ok(cwd) => here_globals_in(
            &cwd,
            std::env::vars().collect(),
            &project.inv,
            identity,
            bare,
        ),
        Err(_) => exact_globals(&project.inv, identity),
    };
    let mut words: Vec<String> = prefix.iter().map(|word| (*word).to_string()).collect();
    words.extend(globals);
    words.extend_from_slice(tail);
    aim_for(project, shell_join(&words))
}

/// The shortest globals that `Invocation::capture_in` reads back, from
/// `cwd` and `env`, to the Compose files, project directory, env files,
/// profiles and project name of `inv`. Measured rather than reasoned: each
/// candidate is captured exactly as the pasted line will be, and the full
/// absolute spelling remains the answer when no shorter one agrees.
///
/// `-p` is kept whenever the identity is known. Dropping it could hand the
/// name to a `name:` inside a Compose file, which this crate never opens.
///
/// Only for a line printed into the shell that ran this command, which is
/// the one place `cwd` and `env` are the reader's. At the filesystem root,
/// where the service runs, "relative" would mean every path, so the
/// absolute spelling is used there unconditionally.
fn here_globals_in(
    cwd: &Path,
    env: std::collections::BTreeMap<String, String>,
    inv: &Invocation,
    identity: Option<&str>,
    bare: Bare,
) -> Vec<String> {
    let exact = exact_globals(inv, identity);
    let Ok(cwd) = cwd.canonicalize() else {
        return exact;
    };
    if cwd.parent().is_none() {
        return exact;
    }
    let shown = |path: &Path| match path.strip_prefix(&cwd) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    };
    let named: Vec<String> = identity
        .map(|identity| vec!["-p".to_string(), identity.to_string()])
        .unwrap_or_default();
    let mut spelled = Vec::new();
    for file in &inv.compose_files {
        spelled.extend(["-f".to_string(), shown(file)]);
    }
    spelled.extend(named.iter().cloned());
    for profile in &inv.profiles {
        spelled.extend(["--profile".to_string(), profile.clone()]);
    }
    for env_file in &inv.env_files {
        spelled.extend(["--env-file".to_string(), shown(env_file)]);
    }
    let mut with_dir = spelled.clone();
    with_dir.extend(["--project-directory".to_string(), shown(&inv.project_dir)]);

    let reads_back = |argv: &[String]| {
        Invocation::capture_in(&cwd, argv, env.clone()).is_ok_and(|got| {
            got.compose_files == inv.compose_files
                && got.project_dir == inv.project_dir
                && got.env_files == inv.env_files
                && got.profiles == inv.profiles
                && got.project_name.as_deref() == identity
        })
    };
    [named, spelled, with_dir]
        .into_iter()
        .filter(|argv| bare == Bare::Allowed || !argv.is_empty())
        .find(|argv| reads_back(argv))
        .unwrap_or(exact)
}

/// A rendered command for a caller that RUNS it instead of printing it:
/// `ulak ui` starts `ulak … docker compose up -d` itself. `words[0]` is
/// `ulak`; `env` is the host variable `aim_for` would prefix.
pub(crate) struct Spawn {
    pub(crate) env: Option<(&'static str, String)>,
    pub(crate) words: Vec<String>,
}

/// The spawnable twin of `compose_command`. Same inputs as
/// `render_project` — every selector, the historical identity, the aim —
/// so a command the dashboard runs and the one it would tell you to paste
/// cannot address different stacks. A test pins the two together.
pub(crate) fn compose_spawn(project: &Project, tail: &[&str]) -> Spawn {
    spawn_for(&["ulak", "docker", "compose"], project, tail)
}

/// The spawnable twin of `root_command`'s `--raw` spelling: a name would
/// have to be looked up again in the catalog, these words already aim.
pub(crate) fn root_spawn(project: &Project, tail: &[&str]) -> Spawn {
    spawn_for(&["ulak"], project, tail)
}

fn spawn_for(prefix: &[&str], project: &Project, tail: &[&str]) -> Spawn {
    let mut words: Vec<String> = prefix.iter().map(|word| (*word).to_string()).collect();
    let mut exact = project.inv.clone();
    if let Some(identity) = project.compose_model_project_name() {
        exact.project_name = Some(identity.to_string());
    }
    words.extend(exact.globals_argv());
    words.extend(tail.iter().map(|word| (*word).to_string()));
    let env = match project.destination() {
        Ok(destination) if destination.drift.is_some() => {
            config::env_var_for("host").map(|var| (var, destination.dest))
        }
        _ => None,
    };
    Spawn { env, words }
}

fn shell_join(words: &[String]) -> String {
    words
        .iter()
        .enumerate()
        .map(|(at, word)| match word.split_once('=') {
            // After the program name an `=` is an ordinary byte, and quoting
            // the whole word printed `'--max-delete=3'`, which reads like a
            // mistake. Quoting each side keeps the bytes; the first word is
            // never split, because there `A=b` would be an assignment.
            Some((flag, value)) if at > 0 && !flag.is_empty() => {
                format!("{}={}", sh_quote(flag), sh_quote(value))
            }
            _ => sh_quote(word),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv(cwd: &str, project: &str) -> Invocation {
        Invocation::workspace(PathBuf::from(cwd), PathBuf::from(project))
    }

    /// A declaration made at the project root remains the answer below it,
    /// and a more deeply nested declaration outranks the broad one.
    #[test]
    fn the_most_specific_declared_scope_wins() {
        let broad = inv("/repo", "/repo");
        let nested = inv("/repo/app", "/repo/app");
        assert_eq!(
            scope_score(Path::new("/repo/app/src"), Some(Path::new("/repo")), &broad),
            Some(2)
        );
        assert_eq!(
            scope_score(
                Path::new("/repo/app/src"),
                Some(Path::new("/repo")),
                &nested
            ),
            Some(3)
        );
    }

    /// Sharing an Ulak config does not let a declaration in one child catch
    /// a command typed in its sibling. The Compose project directory may be
    /// their parent because the first `-f` lives there; it is a path base,
    /// not evidence that the declaration was made for every child.
    #[test]
    fn a_sibling_is_not_a_declared_scope() {
        let app = inv("/repo/app", "/repo");
        assert_eq!(
            scope_score(Path::new("/repo/other"), Some(Path::new("/repo")), &app),
            None
        );
        assert_eq!(
            scope_score(Path::new("/repo"), Some(Path::new("/repo")), &app),
            Some(0)
        );
    }

    /// The escape must be spelled where the user is stuck, and spelled the
    /// same everywhere: a host with a space or a quote reaches the shell
    /// intact, and the last line is the one command that ends the wedge.
    #[test]
    fn the_drift_warning_names_both_servers_and_ends_with_the_retirement() {
        let lines = drift_lines("dead server", "new-server");
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].contains("dead server") && lines[0].contains("new-server"),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("dead server"), "{}", lines[1]);
        assert_eq!(
            lines[2],
            "if dead server is gone for good: ulak clean --forget-destination 'dead server'"
        );
    }

    /// A pasted command re-binds to the config unless the server rides
    /// along. `ULAK_HOST` is the config table's own variable for `host`,
    /// read back from the table rather than spelled here twice, and `env`
    /// carries it into fish as well as sh.
    #[test]
    fn a_command_aimed_at_a_declared_server_carries_it_as_the_host_variable() {
        assert_eq!(
            aimed_at("old-server", "ulak -f a.yaml status"),
            "env ULAK_HOST=old-server ulak -f a.yaml status"
        );
        assert_eq!(
            aimed_at("deploy@10.0.0.1", "ulak clean"),
            "env ULAK_HOST=deploy@10.0.0.1 ulak clean"
        );
        assert_eq!(
            aimed_at("it's", "ulak status"),
            "env ULAK_HOST='it'\\''s' ulak status"
        );
    }

    /// Every rendered command from a pinned project carries the declared
    /// server once the config has moved — root and Compose forms alike —
    /// and none does while the two agree. One site renders them all, so
    /// one test reads them all.
    ///
    /// A pinned project WITHOUT a declaration on disk, deliberately: with
    /// one, root commands are spelled by name and need no aiming (see
    /// `a_root_command_for_a_declared_stack_is_its_name_with_every_argument`),
    /// so the identity is unique to this test.
    #[test]
    fn every_command_rendered_from_a_drifted_project_is_aimed_and_none_otherwise() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::write(dir.join("ulak.toml"), "host = \"new-server\"\n").unwrap();
        std::fs::write(dir.join("compose.yaml"), "services: {}\n").unwrap();
        let inv = Invocation::capture_in(&dir, &[], std::collections::BTreeMap::new()).unwrap();
        let mut project = Project::locate_from(inv).unwrap();
        let api = format!("undeclared-{}", std::process::id());
        let api = api.as_str();

        project.pin_existing_stack("new-server", api);
        for line in [
            root_command(&project, &["clean"]),
            compose_command(&project, &["down"]),
            compose_command_here_as(&project, api, &["up", "-d"]),
        ] {
            assert!(line.starts_with("ulak "), "no drift, no prefix: {line}");
        }

        project.pin_existing_stack("dead-server", api);
        for line in [
            root_command(&project, &["clean"]),
            compose_command(&project, &["down"]),
            compose_command_here_as(&project, api, &["up", "-d"]),
        ] {
            assert!(
                line.starts_with("env ULAK_HOST=dead-server ulak "),
                "a drifted project's paste must carry its server: {line}"
            );
            assert!(line.contains(&format!("-p {api}")), "{line}");
        }
    }

    /// What the dashboard runs is what it would tell you to paste: the
    /// same words, and the same host aim once the config has drifted.
    #[test]
    fn a_spawned_command_is_the_pasteable_one_word_for_word() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::write(dir.join("ulak.toml"), "host = \"new-server\"\n").unwrap();
        std::fs::write(dir.join("compose.yaml"), "services: {}\n").unwrap();
        let inv = Invocation::capture_in(&dir, &[], std::collections::BTreeMap::new()).unwrap();
        let mut project = Project::locate_from(inv).unwrap();
        let joined = |spawn: Spawn| {
            let line = shell_join(&spawn.words);
            match spawn.env {
                Some((var, dest)) => format!("env {var}={} {line}", sh_quote(&dest)),
                None => line,
            }
        };
        for pin in ["new-server", "dead server"] {
            project.pin_existing_stack(pin, "api");
            assert_eq!(
                joined(compose_spawn(&project, &["up", "-d"])),
                compose_command(&project, &["up", "-d"]),
                "pinned to {pin}"
            );
            assert_eq!(
                joined(root_spawn(&project, &["sync"])),
                root_command_in(Spelling::Raw, &project, &["sync"]),
                "pinned to {pin}"
            );
        }
    }

    /// The offline escape must find a declaration whose Compose file is
    /// gone — that is the checkout it exists for — and must still refuse a
    /// sibling's, a foreign workspace id that could name a path, and a
    /// destination `config` would never have accepted.
    #[test]
    fn a_declaration_whose_compose_file_is_gone_is_still_in_scope_to_retire() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("ulak.toml"), "host = \"new-server\"\n").unwrap();
        let checkout = root.join("app");
        std::fs::create_dir_all(&checkout).unwrap();
        let tag = std::process::id();
        let declare = |cwd: &Path, workspace_id: &str, destination: &str| {
            let desired = intent::Desired {
                schema: intent::SCHEMA,
                live: true,
                workspace_id: workspace_id.into(),
                workspace_namespace: "client-a".into(),
                destination: destination.into(),
                identity: format!("stack-{tag}"),
                cwd: cwd.display().to_string(),
                argv_globals: vec!["-f".into(), cwd.join("gone.yaml").display().to_string()],
                compose_env: std::collections::BTreeMap::new(),
                updated_unix: 1,
            };
            intent::write_desired(&desired.stack_id(), &desired).unwrap();
        };
        declare(&checkout, "abc123", &format!("dead-{tag}"));
        declare(&root.join("sibling"), "abc123", &format!("sibling-{tag}"));
        declare(&checkout, "../../escape", &format!("hostile-id-{tag}"));
        declare(&checkout, "abc123", "-oProxyCommand=x");

        let found: Vec<String> = declarations_in_scope(&checkout, Some(&root))
            .into_iter()
            .map(|d| d.destination)
            .filter(|d| d.contains(&tag.to_string()) || d.starts_with('-'))
            .collect();
        assert_eq!(
            found,
            vec![format!("dead-{tag}")],
            "only the checkout's own, well-formed declaration is in scope"
        );
    }

    /// The step an ssh failure offers exists only when a declaration on
    /// this machine names that server: offering it for a host the user
    /// typed today would send them to retire something that is not there.
    #[test]
    fn the_retirement_step_is_offered_only_for_a_declared_server() {
        let desired = intent::Desired {
            schema: intent::SCHEMA,
            live: true,
            workspace_id: "checkout-a".into(),
            workspace_namespace: "client-a".into(),
            destination: "dead-server".into(),
            identity: "api".into(),
            cwd: "/checkout/a".into(),
            argv_globals: Vec::new(),
            compose_env: std::collections::BTreeMap::new(),
            updated_unix: 1,
        };
        intent::write_desired(&desired.stack_id(), &desired).unwrap();

        let step = retirement_step("dead-server").expect("a declared server earns the step");
        assert!(
            step.contains("ulak clean --forget-destination dead-server"),
            "{step}"
        );
        assert_eq!(retirement_step("typed-today"), None);
    }

    /// Suggestions carry every resolved selector in shell-safe form. Leaving
    /// one out is the dead end this module exists to remove.
    #[test]
    fn an_exact_command_carries_the_whole_invocation() {
        let mut invocation = inv("/repo with space", "/repo with space");
        invocation.compose_files = vec![PathBuf::from("/repo with space/compose.dev.yaml")];
        invocation.project_name = Some("chosen".into());
        assert_eq!(
            render(&["ulak"], &invocation, None, &["status".into()]),
            "ulak -f '/repo with space/compose.dev.yaml' -p chosen --project-directory '/repo with space' status"
        );
        invocation.project_name = None;
        assert_eq!(
            render(
                &["ulak"],
                &invocation,
                Some("historical"),
                &["clean".into()]
            ),
            "ulak -f '/repo with space/compose.dev.yaml' -p historical --project-directory '/repo with space' clean"
        );
    }

    // ─── a stack by name, several here, and spelling for this shell ────

    fn declared(identity: &str, destination: &str, cwd: &str, live: bool) -> Desired {
        Desired {
            schema: intent::SCHEMA,
            live,
            workspace_id: "abc123".into(),
            workspace_namespace: "client-a".into(),
            destination: destination.into(),
            identity: identity.into(),
            cwd: cwd.into(),
            argv_globals: Vec::new(),
            compose_env: std::collections::BTreeMap::new(),
            updated_unix: 1,
        }
    }

    fn catalog_of(entries: &[Desired]) -> Vec<(String, Desired)> {
        entries.iter().map(|d| (d.stack_id(), d.clone())).collect()
    }

    /// The message and every "now" line, as a user reads them.
    fn said(error: &anyhow::Error) -> String {
        let fail = error
            .downcast_ref::<ui::Fail>()
            .expect("a refusal with next steps");
        format!("{}\n{}", fail.msg, fail.now.join("\n"))
    }

    fn resolved(catalog: &[(String, Desired)], spec: &str, cwd: &str) -> Result<String> {
        resolve(catalog, spec, Path::new(cwd), None, "status", &[])
            .map(|(_, desired)| format!("{}@{}", desired.identity, desired.destination))
    }

    /// The name is the whole selector when no other server shares it, and
    /// it reaches a stack declared from any directory.
    #[test]
    fn a_name_alone_selects_the_only_stack_of_that_name_from_anywhere() {
        let catalog = catalog_of(&[
            declared("shop", "devbox", "/code/shop", true),
            declared("shop-test", "devbox", "/code/shop", false),
            declared("blog", "other", "/code/blog", true),
        ]);
        assert_eq!(resolved(&catalog, "shop", "/home").unwrap(), "shop@devbox");
        assert_eq!(
            resolved(&catalog, "blog", "/code/shop").unwrap(),
            "blog@other"
        );
        assert_eq!(
            resolved(&catalog, "shop-test@devbox", "/").unwrap(),
            "shop-test@devbox"
        );
    }

    /// Two servers with a stack of one name: the checkout it is typed in
    /// settles it when exactly one was declared there; anywhere else both
    /// spelled choices are offered and neither is guessed.
    #[test]
    fn a_name_two_servers_share_is_settled_by_the_checkout_or_by_its_server() {
        let catalog = catalog_of(&[
            declared("shop", "old-server", "/work/shop", false),
            declared("shop", "new-server", "/play/shop", true),
        ]);
        assert_eq!(
            resolved(&catalog, "shop", "/work/shop/src").unwrap(),
            "shop@old-server"
        );
        assert_eq!(
            resolved(&catalog, "shop@new-server", "/work/shop").unwrap(),
            "shop@new-server"
        );

        let refused = said(&resolved(&catalog, "shop", "/elsewhere").unwrap_err());
        assert!(refused.contains("declared on 2 servers"), "{refused}");
        assert!(
            refused.contains("ulak status shop@old-server")
                && refused.contains("ulak status shop@new-server"),
            "{refused}"
        );
    }

    /// A Compose project name cannot hold `@`, and a destination may.
    #[test]
    fn a_name_is_split_at_the_first_at_sign_so_the_server_keeps_its_user() {
        let catalog = catalog_of(&[
            declared("shop", "deploy@10.0.0.1", "/a", true),
            declared("shop", "other", "/b", true),
        ]);
        assert_eq!(
            resolved(&catalog, "shop@deploy@10.0.0.1", "/").unwrap(),
            "shop@deploy@10.0.0.1"
        );
    }

    /// A typo is answered with the name it most plausibly meant, spelled
    /// as the whole command with every argument, then with what exists.
    #[test]
    fn an_unknown_name_offers_the_near_miss_and_what_is_declared() {
        let catalog = catalog_of(&[
            declared("shop", "devbox", "/code/shop", true),
            declared("shop-test", "devbox", "/code/shop", false),
            declared("blog", "devbox", "/code/blog", true),
        ]);
        let args = ["--dry-run".to_string()];
        let error = resolve(&catalog, "shpo", Path::new("/home"), None, "sync", &args)
            .map(|_| ())
            .unwrap_err();
        let refused = said(&error);
        assert!(refused.contains("no stack named shpo"), "{refused}");
        assert!(
            refused.contains("did you mean: ulak sync shop --dry-run"),
            "{refused}"
        );
        assert!(
            refused.contains("declared on this machine: blog, shop, shop-test"),
            "{refused}"
        );

        // Typed inside a checkout, the list narrows to what was declared
        // there.
        let refused = said(&resolved(&catalog, "nope", "/code/shop").unwrap_err());
        assert!(
            refused.contains("declared here: shop, shop-test"),
            "{refused}"
        );
        assert!(!refused.contains("did you mean"), "{refused}");

        let refused = said(&resolved(&[], "shop", "/").unwrap_err());
        assert!(
            refused.contains("nothing is declared on this machine yet"),
            "{refused}"
        );
    }

    #[test]
    fn a_name_on_the_wrong_server_says_which_server_has_it() {
        let catalog = catalog_of(&[declared("shop", "devbox", "/code/shop", true)]);
        let refused = said(&resolved(&catalog, "shop@elsewhere", "/").unwrap_err());
        assert!(
            refused.contains("shop is not declared on elsewhere"),
            "{refused}"
        );
        assert!(
            refused.contains("it is on devbox: ulak status shop"),
            "{refused}"
        );
    }

    /// A name chosen from anywhere can point at a checkout that has since
    /// moved. The bare rebuild error named no stack, and its fallback step,
    /// `ulak doctor`, hit the same wall again.
    #[test]
    fn a_named_stack_whose_checkout_is_gone_says_where_it_was_declared() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("moved-away");
        let gone = gone.to_str().unwrap();
        let refusal = |desired: &Desired, command: &str| {
            let error = desired.rebuild_for_stack(&desired.stack_id()).unwrap_err();
            said(&unreadable(desired, error, command, &["--dry-run".into()]))
        };

        let live = declared("blog", "devbox", gone, true);
        let refused = refusal(&live, "sync");
        assert!(
            refused.starts_with(&format!(
                "blog was declared from {gone}, which no longer exists"
            )),
            "{refused}"
        );
        assert!(
            refused.contains("cd into it, then ulak docker compose -p blog up -d"),
            "{refused}"
        );
        assert!(refused.contains("ulak status"), "{refused}");
        assert!(!refused.contains("ulak doctor"), "{refused}");

        // Idle stays idle: it is only run from the new place, and never
        // cleaned from there, where the workspace is a different one.
        let idle = declared("blog", "devbox", gone, false);
        let refused = refusal(&idle, "sync");
        assert!(
            refused.contains("cd into it, then: ulak sync --dry-run"),
            "{refused}"
        );
        assert!(!refused.contains("up -d"), "{refused}");
        let refused = refusal(&idle, "clean");
        assert!(!refused.contains("cd into it"), "{refused}");

        // The checkout is there but its file is not: the reason and its
        // own step survive under the stack's name.
        let mut missing_file = declared("shop-test", "devbox", tmp.path().to_str().unwrap(), false);
        missing_file.argv_globals = vec!["-f".into(), "compose.test.yaml".into()];
        let refused = refusal(&missing_file, "status");
        assert!(
            refused.starts_with("shop-test was declared from ")
                && refused.contains("compose file compose.test.yaml (from -f) cannot be read"),
            "{refused}"
        );
        assert!(refused.contains("check the path from"), "{refused}");
    }

    #[test]
    fn an_empty_name_or_server_is_refused_with_both_spellings() {
        for spec in ["", "@devbox", "shop@"] {
            let refused = said(&resolved(&[], spec, "/").unwrap_err());
            assert!(
                refused.contains("is not a stack name"),
                "{spec:?}: {refused}"
            );
            assert!(
                refused.contains("ulak status my-app") && refused.contains("my-app@my-server"),
                "{refused}"
            );
        }
    }

    /// Typed flags and a typed name are two answers to one question.
    #[test]
    fn a_name_and_compose_flags_together_are_refused_with_each_way_alone() {
        let globals = ["-f".to_string(), "compose.dev.yaml".to_string()];
        let args = ["--dry-run".to_string()];
        let refused = said(&named(&globals, "sync", &args, "shop").unwrap_err());
        assert!(refused.contains("both choose the project"), "{refused}");
        assert!(refused.contains("ulak sync shop --dry-run"), "{refused}");
        assert!(
            refused.contains("ulak -f compose.dev.yaml sync --dry-run"),
            "{refused}"
        );
    }

    #[test]
    fn a_selector_names_the_server_only_when_another_server_shares_the_name() {
        let shop = declared("shop", "devbox", "/a", true);
        let blog = declared("blog", "devbox", "/b", true);
        let blog_elsewhere = declared("blog", "other", "/c", true);
        let catalog = catalog_of(&[shop.clone(), blog.clone(), blog_elsewhere.clone()]);
        assert_eq!(selector_in(&catalog, &shop), "shop");
        assert_eq!(selector_in(&catalog, &blog), "blog@devbox");
        assert_eq!(selector_in(&catalog, &blog_elsewhere), "blog@other");
    }

    #[test]
    fn the_near_miss_is_one_unique_answer_or_none() {
        let names = ["blob", "blog", "shop", "shop-test"];
        let near = |typed| closest(typed, &names);
        assert_eq!(near("shpo"), Some("shop"), "two edits away");
        assert_eq!(near("shop-t"), Some("shop-test"), "the only name it begins");
        assert_eq!(
            near("blo"),
            None,
            "begins two names, and is as near to both"
        );
        assert_eq!(near("zzzzzz"), None, "nothing is near");
        assert_eq!(edits("kitten", "sitting"), 3);
        assert_eq!(edits("", "abc"), 3);
    }

    /// A name two servers share is still one near miss. Judged per
    /// declaration it tied with itself, and `apii` suggested nothing.
    #[test]
    fn a_near_miss_two_servers_share_is_spelled_to_resolve_from_here() {
        let catalog = catalog_of(&[
            declared("api", "devbox", "/code/api", true),
            declared("api", "staging", "/work/api", true),
            declared("blog", "devbox", "/code/blog", true),
        ]);
        let args = ["--dry-run".to_string()];
        let refused = |cwd: &str| {
            said(
                &resolve(&catalog, "apii", Path::new(cwd), None, "sync", &args)
                    .map(|_| ())
                    .unwrap_err(),
            )
        };

        // The checkout settles which server, so the name alone resolves.
        let here = refused("/code/api");
        assert!(
            here.contains("did you mean: ulak sync api --dry-run"),
            "{here}"
        );
        assert!(!here.contains("or: "), "{here}");

        // Anywhere else it takes the server, once for each.
        let elsewhere = refused("/");
        assert!(
            elsewhere.contains("did you mean: ulak sync api@devbox --dry-run")
                && elsewhere.contains("or: ulak sync api@staging --dry-run"),
            "{elsewhere}"
        );
    }

    #[test]
    fn a_long_list_of_names_stays_one_line() {
        let names: Vec<String> = (0..11).map(|i| format!("s{i:02}")).collect();
        assert_eq!(
            some_of(&names),
            "s00, s01, s02, s03, s04, s05, s06, s07, and 3 more"
        );
        assert_eq!(some_of(&names[..2]), "s00, s01");
    }

    fn candidate(cwd: &Path, file: &str, identity: &str, live: bool) -> Candidate {
        let inv = Invocation::capture_in(
            cwd,
            &["-f".to_string(), file.to_string()],
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        let desired = declared(identity, "devbox", &cwd.display().to_string(), live);
        Candidate {
            stack_id: desired.stack_id(),
            rebuilt: Rebuilt {
                workspace_key: crate::config::WorkspaceKey::from_namespace(
                    "client-a",
                    inv.workspace_identity_path(),
                )
                .unwrap(),
                inv,
            },
            desired,
            score: 1,
        }
    }

    /// The refusal that replaced a screen of absolute `-f`,
    /// `--project-directory` and `# comment` lines: one short command per
    /// stack, each carrying every argument typed, so a `--dry-run` stays a
    /// dry run. Without a terminal, `Choose::Ask` ends here too.
    #[test]
    fn several_stacks_here_are_refused_with_one_short_named_command_each() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::write(dir.join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(dir.join("compose.test.yaml"), "services: {}\n").unwrap();
        let several = || Several {
            cwd: dir.clone(),
            candidates: vec![
                candidate(&dir, "compose.yaml", "shop", true),
                candidate(&dir, "compose.test.yaml", "shop-test", false),
            ],
            catalog: Vec::new(),
        };

        let args = ["--dry-run", "--max-delete", "7"].map(String::from);
        let refused = said(&several().refusal(Spelling::Short, "sync", &args));
        assert_eq!(
            refused,
            "2 stacks are declared here — name the one to sync\n\
             ulak sync shop --dry-run --max-delete 7\n\
             ulak sync shop-test --dry-run --max-delete 7\n\
             each one's state: ulak status"
        );
        assert!(!refused.contains('#') && !refused.contains("--project-directory"));

        let error = several()
            .choose("doctor", &[], Choose::Ask)
            .map(|_| ())
            .unwrap_err();
        assert!(said(&error).contains("ulak doctor shop-test"));
        let refused = said(&several().refusal(Spelling::Short, "clean", &[]));
        assert!(
            refused.contains("name the one whose workspace to remove"),
            "{refused}"
        );

        let labels = pick_labels(&several().listed());
        assert_eq!(
            labels,
            vec![
                "shop        live   devbox   compose.yaml",
                "shop-test   idle   devbox   compose.test.yaml",
            ]
        );
    }

    /// `--raw` keeps the old full lines on request: every selector
    /// absolute, every typed argument, and a `#` label so each line still
    /// pastes as it stands.
    #[test]
    fn several_stacks_refused_raw_spell_each_invocation_in_full() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::write(dir.join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(dir.join("compose.test.yaml"), "services: {}\n").unwrap();
        let several = Several {
            cwd: dir.clone(),
            candidates: vec![
                candidate(&dir, "compose.yaml", "shop", true),
                candidate(&dir, "compose.test.yaml", "shop-test", false),
            ],
            catalog: Vec::new(),
        };
        let args = ["--dry-run".to_string()];
        let refused = said(&several.refusal(Spelling::Raw, "sync", &args));
        let at = sh_quote(&dir.display().to_string());
        for (file, name) in [("compose.yaml", "shop"), ("compose.test.yaml", "shop-test")] {
            let full = format!(
                "-f {} -p {name} --project-directory {at} sync --dry-run  # {name} on devbox",
                sh_quote(&dir.join(file).display().to_string())
            );
            assert!(refused.contains(&full), "{full}\nin:\n{refused}");
        }
        assert!(
            !refused.contains("ulak sync shop "),
            "no stack names: {refused}"
        );
        assert!(
            refused.ends_with("each one's state: ulak status --raw"),
            "{refused}"
        );

        let listed = several.listed();
        assert!(
            listed[1]
                .in_full(&["status"])
                .contains("-p shop-test --project-directory"),
            "{}",
            listed[1].in_full(&["status"])
        );
    }

    /// A declared project's next steps are its name: the same line from
    /// every directory, and drift-proof without `env ULAK_HOST=`, since the
    /// name selects the declaration and the declaration pins the server.
    #[test]
    fn a_root_command_for_a_declared_stack_is_its_name_with_every_argument() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::write(dir.join("ulak.toml"), "host = \"new-server\"\n").unwrap();
        std::fs::write(dir.join("compose.yaml"), "services: {}\n").unwrap();
        let inv = Invocation::capture_in(&dir, &[], std::collections::BTreeMap::new()).unwrap();
        let mut project = Project::locate_from(inv).unwrap();
        let tag = std::process::id();
        let identity = format!("named-{tag}");
        let server = format!("pinned-{tag}");
        let declaration = declared(&identity, &server, &dir.display().to_string(), true);
        intent::write_desired(&declaration.stack_id(), &declaration).unwrap();
        project.pin_existing_stack(&server, &identity);

        assert_eq!(
            root_command(&project, &["sync", "--max-delete=3"]),
            format!("ulak sync {identity} --max-delete=3")
        );
        assert_eq!(
            root_command(&project, &["doctor"]),
            format!("ulak doctor {identity}")
        );

        // `--raw` names no stack: the files, name and directory in full,
        // aimed at the declared server because the config has moved on.
        let raw = root_command_in(Spelling::Raw, &project, &["sync", "--max-delete=3"]);
        assert!(
            raw.starts_with(&format!("env ULAK_HOST={server} ulak -f "))
                && raw.ends_with(&format!(
                    "-p {identity} --project-directory {} sync --max-delete=3",
                    sh_quote(&dir.display().to_string())
                )),
            "{raw}"
        );
    }

    fn captured(dir: &Path, argv: &[&str]) -> Invocation {
        let argv: Vec<String> = argv.iter().map(|word| word.to_string()).collect();
        Invocation::capture_in(dir, &argv, std::collections::BTreeMap::new()).unwrap()
    }

    fn here(cwd: &Path, inv: &Invocation, identity: Option<&str>, bare: Bare) -> String {
        here_globals_in(cwd, std::collections::BTreeMap::new(), inv, identity, bare).join(" ")
    }

    /// Each hint is the shortest line that reads back, from where the user
    /// stands, to the exact stack: measured by capturing it, never assumed.
    #[test]
    fn a_line_for_this_shell_is_the_shortest_spelling_that_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let app = root.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(app.join("compose.test.yaml"), "services: {}\n").unwrap();

        // Discovery already finds the default file: only the name is said.
        let default = captured(&app, &[]);
        assert_eq!(here(&app, &default, Some("shop"), Bare::Allowed), "-p shop");
        assert_eq!(here(&app, &default, None, Bare::Allowed), "");
        // A root command never goes bare: declarations come first there.
        assert_eq!(here(&app, &default, None, Bare::Refused), "-f compose.yaml");

        // A second file is named, relative to where the user stands.
        let test = captured(&app, &["-f", "compose.test.yaml"]);
        assert_eq!(
            here(&app, &test, Some("shop-test"), Bare::Allowed),
            "-f compose.test.yaml -p shop-test"
        );
        assert_eq!(
            here(&root, &test, Some("shop-test"), Bare::Allowed),
            "-f app/compose.test.yaml -p shop-test"
        );

        // From an unrelated directory the path is absolute, and the
        // project directory still goes unsaid because it is Docker's own
        // default for that file.
        let away = tempfile::tempdir().unwrap();
        let file = app.join("compose.test.yaml");
        assert_eq!(
            here(away.path(), &test, Some("shop-test"), Bare::Allowed),
            format!("-f {} -p shop-test", file.display())
        );

        // A project directory that is not the default is kept.
        let moved = captured(
            &app,
            &["-f", "compose.test.yaml", "--project-directory", ".."],
        );
        assert_eq!(
            here(&app, &moved, Some("x"), Bare::Allowed),
            format!(
                "-f compose.test.yaml -p x --project-directory {}",
                root.display()
            )
        );

        // At the filesystem root, where the service runs, nothing is
        // relative: its text is read later, from anywhere.
        assert_eq!(
            here(Path::new("/"), &test, Some("shop-test"), Bare::Allowed),
            exact_globals(&test, Some("shop-test")).join(" ")
        );
    }

    /// The environment of the shell is part of what a paste reads: a
    /// `COMPOSE_FILE` there would turn the bare form into another project,
    /// so the files are spelled.
    #[test]
    fn a_compose_file_in_this_shells_environment_keeps_the_files_spelled() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().canonicalize().unwrap();
        std::fs::write(app.join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(app.join("other.yaml"), "services: {}\n").unwrap();
        let default = captured(&app, &[]);
        let env = std::collections::BTreeMap::from([(
            "COMPOSE_FILE".to_string(),
            "other.yaml".to_string(),
        )]);
        assert_eq!(
            here_globals_in(&app, env, &default, Some("shop"), Bare::Allowed).join(" "),
            "-f compose.yaml -p shop"
        );
    }
}
