//! Which Compose arguments name a path on THIS machine.
//!
//! `ulak docker compose <cmd>` forwards argv verbatim, and for most of
//! the thirty-odd subcommands that is exactly right: the compose MODEL
//! is already synced, so a subcommand that only names services means the
//! same thing on both machines. The exceptions are the handful whose OWN
//! arguments name a local path — `compose cp ./a web:/a`, `compose config
//! -o rendered.yaml`. Forwarded verbatim those read a file that is not
//! there, or write one into a directory on the SERVER that the next sync
//! may delete, under a name the user believes is in their current
//! directory. It is the same trap `bridge.rs` describes for `docker save
//! -o`, arriving through Compose instead.
//!
//! This module only says WHERE those paths are. It opens nothing, spawns
//! nothing and syncs nothing: `scan` reports, `rewrite` splices, and what
//! a remote path should look like stays the caller's decision.
//!
//! Measured against Docker Compose v5.1.2 (`docker compose <cmd> --help`,
//! Docker 29.4.0) — not remembered, and in two places the difference
//! matters. `compose build` has NO `--secret`, unlike `docker buildx
//! build`. `compose run` spells its env file `--env-from-file`;
//! `--env-file` is a compose GLOBAL, which passthrough has already split
//! off before `args` reaches here.

use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    File,
    Dir,
    Either,
}

#[derive(Debug, Clone)]
pub struct LocalPath {
    /// Index into the argv slice you were given.
    pub index: usize,
    /// Byte range inside args[index] that is the path itself, so a
    /// caller can splice a remote path in without re-parsing. For
    /// `-v ./data:/app` this covers only `./data`.
    pub span: Range<usize>,
    /// Resolved against cwd, absolute. Not canonicalized for a
    /// Write — it may not exist yet.
    pub path: PathBuf,
    pub direction: Direction,
    pub shape: Shape,
    /// Where it came from, for error messages: "-v", "--output".
    pub flag: &'static str,
}

// ─── the flag lists, one per subcommand that carries a path ─────────

/// `docker compose cp --help`: `--index int` is its ONLY value-taking
/// flag — `--all`, `-a/--archive`, `--dry-run` and `-L/--follow-link`
/// are booleans. Without `--index` written down here, `compose cp
/// --index 1 web:/etc/hosts ./out` would read `1` as SRC_PATH and copy
/// the wrong thing to the wrong place.
///
/// Not the same set as `docker cp` (see `bridge.rs`'s `CP_BOOL_FLAGS`):
/// compose cp has no `-q/--quiet`, and docker cp has no `--all` or
/// `--index`. Sharing one list between them would be wrong in both
/// directions.
const CP_VALUE_FLAGS: &[&str] = &["--index"];

/// Every `docker compose run` flag that takes a value.
///
/// This list is what finds the SERVICE positional, and that is the
/// subtle part of the whole module. Compose turns interspersed parsing
/// OFF for `run`, so the first non-flag argument is SERVICE and
/// everything after it belongs to the container — verified, not assumed:
/// `compose run web echo --bogus-flag` runs `echo --bogus-flag`, while
/// the same flag before `web` is rejected as unknown. Miss one entry
/// here and its value is mistaken for SERVICE, which ends the scan early
/// and leaves the real `-v` behind it unsynced.
///
/// `-w/--workdir` is in the list because it takes a value, NOT because
/// it names anything local: it is a directory inside the container.
const RUN_VALUE_FLAGS: &[&str] = &[
    "--cap-add",
    "--cap-drop",
    "--entrypoint",
    "-e",
    "--env",
    "--env-from-file",
    "-l",
    "--label",
    "--name",
    "-p",
    "--publish",
    "--pull",
    "-u",
    "--user",
    "-v",
    "--volume",
    "-w",
    "--workdir",
];

/// `docker compose config` value flags. `--format` and `--hash` are here
/// so that `config --format yaml -o out.yaml` cannot read `-o` as the
/// format's value.
const CONFIG_VALUE_FLAGS: &[&str] = &["--format", "--hash", "-o", "--output"];

/// `docker compose export` value flags.
const EXPORT_VALUE_FLAGS: &[&str] = &["--index", "-o", "--output"];

/// `docker compose build` value flags. `--pull` is absent on purpose: on
/// `build` it is a BOOLEAN, though the identically spelled flag on `run`,
/// `up` and `create` takes a string. Listing it here would swallow the
/// following argument.
const BUILD_VALUE_FLAGS: &[&str] = &[
    "--build-arg",
    "--builder",
    "-m",
    "--memory",
    "--provenance",
    "--sbom",
    "--ssh",
];

/// `docker compose bridge convert` value flags. `-t/--transformation`
/// names a transformation IMAGE, not a path.
const BRIDGE_CONVERT_VALUE_FLAGS: &[&str] =
    &["-o", "--output", "--templates", "-t", "--transformation"];

/// `docker compose bridge transformations create` value flags.
/// `-f/--from` names an existing transformation image to copy.
const TRANSFORMATIONS_CREATE_VALUE_FLAGS: &[&str] = &["-f", "--from"];

// ─── the entry points ───────────────────────────────────────────────

/// Every local path this compose subcommand names, in argv order.
///
/// `args` is what `Invocation::args` holds, and `at` is where the
/// subcommand sits inside it — usually 0, but NOT always: the globals
/// compose passes through untouched (`--progress`, `--dry-run`,
/// `--ansi`, …) are re-emitted at the front of the same list. Reading
/// `args[0]` instead matched none of the subcommands below whenever one
/// of those was typed, so `compose --progress plain run -v ./data:/data`
/// found no local path, never synced the bind source, and let the
/// server's docker create an empty directory in its place — the exact
/// failure this module exists to prevent, reached through the front
/// door. `None` means there is no subcommand at all.
///
/// An unknown subcommand returns an empty vec, not an error: forwarding
/// verbatim is the correct behaviour for the other thirty of them.
pub fn scan(cwd: &Path, args: &[String], at: Option<usize>) -> Result<Vec<LocalPath>> {
    let Some(at) = at else {
        return Ok(Vec::new());
    };
    Ok(match args.get(at).map(String::as_str) {
        Some("cp") => cp(cwd, args, at),
        Some("run") => run(cwd, args, at),
        Some("build") => build(cwd, args, at),
        Some("bridge") => bridge(cwd, args, at),
        Some(name) if streamed_value_flags(name).is_some() => {
            let flags = streamed_value_flags(name).unwrap_or(&[]);
            output_paths(cwd, args, at + 1, flags, Shape::File)
        }
        // The other thirty name services, images and timestamps. Their
        // argv means the same thing on both machines already.
        _ => Vec::new(),
    })
}

/// The flags a subcommand whose `-o` is answered by a STREAM parses.
/// One table, so `scan` and `output_spots` cannot come to disagree
/// about which subcommands those are or how their argv reads.
fn streamed_value_flags(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "config" => Some(CONFIG_VALUE_FLAGS),
        "export" => Some(EXPORT_VALUE_FLAGS),
        _ => None,
    }
}

/// EVERY `-o/--output` such a subcommand names, as the argv index and
/// the byte range its value occupies — including the occurrences `local`
/// makes no path out of, which is why `scan` alone cannot answer this.
///
/// pflag reads a repeated string flag to its LAST occurrence, so WHICH
/// one came last decides where the stream goes, and the last one is not
/// always a path this machine can land: `local` keeps `-`, a `~` path
/// and an empty value out, and those are exactly the values a caller
/// must not mistake for "no `-o` was given". Seeing only the paths, it
/// would land the bytes in a file the user had already overruled.
pub fn output_spots(args: &[String], at: Option<usize>) -> Vec<(usize, Range<usize>)> {
    let Some(at) = at else {
        return Vec::new();
    };
    let Some(flags) = args
        .get(at)
        .map(String::as_str)
        .and_then(streamed_value_flags)
    else {
        return Vec::new();
    };
    let mut spots = Vec::new();
    let parsed = walk(args, at + 1, flags, true, |flag, index, span| {
        if matches!(flag, "-o" | "--output") {
            spots.push((index, span));
        }
    });
    if parsed.help { Vec::new() } else { spots }
}

/// Replace each path with whatever `to` returns, right-to-left so
/// earlier spans stay valid.
pub fn rewrite(args: &mut [String], found: &[LocalPath], to: impl Fn(&LocalPath) -> String) {
    // Two paths can share one argument — `--ssh id=/a,/b` is two — so
    // splicing a longer remote path into the first would push the
    // second's span off its own bytes. Sorted rather than assumed to
    // arrive in argv order, because a caller that hands back a filtered
    // or regrouped slice deserves the same argv as one that does not.
    let mut order: Vec<&LocalPath> = found.iter().collect();
    order.sort_by_key(|p| (p.index, p.span.start));
    for path in order.into_iter().rev() {
        args[path.index].replace_range(path.span.clone(), &to(path));
    }
}

// ─── per-subcommand readers ─────────────────────────────────────────

/// `cp SERVICE:SRC_PATH DEST_PATH|-` or `cp SRC_PATH|- SERVICE:DEST_PATH`.
/// Whichever end is not a service reference is the one on this machine,
/// and which end that is decides the direction: the first positional is
/// read, the second is written.
fn cp(cwd: &Path, args: &[String], at: usize) -> Vec<LocalPath> {
    let parsed = walk(args, at + 1, CP_VALUE_FLAGS, true, |_, _, _| {});
    if parsed.help {
        return Vec::new();
    }
    let sides = [
        (Direction::Read, "SRC_PATH"),
        (Direction::Write, "DEST_PATH"),
    ];
    parsed
        .positionals
        .iter()
        .zip(sides)
        .filter_map(|(&index, (direction, flag))| {
            if is_service(&args[index]) {
                return None;
            }
            // Shape::Either because `cp` copies files and directories
            // alike, and which one this is can only be answered by the
            // side the path lives on.
            let whole = 0..args[index].len();
            local(cwd, args, index, whole, direction, Shape::Either, flag)
        })
        .collect()
}

fn run(cwd: &Path, args: &[String], at: usize) -> Vec<LocalPath> {
    let mut found = Vec::new();
    let parsed = walk(args, at + 1, RUN_VALUE_FLAGS, false, |flag, index, span| {
        match flag {
            "-v" | "--volume" => {
                let Some(source) = bind_source(&args[index][span.clone()]) else {
                    return;
                };
                // `source` is relative to the value, the value is
                // relative to the argument: `--volume=./data:/app` needs
                // both offsets or the span lands inside the flag name.
                let at = span.start + source.start..span.start + source.end;
                found.extend(local(
                    cwd,
                    args,
                    index,
                    at,
                    Direction::Read,
                    Shape::Either,
                    flag,
                ));
            }
            // Read on this machine and handed to the container as
            // variables. NOT `--env-file`, which is the compose global
            // that selects the model's own environment.
            "--env-from-file" => {
                found.extend(local(
                    cwd,
                    args,
                    index,
                    span,
                    Direction::Read,
                    Shape::File,
                    flag,
                ));
            }
            _ => {}
        }
    });
    if parsed.help { Vec::new() } else { found }
}

/// The `-o/--output` family, plus `bridge convert`'s `--templates`.
/// `shape` is what `-o` writes: a FILE for `config` and `export`, a
/// DIRECTORY for `bridge convert`.
fn output_paths(
    cwd: &Path,
    args: &[String],
    from: usize,
    value_flags: &[&'static str],
    shape: Shape,
) -> Vec<LocalPath> {
    let mut found = Vec::new();
    let parsed = walk(args, from, value_flags, true, |flag, index, span| {
        let (direction, shape) = match flag {
            "-o" | "--output" => (Direction::Write, shape),
            "--templates" => (Direction::Read, Shape::Dir),
            _ => return,
        };
        found.extend(local(cwd, args, index, span, direction, shape, flag));
    });
    if parsed.help { Vec::new() } else { found }
}

fn build(cwd: &Path, args: &[String], at: usize) -> Vec<LocalPath> {
    let mut found = Vec::new();
    let parsed = walk(
        args,
        at + 1,
        BUILD_VALUE_FLAGS,
        true,
        |flag, index, span| {
            if flag != "--ssh" {
                return;
            }
            for key in ssh_keys(&args[index][span.clone()]) {
                let at = span.start + key.start..span.start + key.end;
                found.extend(local(
                    cwd,
                    args,
                    index,
                    at,
                    Direction::Read,
                    Shape::File,
                    flag,
                ));
            }
        },
    );
    if parsed.help { Vec::new() } else { found }
}

fn bridge(cwd: &Path, args: &[String], at: usize) -> Vec<LocalPath> {
    match args.get(at + 1).map(String::as_str) {
        Some("convert") => output_paths(cwd, args, at + 2, BRIDGE_CONVERT_VALUE_FLAGS, Shape::Dir),
        Some("transformations") => transformations(cwd, args, at),
        _ => Vec::new(),
    }
}

/// `bridge transformations create [OPTION] PATH` — PATH is a directory
/// this command SCAFFOLDS on whichever machine it runs on, so forwarded
/// verbatim the new transformation appears on the server and the user is
/// left looking at nothing here. Its sibling `list` names no path.
fn transformations(cwd: &Path, args: &[String], at: usize) -> Vec<LocalPath> {
    if args.get(at + 2).map(String::as_str) != Some("create") {
        return Vec::new();
    }
    let parsed = walk(
        args,
        at + 3,
        TRANSFORMATIONS_CREATE_VALUE_FLAGS,
        true,
        |_, _, _| {},
    );
    if parsed.help {
        return Vec::new();
    }
    parsed
        .positionals
        .first()
        .and_then(|&index| {
            let whole = 0..args[index].len();
            local(
                cwd,
                args,
                index,
                whole,
                Direction::Write,
                Shape::Dir,
                "PATH",
            )
        })
        .into_iter()
        .collect()
}

// ─── value shapes ───────────────────────────────────────────────────

/// The LOCAL source inside a `-v` value, as a byte range, or `None` when
/// there is nothing on this machine to carry. `-v` is
/// `[SOURCE:]DEST[:OPTIONS]`, and three of its shapes name nothing here:
///
/// - `cache:/x` is a NAMED VOLUME — daemon state, which lives on the
///   server and should stay there. Syncing a directory onto it would
///   replace live data with an empty tree.
/// - `/data` alone is an ANONYMOUS volume: one path, and it is the
///   container's, not this machine's.
/// - `~/x:/app` is the REMOTE home. `sh_quote` (ssh.rs) lists `~` among
///   the bytes it does not escape, so the tilde reaches the server's
///   shell unquoted and expands there. It never meant a directory here.
fn bind_source(value: &str) -> Option<Range<usize>> {
    let colon = value.find(':')?;
    let source = &value[..colon];
    // Docker's own rule, and the same one `is_service` applies below.
    let here = source.starts_with('/') || source.starts_with('.');
    here.then_some(0..colon)
}

/// The local paths inside a `--ssh` value, which is
/// `default|<id>[=<socket>|<key>[,<key>]]`.
///
/// A bare `default` or `myid` names no path: it means the agent socket
/// in `$SSH_AUTH_SOCK`, which after forwarding is the SERVER's agent and
/// not this machine's. Everything after the `=` is a path, comma
/// separated — and unlike `-v` there is no named-volume shape to tell
/// apart, so a bare `key.pem` here really is a relative file.
fn ssh_keys(value: &str) -> Vec<Range<usize>> {
    let Some(eq) = value.find('=') else {
        return Vec::new();
    };
    let mut at = eq + 1;
    let mut keys = Vec::new();
    for key in value[eq + 1..].split(',') {
        keys.push(at..at + key.len());
        at += key.len() + 1;
    }
    keys
}

/// Docker's own rule for `SERVICE:PATH`, matching `split_container` in
/// `bridge.rs`: a leading `/`, `.` or `~` makes it a path however many
/// colons it holds, so `./a:b.txt` is a file and not a service called
/// `./a`.
fn is_service(arg: &str) -> bool {
    if arg.starts_with('/') || arg.starts_with('.') || arg.starts_with('~') {
        return false;
    }
    match arg.split_once(':') {
        Some((service, path)) => !service.is_empty() && !path.is_empty(),
        None => false,
    }
}

/// One found path, resolved — or `None` when the argument names nothing
/// on this machine: an empty value, a `-` stream (Compose accepts one on
/// either end of `cp` and in place of `-o`, and our stdio is already the
/// user's), or a `~` the remote shell will expand against the remote
/// home.
fn local(
    cwd: &Path,
    args: &[String],
    index: usize,
    span: Range<usize>,
    direction: Direction,
    shape: Shape,
    flag: &'static str,
) -> Option<LocalPath> {
    let raw = &args[index][span.clone()];
    if raw.is_empty() || raw == "-" || raw.starts_with('~') {
        return None;
    }
    let path = resolve(cwd, Path::new(raw), direction);
    Some(LocalPath {
        index,
        span,
        path,
        direction,
        shape,
        flag,
    })
}

/// Absolute, and free of `.` and `..` wherever the filesystem can say so.
///
/// A Read is canonicalized outright: it exists, and the caller will
/// compare it against the workspace anchor, which is canonical too (see
/// `docker.rs`'s `local_path`). A Write's LAST component is deliberately
/// left alone — `-o rendered.yaml` names a file that does not exist yet,
/// and canonicalize would fail on it — so only its parent is resolved.
/// A Read that turns out to be missing (`-v ./not-yet:/app`, which
/// Docker would create) falls back to the same treatment rather than
/// failing: this module reports what argv says, it does not judge it.
fn resolve(cwd: &Path, raw: &Path, direction: Direction) -> PathBuf {
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    if direction == Direction::Read
        && let Ok(real) = joined.canonicalize()
    {
        return real;
    }
    match (joined.parent(), joined.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(real) => real.join(name),
            Err(_) => joined,
        },
        _ => joined,
    }
}

// ─── compose's own flag parser, as much of it as this needs ─────────

struct Parsed {
    /// Indices of the positional arguments, in argv order.
    positionals: Vec<usize>,
    /// `--help` (or `-h`) reached compose's own parser, so this command
    /// is going to print its usage and touch no file at all. Detected
    /// here rather than over the whole argv on purpose: in `compose run
    /// web curl -h` the `-h` is the CONTAINER's, and a scan that gave up
    /// on it would leave the `-v` before `web` unsynced.
    help: bool,
}

/// One pass of compose's flag parser over `args[from..]`: every
/// value-carrying occurrence of a flag in `value_flags` is handed to
/// `visit` with the byte range its VALUE occupies inside that argument,
/// and the positionals come back.
///
/// `interspersed` is compose's own switch. `false` — `run` — stops
/// parsing at the first positional, because everything after SERVICE
/// belongs to the container. `true` — `cp`, `config`, `build` — keeps
/// going, because `compose cp web:/f ./out --index 1` is accepted.
///
/// All five spellings pflag accepts are handled, because people write
/// all five: `-o out.yaml`, `-oout.yaml`, `-o=out.yaml`, `--output
/// out.yaml`, `--output=out.yaml`. So is a bundle that ends in a value
/// flag (`-Pv ./x:/app`), which pflag also accepts — verified against
/// v5.1.2, where `-Pv/tmp:` is rejected for the volume spec rather than
/// for the flag.
fn walk(
    args: &[String],
    from: usize,
    value_flags: &[&'static str],
    interspersed: bool,
    mut visit: impl FnMut(&'static str, usize, Range<usize>),
) -> Parsed {
    let mut parsed = Parsed {
        positionals: Vec::new(),
        help: false,
    };
    let mut i = from;
    while i < args.len() {
        let arg = &args[i];
        // A lone `-` is a positional naming a stream, not a flag.
        if arg == "-" || !arg.starts_with('-') {
            parsed.positionals.push(i);
            if !interspersed {
                return parsed;
            }
            i += 1;
            continue;
        }
        if arg == "--" {
            // Past `--` everything is a positional, whatever it looks
            // like — a file really can be named `--output`.
            parsed.positionals.extend(i + 1..args.len());
            return parsed;
        }

        if let Some(long) = arg.strip_prefix("--") {
            let name = long.split_once('=').map_or(long, |(name, _)| name);
            if name == "help" {
                parsed.help = true;
            }
            let matched = value_flags
                .iter()
                .copied()
                .find(|f| f.strip_prefix("--") == Some(name));
            // A `=` in the word is what tells the two spellings apart:
            // `--output=x` carries its value, `--output x` takes the
            // next word.
            let inline = long.len() > name.len();
            match matched {
                // `--output=out.yaml`: the value is the rest of the word.
                Some(flag) if inline => {
                    visit(flag, i, 2 + name.len() + 1..arg.len());
                    i += 1;
                }
                // `--output out.yaml`: the value is the next word. A
                // flag with nothing after it is left alone — compose
                // rejects it in its own words, and this module never
                // edits argv, so there is nothing here to swallow.
                Some(flag) => {
                    if let Some(next) = args.get(i + 1) {
                        visit(flag, i + 1, 0..next.len());
                    }
                    i += 2;
                }
                None => i += 1,
            }
            continue;
        }

        // A shorthand bundle. Booleans pack together until one that
        // takes a value, which swallows the rest of the word — or the
        // next word, when it is the last character.
        let mut consumed = 1;
        for (off, ch) in arg.char_indices().skip(1) {
            if ch == 'h' {
                parsed.help = true;
            }
            let Some(flag) = value_flags
                .iter()
                .copied()
                .find(|f| f.len() == 2 && f.ends_with(ch))
            else {
                continue;
            };
            let rest = &arg[off + ch.len_utf8()..];
            if rest.is_empty() {
                if let Some(next) = args.get(i + 1) {
                    visit(flag, i + 1, 0..next.len());
                }
                consumed = 2;
            } else {
                // pflag takes an `=` straight after the flag character as
                // a separator; anywhere else it is part of the value.
                let start = off + ch.len_utf8() + usize::from(rest.starts_with('='));
                visit(flag, i, start..arg.len());
            }
            break;
        }
        i += consumed;
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// A real directory to resolve against, canonicalized because macOS
    /// answers `/var` with `/private/var` and a raw tempdir path would
    /// never match what `scan` returns.
    fn cwd() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        (temp, cwd)
    }

    /// The subcommand is not always `args[0]`, and reading it as if it
    /// were is silent.
    ///
    /// Compose has globals Ulak does not own and passes through
    /// untouched — `--progress`, `--dry-run`, `--ansi`, `--parallel`,
    /// `--log-level`, `--compatibility`, `--all-resources`. They are
    /// re-emitted at the FRONT of `Invocation::args`, ahead of the
    /// subcommand. `scan` used to dispatch on `args[0]`, so every one of
    /// them turned `cp`/`run`/`build`/`config`/`export`/`bridge` into
    /// "one of the other thirty": no local path was found, nothing was
    /// synced, and the command still ran. `run -v ./data:/data` then
    /// mounted an empty directory the server's docker had just created —
    /// no error, no output, the whole failure this module exists to
    /// prevent.
    ///
    /// Driven through `Invocation` rather than a hand-written index,
    /// because the index is the thing under test: a test that passed its
    /// own `at` would agree with itself.
    #[test]
    fn a_global_before_the_subcommand_does_not_hide_the_paths_it_names() {
        let (_t, cwd) = cwd();
        std::fs::create_dir(cwd.join("data")).unwrap();
        std::fs::write(cwd.join("compose.yaml"), "services: {}\n").unwrap();

        // One of each shape: a bool global, and a value global whose
        // value is a bare word that a "first word without a dash" scan
        // would happily mistake for the subcommand.
        for globals in [v(&["--dry-run"]), v(&["--progress", "plain"])] {
            let mut argv = globals.clone();
            argv.extend(v(&["run", "-v", "./data:/data", "web", "true"]));
            let inv =
                crate::invocation::Invocation::rebuild_in(&cwd, &argv, Default::default()).unwrap();
            assert_eq!(
                inv.subcommand.as_deref(),
                Some("run"),
                "{globals:?} hid the subcommand itself"
            );
            // Without this the test proves nothing: if the globals did
            // not survive into `args`, the subcommand would sit at 0 and
            // the old `args[0]` reader would pass too.
            assert_eq!(
                inv.subcommand_at,
                Some(globals.len()),
                "{globals:?} did not reach args, so this scenario is not the one it names: {:?}",
                inv.args
            );

            let found = scan(&cwd, &inv.args, inv.subcommand_at).unwrap();
            assert_eq!(
                found.len(),
                1,
                "{globals:?} hid the bind source: {found:?} from {:?}",
                inv.args
            );
            assert_eq!(found[0].path, cwd.join("data"));
            assert_eq!(
                inv.args[found[0].index], "./data:/data",
                "the span points at a different word than the one it named"
            );
        }
    }

    #[test]
    fn both_directions_of_compose_cp_find_the_local_end() {
        let (_t, cwd) = cwd();
        std::fs::write(cwd.join("local.txt"), "hi").unwrap();

        // Local → service: the local end is SRC, and it is read.
        let args = v(&["cp", "./local.txt", "web:/app/local.txt"]);
        let found = scan(&cwd, &args, Some(0)).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].index, 1);
        assert_eq!(found[0].direction, Direction::Read);
        assert_eq!(found[0].flag, "SRC_PATH");
        assert_eq!(found[0].path, cwd.join("local.txt"));

        // Service → local: the local end is DEST, and it is written.
        let args = v(&["cp", "web:/app/out.txt", "./out.txt"]);
        let found = scan(&cwd, &args, Some(0)).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].index, 2);
        assert_eq!(found[0].direction, Direction::Write);
        assert_eq!(found[0].flag, "DEST_PATH");
        assert_eq!(found[0].path, cwd.join("out.txt"));
    }

    #[test]
    fn a_service_reference_is_told_from_a_local_path_the_way_bridge_does() {
        assert!(is_service("web:/app"));
        assert!(is_service("a1b2c3:/etc/passwd"));
        // A local path wins however many colons it holds — Docker's own
        // rule. Without it `./a:b.txt` would be read as a service `./a`,
        // and the file would never be carried.
        for here in ["./a:b.txt", "/tmp/a:b", "~/x:y", "plain.txt", "-"] {
            assert!(!is_service(here), "{here} is a local path");
        }
    }

    #[test]
    fn the_index_flag_is_never_mistaken_for_a_cp_path() {
        let (_t, cwd) = cwd();
        // `--index` takes a value, so `1` is its argument and the two
        // paths come after it. Reading left to right without knowing
        // that would treat `1` as SRC_PATH and copy the wrong thing.
        let args = v(&["cp", "--index", "1", "web:/etc/hosts", "./hosts"]);
        let found = scan(&cwd, &args, Some(0)).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].index, 4);
        assert_eq!(found[0].direction, Direction::Write);
    }

    #[test]
    fn a_named_volume_is_daemon_state_and_stays_untouched() {
        let (_t, cwd) = cwd();
        for spec in [
            "cache:/x", // a named volume lives on the server
            "/data",    // an anonymous volume: the container's path
            "pgdata:/var/lib/postgresql/data:rw",
        ] {
            let args = v(&["run", "-v", spec, "web"]);
            let found = scan(&cwd, &args, Some(0)).unwrap();
            assert!(found.is_empty(), "{spec} named nothing local: {found:?}");
        }
    }

    #[test]
    fn a_tilde_source_belongs_to_the_remote_home_and_is_left_alone() {
        let (_t, cwd) = cwd();
        // `sh_quote` does not escape `~`, so it reaches the server's
        // shell and expands against the server's home. Claiming it as a
        // local path would sync this machine's home over it.
        let args = v(&["run", "-v", "~/keys:/keys", "web"]);
        assert!(scan(&cwd, &args, Some(0)).unwrap().is_empty());
        let args = v(&["config", "-o", "~/rendered.yaml"]);
        assert!(scan(&cwd, &args, Some(0)).unwrap().is_empty());
    }

    #[test]
    fn the_output_flag_is_found_in_all_three_spellings() {
        let (_t, cwd) = cwd();
        for spelling in [
            v(&["config", "-o", "rendered.yaml"]),
            v(&["config", "--output", "rendered.yaml"]),
            v(&["config", "--output=rendered.yaml"]),
        ] {
            let found = scan(&cwd, &spelling, Some(0)).unwrap();
            assert_eq!(found.len(), 1, "{spelling:?}");
            assert_eq!(found[0].direction, Direction::Write);
            assert_eq!(found[0].shape, Shape::File);
            assert_eq!(found[0].path, cwd.join("rendered.yaml"), "{spelling:?}");
            // The span must cover the path and nothing else, or a
            // rewrite would splice a remote path over the flag name.
            assert_eq!(
                &spelling[found[0].index][found[0].span.clone()],
                "rendered.yaml",
                "{spelling:?}"
            );
        }
    }

    #[test]
    fn the_run_service_boundary_hides_the_containers_own_arguments() {
        let (_t, cwd) = cwd();
        std::fs::create_dir(cwd.join("data")).unwrap();
        // `web` ends compose's parsing. `cat /etc/passwd` is the
        // container's command and `-v /host:/nope` is its argument, not
        // a bind mount — compose never sees either as a flag.
        let args = v(&[
            "run",
            "--rm",
            "-v",
            "./data:/app",
            "web",
            "cat",
            "/etc/passwd",
            "-v",
            "/host:/nope",
        ]);
        let found = scan(&cwd, &args, Some(0)).unwrap();
        assert_eq!(found.len(), 1, "only the mount before SERVICE: {found:?}");
        assert_eq!(found[0].index, 3);
        assert_eq!(found[0].path, cwd.join("data"));
        assert_eq!(found[0].direction, Direction::Read);
    }

    #[test]
    fn a_value_flag_before_the_service_does_not_end_the_scan() {
        let (_t, cwd) = cwd();
        std::fs::create_dir(cwd.join("data")).unwrap();
        // `--name` takes a value, so `web` here is the container's name
        // and the SERVICE is `api`. A parser that did not know that
        // would stop at `web` and never reach the `-v` behind it.
        let args = v(&["run", "--name", "web", "-v", "./data:/app", "api", "sh"]);
        let found = scan(&cwd, &args, Some(0)).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, cwd.join("data"));
    }

    #[test]
    fn bundled_shorthand_still_yields_its_volume() {
        let (_t, cwd) = cwd();
        std::fs::create_dir(cwd.join("data")).unwrap();
        // All four are accepted by pflag, and `-Pv` bundles a boolean
        // with a value flag. Measured against v5.1.2.
        for spelling in [
            v(&["run", "-v", "./data:/app", "web"]),
            v(&["run", "-v./data:/app", "web"]),
            v(&["run", "-v=./data:/app", "web"]),
            v(&["run", "-Pv./data:/app", "web"]),
        ] {
            let found = scan(&cwd, &spelling, Some(0)).unwrap();
            assert_eq!(found.len(), 1, "{spelling:?}");
            assert_eq!(found[0].path, cwd.join("data"), "{spelling:?}");
            assert_eq!(
                &spelling[found[0].index][found[0].span.clone()],
                "./data",
                "the span covers the source alone: {spelling:?}"
            );
        }
    }

    #[test]
    fn compose_run_reads_env_from_file_not_the_compose_global_env_file() {
        let (_t, cwd) = cwd();
        std::fs::write(cwd.join("dev.env"), "A=1\n").unwrap();
        let args = v(&["run", "--env-from-file", "./dev.env", "web"]);
        let found = scan(&cwd, &args, Some(0)).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].shape, Shape::File);
        assert_eq!(found[0].direction, Direction::Read);
        assert_eq!(found[0].path, cwd.join("dev.env"));

        // `--env-file` is a compose GLOBAL. Passthrough splits it off
        // before this module sees argv, so run must not claim it.
        let args = v(&["run", "--env-file", "./dev.env", "web"]);
        assert!(scan(&cwd, &args, Some(0)).unwrap().is_empty());
    }

    #[test]
    fn the_ssh_flag_carries_every_key_after_its_equals() {
        let (_t, cwd) = cwd();
        std::fs::write(cwd.join("a.pem"), "").unwrap();
        std::fs::write(cwd.join("b.pem"), "").unwrap();
        let args = v(&["build", "--ssh", "id=./a.pem,./b.pem", "web"]);
        let found = scan(&cwd, &args, Some(0)).unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].path, cwd.join("a.pem"));
        assert_eq!(found[1].path, cwd.join("b.pem"));
        assert_eq!(&args[found[1].index][found[1].span.clone()], "./b.pem");

        // A bare `default` names the agent socket, which after
        // forwarding is the server's — there is no path here to carry.
        let args = v(&["build", "--ssh", "default", "web"]);
        assert!(scan(&cwd, &args, Some(0)).unwrap().is_empty());
    }

    #[test]
    fn build_has_no_secret_flag_to_confuse_with_buildx() {
        let (_t, cwd) = cwd();
        // `docker buildx build` has `--secret id=x,src=./f`; `docker
        // compose build` v5.1.2 does not. Claiming a path here would
        // rewrite an argument compose is about to reject anyway.
        let args = v(&["build", "--secret", "id=x,src=./f", "web"]);
        assert!(scan(&cwd, &args, Some(0)).unwrap().is_empty());
    }

    #[test]
    fn bridge_convert_writes_a_directory_and_reads_its_templates() {
        let (_t, cwd) = cwd();
        std::fs::create_dir(cwd.join("tpl")).unwrap();
        let args = v(&["bridge", "convert", "-o", "out/k8s", "--templates", "./tpl"]);
        let found = scan(&cwd, &args, Some(0)).unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].direction, Direction::Write);
        assert_eq!(found[0].shape, Shape::Dir);
        // A Write is not canonicalized: `out/k8s` does not exist yet,
        // and demanding that it did would break the command's whole job.
        assert_eq!(found[0].path, cwd.join("out/k8s"));
        assert_eq!(found[1].direction, Direction::Read);
        assert_eq!(found[1].shape, Shape::Dir);
        assert_eq!(found[1].path, cwd.join("tpl"));
    }

    #[test]
    fn a_transformation_is_scaffolded_on_the_machine_that_asked_for_it() {
        let (_t, cwd) = cwd();
        let args = v(&["bridge", "transformations", "create", "-f", "img", "./mine"]);
        let found = scan(&cwd, &args, Some(0)).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].flag, "PATH");
        assert_eq!(found[0].direction, Direction::Write);
        assert_eq!(found[0].path, cwd.join("mine"));

        // `list` names nothing at all.
        let args = v(&["bridge", "transformations", "list"]);
        assert!(scan(&cwd, &args, Some(0)).unwrap().is_empty());
    }

    #[test]
    fn a_stream_is_not_a_path_on_either_side() {
        let (_t, cwd) = cwd();
        // Compose's own `-` form already does the right thing, and our
        // stdio is the user's.
        for args in [
            v(&["cp", "web:/app/out.txt", "-"]),
            v(&["cp", "-", "web:/app/in.txt"]),
            v(&["export", "-o", "-", "web"]),
        ] {
            assert!(scan(&cwd, &args, Some(0)).unwrap().is_empty(), "{args:?}");
        }
    }

    #[test]
    fn help_never_names_a_file() {
        let (_t, cwd) = cwd();
        // Compose is going to print usage and open nothing.
        for args in [
            v(&["config", "--help", "-o", "rendered.yaml"]),
            v(&["cp", "-h", "./a", "web:/a"]),
        ] {
            assert!(scan(&cwd, &args, Some(0)).unwrap().is_empty(), "{args:?}");
        }
    }

    #[test]
    fn an_unknown_subcommand_claims_nothing() {
        let (_t, cwd) = cwd();
        // Forwarding argv verbatim is right for these, so an empty vec
        // is the answer and not an error.
        for args in [
            v(&["up", "-d", "web"]),
            v(&["logs", "-f", "--tail", "50", "web"]),
            v(&["exec", "-w", "/app", "web", "sh"]),
            v(&["watch", "--no-up"]),
            v(&[]),
        ] {
            assert!(scan(&cwd, &args, Some(0)).unwrap().is_empty(), "{args:?}");
        }
    }

    #[test]
    fn rewriting_splices_paths_back_without_disturbing_their_neighbours() {
        let (_t, cwd) = cwd();
        std::fs::create_dir(cwd.join("data")).unwrap();
        std::fs::write(cwd.join("dev.env"), "A=1\n").unwrap();
        let mut args = v(&[
            "run",
            "--volume=./data:/app:ro",
            "--env-from-file",
            "./dev.env",
            "web",
            "sh",
        ]);
        let found = scan(&cwd, &args, Some(0)).unwrap();
        assert_eq!(found.len(), 2, "{found:?}");

        rewrite(&mut args, &found, |p| {
            format!("/srv/{}", p.path.file_name().unwrap().to_string_lossy())
        });
        assert_eq!(
            args,
            v(&[
                "run",
                "--volume=/srv/data:/app:ro",
                "--env-from-file",
                "/srv/dev.env",
                "web",
                "sh",
            ]),
            "the flag, the mount target and its options all survive"
        );
    }

    #[test]
    fn two_paths_in_one_argument_both_survive_a_rewrite() {
        let (_t, cwd) = cwd();
        std::fs::write(cwd.join("a.pem"), "").unwrap();
        std::fs::write(cwd.join("b.pem"), "").unwrap();
        // Both spans point into the same argument, so a left-to-right
        // rewrite would move the second one off its own bytes. The
        // replacement is deliberately longer than what it replaces, so
        // the test fails if the order is ever reversed.
        let mut args = v(&["build", "--ssh", "id=./a.pem,./b.pem", "web"]);
        let found = scan(&cwd, &args, Some(0)).unwrap();
        rewrite(&mut args, &found, |p| {
            format!(
                "/remote/keys/{}",
                p.path.file_name().unwrap().to_string_lossy()
            )
        });
        assert_eq!(
            args,
            v(&[
                "build",
                "--ssh",
                "id=/remote/keys/a.pem,/remote/keys/b.pem",
                "web",
            ])
        );
    }
}
