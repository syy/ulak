//! The local file bridge: commands whose arguments name a file on THIS
//! machine, carried as a stream instead of as a path.
//!
//! Why a bridge and not a path rewrite. `docker save -o api.tar api`
//! forwarded verbatim writes `api.tar` on the SERVER — in a directory
//! the next sync may delete, under a name the user believes is in their
//! current directory. The same trap runs the other way for `docker load
//! -i api.tar`, which reads a file that is not there, or worse, reads a
//! different file that is.
//!
//! So none of these forward the path. Docker already has a stdio form
//! for every one of them — `save` writes the tar to stdout, `load` reads
//! it from stdin, `import` and `secret create` take `-` — and ssh
//! carries the bytes while the local file is opened on the side it
//! actually lives on.
//!
//! `docker cp` is the exception, because its semantics depend on what
//! exists at the destination: `cp src ctr:/app` means "into /app" when
//! /app is a directory and "as /app" when it is not, and only the far
//! side knows which. It is not reimplemented here. The bytes are staged
//! across and the REAL `docker cp` runs where the answer is known.

use std::fs::File;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result};

use crate::catalog::{Bridge, Resolved};
use crate::docker::{Remote, Tty, status_code};
use crate::ssh::sh_quote;
use crate::ui::fail;

pub fn run(resolved: &Resolved, kind: Bridge) -> Result<ExitCode> {
    // Docker's own help never touches a file; let the server print it.
    if help_wanted(resolved) {
        let remote = Remote::open()?;
        return status_code(remote.docker(&resolved.argv, None, Tty::Auto, &[])?);
    }
    match kind {
        Bridge::Pull => pull(resolved),
        Bridge::Push => push(resolved),
        Bridge::Confidential => confidential(resolved),
        Bridge::Cp => cp(resolved),
    }
}

/// Whether this call is asking Docker for its own usage text.
///
/// A whole-tail `any` over the words `--help` and `-h` is what this
/// was, and it reads the carrier flag's VALUE as a request. Measured on
/// Docker 29.4.0: `docker save -o --help alpine:latest` exits 0 having
/// written a 4.2 MB archive to a file honestly named `--help`, and
/// `docker load -qi --help` reads one back. Answered as help, that
/// argv was forwarded verbatim and the archive was written on the
/// SERVER — the same mistake `buildflags::help_wanted` was fixed for
/// one route over, and this is the tree agreeing with itself again.
///
/// The carrier comes out first, through the same `take_flag` the
/// handlers use, so its value cannot be mistaken for a word the user
/// typed. Only the carrier: it is the one flag on these commands whose
/// value names a file on THIS machine, and so the only one whose
/// misreading costs a file on the wrong machine. `--platform` and
/// `--builder` take values too and are NOT stepped over, so `docker
/// load --platform --help` still reads as a request — it prints usage
/// where Docker would have said "invalid platform", touching nothing
/// on either side. Naming every value flag of every bridged command
/// means a second table beside `carrier`, and one that drifts, to buy
/// a better error message.
///
/// `take_flag`'s refusal is deliberately dropped. `docker save -o`
/// with nothing after it leaves argv untouched (every `take_flag`
/// mutation happens on the way to an `Ok`), the handler raises the
/// same refusal a moment later against the argv the user really typed,
/// and in the meantime `docker save --help -o` still prints help.
///
/// Only as far as `--`, because that is where Docker stops reading
/// flags too. Measured: `docker cp -- --help ctr:/x` answers `lstat
/// …/--help: no such file or directory`, so a file honestly named
/// `--help` is a path — and scanning past the terminator turned
/// copying it into a help screen.
fn help_wanted(resolved: &Resolved) -> bool {
    let mut probe = resolved.argv.clone();
    let _ = take_flag(
        &mut probe,
        resolved.tail_start,
        carrier(resolved.entry.path),
        bool_shorthands(resolved.entry.path),
    );
    probe[resolved.tail_start..]
        .iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "--help" || a == "-h")
}

/// The local file a carrier flag named, or `None` when it named none.
///
/// Two spellings mean "no file", and both are Docker's. `-` is the
/// obvious one. The EMPTY value is not obvious and is the one a script
/// reaches by accident, writing `-o "$OUT"` with `OUT` unset — measured
/// on 29.4.0, `docker save -o "" alpine:3.20` wrote 4 MB of tar to
/// STDOUT and `docker load -i "" < api.tar` read the archive from
/// STDIN. So an empty value is not a file with an empty name, and
/// reading it as one cost the whole transfer: `PathBuf::from("")` is
/// not a directory and has no `file_name`, so the image streamed into a
/// bare `.ulak-partial` in the CURRENT directory and was thrown away by
/// a rename to "" that failed naming no path at all.
///
/// Answering `None` puts both halves back on Docker's own road: the
/// stream goes to our stdout, where `OnTerminal::Refuse` reproduces
/// Docker's own refusal to write an archive to a terminal, or comes off
/// our stdin, which is already the user's.
fn named_file(carried: Option<String>) -> Option<String> {
    carried.filter(|p| p != "-" && !p.is_empty())
}

/// Which flag carries the local file, per command. Measured against the
/// help output of Docker 29.4.0 and Buildx 0.33.0 — not remembered.
///
/// An empty list means the file is a POSITIONAL (`docker import FILE`,
/// `docker secret create NAME FILE`), which each handler locates for
/// itself because "which positional" differs per command.
fn carrier(path: &[&str]) -> &'static [&'static str] {
    match path.last().copied().unwrap_or_default() {
        // save / export / history export: -o, --output
        "save" | "export" => &["-o", "--output"],
        // load: -i, --input
        "load" => &["-i", "--input"],
        _ => &[],
    }
}

/// The BOOL shorthands the same commands take, so a bundle can be split
/// down to the one flag that names a file. pflag reads `-qi api.tar` as
/// `-q -i api.tar`, and a bundle this code could not read left the local
/// path in argv for the server to open.
///
/// Keyed on the whole path rather than its last word, unlike `carrier`:
/// `docker export` and `docker buildx history export` are both `export`
/// and only the second one has `-D`. Measured against `--help` on Docker
/// 29.4.0 and Buildx 0.33.0 — `docker load -qiapi.tar` loads, and
/// `docker buildx history export -Doout.tar` writes `out.tar`.
fn bool_shorthands(path: &[&str]) -> &'static [&'static str] {
    match path {
        ["load"] | ["image", "load"] => &["-q"],
        ["buildx", "history", "export"] | ["builder", "history", "export"] => &["-D"],
        _ => &[],
    }
}

// ─── remote stdout lands in a local file ────────────────────────────

fn pull(resolved: &Resolved) -> Result<ExitCode> {
    let mut args = resolved.argv.clone();
    let flags = carrier(resolved.entry.path);
    let bools = bool_shorthands(resolved.entry.path);
    let out = take_flag(&mut args, resolved.tail_start, flags, bools)?;

    let remote = Remote::open()?;
    // NEVER a TTY: this is a tar stream. A pty would translate newlines
    // and quietly corrupt every archive that crossed it.
    let (cmd, redacted) = remote.spell(&args, None, Tty::Never, &[]);
    let local = named_file(out).map(PathBuf::from);
    // save/export/history export all carry an archive, so a bare
    // terminal is always a mistake here.
    land(
        cmd,
        &redacted,
        local.as_deref(),
        OnTerminal::Refuse {
            command: &resolved.entry.name(),
        },
    )
}

/// Run a command whose STDOUT is the answer, and put that answer where
/// the user asked for it: in `local`, or on our own stdout when they
/// named no file.
///
/// Shared, because getting it right is four things a caller should not
/// have to remember, and every one of them was a bug first:
///
///   * the destination is checked BEFORE the transfer, so `-o mydir`
///     fails in an instant rather than at the far end of a multi-gigabyte
///     image;
///   * the bytes land in `<name>.ulak-partial` and are renamed on
///     success, so a failed command cannot destroy the file the user
///     already had, and a half-written archive is never left looking
///     whole;
///   * the partial does not survive any exit path, including a rename
///     that fails;
///   * with no file named, what a terminal means is the CALLER's to
///     say — see `OnTerminal`.
pub enum OnTerminal<'a> {
    /// The stream is binary, so a terminal is a mistake. Docker refuses
    /// this itself when run locally ("cowardly refusing to save to a
    /// terminal") and cannot do so through ssh, where it only ever sees
    /// a pipe — so the refusal is reproduced here, naming the command.
    Refuse { command: &'a str },
    /// The output is text somebody reads. `docker compose config` with
    /// no `-o` printing YAML to the screen is its primary use, not an
    /// accident, and refusing it would break `… config | less`.
    Print,
}

impl OnTerminal<'_> {
    /// The refusal this policy makes, if it makes one — split out of
    /// `land` so the rule can be tested without a terminal to run in.
    ///
    /// `is_terminal()` is not something a test can arrange: cargo hands
    /// every test a pipe, so the refusal branch was unreachable from the
    /// suite and unexercised anywhere. A guard nothing tests can quietly
    /// stop guarding, and this one is all that stands between `ulak
    /// docker save api` typed at a prompt and a few hundred megabytes of
    /// tar on the user's screen. Split the way `docker.rs` splits
    /// `asked_for_a_tty` out of `tty_for`, and for the same reason.
    fn refusal(&self, to_a_terminal: bool) -> Option<anyhow::Error> {
        match self {
            OnTerminal::Refuse { command } if to_a_terminal => Some(
                fail!("cowardly refusing to write `docker {command}` output to your terminal")
                    .now("name a file with -o, or redirect it: … > out")
                    .into_err(),
            ),
            _ => None,
        }
    }
}

pub fn land(
    mut cmd: std::process::Command,
    redacted: &str,
    local: Option<&Path>,
    on_terminal: OnTerminal<'_>,
) -> Result<ExitCode> {
    let Some(local) = local else {
        if let Some(refusal) = on_terminal.refusal(std::io::stdout().is_terminal()) {
            return Err(refusal);
        }
        cmd.stdout(Stdio::inherit());
        let status = cmd.status().context("cannot spawn ssh")?;
        crate::passthrough::record_ssh_audit(&cmd, redacted, status.code());
        return status_code(status);
    };

    // A file with no name is a caller's bug rather than a user's
    // spelling — `pull` reads an empty carrier value the way Docker
    // does, as no file at all (see `named_file`) — so this is the last
    // line of defence, and it is here because the failure was so quiet:
    // an empty path is not a directory and has no `file_name`, so
    // `with_file_name` produced a bare `.ulak-partial` in the CURRENT
    // directory, the whole image streamed into it, and only the rename
    // afterwards failed, naming no path for the user to go and look at.
    if local.as_os_str().is_empty() {
        return Err(fail!("no file was named to write the output to")
            .now("give the flag a path: -o out.tar")
            .into_err());
    }
    if local.is_dir() {
        return Err(fail!("{} is a directory", local.display())
            .now("name the file to write, not the directory to write it in")
            .into_err());
    }
    let partial = local.with_file_name(format!(
        "{}{PARTIAL_SUFFIX}",
        local.file_name().unwrap_or_default().to_string_lossy()
    ));
    // Nothing else ever comes back for a partial that outlived its
    // command — `Drop` does not run on Ctrl-C — so the next command
    // writing into the same directory is the one thing that reliably
    // does. See `sweep`.
    sweep(&holding(&partial), STALE);
    let file =
        File::create(&partial).with_context(|| format!("cannot write {}", partial.display()))?;
    cmd.stdout(Stdio::from(file));
    // A `?` here left the empty `.ulak-partial` behind for good: the
    // file exists from this line on, nothing else ever removes it, and
    // it sits in the directory the user asked their output to go to.
    // Failing to reach the server is exactly the case where they will
    // retry and find it.
    let status = match cmd.status() {
        Ok(status) => status,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(anyhow::Error::new(e).context("cannot spawn ssh"));
        }
    };
    crate::passthrough::record_ssh_audit(&cmd, redacted, status.code());

    let placed = status
        .success()
        .then(|| {
            std::fs::rename(&partial, local)
                .with_context(|| format!("cannot put the output at {}", local.display()))
        })
        .transpose();
    if placed.is_err() || !status.success() {
        let _ = std::fs::remove_file(&partial);
    }
    placed?;
    status_code(status)
}

// ─── a local file feeds remote stdin ────────────────────────────────

fn push(resolved: &Resolved) -> Result<ExitCode> {
    let mut args = resolved.argv.clone();
    let flags = carrier(resolved.entry.path);

    let source = if flags.is_empty() {
        import_source(&mut args, resolved.tail_start)
    } else {
        named_file(take_flag(
            &mut args,
            resolved.tail_start,
            flags,
            bool_shorthands(resolved.entry.path),
        )?)
    };

    let remote = Remote::open()?;
    let (mut cmd, redacted) = remote.spell(&args, None, Tty::Never, &[]);
    // With no local file named, Docker reads stdin — and ours is
    // already the user's, so it needs no help.
    if let Some(path) = &source {
        let file = File::open(path).with_context(|| format!("cannot read {path}"))?;
        cmd.stdin(Stdio::from(file));
    }
    let status = cmd.status().context("cannot spawn ssh for docker")?;
    crate::passthrough::record_ssh_audit(&cmd, &redacted, status.code());
    status_code(status)
}

/// `docker import` flags that take a value, so the first POSITIONAL can
/// be found without mistaking a flag's argument for it.
const IMPORT_VALUE_FLAGS: &[&str] = &["-c", "--change", "-m", "--message", "--platform"];

/// `docker secret create` / `docker config create` flags that take a
/// value. Both commands share this set.
const CREATE_VALUE_FLAGS: &[&str] = &["-d", "--driver", "-l", "--label", "--template-driver"];

/// Take the local file out of argv, leaving Docker's own `-` where it
/// stood, and hand back the path for this side to open.
///
/// This one line IS the streaming trick, for `import` and for `secret
/// create` both: the bytes travel on stdin, and argv — which the audit
/// trail keeps, and which the server's shell sees — carries only a dash.
/// Leave the path in and it is forwarded verbatim, so the SERVER opens
/// its own `secrets/api-key.txt`. Silently, and for a secret.
///
/// A `-` the user typed themselves is already the stream form: Docker
/// reads stdin, and ours is theirs, so there is nothing to open here.
fn stream_instead(args: &mut [String], at: usize) -> Option<String> {
    (args[at] != "-").then(|| std::mem::replace(&mut args[at], "-".to_string()))
}

/// `docker import file|URL|- [REPOSITORY[:TAG]]`: the first positional
/// names the file, unless it names a URL the SERVER is to fetch.
fn import_source(args: &mut [String], from: usize) -> Option<String> {
    let i = first_positional(args, from, IMPORT_VALUE_FLAGS)?;
    (!is_remote_source(&args[i]))
        .then(|| stream_instead(args, i))
        .flatten()
}

/// `docker secret create NAME [file|-]`, `docker config create NAME
/// file|-`: NAME is the first positional and the file is the SECOND.
///
/// Both positionals come out of one scan on purpose. Asking twice, the
/// second time starting past the first answer, forgets that flag parsing
/// already ended at a `--` the first scan walked over — and then
/// `docker secret create -- key -secret.txt` reads `-secret.txt` as a
/// flag, finds no file, and forwards the path for the server to open.
/// Measured on 29.4.0: Docker answers `error reading from -secret.txt`,
/// so past `--` that word is the FILE, and `--` is the only way to name
/// one that starts with a dash.
fn create_source(args: &mut [String], from: usize) -> Option<String> {
    let file = *positionals(args, from, CREATE_VALUE_FLAGS).get(1)?;
    stream_instead(args, file)
}

// ─── a local file feeds remote stdin, and leaves no trace ───────────

/// `docker secret create NAME [file|-]`, `docker config create NAME
/// file|-`.
///
/// The file is streamed rather than staged for a second reason beyond
/// the wrong-machine one: a secret written into the workspace would be
/// on the server's disk, inside a directory the sync mirrors, until
/// something removed it. Nothing here writes it down, and argv carries
/// only the name and a `-`, so the audit trail cannot hold it either.
fn confidential(resolved: &Resolved) -> Result<ExitCode> {
    let mut args = resolved.argv.clone();
    let source = create_source(&mut args, resolved.tail_start);

    let remote = Remote::open()?;
    let (mut cmd, redacted) = remote.spell(&args, None, Tty::Never, &[]);
    if let Some(path) = &source {
        let file = File::open(path).with_context(|| format!("cannot read {path}"))?;
        cmd.stdin(Stdio::from(file));
    } else if std::io::stdin().is_terminal() {
        return Err(fail!(
            "`docker {}` needs the secret's content",
            resolved.entry.name()
        )
        .now("name a local file, or pipe it in: … create NAME - < secret.txt")
        .into_err());
    }
    let status = cmd.status().context("cannot spawn ssh for docker")?;
    crate::passthrough::record_ssh_audit(&cmd, &redacted, status.code());
    status_code(status)
}

// ─── docker cp ──────────────────────────────────────────────────────

/// The opening of both `cp` scripts: a private staging directory under
/// the server's own `~/.ulak`, removed however the script ends.
///
/// Under the home directory rather than `/tmp` on purpose — it is the
/// same filesystem the workspace lives on, so a large copy cannot fill
/// a small `/tmp`, and a server that reboots does not take a half-copy
/// with it. `set -e` is what makes the second half not run when the
/// first fails; `trap … EXIT` is what cleans up when it does.
/// `HUP INT TERM` as well as `EXIT`, because with `Tty::Never` there is
/// no pty to carry a local Ctrl-C to the remote shell as SIGINT — the
/// connection simply drops, and only the signals ssh does deliver can
/// still fire the cleanup.
const STAGE: &str = "set -e; mkdir -p \"$HOME/.ulak\"; \
                     S=$(mktemp -d \"$HOME/.ulak/cp.XXXXXX\"); \
                     trap 'rm -rf \"$S\"' EXIT HUP INT TERM; ";

/// Flags, shell-quoted, each with its leading space — so the caller can
/// write `docker cp{opts}` and get a correct command line whether there
/// are flags or not.
fn quoted(opts: &[String]) -> String {
    opts.iter().fold(String::new(), |mut s, o| {
        s.push(' ');
        s.push_str(&sh_quote(o));
        s
    })
}

fn cp(resolved: &Resolved) -> Result<ExitCode> {
    let CpArgs {
        opts,
        follow,
        archive,
        src,
        dst,
    } = CpArgs::parse(resolved.tail())?;
    let (src, dst) = (src.as_str(), dst.as_str());
    // Docker's own globals — `--log-level debug` and the like — came
    // through `resolve` in front of the command and belong in front of
    // the `docker` these scripts build too. Dropping them made
    // `ulak docker --log-level debug cp …` silently quieter than the
    // same flag on every other route.
    let globals = &resolved.argv[..resolved.globals];

    let remote = Remote::open()?;
    // Going UP, `-L` is ours to apply, because the symlink it would
    // follow is on this machine. Going DOWN it is not: the link is
    // inside the container, and only the far side's `docker cp` can see
    // it. Measured — `docker cp -L ctr:/flink .` brings back the FILE
    // the link points at under the link's own name, and stripping the
    // flag brought back the link. Everything else the far side gets
    // either way, `-a` included; see `cp_up` for what that costs.
    let up_opts: Vec<String> = opts.iter().filter(|o| *o != "-L").cloned().collect();

    // Here rather than in `Staging::beside`, which is reached only when
    // the destination of a DOWN copy is not already a directory — one
    // of the four roads through this function. A workspace that only
    // copies up, or only down into directories that exist, never swept
    // at all, so a leftover from the one copy that did stage sat in it
    // forever. This is the road they all take.
    if let Some(local) = [src, dst]
        .into_iter()
        .find(|p| *p != "-" && split_container(p).is_none())
    {
        sweep(&holding(Path::new(local)), STALE);
    }

    match (split_container(src), split_container(dst)) {
        // Either end is already a stream: Docker's own `-` form does
        // exactly what is needed, and our stdio is the user's.
        _ if src == "-" || dst == "-" => {
            let (mut cmd, redacted) = remote.spell(&resolved.argv, None, Tty::Never, &[]);
            let status = cmd.status().context("cannot spawn ssh for docker")?;
            crate::passthrough::record_ssh_audit(&cmd, &redacted, status.code());
            status_code(status)
        }
        (Some(_), Some(_)) | (None, None) => {
            // Container-to-container and host-to-host are Docker's own
            // errors to give, in Docker's own words.
            let (mut cmd, redacted) = remote.spell(&resolved.argv, None, Tty::Never, &[]);
            let status = cmd.status().context("cannot spawn ssh for docker")?;
            crate::passthrough::record_ssh_audit(&cmd, &redacted, status.code());
            status_code(status)
        }
        (None, Some(_)) => cp_up(&remote, globals, src, dst, &up_opts, follow, archive),
        (Some(_), None) => cp_down(&remote, globals, src, dst, &opts),
    }
}

/// Which of `docker cp`'s three flags a word names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CpFlag {
    Archive,
    Follow,
    Quiet,
}

/// `docker cp`'s only flags, all of them booleans — which is what makes
/// "the two non-flag words are the paths" a safe rule here and nowhere
/// else. From `docker cp --help`, Docker 29.4.0.
fn cp_flag(name: &str) -> Option<CpFlag> {
    match name {
        "-a" | "--archive" => Some(CpFlag::Archive),
        "-L" | "--follow-link" => Some(CpFlag::Follow),
        "-q" | "--quiet" => Some(CpFlag::Quiet),
        _ => None,
    }
}

/// `docker cp`'s argv, read the way pflag reads it.
#[derive(Debug)]
struct CpArgs {
    /// Flags to hand on, in ONE canonical spelling each.
    ///
    /// Not the user's spelling, because the user's spelling can say the
    /// opposite of what it looks like: `docker cp -a=false` is legal and
    /// turns `-a` OFF, so passing the word through would have carried an
    /// archive copy nobody asked for.
    opts: Vec<String>,
    follow: bool,
    archive: bool,
    src: String,
    dst: String,
}

impl CpArgs {
    fn parse(tail: &[String]) -> Result<CpArgs> {
        let (mut archive, mut follow, mut quiet) = (false, false, false);
        let mut paths = Vec::new();
        let mut only_paths = false;
        for arg in tail {
            // `--` ends flag parsing, which is the only way to name a
            // source that starts with a dash.
            if !only_paths && arg == "--" {
                only_paths = true;
                continue;
            }
            if only_paths || arg == "-" || !arg.starts_with('-') {
                paths.push(arg.clone());
                continue;
            }
            let mut set = |flag, on| match flag {
                CpFlag::Archive => archive = on,
                CpFlag::Follow => follow = on,
                CpFlag::Quiet => quiet = on,
            };
            if arg.starts_with("--") {
                let (name, value) = match arg.split_once('=') {
                    Some((name, value)) => (name, Some(value)),
                    None => (arg.as_str(), None),
                };
                let flag = cp_flag(name).ok_or_else(|| unknown_cp_flag(name))?;
                set(flag, cp_bool_value(name, value)?);
                continue;
            }
            // pflag bundles shorthands, so `-aL` is `-a -L`. Splitting
            // rather than matching the whole word is the difference
            // between accepting what Docker accepts and refusing it —
            // and the last shorthand in a bundle can still take a value,
            // so `-aL=false` is `-a --follow-link=false`. All measured.
            let mut rest = &arg[1..];
            while let Some(c) = rest.chars().next() {
                rest = &rest[c.len_utf8()..];
                let short = format!("-{c}");
                let flag = cp_flag(&short).ok_or_else(|| unknown_cp_flag(&short))?;
                let value = rest.strip_prefix('=');
                if value.is_some() {
                    rest = "";
                }
                set(flag, cp_bool_value(&short, value)?);
            }
        }

        let [src, dst] = paths.as_slice() else {
            return Err(fail!("`docker cp` needs exactly two paths")
                .now("for example: ulak docker cp ./local.txt web:/app/local.txt")
                .now("or the other way: ulak docker cp web:/app/out.txt ./out.txt")
                .into_err());
        };
        // Docker refuses an empty path in its own client, before a byte
        // moves: `docker cp "" web:/backup` answers `source can not be
        // empty` and exits 1 (measured, 29.4.0). Here the same word
        // would name THE WHOLE PROJECT — `absolute("")` is the working
        // directory, whose `symlink_metadata` succeeds and whose
        // `file_name` is the project's own — so `cp "$SRC" web:/backup`
        // with `SRC` unset would tar the tree, `.env` and all, into the
        // container and report success. The `pull` route already stands
        // this guard for the same reason; see `land`.
        for (path, end) in [(src, "source"), (dst, "destination")] {
            if path.is_empty() {
                return Err(fail!("the {end} of a copy cannot be empty")
                    .now("name both ends: ulak docker cp ./local.txt web:/app/local.txt")
                    .now("an empty word here would copy this whole directory")
                    .into_err());
            }
        }
        let mut opts = Vec::new();
        for (on, spelling) in [(archive, "-a"), (follow, "-L"), (quiet, "-q")] {
            if on {
                opts.push(spelling.to_string());
            }
        }
        Ok(CpArgs {
            opts,
            follow,
            archive,
            src: src.clone(),
            dst: dst.clone(),
        })
    }
}

/// pflag hands a bool flag's value to `strconv.ParseBool`, so `-q=1` and
/// `--archive=false` are real spellings that this used to refuse
/// outright. Measured, including the refusal: `docker cp -a=yes` answers
/// `invalid argument "yes" for "-a, --archive" flag`.
fn cp_bool_value(flag: &str, raw: Option<&str>) -> Result<bool> {
    match raw {
        None => Ok(true),
        Some("1" | "t" | "T" | "TRUE" | "true" | "True") => Ok(true),
        Some("0" | "f" | "F" | "FALSE" | "false" | "False") => Ok(false),
        Some(other) => Err(fail!("{flag} takes true or false, not `{other}`")
            .now(format!("write it as {flag} to turn it on, or leave it out"))
            .into_err()),
    }
}

fn unknown_cp_flag(flag: &str) -> anyhow::Error {
    fail!("`docker cp` has no flag {flag}")
        .now("it takes only -a/--archive, -L/--follow-link and -q/--quiet")
        .into_err()
}

/// The local path a `docker cp` source names, and whether the user asked
/// for its CONTENTS.
///
/// A trailing `/.` is the one spelling that means "what is in this
/// directory" rather than "this directory". Stripping dots
/// unconditionally is worse than not handling it at all: it turns a file
/// honestly named `data.` into a missing path, and quietly rewrites
/// `./dir/..` — the parent — as `./dir`.
fn split_contents_only(raw: &str) -> (&str, bool) {
    match raw.strip_suffix("/.") {
        Some(dir) if !dir.is_empty() => (dir, true),
        // `/.` and `.` both mean "the contents of this directory".
        _ if raw == "." => (".", true),
        _ => (raw, false),
    }
}

/// Where `-L` says to pack the tar from, and under what name.
///
/// `--follow-link` follows the SOURCE path's own symlink and NOTHING
/// else. Measured on 29.4.0, both directions: with `link -> real/`,
/// `docker cp -L link ctr:/d/` lands a DIRECTORY called `link` whose own
/// inner symlinks arrive as symlinks. `tar -h`, which this used to pass
/// instead, dereferences every link in the tree — an inner `ilink ->
/// inner.txt` arrived as a second copy of the file, and a tree of links
/// arrived many times its own size.
///
/// A link pointing at nothing is an error here, as it is for Docker
/// (`lstat …/nowhere: no such file or directory`), and only with `-L`:
/// without it the link itself is what travels.
fn followed(local: &Path, src: &str) -> Result<(PathBuf, String)> {
    let target = std::fs::canonicalize(local).map_err(|_| {
        fail!("-L cannot follow {src}")
            .now("the link points at something that is not on this machine")
            .now("drop -L to copy the link itself")
            .into_err()
    })?;
    let dir = target
        .parent()
        .ok_or_else(|| fail!("cannot copy the filesystem root").into_err())?
        .to_path_buf();
    let name = target
        .file_name()
        .ok_or_else(|| fail!("cannot copy a path with no name: {src}").into_err())?
        .to_string_lossy()
        .into_owned();
    Ok((dir, name))
}

/// The remote line for a local → container copy.
///
/// `pack` is the name the tar carries and `name` the name the copy must
/// land under; they differ only when `-L` followed a link to something
/// called something else, and the staged entry is renamed before the
/// real `docker cp` ever sees it.
fn up_script(
    globals: &[String],
    opts: &[String],
    pack: &str,
    name: &str,
    dst: &str,
    archive: bool,
) -> String {
    // `-a` promises to carry uid and gid, and the staging step cannot
    // keep that promise: the untar on the far side runs as the ssh
    // user, so ownership is discarded before the real `docker cp -a`
    // ever sees it. Measured — a 501:0 file arrived 1000:1000.
    //
    // So `-a` takes the other road: the tar goes straight into `docker
    // cp -a - CONTAINER:DEST`, where the daemon reads the ownership out
    // of the archive itself. The cost is that `-` requires DEST to be a
    // directory, so `-a` cannot rename — and Docker says so in its own
    // words, which is better than us silently not doing what -a says.
    if archive {
        return format!(
            "docker{globals} cp{opts} - {dst}",
            globals = quoted(globals),
            opts = quoted(opts),
            dst = sh_quote(dst)
        );
    }
    // `"$S"/'name'` concatenates in the shell, so the staged path stays
    // quoted on both halves and a filename with a space or a quote in it
    // cannot reach the command line as anything but one word.
    let staged = format!("\"$S\"/{}", sh_quote(name));
    let rename = if pack == name {
        String::new()
    } else {
        format!("mv \"$S\"/{} {staged}; ", sh_quote(pack))
    };
    // `-p` on the untar, because without it the far side's umask eats
    // the permission bits: measured, a 0666 file came out 0644 and a
    // 0777 directory came out 0755, while real `docker cp` carries the
    // mode across untouched.
    format!(
        "{STAGE}tar -xpf - -C \"$S\"; {rename}docker{globals} cp{opts} {staged} {dst}",
        globals = quoted(globals),
        opts = quoted(opts),
        dst = sh_quote(dst),
    )
}

/// The remote line for a container → local copy.
fn down_script(globals: &[String], opts: &[String], src: &str) -> String {
    format!(
        "{STAGE}docker{globals} cp{opts} {src} \"$S\"/; tar -C \"$S\" -cf - .",
        globals = quoted(globals),
        opts = quoted(opts),
        src = sh_quote(src),
    )
}

/// Local → container. The tar is built here and unpacked into a staging
/// directory on the server, where the REAL `docker cp` decides what
/// `ctr:/app` means. That decision needs to see the container, so it is
/// not made here.
fn cp_up(
    remote: &Remote,
    globals: &[String],
    src: &str,
    dst: &str,
    opts: &[String],
    follow: bool,
    archive: bool,
) -> Result<ExitCode> {
    // Docker gives a trailing `/.` one meaning and one only: the
    // CONTENTS of the directory rather than the directory. Resolving
    // the path first would lose it — `PathBuf::from("./src/.")`'s
    // file_name is `src` — and `cp ./src/. ctr:/app` would quietly copy
    // the directory instead of what is in it.
    let (bare, contents_only) = split_contents_only(src);
    let local = absolute(Path::new(bare))?;
    // `symlink_metadata`, not `exists`: a dangling symlink is a thing
    // `docker cp` copies happily — the link arrives intact and points
    // at nothing, which is what the user asked for. `exists()` follows
    // the link and calls it missing.
    if std::fs::symlink_metadata(&local).is_err() {
        return Err(fail!("{src} is not a file or directory on this machine")
            .now("check the path; `docker cp` reads its source locally")
            .into_err());
    }
    // The NAME comes from the path as typed, never from what a symlink
    // points at. `-L` says "send what the link points to", not "rename
    // it to the target" — `cp -L link.txt ctr:/app/` must land
    // `/app/link.txt`, and canonicalizing first landed `/app/real.txt`.
    let (dir, name) = if contents_only {
        (local.clone(), ".".to_string())
    } else {
        let parent = local
            .parent()
            .ok_or_else(|| fail!("cannot copy the filesystem root").into_err())?;
        let name = local
            .file_name()
            .ok_or_else(|| fail!("cannot copy a path with no name: {src}").into_err())?
            .to_string_lossy()
            .into_owned();
        (parent.to_path_buf(), name)
    };

    // `-L` follows the SOURCE path's own symlink and nothing else, so
    // where the tar is packed FROM can change while the name it travels
    // under does not.
    let (pack_dir, pack_name) = if follow
        && !contents_only
        && std::fs::symlink_metadata(&local).is_ok_and(|m| m.is_symlink())
    {
        followed(&local, src)?
    } else {
        (dir, name.clone())
    };
    // `-a` sends the tar straight into `docker cp -a -` so the daemon
    // can read ownership out of the archive, which leaves no staging
    // directory on the far side to rename in — and the name inside the
    // archive is the one `-L` resolved to, not the link's. Saying so
    // beats landing `real` where the user asked for `link`.
    if archive && pack_name != name {
        return Err(fail!("-a and -L together would land {pack_name}, not {name}")
            .now("-a carries ownership by streaming the archive as it is, and the name in it is the one the link points at")
            .now("name what the link points at yourself, or drop -a")
            .into_err());
    }
    let script = up_script(globals, opts, &pack_name, &name, dst, archive);

    // macOS bsdtar archives com.apple.* metadata by default. Docker's
    // Linux extractor then tries to restore names such as
    // `com.apple.provenance` and rejects the copy with lsetxattr(2).
    // Docker cp promises permissions (and, with -a, uid/gid), not host
    // filesystem xattrs; keep the stream portable across daemon OSes.
    let mut tar = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("--no-xattrs")
        .arg("-C")
        .arg(&pack_dir)
        .args(["-cf", "-", "--"])
        .arg(&pack_name)
        .stdout(Stdio::piped())
        .spawn()
        .context("cannot run tar to pack the source (is tar installed?)")?;
    let packed = tar.stdout.take().expect("tar stdout was piped");

    let mut cmd = remote.script(&script, Tty::Never);
    cmd.stdin(Stdio::from(packed));
    let status = cmd.status().context("cannot spawn ssh for docker cp")?;
    crate::passthrough::record_ssh_audit(&cmd, &script, status.code());
    let packing = tar.wait().context("tar did not finish")?;
    if !packing.success() && status.success() {
        return Err(incomplete_send(src, dst));
    }
    status_code(status)
}

/// What to say when the local `tar` failed but the copy went through
/// anyway.
///
/// That pair is not a contradiction, it is the ordinary case: tar exits
/// non-zero for one unreadable member (2 for GNU, 1 for bsdtar) while
/// still writing a COMPLETE, valid archive of everything else — so the
/// far side unpacked it and the real `docker cp` ran. `ulak docker cp
/// ./volumes/db ctr:/restore`, where postgres wrote a few files as uid
/// 999, lands a tree with holes in it.
///
/// So the message says so. "could not read <src> to send it" was true
/// and read as "nothing happened", which is the one thing it does not
/// mean — the down leg has said "some of it may already be in …" for
/// exactly this situation, and the up leg was quieter about a container
/// the user is about to trust.
fn incomplete_send(src: &str, dst: &str) -> anyhow::Error {
    fail!("could not read all of {src} to send it")
        .now("tar's own message is above")
        .now(format!(
            "the rest of it was copied — some of {dst} has changed, check before retrying"
        ))
        .into_err()
}

/// Container → local. `docker cp` runs on the server into a staging
/// directory, the staged tree comes back as a tar, and the placement
/// rule — "into DEST when DEST is a directory, as DEST otherwise" — is
/// applied here, where the local filesystem can actually be asked.
fn cp_down(
    remote: &Remote,
    globals: &[String],
    src: &str,
    dst: &str,
    opts: &[String],
) -> Result<ExitCode> {
    let dest = PathBuf::from(dst);
    // `ctr:/tree/.` means the CONTENTS of /tree, so the staging
    // directory holds several top-level entries and the destination
    // itself is what they go into. Insisting on exactly one entry —
    // right for every other spelling — refused this one outright.
    let (_, contents_only) = split_contents_only(src);
    // Whether the destination is one WE made. It has to be made before
    // any bytes move, because being a directory is what makes the tar
    // merge into it — but if the copy then fails it is a directory the
    // user never had. Measured on 29.4.0: `docker cp ctr:/no/such/path/.
    // ./fresh` exits 1 and leaves `./fresh` uncreated, while the same
    // copy of a path that exists creates it. And this one cannot be
    // swept later: unlike a staging directory it wears an ordinary name
    // the user chose.
    let fresh = contents_only && !dest.is_dir();
    if fresh {
        std::fs::create_dir_all(&dest)
            .with_context(|| format!("cannot create {}", dest.display()))?;
    }
    let into_dir = dest.is_dir();
    let parent = if into_dir {
        dest.clone()
    } else {
        holding(&dest)
    };
    if !parent.is_dir() {
        return Err(
            fail!("{} is not a directory on this machine", parent.display())
                .now("`docker cp` writes into an existing directory; create it first")
                .into_err(),
        );
    }

    let script = down_script(globals, opts, src);

    // Into the destination directly when DEST is a directory: tar's own
    // merge is the behaviour Docker has there, for free. Otherwise the
    // tree lands in a staging directory beside it, because only a
    // rename can put a whole tree in place without a window in which
    // the destination is half-replaced.
    let stage = (!into_dir).then(|| Staging::beside(&parent)).transpose()?;
    let unpack_into = stage
        .as_ref()
        .map(|s| s.path.clone())
        .unwrap_or_else(|| parent.clone());

    let mut cmd = remote.script(&script, Tty::Never);
    cmd.stdout(Stdio::piped());
    let mut ssh = cmd.spawn().context("cannot spawn ssh for docker cp")?;
    let streamed = ssh.stdout.take().expect("ssh stdout was piped");

    let unpacked = unpack(&unpack_into, Stdio::from(streamed))?;
    let status = ssh.wait().context("ssh did not finish")?;
    crate::passthrough::record_ssh_audit(&cmd, &script, status.code());

    // BOTH sides, and this is the whole point of the staging step.
    // `Command::status()` returns `Ok` for a tar that exited 2 — it
    // reports whether the process could be SPAWNED, not whether it
    // worked — so an extraction that failed halfway used to delete the
    // user's existing destination, rename the partial tree over it, and
    // hand back ssh's 0.
    //
    // The example that used to stand here was `docker cp web:/dev ./dev`
    // as a non-root user, said to make tar exit 2 on `Cannot mknod`, and
    // it does not reach this line by either road. Structurally, `STAGE`
    // has the SERVER materialise the tree with its own `docker cp` under
    // `set -e` first, so a refusal there leaves ssh non-zero and this is
    // not the branch taken. And the refusal may never come: measured on
    // 29.4.0, `docker cp <ctr>:/dev <dir>` exits 0 for an unprivileged
    // user because docker's extractor degrades a device node to an empty
    // regular file rather than calling mknod at all.
    //
    // What does reach here is any local `tar` that writes part of the
    // tree and then exits non-zero — an unreadable member, a full disk,
    // a parent that turned read-only mid-extraction.
    // `an_unpack_that_fails_leaves_a_new_destination_uncreated` in
    // tests/e2e_bridge.rs drives exactly that, with a `tar` on PATH.
    let worked = status.success() && unpacked.success();
    if worked && let Some(stage) = &stage {
        place(&one_entry(&stage.path)?, &dest)?;
    }
    let given_back = !worked && fresh && give_back(&dest);
    if !unpacked.success() && status.success() {
        let mut err = fail!("the copy arrived but could not be unpacked here")
            .now("tar's own message is above");
        // Only the staged branch can promise that — and the `/.` copy
        // whose destination we made and have just taken back, which
        // leaves the tree exactly as the user had it. When DEST was
        // already a directory the tar extracts straight into it, because
        // that is what gives Docker's merge behaviour for free — so a
        // half extraction has already landed there and saying otherwise
        // would be a lie in the one place a user is about to act on it.
        err = if stage.is_some() || given_back {
            err.now("nothing at the destination was changed")
        } else {
            err.now(format!(
                "some of it may already be in {} — check before retrying",
                dest.display()
            ))
        };
        return Err(err.into_err());
    }
    status_code(status)
}

/// Hand back a destination this copy had to create and could not fill,
/// answering whether it went.
///
/// `remove_dir` and not `remove_dir_all`, on purpose: it refuses a
/// directory with anything in it, so a copy that half-extracted keeps
/// what landed — which the caller then reports — and only a directory
/// that stayed empty, the one the user never had, goes away. Deleting a
/// tree here to tidy up would be the one outcome they cannot undo.
fn give_back(dest: &Path) -> bool {
    std::fs::remove_dir(dest).is_ok()
}

/// Extract the copy that came back into `into`, leaving `into`'s own
/// mode alone and every entry below it with the mode the far side sent.
///
/// Both halves of that are measured, and neither is free:
///
///   * `-p`, because without it the local umask eats the bits the far
///     side sent — a 0666 file arrived 0644 and a 0777 directory arrived
///     0755, while real `docker cp` preserves the mode;
///   * and then the destination's own mode is put BACK, because the
///     remote packs `tar -C "$S" -cf - .` and so the archive's first
///     entry is the staging directory itself, which `mktemp -d` made
///     0700. With `-p` that mode lands on the destination: measured
///     with bsdtar 3.5.3, a 0755 directory came out 0700 while the 0777
///     directory inside it stayed 0777. Docker's own archive is rooted
///     at the copied entry and never touches the destination at all, so
///     an unrestored `docker cp ctr:/x ./served-dir` silently takes a
///     shared directory private.
///
/// A function, and not four lines inline, so that second part can be
/// asserted at all: it had no test anywhere, and it fails silently.
fn unpack(into: &Path, stream: Stdio) -> Result<std::process::ExitStatus> {
    let keep = std::fs::metadata(into).map(|m| m.permissions()).ok();
    let unpacked = Command::new("tar")
        .arg("-xpf")
        .arg("-")
        .arg("-C")
        .arg(into)
        .stdin(stream)
        .status()
        .context("cannot run tar to unpack the copy (is tar installed?)")?;
    // Whatever tar made of it — a failed extraction hands the mode over
    // just as readily as a good one, having already unpacked the root
    // entry before it stopped.
    if let Some(mode) = keep {
        let _ = std::fs::set_permissions(into, mode);
    }
    Ok(unpacked)
}

/// Put the staged tree where the user asked for it.
///
/// The guard is Docker's own. `archive.PrepareArchiveCopy` answers
/// `ErrCannotCopyDir` when the source is a directory and the destination
/// is not one, and measured on 29.4.0 `docker cp c:/tree ./target.txt`
/// prints "cannot copy directory", exits 1, and leaves `target.txt`
/// holding exactly what it held before. Renaming the tree over it
/// instead DELETED the file the user had and exited 0 — the one outcome
/// they cannot undo, and the reason this is a function with a test
/// rather than three lines inline.
///
/// `exists` and not `symlink_metadata`, because a dangling symlink is
/// not something Docker treats as being in the way: measured, it follows
/// the link and creates the tree at the far end of it.
fn place(staged: &Path, dest: &Path) -> Result<()> {
    if std::fs::symlink_metadata(staged).is_ok_and(|m| m.is_dir()) && dest.exists() {
        return Err(fail!("cannot copy a directory onto {}", dest.display())
            .now("`docker cp` refuses this too, and nothing here was changed")
            .now("name a directory to copy INTO, or remove the file first")
            .into_err());
    }
    if dest.is_file() || dest.is_symlink() {
        std::fs::remove_file(dest).with_context(|| format!("cannot replace {}", dest.display()))?;
    }
    std::fs::rename(staged, dest)
        .with_context(|| format!("cannot put {} at {}", staged.display(), dest.display()))
}

/// A staging directory that removes itself.
///
/// Beside the destination rather than in `/tmp`, because the last step
/// is a rename and a rename does not cross filesystems. Named with the
/// clock as well as the pid: a leaked `.ulak-cp-<pid>` from an
/// interrupted run would otherwise make the next run that draws the
/// same pid fail at `create_dir`, and pid reuse under Linux's default
/// 32768 is routine rather than exotic.
struct Staging {
    path: PathBuf,
}

const STAGE_PREFIX: &str = ".ulak-cp-";

/// What `land` calls its half-written output while it is still writing
/// it. Named here because `sweep` clears these too: the two litters are
/// left behind by the same interruption and have the same nobody to
/// come back for them.
const PARTIAL_SUFFIX: &str = ".ulak-partial";

/// How long a leaked staging directory sits before the next copy through
/// the same directory clears it. One only exists while a single `docker
/// cp` is running, so a day is far longer than any live one — and the
/// alternative to a threshold is deleting a directory a concurrent copy
/// is still extracting into.
const STALE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

impl Staging {
    fn beside(parent: &Path) -> Result<Staging> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = parent.join(format!("{STAGE_PREFIX}{}-{stamp:08x}", std::process::id()));
        std::fs::create_dir(&path).with_context(|| format!("cannot create {}", path.display()))?;
        Ok(Staging { path })
    }
}

/// Clear what a previous command could not: staging directories, and
/// the half-written outputs `land` names `<file>.ulak-partial`.
///
/// `Drop` does not run on `std::process::exit`, and it does not run on
/// the Ctrl-C that kills this process either — so an interrupted copy
/// leaves its staging directory behind, and an interrupted `docker save
/// -o api.tar` leaves `api.tar.ulak-partial`, both in the user's own
/// workspace, where nothing else will ever remove them. They do not even
/// travel: the sync skips both names (walk.rs), so they simply
/// accumulate. The next command writing into the same directory is the
/// one thing that reliably comes along, so it does the clearing —
/// `land` before it opens its own partial, and `cp` before either
/// direction runs.
fn sweep(parent: &Path, older_than: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(STAGE_PREFIX) && !name.ends_with(PARTIAL_SUFFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= older_than);
        if !stale {
            continue;
        }
        // A staging leftover is a directory and a partial is a file, but
        // which one a name means is the name's claim, not a fact — so
        // ask, rather than call `remove_dir_all` on something that might
        // be an ordinary file the user made.
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        } else {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The directory a path sits in, with a bare name meaning the current
/// one. `Path::parent` answers `Some("")` for `api.tar`, and an empty
/// path is not a directory anything can be read from or written into.
fn holding(path: &Path) -> PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf()
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The single top-level entry a `docker cp` staging tree holds. More
/// than one means the far side sent something this code did not ask
/// for, and guessing which to keep would be worse than saying so.
fn one_entry(stage: &Path) -> Result<PathBuf> {
    let mut found = Vec::new();
    for entry in
        std::fs::read_dir(stage).with_context(|| format!("cannot read {}", stage.display()))?
    {
        found.push(entry?.path());
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(fail!("the copy arrived empty")
            .now("check that the path exists inside the container")
            .into_err()),
        n => Err(fail!("the copy arrived as {n} separate entries")
            .now("report this: `docker cp` produces exactly one")
            .into_err()),
    }
}

// ─── shared argv reading ────────────────────────────────────────────

/// Take the flag that names the local file out of `args` and return its
/// value. `None` when the flag is absent.
///
/// Every spelling pflag accepts, because the ones this used to miss are
/// the whole failure this module exists to prevent. `docker save
/// -oapi.tar api` is legal — measured, it wrote a 168 MB tar — and the
/// attached form went unread, so the path stayed in argv and `api.tar`
/// was written on the SERVER. `docker save -zout.tar` answering "unknown
/// shorthand flag: 'z' in -zout.tar" is pflag proving it really parses
/// the form rather than treating the word as one long name.
///
/// So: `--output VALUE`, `--output=VALUE`, `-o VALUE`, `-oVALUE`,
/// `-o=VALUE`, and each of those again behind a bundle of bool
/// shorthands — `docker load -qi api.tar` and `-qiapi.tar` both load.
/// Every occurrence of the carrier, and the LAST one is the answer.
///
/// pflag's rule for a repeated string flag, which `buildflags::dockerfile`
/// already measured for `-f`, and which was measured here too on 29.4.0:
/// `docker save -o first.tar -o second.tar alpine` writes second.tar and
/// never creates first.tar, `-ofirst.tar --output=second.tar` does the
/// same across spellings, and `docker load -i good.tar -i nosuch.tar`
/// complains about nosuch.tar.
///
/// Stopping at the FIRST one left the second in argv for the server to
/// open. Three things then happened at once and none of them said so:
/// the whole archive was written to `~/second.tar` on the SERVER, remote
/// stdout carried nothing, and `land` renamed that nothing over the
/// user's existing `first.tar` — with ssh exiting 0 through all of it.
fn take_flag(
    args: &mut Vec<String>,
    from: usize,
    flags: &[&str],
    bools: &[&str],
) -> Result<Option<String>> {
    let mut last = None;
    // Each turn takes one carrier out — a whole word, or one letter and
    // its value out of a bundle — so the tail is strictly shorter every
    // time and this cannot spin.
    while let Some(value) = take_one(args, from, flags, bools)? {
        last = Some(value);
    }
    Ok(last)
}

/// The FIRST carrier in the tail, taken out of `args`. On its own this
/// is the pflag reading of one occurrence and nothing more; `take_flag`
/// calls it until the tail runs out.
fn take_one(
    args: &mut Vec<String>,
    from: usize,
    flags: &[&str],
    bools: &[&str],
) -> Result<Option<String>> {
    if flags.is_empty() {
        return Ok(None);
    }
    let mut i = from;
    while i < args.len() {
        let arg = args[i].clone();
        // pflag stops reading flags at `--`, and so must this: `docker
        // save -- -o` names an IMAGE called `-o`, and taking it as the
        // output flag would have eaten it.
        if arg == "--" {
            return Ok(None);
        }
        // `--output=VALUE` and `-o=VALUE` both land here, because the
        // short flag is spelled out in `flags` as well.
        if let Some((flag, value)) = arg.split_once('=')
            && flags.contains(&flag)
        {
            args.remove(i);
            // pflag strips one `=` from a SHORT flag's attached value
            // only when something follows it, so the two spellings part
            // company where the value runs out. Measured on 29.4.0:
            // `docker save -o=` wrote a 4 MB archive to a file honestly
            // named `=`, while `docker save --output=` wrote it to
            // stdout. Reading the short form as an empty name sent the
            // image nowhere at all — see `land`.
            let short = flag.len() == 2 && !flag.starts_with("--");
            return Ok(Some(if value.is_empty() && short {
                "=".to_string()
            } else {
                value.to_string()
            }));
        }
        if flags.contains(&arg.as_str()) {
            let value = args.get(i + 1).cloned().ok_or_else(|| needs_a_file(&arg))?;
            args.drain(i..=i + 1);
            return Ok(Some(value));
        }
        if arg.len() > 1
            && arg.starts_with('-')
            && !arg.starts_with("--")
            && let Some(value) = take_bundled(args, i, flags, bools)?
        {
            return Ok(Some(value));
        }
        i += 1;
    }
    Ok(None)
}

fn needs_a_file(flag: &str) -> anyhow::Error {
    fail!("{flag} needs a file path")
        .now(format!("for example: {flag} out.tar"))
        .into_err()
}

/// One pflag shorthand bundle, read the way pflag reads it: the letters
/// run left to right, bools carry on to the next letter, and the first
/// letter that takes a VALUE swallows the rest of the word — measured,
/// `docker load -iq api.tar` reads its input from a file literally
/// called `q` and then complains that `api.tar` is a stray argument.
///
/// `Ok(None)` means "not a flag this command carries a file in"; the
/// word is left for the server to reject in Docker's own words. What is
/// never left is a word that MIGHT be carrying one — see the refusal.
fn take_bundled(
    args: &mut Vec<String>,
    at: usize,
    flags: &[&str],
    bools: &[&str],
) -> Result<Option<String>> {
    let word = args[at].clone();
    // The bools ahead of the carrier stay: `-qi api.tar` still means
    // `--quiet` once the `-i` and its path are gone.
    let mut kept = String::from("-");
    let mut rest = &word[1..];
    while !rest.is_empty() {
        let here = format!("-{rest}");
        // `-oapi.tar`, here and after any number of bools. The same
        // helper the audit trail reads `-phunter2` with, because it is
        // the same pflag rule: a shorthand swallows the rest of its own
        // word.
        if let Some(flag) = crate::passthrough::attached_short(&here, flags) {
            let tail = &here[flag.len()..];
            let value = tail.strip_prefix('=').unwrap_or(tail).to_string();
            if kept.len() > 1 {
                args[at] = kept;
            } else {
                args.remove(at);
            }
            return Ok(Some(value));
        }
        if flags.contains(&here.as_str()) {
            let value = args
                .get(at + 1)
                .cloned()
                .ok_or_else(|| needs_a_file(&here))?;
            args.remove(at + 1);
            if kept.len() > 1 {
                args[at] = kept;
            } else {
                args.remove(at);
            }
            return Ok(Some(value));
        }
        let c = rest.chars().next().expect("rest is not empty");
        rest = &rest[c.len_utf8()..];
        if bools.contains(&format!("-{c}").as_str()) {
            // pflag lets a bool take a value too, and it eats the rest
            // of the word when it does: `-q=truei` is an invalid bool,
            // not `-q -i`.
            if rest.starts_with('=') {
                return Ok(None);
            }
            kept.push(c);
            continue;
        }
        // An unknown shorthand ends the reading, and what is left of the
        // word decides what happens. A remainder with no carrier letter
        // in it cannot be hiding a local path, so it travels and Docker
        // says "unknown shorthand flag" in its own words. A remainder
        // that still holds one might be `-xo out.tar`, and guessing
        // wrong writes the file on the server.
        return match flags.iter().find(|f| f.len() == 2 && rest.contains(&f[1..])) {
            Some(carrier) => Err(fail!("cannot tell what {word} means")
                .now(format!(
                    "it holds `{carrier}`, which names a file on THIS machine, behind a flag Ulak does not know"
                ))
                .now(format!("spell the file out on its own: {carrier} out.tar"))
                .into_err()),
            None => Ok(None),
        };
    }
    Ok(None)
}

/// Every argument at or after `from` that is a positional — i.e. not a
/// flag, and not the VALUE of a flag that takes one — in the order the
/// user typed them.
///
/// One scan for the whole tail, rather than one scan per positional,
/// because the end of flag parsing is a fact about the WHOLE argv and a
/// second scan cannot inherit it: starting again past a `--` the first
/// scan crossed reads the word after it as a flag, and the file it names
/// goes unfound. See `create_source` for what that cost.
fn positionals(args: &[String], from: usize, value_flags: &[&str]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut i = from;
    let mut only_paths = false;
    while i < args.len() {
        let arg = &args[i];
        // `--` ends flag parsing, so everything after it is a positional
        // however much it looks like a flag — the only way to name a
        // file called `-weird.tar`. Without this the file was never
        // found, the path stayed in argv, and the SERVER opened it.
        if !only_paths && arg == "--" {
            only_paths = true;
            i += 1;
            continue;
        }
        if only_paths || arg == "-" || !arg.starts_with('-') {
            found.push(i);
            i += 1;
            continue;
        }
        let flag = arg.split('=').next().unwrap_or(arg);
        if value_flags.contains(&flag) && !arg.contains('=') {
            i += 1;
        }
        i += 1;
    }
    found
}

/// The first argument at or after `from` that is a positional.
fn first_positional(args: &[String], from: usize, value_flags: &[&str]) -> Option<usize> {
    positionals(args, from, value_flags).first().copied()
}

/// Docker's own rule for `CONTAINER:PATH`, from `splitCpArg`: a leading
/// `/` or `.` makes it a local path however many colons it contains, so
/// `./a:b.txt` is a file and not a container named `./a`. Everything
/// else with a colon in it names a container, and the PATH may be empty.
///
/// Three measurements, and each of them corrects a rule that was not
/// Docker's:
///
///   * `docker cp web: ./x` answers "bad parameter: path cannot be
///     empty" — from the DAEMON, so a trailing colon IS a container
///     reference. Demanding a non-empty path read it as a local file
///     instead, and `docker cp ./local.txt web:` was then forwarded as
///     typed, for the SERVER to read `./local.txt` off its own disk.
///   * `docker cp :/x ./y` answers "must specify at least one container
///     source": an empty NAME is not a container.
///   * `docker cp '~/x:y' ./z` answers "No such container: ~/x". The `~`
///     arm this used to carry was invention — a shell expands `~/` long
///     before Docker sees it, and Docker has no rule for the ones it
///     does not.
fn split_container(arg: &str) -> Option<(String, String)> {
    if arg.starts_with('/') || arg.starts_with('.') {
        return None;
    }
    let (container, path) = arg.split_once(':')?;
    (!container.is_empty()).then(|| (container.to_string(), path.to_string()))
}

/// A source the SERVER fetches, not one we read: `docker import` takes
/// a URL as happily as a file.
///
/// Exactly two schemes, lower case, because that is what `docker import`
/// recognises. Measured on 29.4.0: `ftp://host/x.tar` and `HTTPS://…`
/// both answer `open …: no such file or directory`, which is Docker
/// opening them as LOCAL FILES. Calling every `scheme://` a URL sent a
/// file honestly named `a://b` to the server to open, on the server's
/// own disk.
fn is_remote_source(raw: &str) -> bool {
    raw.starts_with("http://") || raw.starts_with("https://")
}

/// Absolute, with `.` and `..` folded away lexically.
///
/// The folding is what makes `docker cp nest/inner/.. ctr:/app` work:
/// `Path::file_name` is `None` for anything ending in `..`, so without
/// it the copy died on "cannot copy a path with no name" while plain
/// docker copies the parent. Lexical rather than by asking the
/// filesystem, on purpose — Docker's own client cleans the path the
/// same way, and matching it matters more than resolving a symlink the
/// user did not ask about.
fn absolute(p: &Path) -> Result<PathBuf> {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .context("cannot read current directory")?
            .join(p)
    };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    const OUT: &[&str] = &["-o", "--output"];

    fn asks_for_help(args: &[&str]) -> bool {
        help_wanted(&crate::catalog::resolve(&v(args)).unwrap())
    }

    /// A help request is a flag, not a word that appears somewhere.
    ///
    /// Measured on Docker 29.4.0: `docker save -o --help alpine:latest`
    /// exits 0 having written a 4.2 MB archive to a file named
    /// `--help`, and `docker load -qi --help` reads one back. Read as
    /// help, both were forwarded verbatim and the file was the
    /// SERVER's.
    #[test]
    fn only_a_real_help_flag_is_forwarded_as_one() {
        for asking in [
            vec!["save", "--help"],
            vec!["save", "-h"],
            vec!["load", "--help"],
            vec!["export", "-h"],
            vec!["cp", "--help"],
            vec!["import", "--help"],
            // The carrier is read and gone; the request behind it stands.
            vec!["save", "-o", "out.tar", "--help", "api"],
            vec!["save", "--help", "-o", "out.tar"],
            // A carrier with nothing after it is the handler's refusal
            // to raise, not this one's — and help still prints.
            vec!["save", "--help", "-o"],
        ] {
            assert!(asks_for_help(&asking), "{asking:?} asks for help");
        }
        for filing in [
            vec!["save", "-o", "--help", "alpine:latest"],
            vec!["save", "-o", "-h", "alpine:latest"],
            vec!["save", "--output", "--help", "alpine:latest"],
            vec!["export", "-o", "--help", "api"],
            vec!["load", "-i", "--help"],
            vec!["load", "-qi", "--help"],
            vec!["buildx", "history", "export", "-o", "--help"],
            // Already right, and it has to stay right: past `--`,
            // `--help` is a path Docker tries to lstat.
            vec!["cp", "--", "--help", "ctr:/x"],
        ] {
            assert!(
                !asks_for_help(&filing),
                "{filing:?} names a file called --help, it does not ask for help"
            );
        }
    }

    #[test]
    fn the_output_flag_leaves_argv_in_all_three_spellings() {
        for spelling in [
            vec!["save", "-o", "api.tar", "api:latest"],
            vec!["save", "--output", "api.tar", "api:latest"],
            vec!["save", "--output=api.tar", "api:latest"],
        ] {
            let mut args = v(&spelling);
            let got = take_flag(&mut args, 1, OUT, &[]).unwrap();
            assert_eq!(got.as_deref(), Some("api.tar"), "{spelling:?}");
            assert_eq!(
                args,
                v(&["save", "api:latest"]),
                "the flag must not reach the server: {spelling:?}"
            );
        }
    }

    /// A repeated carrier: the LAST one wins, and none of them is left
    /// behind for the server.
    ///
    /// Measured on 29.4.0: `docker save -o first.tar -o second.tar
    /// alpine:3.20` exits 0 having written second.tar, and first.tar is
    /// never created. Reading only the first left `-o second.tar` in
    /// argv, so three things happened at once and none of them said so —
    /// the archive was written to `~/second.tar` on the SERVER, remote
    /// stdout carried nothing, and that nothing was renamed over the
    /// user's existing `first.tar` with ssh exiting 0.
    #[test]
    fn a_repeated_carrier_is_read_to_the_last_one_and_none_are_forwarded() {
        for (spelling, want) in [
            (
                vec!["save", "-o", "first.tar", "-o", "second.tar", "api"],
                "second.tar",
            ),
            (
                vec!["save", "-o", "first.tar", "--output=second.tar", "api"],
                "second.tar",
            ),
            (
                vec!["save", "-ofirst.tar", "-o", "second.tar", "api"],
                "second.tar",
            ),
            (
                vec!["save", "--output", "first.tar", "-osecond.tar", "api"],
                "second.tar",
            ),
            // Three, because "the last one" and "the second one" are the
            // same answer for two and this has to be the former.
            (
                vec!["save", "-o", "a.tar", "-o", "b.tar", "-o", "c.tar", "api"],
                "c.tar",
            ),
        ] {
            let mut args = v(&spelling);
            let got = take_flag(&mut args, 1, OUT, &[]).unwrap();
            assert_eq!(got.as_deref(), Some(want), "{spelling:?}");
            assert_eq!(
                args,
                v(&["save", "api"]),
                "a carrier survived into argv, so the server opens it: {spelling:?}"
            );
        }

        // The bundled form, where each turn shortens a word rather than
        // removing one — the shape that could have spun.
        let mut args = v(&["load", "-qi", "first.tar", "-qi", "second.tar"]);
        let got = take_flag(&mut args, 1, &["-i", "--input"], &["-q"]).unwrap();
        assert_eq!(got.as_deref(), Some("second.tar"));
        assert_eq!(args, v(&["load", "-q", "-q"]));
    }

    #[test]
    fn the_attached_shorthand_is_read_and_not_forwarded() {
        // `docker save -oapi.tar api` is legal — measured, it wrote a
        // 168 MB tar — and going unread meant the path stayed in argv
        // and the archive was written on the SERVER.
        for spelling in [
            vec!["save", "-oapi.tar", "api:latest"],
            vec!["save", "-o=api.tar", "api:latest"],
        ] {
            let mut args = v(&spelling);
            let got = take_flag(&mut args, 1, OUT, &[]).unwrap();
            assert_eq!(got.as_deref(), Some("api.tar"), "{spelling:?}");
            assert_eq!(args, v(&["save", "api:latest"]), "{spelling:?}");
        }
        // A name that begins with `=` survives, because pflag strips
        // exactly one: `docker save -o==eq.tar` wrote `=eq.tar`.
        let mut args = v(&["save", "-o==eq.tar", "api:latest"]);
        assert_eq!(
            take_flag(&mut args, 1, OUT, &[]).unwrap().as_deref(),
            Some("=eq.tar")
        );
    }

    #[test]
    fn a_bool_bundle_in_front_of_the_carrier_is_split_not_forwarded() {
        // `docker load -qi api.tar` and `-qiapi.tar` both load. The `-q`
        // is the far side's business and stays; the path is ours.
        for spelling in [
            vec!["load", "-qi", "api.tar"],
            vec!["load", "-qiapi.tar"],
            vec!["load", "-qi=api.tar"],
        ] {
            let mut args = v(&spelling);
            let got = take_flag(&mut args, 1, &["-i", "--input"], &["-q"]).unwrap();
            assert_eq!(got.as_deref(), Some("api.tar"), "{spelling:?}");
            assert_eq!(args, v(&["load", "-q"]), "{spelling:?}");
        }
        // `docker buildx history export -Doout.tar` writes `out.tar` —
        // measured, and the same shape with a different bool.
        let mut args = v(&["buildx", "history", "export", "-Doout.tar"]);
        assert_eq!(
            take_flag(&mut args, 3, OUT, &["-D"]).unwrap().as_deref(),
            Some("out.tar")
        );
        assert_eq!(args, v(&["buildx", "history", "export", "-D"]));
    }

    #[test]
    fn a_bundle_that_might_be_hiding_a_local_path_is_refused() {
        // `-x` is not a flag this command has, so what `o` means here is
        // unknowable — and forwarding it would write `out.tar` on the
        // server. Refusing costs a minute; guessing costs an afternoon.
        let mut args = v(&["save", "-xo", "out.tar", "api"]);
        let err = take_flag(&mut args, 1, OUT, &[]).unwrap_err();
        assert!(
            err.to_string().contains("cannot tell what -xo means"),
            "{err}"
        );

        // Without a carrier letter in it there is nothing to hide, so
        // the word travels and Docker gives its own "unknown shorthand
        // flag" in its own words.
        let mut args = v(&["save", "-z", "api"]);
        assert!(take_flag(&mut args, 1, OUT, &[]).unwrap().is_none());
        assert_eq!(args, v(&["save", "-z", "api"]));
    }

    #[test]
    fn a_flag_without_its_file_is_reported_not_swallowed() {
        let mut args = v(&["save", "-o"]);
        let err = take_flag(&mut args, 1, OUT, &[]).unwrap_err();
        assert!(err.to_string().contains("needs a file path"), "{err}");
        // The same word behind a bundle, where the value is the next
        // argument that is not there.
        let mut args = v(&["load", "-qi"]);
        let err = take_flag(&mut args, 1, &["-i", "--input"], &["-q"]).unwrap_err();
        assert!(err.to_string().contains("needs a file path"), "{err}");
    }

    #[test]
    fn no_output_flag_means_the_stream_stays_a_stream() {
        let mut args = v(&["save", "api:latest"]);
        assert!(take_flag(&mut args, 1, OUT, &[]).unwrap().is_none());
        assert_eq!(args, v(&["save", "api:latest"]));
        // Past `--` there are no flags left to find: `docker save -- -o`
        // names an IMAGE, and Docker answers "invalid reference format".
        let mut args = v(&["save", "--", "-o", "api.tar"]);
        assert!(take_flag(&mut args, 1, OUT, &[]).unwrap().is_none());
        assert_eq!(args, v(&["save", "--", "-o", "api.tar"]));
    }

    #[test]
    fn a_flags_value_is_never_mistaken_for_the_positional() {
        // `-m` takes a message, so `imported` is its value and `x.tar`
        // is the file. Reading left to right without knowing that would
        // have tried to open "imported" and reported it missing.
        let args = v(&["import", "-m", "imported", "x.tar", "repo:tag"]);
        assert_eq!(
            first_positional(&args, 1, IMPORT_VALUE_FLAGS).map(|i| args[i].as_str()),
            Some("x.tar")
        );
        // The inline spelling consumes no following word.
        let args = v(&["import", "--message=imported", "x.tar"]);
        assert_eq!(
            first_positional(&args, 1, IMPORT_VALUE_FLAGS).map(|i| args[i].as_str()),
            Some("x.tar")
        );
        // A bare `-` IS the positional.
        let args = v(&["import", "-", "repo:tag"]);
        assert_eq!(first_positional(&args, 1, IMPORT_VALUE_FLAGS), Some(1));
    }

    #[test]
    fn a_positional_behind_the_terminator_is_still_found() {
        // `--` is the only way to name a file that starts with a dash,
        // and skipping it left `-weird.tar` in argv for the SERVER to
        // open — the exact wrong-machine read this module exists to
        // prevent.
        let args = v(&["import", "--", "-weird.tar", "repo:tag"]);
        assert_eq!(
            first_positional(&args, 1, IMPORT_VALUE_FLAGS).map(|i| args[i].as_str()),
            Some("-weird.tar")
        );
        // A terminator with nothing after it names no file.
        let args = v(&["import", "--"]);
        assert_eq!(first_positional(&args, 1, IMPORT_VALUE_FLAGS), None);
    }

    #[test]
    fn secret_create_finds_the_file_after_the_name() {
        let mut args = v(&["secret", "create", "-l", "env=prod", "api-key", "key.txt"]);
        assert_eq!(create_source(&mut args, 2).as_deref(), Some("key.txt"));
        assert_eq!(
            args,
            v(&["secret", "create", "-l", "env=prod", "api-key", "-"]),
            "and the path does not reach the server"
        );
        // `-d` takes a driver, so the word after it is its value and not
        // the NAME — reading left to right without knowing that made
        // `vault` the name and `api-key` the file.
        let mut args = v(&["secret", "create", "-d", "vault", "api-key", "key.txt"]);
        assert_eq!(create_source(&mut args, 2).as_deref(), Some("key.txt"));
    }

    #[test]
    fn a_dash_leading_secret_file_behind_the_terminator_is_still_ours_to_read() {
        // Measured on 29.4.0: `docker secret create -- key -secret.txt`
        // answers `error reading from -secret.txt: open -secret.txt: no
        // such file or directory`, so past `--` that word IS the file —
        // and without the terminator Docker refuses it outright as
        // `unknown shorthand flag: 's' in -secret.txt`, which makes `--`
        // the only way to name a secret file that starts with a dash.
        //
        // Finding NAME and then starting a SECOND scan past it lost the
        // memory that flag parsing had already ended: the second scan
        // read `-secret.txt` as a flag, found no file at all, and left
        // the path in argv for the SERVER to open its own. A secret,
        // read off the wrong machine, silently.
        let mut args = v(&["secret", "create", "--", "key", "-secret.txt"]);
        assert_eq!(create_source(&mut args, 2).as_deref(), Some("-secret.txt"));
        assert_eq!(args, v(&["secret", "create", "--", "key", "-"]));
        // The same shape for `config create`, which shares the parse.
        let mut args = v(&["config", "create", "--", "name", "-f.txt"]);
        assert_eq!(create_source(&mut args, 2).as_deref(), Some("-f.txt"));
    }

    #[test]
    fn the_local_file_leaves_argv_as_a_dash() {
        // The whole streaming trick is this rewrite: the bytes go up on
        // stdin and argv carries a `-`. Leave the path in and argv is
        // forwarded verbatim, so the server opens its own file of that
        // name — which for `secret create` is the wrong machine's
        // secret, written into an image, with nothing said about it.
        let mut args = v(&["import", "./rootfs.tar", "repo:tag"]);
        assert_eq!(import_source(&mut args, 1).as_deref(), Some("./rootfs.tar"));
        assert_eq!(args, v(&["import", "-", "repo:tag"]));

        let mut args = v(&["secret", "create", "api-key", "secrets/api-key.txt"]);
        assert_eq!(
            create_source(&mut args, 2).as_deref(),
            Some("secrets/api-key.txt")
        );
        assert_eq!(args, v(&["secret", "create", "api-key", "-"]));

        // A URL is the server's to fetch, so argv keeps it and nothing
        // here tries to open it.
        let url = "https://example.invalid/rootfs.tar";
        let mut args = v(&["import", url, "repo:tag"]);
        assert!(import_source(&mut args, 1).is_none());
        assert_eq!(args, v(&["import", url, "repo:tag"]));

        // A `-` the user typed is already the stream form, and our
        // stdin is theirs: nothing to open, nothing to rewrite.
        let mut args = v(&["secret", "create", "api-key", "-"]);
        assert!(create_source(&mut args, 2).is_none());
        assert_eq!(args, v(&["secret", "create", "api-key", "-"]));
        let mut args = v(&["import", "-", "repo:tag"]);
        assert!(import_source(&mut args, 1).is_none());
        assert_eq!(args, v(&["import", "-", "repo:tag"]));

        // `docker secret create NAME` with no file reads stdin too.
        let mut args = v(&["secret", "create", "api-key"]);
        assert!(create_source(&mut args, 2).is_none());
        assert_eq!(args, v(&["secret", "create", "api-key"]));
    }

    #[test]
    fn a_container_reference_is_told_from_a_local_path() {
        assert_eq!(
            split_container("web:/app"),
            Some(("web".into(), "/app".into()))
        );
        assert_eq!(
            split_container("a1b2c3:/etc/passwd"),
            Some(("a1b2c3".into(), "/etc/passwd".into()))
        );
        // A local path wins however many colons it holds — this is
        // Docker's own rule, and without it `./a:b.txt` would be read
        // as a container called `./a`.
        for local in ["./a:b.txt", "/tmp/a:b", "plain.txt"] {
            assert_eq!(split_container(local), None, "{local} is a local path");
        }
        // An empty NAME is not a container: `docker cp :/x ./y` answers
        // "must specify at least one container source".
        assert_eq!(split_container(":/x"), None);
        // A `~` is: `docker cp '~/x:y' ./z` answers "No such container:
        // ~/x". A shell expands `~/` long before Docker sees it, so the
        // arm that called this local was invention.
        assert_eq!(split_container("~/x:y"), Some(("~/x".into(), "y".into())));
    }

    #[test]
    fn a_trailing_colon_is_a_container_with_no_path() {
        // Measured: `docker cp web: ./x` answers "bad parameter: path
        // cannot be empty" — from the DAEMON, so Docker read it as a
        // container. Reading it as a local file sent `docker cp
        // ./local.txt web:` to the server AS TYPED, where `./local.txt`
        // is the server's own file.
        assert_eq!(split_container("web:"), Some(("web".into(), String::new())));
    }

    #[test]
    fn a_url_import_is_the_servers_to_fetch() {
        assert!(is_remote_source("https://example.invalid/rootfs.tar"));
        assert!(is_remote_source("http://example.invalid/rootfs.tar"));
        assert!(!is_remote_source("./rootfs.tar"));
        // `docker import` knows two schemes and no others. Measured:
        // `ftp://…`, `a://b` and `HTTPS://…` all answer `open …: no such
        // file or directory`, which is Docker opening them as files —
        // so they are ours to read, not the server's to fetch.
        for local in ["ftp://host/x.tar", "a://b", "HTTPS://host/x.tar"] {
            assert!(!is_remote_source(local), "{local} is a local file");
        }
    }

    #[test]
    fn cp_reads_its_flags_the_way_pflag_does() {
        // Bundled shorthands are what people type, and refusing `-aL`
        // meant refusing something plain `docker cp` accepts.
        let a = CpArgs::parse(&v(&["-aL", "f.txt", "web:/app/"])).unwrap();
        assert_eq!(a.opts, v(&["-a", "-L"]));
        assert!(a.follow && a.archive);
        assert_eq!((a.src.as_str(), a.dst.as_str()), ("f.txt", "web:/app/"));

        // `--` is the only way to name a source that starts with a dash.
        let a = CpArgs::parse(&v(&["--", "-weird.txt", "web:/app/"])).unwrap();
        assert!(a.opts.is_empty());
        assert_eq!(a.src, "-weird.txt");

        // A bare `-` is a stream, not a flag.
        let a = CpArgs::parse(&v(&["-", "web:/app/"])).unwrap();
        assert_eq!(a.src, "-");

        let err = CpArgs::parse(&v(&["-az", "f", "web:/x"])).unwrap_err();
        assert!(err.to_string().contains("no flag -z"), "{err}");
    }

    #[test]
    fn cp_refuses_an_empty_path_the_way_docker_does() {
        // Measured on 29.4.0: `docker cp "" web:/backup` answers
        // `source can not be empty` and `docker cp web:/f ""` answers
        // `destination can not be empty` — both exit 1 in the client,
        // with nothing moved. Ulak has more to lose by carrying on than
        // Docker does: an empty source resolves to the WORKING
        // DIRECTORY, so `cp "$SRC" web:/backup` with `SRC` unset packed
        // the whole project into the container and exited 0.
        for (spelling, end) in [
            (vec!["", "web:/backup"], "source"),
            (vec!["web:/app/out.txt", ""], "destination"),
            (vec!["-a", "", "web:/backup"], "source"),
        ] {
            let err = CpArgs::parse(&v(&spelling)).unwrap_err();
            assert!(
                err.to_string()
                    .contains(&format!("the {end} of a copy cannot be empty")),
                "{spelling:?}: {err}"
            );
        }
        // A bare `-` is still a stream and an ordinary path is still a
        // path: the guard is about the empty word alone.
        assert!(CpArgs::parse(&v(&["-", "web:/app/"])).is_ok());
        assert!(CpArgs::parse(&v(&[".", "web:/app/"])).is_ok());
    }

    #[test]
    fn cp_accepts_the_bool_spellings_pflag_accepts() {
        // pflag hands a bool's value to `strconv.ParseBool`, so all of
        // these are real. Measured on 29.4.0 — each one exited 0, and
        // this used to answer "`docker cp` has no flag --quiet=true".
        // Parsing is not the claim — TRAVELLING is. A `-q=1` that parses
        // and then drops `-q` on the floor never reaches the far side,
        // and the only sign is that `docker cp` was noisy when the user
        // asked for quiet.
        for (spelling, opts) in [
            (vec!["--quiet=true", "f", "web:/x"], vec!["-q"]),
            (vec!["-q=true", "f", "web:/x"], vec!["-q"]),
            (vec!["-q=1", "f", "web:/x"], vec!["-q"]),
            (vec!["-aL=true", "f", "web:/x"], vec!["-a", "-L"]),
        ] {
            let a = CpArgs::parse(&v(&spelling)).unwrap();
            assert_eq!(a.opts, v(&opts), "{spelling:?}");
            assert_eq!(a.archive, opts.contains(&"-a"), "{spelling:?}");
            assert_eq!(a.follow, opts.contains(&"-L"), "{spelling:?}");
            assert_eq!(
                (a.src.as_str(), a.dst.as_str()),
                ("f", "web:/x"),
                "and the flag is not read as a path: {spelling:?}"
            );
        }
        // And `false` means false: forwarding the word as typed would
        // have carried an archive copy nobody asked for.
        let a = CpArgs::parse(&v(&["-a=false", "f", "web:/x"])).unwrap();
        assert!(!a.archive, "-a=false turns -a off");
        assert!(a.opts.is_empty(), "and it must not travel: {:?}", a.opts);
        let a = CpArgs::parse(&v(&["--archive=false", "-L", "f", "web:/x"])).unwrap();
        assert_eq!(a.opts, v(&["-L"]));

        // Docker's own refusal: `invalid argument "yes" for "-a,
        // --archive" flag`.
        let err = CpArgs::parse(&v(&["-a=yes", "f", "web:/x"])).unwrap_err();
        assert!(err.to_string().contains("true or false"), "{err}");
    }

    #[test]
    fn only_a_trailing_slash_dot_means_the_contents() {
        assert_eq!(split_contents_only("./src/."), ("./src", true));
        assert_eq!(split_contents_only("/a/b/."), ("/a/b", true));
        assert_eq!(split_contents_only("."), (".", true));
        // A file honestly named `data.` — trimming dots blindly turned
        // this into a missing path.
        assert_eq!(split_contents_only("./data."), ("./data.", false));
        // And `..` is the PARENT, not the same directory with a dot
        // chopped off.
        assert_eq!(split_contents_only("./dir/.."), ("./dir/..", false));
        assert_eq!(split_contents_only("./src"), ("./src", false));
    }

    #[test]
    fn cp_takes_only_the_three_flags_it_documents() {
        // The list is small enough to write down, which is exactly why
        // "the two non-flag words are the paths" is safe here.
        for (flag, means) in [
            ("-a", CpFlag::Archive),
            ("--archive", CpFlag::Archive),
            ("-L", CpFlag::Follow),
            ("--follow-link", CpFlag::Follow),
            ("-q", CpFlag::Quiet),
            ("--quiet", CpFlag::Quiet),
        ] {
            assert_eq!(cp_flag(flag), Some(means));
        }
        assert_eq!(cp_flag("-o"), None);
    }

    // ─── docker cp, remote lines ────────────────────────────────────

    #[test]
    fn dockers_own_globals_reach_the_line_both_scripts_build() {
        // `resolve` keeps Docker's globals in front of the command for
        // every other route; these two rebuilt the command by hand and
        // dropped them, so `--log-level debug` did nothing on `cp` and
        // everything everywhere else.
        let globals = v(&["--log-level", "debug"]);
        for archive in [false, true] {
            let up = up_script(
                &globals,
                &v(&["-q"]),
                "f.txt",
                "f.txt",
                "web:/app/",
                archive,
            );
            assert!(up.contains("docker --log-level debug cp -q"), "{up}");
        }
        let down = down_script(&globals, &v(&["-L"]), "web:/etc/hosts");
        assert!(down.contains("docker --log-level debug cp -L"), "{down}");
        // With no globals the line is exactly what it always was.
        let plain = up_script(&[], &[], "f.txt", "f.txt", "web:/app/", true);
        assert_eq!(plain, "docker cp - web:/app/");
    }

    #[test]
    fn a_source_that_could_not_be_read_whole_says_what_reached_the_container() {
        // tar exits non-zero for ONE unreadable member (2 for GNU, 1 for
        // bsdtar) while still writing a complete, valid archive of
        // everything else — so the far side unpacked it and the real
        // `docker cp` ran. `ulak docker cp ./volumes/db ctr:/restore`,
        // where postgres wrote files as uid 999, is the everyday case.
        // "could not read ./volumes/db to send it" was true and read as
        // "nothing happened", which is the one thing it does not mean.
        let err = crate::ui::flatten(&incomplete_send("./volumes/db", "ctr:/restore"));
        assert!(err.contains("could not read all of ./volumes/db"), "{err}");
        assert!(
            err.contains("ctr:/restore"),
            "the container destination has changed and must be named: {err}"
        );
    }

    #[test]
    fn the_untar_keeps_the_permission_bits_the_far_side_sent() {
        // Measured: without `-p` the umask ate them — a 0666 file came
        // out 0644 and a 0777 directory came out 0755, while real
        // `docker cp` carries the mode across untouched.
        let up = up_script(&[], &[], "f.txt", "f.txt", "web:/app/", false);
        assert!(up.contains("tar -xpf -"), "{up}");
    }

    #[test]
    fn following_a_link_sends_its_target_under_the_links_own_name() {
        // `-L` follows the SOURCE path's own symlink and nothing else.
        // The staged entry arrives named after the TARGET, so the remote
        // line renames it back before the real `docker cp` sees it —
        // `docker cp -L link ctr:/d/` lands `/d/link`, measured.
        let script = up_script(&[], &[], "real", "link", "web:/d/", false);
        assert!(
            script.contains("mv \"$S\"/real \"$S\"/link; docker"),
            "{script}"
        );
        // Nothing to rename when the names already agree.
        let script = up_script(&[], &[], "f.txt", "f.txt", "web:/d/", false);
        assert!(!script.contains("mv "), "{script}");
    }

    #[test]
    fn a_link_is_resolved_to_what_it_points_at_and_no_further() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("real/inner.txt"), b"INNER").unwrap();
        std::os::unix::fs::symlink("inner.txt", root.join("real/ilink")).unwrap();
        std::os::unix::fs::symlink("real", root.join("link")).unwrap();

        let (dir, name) = followed(&root.join("link"), "link").unwrap();
        assert_eq!(dir, root);
        assert_eq!(
            name, "real",
            "the tar is packed from what the link points at"
        );
        // The inner link is untouched: `tar -h`, which this replaced,
        // dereferenced every link in the tree instead of just this one.
        assert!(root.join("real/ilink").is_symlink());

        // A link pointing at nothing is an error, as it is for Docker:
        // `lstat …/nowhere: no such file or directory`.
        std::os::unix::fs::symlink("nowhere", root.join("broken")).unwrap();
        let err = followed(&root.join("broken"), "broken").unwrap_err();
        assert!(err.to_string().contains("-L cannot follow"), "{err}");
    }

    // ─── docker cp, landing the tree ────────────────────────────────

    #[test]
    fn a_directory_is_never_put_where_a_file_already_is() {
        // Measured on 29.4.0: with `target.txt` holding PRECIOUS,
        // `docker cp c:/tree ./target.txt` printed "cannot copy
        // directory", exited 1, and target.txt still held PRECIOUS.
        // This used to remove the file and rename the tree over it — and
        // exit 0.
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("tree");
        std::fs::create_dir(&staged).unwrap();
        std::fs::write(staged.join("a.txt"), b"A").unwrap();

        let dest = tmp.path().join("target.txt");
        std::fs::write(&dest, b"PRECIOUS").unwrap();
        let err = place(&staged, &dest).unwrap_err();
        assert!(err.to_string().contains("cannot copy a directory"), "{err}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"PRECIOUS");
        assert!(staged.is_dir(), "and the staged tree is untouched");

        // A symlink to a file is in the way just as much: measured, the
        // same message and the same exit.
        let real = tmp.path().join("real.txt");
        std::fs::write(&real, b"REAL").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(place(&staged, &link).is_err());
        assert_eq!(std::fs::read(&real).unwrap(), b"REAL");
    }

    #[test]
    fn a_file_still_replaces_what_is_there_and_a_tree_still_lands_on_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        // A staged FILE replaces the file that is there — this is what
        // `docker cp c:/f.txt ./old.txt` does, and the guard above must
        // not have taken it away.
        let staged = tmp.path().join("f.txt");
        std::fs::write(&staged, b"NEW").unwrap();
        let dest = tmp.path().join("old.txt");
        std::fs::write(&dest, b"OLD").unwrap();
        place(&staged, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"NEW");

        // A staged TREE lands where nothing is.
        let staged = tmp.path().join("tree");
        std::fs::create_dir(&staged).unwrap();
        let dest = tmp.path().join("fresh");
        place(&staged, &dest).unwrap();
        assert!(dest.join("").is_dir());
    }

    #[test]
    fn an_interrupted_copy_does_not_leave_its_staging_directory_forever() {
        // `Drop` does not run on `std::process::exit` or on the Ctrl-C
        // that kills the process, so the directory outlives the copy —
        // inside the user's workspace, which the sync mirrors.
        let tmp = tempfile::tempdir().unwrap();
        let leaked = tmp.path().join(format!("{STAGE_PREFIX}1234-0000abcd"));
        std::fs::create_dir(&leaked).unwrap();
        std::fs::write(leaked.join("half"), b"x").unwrap();
        // The same interruption during `docker save -o api.tar` leaves
        // this instead, and it had no owner at all: `land` creates it,
        // only `land` removes it, and nothing ever came back for one
        // that outlived its command.
        let partial = tmp.path().join(format!("api.tar{PARTIAL_SUFFIX}"));
        std::fs::write(&partial, b"half an image").unwrap();
        let mine = tmp.path().join("keep-me");
        std::fs::create_dir(&mine).unwrap();
        let finished = tmp.path().join("api.tar");
        std::fs::write(&finished, b"a whole image").unwrap();

        // A copy that is still running must survive: this is what makes
        // sweeping at the start of every copy safe to do at all.
        sweep(tmp.path(), STALE);
        assert!(
            leaked.is_dir(),
            "a fresh staging directory belongs to a live copy"
        );
        assert!(
            partial.is_file(),
            "and a fresh partial belongs to a live save"
        );

        sweep(tmp.path(), std::time::Duration::ZERO);
        assert!(!leaked.exists(), "a stale one is litter");
        assert!(!partial.exists(), "and so is a stale partial");
        assert!(mine.is_dir(), "and nothing else is touched");
        assert_eq!(std::fs::read(&finished).unwrap(), b"a whole image");
    }

    #[test]
    fn a_destination_a_failed_copy_created_is_given_back_unless_it_holds_something() {
        // `cp ctr:/tree/. ./fresh` must create `./fresh` before the
        // bytes move, because being a directory is what makes the tar
        // merge into it. Measured on 29.4.0, `docker cp
        // ctr:/no/such/path/. ./fresh` exits 1 and leaves `./fresh`
        // uncreated — and this one cannot be swept later either, since
        // it wears the ordinary name the user chose.
        let tmp = tempfile::tempdir().unwrap();
        let fresh = tmp.path().join("fresh");
        std::fs::create_dir(&fresh).unwrap();
        assert!(
            give_back(&fresh),
            "a destination nothing landed in goes back"
        );
        assert!(!fresh.exists());

        // But a half-extracted tree stays, and is reported instead:
        // deleting what did arrive is the one outcome a user cannot
        // undo, which is why this is `remove_dir` and not
        // `remove_dir_all`.
        let half = tmp.path().join("half");
        std::fs::create_dir(&half).unwrap();
        std::fs::write(half.join("landed.txt"), b"PART").unwrap();
        assert!(
            !give_back(&half),
            "and the caller is told, so it does not claim nothing was changed"
        );
        assert_eq!(std::fs::read(half.join("landed.txt")).unwrap(), b"PART");
    }

    #[test]
    fn the_copy_keeps_the_destinations_own_mode_and_the_modes_below_it() {
        use std::os::unix::fs::PermissionsExt;
        let mode_of = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let chmod = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };

        // The remote packs `tar -C "$S" -cf - .`, so the archive's first
        // entry is the `mktemp -d` staging directory itself — 0700. This
        // builds exactly that archive.
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        let inner = stage.join("tree");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(inner.join("f.txt"), b"F").unwrap();
        chmod(&inner.join("f.txt"), 0o666);
        chmod(&inner, 0o777);
        chmod(&stage, 0o700);
        let archive = tmp.path().join("copy.tar");
        assert!(
            Command::new("tar")
                .arg("-C")
                .arg(&stage)
                .arg("-cf")
                .arg(&archive)
                .arg(".")
                .status()
                .unwrap()
                .success()
        );

        let dest = tmp.path().join("landing");
        std::fs::create_dir(&dest).unwrap();
        chmod(&dest, 0o755);
        let file = File::open(&archive).unwrap();
        assert!(unpack(&dest, Stdio::from(file)).unwrap().success());

        // Measured with bsdtar 3.5.3: without the restore this comes out
        // 0700, because `-p` hands the destination the staging
        // directory's mode. Docker's own archive is rooted at the copied
        // entry and never touches the destination, so `docker cp ctr:/x
        // ./served-dir` taking a shared directory private is ours alone
        // — and silent.
        assert_eq!(
            mode_of(&dest),
            0o755,
            "the destination keeps its own mode, not the staging directory's"
        );
        // And `-p` is still doing its half: without it the umask ate
        // these, 0777 coming out 0755 and 0666 coming out 0644.
        assert_eq!(
            mode_of(&dest.join("tree")),
            0o777,
            "everything BELOW it keeps what the far side sent"
        );
        assert_eq!(mode_of(&dest.join("tree/f.txt")), 0o666);
    }

    #[test]
    fn a_binary_stream_is_refused_a_terminal_and_text_is_not() {
        // Docker refuses this itself when run locally and cannot do so
        // through ssh, where it only ever sees a pipe — so this refusal
        // is the only thing between `ulak docker save api` typed at a
        // prompt and a few hundred megabytes of tar on the screen. It
        // had no test of any kind: cargo hands every test a pipe, so the
        // branch was unreachable from the suite until the decision came
        // out of `land`.
        let refuse = OnTerminal::Refuse { command: "save" };
        let err = refuse.refusal(true).expect("a terminal is refused");
        assert!(err.to_string().contains("cowardly refusing"), "{err}");
        assert!(
            err.to_string().contains("docker save"),
            "and it names the command that was typed: {err}"
        );
        assert!(
            refuse.refusal(false).is_none(),
            "a pipe is what `… > out.tar` and `… | docker load` are"
        );
        // `docker compose config` prints YAML somebody reads, and
        // `… config | less` has to keep working.
        assert!(OnTerminal::Print.refusal(true).is_none());
    }

    #[test]
    fn an_empty_carrier_value_names_no_file_the_way_docker_reads_one() {
        // All four measured on 29.4.0, because the answer is not the
        // one it looks like:
        //   `docker save -o "" alpine:3.20`   → 4 MB of tar on STDOUT
        //   `docker save --output= alpine`    → the same, on STDOUT
        //   `docker load -i "" < api.tar`     → loaded, from STDIN
        //   `docker save -o= alpine:3.20`     → a 4 MB file named `=`
        // The first three are "no file named", and reading them as a
        // file with an empty name transferred the whole image into a
        // bare `.ulak-partial` in the CURRENT directory and threw it
        // away with a rename to "" that failed naming no path.
        for spelling in [
            vec!["save", "-o", "", "api"],
            vec!["save", "--output=", "api"],
        ] {
            let mut args = v(&spelling);
            let got = take_flag(&mut args, 1, OUT, &[]).unwrap();
            assert_eq!(got.as_deref(), Some(""), "{spelling:?}");
            assert_eq!(args, v(&["save", "api"]), "{spelling:?}");
            assert_eq!(named_file(got), None, "no file was named: {spelling:?}");
        }
        // The fourth is a file, and its name is `=`: pflag strips one
        // `=` from a short flag's attached value only when something
        // follows it. Refusing it, or streaming it, would put the
        // archive somewhere Docker does not.
        let mut args = v(&["save", "-o=", "api"]);
        let got = take_flag(&mut args, 1, OUT, &[]).unwrap();
        assert_eq!(got.as_deref(), Some("="));
        assert_eq!(named_file(got).as_deref(), Some("="));
        assert_eq!(args, v(&["save", "api"]));
        // `-` is the other spelling for the stream, on both sides.
        assert_eq!(named_file(Some("-".into())), None);
        assert_eq!(
            named_file(Some("api.tar".into())).as_deref(),
            Some("api.tar")
        );

        // And should a caller ever hand `land` a file with no name
        // anyway, it refuses before it opens anything: this is what the
        // whole image used to stream into.
        let cmd = Command::new("/there-is-no-such-binary");
        let err = land(cmd, "redacted", Some(Path::new("")), OnTerminal::Print).unwrap_err();
        assert!(err.to_string().contains("no file was named"), "{err}");
        assert!(
            !Path::new(PARTIAL_SUFFIX).exists(),
            "and nothing was opened to hold the transfer"
        );
    }

    #[test]
    fn a_partial_that_could_not_be_written_does_not_outlive_the_command() {
        // `File::create` runs before the spawn, so a `?` on the spawn
        // left an empty `.ulak-partial` in the directory the user asked
        // their output to go to — for good, since nothing else ever
        // removes it and the retry is what finds it.
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("api.tar");
        let cmd = Command::new(tmp.path().join("there-is-no-such-binary"));
        let err = land(cmd, "redacted", Some(&out), OnTerminal::Print).unwrap_err();
        assert!(err.to_string().contains("cannot spawn ssh"), "{err}");
        assert!(!out.with_file_name("api.tar.ulak-partial").exists());
        assert!(!out.exists(), "and the output itself was never created");
    }
}
