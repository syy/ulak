//! `docker stack deploy` and `docker stack config`: a Compose model
//! that is read by the CLIENT.
//!
//! Swarm takes a service spec, not a directory. Everything that turns
//! YAML into that spec happens in the docker CLI before the daemon is
//! told anything: the compose files are merged and interpolated, every
//! `env_file:` is read into the environment, and every `configs:` /
//! `secrets:` `file:` is read for its CONTENTS, which travel to the
//! swarm as objects. Forward `-c stack.yml` verbatim and the CLI on the
//! server looks for all of it in whatever directory the ssh session
//! landed in.
//!
//! So the model is resolved the way Compose's already is — `footprint`
//! runs the resolution ON THE SERVER, over a bootstrap copy that
//! reproduces the absolute local paths, and what comes back is the set
//! of local files this stack needs. That set is synced and the `-c`
//! paths are rewritten to where the files landed.
//!
//! THREE THINGS MEASURED against docker 29.4.0, because each of them
//! would otherwise have been guessed wrong:
//!
//! Relative paths inside EVERY `-c` file resolve against the FIRST
//! file's directory — not against each file's own directory, and not
//! against the cwd. `-c sub/stack.yml -c second.yml` resolves
//! `second.yml`'s `./second.conf` to `sub/second.conf`. That is the rule
//! `Invocation` already encodes as the project directory, which is why
//! the reuse is exact rather than approximate. It is also why the run
//! happens in the workspace root rather than the mirror of the cwd: for
//! a stack the cwd is not load-bearing, so nothing has to be relative
//! to it.
//!
//! `stack` reads no `.env` file. Not from the cwd, not from beside the
//! compose file — interpolation comes from the process environment
//! alone. Measured on 29.4.0 in both places: with `TAG=1.2.3` written
//! into a `.env` beside the compose file, and again into the cwd's,
//! `stack config` still printed `image: 'app:'` and mentioned neither
//! file. On the server that environment is the ssh session's, and none
//! of this shell's travels — `Remote::spell` sends `cd DIR && docker …`
//! and nothing more — so `image: app:${TAG}` deploys as `app:`, which
//! docker then reports as "invalid reference format", naming neither
//! the variable nor the reason. Compose escapes this because it DOES
//! read the `.env` that travels with the workspace; stack has no such
//! door. An explicit `env_file: .env` is not that door either, and the
//! difference is worth having measured: the file IS read, and the
//! variables in it arrive in the service's `environment:` — but the
//! substitution has already happened by then, so the same run printed
//! `environment: {TAG: 1.2.3}` and `image: 'app:'` in one breath.
//!
//! What this module can do is refuse to let that happen in silence, and
//! that is all it does. When a `.env` sits where a user would expect it
//! to be read and a compose file actually interpolates something,
//! `warn_about_unread_env_files` names the file and says where the
//! values have to come from instead. It never opens it: reading one
//! here and substituting the values would deploy a stack that docker,
//! run by hand on that same server, would have deployed differently.
//! Quietly disagreeing with docker is worse than the empty string,
//! which at least fails where the user can see it.
//!
//! `stack deploy` never builds. It says so — "Ignoring unsupported
//! options: build" — and requires an `image:` that already exists. A
//! build context is therefore not part of a stack's footprint, and it
//! is carried anyway: `stack deploy -c x.yml` and `compose up -f x.yml`
//! hash to the same workspace, so two different footprints over it
//! would take turns marking each other's files doomed. A carried,
//! unused context costs one incremental rsync; deletion ping-pong costs
//! the user their files twice over.
//!
//! `--with-registry-auth` is passed through untouched, and the reason is
//! worth writing down because it reads backwards at first. The flag
//! makes the CLI read registry credentials out of its OWN
//! `~/.docker/config.json` and attach them to the service spec so swarm
//! agents can pull a private image. Under Ulak that CLI is on the
//! server, so the credentials sent are the server's — the ones
//! `ulak docker login` put there — and this machine's config file is
//! never opened, because no docker CLI ever runs here. Refusing the flag
//! would protect nothing that is currently exposed while costing every
//! multi-node private-registry stack its only way to distribute pull
//! credentials. The flip side is real but is docker's to report: a user
//! who logged in on their laptop rather than through Ulak sends nothing
//! useful, and the pull fails naming the registry.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::catalog::Resolved;
use crate::config::Project;
use crate::docker::{Remote, Tty, status_code};
use crate::invocation::Invocation;
use crate::lockfile::WorkspaceLock;
use crate::ssh::Ssh;
use crate::sync;
use crate::ui::{self, fail};

/// Every flag of `stack deploy` and `stack config` that consumes NO
/// argument, read off both `--help` outputs (29.4.0). The two that do
/// take one are `-c/--compose-file` and `--resolve-image`.
///
/// The booleans are the table worth keeping, because anything absent
/// from it is assumed to take a value and that is the conservative
/// direction. Guess "boolean" for a flag docker adds later and
/// `--newflag --compose-file f.yml` claims `f.yml` — which in that
/// reading is the STACK NAME — and rewrites it into a path, renaming
/// the user's stack. Guess "takes a value" and the worst case is a
/// compose file stepped over, which the server then reports as a file
/// it cannot open.
///
/// A smaller job than the same table in `runspec` either way: nothing
/// here depends on finding a positional, because stack reads its flags
/// on both sides of the STACK name — measured, `deploy mystack -c f.yml`
/// and `deploy -c f.yml mystack` are the same command.
const LONG_WITHOUT_VALUE: &[&str] = &[
    "detach",
    "help",
    "prune",
    "quiet",
    "skip-interpolation",
    "with-registry-auth",
];

/// `-c` takes a value; `-d` and `-q` do not. All three spellings are
/// real and all three were tried: `-c f.yml`, `-c=f.yml`, and the
/// bundled `-qc f.yml`.
const SHORT_WITH_VALUE: &str = "c";
const SHORT_WITHOUT_VALUE: &str = "dq";

pub fn run(resolved: &Resolved) -> Result<ExitCode> {
    let mut args = resolved.argv.clone();
    let spec = StackSpec::parse(&args, resolved.tail_start)?;

    // No `-c` names no model. Unlike compose, stack never goes looking
    // for a compose file of its own accord, so there is nothing here to
    // resolve and docker's own complaint is the right one to hear.
    if spec.files.is_empty() {
        let remote = Remote::open()?;
        return status_code(remote.docker_with_stdin(
            &args,
            None,
            Tty::Auto,
            resolved.entry.secret_flags,
            stdin_for(),
        )?);
    }

    let cwd = std::env::current_dir().context("cannot read current directory")?;
    // `rebuild_in` rather than the typed door on purpose: `capture_in`
    // REMEMBERS the compose files it was handed, and a stack's `-c` is
    // not this directory's compose invocation. Through the typed door,
    // one `stack deploy -c stack.yml` would re-aim every later bare
    // `ulak docker compose up` here at the stack file.
    let inv = Invocation::rebuild_in(&cwd, &spec.as_compose_argv(), std::env::vars().collect())?;
    let mut project = Project::locate_from(inv)?;
    // The door for a caller-owned invocation also leaves audit ownership to
    // that caller: the service keeps one fleet trail, management activates
    // only after selection, and a human typed this stack route now.
    crate::audit::set_context(project.workspace_id());
    warn_about_unread_env_files(&project.inv);

    let dest = project.ssh_dest()?;
    let ssh = Ssh::new(&dest)?;
    let footprint = crate::footprint::resolve_cached(&project, &ssh, &dest)?;
    project.anchor = footprint.anchor.clone();
    spec.rewrite(&project, &mut args)?;

    // Held across the command, not just the sync. `compose up` is a
    // TREE_READER for the same reason and makes the same choice: the
    // model is read at the start, and a deploy asked to wait for
    // convergence is the same shape of command as an `up` in the
    // foreground.
    let _lock = WorkspaceLock::acquire(project.workspace_id())?;
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

    // The workspace root, not the mirror of the cwd: every path this
    // command resolves hangs off the first compose file, which is named
    // here relative to the anchor. It also means a stack deployed from
    // outside the anchor still works, where a cwd mirror would have had
    // no remote directory to name.
    let workdir = project.remote_dir();
    let remote = Remote::to(&dest)?;
    let status = remote.docker_with_stdin(
        &args,
        Some(&workdir),
        Tty::Auto,
        resolved.entry.secret_flags,
        stdin_for(),
    )?;

    // No pull back. `-d` defaults to TRUE for deploy, so the command
    // hands the stack to the swarm and returns before a service has
    // written anything; `config` only ever writes to stdout. What the
    // services produce afterwards is the next sync's business, which is
    // the same answer compose's detached `up` gets.
    //
    // `config` prints the model as the SERVER resolved it, so the paths
    // in its output are workspace paths there rather than the ones the
    // user typed. That is the honest answer to what it was asked — this
    // is the model the deploy will use — and rewriting them back would
    // be inventing a rendering nobody could act on.
    status_code(status)
}

/// Where the remote `stack deploy`/`config` reads its stdin from.
///
/// Nowhere. The model comes out of the `-c` files, and the one spelling
/// that would have read a stream — `-c -` — is refused in `claim` below,
/// because the model has to be resolved before it can be deployed and a
/// stream can only be read once. So there is no per-command question
/// here, only the general one.
///
/// The policy is `docker::stdin_for`'s, and the reason is written out
/// there: ssh drains its own stdin as soon as the channel is up, whether
/// or not anything on the far side is listening, so an `ulak docker
/// stack deploy` in the middle of a shell pipeline ate what the next
/// reader was going to get — which plain docker never does.
fn stdin_for() -> std::process::Stdio {
    if wants_this_terminals_stdin(std::io::stdin().is_terminal()) {
        std::process::Stdio::inherit()
    } else {
        std::process::Stdio::null()
    }
}

/// Split out from `stdin_for` for the reason `docker.rs` splits its own:
/// stdin is never a terminal under `cargo test`, and a decision no test
/// can reach is one that quietly drifts back to inheriting.
///
/// A terminal is handed over whatever the command is — there is nothing
/// queued on it to lose, and taking it away would break anything on the
/// far side that decides to ask a question.
fn wants_this_terminals_stdin(stdin_is_a_terminal: bool) -> bool {
    stdin_is_a_terminal
}

/// Name every `.env` this deploy's variables are not going to come from.
///
/// "not read when the variables are substituted" rather than "not read":
/// a compose file that names the same file in an explicit `env_file:`
/// DOES have it read, into the service's environment. It still supplies
/// nothing to the substitution, which is the surprise being headed off,
/// and claiming more than that would be a warning the user can catch us
/// out on.
fn warn_about_unread_env_files(inv: &Invocation) {
    for path in unread_env_files(inv) {
        ui::warn(&format!(
            "{} is not read when this stack's variables are substituted",
            path.display()
        ));
        for line in UNREAD_ENV_ADVICE {
            ui::dim(line);
        }
    }
}

const UNREAD_ENV_ADVICE: &[&str] = &[
    "`docker stack` substitutes from the environment alone — here, the ssh session's",
    "an unset variable becomes the empty string, so `image: app:${TAG}` deploys as `app:`",
    "export the values in the server's shell profile, or write them into the compose file",
];

/// The `.env` files a user has reason to expect to be read here.
///
/// Two places, and only two: the directory the command was typed in,
/// and the project directory, which is where Compose reads its own.
/// Both were measured to be ignored by `stack`.
///
/// A detector, in the same spirit as bake's: it answers "is there a file
/// whose absence from the interpolation is going to surprise you", names
/// it, and never opens it. The answer is only worth giving when
/// something is actually interpolated — a stack with no variables in it
/// is unaffected, and a warning that fires on every project teaches the
/// user to skip the one that matters.
///
/// Kept apart from the printing because `ui::warn` writes straight to
/// stderr, where no test can reach it. This is the module's one defence
/// against a silent empty substitution, so it has to be one a test can
/// hold.
fn unread_env_files(inv: &Invocation) -> Vec<PathBuf> {
    if !inv.compose_files.iter().any(|f| interpolates(f)) {
        return Vec::new();
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in [inv.cwd.as_path(), inv.project_dir.as_path()] {
        // Both are canonical by construction, so one directory reached
        // two ways is still one entry rather than the same warning
        // printed twice.
        let candidate = dir.join(".env");
        if candidate.is_file() && !found.contains(&candidate) {
            found.push(candidate);
        }
    }
    found
}

/// Whether this compose file interpolates anything at all.
///
/// `$$` is Compose's escape for a literal `$`, so it goes first:
/// `image: app:$${TAG}` deploys a literal `${TAG}` and wants no `.env`.
/// What is left is a variable wherever a `$` is followed by `{` or by
/// the first character of a name, which are the two spellings Compose
/// accepts.
fn interpolates(file: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else {
        return false;
    };
    text.replace("$$", "")
        .as_bytes()
        .windows(2)
        .any(|w| w[0] == b'$' && (w[1] == b'{' || w[1] == b'_' || w[1].is_ascii_alphabetic()))
}

/// Where one flag's value sits: the argument it is in, and the byte it
/// starts at. A value of its own (`-c f.yml`) is the same shape with
/// the offset at zero, which is why this needs no second case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    index: usize,
    offset: usize,
}

impl Slot {
    fn get(self, args: &[String]) -> &str {
        &args[self.index][self.offset..]
    }

    fn set(self, args: &mut [String], value: &str) {
        args[self.index] = format!("{}{value}", &args[self.index][..self.offset]);
    }
}

/// The compose files one `stack deploy`/`stack config` names.
#[derive(Debug, Default)]
struct StackSpec {
    /// In the order they were typed, which is the order that decides
    /// the project directory and the merge.
    files: Vec<(Slot, String)>,
}

impl StackSpec {
    fn parse(args: &[String], tail_start: usize) -> Result<StackSpec> {
        let mut spec = StackSpec::default();
        let mut i = tail_start;
        while i < args.len() {
            let arg = args[i].as_str();
            if arg == "--" {
                break;
            }
            if let Some(long) = arg.strip_prefix("--") {
                // Docker prints its help and exits; no file is read.
                if long == "help" {
                    return Ok(StackSpec::default());
                }
                if let Some((name, _)) = long.split_once('=') {
                    if name == "compose-file" {
                        let offset = "--".len() + name.len() + "=".len();
                        spec.claim(Slot { index: i, offset }, args)?;
                    }
                    i += 1;
                    continue;
                }
                if LONG_WITHOUT_VALUE.contains(&long) || i + 1 == args.len() {
                    i += 1;
                    continue;
                }
                if long == "compose-file" {
                    spec.claim(
                        Slot {
                            index: i + 1,
                            offset: 0,
                        },
                        args,
                    )?;
                }
                i += 2;
                continue;
            }
            if arg.len() > 1
                && let Some(bundle) = arg.strip_prefix('-')
            {
                i += spec.claim_bundle(bundle, i, args)?;
                continue;
            }
            // The STACK name, or nothing at all for `config`. Neither is
            // a path, and neither ends the scan: docker reads flags on
            // both sides of it.
            i += 1;
        }
        Ok(spec)
    }

    /// One `-qc` bundle. Returns how many arguments it consumed.
    fn claim_bundle(&mut self, bundle: &str, index: usize, args: &[String]) -> Result<usize> {
        for (pos, c) in bundle.char_indices() {
            if !SHORT_WITH_VALUE.contains(c) {
                if !SHORT_WITHOUT_VALUE.contains(c) {
                    // Same forgiveness as the long form, and the same
                    // reason: nothing downstream depends on this scan
                    // having understood every letter.
                    return Ok(1);
                }
                continue;
            }
            let rest = &bundle[pos + c.len_utf8()..];
            let (slot, consumed) = if rest.is_empty() {
                if index + 1 == args.len() {
                    return Ok(1);
                }
                (
                    Slot {
                        index: index + 1,
                        offset: 0,
                    },
                    2,
                )
            } else {
                // Docker drops one `=` between a bundled flag and the
                // value stuck to it.
                let eaten = usize::from(rest.starts_with('='));
                let offset = "-".len() + pos + c.len_utf8() + eaten;
                (Slot { index, offset }, 1)
            };
            if c == 'c' {
                self.claim(slot, args)?;
            }
            return Ok(consumed);
        }
        Ok(1)
    }

    fn claim(&mut self, slot: Slot, args: &[String]) -> Result<()> {
        let raw = slot.get(args);
        if raw == "-" {
            return Err(
                fail!("Ulak cannot read a stack's compose file from stdin (`-c -`)")
                    .now("write it to a file and pass that path instead")
                    .now("the model has to be resolved before the stack can be deployed, and a stream can only be read once")
                    .into_err(),
            );
        }
        self.files.push((slot, raw.to_string()));
        Ok(())
    }

    /// The same file list, spelled the way `Invocation` reads it. The
    /// raw arguments are handed over untouched so they resolve against
    /// the cwd exactly as docker would have resolved them.
    fn as_compose_argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.files.len() * 2);
        for (_, raw) in &self.files {
            argv.push("-f".to_string());
            argv.push(raw.clone());
        }
        argv
    }

    /// Point every `-c` at the file's place in the remote workspace.
    ///
    /// The pairing is positional: `as_compose_argv` hands these same
    /// files over in this same order and `Invocation` resolves them one
    /// for one. If that ever stopped being true, `zip` would quietly
    /// rewrite one `-c` to another's path — a stack deployed from the
    /// wrong file, with nothing said — so the lengths are checked rather
    /// than trusted.
    fn rewrite(&self, project: &Project, args: &mut [String]) -> Result<()> {
        let resolved = &project.inv.compose_files;
        if resolved.len() != self.files.len() {
            return Err(fail!(
                "ulak resolved {} compose file(s) for a stack that names {}",
                resolved.len(),
                self.files.len()
            )
            .now("report this: the -c list and the resolved model have come apart")
            .into_err());
        }
        for ((slot, _), local) in self.files.iter().zip(resolved) {
            slot.set(args, &project.remote_rel(local)?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A project whose first compose file is `sub/stack.yml`, plus a
    /// second one beside it.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let yaml = "services:\n  web:\n    image: nginx\n";
        std::fs::write(root.join("sub/stack.yml"), yaml).unwrap();
        std::fs::write(root.join("second.yml"), yaml).unwrap();
        (temp, root)
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    /// `stack deploy`'s own arguments start at index 2.
    fn parse(parts: &[&str]) -> StackSpec {
        StackSpec::parse(&argv(parts), 2).unwrap()
    }

    fn named(spec: &StackSpec) -> Vec<String> {
        spec.files.iter().map(|(_, raw)| raw.clone()).collect()
    }

    /// A project as `run` would have built it, without reading the
    /// developer's own global config.
    ///
    /// `cwd` is where the command was typed and `anchor` is the
    /// workspace root the footprint settled on. They are separate
    /// because in real use they usually are: the anchor has to cover
    /// every `-c` file, so a stack deployed from a subdirectory is
    /// anchored above it.
    fn project_in(cwd: &Path, anchor: &Path, files: &[&str]) -> Project {
        let mut argv = Vec::new();
        for f in files {
            argv.push("-f".to_string());
            argv.push((*f).to_string());
        }
        let inv = Invocation::rebuild_in(cwd, &argv, BTreeMap::new()).unwrap();
        let workspace_key = crate::config::WorkspaceKey::from_namespace(
            "test-client",
            inv.workspace_identity_path(),
        )
        .unwrap();
        Project {
            config_home: anchor.to_path_buf(),
            anchor: anchor.to_path_buf(),
            name: "fixture".into(),
            identity: inv.compose_identity(),
            stack_pin: None,
            config: crate::config::Config::default(),
            workspace_key,
            inv,
        }
    }

    fn project_for(root: &Path, files: &[&str]) -> Project {
        project_in(root, root, files)
    }

    #[test]
    fn the_flag_table_never_puts_one_flag_on_both_sides() {
        for c in SHORT_WITH_VALUE.chars() {
            assert!(!SHORT_WITHOUT_VALUE.contains(c), "-{c} is on both");
        }
        for flag in LONG_WITHOUT_VALUE {
            assert!(
                !matches!(*flag, "compose-file" | "resolve-image"),
                "--{flag} takes a value"
            );
        }
    }

    #[test]
    fn an_unknown_flag_is_assumed_to_take_a_value() {
        // The conservative direction, and the reason is in the table's
        // doc: guessing "boolean" would let `--newflag --compose-file
        // f.yml` claim the STACK NAME as a compose file and rewrite it
        // into a path.
        let spec = parse(&[
            "stack",
            "deploy",
            "--brand-new-flag",
            "mystack",
            "-c",
            "sub/stack.yml",
        ]);

        assert_eq!(named(&spec), vec!["sub/stack.yml"]);
    }

    #[test]
    fn every_compose_file_spelling_is_found() {
        for parts in [
            vec!["stack", "deploy", "-c", "sub/stack.yml", "st"],
            vec!["stack", "deploy", "-c=sub/stack.yml", "st"],
            vec!["stack", "deploy", "--compose-file", "sub/stack.yml", "st"],
            vec!["stack", "deploy", "--compose-file=sub/stack.yml", "st"],
            vec!["stack", "deploy", "-qc", "sub/stack.yml", "st"],
            vec!["stack", "deploy", "-qc=sub/stack.yml", "st"],
        ] {
            let spec = parse(&parts);
            assert_eq!(named(&spec), vec!["sub/stack.yml"], "{parts:?}");
        }
    }

    #[test]
    fn a_flag_after_the_stack_name_is_still_read() {
        // Measured: `deploy mystack -c f.yml` and `deploy -c f.yml
        // mystack` are the same command. Unlike `run`, the positional
        // does not end the scan.
        let spec = parse(&["stack", "deploy", "mystack", "-c", "sub/stack.yml"]);

        assert_eq!(named(&spec), vec!["sub/stack.yml"]);
    }

    #[test]
    fn the_stack_name_is_never_mistaken_for_a_compose_file() {
        let spec = parse(&["stack", "deploy", "--prune", "mystack"]);
        assert!(spec.files.is_empty(), "a stack name is not a file");

        // The boolean flag has to stand in FRONT of something it must
        // not swallow, or the assertion is blind: with nothing to claim
        // in the argv, `files` is empty whether `--prune` consumed a
        // word or not. Measured against that — with "prune" taken out
        // of LONG_WITHOUT_VALUE the flag eats the `-c`, the model is
        // never resolved, and `-c sub/stack.yml` goes to a server that
        // cannot open it.
        let spec = parse(&["stack", "deploy", "--prune", "-c", "sub/stack.yml"]);

        assert_eq!(
            named(&spec),
            vec!["sub/stack.yml"],
            "a boolean flag consumes no word"
        );
    }

    #[test]
    fn resolve_image_takes_a_value_that_is_not_a_file() {
        let spec = parse(&[
            "stack",
            "deploy",
            "--resolve-image",
            "changed",
            "-c",
            "sub/stack.yml",
            "st",
        ]);

        assert_eq!(named(&spec), vec!["sub/stack.yml"]);
    }

    #[test]
    fn every_compose_file_is_kept_in_the_order_it_was_typed() {
        // The order is not cosmetic: the FIRST file decides the
        // directory that every relative path inside every file resolves
        // against.
        let spec = parse(&[
            "stack",
            "deploy",
            "-c",
            "sub/stack.yml",
            "-c",
            "second.yml",
            "st",
        ]);

        assert_eq!(named(&spec), vec!["sub/stack.yml", "second.yml"]);
    }

    #[test]
    fn a_compose_file_from_stdin_is_refused_in_the_stacks_own_words() {
        let err = StackSpec::parse(&argv(&["stack", "deploy", "-c", "-", "st"]), 2).unwrap_err();

        assert!(format!("{err}").contains("stdin"), "{err}");
        assert!(format!("{err}").contains("`-c -`"), "{err}");
    }

    /// `deploy` and `config` read no stdin at all, so ssh must not be
    /// left to drain a pipeline into a command that will never look at
    /// it — the swallowing `docker::stdin_for` was written against.
    ///
    /// Pinned beside the refusal it rests on: `-c -` is the only way a
    /// stack could ever have wanted a stream, and it is shut.
    #[test]
    fn a_stack_never_reads_a_piped_stdin() {
        assert!(
            !wants_this_terminals_stdin(false),
            "a piped stdin belongs to whatever reads next, not to ssh"
        );
        assert!(
            wants_this_terminals_stdin(true),
            "a terminal has nothing queued on it to lose"
        );
        assert!(
            StackSpec::parse(&argv(&["stack", "deploy", "-c", "-", "st"]), 2).is_err(),
            "and the one spelling that would have read a stream is refused, not carried"
        );
    }

    #[test]
    fn asking_for_help_reads_no_file() {
        let spec = parse(&["stack", "config", "--help", "-c", "sub/stack.yml"]);

        assert!(spec.files.is_empty());
    }

    #[test]
    fn a_stack_that_names_no_compose_file_claims_nothing() {
        let spec = parse(&["stack", "deploy", "mystack"]);

        assert!(spec.files.is_empty(), "docker has no default to guess at");
    }

    #[test]
    fn the_first_compose_file_decides_the_project_directory() {
        // Docker's own rule, measured: with `-c sub/stack.yml -c
        // second.yml`, second.yml's `./second.conf` resolved to
        // sub/second.conf. Ulak has to agree, or the footprint names
        // files the deploy will not look for.
        let (_temp, root) = fixture();
        let project = project_for(&root, &["sub/stack.yml", "second.yml"]);

        assert_eq!(project.inv.project_dir, root.join("sub"));
        assert_eq!(
            project.inv.compose_files,
            vec![root.join("sub/stack.yml"), root.join("second.yml")]
        );
    }

    #[test]
    fn the_compose_files_are_respelled_where_they_landed() {
        // Typed from `sub/`, so the user writes `stack.yml` and
        // `../second.yml`; the anchor is the root above, and the server
        // has no `sub` to be standing in. Neither spelling survives, and
        // that is the point — this used to assert `sub/stack.yml` back
        // out of an argv that already said `sub/stack.yml`, which passes
        // just as well with `rewrite` deleted.
        let (_temp, root) = fixture();
        let mut args = argv(&[
            "stack",
            "deploy",
            "-c",
            "stack.yml",
            "--compose-file=../second.yml",
            "st",
        ]);
        let spec = StackSpec::parse(&args, 2).unwrap();
        let project = project_in(&root.join("sub"), &root, &["stack.yml", "../second.yml"]);

        spec.rewrite(&project, &mut args).unwrap();

        assert_eq!(args[3], "sub/stack.yml");
        assert_eq!(args[4], "--compose-file=second.yml");
    }

    #[test]
    fn an_absolute_compose_file_is_respelled_against_the_workspace() {
        let (_temp, root) = fixture();
        let absolute = root.join("sub/stack.yml");
        let mut args = argv(&["stack", "deploy", "-c", absolute.to_str().unwrap(), "st"]);
        let spec = StackSpec::parse(&args, 2).unwrap();
        let project = project_for(&root, &[absolute.to_str().unwrap()]);

        spec.rewrite(&project, &mut args).unwrap();

        assert_eq!(args[3], "sub/stack.yml", "an absolute path cannot travel");
    }

    /// A stack whose image tag is interpolated, plus whatever `.env`
    /// files the caller asks for.
    fn interpolating(dot_env: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(
            root.join("sub/stack.yml"),
            "services:\n  web:\n    image: app:${TAG}\n",
        )
        .unwrap();
        for dir in dot_env {
            std::fs::write(root.join(dir).join(".env"), "TAG=1.2.3\n").unwrap();
        }
        (temp, root)
    }

    fn invocation(cwd: &Path, files: &[&str]) -> Invocation {
        let mut args = Vec::new();
        for f in files {
            args.push("-f".to_string());
            args.push((*f).to_string());
        }
        Invocation::rebuild_in(cwd, &args, BTreeMap::new()).unwrap()
    }

    #[test]
    fn an_env_file_stack_will_not_read_is_named_instead_of_left_to_surprise() {
        // Measured on 29.4.0: with `TAG=1.2.3` in this exact file,
        // `stack config` printed `image: 'app:'` and said nothing about
        // it. Compose would have read it; stack never does, and the
        // user finds out from "invalid reference format".
        let (_temp, root) = interpolating(&["sub"]);
        let inv = invocation(&root, &["sub/stack.yml"]);

        assert_eq!(unread_env_files(&inv), vec![root.join("sub/.env")]);
    }

    #[test]
    fn a_cwd_and_a_project_directory_that_are_one_place_are_warned_about_once() {
        // Typed from `sub/`, where the compose file also lives: both
        // candidate directories are the same one, and the same file
        // named twice would print the same warning twice.
        let (_temp, root) = interpolating(&["sub"]);
        let inv = invocation(&root.join("sub"), &["stack.yml"]);

        assert_eq!(unread_env_files(&inv), vec![root.join("sub/.env")]);
    }

    #[test]
    fn a_stack_with_nothing_to_interpolate_is_left_alone() {
        // A warning on every project is a warning the user learns to
        // scroll past, so it only fires where it changes the outcome.
        let (_temp, root) = fixture();
        std::fs::write(root.join(".env"), "TAG=1.2.3\n").unwrap();
        std::fs::write(root.join("sub/.env"), "TAG=1.2.3\n").unwrap();
        let inv = invocation(&root, &["sub/stack.yml"]);

        assert!(
            unread_env_files(&inv).is_empty(),
            "`image: nginx` needs no environment"
        );

        // And `$$` is Compose's escape for a literal `$`, so it is not a
        // variable and wants no `.env` either.
        std::fs::write(
            root.join("sub/stack.yml"),
            "services:\n  web:\n    command: echo $${NOT_A_VAR}\n",
        )
        .unwrap();
        assert!(unread_env_files(&invocation(&root, &["sub/stack.yml"])).is_empty());
    }

    #[test]
    fn a_stack_with_no_env_file_anywhere_is_left_alone() {
        let (_temp, root) = interpolating(&[]);
        let inv = invocation(&root, &["sub/stack.yml"]);

        assert!(
            unread_env_files(&inv).is_empty(),
            "there is no file to name, and the variable may well come from the server"
        );
    }

    #[test]
    fn the_env_warning_says_where_the_values_have_to_come_from_instead() {
        // Naming the file without naming the way out leaves the user
        // with a fact and no move.
        assert!(
            UNREAD_ENV_ADVICE.iter().any(|l| l.contains("ssh session")),
            "the environment that decides this is on the server: {UNREAD_ENV_ADVICE:?}"
        );
        assert!(
            UNREAD_ENV_ADVICE.iter().any(|l| l.contains("empty string")),
            "the empty substitution is the part docker's own error hides: {UNREAD_ENV_ADVICE:?}"
        );
        assert!(
            UNREAD_ENV_ADVICE.iter().any(|l| l.contains("export")),
            "and one of them has to be an instruction: {UNREAD_ENV_ADVICE:?}"
        );
    }

    #[test]
    fn the_compose_argv_hands_the_raw_spellings_over_untouched() {
        // What `Invocation` resolves has to be what docker would have
        // resolved: the same strings, against the same cwd.
        let spec = parse(&["stack", "deploy", "-c", "../sibling/stack.yml", "st"]);

        assert_eq!(
            spec.as_compose_argv(),
            vec!["-f".to_string(), "../sibling/stack.yml".to_string()]
        );
    }
}
