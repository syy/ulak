//! Docker-level commands below the explicit `ulak docker` boundary.
//!
//! The public tree mirrors Docker's own tree while each branch declares
//! what it needs from the local filesystem: Compose resolves its model in
//! `passthrough`, build adds its context to the workspace, and daemon-only
//! branches never sync because their inputs already live in Docker.
//!
//! Which branch a command takes is not decided here — it is read from
//! `catalog`, so the routing table and the dispatcher cannot disagree.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::catalog::{self, Resolved, Route};
use crate::config::{Project, Workspace};
use crate::dockerignore::BuildContext;
use crate::footprint::{Entry, Footprint, Why};
use crate::lockfile::WorkspaceLock;
use crate::ssh::{Ssh, sh_quote};
use crate::sync;
use crate::ui::{self, fail};

/// The one door into Docker's tree: resolve what was typed, then hand
/// it to the transport its catalog entry names.
pub fn dispatch(globals: &[String], argv: Vec<OsString>) -> Result<ExitCode> {
    let args = decode_args(argv)?;

    // Our own tree, printed locally: the user is asking what Ulak can
    // carry, which is a question about Ulak and not about the server.
    if let Some(path) = help_request(&args) {
        print!("{}", catalog::help(&path)?);
        return Ok(ExitCode::SUCCESS);
    }

    let resolved = catalog::resolve(&args)?;
    refuse_a_password_on_the_command_line(&resolved)?;
    let branch = resolved.entry.name();
    match resolved.entry.route {
        // A namespace is not a command. With nothing after it, the
        // question is "what lives here?" — with something after it,
        // the something was a flag the path walk could not step over
        // (`buildx --builder mine build .` is a legal docker line), and
        // answering THAT with a listing and exit 0 reads in CI as a
        // green build that produced no image.
        Route::Family
            if resolved
                .tail()
                .iter()
                .all(|a| a == "-h" || a == "--help" || a == "help") =>
        {
            let path: Vec<&str> = resolved.entry.path.to_vec();
            print!("{}", catalog::help(&path)?);
            Ok(ExitCode::SUCCESS)
        }
        Route::Family => {
            let stray = resolved.tail().first().cloned().unwrap_or_default();
            Err(
                fail!("`docker {branch}` is a group of commands, and `{stray}` is not one of them")
                    .now(format!(
                        "a flag here hides the command from Ulak — put it after: ulak docker {branch} <command> {stray} …"
                    ))
                    .now(format!("`ulak docker help {branch}` lists them"))
                    .into_err(),
            )
        }
        Route::Compose => {
            // Compose builds its own `docker compose` prefix on the far
            // side, so anything typed before the word `compose` has no
            // seat on that command line. Refused rather than dropped:
            // a `--log-level debug` that simply vanishes is a debugging
            // session spent wondering why nothing got more verbose.
            if resolved.globals > 0 {
                let stray = resolved.argv[..resolved.globals].join(" ");
                return Err(fail!("`{stray}` cannot come before `compose`")
                    .now(format!(
                        "Compose has its own globals: ulak docker compose {stray} …"
                    ))
                    .into_err());
            }
            let tail = resolved.tail().iter().map(OsString::from).collect();
            crate::passthrough::run(globals, tail)
        }
        Route::Build => {
            reject_compose_globals(globals, &branch)?;
            build(&resolved)
        }
        Route::Bake => {
            reject_compose_globals(globals, &branch)?;
            bake(&resolved)
        }
        Route::Stack => {
            reject_compose_globals(globals, &branch)?;
            crate::stack::run(&resolved)
        }
        // The one arm that forwards argv untouched, which is safe only
        // because `catalog::resolve` has already refused the flags that
        // would open a file HERE — `exec --env-file .env`, `swarm ca
        // --ca-cert`, `imagetools create -f`. That refusal lives in
        // `resolve` rather than here so every caller of it is covered;
        // the test at the bottom of this file pins that the door
        // dispatch walks through is still shut.
        Route::Daemon | Route::Stream => {
            reject_compose_globals(globals, &branch)?;
            let remote = Remote::open()?;
            status_code(remote.docker_with_stdin(
                &resolved.argv,
                None,
                tty_for(&resolved),
                resolved.entry.secret_flags,
                stdin_for(&resolved),
            )?)
        }
        Route::Bridge(kind) => {
            reject_compose_globals(globals, &branch)?;
            crate::bridge::run(&resolved, kind)
        }
        Route::Footprint => {
            reject_compose_globals(globals, &branch)?;
            crate::runspec::run(&resolved)
        }
        Route::Excluded(why) => Err(fail!("`docker {branch}` is not carried to the server")
            .now(why)
            .into_err()),
        Route::Planned(missing) => Err(fail!("`docker {branch}` does not work through Ulak yet")
            .now(missing)
            .now("if this one blocks you, say so — it is mapped, not forgotten")
            .into_err()),
    }
}

/// A secret passed as a flag VALUE cannot be carried safely, so it is
/// refused rather than redacted.
///
/// Redaction was the first answer and it is not enough. It keeps the
/// password out of Ulak's own audit trail, and does nothing about the
/// place it actually ends up: `Remote::spell` builds ONE remote shell
/// command string, so `docker login -p hunter2` is visible in `ps` to
/// every other user on that server for as long as the call runs. Docker
/// warns about `-p` locally for the same reason and offers
/// `--password-stdin`, which travels down the ssh channel and never
/// appears in an argument list at either end.
///
/// The redaction stays: it covers `swarm join --token`, where the value
/// is a cluster join credential that the remote command line genuinely
/// needs.
fn refuse_a_password_on_the_command_line(resolved: &Resolved) -> Result<()> {
    if resolved.entry.path != ["login"] {
        return Ok(());
    }
    // All four spellings pflag accepts, `-phunter2` included — the one
    // that hides the value inside the flag's own word and slips past
    // anything keyed on `=` or on an exact match.
    const PASSWORD: &[&str] = &["-p", "--password"];
    let named = resolved.tail().iter().find_map(|a| {
        let flag = a.split('=').next().unwrap_or(a);
        PASSWORD
            .contains(&flag)
            .then_some(flag)
            .or_else(|| crate::passthrough::attached_short(a, PASSWORD))
    });
    let Some(flag) = named else { return Ok(()) };
    Err(fail!("`docker login {flag}` would put your password in an argument list on the server")
        .now("pipe it instead: echo \"$TOKEN\" | ulak docker login -u <user> --password-stdin")
        .now("Ulak keeps it out of its own audit trail either way — what it cannot hide is `ps` on the far side")
        .into_err())
}

/// `ulak docker`, `ulak docker --help`, `ulak docker help [PATH…]`.
/// Returns the path whose help was asked for.
fn help_request(args: &[String]) -> Option<Vec<&str>> {
    match args.split_first() {
        None => Some(Vec::new()),
        Some((first, rest)) if first == "-h" || first == "--help" => rest.is_empty().then(Vec::new),
        Some((first, rest)) if first == "help" => Some(rest.iter().map(String::as_str).collect()),
        _ => None,
    }
}

/// The only commands where `-t` means "give me a terminal". Everywhere
/// else it means something else entirely: to `docker logs` and `docker
/// service logs` it is `--timestamps`, and to `docker build` it is the
/// image tag. Reading `-t` as a TTY request across the whole tree makes
/// `docker logs -t api > out.log` allocate a pty, and every line in
/// that file then ends CR LF.
const TTY_FLAG_COMMANDS: &[&str] = &["run", "exec"];

/// Commands whose stdio carries a protocol rather than text.
///
/// `dial-stdio` proxies BuildKit's binary wire protocol, so a pty in
/// front of it is not a nicety that goes unused — it translates bytes.
/// These are usually invoked by tooling with pipes on both ends, which
/// is why nobody has hit it; a human running one from a terminal would.
const BINARY_STREAM_COMMANDS: &[&str] = &["dial-stdio"];

/// Whether this call wants a terminal on the far side.
///
/// The default answer — "when both of our ends are terminals" — is
/// right for nearly everything. What it misses is `docker exec -it`
/// with output piped into a pager: Docker was asked for a TTY, and
/// without `ssh -t` there is no terminal for it to allocate.
pub(crate) fn tty_for(resolved: &Resolved) -> Tty {
    let leaf = resolved.entry.path.last().copied().unwrap_or_default();
    if BINARY_STREAM_COMMANDS.contains(&leaf) {
        return Tty::Never;
    }
    if asked_for_a_tty(resolved) && std::io::stdin().is_terminal() {
        Tty::Force
    } else {
        Tty::Auto
    }
}

/// The shorts of `run` and `exec` whose value is the rest of the word,
/// read off `docker run --help` and `docker exec --help` rather than
/// recalled. A cluster ends at the first of these, because from there
/// the characters are that flag's value: `-eMODE=test` is one `-e`, not
/// an `-e` and a TTY request, and `-w/tmp/app` is a working directory.
///
/// Nothing here is `exec`-only or `run`-only on purpose — `exec`'s
/// value shorts (`-e`, `-u`, `-w`) are a subset of `run`'s, so one list
/// reads both correctly and there is no second table to drift.
const VALUE_SHORTS: &[char] = &['a', 'c', 'e', 'h', 'l', 'm', 'p', 'u', 'v', 'w'];

/// Split out from `tty_for` so it can be tested: the terminal check
/// around it is always false under `cargo test`, which would have left
/// the interesting half of this decision unexercised.
fn asked_for_a_tty(resolved: &Resolved) -> bool {
    let leaf = resolved.entry.path.last().copied().unwrap_or_default();
    if !TTY_FLAG_COMMANDS.contains(&leaf) {
        return false;
    }
    // Only Docker's own flags, which stop at the first bare word: for
    // `exec` that word is the container and for `run` it is the image,
    // and everything past it belongs to the program being run. Without
    // this boundary, `docker exec db pg_dump -t users mydb > dump.sql`
    // reads pg_dump's `--table` as a TTY request, gets a pty, and turns
    // every newline in the dump into CR LF — the same corruption
    // bridge.rs allocates `Tty::Never` to avoid, one route over.
    //
    // The boundary is deliberately conservative: a value that does not
    // start with `-` also ends the scan, so `run --name x -it img` is
    // read as not asking. That costs a pty only when stdout is
    // redirected AND a TTY was asked for — an odd pair — while the case
    // it protects is one people type every day.
    let mut asked = false;
    let mut detached = false;
    for arg in resolved
        .tail()
        .iter()
        .take_while(|a| a.starts_with('-') && a.as_str() != "--")
    {
        if let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.starts_with('-')) {
            // A short cluster is booleans until a flag that takes a
            // value, and `-t` is a boolean, so `-it`, `-ti` and `-itd`
            // are read a character at a time. Reading the whole WORD for
            // a `t` instead — which is what this did — turns `docker run
            // -w/tmp/app` and `-eMODE=test` into TTY requests, and a pty
            // the user never asked for ends every line of piped output
            // CR LF.
            //
            // A boolean shorthand may also carry its own value, and the
            // `=` belongs to the letter it FOLLOWS, not to the word.
            // Measured on 29.4.0: `run -t=false`, `-it=false` and
            // `-t=0` all leave the container without a terminal, and
            // `-d=false` runs in the foreground. Walking on past the
            // `=` read the value's own letters as flags, so `-t=false`
            // set `asked` from its `t` and `-i=true` set it from the
            // one inside "true" — a pty forced on for the two spellings
            // that were asking for the opposite, ending every line of
            // redirected output CR LF. `RunSpec::claim` has read the
            // same argv correctly all along (`runspec.rs`), which is
            // the reading borrowed here rather than written twice.
            let mut rest = cluster;
            while let Some(ch) = rest.chars().next() {
                rest = &rest[ch.len_utf8()..];
                if let Some(value) = rest.strip_prefix('=') {
                    asked |= ch == 't' && crate::runspec::flag_bool(value);
                    detached |= ch == 'd' && crate::runspec::flag_bool(value);
                    break;
                }
                asked |= ch == 't';
                detached |= ch == 'd';
                if VALUE_SHORTS.contains(&ch) {
                    break;
                }
            }
            continue;
        }
        // pflag spells a long boolean two ways, and comparing the whole
        // word only ever saw one of them. Measured on 29.4.0: `run
        // --detach=true` detaches and `run --tty=true` allocates a pty,
        // so both are real requests. The pair that cost something is
        // `run -t --detach=true`: `detached` stayed false, the `-d` cut
        // below never fired, and an `ssh -t` was held open for a client
        // that prints an id and exits — with the id CR LF terminated,
        // which is enough to break `id=$(ulak docker run …)`.
        //
        // The same argv is already read correctly one module over, where
        // `RunSpec::claim` splits on `=` before consulting its boolean
        // table. Borrowing that table rather than writing a second one
        // is what keeps the two halves of the same `-t`/`-d` decision
        // from drifting apart.
        let (name, value) = arg.split_once('=').unwrap_or((arg.as_str(), "true"));
        asked |= name == "--tty" && crate::runspec::flag_bool(value);
        detached |= name == "--detach" && crate::runspec::flag_bool(value);
    }
    // `docker run -itd` asks for a container WITH a terminal and then
    // hands it back immediately: the client prints an id and exits.
    // Holding an `ssh -t` open for that puts this terminal in raw mode
    // for a command that never uses it, and the id comes back CR LF
    // terminated — enough to break `id=$(ulak docker run -itd …)`.
    asked && !detached
}

/// Commands whose far side reads stdin, so ssh must be allowed to carry
/// it: the interactive ones, `login`'s prompts and `--password-stdin`,
/// the stdio proxies, and every `prune`, which asks "Are you sure?" and
/// reads the answer from stdin (measured — a `printf 'n\nEXTRA\n' |
/// docker volume prune` consumes both lines).
///
/// `swarm unlock` is the quiet one. It takes the key as no argument at
/// all — there is nowhere for it to arrive but stdin — and when stdin is
/// not a terminal the client stops prompting and reads a line off it.
/// So `echo "$KEY" | ulak docker swarm unlock`, which is how anybody
/// scripts a reboot, handed the far side `/dev/null`: the key never
/// arrived, and the failure looks like a wrong key rather than a missing
/// one.
///
/// Leaf-matched like `TTY_FLAG_COMMANDS`, which is what makes one word
/// cover `system prune`, `image prune`, `buildx prune` and the rest, and
/// `plugin install`/`upgrade`, whose permission grant is the same kind
/// of prompt. `unlock` is one word for the same reason and cannot catch
/// `swarm unlock-key`, which is a different leaf and reads nothing.
const STDIN_COMMANDS: &[&str] = &[
    "attach",
    "dial-stdio",
    "exec",
    "install",
    "login",
    "prune",
    "start",
    "unlock",
    "upgrade",
];

/// Where the remote command's stdin comes from.
///
/// ssh reads its own stdin as soon as the channel is up and forwards it
/// whether or not anything on the far side is listening — measured
/// against the e2e sshd fixture: `printf 'a\nb\n' | { ssh host 'echo
/// hi'; cat; }` leaves `cat` with nothing, while the same pipeline
/// around plain `docker ps` leaves both lines. So `ulak docker ps` in
/// the middle of a shell pipeline silently eats up to an ssh window of
/// whatever the NEXT reader was going to get, which plain docker never
/// does.
///
/// `/dev/null` is given only where both halves are true: this stdin is
/// not a terminal (a terminal has nothing queued to lose, and taking it
/// away would break every prompt), and the command is not one that reads
/// stdin. Getting that second list wrong fails loudly — a prompt that
/// sees EOF says so and aborts — where the swallowing it prevents fails
/// silently.
fn stdin_for(resolved: &Resolved) -> std::process::Stdio {
    if wants_this_terminals_stdin(resolved, std::io::stdin().is_terminal()) {
        std::process::Stdio::inherit()
    } else {
        std::process::Stdio::null()
    }
}

/// Split out from `stdin_for` for the same reason `asked_for_a_tty` is
/// split out of `tty_for`: stdin is never a terminal under `cargo test`.
fn wants_this_terminals_stdin(resolved: &Resolved, stdin_is_a_terminal: bool) -> bool {
    let leaf = resolved.entry.path.last().copied().unwrap_or_default();
    stdin_is_a_terminal || STDIN_COMMANDS.contains(&leaf)
}

/// Whether the far side gets a pseudo-terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tty {
    /// When both of our ends are terminals — the right default.
    Auto,
    /// Never: the streams are binary, or this is a background child.
    Never,
    /// Always: Docker was explicitly asked for one.
    Force,
}

/// A link to the workspace's server.
///
/// Deliberately built from `Workspace::locate` and nothing else: a
/// daemon command needs the workspace's host selection, never a Compose
/// model or a workspace transfer. Object ids, labels and Go templates
/// mean the same thing from any client, and may name resources that do
/// not belong to the workspace at all.
pub struct Remote {
    pub ssh: Ssh,
}

impl Remote {
    pub fn open() -> Result<Remote> {
        Remote::to(&Workspace::locate()?.ssh_dest()?)
    }

    pub fn to(dest: &str) -> Result<Remote> {
        Ok(Remote {
            ssh: Ssh::new(dest)?,
        })
    }

    /// The ssh invocation for one remote `docker …`, unspawned, so a
    /// caller that needs to own a stream (the file bridge) can redirect
    /// it. The second value is the audit-safe twin of the remote
    /// command line — pass it to `record_ssh_audit` after the run.
    pub fn spell(
        &self,
        args: &[String],
        workdir: Option<&str>,
        tty: Tty,
        secret_flags: &[&str],
    ) -> (std::process::Command, String) {
        let head = match workdir {
            Some(dir) => format!("mkdir -p {d} && cd {d} && docker", d = sh_quote(dir)),
            None => "docker".into(),
        };
        let mut remote = head.clone();
        let mut redacted = head;
        for (arg, safe) in args
            .iter()
            .zip(crate::passthrough::redact_argv(args, secret_flags))
        {
            remote.push(' ');
            remote.push_str(&sh_quote(arg));
            redacted.push(' ');
            redacted.push_str(&sh_quote(&safe));
        }
        let mut cmd = self.ssh.command();
        let want_tty = match tty {
            Tty::Force => true,
            Tty::Never => false,
            Tty::Auto => std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        };
        if want_tty {
            cmd.arg("-t");
        }
        cmd.arg("--").arg(&self.ssh.dest).arg(&remote);
        (cmd, redacted)
    }

    /// An ssh invocation running an arbitrary remote shell line.
    ///
    /// The file bridge needs this: `docker cp` on the far side has to be
    /// wrapped in staging and cleanup, which is more than one `docker`
    /// call, and building that string here keeps every remote command
    /// going through the same audit door.
    /// Wrapped in `sh -c '…'` rather than handed to the login shell.
    /// The rest of the crate sends scripts as `sh -s` with the text on
    /// stdin, for the reason ssh.rs writes down: a csh/fish/zsh login
    /// shell then only ever parses two safe words. `sh -s` is not
    /// available here — the bridge's stdin is carrying a tar — so this
    /// gets the same protection the other way round. Without it, `set
    /// -e`, `$( )` and `trap` reach a tcsh login shell, and `docker cp`
    /// dies of a syntax error on exactly the servers ssh.rs went out of
    /// its way to keep working.
    pub fn script(&self, remote: &str, tty: Tty) -> std::process::Command {
        let mut cmd = self.ssh.command();
        let want_tty = match tty {
            Tty::Force => true,
            Tty::Never => false,
            Tty::Auto => std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        };
        if want_tty {
            cmd.arg("-t");
        }
        cmd.arg("--")
            .arg(&self.ssh.dest)
            .arg(format!("sh -c {}", sh_quote(remote)));
        cmd
    }

    /// Spell it, run it inheriting this terminal, audit it.
    pub fn docker(
        &self,
        args: &[String],
        workdir: Option<&str>,
        tty: Tty,
        secret_flags: &[&str],
    ) -> Result<std::process::ExitStatus> {
        self.docker_with_stdin(
            args,
            workdir,
            tty,
            secret_flags,
            std::process::Stdio::inherit(),
        )
    }

    /// The same, for a caller that knows where stdin should come from.
    /// `stdin_for` says why the daemon arm does not simply inherit it.
    pub fn docker_with_stdin(
        &self,
        args: &[String],
        workdir: Option<&str>,
        tty: Tty,
        secret_flags: &[&str],
        stdin: std::process::Stdio,
    ) -> Result<std::process::ExitStatus> {
        let (mut cmd, redacted) = self.spell(args, workdir, tty, secret_flags);
        cmd.stdin(stdin);
        let status = cmd.status().context("cannot spawn ssh for docker")?;
        crate::passthrough::record_ssh_audit(&cmd, &redacted, status.code());
        Ok(status)
    }
}

pub fn build(resolved: &Resolved) -> Result<ExitCode> {
    let mut args = resolved.argv.clone();
    let workspace = Workspace::locate()?;
    let remote = Remote::to(&workspace.ssh_dest()?)?;
    let ssh = remote.ssh.clone();
    let input = BuildInput::parse(&workspace.cwd, &args, resolved.tail_start)?;

    // URL/Git/stdin builds carry no local filesystem CONTEXT. They still
    // target the configured daemon, and the context itself is fetched by
    // the builder, so there is nothing to sync for it — but the rest of
    // the command line can still name this machine, and until now those
    // flags went to the far side unread.
    let Some(input) = input else {
        refuse_local_paths_without_a_context(&workspace.cwd, &args, resolved.tail_start)?;
        return status_code(remote.docker(&args, None, Tty::Auto, &[])?);
    };

    let anchor = crate::invocation::workspace_anchor(
        workspace.workspace_id(),
        build_anchor(&workspace, &input),
    );
    let mut footprint = Footprint {
        anchor: anchor.clone(),
        entries: Vec::new(),
        server_refs: Vec::new(),
        whole_anchor: false,
        contexts: Vec::new(),
        pinned: Vec::new(),
        model_json: String::new(),
    };
    input.extend_footprint(&mut footprint)?;
    input.rewrite_args(&workspace.cwd, &mut args);

    // Everything else a build reads or writes on THIS machine:
    // `--secret src=`, `--ssh k=`, `--build-context`, a local cache, a
    // local `--output`, `--iidfile`, `--metadata-file`. Before this,
    // those flags were forwarded verbatim, so `--secret
    // id=npm,src=./.npmrc` handed the build whatever `./.npmrc` the ssh
    // session's directory happened to contain — usually nothing — and
    // `--output type=local,dest=./dist` left the artefacts in a
    // directory on the server the user never sees.
    let extra = crate::buildflags::scan(&workspace.cwd, &args, resolved.tail_start)?;
    let (carried, elsewhere): (Vec<_>, Vec<_>) = extra
        .into_iter()
        .partition(|i| i.path.starts_with(&footprint.anchor));
    for input in &elsewhere {
        // Not widened to cover it: the anchor decides the remote layout,
        // so one `--secret src=/etc/ssl/key` would move the whole
        // workspace to the root of the filesystem.
        ui::warn(&format!(
            "{} ({}) is outside this workspace, so it is read on the server, not here",
            input.path.display(),
            input.flag
        ));
    }
    let mut outputs = Outputs::new();
    carry_build_inputs(&carried, &mut footprint, &mut outputs);
    footprint.pinned.sort();
    footprint.pinned.dedup();
    crate::buildflags::rewrite(&mut args, &carried, |i| {
        relative_from(&workspace.cwd, &i.path)
    });

    let project = workspace.into_project(anchor);

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

    let workdir = remote_workdir(&project)?;
    let status = remote.docker(&args, Some(&workdir), Tty::Auto, &[])?;
    if status.success() {
        outputs.keep();
    }

    // Output files written inside the synced context come home, but a
    // pull problem must never replace Docker's own exit code.
    if let Err(e) = sync::pull_back(
        &project,
        &footprint,
        &ssh,
        true,
        &crate::proc::Budget::new(crate::proc::RECONCILE),
        None,
    ) {
        ui::render_error(&e);
    }

    status_code(status)
}

/// `docker bake` — many builds from one file, each with its own context.
///
/// Bake resolves every relative path against the process working
/// directory (measured: not against the bake file's own directory, even
/// with two `-f` files in different places), and the remote workspace
/// reproduces the local layout under the anchor. Definition flags are
/// canonicalised to a relative spelling so absolute `-f` paths and local
/// symlinks obey that same rule on the server.
///
/// What the plan is for is knowing WHICH paths those are. Bake files are
/// HCL with functions, variables, inheritance and group expansion; the
/// only honest way to know what a bake would read is to ask Buildx. The
/// server's Buildx answers from a small bootstrap mirror before the
/// workspace is synced.
fn bake(resolved: &Resolved) -> Result<ExitCode> {
    let mut args = resolved.argv.clone();
    let workspace = Workspace::locate()?;
    let remote = Remote::to(&workspace.ssh_dest()?)?;
    let ssh = remote.ssh.clone();
    let plan = match crate::bake::resolve(
        &workspace.cwd,
        &args,
        resolved.tail_start,
        &ssh,
        &workspace.remote_workspace_root(),
    )? {
        crate::bake::Resolution::Forward => {
            return status_code(remote.docker(&args, None, Tty::Auto, &[])?);
        }
        crate::bake::Resolution::Answer(answer) => return answer.emit(),
        crate::bake::Resolution::Plan(plan) => plan,
    };
    crate::bake::rewrite_definition_args(&workspace.cwd, &mut args, resolved.tail_start)?;

    let anchor =
        crate::invocation::workspace_anchor(workspace.workspace_id(), workspace.root.clone());
    let mut footprint = Footprint {
        anchor: anchor.clone(),
        entries: Vec::new(),
        server_refs: Vec::new(),
        whole_anchor: false,
        contexts: Vec::new(),
        pinned: Vec::new(),
        model_json: String::new(),
    };

    let mut outside: Vec<String> = Vec::new();
    let mut outputs = Outputs::new();
    let mut add_dir = |fp: &mut Footprint,
                       path: &Path,
                       why: Why,
                       service: &str,
                       writable: bool,
                       known_dir: bool| {
        if !path.starts_with(&anchor) {
            // The anchor is not widened to reach it: the anchor decides
            // the remote layout, so one `cache_from: /var/cache/x` would
            // move the whole workspace to the root of the filesystem.
            outside.push(format!("{} ({service})", path.display()));
            return;
        }
        if fp.entries.iter().any(|e| e.local == *path) {
            return;
        }
        let is_dir = known_dir || path.is_dir();
        fp.entries.push(Entry {
            local: path.to_path_buf(),
            is_dir,
            exists: path.exists(),
            empty: is_dir && std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_none()),
            why,
            service: service.to_string(),
            writable,
        });
        if !is_dir {
            fp.pinned.push(path.to_path_buf());
        }
    };

    for file in &plan.files {
        add_dir(
            &mut footprint,
            file,
            Why::ComposeFile,
            "bake file",
            false,
            false,
        );
    }
    for target in &plan.targets {
        let who = format!("bake {}", target.name);
        if let Some(context) = &target.context {
            add_dir(&mut footprint, context, Why::Build, &who, false, false);
            if context.is_dir() && context.starts_with(&anchor) {
                footprint.contexts.push(BuildContext {
                    root: context.clone(),
                    dockerfile: target.dockerfile.clone(),
                });
                footprint.whole_anchor |= *context == anchor;
            }
        }
        if let Some(dockerfile) = &target.dockerfile {
            add_dir(&mut footprint, dockerfile, Why::Build, &who, false, false);
        }
        for (name, path) in &target.contexts {
            add_dir(
                &mut footprint,
                path,
                Why::Build,
                &format!("{who} ({name})"),
                false,
                false,
            );
        }
        for path in &target.reads {
            add_dir(&mut footprint, path, Why::Build, &who, false, false);
        }
        for write in &target.writes {
            // The plan says whether this is a directory, and it has to:
            // a local output does not exist yet, so asking the
            // filesystem answers "not a directory" and rsync is handed a
            // file rule for a tree. Measured — `dist/alpha` got `+
            // dist/alpha` instead of `+ dist/alpha/` plus `+
            // dist/alpha/***`, so nothing came home. An `|| is_dir()`
            // still covers a directory that happens to exist already.
            if write.path.starts_with(&anchor) {
                outputs.make_room(&write.path, write.is_dir);
            }
            add_dir(
                &mut footprint,
                &write.path,
                Why::Build,
                &who,
                true,
                write.is_dir,
            );
        }
    }
    if let Some(meta) = bake_metadata_file(
        &workspace.cwd,
        &args,
        resolved.tail_start,
        &anchor,
        &mut outputs,
    ) {
        add_dir(
            &mut footprint,
            &meta,
            Why::Build,
            "--metadata-file",
            true,
            false,
        );
    }
    for path in &outside {
        ui::warn(&format!(
            "{path} is outside this workspace, so bake reads it on the server, not here"
        ));
    }
    footprint.contexts.sort_by(|a, b| a.root.cmp(&b.root));
    footprint.contexts.dedup();
    footprint.pinned.sort();
    footprint.pinned.dedup();

    let project = workspace.into_project(anchor);
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

    let workdir = remote_workdir(&project)?;
    let status = remote.docker(&args, Some(&workdir), Tty::Auto, &[])?;
    if status.success() {
        outputs.keep();
    }

    // Local outputs and the metadata file come home. Never fatal: the
    // exit code the user asked about is Buildx's.
    if let Err(e) = sync::pull_back(
        &project,
        &footprint,
        &ssh,
        true,
        &crate::proc::Budget::new(crate::proc::RECONCILE),
        None,
    ) {
        ui::render_error(&e);
    }
    status_code(status)
}

fn build_anchor(workspace: &Workspace, input: &BuildInput) -> PathBuf {
    let mut paths = vec![workspace.root.as_path()];
    paths.push(if input.context.is_dir() {
        input.context.as_path()
    } else {
        input.context.parent().unwrap_or(input.context.as_path())
    });
    if let Some((_, _, dockerfile)) = &input.dockerfile {
        paths.push(dockerfile.parent().unwrap_or(dockerfile.as_path()));
    }
    crate::invocation::common_ancestor(&paths)
}

fn reject_compose_globals(globals: &[String], branch: &str) -> Result<()> {
    if globals.is_empty() {
        return Ok(());
    }
    Err(
        fail!("Compose project flags do not apply to `ulak docker {branch}`")
            .now(format!(
                "put Docker's own flags after the command: ulak docker {branch} …"
            ))
            .into_err(),
    )
}

/// The local output paths this run brought into being so the far side's
/// results would have somewhere to land — and their way back out if
/// nothing ever landed.
///
/// Why they have to exist first: `footprint::filter_rules` skips an
/// entry that is not here, and it is right to. The rule chain is
/// identical in both directions, so a rule that carried a
/// not-yet-existing directory home would carry a database home with it —
/// the measured case is a stack naming `./volumes/db/data`. `runspec`
/// leaves a missing bind source alone for exactly that reason and warns
/// instead. A BUILD OUTPUT is the one place where that ambiguity does
/// not exist: `--output type=local,dest=./dist` says where the artefacts
/// go and nothing else could be meant, and a local build has Docker
/// create the directory itself. Measured before any of this existed:
/// `dist/alpha` got the rsync rule `+ dist/alpha` — a FILE rule, because
/// it was not there to be seen as a directory — so rsync never descended
/// and the build's output stayed on the server.
///
/// Why a guard rather than a create call and a cleanup call: the cleanup
/// used to sit behind `if !status.success()`, which every early return
/// between the two skipped — a footprint error, a lock that cannot be
/// taken, a sync that fails — each of them leaving a zero-byte
/// `metadata.json` or an empty `dist/` in the user's tree, where a
/// plain `docker build` that failed the same way leaves nothing at all
/// (measured: a failing build writes neither `--metadata-file` nor
/// `--iidfile` nor the `--output` directory). `Drop` covers all of them,
/// and it runs after the pull, so what it sees is the final state.
///
/// It cannot take back a real output, and that is the point of both
/// halves: a FILE goes only while it is still zero bytes, and a real one
/// never is — a successful `--metadata-file` is 850 bytes of JSON here,
/// and buildx writes it only on success. A DIRECTORY goes through
/// `remove_dir`, which refuses a directory with anything in it, so an
/// output that did come home is never the thing that gets deleted.
///
/// What it cannot cover: `Drop` does not run on `std::process::exit`,
/// and it does not run on the Ctrl-C that kills this process either, so
/// an interrupted build still leaves its placeholder behind. Unlike
/// bridge.rs's staging directories these have ordinary user-chosen
/// names, so there is nothing a later run could safely sweep — an empty
/// `dist/` is indistinguishable from one the user made.
struct Outputs {
    made: Vec<Made>,
    keep: bool,
}

/// One path Ulak created, and how it is taken back.
enum Made {
    File(PathBuf),
    Dir(PathBuf),
}

impl Outputs {
    fn new() -> Outputs {
        Outputs {
            made: Vec::new(),
            keep: false,
        }
    }

    /// Make `path` exist, so rsync can see what shape it is.
    fn make_room(&mut self, path: &Path, is_dir: bool) {
        // `symlink_metadata`, not `exists`: `exists` follows the link,
        // so a symlink pointing at a file that is not there reads as
        // absent — and `File::create` then follows that SAME link and
        // writes the placeholder at its target, outside the workspace,
        // after which taking it back removes the user's link and leaves
        // the stray file. Whatever is already at this path is the
        // user's, live link or dangling one, so it is left alone.
        if let Ok(here) = path.symlink_metadata() {
            if here.is_symlink() && !path.exists() {
                ui::warn(&format!(
                    "{} is a symlink to something that is not here, so its output cannot be prepared — it will stay on the server",
                    path.display()
                ));
            }
            return;
        }
        // Every directory that has to be brought into being is recorded
        // too, not just the output itself: `--metadata-file
        // build/out/meta.json` otherwise leaves `build/out/` behind on a
        // build that wrote nothing into it.
        let deepest = if is_dir { Some(path) } else { path.parent() };
        for dir in deepest.map(missing_dirs).unwrap_or_default() {
            if std::fs::create_dir(&dir).is_err() {
                return;
            }
            self.made.push(Made::Dir(dir));
        }
        if !is_dir && std::fs::File::create(path).is_ok() {
            self.made.push(Made::File(path.to_path_buf()));
        }
    }

    /// The command succeeded: everything here is now the user's, empty
    /// or not.
    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for Outputs {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        // Reverse order, so a file goes before the directory holding it
        // and a nested directory before its parent — `remove_dir` only
        // ever succeeds on the empty one anyway, and this is what lets
        // the whole chain go when nothing arrived.
        for made in self.made.iter().rev() {
            match made {
                Made::File(path) => {
                    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() == 0) {
                        let _ = std::fs::remove_file(path);
                    }
                }
                Made::Dir(path) => {
                    let _ = std::fs::remove_dir(path);
                }
            }
        }
    }
}

/// The ancestors of `dir` that are not there yet, shallowest first —
/// which is both the order they have to be created in and, reversed, the
/// order they can be removed in.
fn missing_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut at = Some(dir);
    while let Some(path) = at {
        if path.symlink_metadata().is_ok() {
            break;
        }
        missing.push(path.to_path_buf());
        at = path.parent();
    }
    missing.reverse();
    missing
}

/// Everything `buildflags` found, as footprint entries — with room made
/// first for the ones the build is going to write.
///
/// A READ path that is not here is deliberately NOT an error. This used
/// to refuse one, which reads as caution and is not: `buildflags`
/// already refuses the reads Docker itself stats — one flag at a time,
/// measured — and the only path that reaches here missing is the one it
/// deliberately lets through. A `--cache-from type=local,src=./cold`
/// pointing at a directory that does not exist is a cache MISS to
/// Docker, not a failure (measured on 29.4.0: "local cache import at
/// /nonexistent/cachedir skipped due to err", then the build runs and
/// exits 0), so refusing it here turned a build that works without Ulak
/// into one that does not. Left alone, the entry is marked absent, gets
/// no rsync rule, and the remote build misses the same cache this one
/// would have.
fn carry_build_inputs(
    carried: &[crate::buildflags::LocalInput],
    footprint: &mut Footprint,
    outputs: &mut Outputs,
) {
    for input in carried {
        let is_dir = input.shape == crate::buildflags::Shape::Dir;
        if input.direction == crate::buildflags::Direction::Write {
            outputs.make_room(&input.path, is_dir);
        }
        footprint.entries.push(Entry {
            local: input.path.clone(),
            is_dir: is_dir || input.path.is_dir(),
            exists: input.path.exists(),
            empty: false,
            why: Why::Build,
            service: format!("docker build {}", input.flag),
            writable: input.direction == crate::buildflags::Direction::Write,
        });
        footprint.pinned.push(input.path.clone());
    }
}

/// The `--metadata-file` a bake writes, made to exist so it can come
/// home on the run that creates it.
///
/// Without the placeholder the entry is absent, `filter_rules` skips it,
/// and the file arrives one bake late: the user reads the answer from
/// the PREVIOUS run and never knows. The target outputs beside it have
/// had this since they were written; this one was added later and did
/// not.
fn bake_metadata_file(
    cwd: &Path,
    args: &[String],
    tail_start: usize,
    anchor: &Path,
    outputs: &mut Outputs,
) -> Option<PathBuf> {
    let meta = crate::bake::metadata_file(cwd, args, tail_start)?;
    // Outside the anchor there is nothing to carry it in, and the caller
    // says so; creating a file out there would only litter.
    if meta.starts_with(anchor) {
        outputs.make_room(&meta, false);
    }
    Some(meta)
}

/// A build whose context is a URL, a git repository or stdin can still
/// name this machine's files with its other flags, and there is no
/// workspace to carry them in — the anchor is derived from the context,
/// and this build has none.
///
/// Forwarded, `--secret id=npm,src=./.npmrc` hands the build whatever
/// `./.npmrc` the ssh session's home directory happens to hold, and
/// `--output type=local,dest=./dist` leaves the artefacts in a directory
/// on the server the user never looks in. Both are the silent kind of
/// wrong, so they are refused by name — the same trade the catalog makes
/// for a command it does not know: a minute spent reading an error beats
/// an afternoon spent on the wrong machine's filesystem.
fn refuse_local_paths_without_a_context(
    cwd: &Path,
    args: &[String],
    tail_start: usize,
) -> Result<()> {
    let Some(input) = crate::buildflags::scan(cwd, args, tail_start)?
        .into_iter()
        .next()
    else {
        return Ok(());
    };
    let side = if input.direction == crate::buildflags::Direction::Read {
        "the server would read its own"
    } else {
        "the build would write the server's own"
    };
    Err(fail!(
        "{} names {}, and this build's context is not a local directory",
        input.flag,
        input.path.display()
    )
    .now(format!(
        "Ulak carries a build's local files under its context, and a URL, git or stdin context gives it none — so {side}"
    ))
    .now(
        "build from a local context, or run this build on the server itself over ssh — \
         `ulak status` names the host this workspace is bound to",
    )
    .into_err())
}

/// Where a command runs on the server: the remote mirror of the
/// directory the user is standing in.
///
/// `pub(crate)` because more than one engine needs it now, and this
/// encodes a contract — "remote minus workspace equals local minus
/// anchor" — that two copies would eventually disagree about.
pub(crate) fn remote_workdir(project: &Project) -> Result<String> {
    let rel = project.remote_rel(&project.inv.cwd)?;
    Ok(if rel == "." {
        project.remote_dir()
    } else {
        format!("{}/{rel}", project.remote_dir())
    })
}

/// Local inputs read by the basic `docker build [OPTIONS] CONTEXT`
/// shape. Docker requires the positional context last, which lets us
/// preserve every unrelated flag without trying to own Docker's parser.
#[derive(Debug)]
struct BuildInput {
    context_index: usize,
    context: PathBuf,
    /// Which argv word holds the Dockerfile path, the byte range inside
    /// it, and where that resolves here. The range is carried rather
    /// than the spelling because `-f`, `-fX`, `-f=X`, `--file X` and
    /// `--file=X` are all the same flag and all have to be respelled.
    dockerfile: Option<(usize, std::ops::Range<usize>, PathBuf)>,
}

impl BuildInput {
    /// `None` means there is no local context to sync: stdin, a URL/Git
    /// context, or Docker's own help output.
    ///
    /// `tail_start` is where the command's own arguments begin — 1 for
    /// `build`, 2 for `image build`, 2 for `buildx build`. Everything
    /// before it is the command path and must never be mistaken for a
    /// context or read as a flag.
    fn parse(cwd: &Path, args: &[String], tail_start: usize) -> Result<Option<BuildInput>> {
        if crate::buildflags::help_wanted(args, tail_start) {
            return Ok(None);
        }
        // The context is the one POSITIONAL, not the last word. This
        // used to take `args.last()`, on the premise that Docker
        // requires the context last — it does not. pflag allows
        // interspersed flags, so `docker build . -t api` is legal and
        // people write it, and the old rule read `api` as the context:
        // in a repo that happens to have an `api/` directory, Ulak
        // synced the wrong tree and built it, silently.
        let positionals = crate::buildflags::positionals(args, tail_start);
        let context_index = match positionals.as_slice() {
            [only] => *only,
            [] => {
                return Err(fail!("docker build needs a context")
                    .now("for example: ulak docker build -t api .")
                    .into_err());
            }
            many => {
                let words: Vec<&str> = many.iter().map(|i| args[*i].as_str()).collect();
                return Err(fail!(
                    "docker build takes one context, and this names {}: {}",
                    many.len(),
                    words.join(" ")
                )
                .now("if one of these is a flag's value, Ulak does not know that flag takes one — report it")
                .into_err());
            }
        };
        let raw_context = &args[context_index];
        if raw_context == "-" || remote_context(raw_context) {
            return Ok(None);
        }
        let context = local_path(cwd, raw_context, "build context")?;

        let mut dockerfile = None;
        if let Some(found) = crate::buildflags::dockerfile(args, tail_start) {
            let Some((index, span)) = found.at else {
                return Err(fail!("{} needs a Dockerfile path", found.flag)
                    .now("for example: ulak docker build -f docker/Dockerfile .")
                    .into_err());
            };
            let raw = &args[index][span.clone()];
            // `-f -` reads the Dockerfile from stdin, and `--file=` with
            // nothing after it asks for the default (measured: it builds
            // from `Dockerfile`). Neither names a path to carry.
            if !raw.is_empty() && raw != "-" {
                dockerfile = Some((index, span, local_path(cwd, raw, "Dockerfile")?));
            }
        }

        Ok(Some(BuildInput {
            context_index,
            context,
            dockerfile,
        }))
    }

    fn extend_footprint(&self, fp: &mut Footprint) -> Result<()> {
        require_inside(&fp.anchor, &self.context, "build context")?;
        let is_dir = self.context.is_dir();
        push_entry(fp, self.context.clone(), is_dir);
        if is_dir {
            fp.contexts.push(BuildContext {
                root: self.context.clone(),
                dockerfile: self.dockerfile.as_ref().map(|(_, _, p)| p.clone()),
            });
            fp.contexts.sort_by(|a, b| a.root.cmp(&b.root));
            fp.contexts.dedup();
            fp.whole_anchor |= self.context == fp.anchor;
        }

        if let Some((_, _, dockerfile)) = &self.dockerfile {
            require_inside(&fp.anchor, dockerfile, "Dockerfile")?;
            push_entry(fp, dockerfile.clone(), false);
            fp.pinned.push(dockerfile.clone());
            fp.pinned.sort();
            fp.pinned.dedup();
        }
        Ok(())
    }

    fn rewrite_args(&self, cwd: &Path, args: &mut [String]) {
        args[self.context_index] = relative_from(cwd, &self.context);
        // Spliced into the span rather than rebuilt from a spelling: the
        // word may be `-fPATH` or `-qf=PATH` as easily as a bare value,
        // and a rewrite that assumed one of the five put the path back
        // in a shape Docker reads as something else.
        if let Some((index, span, dockerfile)) = &self.dockerfile {
            let path = relative_from(cwd, dockerfile);
            args[*index].replace_range(span.clone(), &path);
        }
    }
}

fn push_entry(fp: &mut Footprint, local: PathBuf, is_dir: bool) {
    if fp
        .entries
        .iter()
        .any(|e| e.local == local && e.is_dir == is_dir)
    {
        return;
    }
    let empty = is_dir && std::fs::read_dir(&local).is_ok_and(|mut d| d.next().is_none());
    fp.entries.push(Entry {
        local,
        is_dir,
        exists: true,
        empty,
        why: Why::Build,
        service: "docker build".into(),
        writable: false,
    });
}

fn require_inside(anchor: &Path, path: &Path, label: &str) -> Result<()> {
    if path.starts_with(anchor) {
        return Ok(());
    }
    Err(fail!(
        "the {label} {} sits outside this project's workspace anchor {}",
        path.display(),
        anchor.display()
    )
    .now("move the context under the project, or declare it as a compose build context")
    .into_err())
}

fn local_path(cwd: &Path, raw: &str, label: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    path.canonicalize()
        .with_context(|| format!("{label} {} cannot be read", path.display()))
}

fn remote_context(raw: &str) -> bool {
    raw.contains("://") || raw.starts_with("git@")
}

fn relative_from(from: &Path, target: &Path) -> String {
    let common = crate::invocation::common_ancestor(&[from, target]);
    let mut out = PathBuf::new();
    if let Ok(up) = from.strip_prefix(&common) {
        for _ in up.components() {
            out.push("..");
        }
    }
    if let Ok(down) = target.strip_prefix(&common) {
        out.push(down);
    }
    if out.as_os_str().is_empty() {
        ".".into()
    } else {
        out.to_string_lossy().into_owned()
    }
}

pub fn status_code(status: std::process::ExitStatus) -> Result<ExitCode> {
    Ok(ExitCode::from(shell_code(status)))
}

/// What a shell would have reported for this child, so that whatever ran
/// `ulak` reads the same number it would have read from `docker`.
///
/// A child that a signal killed has no exit code of its own, and every
/// shell and CI runner fills that in the same way: 128 plus the signal's
/// number. Ulak used to answer a flat 130 for all of them, which is
/// SIGINT's number told about every signal — `ulak docker save img |
/// head -c 100` closes the pipe under the child ssh, `Command` restores
/// SIGPIPE to SIG_DFL in the child so ssh dies of it, and the caller was
/// told the user had pressed Ctrl-C. A number that fits in no u8 is not
/// a signal any platform sends, and 130 stays the answer there rather
/// than a wrapped one.
fn shell_code(status: std::process::ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt as _;
    match status.code() {
        Some(code) => u8::try_from(code).unwrap_or(1),
        None => status
            .signal()
            .and_then(|signo| u8::try_from(128 + signo).ok())
            .unwrap_or(130),
    }
}

fn decode_args(argv: Vec<OsString>) -> Result<Vec<String>> {
    argv.into_iter()
        .map(|arg| {
            arg.into_string().map_err(|bad| {
                fail!("argument {:?} is not valid UTF-8", bad)
                    .now("re-run with UTF-8 arguments")
                    .into_err()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(args: &[&str]) -> Resolved {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        catalog::resolve(&argv).unwrap()
    }

    #[test]
    fn only_the_commands_where_dash_t_means_a_terminal_get_one() {
        for wants in [
            vec!["run", "-it", "alpine", "sh"],
            vec!["run", "--tty", "-i", "alpine", "sh"],
            vec!["exec", "-it", "web", "bash"],
            vec!["container", "exec", "-ti", "web", "bash"],
            // `-d=false` is a run that stays in the FOREGROUND —
            // measured on 29.4.0, it prints no id — so the `-t` before
            // it is a live request. Read as another `d`, it cancelled
            // the very terminal it was asking to keep.
            vec!["run", "-t", "-d=false", "alpine", "sh"],
            vec!["run", "-itd=false", "alpine", "sh"],
        ] {
            assert!(
                asked_for_a_tty(&resolved(&wants)),
                "{wants:?} explicitly asked Docker for a TTY"
            );
        }

        for does_not in [
            // `-t` here is --timestamps. Reading it as a TTY request
            // makes `docker logs -t api > out.log` end every line CR LF.
            vec!["logs", "-t", "api"],
            vec!["service", "logs", "-t", "api"],
            // `--tail` is a long flag, so the bundle rule must not see
            // the `t` in it.
            vec!["logs", "--tail", "50", "api"],
            vec!["exec", "-i", "web", "sh"],
            vec!["ps", "-a"],
            vec!["run", "--rm", "alpine", "true"],
            // Every other live stream on the tree: none of these has a
            // `-t` that means a terminal, and `stats` and `events` hold
            // the terminal for as long as they run.
            vec!["stats"],
            vec!["events", "--since", "1h"],
            vec!["attach", "web"],
            vec!["logs", "-f", "api"],
            // A short flag's VALUE is not a flag. Both of these are
            // ordinary lines — measured on 29.4.0, `-w/tmp` sets the
            // working directory and `-eMODE=test` sets the variable —
            // and reading the whole word for a `t` gave both of them a
            // pty nobody asked for.
            vec!["run", "-w/tmp/app", "alpine", "pwd"],
            vec!["run", "-eMODE=test", "alpine", "env"],
            vec!["exec", "-uroot", "web", "sh"],
            // A boolean shorthand carrying its own value, which is a
            // request for the OPPOSITE. Measured on 29.4.0: `-t=false`,
            // `-it=false` and `-t=0` all leave the container without a
            // terminal. Walking past the `=` read the value's letters
            // as flags, so these three forced the pty they were turning
            // off — and `-i=true`, which asks for no terminal at all,
            // got one from the `t` inside "true".
            vec!["run", "-t=false", "alpine", "true"],
            vec!["run", "-it=false", "alpine", "true"],
            vec!["run", "-t=0", "alpine", "true"],
            vec!["run", "-i=true", "alpine", "cat"],
        ] {
            assert!(
                !asked_for_a_tty(&resolved(&does_not)),
                "{does_not:?} did not ask for a TTY"
            );
        }
    }

    /// This one had a test asserting the OPPOSITE, which is worse than
    /// having none: `docker run -itd` was pinned as a TTY request.
    ///
    /// It is not one. `-d` hands the container straight back — the
    /// client prints an id and exits — so an `ssh -t` around it puts
    /// this terminal into raw mode for a command that never reads it,
    /// and the id comes back CR LF terminated, which is enough to break
    /// `id=$(ulak docker run -itd …)`. The `-t` still reaches Docker and
    /// the container still gets its pty; what it no longer does is hold
    /// one open on the way there.
    #[test]
    fn a_detached_run_asks_for_no_terminal_of_its_own() {
        for detached in [
            vec!["run", "-itd", "alpine"],
            vec!["run", "-dit", "alpine"],
            vec!["run", "-t", "-d", "alpine"],
            vec!["run", "--tty", "--detach", "alpine"],
            vec!["container", "run", "-itd", "alpine"],
            // pflag's other spelling of the same booleans. Measured on
            // 29.4.0: `--detach=true` detaches and `--tty=true`
            // allocates a pty, so these are the same two requests in
            // longhand — and comparing the whole word saw neither, which
            // left the `-d` cut below unfired on the first line here.
            vec!["run", "-t", "--detach=true", "alpine"],
            vec!["run", "--tty=true", "--detach=true", "alpine"],
            vec!["run", "--tty=1", "-d", "alpine"],
            // `--tty=false` asks for no terminal at all, so it is not a
            // request that `-d` has to cancel — it was never made.
            vec!["run", "--tty=false", "alpine"],
            vec!["run", "-t", "--detach=1", "alpine"],
        ] {
            assert!(
                !asked_for_a_tty(&resolved(&detached)),
                "{detached:?} runs in the background and must not hold this terminal"
            );
        }
    }

    #[test]
    fn stdin_is_only_carried_to_a_command_that_reads_it() {
        // Measured against the e2e sshd fixture: ssh drains its own
        // stdin as soon as the channel is up, so `printf 'a\nb\n' | {
        // ulak docker ps; cat; }` leaves `cat` with nothing where plain
        // `docker ps` leaves both lines.
        for reads in [
            vec!["exec", "-i", "web", "sh"],
            vec!["attach", "web"],
            vec!["start", "-i", "web"],
            vec!["login", "-u", "me", "--password-stdin"],
            vec!["system", "dial-stdio"],
            // The prune family asks "Are you sure?" and reads the answer
            // from stdin — measured, it consumes the whole pipe.
            vec!["system", "prune"],
            vec!["buildx", "prune"],
            vec!["volume", "prune"],
            vec!["plugin", "install", "vieux/sshfs"],
            // The key has no argument to arrive in, so stdin is the
            // only door. Measured on 29.4.0 against a stub daemon
            // reporting a locked swarm: with a pipe on stdin the client
            // stops prompting and reads the key off it, so `echo "$KEY"
            // | ulak docker swarm unlock` handed the far side
            // /dev/null and failed as though the key were wrong.
            vec!["swarm", "unlock"],
        ] {
            assert!(
                wants_this_terminals_stdin(&resolved(&reads), false),
                "{reads:?} reads stdin, so a pipeline into it must still arrive"
            );
        }

        for does_not in [
            vec!["ps"],
            vec!["logs", "-f", "api"],
            vec!["stats"],
            vec!["events"],
            vec!["inspect", "web"],
            vec!["image", "ls"],
            // Its neighbour, which PRINTS the key and reads nothing —
            // a different leaf, so one word cannot cover both.
            vec!["swarm", "unlock-key"],
        ] {
            assert!(
                !wants_this_terminals_stdin(&resolved(&does_not), false),
                "{does_not:?} never reads stdin, and ssh would swallow it"
            );
            // With a terminal on this end there is nothing queued to
            // lose, and taking it away would silence anything that
            // decides to ask a question.
            assert!(
                wants_this_terminals_stdin(&resolved(&does_not), true),
                "{does_not:?} must keep a terminal's stdin"
            );
        }
    }

    /// The dispatcher's only door is `catalog::resolve`, and the
    /// `Daemon`/`Stream` arm behind it forwards argv untouched. If the
    /// client-path refusal ever leaves `resolve` without being wired in
    /// here, `ulak docker exec --env-file .env web sh` starts reading
    /// the SERVER's `.env`, silently — so the door is pinned from this
    /// side too.
    #[test]
    fn a_flag_that_opens_a_file_here_never_reaches_the_daemon_arm() {
        for (refused, flag) in [
            (
                vec!["exec", "--env-file", ".env", "web", "sh"],
                "--env-file",
            ),
            (
                vec!["container", "exec", "--env-file=.env", "web", "sh"],
                "--env-file",
            ),
            (vec!["swarm", "ca", "--ca-cert", "./ca.pem"], "--ca-cert"),
            (
                vec!["buildx", "imagetools", "create", "-f", "./desc.json", "img"],
                "-f",
            ),
        ] {
            let argv: Vec<String> = refused.iter().map(|s| s.to_string()).collect();
            let err = catalog::resolve(&argv)
                .expect_err(&format!("{refused:?} names a file on this machine"))
                .to_string();
            // The flag, because that is the word on the user's screen —
            // an error that only says "something here is local" leaves
            // them to find which.
            assert!(
                err.contains(flag),
                "{refused:?} was refused without naming the flag: {err}"
            );
        }
        // And the same commands without that flag still reach the arm.
        for carried in [
            vec!["exec", "-it", "web", "sh"],
            vec!["swarm", "ca", "--rotate"],
        ] {
            let argv: Vec<String> = carried.iter().map(|s| s.to_string()).collect();
            assert!(catalog::resolve(&argv).is_ok(), "{carried:?} is carriable");
        }
    }

    #[test]
    fn build_context_and_file_are_found_and_rewritten() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("app/docker")).unwrap();
        std::fs::write(temp.path().join("app/docker/Devfile"), "FROM scratch\n").unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let mut args = vec![
            "build".into(),
            "-f".into(),
            "app/docker/Devfile".into(),
            "-t".into(),
            "demo".into(),
            "app".into(),
        ];
        let input = BuildInput::parse(&cwd, &args, 1).unwrap().unwrap();
        assert_eq!(input.context, cwd.join("app"));
        assert_eq!(
            input.dockerfile.as_ref().unwrap().2,
            cwd.join("app/docker/Devfile")
        );
        input.rewrite_args(&cwd, &mut args);
        assert_eq!(args[2], "app/docker/Devfile");
        assert_eq!(args[5], "app");
    }

    /// The spellings pflag accepts for `-f` that a literal match misses.
    ///
    /// Measured on Buildx 0.33.0, every line here builds. The one that
    /// made this an invariant violation rather than an inconvenience is
    /// the escaping relative path: with `-f` unread, the Dockerfile
    /// never joined the footprint, never widened the anchor and never
    /// got respelled, so the argv reached the far side untouched and the
    /// SERVER opened its own `../shared/Dockerfile`.
    #[test]
    fn every_spelling_of_the_dockerfile_flag_is_read_and_respelled() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("repo/ctx")).unwrap();
        std::fs::create_dir_all(temp.path().join("repo/shared")).unwrap();
        std::fs::write(temp.path().join("repo/shared/Dockerfile"), "FROM scratch\n").unwrap();
        let root = temp.path().join("repo").canonicalize().unwrap();
        let cwd = root.join("ctx");

        for spelling in [
            vec!["-f", "../shared/Dockerfile"],
            vec!["-f../shared/Dockerfile"],
            vec!["-f=../shared/Dockerfile"],
            vec!["--file", "../shared/Dockerfile"],
            vec!["--file=../shared/Dockerfile"],
            vec!["-qf../shared/Dockerfile"],
            vec!["-qf", "../shared/Dockerfile"],
        ] {
            let mut args = vec!["build".to_string()];
            args.extend(spelling.iter().map(|s| s.to_string()));
            args.push(".".to_string());
            let input = BuildInput::parse(&cwd, &args, 1)
                .unwrap()
                .unwrap_or_else(|| panic!("{spelling:?} names a context"));
            assert_eq!(
                input.dockerfile.as_ref().map(|(_, _, p)| p.clone()),
                Some(root.join("shared/Dockerfile")),
                "{spelling:?} names a Dockerfile"
            );

            // The anchor has to climb to cover it, or the sync never
            // carries the file the build is about to read.
            let workspace = Workspace {
                cwd: cwd.clone(),
                root: cwd.clone(),
                name: "ctx".into(),
                config: crate::config::Config::default(),
                workspace_key: crate::config::WorkspaceKey::from_namespace("test-client", &cwd)
                    .unwrap(),
            };
            assert_eq!(build_anchor(&workspace, &input), root, "{spelling:?}");

            // The word goes back in the spelling it arrived in, with
            // only the path swapped — an absolute one, so the rewrite
            // has something to do and a splice that lands in the wrong
            // place shows up.
            let absolute = root.join("shared/Dockerfile");
            let mut spelled: Vec<String> = args
                .iter()
                .map(|a| a.replace("../shared/Dockerfile", &absolute.to_string_lossy()))
                .collect();
            let input = BuildInput::parse(&cwd, &spelled, 1).unwrap().unwrap();
            input.rewrite_args(&cwd, &mut spelled);
            assert_eq!(spelled, args, "{spelling:?} keeps its own spelling");
        }
    }

    /// `--label -f .` is a label whose value is the word `-f`, and it
    /// builds. Stepping word by word read that `-f` as a real flag and
    /// took the context for its own Dockerfile.
    #[test]
    fn a_dash_f_inside_another_flags_value_is_not_a_dockerfile() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let args = vec![
            "build".to_string(),
            "--label".to_string(),
            "-f".to_string(),
            ".".to_string(),
        ];
        let input = BuildInput::parse(&cwd, &args, 1).unwrap().unwrap();
        assert_eq!(input.context, cwd);
        assert!(input.dockerfile.is_none());
    }

    /// `docker build --build-arg --help .` is a build whose `--build-arg`
    /// value is the word `--help`, and it builds (measured). Reading it
    /// as a help request meant no context, no sync, and a verbatim
    /// forward — so the SERVER's `.` was built and nobody was told.
    #[test]
    fn a_help_shaped_flag_value_is_not_a_help_request() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        for asking in [vec!["--help"], vec!["--help=true"], vec!["-h"], vec!["-qh"]] {
            let mut args = vec!["build".to_string()];
            args.extend(asking.iter().map(|s| s.to_string()));
            args.push(".".to_string());
            assert!(
                BuildInput::parse(&cwd, &args, 1).unwrap().is_none(),
                "{asking:?} asks for help"
            );
        }
        for building in [
            vec!["--build-arg", "--help"],
            vec!["--label", "-h"],
            vec!["--help=false"],
        ] {
            let mut args = vec!["build".to_string()];
            args.extend(building.iter().map(|s| s.to_string()));
            args.push(".".to_string());
            let input = BuildInput::parse(&cwd, &args, 1)
                .unwrap()
                .unwrap_or_else(|| panic!("{building:?} is a build, not a help request"));
            assert_eq!(input.context, cwd);
        }
    }

    #[test]
    fn absolute_paths_become_remote_cwd_relative() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("repo/sub")).unwrap();
        std::fs::create_dir_all(temp.path().join("repo/context")).unwrap();
        let cwd = temp.path().join("repo/sub").canonicalize().unwrap();
        let context = temp.path().join("repo/context").canonicalize().unwrap();
        assert_eq!(relative_from(&cwd, &context), "../context");
    }

    #[test]
    fn a_parent_build_context_widens_a_compose_free_workspace() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("repo/tools")).unwrap();
        let root = temp.path().join("repo").canonicalize().unwrap();
        std::fs::write(root.join("tools/Dockerfile.base"), "FROM scratch\n").unwrap();
        let workspace = Workspace {
            cwd: root.join("tools"),
            root: root.join("tools"),
            name: "tools".into(),
            config: crate::config::Config::default(),
            workspace_key: crate::config::WorkspaceKey::from_namespace(
                "test-client",
                &root.join("tools"),
            )
            .unwrap(),
        };
        let args = vec![
            "build".into(),
            "-f".into(),
            "Dockerfile.base".into(),
            "..".into(),
        ];
        let input = BuildInput::parse(&workspace.cwd, &args, 1)
            .unwrap()
            .unwrap();

        assert_eq!(build_anchor(&workspace, &input), root);
    }

    #[test]
    fn stdin_and_url_contexts_do_not_claim_local_paths() {
        let cwd = std::env::current_dir().unwrap();
        for context in [
            "-",
            "https://example.invalid/repo.git",
            "git@example.invalid:x/y.git",
        ] {
            let args = vec!["build".into(), context.into()];
            assert!(BuildInput::parse(&cwd, &args, 1).unwrap().is_none());
        }
    }

    fn footprint_at(anchor: &Path) -> Footprint {
        Footprint {
            anchor: anchor.to_path_buf(),
            entries: Vec::new(),
            server_refs: Vec::new(),
            whole_anchor: false,
            contexts: Vec::new(),
            pinned: Vec::new(),
            model_json: String::new(),
        }
    }

    #[test]
    fn an_output_that_stays_empty_is_taken_back_however_the_run_ends() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let meta = root.join("build/out/meta.json");
        let dist = root.join("dist");
        {
            let mut outputs = Outputs::new();
            outputs.make_room(&meta, false);
            outputs.make_room(&dist, true);
            assert!(meta.is_file(), "rsync needs the path to exist to carry it");
            assert!(dist.is_dir(), "and needs to see that this one is a tree");
            // No `keep`, which stands for every way a build can end
            // short of success: the sync fails, the lock cannot be
            // taken, Docker exits non-zero, the user hits an error
            // between the two.
        }
        assert!(
            !meta.exists(),
            "a zero-byte metadata file was left looking like an answer"
        );
        assert!(
            !root.join("build").exists(),
            "the directories made to hold it were left behind"
        );
        assert!(
            !dist.exists(),
            "an empty dist/ was left where plain docker leaves nothing"
        );
    }

    #[test]
    fn an_output_that_was_filled_in_is_never_taken_back() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let meta = root.join("meta.json");
        let dist = root.join("dist");
        {
            let mut outputs = Outputs::new();
            outputs.make_room(&meta, false);
            outputs.make_room(&dist, true);
            // What the pull brings home before the guard is dropped.
            std::fs::write(&meta, "{\"image.name\":\"api\"}").unwrap();
            std::fs::write(dist.join("layer.tar"), "x").unwrap();
        }
        assert!(meta.is_file(), "the answer the build produced was deleted");
        assert!(
            dist.join("layer.tar").is_file(),
            "the output the build produced was deleted"
        );

        // And a successful run keeps even an empty one: it is the
        // build's own answer by then, not Ulak's placeholder.
        let empty = root.join("empty.json");
        {
            let mut outputs = Outputs::new();
            outputs.make_room(&empty, false);
            outputs.keep();
        }
        assert!(empty.is_file());
    }

    #[test]
    fn an_output_path_that_is_a_symlink_is_never_written_through() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("elsewhere")).unwrap();
        let target = root.join("elsewhere/meta.json");
        let link = root.join("meta.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        {
            let mut outputs = Outputs::new();
            outputs.make_room(&link, false);
            assert!(
                !target.exists(),
                "the placeholder was created through the link, outside the workspace"
            );
        }
        assert!(
            link.symlink_metadata().is_ok(),
            "the user's symlink was taken back as if Ulak had made it"
        );
    }

    #[test]
    fn a_local_cache_that_is_not_here_is_a_miss_and_not_a_failure() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("ctx")).unwrap();
        let args: Vec<String> = ["build", "--cache-from", "type=local,src=./cold", "ctx"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let carried = crate::buildflags::scan(&root, &args, 1).unwrap();

        let mut fp = footprint_at(&root);
        let mut outputs = Outputs::new();
        carry_build_inputs(&carried, &mut fp, &mut outputs);

        // Docker treats it as a cache miss and builds — measured on
        // 29.4.0, "local cache import at … skipped due to err" and then
        // exit 0. Refusing it here broke a build that works without Ulak.
        assert_eq!(fp.entries.len(), 1);
        assert!(!fp.entries[0].exists, "a cache that is not here is absent");
        assert!(
            fp.filter_rules().iter().all(|rule| !rule.contains("cold")),
            "a path that is not here must get no rule: {:?}",
            fp.filter_rules()
        );
        assert!(
            !root.join("cold").exists(),
            "a READ path is never created — only outputs are"
        );
    }

    #[test]
    fn a_context_that_is_not_local_still_refuses_a_flag_that_is() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        std::fs::write(cwd.join(".npmrc"), "//registry/:_authToken=x\n").unwrap();

        for (line, flag) in [
            (
                vec![
                    "build",
                    "--secret",
                    "id=npm,src=./.npmrc",
                    "https://example.invalid/repo.git",
                ],
                "--secret",
            ),
            (
                vec![
                    "build",
                    "--output",
                    "type=local,dest=./dist",
                    "git@example.invalid:x/y.git",
                ],
                "--output",
            ),
            (
                vec!["build", "--metadata-file", "./meta.json", "-"],
                "--metadata-file",
            ),
        ] {
            let argv: Vec<String> = line.iter().map(|s| s.to_string()).collect();
            let err = refuse_local_paths_without_a_context(&cwd, &argv, 1)
                .expect_err(&format!("{line:?} names a path on this machine"))
                .to_string();
            assert!(err.contains(flag), "the refusal must name the flag: {err}");
        }

        // Nothing local on the line: these are the builds this branch
        // exists to forward, and they still go.
        for line in [
            vec!["build", "https://example.invalid/repo.git"],
            vec!["build", "-t", "api", "git@example.invalid:x/y.git"],
            vec![
                "build",
                "--cache-from",
                "type=registry,ref=user/app:cache",
                "-",
            ],
            vec!["build", "--secret", "type=env,id=token,src=TOKEN", "-"],
        ] {
            let argv: Vec<String> = line.iter().map(|s| s.to_string()).collect();
            assert!(
                refuse_local_paths_without_a_context(&cwd, &argv, 1).is_ok(),
                "{line:?} names nothing here and must still be forwarded"
            );
        }
        assert!(
            !cwd.join("dist").exists(),
            "a refusal must not leave a directory behind"
        );
    }

    #[test]
    fn a_bake_metadata_file_is_there_before_the_bake_runs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let args: Vec<String> = ["bake", "--metadata-file", "out/meta.json", "app"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let meta = {
            let mut outputs = Outputs::new();
            let meta = bake_metadata_file(&root, &args, 1, &root, &mut outputs).unwrap();
            assert_eq!(meta, root.join("out/meta.json"));
            // Without this the entry is absent, `filter_rules` skips it,
            // and the file arrives one bake late — holding the previous
            // run's answer.
            assert!(meta.is_file(), "the bake has nowhere to write home to");
            meta
        };
        assert!(!meta.exists(), "an empty metadata file was left behind");

        // Outside the anchor there is nothing to carry it in, so the
        // path is reported (by the caller) and never created.
        let mut outputs = Outputs::new();
        let elsewhere =
            bake_metadata_file(&root, &args, 1, &root.join("proj"), &mut outputs).unwrap();
        assert!(!elsewhere.exists());
    }

    #[test]
    fn a_direct_context_compose_never_named_joins_the_footprint() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("direct")).unwrap();
        std::fs::write(root.join("direct/Dockerfile"), "FROM scratch\n").unwrap();
        let args = vec!["build".into(), "direct".into()];
        let input = BuildInput::parse(&root, &args, 1).unwrap().unwrap();
        let mut fp = Footprint {
            anchor: root.clone(),
            entries: Vec::new(),
            server_refs: Vec::new(),
            whole_anchor: false,
            contexts: Vec::new(),
            pinned: Vec::new(),
            model_json: String::new(),
        };

        input.extend_footprint(&mut fp).unwrap();

        assert_eq!(fp.sync_dirs(), vec![root.join("direct")]);
        assert_eq!(fp.contexts[0].root, root.join("direct"));
        assert!(fp.filter_rules().iter().any(|rule| rule == "+ /direct/***"));
    }

    /// Every route in Ulak ends here, so this number is the one a script
    /// or a CI step reads instead of Docker's own.
    ///
    /// The codes are Docker's vocabulary and are meant to survive
    /// untouched: 125 is the daemon refusing the run, 126 an entrypoint
    /// that is not executable, 127 one that is not there, 255 what ssh
    /// reports for its own failures. The signals are the half that used
    /// to be flattened to 130 — measured in this shell, `kill -PIPE`
    /// gives 141, `-KILL` 137, `-TERM` 143 and only `-INT` gives 130, so
    /// answering 130 for all of them told every caller of `ulak docker
    /// save img | head` that somebody had pressed Ctrl-C.
    #[test]
    fn dockers_own_code_travels_and_a_signal_becomes_the_shells_number() {
        use std::os::unix::process::ExitStatusExt as _;
        for code in [0, 1, 2, 125, 126, 127, 255] {
            let status = std::process::ExitStatus::from_raw(code << 8);
            assert_eq!(status.code(), Some(code), "the fixture itself is wrong");
            assert_eq!(
                shell_code(status),
                u8::try_from(code).unwrap(),
                "exit {code}"
            );
        }

        for (signo, want) in [(2, 130), (9, 137), (13, 141), (15, 143)] {
            let status = std::process::ExitStatus::from_raw(signo);
            assert_eq!(status.code(), None, "signal {signo} leaves no exit code");
            assert_eq!(shell_code(status), want, "killed by signal {signo}");
        }

        // The premise the whole thing rests on, spawned rather than
        // recalled: Rust ignores SIGPIPE in its own process but restores
        // it in the child, so a child on the far end of a closed pipe
        // really does die of a signal and really does arrive here with
        // no exit code. Ulak's own children are `ssh`, and a `save`
        // piped into `head` is how they meet one.
        let killed = std::process::Command::new("sh")
            .args(["-c", "kill -PIPE $$"])
            .status()
            .expect("sh");
        assert_eq!(killed.code(), None, "SIGPIPE was ignored in the child");
        assert_eq!(shell_code(killed), 141);
    }
}
