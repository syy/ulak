//! Project selection for Ulak's own root management commands.
//!
//! Docker Compose starts every invocation from argv, environment and file
//! discovery. `invocation.rs` owns that rule and this module never changes
//! it. A root command asks a different question once Ulak has declared a
//! stack: status, doctor, sync and clean must address the exact Compose
//! files, project name, destination and sync workspace that Ulak is already
//! maintaining. `Desired` is the one complete, validated answer.
//!
//! Selection therefore has three rungs, in this order:
//!   1. globals or owned `COMPOSE_*` input supplied for this command;
//!   2. the most specific validated Desired declaration for this local
//!      Ulak workspace and cwd;
//!   3. ordinary Docker discovery, but never across a nearer Ulak config
//!      boundary.
//!
//! Several declarations at the same specificity are ambiguous. Guessing is
//! especially unsafe for `clean`, so every root command refuses and prints
//! the exact explicit invocations instead. A declaration is rebuilt through
//! `Desired::rebuild_for_stack`; this module never parses its argv or trusts
//! its paths a second way. This module also owns pasteable command rendering:
//! every Compose selector, the historical identity when there is one, and
//! every root-command argument travel together so a `--dry-run` cannot turn
//! into a mutating retry.
//!
//! A declaration outranks a later `host` edit, and this module is where that
//! is said: `warn_on_destination_drift` speaks once, at the one site a
//! declaration is chosen, and every other surface reads
//! `Project::destination()` rather than comparing the two again. The way
//! out for a server that no longer exists is `clean --forget-destination`,
//! selected by DESTINATION through `declared_on`: today's config no longer
//! names that server, so nothing that starts from the config could reach the
//! declaration. Every pasteable command rendered from a project passes
//! through `render_project`, and that is where a pinned project whose config
//! no longer answers the same server gets `env ULAK_HOST=<declared>` in front
//! (`aimed_at`): the globals alone bind a paste to the config, which for a
//! drifted checkout is a different stack than the line describes.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::config::{self, Project, Workspace};
use crate::intent::{self, Desired, Rebuilt};
use crate::invocation::Invocation;
use crate::ssh::sh_quote;
use crate::ui::{self, fail};

pub enum Context {
    Project(Box<Project>),
    Workspace(Box<Workspace>),
    Outside,
}

struct Candidate {
    desired: Desired,
    rebuilt: Rebuilt,
    score: usize,
}

/// Locate the context for one of Ulak's own root commands.
pub fn locate(globals: &[String], command: &str, command_args: &[String]) -> Result<Context> {
    if !globals.is_empty() || crate::invocation::compose_environment_is_explicit() {
        return Project::locate(globals).map(|project| Context::Project(Box::new(project)));
    }

    let cwd = std::env::current_dir()
        .context("cannot read current directory")?
        .canonicalize()
        .context("cannot canonicalize current directory")?;
    let boundary = config::workspace_home(&cwd);

    let mut candidates = declared_candidates(&cwd, boundary.as_deref());
    if let Some(best) = candidates.iter().map(|candidate| candidate.score).max() {
        candidates.retain(|candidate| candidate.score == best);
        if candidates.len() > 1 {
            return Err(ambiguous(&cwd, command, command_args, &candidates));
        }
        // `best` came from this vector and `retain` keeps every row with
        // that score, so this branch has exactly one element.
        let candidate = candidates.remove(0);
        let project = candidate.desired.project_from_rebuilt(candidate.rebuilt)?;
        warn_on_destination_drift(&project);
        activate(&project);
        return Ok(Context::Project(Box::new(project)));
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
pub fn project(globals: &[String], command: &str, command_args: &[String]) -> Result<Project> {
    match locate(globals, command, command_args)? {
        Context::Project(project) => Ok(*project),
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

/// An exact root command for this invocation, safe to paste into a shell.
pub fn root_command(project: &Project, tail: &[&str]) -> String {
    render_project(
        &["ulak"],
        project,
        &tail
            .iter()
            .map(|word| (*word).to_string())
            .collect::<Vec<_>>(),
    )
}

/// An exact Docker Compose command for this invocation, safe to paste.
pub fn compose_command(project: &Project, tail: &[&str]) -> String {
    render_project(
        &["ulak", "docker", "compose"],
        project,
        &tail
            .iter()
            .map(|word| (*word).to_string())
            .collect::<Vec<_>>(),
    )
}

/// An exact Docker Compose command for a known resolved identity over the
/// selected files. The server model, historical lifecycle state or Docker
/// labels may know `-p` more precisely than today's local inputs; spelling it
/// here keeps that answer attached to the invocation that can act on it.
pub fn compose_command_for_identity(project: &Project, identity: &str, tail: &[&str]) -> String {
    aim_for(
        project,
        render(
            &["ulak", "docker", "compose"],
            &project.inv,
            Some(identity),
            &tail
                .iter()
                .map(|word| (*word).to_string())
                .collect::<Vec<_>>(),
        ),
    )
}

/// Say once, here, that a declaration outranks a later config edit.
///
/// ONE site, because this is the only place a declaration is chosen for a
/// root command. `init` passes through here too and addresses no server,
/// so the headline is about the checkout, not about "this command".
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
            let declared_from = Path::new(&desired.cwd);
            !desired.workspace_id.is_empty()
                && desired.workspace_id.chars().all(|c| c.is_ascii_hexdigit())
                && config::validate_ssh_dest(&desired.destination).is_ok()
                && (boundary.is_none()
                    || config::workspace_home(declared_from).as_deref() == boundary)
                && scope_of(cwd, boundary, declared_from).is_some()
        })
        .collect()
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
    let cwd = std::env::current_dir()
        .context("cannot read current directory")?
        .canonicalize()
        .context("cannot canonicalize current directory")?;
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

fn ambiguous(
    cwd: &Path,
    command: &str,
    command_args: &[String],
    candidates: &[Candidate],
) -> anyhow::Error {
    let mut error = fail!(
        "more than one declared Compose project matches {}",
        cwd.display()
    )
    .now("choose one explicitly:");
    let tail = command_line(command, command_args);
    for candidate in candidates {
        // Through the pinned project, so `render_project` aims the line
        // exactly as it would for a chosen declaration: explicit globals
        // select by config, and without the host both choices would paste
        // into the SAME stack.
        let rebuilt = Rebuilt {
            inv: candidate.rebuilt.inv.clone(),
            workspace_key: candidate.rebuilt.workspace_key.clone(),
        };
        let rendered = match candidate.desired.project_from_rebuilt(rebuilt) {
            Ok(project) => render_project(&["ulak"], &project, &tail),
            Err(_) => render(
                &["ulak"],
                &candidate.rebuilt.inv,
                Some(&candidate.desired.identity),
                &tail,
            ),
        };
        error = error.now(format!(
            "{rendered}  # {} on {}",
            candidate.desired.identity, candidate.desired.destination
        ));
    }
    error.into_err()
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
    let mut exact = inv.clone();
    if let Some(identity) = identity {
        exact.project_name = Some(identity.to_string());
    }
    words.extend(exact.globals_argv());
    words.extend_from_slice(tail);
    shell_join(&words)
}

fn shell_join(words: &[String]) -> String {
    words
        .iter()
        .map(|word| sh_quote(word))
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
    #[test]
    fn every_command_rendered_from_a_drifted_project_is_aimed_and_none_otherwise() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::write(dir.join("ulak.toml"), "host = \"new-server\"\n").unwrap();
        std::fs::write(dir.join("compose.yaml"), "services: {}\n").unwrap();
        let inv = Invocation::capture_in(&dir, &[], std::collections::BTreeMap::new()).unwrap();
        let mut project = Project::locate_from(inv).unwrap();

        project.pin_existing_stack("new-server", "api");
        for line in [
            root_command(&project, &["clean"]),
            compose_command(&project, &["down"]),
            compose_command_for_identity(&project, "api", &["up", "-d"]),
        ] {
            assert!(line.starts_with("ulak "), "no drift, no prefix: {line}");
        }

        project.pin_existing_stack("dead-server", "api");
        for line in [
            root_command(&project, &["clean"]),
            compose_command(&project, &["down"]),
            compose_command_for_identity(&project, "api", &["up", "-d"]),
        ] {
            assert!(
                line.starts_with("env ULAK_HOST=dead-server ulak "),
                "a drifted project's paste must carry its server: {line}"
            );
            assert!(line.contains("-p api"), "{line}");
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
}
