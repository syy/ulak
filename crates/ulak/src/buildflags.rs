//! Where a build's argv names THIS machine's filesystem.
//!
//! `docker.rs` owns the two inputs Docker's usage line documents — the
//! positional context and `-f` — and `scan` deliberately returns
//! neither, so nothing is carried across twice. What is left is the
//! quiet half: nine more flags that also name a local path, are
//! forwarded verbatim today, and therefore resolve against the SERVER.
//! `--secret id=npm,src=./.npmrc` hands the build the server's
//! `./.npmrc`, which is usually nothing at all, and the build fails
//! somewhere deep inside a `RUN npm ci`. `--output type=local,dest=./dist`
//! is worse, because it succeeds: the artifacts land in a directory on
//! the server that the user never looks in, and the empty `./dist` here
//! reads as a build that produced nothing.
//!
//! Nothing in this module opens an ssh connection, syncs, or spawns
//! anything. `scan` says where the paths are and `rewrite` puts new ones
//! back, so what "carry this across" means stays the caller's decision.
//! `positionals` answers the other half of the same question — which
//! words are NOT flags — because the context can only be picked out by
//! a reader that already knows which words a flag has eaten.
//!
//! Every claim here was measured against the client on this machine —
//! Docker 29.4.0, Buildx 0.33.0 — and not remembered. `docker build`,
//! `docker image build` and `docker builder build` are all aliases of
//! `docker buildx build`, so one table serves every spelling.
//!
//! `DOCKER_BUILDKIT=0` is the exception, and it was measured too: it
//! selects the deprecated classic builder, which is a genuinely
//! different parser — it rejects `--secret`, `--output`,
//! `--build-context` and `--provenance` as unknown. One table still
//! serves both, because the difference is only ever presence: every
//! flag the classic builder gives a value to, buildx gives a value to
//! as well, and no flag is a boolean on one side and value-taking on
//! the other. Its one exclusive flag, `--disable-content-trust`, is a
//! boolean, so leaving it out of the table reads it correctly anyway.

use std::borrow::Cow;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;

use crate::ui::fail;

/// Does Docker read this path, or write it?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Read,
    Write,
}

/// What has to be at the far end for the path to mean the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    File,
    Dir,
    /// Docker stats it without demanding either.
    Either,
}

#[derive(Debug, Clone)]
pub struct LocalInput {
    /// Index into the FULL argv, not the tail.
    pub index: usize,
    /// The byte range inside `args[index]` that is the path itself. For
    /// `--secret id=npm,src=./.npmrc` this covers only `./.npmrc`, so a
    /// caller can splice a remote path in without re-parsing the value.
    pub span: Range<usize>,
    /// Resolved against cwd, absolute. Canonical for a READ; for a
    /// WRITE only folded, since the path may not exist yet.
    pub path: PathBuf,
    pub direction: Direction,
    pub shape: Shape,
    /// The path sits inside a QUOTED field of a CSV value, so a
    /// replacement has to be escaped the way docker will unescape it.
    /// `rewrite` does that; nothing outside this module needs to.
    pub csv_quoted: bool,
    /// The flag it came from, for error messages: "--secret".
    pub flag: &'static str,
}

/// Every local path this build names, in argv order.
///
/// `tail_start` is where the command's own arguments begin — 1 for
/// `build`, 2 for `image build` and `buildx build`. Nothing before it is
/// read, because a command-path word is not a flag and must never be
/// mistaken for one.
pub fn scan(cwd: &Path, args: &[String], tail_start: usize) -> Result<Vec<LocalInput>> {
    // Docker's own help builds nothing and names nothing. Asked with a
    // flag-aware walk rather than an `any` over the words, because an
    // `any` reads `--build-arg --help` as a request and then declines to
    // find the `--secret src=./.npmrc` sitting beside it.
    if help_wanted(args, tail_start) {
        return Ok(Vec::new());
    }

    let mut found = Vec::new();
    let mut i = tail_start;
    while i < args.len() {
        let arg = args[i].as_str();
        // Cobra stops reading flags at `--`; everything after it is a
        // positional, and the positionals here are docker.rs's.
        if arg == "--" {
            break;
        }
        match classify(arg) {
            Word::Attached(carrier, at) => {
                collect(cwd, &mut found, carrier, i, at, &args[i][at..])?;
                i += 1;
            }
            Word::Detached(carrier) => {
                let value = args.get(i + 1).ok_or_else(|| {
                    fail!("{} needs a value", carrier.long)
                        .now(format!("for example: {} {}", carrier.long, carrier.example))
                        .into_err()
                })?;
                collect(cwd, &mut found, carrier, i + 1, 0, value)?;
                i += 2;
            }
            // Its value is the next word, and that word is not a flag
            // however much it looks like one.
            Word::Eats => i += 2,
            Word::Plain => i += 1,
        }
    }
    Ok(found)
}

/// Replace each input's path with whatever `to` returns for it.
///
/// Right to left, because one argv word can hold two paths — `--ssh
/// gh=./a,./b` — and rewriting the first would move the second.
pub fn rewrite(args: &mut [String], inputs: &[LocalInput], to: impl Fn(&LocalInput) -> String) {
    let mut order: Vec<&LocalInput> = inputs.iter().collect();
    // Sorted rather than merely reversed, so a caller that filtered or
    // reordered what `scan` returned still gets correct spans.
    order.sort_by_key(|input| std::cmp::Reverse((input.index, input.span.start)));
    for input in order {
        let mut path = to(input);
        // Inside a quoted CSV field docker reads `""` as one literal
        // quote, so a remote path holding one has to go back doubled.
        // Spliced in raw it would CLOSE the field early, and everything
        // after it — the `type=`, the `id=` — would be read as part of
        // the path or refused outright.
        if input.csv_quoted {
            path = path.replace('"', "\"\"");
        }
        args[input.index].replace_range(input.span.clone(), &path);
    }
}

/// Indices of the command's POSITIONAL arguments, in argv order — the
/// words that are neither a flag nor the value of one.
///
/// Why this exists rather than "the last word": pflag reads flags
/// interspersed with positionals, so `docker build . -t api` is legal
/// and people write it — measured, `docker buildx build ./ctx -o ./out`
/// builds ./ctx and writes ./out. Reading the last word as the context
/// picks up `api` instead, and in a repo that happens to have an `api/`
/// directory the wrong tree is synced and built with nothing said.
///
/// Every index is returned, not just the first. `docker build` takes
/// exactly one, so a second is worth an error — but which error, in
/// whose words, is the caller's to choose.
pub fn positionals(args: &[String], tail_start: usize) -> Vec<usize> {
    let mut found = Vec::new();
    let mut i = tail_start;
    while i < args.len() {
        let arg = args[i].as_str();
        // Cobra stops reading flags at `--`, so every word after it is
        // a positional however much it looks like a flag. Measured:
        // `build -- ./ctx -o ./out` writes no ./out, and Docker counts
        // three arguments where it wanted one.
        if arg == "--" {
            found.extend(i + 1..args.len());
            break;
        }
        match classify(arg) {
            Word::Attached(..) => i += 1,
            Word::Detached(_) | Word::Eats => i += 2,
            Word::Plain => {
                // A boolean flag is still a flag. A bare `-` is the one
                // word that leads with a dash and is not one: `docker
                // build -` reads its context from stdin.
                if arg == "-" || !arg.starts_with('-') {
                    found.push(i);
                }
                i += 1;
            }
        }
    }
    found
}

/// Where `-f` put the Dockerfile.
pub struct Dockerfile {
    /// The flag as it was typed, so an error can quote what is on screen.
    pub flag: String,
    /// The word holding the path and the byte range inside it that is
    /// the path alone — so a rewrite splices a new one in without
    /// knowing which of the four spellings it is putting it back into.
    /// `None` when the flag was the last word and named nothing.
    pub at: Option<(usize, Range<usize>)>,
}

/// What `-f` names, in whatever spelling it was written.
///
/// `scan` deliberately returns neither of the two inputs Docker's usage
/// line documents, so nothing is carried across twice — but READING one
/// of them still belongs here, because finding `-f` means knowing which
/// words the flags before it have already eaten, and that table is here.
///
/// docker.rs used to walk the argv itself, matching the literals `-f`,
/// `--file` and a `--file=` prefix. Two ways that was wrong, both
/// measured on Buildx 0.33.0. `-fPATH`, `-f=PATH` and `-qfPATH` all
/// build, and none of them matched — so the Dockerfile never joined the
/// footprint, never widened the anchor and never got rewritten, and the
/// argv went over verbatim: `docker build -f../shared/Dockerfile ./ctx`
/// had the SERVER open its own `../shared/Dockerfile`. And stepping word
/// by word read the `-f` inside `--label -f .` as a real flag, which
/// made the context double as its own Dockerfile.
///
/// The LAST one wins, because that is what pflag does with a repeated
/// string flag — measured: `-f ../shared/Dockerfile -f Dockerfile .`
/// builds the second.
pub fn dockerfile(args: &[String], tail_start: usize) -> Option<Dockerfile> {
    let mut found = None;
    let mut i = tail_start;
    while i < args.len() {
        let arg = args[i].as_str();
        // Past `--` every word is a positional, however much it looks
        // like a flag: `build -- -f Dockerfile .` counts three arguments
        // where Docker wanted one, rather than naming a Dockerfile.
        if arg == "--" {
            break;
        }
        match file_flag(arg) {
            Some(At::Here(at)) => {
                found = Some(Dockerfile {
                    flag: arg.to_string(),
                    at: Some((i, at..arg.len())),
                });
                i += 1;
                continue;
            }
            Some(At::Next) => {
                found = Some(Dockerfile {
                    flag: arg.to_string(),
                    at: args.get(i + 1).map(|value| (i + 1, 0..value.len())),
                });
                i += 2;
                continue;
            }
            None => {}
        }
        match classify(arg) {
            Word::Attached(..) | Word::Plain => i += 1,
            Word::Detached(_) | Word::Eats => i += 2,
        }
    }
    found
}

/// Where the Dockerfile path sits relative to the flag that named it.
enum At {
    /// Attached to the flag's own word, starting at this byte offset.
    Here(usize),
    /// The next word.
    Next,
}

/// Read one word as `-f` might have been spelled, or `None` if it is not
/// the file flag at all. The short side walks the bundle exactly as
/// `classify` does, because it is the same parser: booleans may lead,
/// and the first shorthand that takes a value takes the rest of the word.
fn file_flag(arg: &str) -> Option<At> {
    if let Some(rest) = arg.strip_prefix("--") {
        return match rest.split_once('=') {
            Some(("file", _)) => Some(At::Here("--file=".len())),
            None if rest == "file" => Some(At::Next),
            _ => None,
        };
    }
    if arg.len() < 2 || !arg.starts_with('-') {
        return None;
    }
    for (pos, ch) in arg.char_indices().skip(1) {
        if BOOL_SHORTS.contains(&ch) {
            continue;
        }
        if ch != 'f' {
            return None;
        }
        let after = pos + ch.len_utf8();
        let tail = &arg[after..];
        return match tail.is_empty() {
            true => Some(At::Next),
            // pflag strips exactly one `=` between a shorthand and its
            // attached value, so `-f=x` names `x` and `-f==x` names `=x`.
            false if tail.starts_with('=') => Some(At::Here(after + 1)),
            false => Some(At::Here(after)),
        };
    }
    None
}

/// Whether this build is asking Docker for its own usage text.
///
/// A whole-tail `any` over the words `--help` and `-h` is what this was,
/// and it reads a flag's VALUE as a request: measured, `docker build
/// --build-arg --help .` is a legal build whose `--build-arg` value is
/// the word `--help`. Answering it with "no local context" made Ulak
/// forward the line unsynced, and the SERVER's `.` got built.
///
/// Every ambiguous spelling falls towards BUILDING, because the two
/// mistakes do not cost the same. Missing a help request syncs a
/// workspace nobody needed and then prints the usage anyway; inventing
/// one skips the sync and hands a build to the wrong machine. So
/// `--help=false` (which builds, measured) is not a request, and neither
/// is anything past `--`.
pub fn help_wanted(args: &[String], tail_start: usize) -> bool {
    let mut i = tail_start;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return false;
        }
        if asks_for_help(arg) {
            return true;
        }
        match classify(arg) {
            Word::Attached(..) | Word::Plain => i += 1,
            Word::Detached(_) | Word::Eats => i += 2,
        }
    }
    false
}

/// pflag's truthy words, and only those: a boolean flag spelled
/// `--help=1` is a request and `--help=0` is not. Anything outside this
/// vocabulary makes Docker refuse the whole command, so what Ulak reads
/// it as cannot matter.
const TRUE_WORDS: &[&str] = &["1", "t", "T", "TRUE", "true", "True"];

fn asks_for_help(arg: &str) -> bool {
    if let Some(rest) = arg.strip_prefix("--") {
        return match rest.split_once('=') {
            Some(("help", value)) => TRUE_WORDS.contains(&value),
            None => rest == "help",
            _ => false,
        };
    }
    // `-h` bundles like any other boolean — measured, `docker build -qh
    // .` prints the usage — but only while the bundle is still
    // booleans: in `-fh` the `h` is the Dockerfile's name.
    if arg.len() < 2 || !arg.starts_with('-') {
        return false;
    }
    arg.chars()
        .skip(1)
        .take_while(|ch| BOOL_SHORTS.contains(ch))
        .any(|ch| ch == 'h')
}

// ─── the flag table ─────────────────────────────────────────────────

/// One flag that can name a local path, and how to find the paths inside
/// its value.
struct Carrier {
    long: &'static str,
    /// Of these, Docker gives a short form only to `--output`. `-f` is
    /// the Dockerfile, which docker.rs already owns.
    short: Option<char>,
    find: fn(&str) -> Result<Vec<Found>, NotCsv>,
    /// Whether a READ path that is not here should stop the build. True
    /// wherever the client stats the path itself — measured, one flag at
    /// a time — because then a local build fails on it too.
    ///
    /// `resolve` asks this only about a READ, since a written path is
    /// not meant to exist yet. So on a writing carrier the value decides
    /// nothing at all, and what it is doing there is recording the
    /// measurement beside the flag it belongs to.
    strict: bool,
    /// Shown when the flag arrives without its value.
    example: &'static str,
}

/// A path inside one flag value: the byte range of the path alone, so
/// the rest of the value survives a rewrite untouched.
struct Found {
    span: Range<usize>,
    direction: Direction,
    shape: Shape,
    /// The span sits inside a QUOTED CSV field, so the bytes there are
    /// the ESCAPED spelling of the path and a rewrite has to escape what
    /// it puts back.
    csv_quoted: bool,
}

const CARRIERS: &[Carrier] = &[
    Carrier {
        long: "--secret",
        short: None,
        find: secret,
        strict: true,
        example: "id=npm,src=./.npmrc",
    },
    Carrier {
        long: "--ssh",
        short: None,
        find: ssh,
        strict: true,
        example: "default",
    },
    Carrier {
        long: "--build-context",
        short: None,
        find: build_context,
        strict: true,
        example: "vendor=./vendor",
    },
    // A local cache that is not there is a cache MISS, not a failure:
    // measured — `--cache-from type=local,src=/nope` builds happily — so
    // refusing it here would break builds that work without Ulak.
    Carrier {
        long: "--cache-from",
        short: None,
        find: cache_from,
        strict: false,
        example: "type=local,src=./cache",
    },
    Carrier {
        long: "--cache-to",
        short: None,
        find: cache_to,
        strict: true,
        example: "type=local,dest=./cache",
    },
    Carrier {
        long: "--output",
        short: Some('o'),
        find: output,
        strict: true,
        example: "type=local,dest=./dist",
    },
    // Both are fatal when the directory that should hold them is not
    // there, and both fail LATE — measured, the image is built, exported
    // and tagged first, and only then does `--iidfile` say `writing
    // image ID file: open …: no such file or directory` and
    // `--metadata-file` say `invalid output path: stat …`. Recorded as
    // strict because that is what the client does, not because anything
    // reads it: a WRITE never reaches the strict branch in `resolve`.
    Carrier {
        long: "--iidfile",
        short: None,
        find: written_file,
        strict: true,
        example: "./image-id.txt",
    },
    Carrier {
        long: "--metadata-file",
        short: None,
        find: written_file,
        strict: true,
        example: "./metadata.json",
    },
    // Not strict, and a missing policy file IS fatal — the exception is
    // deliberate. The client resolves this path against the CONTEXT and
    // this module resolves it against cwd, so "it is not on this
    // machine" is not something it can honestly say about a value it
    // measured from the wrong place. See `policy` below.
    Carrier {
        long: "--policy",
        short: None,
        find: policy,
        strict: false,
        // Rego, not JSON — measured, a `{}` in one of these is answered
        // with `rego_parse_error: package expected`.
        example: "filename=policy.rego",
    },
];

/// Every build flag that consumes the FOLLOWING word, so a value is
/// never read as a flag: `--label --secret` sets a label called
/// `--secret` and hides nothing at all.
///
/// NOT from `--help`, which is where this table went wrong the first
/// time. Help prints 36 flags; the client parses 51. The other fifteen
/// are hidden, and ten of them take a value — buildx keeps the classic
/// builder's resource flags as accepted no-ops, and a no-op still eats
/// a word. Left out, `docker build -m 512m .` counted `512m` as a
/// second context and Ulak refused a build the client happily runs.
/// The whole set was re-derived by asking the client one flag at a
/// time — `docker buildx build --<flag>` answers "flag needs an
/// argument", "unknown flag", or "requires 1 argument" at parse time,
/// without ever reaching a daemon — over a 265-name sweep gathered
/// from the entire `docker --help` tree.
///
/// The booleans are left out on purpose: `--check`, `--compress`,
/// `--force-rm`, `--load`, `--no-cache`, `--pull`, `--push`, `--rm`,
/// `--squash`, `-D/--debug`, `-q/--quiet` and `--help`. `--provenance`
/// and `--sbom` belong here despite reading like switches: measured,
/// `--provenance ./ctx` swallows the context and Docker then reports
/// it has none.
const VALUE_FLAGS: &[&str] = &[
    "--add-host",
    "--allow",
    "--annotation",
    "--attest",
    "--build-arg",
    "--build-context",
    "--builder",
    "--cache-from",
    "--cache-to",
    "--call",
    "--cgroup-parent",
    "--cpu-period",
    "--cpu-quota",
    "--cpu-shares",
    "--cpuset-cpus",
    "--cpuset-mems",
    "--file",
    "--iidfile",
    "--isolation",
    "--label",
    "--memory",
    "--memory-swap",
    "--metadata-file",
    "--network",
    "--no-cache-filter",
    "--output",
    "--platform",
    "--policy",
    "--print",
    "--progress",
    "--provenance",
    "--sbom",
    "--secret",
    "--security-opt",
    "--shm-size",
    "--ssh",
    "--tag",
    "--target",
    "--ulimit",
];

/// Short flags that take a value. pflag accepts `-o ./dist`, `-o=./dist`
/// and `-o./dist`, and lets booleans lead in the same word — `-qo
/// ./dist` works too. All four measured against the real client.
///
/// Measured exhaustively, every letter a–z and A–Z, because a long name
/// in `VALUE_FLAGS` does nothing for its short spelling: `-m` and `-c`
/// are the hidden `--memory` and `--cpu-shares`, and until they were
/// here `docker build -m 512m .` was read as two contexts and refused.
const VALUE_SHORTS: &[char] = &['c', 'f', 'm', 'o', 't'];

/// Short flags that take none, so a bundle can be read past them. The
/// same sweep found exactly these three; `-h` still parses as one, even
/// though the client answers it by calling the spelling deprecated.
const BOOL_SHORTS: &[char] = &['D', 'q', 'h'];

// ─── reading one argv word ──────────────────────────────────────────

/// What one argv word is, read the way Docker's own parser reads it.
enum Word {
    /// A path-carrying flag whose value is attached, at this byte
    /// offset: `--secret=X`, `-oX`, `-o=X`.
    Attached(&'static Carrier, usize),
    /// A path-carrying flag whose value is the next word.
    Detached(&'static Carrier),
    /// A flag that eats the next word but names no path.
    Eats,
    /// A boolean flag, a positional, or a flag we have no interest in.
    Plain,
}

fn classify(arg: &str) -> Word {
    if let Some(rest) = arg.strip_prefix("--") {
        let (name, value_at) = match rest.find('=') {
            Some(eq) => (&rest[..eq], Some("--".len() + eq + 1)),
            None => (rest, None),
        };
        if let Some(carrier) = CARRIERS.iter().find(|c| c.long["--".len()..] == *name) {
            return match value_at {
                Some(at) => Word::Attached(carrier, at),
                None => Word::Detached(carrier),
            };
        }
        // `--label=x` carries its value in the same word, so the next
        // word is free again.
        if value_at.is_none() && VALUE_FLAGS.contains(&arg) {
            return Word::Eats;
        }
        return Word::Plain;
    }

    // A short bundle is read left to right: booleans may lead, and the
    // first shorthand that takes a value takes the rest of the word —
    // or, when nothing is left of it, the next word.
    if arg.len() > 1 && arg.starts_with('-') {
        for (pos, ch) in arg.char_indices().skip(1) {
            if BOOL_SHORTS.contains(&ch) {
                continue;
            }
            if !VALUE_SHORTS.contains(&ch) {
                // An unknown shorthand; Docker will reject the whole
                // command, and guessing past it could only invent work.
                return Word::Plain;
            }
            let after = pos + ch.len_utf8();
            let tail = &arg[after..];
            let at = if tail.starts_with('=') {
                after + 1
            } else {
                after
            };
            let carrier = CARRIERS.iter().find(|c| c.short == Some(ch));
            return match (carrier, tail.is_empty()) {
                (Some(carrier), true) => Word::Detached(carrier),
                (Some(carrier), false) => Word::Attached(carrier, at),
                (None, true) => Word::Eats,
                (None, false) => Word::Plain,
            };
        }
    }
    Word::Plain
}

fn collect(
    cwd: &Path,
    out: &mut Vec<LocalInput>,
    carrier: &'static Carrier,
    index: usize,
    offset: usize,
    value: &str,
) -> Result<()> {
    // A `find` is handed the value and never the flag it came from, so
    // the flag — half of a message worth reading — is added here.
    let found = (carrier.find)(value).map_err(|NotCsv(why)| csv_error(carrier.long, value, why))?;
    for found in found {
        let raw = &value[found.span.clone()];
        // Inside a quoted field `""` is one literal quote, so the bytes
        // in the argv are not the path: a directory called `a"b` is
        // written `"src=./a""b"`. Resolving the escaped spelling would
        // stat a name with two quotes in it and report the wrong path
        // missing — while the span, which is what a rewrite splices
        // into, stays the escaped bytes it has to be.
        let raw = match found.csv_quoted && raw.contains('"') {
            true => Cow::Owned(raw.replace("\"\"", "\"")),
            false => Cow::Borrowed(raw),
        };
        out.push(LocalInput {
            index,
            span: offset + found.span.start..offset + found.span.end,
            path: resolve(cwd, &raw, &found, carrier)?,
            direction: found.direction,
            shape: found.shape,
            csv_quoted: found.csv_quoted,
            flag: carrier.long,
        });
    }
    Ok(())
}

/// Absolute, and canonical when Docker would have had to find the file.
///
/// `~` is deliberately NOT expanded. Measured: buildx passes the value
/// through untouched, so `--secret id=npm,src=~/.npmrc` makes the client
/// stat a directory literally named `~` and fail. Expanding it here
/// would let an Ulak build succeed where the same command fails locally,
/// which is the one difference this project must never introduce — so
/// the error below explains the tilde instead.
fn resolve(cwd: &Path, raw: &str, found: &Found, carrier: &Carrier) -> Result<PathBuf> {
    let joined = match Path::new(raw) {
        p if p.is_absolute() => p.to_path_buf(),
        p => cwd.join(p),
    };
    if found.direction == Direction::Write {
        return Ok(folded(&joined));
    }
    match joined.canonicalize() {
        Ok(path) => Ok(path),
        Err(_) if !carrier.strict => Ok(folded(&joined)),
        Err(_) => {
            let hint = if raw.starts_with('~') {
                "docker does not expand `~` inside a flag value — write $HOME/… instead"
            } else {
                "check the path: docker reads it on this machine, not on the server"
            };
            Err(fail!(
                "{} names {}, which is not on this machine",
                carrier.long,
                joined.display()
            )
            .now(hint)
            .into_err())
        }
    }
}

/// `a/../b` folded to `b` without asking the filesystem.
///
/// A written path cannot be canonicalized, because the point of it is
/// that it does not exist yet. It still has to come back clean: callers
/// test these against the workspace anchor with `starts_with`, which a
/// surviving `..` component would quietly defeat.
fn folded(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            // The input is already absolute, so this cannot climb out
            // past the root — `pop` on `/` simply leaves `/`.
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            part => out.push(part),
        }
    }
    out
}

// ─── one value at a time ────────────────────────────────────────────

/// `--secret id=npm,src=./.npmrc`, with `source=` for the same thing.
///
/// `type=env` turns `src=` into the NAME of an environment variable —
/// buildx's own fallback when a secret has no file — so it names nothing
/// on disk. Measured: a build with `--secret type=env,id=t,src=NOT_A_PATH`
/// stats nothing and runs. A secret with no `src` at all is the same
/// story: buildx reads the environment variable named by its id.
fn secret(value: &str) -> Result<Vec<Found>, NotCsv> {
    let fields = fields(value)?;
    if fields.iter().any(|f| f.is("type") && f.value == "env") {
        return Ok(Vec::new());
    }
    Ok(fields
        .iter()
        .filter(|f| f.is("src") || f.is("source"))
        .map(|f| Found {
            span: f.span.clone(),
            direction: Direction::Read,
            shape: Shape::File,
            csv_quoted: f.quoted,
        })
        .collect())
}

/// `--ssh default`, `--ssh NAME=KEY[,KEY…]`.
///
/// Buildx splits this on the FIRST `=` and then on commas, so a single
/// value can name several keys and each needs its own span. With no `=`
/// the value is only an id and buildkit forwards this machine's agent
/// socket — `--ssh default` names no file to carry.
///
/// A path here may itself be an agent socket rather than a key, since
/// buildkit accepts either, so a caller must look at what it found
/// before trying to copy it.
/// Not a CSV value, measured: `--ssh 'gh=./k,d/key'` is split on plain
/// commas and answers `stat ./k`, and quoting a key keeps the quotes in
/// the name it stats. So the CSV reading below is deliberately not
/// applied here.
fn ssh(value: &str) -> Result<Vec<Found>, NotCsv> {
    let Some(eq) = value.find('=') else {
        return Ok(Vec::new());
    };
    let mut at = eq + 1;
    let mut found = Vec::new();
    for key in value[eq + 1..].split(',') {
        let span = at..at + key.len();
        at += key.len() + 1;
        if !key.is_empty() {
            found.push(Found {
                span,
                direction: Direction::Read,
                shape: Shape::File,
                csv_quoted: false,
            });
        }
    }
    Ok(found)
}

/// `--build-context NAME=PATH`, where PATH is local unless Docker
/// resolves it somewhere else entirely.
/// Not a CSV value either, measured: `--build-context 'v=./k,d'` builds
/// from a directory whose name holds the comma, and quoting the field
/// whole is answered with `invalid context name "v`.
fn build_context(value: &str) -> Result<Vec<Found>, NotCsv> {
    // Without a name Docker rejects the flag outright.
    let Some(eq) = value.find('=') else {
        return Ok(Vec::new());
    };
    let at = eq + 1;
    let raw = &value[at..];

    // An OCI layout is a URL that is really a local directory, and the
    // client is what reads it: buildx replaces the directory with a
    // content-store id of its own before the reference ever reaches
    // BuildKit, which is visible when the reference is malformed —
    // `could not parse oci-layout reference "qh0nx2ckj2ql59a5tado5jgv0:@sha256:…"`.
    // A missing directory fails the build with `unable to get info
    // about digest: NotFound`. Only the directory is spanned, so a
    // `:tag@digest` after it survives a rewrite.
    //
    // Two things that make "measured" weaker here than the flat
    // sentence above suggests, and both were measured too: the failure
    // needs the Dockerfile to actually REFERENCE the context, since
    // buildx resolves these lazily and an unreferenced missing layout
    // exits 0; and it needs BuildKit not to be holding the content
    // already, because a digest in the store builds fine without the
    // directory even under `--no-cache`. So spanning the directory is
    // right, but a missing one is not reliably surfaced by Docker.
    //
    // (An earlier note here quoted "could not lock /…/index.json.lock".
    // That string is real but belongs to `--cache-from type=local`,
    // where it is a WARNING and the build continues.)
    if let Some(rest) = raw.strip_prefix(OCI_LAYOUT) {
        let start = at + OCI_LAYOUT.len();
        let dir = rest.split('@').next().unwrap_or(rest);
        let dir = dir.split(':').next().unwrap_or(dir);
        if dir.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(vec![Found {
            span: start..start + dir.len(),
            direction: Direction::Read,
            shape: Shape::Dir,
            csv_quoted: false,
        }]);
    }
    if resolved_elsewhere(raw) {
        return Ok(Vec::new());
    }
    Ok(vec![Found {
        span: at..value.len(),
        direction: Direction::Read,
        shape: Shape::Either,
        csv_quoted: false,
    }])
}

const OCI_LAYOUT: &str = "oci-layout://";

/// A `--build-context` value that names something other than this
/// filesystem, mirroring buildkit's own prefix list.
///
/// Deliberately no wider than that list: a bare `alpine:latest` reads
/// like an image reference and is NOT one — measured, the client stats
/// it as a path and reports that path missing. Treating it as an image
/// here would forward it to the server and lose the error.
fn resolved_elsewhere(raw: &str) -> bool {
    const ELSEWHERE: &[&str] = &[
        // Git and URL contexts, fetched by the builder.
        "http://",
        "https://",
        "git://",
        "git@",
        "github.com/",
        // An image, pulled by the builder.
        "docker-image://",
        // Another stage of this same build.
        "target:",
    ];
    ELSEWHERE.iter().any(|prefix| raw.starts_with(prefix))
}

/// `--cache-from type=local,src=DIR`.
fn cache_from(value: &str) -> Result<Vec<Found>, NotCsv> {
    cache(value, "src", Direction::Read)
}

/// `--cache-to type=local,dest=DIR`.
fn cache_to(value: &str) -> Result<Vec<Found>, NotCsv> {
    cache(value, "dest", Direction::Write)
}

/// Only `type=local` keeps its cache on this machine. Every other
/// backend — registry, gha, s3, azblob, inline, and the bare
/// `user/app:cache` shorthand — names nothing here.
fn cache(value: &str, key: &str, direction: Direction) -> Result<Vec<Found>, NotCsv> {
    let fields = fields(value)?;
    if !fields.iter().any(|f| f.is("type") && f.value == "local") {
        return Ok(Vec::new());
    }
    Ok(fields
        .iter()
        .filter(|f| f.is(key))
        .map(|f| Found {
            span: f.span.clone(),
            direction,
            shape: Shape::Dir,
            csv_quoted: f.quoted,
        })
        .collect())
}

/// `--output type=local,dest=DIR`, `type=tar|oci|docker,dest=FILE`, and
/// the shorthand where the whole value is a directory.
///
/// The shorthand is worth measuring rather than assuming, because it is
/// not what the name suggests: `-o ./out.tar` writes a DIRECTORY called
/// `out.tar`. Only an explicit `type=` ever selects a tarball.
fn output(value: &str) -> Result<Vec<Found>, NotCsv> {
    let fields = fields(value)?;
    // Buildx's own rule, mirrored: one field that came through the CSV
    // reader UNCHANGED and does not begin `type=` IS the destination,
    // whatever it looks like inside. A bare `-` is its tar-to-stdout
    // form, and our stdout is already the user's.
    //
    // "Unchanged" is the part worth measuring rather than assuming, and
    // it is why a quoted field is excluded: `-o '"./out1"'` is one field
    // and holds no comma, but buildx compares the field against the
    // string it was handed, sees the quotes gone, and refuses the whole
    // value with `invalid value ./out1`. Reading it as a destination
    // here would put a directory in the footprint for a build that never
    // starts.
    if fields.len() == 1 && !fields[0].quoted && !value.starts_with("type=") {
        if value.is_empty() || value == "-" {
            return Ok(Vec::new());
        }
        return Ok(vec![Found {
            span: 0..value.len(),
            direction: Direction::Write,
            shape: Shape::Dir,
            csv_quoted: false,
        }]);
    }
    let kind = fields
        .iter()
        .find(|f| f.is("type"))
        .map_or("", |f| f.value.as_ref());
    // `docker` and `oci` write one archive, unless `tar` turns that off
    // — then they unpack an image DIRECTORY instead. Read the way
    // buildx reads it, with Go's `strconv.ParseBool`, because both
    // spellings are typed: measured on Buildx 0.33.0, `-o
    // type=oci,dest=./out-tree,tar=false` and `,tar=0` each leave a
    // directory holding `blobs/`, `index.json` and `oci-layout`.
    //
    // Calling it a file put a zero-byte placeholder there — `make_room`
    // creates one for every Write it thinks is a file — and rsync
    // pushed that file to the same path on the server, where the build
    // then stopped on buildx's own "failed to build: destination
    // directory ./out-tree is a file". The footprint had the wrong shape
    // too, so even a build that got past it would have come home by the
    // single-file rule `docker.rs` measured as carrying nothing.
    //
    // `bake.rs::writes_a_directory` is the same rule on the plan buildx
    // prints, where the value has already been normalised to "false".
    let unpacked = fields
        .iter()
        .find(|f| f.is("tar"))
        .is_some_and(|f| !crate::runspec::flag_bool(&f.value));
    let writes_a_directory = kind == "local" || (matches!(kind, "docker" | "oci") && unpacked);
    Ok(fields
        .iter()
        .filter(|f| f.is("dest") && f.value != "-")
        .map(|f| Found {
            span: f.span.clone(),
            direction: Direction::Write,
            shape: if writes_a_directory {
                Shape::Dir
            } else {
                Shape::File
            },
            csv_quoted: f.quoted,
        })
        .collect())
}

/// `--policy filename=PATH[,filename=PATH…]` — repeatable inside one
/// value, like `--ssh`. Its other keys (reset, disabled, strict,
/// log-level) name no file. The flag is real on Buildx 0.33.0 and the
/// files are Rego, not JSON: a `{}` in one is answered with
/// `rego_parse_error: package expected`.
///
/// The CLIENT reads them — measured, a `.dockerignore` that excludes the
/// file does not stop it loading, so it is not coming back out of the
/// context the daemon was sent. But it is resolved against the CONTEXT
/// ROOT rather than cwd, and that is the whole difficulty here: `docker
/// build --policy filename=pol.rego ./ctx` loads `ctx/pol.rego` and
/// never looks at `./pol.rego`, and anything that leaves that root is
/// refused whatever is at the far end — an absolute path answers `stat
/// /tmp/p/policy.json: invalid argument` with the file sitting right
/// there, and `../pol.rego` answers the same. A missing one is fatal
/// before a single build step runs: `policy file pol.rego not found`,
/// and no image.
///
/// Which is why the carrier is not strict. This module is never told
/// where the context is — that word is docker.rs's — so the path it
/// resolves is the client's only when cwd and context coincide, and
/// refusing a build because there is no `./pol.rego` would refuse one
/// the client runs. The file still comes across in the ordinary case,
/// because a path inside the context is inside the tree docker.rs
/// already syncs.
fn policy(value: &str) -> Result<Vec<Found>, NotCsv> {
    Ok(fields(value)?
        .iter()
        .filter(|f| f.is("filename"))
        .map(|f| Found {
            span: f.span.clone(),
            direction: Direction::Read,
            shape: Shape::File,
            csv_quoted: f.quoted,
        })
        .collect())
}

/// `--iidfile PATH`, `--metadata-file PATH`: the whole value, written by
/// the CLIENT once the build is done and never by the daemon. Measured
/// the only way that separates the two — the docker CLI run inside a
/// container against the host's daemon over a mounted socket, where both
/// files landed in the container's own filesystem, which the daemon
/// cannot reach.
///
/// A build that fails writes neither, which is what lets docker.rs take
/// its placeholders back: a `RUN false` with both flags set leaves
/// nothing on disk at all. One measured exception matters there, and it
/// is `--iidfile`: buildx creates it EMPTY when a build succeeds without
/// producing an image, which `-o type=cacheonly` and `--call check` both
/// do. So a zero-byte file here is not proof that Docker wrote nothing —
/// and it can outlive a non-zero exit too, since `-o type=cacheonly
/// --iidfile A --metadata-file <missing dir>/m.json` exits 1 with A
/// written and empty. What that costs is only the file's existence,
/// because Docker's byte is the placeholder's byte.
fn written_file(value: &str) -> Result<Vec<Found>, NotCsv> {
    if value.is_empty() || value == "-" {
        return Ok(Vec::new());
    }
    Ok(vec![Found {
        span: 0..value.len(),
        direction: Direction::Write,
        shape: Shape::File,
        csv_quoted: false,
    }])
}

// ─── CSV values ─────────────────────────────────────────────────────

/// One `key=value` field of a CSV flag value.
struct Field<'a> {
    key: Cow<'a, str>,
    value: Cow<'a, str>,
    /// Where the VALUE sits in the whole flag value, in the bytes as
    /// TYPED: inside the quotes when the field was quoted, and with any
    /// doubled quote still doubled. That is what a rewrite splices into,
    /// so it has to be the spelling docker will read back.
    span: Range<usize>,
    /// The field was quoted whole, so it may hold commas and its quotes
    /// are doubled — both of which a replacement has to honour.
    quoted: bool,
}

impl Field<'_> {
    fn is(&self, key: &str) -> bool {
        self.key.trim().eq_ignore_ascii_case(key)
    }
}

/// The fields of a CSV flag value, read as docker's own reader reads it.
///
/// Docker parses these with a CSV reader, which makes them order-free
/// and repeatable: `src=./f,id=npm` is the same secret as
/// `id=npm,src=./f`, and `--policy` may carry `filename=` twice. Keys
/// are matched case-insensitively because Docker lowercases them;
/// values are matched exactly because Docker does not.
///
/// A CSV RECORD, though, and not merely a comma-separated string — which
/// is the whole reason this function is longer than a `split(',')`. A
/// field holding a comma is quoted WHOLE, `""` inside it is one literal
/// quote, and both are docker's rules rather than ours: measured on
/// 29.4.0 / buildx 0.33.0, `--secret 'id=t,"src=./a,b/f"'` reads the
/// file `./a,b/f` and builds, and `--output 'type=local,"dest=./a,b"'`
/// writes the directory `./a,b`.
///
/// Splitting on every comma instead produced a field spelled `"src=./a`,
/// whose key is `"src` and therefore not a source at all: no path was
/// found, so nothing was synced, nothing was respelled, and the argv
/// went over verbatim — the build read the SERVER's `./a,b/f`, and the
/// `--output` one left its artefacts in a directory on the server while
/// the empty `./a,b` here read as a build that produced nothing. That is
/// this module's own failure mode arriving through a quoting rule, and
/// `runspec::csv_fields` already fixes the same one for `--mount`.
///
/// A value that is not a CSV record is refused rather than half-read.
/// Docker refuses each of these shapes too — measured, `src="./a,b"`,
/// `src=./a"b`, an unclosed quote and a quoted field with a tail all
/// answer `parse error on line 1: bare " in non-quoted-field` or
/// `extraneous or missing " in quoted-field` — so the cost is one
/// clearer message earlier; the alternative is guessing where a path
/// ends, and a guess here is the wrong machine's filesystem.
fn fields(value: &str) -> Result<Vec<Field<'_>>, NotCsv> {
    let bytes = value.as_bytes();
    let mut fields = Vec::new();
    let mut i = 0;
    loop {
        let start = i;
        let quoted = bytes.get(i) == Some(&b'"');
        let content = if quoted {
            i += 1;
            let from = i;
            loop {
                let Some(rel) = bytes[i..].iter().position(|b| *b == b'"') else {
                    return Err(NotCsv("a quoted field is never closed"));
                };
                i += rel + 1;
                if bytes.get(i) != Some(&b'"') {
                    break;
                }
                i += 1;
            }
            &value[from..i - 1]
        } else {
            let end = bytes[i..]
                .iter()
                .position(|b| *b == b',')
                .map_or(bytes.len(), |p| i + p);
            let text = &value[i..end];
            if text.contains('"') {
                return Err(NotCsv("a field holds a quote it did not open with"));
            }
            i = end;
            text
        };
        // The `=` is found in the ORIGINAL bytes rather than the
        // unescaped ones, because a doubled quote ahead of it would put
        // the value's span a byte off from where the argv keeps it. The
        // opening quote is the only other thing between the field and
        // its content, and it is exactly one byte wide.
        let at = start + usize::from(quoted);
        fields.push(match content.find('=') {
            Some(eq) => Field {
                key: unescape(&content[..eq], quoted),
                value: unescape(&content[eq + 1..], quoted),
                span: at + eq + 1..at + content.len(),
                quoted,
            },
            None => Field {
                key: Cow::Borrowed(""),
                value: unescape(content, quoted),
                span: at..at + content.len(),
                quoted,
            },
        });
        match bytes.get(i) {
            None => return Ok(fields),
            Some(&b',') => i += 1,
            // Only reachable after a quoted field: an unquoted one runs
            // to the next comma or to the end.
            Some(_) => return Err(NotCsv("a quoted field carries on after its closing quote")),
        }
    }
}

/// A quoted field's `""` back to the one quote docker reads it as.
fn unescape(text: &str, quoted: bool) -> Cow<'_, str> {
    match quoted && text.contains('"') {
        true => Cow::Owned(text.replace("\"\"", "\"")),
        false => Cow::Borrowed(text),
    }
}

/// Why a value is not a CSV record docker could read. Only the reason:
/// a `find` is handed the value and never the flag it came from, and the
/// flag is half of a message worth reading, so `collect` adds it.
struct NotCsv(&'static str);

fn csv_error(flag: &str, value: &str, why: &str) -> anyhow::Error {
    fail!("{flag} {value} is not something docker can read: {why}")
        .now("a field holding a comma is quoted WHOLE — \"src=./a,b\" — and a quote inside one is doubled")
        .now("ulak stops here rather than guess where the path ends, because a guess is the wrong machine's filesystem")
        .into_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// A project holding the files these flags name, because a READ path
    /// this module reports has to be one that is really there.
    fn project() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for dir in ["keys", "cache", "vendor", "layout"] {
            std::fs::create_dir(root.join(dir)).unwrap();
        }
        for file in [".npmrc", "policy.json", "keys/id_rsa", "keys/id_ed25519"] {
            std::fs::write(root.join(file), "x\n").unwrap();
        }
        (temp, root)
    }

    fn shape_of(cwd: &Path, args: &[&str]) -> Vec<(&'static str, Direction, Shape)> {
        scan(cwd, &v(args), 1)
            .unwrap()
            .iter()
            .map(|i| (i.flag, i.direction, i.shape))
            .collect()
    }

    #[test]
    fn every_flag_that_names_a_local_path_is_found_in_argv_order() {
        let (_temp, cwd) = project();
        let args = v(&[
            "build",
            "--secret",
            "id=npm,src=./.npmrc",
            "--ssh",
            "github=keys/id_ed25519",
            "--build-context",
            "vendor=./vendor",
            "--cache-from",
            "type=local,src=./cache",
            "--cache-to",
            "type=local,dest=./cache",
            "--output",
            "type=local,dest=./dist",
            "--iidfile",
            "./iid.txt",
            "--metadata-file",
            "./meta.json",
            "--policy",
            "filename=./policy.json",
            "-t",
            "api",
            ".",
        ]);

        let found = scan(&cwd, &args, 1).unwrap();

        let seen: Vec<_> = found
            .iter()
            .map(|i| (i.flag, i.direction, i.shape))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("--secret", Direction::Read, Shape::File),
                ("--ssh", Direction::Read, Shape::File),
                ("--build-context", Direction::Read, Shape::Either),
                ("--cache-from", Direction::Read, Shape::Dir),
                ("--cache-to", Direction::Write, Shape::Dir),
                ("--output", Direction::Write, Shape::Dir),
                ("--iidfile", Direction::Write, Shape::File),
                ("--metadata-file", Direction::Write, Shape::File),
                ("--policy", Direction::Read, Shape::File),
            ]
        );
        assert_eq!(found[0].path, cwd.join(".npmrc"));
        assert_eq!(
            found[5].path,
            cwd.join("dist"),
            "a written path need not exist"
        );
        // The tag and the context belong to nobody here: `-t api` takes
        // a value, and the context is docker.rs's.
        assert!(found.iter().all(|i| i.index < args.len() - 3));
    }

    #[test]
    fn the_context_and_the_dockerfile_stay_with_docker_rs() {
        // Both are already synced by `BuildInput`; returning them here
        // would put them through the footprint twice.
        let (_temp, cwd) = project();
        for spelling in [
            vec!["build", "-f", "Dockerfile", "."],
            vec!["build", "--file", "Dockerfile", "."],
            vec!["build", "--file=Dockerfile", "."],
            vec!["build", "-fDockerfile", "."],
            vec!["build", "-f=Dockerfile", "."],
        ] {
            assert!(
                scan(&cwd, &v(&spelling), 1).unwrap().is_empty(),
                "{spelling:?}"
            );
        }
    }

    #[test]
    fn a_flag_reaches_the_same_path_in_every_spelling_docker_accepts() {
        let (_temp, cwd) = project();
        for spelling in [
            vec!["build", "--output", "type=local,dest=./dist", "."],
            vec!["build", "--output=type=local,dest=./dist", "."],
            vec!["build", "-o", "./dist", "."],
            vec!["build", "-o./dist", "."],
            vec!["build", "-o=./dist", "."],
            // A boolean may lead in the same word; the value still
            // belongs to the shorthand that takes one.
            vec!["build", "-qo", "./dist", "."],
        ] {
            let found = scan(&cwd, &v(&spelling), 1).unwrap();
            assert_eq!(found.len(), 1, "{spelling:?}");
            assert_eq!(found[0].path, cwd.join("dist"), "{spelling:?}");
            assert_eq!(found[0].direction, Direction::Write, "{spelling:?}");
        }
    }

    #[test]
    fn a_secret_finds_its_file_whichever_way_round_it_is_written() {
        let (_temp, cwd) = project();
        for value in [
            "id=npm,src=./.npmrc",
            "src=./.npmrc,id=npm",
            "id=npm,source=./.npmrc",
            "source=./.npmrc,id=npm",
            "id=npm,type=file,src=./.npmrc",
        ] {
            let found = scan(&cwd, &v(&["build", "--secret", value, "."]), 1).unwrap();
            assert_eq!(found.len(), 1, "{value}");
            assert_eq!(found[0].path, cwd.join(".npmrc"), "{value}");
        }
    }

    /// A project with a directory whose name holds a comma, because the
    /// whole point of the CSV reading is a path that cannot be found by
    /// splitting on commas.
    fn comma_project() -> (tempfile::TempDir, PathBuf) {
        let (temp, root) = project();
        std::fs::create_dir(root.join("a,b")).unwrap();
        std::fs::write(root.join("a,b/f"), "x\n").unwrap();
        (temp, root)
    }

    #[test]
    fn a_path_holding_a_comma_is_quoted_whole_and_still_found() {
        // Measured on Buildx 0.33.0, one flag at a time: `--secret
        // 'id=t,"src=./a,b/f"'` reads ./a,b/f and builds, and `--output
        // 'type=local,"dest=./a,b"'` writes the directory ./a,b. Split
        // on every comma these values yield a field called `"src`, which
        // is not a source at all — so nothing was found, nothing was
        // synced, the argv went over verbatim, and the build read and
        // wrote the SERVER's ./a,b without a word.
        let (_temp, cwd) = comma_project();
        for (flag, value, path, direction, after) in [
            (
                "--secret",
                r#"id=t,"src=./a,b/f""#,
                "a,b/f",
                Direction::Read,
                r#"id=t,"src=/srv/x,y""#,
            ),
            (
                "--cache-from",
                r#"type=local,"src=./a,b""#,
                "a,b",
                Direction::Read,
                r#"type=local,"src=/srv/x,y""#,
            ),
            (
                "--cache-to",
                r#"type=local,"dest=./a,b""#,
                "a,b",
                Direction::Write,
                r#"type=local,"dest=/srv/x,y""#,
            ),
            (
                "--output",
                r#"type=local,"dest=./a,b""#,
                "a,b",
                Direction::Write,
                r#"type=local,"dest=/srv/x,y""#,
            ),
            (
                "--policy",
                r#""filename=./a,b/f""#,
                "a,b/f",
                Direction::Read,
                r#""filename=/srv/x,y""#,
            ),
        ] {
            let mut args = v(&["build", flag, value, "."]);
            let found = scan(&cwd, &args, 1).unwrap();
            assert_eq!(found.len(), 1, "{flag} {value}");
            assert_eq!(found[0].path, cwd.join(path), "{flag} {value}");
            assert_eq!(found[0].direction, direction, "{flag} {value}");
            // And the remote path goes back INSIDE the quotes, so what
            // docker reads is still one field — a path holding a comma
            // spliced outside them would be two.
            rewrite(&mut args, &found, |_| "/srv/x,y".to_string());
            assert_eq!(args[2], after, "{flag} {value}");
        }
    }

    #[test]
    fn a_doubled_quote_is_one_quote_in_the_path_and_goes_back_doubled() {
        // Measured: `--secret 'id=t,"src=./a""b"'` answers `failed to
        // stat ./a"b`, so the bytes in the argv are the ESCAPED spelling
        // and the path is one quote shorter. Carrying the escaped
        // spelling across would sync a file nobody has.
        let (_temp, cwd) = project();
        std::fs::write(cwd.join("a\"b"), "x\n").unwrap();
        let mut args = v(&["build", "--secret", r#"id=t,"src=./a""b""#, "."]);

        let found = scan(&cwd, &args, 1).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, cwd.join("a\"b"));
        rewrite(&mut args, &found, |input| {
            format!("/srv/{}", input.path.file_name().unwrap().to_string_lossy())
        });
        assert_eq!(args[2], r#"id=t,"src=/srv/a""b""#);
    }

    #[test]
    fn a_value_docker_cannot_read_as_csv_is_refused_rather_than_half_read() {
        // The three shapes docker's own reader refuses, measured one at
        // a time: `bare " in non-quoted-field` for the first, and
        // `extraneous or missing " in quoted-field` for the other two.
        // None of them builds, so refusing here costs one clearer
        // message earlier and no working command.
        let (_temp, cwd) = comma_project();
        for value in [
            r#"id=t,src="./a,b/f""#,
            r#"id=t,"src=./a,b/f"#,
            r#"id=t,"src=./a"x"#,
        ] {
            let err = scan(&cwd, &v(&["build", "--secret", value, "."]), 1).unwrap_err();
            assert!(err.to_string().contains("--secret"), "{value}: {err}");
        }
        // The pair matters more than either half: the guard that used to
        // stand here refused `src="./a,b/f"`, which docker refuses
        // anyway, and never saw the spelling docker ACCEPTS, because the
        // comma split had already lost it.
        assert_eq!(
            scan(
                &cwd,
                &v(&["build", "--secret", r#"id=t,"src=./a,b/f""#, "."]),
                1
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn an_output_field_that_lost_its_quotes_is_not_the_shorthand_destination() {
        // Buildx compares its single field against the string it was
        // handed and takes the shorthand only when they are the same
        // bytes: measured, `-o '"./out1"'` answers `invalid value
        // ./out1` and `-o './o,c'` answers `invalid value ./o`, and
        // neither writes anything. Reading either as a destination put a
        // directory in the footprint for a build that never starts.
        let (_temp, cwd) = project();
        for value in [r#""./out1""#, "./o,c"] {
            assert!(
                scan(&cwd, &v(&["build", "-o", value, "."]), 1)
                    .unwrap()
                    .is_empty(),
                "{value}"
            );
        }
    }

    #[test]
    fn an_environment_secret_names_no_file_even_when_it_says_src() {
        // With `type=env`, buildx reads `src` as the NAME of a variable.
        // Treating it as a path would report a missing file and refuse a
        // build that works.
        let (_temp, cwd) = project();
        for value in [
            "type=env,id=npm,src=NPM_TOKEN",
            "id=npm,type=env,src=NPM_TOKEN",
            "type=env,id=npm",
            // No `src` at all: the variable is named by the id.
            "id=NPM_TOKEN",
        ] {
            assert!(
                scan(&cwd, &v(&["build", "--secret", value, "."]), 1)
                    .unwrap()
                    .is_empty(),
                "{value}"
            );
        }
    }

    #[test]
    fn a_bare_ssh_default_forwards_the_agent_and_not_a_file() {
        let (_temp, cwd) = project();
        for value in ["default", "github"] {
            assert!(
                scan(&cwd, &v(&["build", "--ssh", value, "."]), 1)
                    .unwrap()
                    .is_empty(),
                "{value}"
            );
        }
    }

    #[test]
    fn one_ssh_value_can_carry_several_keys() {
        let (_temp, cwd) = project();
        let args = v(&["build", "--ssh", "gh=keys/id_rsa,keys/id_ed25519", "."]);

        let found = scan(&cwd, &args, 1).unwrap();

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].path, cwd.join("keys/id_rsa"));
        assert_eq!(found[1].path, cwd.join("keys/id_ed25519"));
        // Both keys live in the same argv word, so each span has to
        // cover its own key and nothing else — not the id in front of
        // the first, nor the comma between them.
        assert_eq!(found[0].index, found[1].index);
        for (input, key) in found.iter().zip(["keys/id_rsa", "keys/id_ed25519"]) {
            assert_eq!(&args[input.index][input.span.clone()], key);
        }
    }

    #[test]
    fn rewriting_two_paths_in_one_argument_leaves_both_of_them_right() {
        let (_temp, cwd) = project();
        let mut args = v(&[
            "build",
            "--ssh",
            "gh=keys/id_rsa,keys/id_ed25519",
            "--secret",
            "id=npm,src=./.npmrc",
            "-t",
            "api",
            ".",
        ]);

        let found = scan(&cwd, &args, 1).unwrap();
        rewrite(&mut args, &found, |input| {
            format!("/srv/{}", input.path.file_name().unwrap().to_string_lossy())
        });

        assert_eq!(args[2], "gh=/srv/id_rsa,/srv/id_ed25519");
        assert_eq!(args[4], "id=npm,src=/srv/.npmrc");
        // Everything this module does not own is left exactly as typed.
        assert_eq!(args[6], "api");
        assert_eq!(args[7], ".");
    }

    #[test]
    fn a_remote_build_context_is_not_a_local_path() {
        let (_temp, cwd) = project();
        for value in [
            "src=https://github.com/x/y.git",
            "src=http://example.invalid/ctx.tar",
            "src=git://example.invalid/x.git",
            "src=git@example.invalid:x/y.git",
            "src=github.com/x/y",
            "img=docker-image://alpine:latest",
            "base=target:builder",
            // No name at all: Docker rejects the flag itself.
            "./vendor",
        ] {
            assert!(
                scan(&cwd, &v(&["build", "--build-context", value, "."]), 1)
                    .unwrap()
                    .is_empty(),
                "{value}"
            );
        }
    }

    #[test]
    fn an_oci_layout_build_context_is_a_local_directory_behind_a_url() {
        let (_temp, cwd) = project();
        let found = scan(
            &cwd,
            &v(&[
                "build",
                "--build-context",
                "base=oci-layout://./layout:v1@sha256:abc",
                ".",
            ]),
            1,
        )
        .unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, cwd.join("layout"));
        assert_eq!(found[0].shape, Shape::Dir);
        // The span stops at the directory, so the tag and digest that
        // name the image inside it survive a rewrite.
        let mut args = v(&[
            "build",
            "--build-context",
            "base=oci-layout://./layout:v1@sha256:abc",
            ".",
        ]);
        rewrite(&mut args, &found, |_| "/srv/layout".into());
        assert_eq!(args[2], "base=oci-layout:///srv/layout:v1@sha256:abc");
    }

    #[test]
    fn a_cache_that_is_not_on_this_machine_names_no_path() {
        let (_temp, cwd) = project();
        for value in [
            "type=registry,ref=user/app:cache",
            "type=gha,scope=build",
            "type=s3,bucket=b,region=r",
            "type=inline",
            // The shorthand: a bare value is a registry reference.
            "user/app:cache",
        ] {
            assert!(shape_of(&cwd, &["build", "--cache-from", value, "."]).is_empty());
            assert!(shape_of(&cwd, &["build", "--cache-to", value, "."]).is_empty());
        }
    }

    #[test]
    fn a_missing_local_cache_is_a_cache_miss_and_not_an_error() {
        // Measured: Docker builds happily with a `type=local` cache
        // source that is not there. Refusing it would break a build that
        // works without Ulak.
        let (_temp, cwd) = project();
        let found = scan(
            &cwd,
            &v(&["build", "--cache-from", "type=local,src=./cold", "."]),
            1,
        )
        .unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, cwd.join("cold"));
    }

    #[test]
    fn a_read_that_is_not_here_is_reported_and_a_write_that_is_not_is_normal() {
        let (_temp, cwd) = project();
        let err = scan(
            &cwd,
            &v(&["build", "--secret", "id=npm,src=./gone.npmrc", "."]),
            1,
        )
        .unwrap_err();
        assert!(err.to_string().contains("gone.npmrc"), "{err}");

        // Nothing writes these before the build does.
        let found = scan(
            &cwd,
            &v(&["build", "--iidfile", "./out/iid.txt", "-o", "./dist", "."]),
            1,
        )
        .unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].path, cwd.join("out/iid.txt"));
    }

    #[test]
    fn a_tilde_is_left_alone_because_docker_leaves_it_alone() {
        // Measured: `--secret id=t,src=~/.npmrc` makes the client stat a
        // directory literally called `~`. Expanding it here would let an
        // Ulak build succeed where the same command fails locally.
        let (_temp, cwd) = project();
        let err = scan(
            &cwd,
            &v(&["build", "--secret", "id=npm,src=~/.npmrc", "."]),
            1,
        )
        .unwrap_err();

        assert!(err.to_string().contains('~'), "{err}");
    }

    #[test]
    fn an_output_with_no_type_is_a_directory_however_it_is_named() {
        // Measured, because the name says otherwise: `-o ./out.tar`
        // writes a DIRECTORY called out.tar.
        let (_temp, cwd) = project();
        for value in ["./dist", "./out.tar"] {
            let found = scan(&cwd, &v(&["build", "-o", value, "."]), 1).unwrap();
            assert_eq!(found[0].shape, Shape::Dir, "{value}");
        }
        // An explicit type is the only thing that chooses an archive —
        // and `tar` turns it back off. Measured on Buildx 0.33.0:
        // `type=oci,dest=./out-tree,tar=false` leaves a directory of
        // `blobs/`, `index.json` and `oci-layout`, and `tar=0` does the
        // same. Read as a file, `make_room` planted a zero-byte
        // placeholder there and the remote build stopped on buildx's own
        // "destination directory ./out-tree is a file".
        for (value, shape) in [
            ("type=local,dest=./dist", Shape::Dir),
            ("type=tar,dest=./out.tar", Shape::File),
            ("type=oci,dest=./out.tar", Shape::File),
            ("type=docker,dest=./out.tar", Shape::File),
            ("type=oci,dest=./out-tree,tar=false", Shape::Dir),
            ("type=oci,dest=./out-tree,tar=0", Shape::Dir),
            ("type=docker,dest=./out-tree,tar=false", Shape::Dir),
            ("type=oci,dest=./out.tar,tar=true", Shape::File),
        ] {
            let found = scan(&cwd, &v(&["build", "--output", value, "."]), 1).unwrap();
            assert_eq!(found[0].shape, shape, "{value}");
        }
    }

    #[test]
    fn an_output_that_is_already_a_stream_writes_no_local_file() {
        let (_temp, cwd) = project();
        for value in ["-", "type=tar,dest=-", "type=registry", "type=cacheonly"] {
            assert!(
                scan(&cwd, &v(&["build", "-o", value, "."]), 1)
                    .unwrap()
                    .is_empty(),
                "{value}"
            );
        }
    }

    #[test]
    fn a_policy_value_can_pin_more_than_one_file() {
        let (_temp, cwd) = project();
        std::fs::write(cwd.join("extra.json"), "{}\n").unwrap();
        let found = scan(
            &cwd,
            &v(&[
                "build",
                "--policy",
                "filename=./policy.json,filename=./extra.json,strict=true",
                ".",
            ]),
            1,
        )
        .unwrap();

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].path, cwd.join("policy.json"));
        assert_eq!(found[1].path, cwd.join("extra.json"));
    }

    #[test]
    fn a_policy_file_is_the_contexts_to_find_and_not_this_modules_to_refuse() {
        // Measured on Buildx 0.33.0: `docker build --policy
        // filename=pol.rego ./ctx` loads `ctx/pol.rego` and never looks
        // at `./pol.rego`. Nothing here knows where the context is, so
        // reporting this path missing would refuse a build the client
        // runs — the one mistake this module must not make.
        let (_temp, cwd) = project();
        let found = scan(
            &cwd,
            &v(&["build", "--policy", "filename=pol.rego", "./ctx"]),
            1,
        )
        .unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, cwd.join("pol.rego"));
        // A read this module CAN speak for is still refused, so the line
        // above is a statement about `--policy` and not a loosening.
        assert!(
            scan(
                &cwd,
                &v(&["build", "--secret", "id=npm,src=./pol.rego", "./ctx"]),
                1
            )
            .is_err()
        );
    }

    #[test]
    fn a_flags_value_is_never_read_as_a_flag() {
        let (_temp, cwd) = project();
        // `--label --secret` sets a label CALLED `--secret`. Reading it
        // as a flag would eat `.` as its value and look for a file the
        // build never named.
        assert!(shape_of(&cwd, &["build", "--label", "--secret", "."]).is_empty());
        // `--provenance` takes a value too, however much it reads like a
        // switch — measured, it swallows the next word.
        assert!(shape_of(&cwd, &["build", "--provenance", "--ssh", "."]).is_empty());
        // A value that merely looks like a path is still just a value.
        assert!(shape_of(&cwd, &["build", "--build-arg", "SRC=./.npmrc", "."]).is_empty());
        // After `--`, Docker reads no more flags and neither do we.
        assert!(shape_of(&cwd, &["build", "--", "--secret", "id=n,src=./.npmrc"]).is_empty());
    }

    #[test]
    fn nothing_before_the_commands_own_arguments_is_scanned() {
        let (_temp, cwd) = project();
        // `docker image build …` puts the tail at 2.
        let args = v(&["image", "build", "--secret", "id=npm,src=./.npmrc", "."]);
        assert_eq!(scan(&cwd, &args, 2).unwrap().len(), 1);
        // Read from further in, the same `--secret` is behind the start
        // and must be invisible rather than half-parsed.
        assert!(scan(&cwd, &args, 4).unwrap().is_empty());
    }

    #[test]
    fn dockers_own_help_names_nothing_on_this_machine() {
        let (_temp, cwd) = project();
        for help in ["--help", "-h"] {
            assert!(
                scan(
                    &cwd,
                    &v(&["build", "--secret", "id=npm,src=./gone", help]),
                    1
                )
                .unwrap()
                .is_empty(),
                "{help}"
            );
        }
    }

    #[test]
    fn a_context_typed_before_its_flags_is_still_the_context() {
        // Measured: `docker buildx build ./ctx -o ./out` builds ./ctx.
        // Reading the LAST word as the context finds `api` here, and a
        // repo with a directory by that name then syncs and builds the
        // wrong tree without saying anything.
        assert_eq!(positionals(&v(&["build", ".", "-t", "api"]), 1), vec![1]);
        assert_eq!(positionals(&v(&["build", "-t", "api", "."]), 1), vec![3]);
        // Both of them, so the caller can say "one context, not two".
        assert_eq!(
            positionals(&v(&["build", ".", "-t", "api", "elsewhere"]), 1),
            vec![1, 4]
        );
    }

    #[test]
    fn a_flags_value_is_never_counted_as_a_positional() {
        // `--provenance` reads like a switch and is not: measured, it
        // swallows the next word and Docker then reports no context at
        // all. Counting `./ctx` here would sync a context this build
        // never had.
        assert!(positionals(&v(&["build", "--provenance", "./ctx"]), 1).is_empty());
        // Every spelling a value arrives in, beside booleans that take
        // none — in each of these the context is the only positional.
        for spelling in [
            vec!["build", "-f", "Dockerfile", "."],
            vec!["build", "--file", "Dockerfile", "."],
            vec!["build", "--file=Dockerfile", "-q", "."],
            vec!["build", "-fDockerfile", "--no-cache", "."],
            vec!["build", "-f=Dockerfile", "."],
            vec!["build", "-qo", "./dist", "."],
            vec!["build", "--secret", "id=npm,src=./.npmrc", "."],
        ] {
            let args = v(&spelling);
            let words: Vec<&str> = positionals(&args, 1)
                .iter()
                .map(|i| args[*i].as_str())
                .collect();
            assert_eq!(words, vec!["."], "{spelling:?}");
        }
    }

    #[test]
    fn a_word_after_the_double_dash_is_a_positional_however_it_looks() {
        assert_eq!(positionals(&v(&["build", "--", "./ctx"]), 1), vec![2]);
        assert_eq!(
            positionals(
                &v(&["build", "-o", "./dist", "--", "./ctx", "-o", "./out"]),
                1
            ),
            vec![4, 5, 6]
        );
    }

    #[test]
    fn a_stdin_context_is_a_positional_like_any_other() {
        // `docker build -` reads its context as a tarball on stdin.
        assert_eq!(positionals(&v(&["build", "-"]), 1), vec![1]);
        // And nothing before the tail is counted here either: `docker
        // image build …` starts at 2, so `build` is not a context.
        assert_eq!(
            positionals(&v(&["image", "build", "-t", "api", "."]), 2),
            vec![4]
        );
    }

    #[test]
    fn a_flag_that_arrives_without_its_value_is_reported() {
        let (_temp, cwd) = project();
        let err = scan(&cwd, &v(&["build", "--secret"]), 1).unwrap_err();
        assert!(err.to_string().contains("--secret"), "{err}");
    }

    #[test]
    fn the_flag_table_agrees_with_the_client_about_the_fifteen_it_does_not_print() {
        // `docker buildx build --help` prints 36 flags; the client
        // parses 51. Each of the other fifteen was settled by running
        // `docker buildx build <flag>` and reading whether pflag asked
        // for an argument. Ten of them take one.
        for flag in [
            "--cpu-period",
            "--cpu-quota",
            "--cpu-shares",
            "--cpuset-cpus",
            "--cpuset-mems",
            "--isolation",
            "--memory",
            "--memory-swap",
            "--print",
            "--security-opt",
        ] {
            assert!(VALUE_FLAGS.contains(&flag), "{flag} takes a value");
        }
        // The remaining five are booleans, and putting a boolean in the
        // table is the mirror-image bug: it would swallow the context.
        for flag in ["--compress", "--force-rm", "--rm", "--squash", "--help"] {
            assert!(!VALUE_FLAGS.contains(&flag), "{flag} takes none");
        }
        // Both short tables come from the same sweep, run over every
        // letter a–z and A–Z rather than read off the long names.
        assert_eq!(VALUE_SHORTS, ['c', 'f', 'm', 'o', 't']);
        assert_eq!(BOOL_SHORTS, ['D', 'q', 'h']);
        for ch in VALUE_SHORTS {
            assert!(!BOOL_SHORTS.contains(ch), "-{ch} is on both sides");
        }
        // A carrier's short spelling is unreachable unless the bundle
        // reader knows it takes a value, so the two tables have to
        // agree — `-o ./dist` silently stopped being seen otherwise.
        for carrier in CARRIERS {
            assert!(VALUE_FLAGS.contains(&carrier.long), "{}", carrier.long);
            if let Some(ch) = carrier.short {
                assert!(VALUE_SHORTS.contains(&ch), "-{ch}");
            }
        }
    }

    #[test]
    fn a_compatibility_no_op_eats_its_value_like_any_other_flag() {
        // Buildx keeps the classic builder's resource flags and accepts
        // them as no-ops — measured, a build carrying all of them at
        // once succeeds, warning only about `--isolation` and
        // `--security-opt`. A no-op still consumes a word, and while
        // these were missing Ulak read that word as a second context
        // and refused builds the client runs.
        //
        // One long spelling and one short stand for the whole set, and
        // the other ten are not listed here: each of them reaches the
        // same `Word::Eats` and the only thing that varies between them
        // is membership in `VALUE_FLAGS`, which
        // `the_flag_table_agrees_with_the_client_about_the_fifteen_it_does_not_print`
        // asserts by name for all ten. Drop one from the table and that
        // test says which one.
        let (_temp, cwd) = project();
        for spelling in [
            vec!["build", "-m", "512m", "."],
            vec!["build", "--memory", "512m", "."],
        ] {
            let args = v(&spelling);
            let words: Vec<&str> = positionals(&args, 1)
                .iter()
                .map(|i| args[*i].as_str())
                .collect();
            assert_eq!(words, vec!["."], "{spelling:?}");
            assert!(scan(&cwd, &args, 1).unwrap().is_empty(), "{spelling:?}");
        }
        // The other half of the same mistake: a value that is not read
        // as one leaves the next word free to be read as a flag. Here
        // that would carry `.` across as though the build had asked for
        // a secret file called `.`. Only the SHORT spelling is asserted,
        // because it is the one that reaches the bundle reader; the long
        // one is `--label --secret .` in
        // `a_flags_value_is_never_read_as_a_flag`, which is the same
        // claim about the same branch.
        assert!(shape_of(&cwd, &["build", "-m", "--secret", "."]).is_empty());
        // Every spelling pflag accepts for a value-taking shorthand,
        // measured against the client: `-m512m`, `-m=512m` and `-qm512m`
        // all build, so none of them may leak a word either.
        for spelling in [
            vec!["build", "-m512m", "."],
            vec!["build", "-m=512m", "."],
            vec!["build", "-qm512m", "."],
            vec!["build", "-c512", "."],
        ] {
            let args = v(&spelling);
            let words: Vec<&str> = positionals(&args, 1)
                .iter()
                .map(|i| args[*i].as_str())
                .collect();
            assert_eq!(words, vec!["."], "{spelling:?}");
        }
    }

    /// Every spelling pflag gives `-f`, and the words that only look like
    /// one. Measured against Buildx 0.33.0: each of the first group
    /// builds from `sub/Dockerfile`, and `-f A -f B` builds from B.
    #[test]
    fn the_dockerfile_flag_is_found_in_every_spelling_pflag_accepts() {
        for spelling in [
            vec!["build", "-f", "sub/Dockerfile", "."],
            vec!["build", "-fsub/Dockerfile", "."],
            vec!["build", "-f=sub/Dockerfile", "."],
            vec!["build", "--file", "sub/Dockerfile", "."],
            vec!["build", "--file=sub/Dockerfile", "."],
            vec!["build", "-qfsub/Dockerfile", "."],
            vec!["build", "-qf", "sub/Dockerfile", "."],
            vec!["build", "-f", "other", "-f", "sub/Dockerfile", "."],
        ] {
            let args = v(&spelling);
            let found = dockerfile(&args, 1).unwrap_or_else(|| panic!("{spelling:?} names one"));
            let (index, span) = found
                .at
                .unwrap_or_else(|| panic!("{spelling:?} has a value"));
            assert_eq!(&args[index][span], "sub/Dockerfile", "{spelling:?}");
        }

        for spelling in [
            // `-f` here is `--label`'s value; the build succeeds and its
            // Dockerfile is the default one.
            vec!["build", "--label", "-f", "."],
            // Past `--` nothing is a flag: Docker counts three arguments.
            vec!["build", "--", "-f", "sub/Dockerfile", "."],
            // `-t` takes the rest of its own word, so the `f` is a tag.
            vec!["build", "-tfoo", "."],
            vec!["build", "--filename", "x", "."],
        ] {
            assert!(dockerfile(&v(&spelling), 1).is_none(), "{spelling:?}");
        }

        // A flag with nothing after it is reported rather than dropped,
        // so the caller can say which spelling was left dangling.
        let bare = dockerfile(&v(&["build", ".", "-f"]), 1).unwrap();
        assert_eq!(bare.flag, "-f");
        assert!(bare.at.is_none());
    }

    /// A help request is a flag, not a word that appears somewhere.
    ///
    /// `docker build --build-arg --help .` builds (measured), and reading
    /// it as help made `scan` return nothing — so a `--secret` beside it
    /// went to the far side unread and the SERVER's file was opened.
    #[test]
    fn only_a_real_help_flag_stops_the_scan() {
        let (_temp, cwd) = project();
        for asking in [
            vec!["build", "--help", "."],
            vec!["build", "--help=true", "."],
            vec!["build", "-h", "."],
            vec!["build", "-qh", "."],
        ] {
            assert!(help_wanted(&v(&asking), 1), "{asking:?}");
        }
        for building in [
            vec!["build", "--build-arg", "--help", "."],
            vec!["build", "--label", "-h", "."],
            vec!["build", "--help=false", "."],
            vec!["build", "--", "--help"],
            // `-f` eats the `h`, so this names a Dockerfile called `h`.
            vec!["build", "-fh", "."],
        ] {
            assert!(!help_wanted(&v(&building), 1), "{building:?}");
        }

        // And the scan that gates on it still finds what is beside it.
        let args = v(&[
            "build",
            "--build-arg",
            "--help",
            "--secret",
            "id=npm,src=./.npmrc",
            ".",
        ]);
        assert_eq!(
            scan(&cwd, &args, 1).unwrap().len(),
            1,
            "the secret beside a help-shaped value is still a local path"
        );
    }
}
