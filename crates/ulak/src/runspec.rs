//! `docker run` and `docker create`: the commands whose flags quietly
//! name local directories.
//!
//! `docker run -v .:/app node npm test` is the single most common thing
//! anybody types, and it is also the one that cannot be forwarded. On
//! the server `.` is whatever directory the ssh session landed in, so
//! the container either mounts the wrong tree or an empty new one that
//! Docker helpfully creates. Nothing errors. The test just runs against
//! no source code.
//!
//! So `run`/`create` are handled the way Compose already is: the bind
//! sources, env files and label files are collected into a footprint,
//! the footprint is synced, and argv is rewritten to the paths those
//! files ended up at on the server.
//!
//! What the rewrite changes is narrower than it sounds. The remote
//! workspace reproduces the local layout under the anchor, and the run
//! happens in the mirror of the local cwd, so a source spelled
//! relatively comes back out spelled exactly the same way — `./app` is
//! still `./app`. What moves is the ground under it. Only an absolute
//! source is respelled, and only into that same relative form.
//!
//! Which sources travel, and which are left exactly as typed:
//!
//! ```text
//!   ./app   ../app   .        this machine: synced, and respelled
//!   /abs inside the root      the same thing, typed the long way
//!   /abs outside the root     the server's filesystem; said out loud
//!   ~/data                    the REMOTE home — compose reads it there too
//!   ~root/x   ~               server-side too, but the far shell will
//!                             not expand them (see `ssh.rs::sh_quote`)
//!   mydata:/x    -v /data     a named or anonymous volume: daemon state
//! ```
//!
//! The one asymmetry: `--cidfile` is an OUTPUT. Docker writes it on the
//! server, so it is cleared there before the run and fetched back after.

use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::catalog::Resolved;
use crate::config::{Project, Workspace};
use crate::docker::{Remote, remote_workdir, status_code, tty_for};
use crate::footprint::{Entry, Footprint, Why};
use crate::invocation::common_ancestor;
use crate::lockfile::WorkspaceLock;
use crate::ssh::{Ssh, sh_quote};
use crate::sync;
use crate::ui::{self, fail};

/// The `docker run` flags that consume the NEXT argument.
///
/// Read off `docker run --help` (29.4.0), then completed by asking the
/// client itself — because the help does not print everything it
/// accepts. The help prints 98 long flags; the client takes 107, and
/// the other nine are hidden or deprecated rather than gone: `--net`,
/// `--net-alias`, `--dns-opt`, `--cpu-count`, `--cpu-percent`,
/// `--io-maxbandwidth`, `--io-maxiops` and `--kernel-memory` all take a
/// value, `--disable-content-trust` does not. `docker run <flag>` with
/// nothing after it settles each one: pflag answers "flag needs an
/// argument" before any daemon contact, and "unknown flag" for a name
/// that does not exist.
///
/// The nine were found by probing every flag-shaped string in the
/// client binary, not by reading the help twice — the first pass came
/// off the help, missed `--kernel-memory`, and so turned a
/// `docker run --kernel-memory 64m …` the client happily accepts into
/// one ulak REFUSED. 92 value-taking plus 15 boolean is the
/// measurement, and the test below pins both counts so the next edit
/// has to re-measure rather than re-describe.
///
/// This table is the only thing standing between argv and the IMAGE
/// positional, which is the first argument that is neither a flag nor
/// the value of one. Everything after the image belongs to the
/// container, where `/app` is a path inside it and rewriting it would
/// be vandalism. Get the boundary wrong in the other direction and the
/// scan stops before the mount that mattered — silently, which is the
/// disease this module exists to cure.
///
/// `docker create` takes a subset of these (it has no `--detach`,
/// `--detach-keys` or `--sig-proxy`), so one table serves both.
const LONG_WITH_VALUE: &[&str] = &[
    "add-host",
    "annotation",
    "attach",
    "blkio-weight",
    "blkio-weight-device",
    "cap-add",
    "cap-drop",
    "cgroup-parent",
    "cgroupns",
    "cidfile",
    "cpu-count",
    "cpu-percent",
    "cpu-period",
    "cpu-quota",
    "cpu-rt-period",
    "cpu-rt-runtime",
    "cpu-shares",
    "cpus",
    "cpuset-cpus",
    "cpuset-mems",
    "detach-keys",
    "device",
    "device-cgroup-rule",
    "device-read-bps",
    "device-read-iops",
    "device-write-bps",
    "device-write-iops",
    "dns",
    "dns-opt",
    "dns-option",
    "dns-search",
    "domainname",
    "entrypoint",
    "env",
    "env-file",
    "expose",
    "gpus",
    "group-add",
    "health-cmd",
    "health-interval",
    "health-retries",
    "health-start-interval",
    "health-start-period",
    "health-timeout",
    "hostname",
    "io-maxbandwidth",
    "io-maxiops",
    "ip",
    "ip6",
    "ipc",
    "isolation",
    "kernel-memory",
    "label",
    "label-file",
    "link",
    "link-local-ip",
    "log-driver",
    "log-opt",
    "mac-address",
    "memory",
    "memory-reservation",
    "memory-swap",
    "memory-swappiness",
    "mount",
    "name",
    "net",
    "net-alias",
    "network",
    "network-alias",
    "oom-score-adj",
    "pid",
    "pids-limit",
    "platform",
    "publish",
    "pull",
    "restart",
    "runtime",
    "security-opt",
    "shm-size",
    "stop-signal",
    "stop-timeout",
    "storage-opt",
    "sysctl",
    "tmpfs",
    "ulimit",
    "user",
    "userns",
    "uts",
    "volume",
    "volume-driver",
    "volumes-from",
    "workdir",
];

/// The rest of the same set: flags that take no argument, so the
/// argument after one of them can be the image.
/// `--disable-content-trust` is the one unprinted flag on this side —
/// it is deprecated and still accepted, and it is a boolean.
const LONG_WITHOUT_VALUE: &[&str] = &[
    "detach",
    "disable-content-trust",
    "help",
    "init",
    "interactive",
    "no-healthcheck",
    "oom-kill-disable",
    "privileged",
    "publish-all",
    "quiet",
    "read-only",
    "rm",
    "sig-proxy",
    "tty",
    "use-api-socket",
];

/// Short spellings of the same split: `-a -c -e -h -l -m -p -u -v -w`
/// take a value, `-d -i -P -q -t` do not.
///
/// Note `-h` is `--hostname` here, not help. A value-taking letter can
/// only ever be the LAST of a bundle — docker hands it the remainder of
/// the bundle (`-itv./src:/app`), or the next argument if there is no
/// remainder.
const SHORT_WITH_VALUE: &str = "acehlmpuvw";
const SHORT_WITHOUT_VALUE: &str = "diPqt";

/// Carry one `run`/`create`.
///
/// `resolved.argv` is the full remote argv (command path included) and
/// `resolved.tail_start` is where the command's own arguments begin —
/// 1 for `docker run`, 2 for `docker container run`.
pub fn run(resolved: &Resolved) -> Result<ExitCode> {
    let mut args = resolved.argv.clone();
    let workspace = Workspace::locate()?;
    let remote = Remote::to(&workspace.ssh_dest()?)?;
    let ssh = remote.ssh.clone();
    let command = resolved.entry.name();
    // Only `run` starts the container; `create` writes it down and stops,
    // so nothing has run and nothing can have been written back.
    let runs_container = resolved.entry.path.last() == Some(&"run");

    let spec = RunSpec::parse(&workspace.root, &workspace.cwd, &args, resolved.tail_start)?;
    for source in &spec.server_refs {
        ui::warn(&format!(
            "{source} is resolved on the server, not here — ulak carries only what is inside {}",
            workspace.root.display()
        ));
    }

    // Nothing local was named. This is `docker run --rm alpine ls`, and
    // it should cost exactly one ssh.
    if spec.is_empty() {
        return status_code(remote.docker_with_stdin(
            &args,
            None,
            tty_for(resolved),
            resolved.entry.secret_flags,
            stdin_for(&spec, runs_container),
        )?);
    }

    let anchor =
        crate::invocation::workspace_anchor(workspace.workspace_id(), workspace.root.clone());
    let footprint = spec.footprint(&anchor, &format!("docker {command}"));
    spec.rewrite(&workspace.cwd, &mut args);
    let project = workspace.into_project(anchor);
    let workdir = remote_workdir(&project)?;

    // Only what is actually here can be pushed. A bind source that is
    // not there yet is docker's business on the far side, exactly as it
    // is locally — it creates the directory itself.
    let needs_sync = footprint.entries.iter().any(|e| e.exists);
    // A `--cidfile` run writes on the server and reads back from there,
    // so it touches the workspace tree whether or not anything travels.
    // Gating the whole block on `needs_sync` let `docker run --cidfile
    // x.cid alpine true` reach `clear_remote_cidfile`'s `mkdir -p` and
    // `rm -f` with no lock taken and, worse, no manifest read: the one
    // check that says this directory is ours lives in `ensure_workspace`,
    // which only `run_sync` was calling. A workspace belonging to
    // another checkout — or a `protect`ed file the server owns — was a
    // `rm -f` away from a run that had nothing to sync.
    let touches_workspace = needs_sync || spec.cidfile.is_some();
    let mut lock = None;
    if touches_workspace {
        let guard = WorkspaceLock::acquire(project.workspace_id())?;
        if needs_sync {
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
        } else {
            // Nothing to push, but the tree still has to exist and still
            // has to be ours before the next line deletes inside it.
            sync::claim_workspace(&ssh, &project)?;
        }
        lock = Some(guard);
    }

    if let Some(cid) = &spec.cidfile {
        clear_remote_cidfile(&project, &ssh, &cid.local)?;
    }

    // `create` and a detached run both come back in about a second, so
    // they keep the lock across the call and no sync can move the tree
    // while the container is being built out of it. A foreground run
    // holds this terminal for as long as the container lives, and
    // passthrough's TREE_READERS makes the same cut for the same
    // reason: nothing on this machine may be blocked from syncing for
    // hours because someone left a shell open.
    if runs_container && !spec.detach {
        lock = None;
    }

    let status = remote.docker_with_stdin(
        &args,
        Some(&workdir),
        tty_for(resolved),
        resolved.entry.secret_flags,
        stdin_for(&spec, runs_container),
    )?;

    // What the container wrote into a bind mount comes home, and a pull
    // problem must never replace docker's own exit code.
    //
    // Gated on the push having had something to push, because that is
    // the same condition: `filter_rules` gives a path that is not here
    // no rule at all, in EITHER direction, so with nothing pushed there
    // is provably nothing a pull could match. What that leaves behind is
    // `missing_sources_stayed_there` below, not a rule this can relax.
    if runs_container
        && needs_sync
        && let Err(e) = pull_home(&project, &footprint, &ssh, lock.take())
    {
        ui::render_error(&e);
    }
    if runs_container {
        missing_sources_stayed_there(&footprint, &project.inv.cwd);
    }
    // Docker writes the id file when it CREATES the container, not when
    // the container succeeds — `run --cidfile=id.txt alpine false` is
    // meant to leave you the id of the thing that exited 1, which is
    // precisely the container worth inspecting. So this is not gated on
    // the exit code; `fetch_cidfile` stays quiet when there is no file,
    // which is the case where docker never got as far as a container.
    if let Some(cid) = &spec.cidfile
        && let Err(e) = fetch_cidfile(&project, &ssh, &cid.local)
    {
        ui::render_error(&e);
    }

    status_code(status)
}

/// Where in argv one flag's value sits.
///
/// Three spellings mean one value: `--volume X`, `--volume=X` and the
/// bundled `-itv X` / `-itv./x:/y`. Recording the position rather than
/// the string is what lets the rewrite put a path back without having
/// to re-serialise everything around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// The value is its own argument.
    Separate(usize),
    /// The value is the tail of one argument, from `offset` on.
    Inline { index: usize, offset: usize },
}

impl Slot {
    fn get(self, args: &[String]) -> &str {
        match self {
            Slot::Separate(i) => &args[i],
            Slot::Inline { index, offset } => &args[index][offset..],
        }
    }

    fn set(self, args: &mut [String], value: &str) {
        match self {
            Slot::Separate(i) => args[i] = value.to_string(),
            Slot::Inline { index, offset } => {
                args[index] = format!("{}{value}", &args[index][..offset]);
            }
        }
    }
}

/// One argument that names a path on this machine.
#[derive(Debug)]
struct Input {
    slot: Slot,
    /// What surrounds the path inside the value (`type=bind,source=`
    /// and `,target=/app`, or the `:/app:ro` of a `-v`), so respelling
    /// is a substitution and never a re-serialisation of the rest.
    prefix: String,
    suffix: String,
    /// The path sits inside a CSV-quoted `--mount` field, so the
    /// respelling goes back in quoted form: a `"` in it doubles.
    csv_quoted: bool,
    local: PathBuf,
    why: Why,
    writable: bool,
}

/// The container id file: an output, so it travels the other way.
#[derive(Debug)]
struct CidFile {
    slot: Slot,
    local: PathBuf,
}

/// Every local path one `run`/`create` names, and nothing else about it.
#[derive(Debug, Default)]
struct RunSpec {
    inputs: Vec<Input>,
    cidfile: Option<CidFile>,
    /// Sources left for the server to resolve, kept only so the user is
    /// told which ones those were.
    server_refs: Vec<String>,
    detach: bool,
    /// Whether the client will stream this terminal's stdin into the
    /// container: `-i`, `--interactive`, or an attached `stdin`.
    interactive: bool,
}

/// What a source names, before anything has been decided about it.
enum Source {
    /// This machine.
    Local(PathBuf),
    /// The server's own filesystem: an absolute path this workspace
    /// does not contain, or a tilde.
    ///
    /// A LEADING `~/` is the one that carries: `sh_quote` passes that
    /// shape through unquoted on purpose, so the far shell expands it
    /// against the REMOTE home — which is where compose resolves a
    /// tilde volume too. Any other tilde spelling (`~`, `~root/x`) is
    /// quoted and reaches docker literally; it is still not this
    /// machine's to sync, so it is still classified here, and docker
    /// says what it thinks of the result.
    Server,
    /// A named or anonymous volume — daemon state with no local side.
    Daemon,
}

impl RunSpec {
    fn parse(root: &Path, cwd: &Path, args: &[String], tail_start: usize) -> Result<RunSpec> {
        let mut spec = RunSpec::default();
        let mut i = tail_start;
        while i < args.len() {
            let arg = args[i].as_str();
            // pflag's terminator: whatever follows is the image and the
            // container's own command line.
            if arg == "--" {
                break;
            }
            if let Some(long) = arg.strip_prefix("--") {
                // Docker prints its help and exits; nothing is read.
                if long == "help" {
                    return Ok(RunSpec::default());
                }
                if let Some((name, _)) = long.split_once('=') {
                    // An inline value cannot swallow the next argument,
                    // so a flag missing from the table is harmless here
                    // — which is also the way out of `unknown_flag`.
                    let offset = "--".len() + name.len() + "=".len();
                    spec.claim(name, Slot::Inline { index: i, offset }, root, cwd, args)?;
                    i += 1;
                    continue;
                }
                if LONG_WITHOUT_VALUE.contains(&long) {
                    spec.detach |= long == "detach";
                    spec.interactive |= long == "interactive";
                    i += 1;
                    continue;
                }
                if !LONG_WITH_VALUE.contains(&long) {
                    return Err(unknown_flag(arg));
                }
                // Docker's own "flag needs an argument" says this better
                // than we could, so let it be the one to say it.
                if i + 1 == args.len() {
                    break;
                }
                spec.claim(long, Slot::Separate(i + 1), root, cwd, args)?;
                i += 2;
                continue;
            }
            if arg.len() > 1
                && let Some(bundle) = arg.strip_prefix('-')
            {
                i += spec.claim_bundle(arg, bundle, i, root, cwd, args)?;
                continue;
            }
            // The image positional. Everything past it is the
            // container's command, where a path means a path inside the
            // container.
            break;
        }
        Ok(spec)
    }

    /// One `-abc` bundle. Returns how many arguments it consumed.
    fn claim_bundle(
        &mut self,
        arg: &str,
        bundle: &str,
        index: usize,
        root: &Path,
        cwd: &Path,
        args: &[String],
    ) -> Result<usize> {
        for (pos, c) in bundle.char_indices() {
            if SHORT_WITH_VALUE.contains(c) {
                let rest = &bundle[pos + c.len_utf8()..];
                let (slot, consumed) = if rest.is_empty() {
                    if index + 1 == args.len() {
                        return Ok(1);
                    }
                    (Slot::Separate(index + 1), 2)
                } else {
                    // Docker drops one `=` between a bundled flag and
                    // the value stuck to it.
                    let eaten = usize::from(rest.starts_with('='));
                    let offset = "-".len() + pos + c.len_utf8() + eaten;
                    (Slot::Inline { index, offset }, 1)
                };
                // `-v` is the only short flag that names a local path,
                // and `-a` the only other one whose value is read at all.
                match c {
                    'v' => self.claim("volume", slot, root, cwd, args)?,
                    'a' => self.claim("attach", slot, root, cwd, args)?,
                    _ => {}
                }
                return Ok(consumed);
            }
            if !SHORT_WITHOUT_VALUE.contains(c) {
                return Err(unknown_flag(arg));
            }
            // pflag reads an `=` straight after a shorthand as that
            // shorthand's VALUE, booleans included, and the value ends
            // the cluster. Measured on 29.4.0: `docker run --rm -d=false
            // alpine echo hello` prints hello in the FOREGROUND where
            // `-d=true` prints an id, and `-t=false` and `-i=1` are
            // accepted too. Walking on to the `=` instead looked it up
            // as a shorthand of its own, found it in neither table, and
            // refused a command the client runs — after having already
            // recorded the run as detached, which is the opposite of
            // what was typed.
            let rest = &bundle[pos + c.len_utf8()..];
            if let Some(value) = rest.strip_prefix('=') {
                self.detach |= c == 'd' && flag_bool(value);
                self.interactive |= c == 'i' && flag_bool(value);
                return Ok(1);
            }
            self.detach |= c == 'd';
            self.interactive |= c == 'i';
        }
        Ok(1)
    }

    /// Sort one flag's value into what it names.
    fn claim(
        &mut self,
        flag: &str,
        slot: Slot,
        root: &Path,
        cwd: &Path,
        args: &[String],
    ) -> Result<()> {
        let value = slot.get(args);
        match flag {
            "v" | "volume" => self.claim_volume(slot, value, root, cwd),
            "mount" => self.claim_mount(slot, value, root, cwd),
            // Both are read by the docker CLI itself, on the server,
            // before the container exists. There is no separate label
            // for a label file and no point inventing one: it is
            // carried at the same moment and for the same reason.
            "env-file" | "label-file" => self.claim_file(slot, value, root, cwd, flag),
            // The third file on this command line the CLI opens itself,
            // and the only one of the three that is a security control.
            "security-opt" => self.claim_security_opt(slot, value, root, cwd),
            "cidfile" => self.claim_cidfile(slot, value, root, cwd),
            // Not paths — the three that decide whether this terminal's
            // stdin is read at all, and so whether ssh may have it.
            //
            // `--attach` is here because this is the one place all its
            // spellings meet: `--attach stdin`, `--attach=stdin` and the
            // bundled `-astdin` are one flag read once, which is what
            // keeps the stdin decision from needing a parser of its own.
            // `--interactive` and `--detach` reach it only written with
            // an `=`, because that branch runs before the boolean table
            // is consulted — which is why `--detach=true` used to fall
            // through to `_` and be recorded as a FOREGROUND run.
            "attach" => {
                // Docker lowercases the stream name before it reads it,
                // so `-a STDIN` attaches stdin exactly as `-a stdin`
                // does — measured, both drain the pipe they are given.
                self.interactive |= value.eq_ignore_ascii_case("stdin");
                Ok(())
            }
            "interactive" => {
                self.interactive |= flag_bool(value);
                Ok(())
            }
            "detach" => {
                self.detach |= flag_bool(value);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// The path on this machine one value names, if it names one at all.
    ///
    /// `bare_is_path` settles the one spelling that reads two ways: a
    /// `-v` source with no leading slash or dot is a named volume, while
    /// the same word after `--env-file` is a file in the cwd.
    fn local_of(
        &mut self,
        root: &Path,
        cwd: &Path,
        src: &str,
        label: &str,
        bare_is_path: bool,
    ) -> Result<Option<PathBuf>> {
        // An empty value names nothing, and resolving it names the worst
        // thing available: `absolutize(cwd, "")` is the cwd itself. So
        // `--env-file "$MISSING_VAR"` recorded the whole project as an
        // input, set `whole_anchor` — syncing the entire workspace off a
        // typo — and rewrote argv to `--env-file=.`, while `--cidfile ""`
        // was refused for a file the user never named. Docker's own
        // answers are `open : no such file or directory` and, for an
        // empty cidfile, nothing at all; both are better than ours.
        if src.is_empty() {
            return Ok(None);
        }
        let local = match classify(root, cwd, src) {
            Source::Local(local) => local,
            Source::Server => {
                self.server_refs.push(src.to_string());
                return Ok(None);
            }
            Source::Daemon if bare_is_path => absolutize(cwd, src),
            Source::Daemon => return Ok(None),
        };
        require_inside(root, &local, src, label)?;
        Ok(Some(local))
    }

    fn claim_volume(&mut self, slot: Slot, value: &str, root: &Path, cwd: &Path) -> Result<()> {
        // One-argument form (`-v /data`): an anonymous volume, which is
        // a path inside the container and nothing here.
        let Some((src, rest)) = value.split_once(':') else {
            return Ok(());
        };
        let Some(local) = self.local_of(root, cwd, src, "bind mount source", false)? else {
            return Ok(());
        };
        let options = rest.split_once(':').map_or("", |(_, o)| o);
        // `ro` is matched exactly, and that is not an oversight of the
        // kind `claim_mount` had: docker folds a `--mount` KEY and folds
        // nothing here. Measured on 29.4.0, `-v ./app:/x:RO` is refused
        // outright — "invalid mode: RO" — so a spelling this misses is
        // one the far side never runs.
        self.inputs.push(Input {
            slot,
            prefix: String::new(),
            suffix: format!(":{rest}"),
            csv_quoted: false,
            local,
            why: Why::Volume,
            writable: !options.split(',').any(|o| o == "ro"),
        });
        Ok(())
    }

    /// `--mount type=bind,source=…`. Only `type=bind` has a local side;
    /// `type=volume`, `type=tmpfs` and `type=image` do not, and
    /// docker's default when `type=` is absent is `volume`.
    ///
    /// The value is a CSV record rather than a comma-separated string,
    /// which is what `csv_fields` is for and why it is worth reading.
    fn claim_mount(&mut self, slot: Slot, value: &str, root: &Path, cwd: &Path) -> Result<()> {
        let fields = csv_fields(value)?;
        let mut kind = "volume";
        let mut source = None;
        let mut writable = true;
        for (n, field) in fields.iter().enumerate() {
            // A field with no `=` is a bare boolean option, so it holds
            // no path — and slicing one for a value it does not have
            // would be a panic, not a misread.
            let Some((key, val)) = field.text.split_once('=') else {
                writable &= !is_readonly(&field.text);
                continue;
            };
            if key.eq_ignore_ascii_case("type") {
                kind = val;
            } else if key.eq_ignore_ascii_case("source") || key.eq_ignore_ascii_case("src") {
                // Where the key ends is where the path begins, and the
                // path is the only part of the field that moves.
                source = Some((n, key.len() + "=".len()));
            } else if is_readonly(key) {
                writable = !mount_bool(val);
            }
        }
        // Docker lowercases a `--mount` key AND the `type=` value before
        // it reads either, which this module alone used to ignore —
        // `buildflags.rs` and `catalog.rs` have folded case all along.
        // Measured on 29.4.0: `--mount TYPE=bind,SOURCE=/nonexistent-zz,
        // TARGET=/x` and `type=BIND,source=…` both reach the daemon as
        // bind mounts. Matched case-sensitively, `SOURCE=` set no source
        // and `type=BIND` set no bind, so both spellings fell out of the
        // spec here: not synced, not respelled, not warned about, and
        // the container read the SERVER's copy of the directory.
        if !kind.eq_ignore_ascii_case("bind") {
            return Ok(());
        }
        let Some((n, key_len)) = source else {
            return Ok(());
        };
        let field = &fields[n];
        let src = &field.text[key_len..];
        let Some(local) = self.local_of(root, cwd, src, "bind mount source", false)? else {
            return Ok(());
        };
        // Everything on either side of the path is carried across as the
        // bytes it already was, quotes and all, so respelling the source
        // cannot disturb a `volume-opt` that happens to hold a comma.
        // The key never contains a quote, so the path starts a fixed
        // distance in whether the field was quoted or not.
        let quote = usize::from(field.quoted);
        self.inputs.push(Input {
            slot,
            prefix: value[..field.start + quote + key_len].to_string(),
            suffix: value[field.end - quote..].to_string(),
            csv_quoted: field.quoted,
            local,
            why: Why::Volume,
            writable,
        });
        Ok(())
    }

    fn claim_file(
        &mut self,
        slot: Slot,
        value: &str,
        root: &Path,
        cwd: &Path,
        flag: &str,
    ) -> Result<()> {
        let Some(local) = self.local_of(root, cwd, value, flag, true)? else {
            return Ok(());
        };
        self.inputs.push(Input {
            slot,
            prefix: String::new(),
            suffix: String::new(),
            csv_quoted: false,
            local,
            why: Why::EnvFile,
            writable: false,
        });
        Ok(())
    }

    /// `--security-opt seccomp=<profile>`: a JSON file the docker CLI
    /// opens ITSELF, which under Ulak means the server's copy of it.
    ///
    /// Measured on 29.4.0 the way `catalog.rs` measured its client-path
    /// list, by naming a file that is not there: `--security-opt
    /// seccomp=./nope.json` answers `opening seccomp profile
    /// (./nope.json) failed`, and it answers that even when the IMAGE
    /// does not exist either — so the file loses the race to the daemon
    /// and the read is client-side. The CLI then sends the CONTENTS,
    /// which is `--env-file`'s class exactly, and `docker create` behaves
    /// the same.
    ///
    /// Only the `seccomp` key names a file. `apparmor=<name>`,
    /// `label=<opt>`, `no-new-privileges` and the literal
    /// `seccomp=unconfined` name nothing on any disk. The key is matched
    /// case-sensitively and the value split at the FIRST separator,
    /// because that is what was measured: `SECCOMP=./p.json` is refused
    /// by the daemon as an invalid option rather than opened, while
    /// `seccomp=UNCONFINED` — wrong case on the VALUE — is opened as a
    /// file. `seccomp:./p.json` is the same flag, and
    /// `seccomp=./a=b.json` splits once, leaving `./a=b.json`.
    ///
    /// Unclaimed, this named the ssh login directory's copy of the
    /// profile: a container started under whatever `p.json` happened to
    /// be on the server, or — far likelier — under no confinement the
    /// user could name, with the error pointing at a path that does not
    /// exist here.
    fn claim_security_opt(
        &mut self,
        slot: Slot,
        value: &str,
        root: &Path,
        cwd: &Path,
    ) -> Result<()> {
        let Some(sep) = value.find(['=', ':']) else {
            return Ok(());
        };
        if &value[..sep] != "seccomp" {
            return Ok(());
        }
        let profile = &value[sep + 1..];
        // Docker's own reserved word for "no profile at all", and the
        // only value of this key that is not a path.
        if profile == "unconfined" {
            return Ok(());
        }
        let Some(local) = self.local_of(root, cwd, profile, "seccomp profile", true)? else {
            return Ok(());
        };
        self.inputs.push(Input {
            slot,
            prefix: value[..=sep].to_string(),
            suffix: String::new(),
            csv_quoted: false,
            local,
            why: Why::EnvFile,
            writable: false,
        });
        Ok(())
    }

    fn claim_cidfile(&mut self, slot: Slot, value: &str, root: &Path, cwd: &Path) -> Result<()> {
        let Some(local) = self.local_of(root, cwd, value, "container id file", true)? else {
            return Ok(());
        };
        // Docker's own rule, applied where the user can act on it: it
        // refuses to overwrite a cidfile, and the copy it would find is
        // on the server, where the message would name a path that means
        // nothing from here.
        if local.exists() {
            return Err(
                fail!("the container id file {} is already there", local.display())
                    .now("docker refuses to overwrite one — remove it, or name another file")
                    .into_err(),
            );
        }
        // Docker opens the id file BEFORE it creates the container and
        // never makes the directory: measured on 29.4.0, `docker run
        // --cidfile nosuchdir/id.cid alpine:3.20 true` answers "failed
        // to create the container ID file: open nosuchdir/id.cid: no
        // such file or directory", exits 127, and leaves no container
        // behind.
        //
        // Here the file is written on the SERVER, where
        // `clear_remote_cidfile` makes the parent with `mkdir -p`. So
        // the missing directory stopped nothing: the container ran, and
        // only `fetch_cidfile`'s write home failed — after the fact,
        // reported to stderr while docker's own 0 went out as the exit
        // code. A run docker would have refused came back saying it had
        // worked, which is the one answer a script reads.
        if let Some(parent) = local.parent().filter(|p| !p.as_os_str().is_empty())
            && !parent.is_dir()
        {
            return Err(fail!(
                "the directory for the container id file is not here: {}",
                parent.display()
            )
            .now(format!("make it first: mkdir -p {}", parent.display()))
            .now("docker writes that file before it starts anything, and makes no directory for it")
            .into_err());
        }
        self.cidfile = Some(CidFile { slot, local });
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.inputs.is_empty() && self.cidfile.is_none()
    }

    fn footprint(&self, anchor: &Path, service: &str) -> Footprint {
        let mut fp = Footprint {
            anchor: anchor.to_path_buf(),
            entries: Vec::new(),
            server_refs: Vec::new(),
            whole_anchor: false,
            contexts: Vec::new(),
            pinned: Vec::new(),
            model_json: String::new(),
        };
        for input in &self.inputs {
            if let Some(seen) = fp
                .entries
                .iter_mut()
                .find(|e| e.local == input.local && e.why == input.why)
            {
                // One directory named twice is one entry, and it is
                // writable if ANY of its mounts is. Keeping the first
                // mount's answer described `-v ./app:/a:ro -v ./app:/b`
                // as a directory nothing writes into, which is the exact
                // shape doctor's writable warnings exist to find.
                seen.writable |= input.writable;
                continue;
            }
            let exists = input.local.exists();
            let is_dir = if exists {
                input.local.is_dir()
            } else {
                // A bind source docker has yet to create is created as a
                // directory — the same assumption compose's footprint
                // makes about a path the model names but nobody wrote.
                input.why == Why::Volume
            };
            fp.whole_anchor |= is_dir && input.local == fp.anchor;
            fp.entries.push(Entry {
                local: input.local.clone(),
                is_dir,
                exists,
                empty: exists
                    && is_dir
                    && std::fs::read_dir(&input.local).is_ok_and(|mut d| d.next().is_none()),
                why: input.why,
                service: service.to_string(),
                writable: input.writable,
            });
        }
        fp.entries.sort_by(|a, b| a.local.cmp(&b.local));
        fp
    }

    fn rewrite(&self, cwd: &Path, args: &mut [String]) {
        for input in &self.inputs {
            let mut path = workdir_relative(cwd, &input.local);
            if input.csv_quoted {
                path = path.replace('"', "\"\"");
            }
            input
                .slot
                .set(args, &format!("{}{path}{}", input.prefix, input.suffix));
        }
        if let Some(cid) = &self.cidfile {
            cid.slot.set(args, &workdir_relative(cwd, &cid.local));
        }
    }
}

/// One field of a `--mount` value: its text with the quoting taken off,
/// and where in the value it sat.
struct Field {
    /// Byte range in the original value, the quotes included, so the
    /// rewrite can put a path back without re-serialising the rest.
    start: usize,
    end: usize,
    text: String,
    quoted: bool,
}

/// Split a `--mount` value the way docker splits it: as one CSV RECORD.
///
/// A field that BEGINS with a quote runs to its matching one and may
/// hold commas; `""` inside it is one literal quote. Both are docker's
/// rules, not ours — `--mount '"source=/a,b",type=bind,target=/x'`
/// mounts a directory whose name contains a comma, measured on 29.4.0.
/// Splitting on every comma instead produced a field spelled
/// `"source=/a`, whose key is `"source` and therefore not a source at
/// all: the bind vanished from the spec, so it was never synced, never
/// respelled and never mentioned, and the container read the SERVER's
/// `/a,b`. That is this module's own failure mode arriving through a
/// quoting rule.
///
/// A value that is not a CSV record is refused rather than half-read.
/// Docker refuses each of these shapes as well ("bare \" in non-quoted
/// field", "extraneous or missing \" in quoted-field"), so the cost is
/// one clearer message earlier; the alternative is guessing where a
/// path ends, and a guess here is the wrong machine's filesystem.
fn csv_fields(value: &str) -> Result<Vec<Field>> {
    let bytes = value.as_bytes();
    let mut fields = Vec::new();
    let mut i = 0;
    loop {
        let start = i;
        if bytes.get(i) != Some(&b'"') {
            let end = bytes[i..]
                .iter()
                .position(|b| *b == b',')
                .map_or(bytes.len(), |p| i + p);
            let text = &value[i..end];
            if text.contains('"') {
                return Err(csv_error(
                    value,
                    "a field holds a quote it did not open with",
                ));
            }
            fields.push(Field {
                start,
                end,
                text: text.to_string(),
                quoted: false,
            });
            if end == bytes.len() {
                return Ok(fields);
            }
            i = end + 1;
            continue;
        }
        i += 1;
        let mut text = String::new();
        loop {
            let Some(rel) = bytes[i..].iter().position(|b| *b == b'"') else {
                return Err(csv_error(value, "a quoted field is never closed"));
            };
            text.push_str(&value[i..i + rel]);
            i += rel + 1;
            if bytes.get(i) != Some(&b'"') {
                break;
            }
            text.push('"');
            i += 1;
        }
        fields.push(Field {
            start,
            end: i,
            text,
            quoted: true,
        });
        match bytes.get(i) {
            Some(&b',') => i += 1,
            None => return Ok(fields),
            Some(_) => {
                return Err(csv_error(
                    value,
                    "a quoted field carries on after its closing quote",
                ));
            }
        }
    }
}

fn csv_error(value: &str, what: &str) -> anyhow::Error {
    fail!("--mount {value} is not something docker can read: {what}")
        .now("a field holding a comma is quoted WHOLE — \"source=/a,b\" — and a quote inside one is doubled")
        .now("ulak stops here rather than guess where the source ends, because a guess is the wrong machine's filesystem")
        .into_err()
}

/// Docker's reading of a VALUED mount option, which is not Go's
/// `ParseBool` and not "anything but false" either: `readonly=0` and
/// `readonly=false` both leave the mount writable, `readonly=1` and
/// `readonly=true` do not, and every other spelling is refused outright
/// (measured — the error names those four words). Treating `=0` as
/// read-only recorded a bind mount the container can write to as one
/// nothing can have been written into.
fn mount_bool(val: &str) -> bool {
    matches!(val, "true" | "1")
}

/// The two spellings of `--mount`'s read-only option, in whatever case
/// they were typed. Docker folds the KEY and not the value: measured,
/// bare `RO` and `ReadOnly=1` both produce a read-only mount, while
/// `readonly=TRUE` is refused outright ("must be one of \"true\", \"1\",
/// \"false\", or \"0\""), which is why `mount_bool` stays exact.
fn is_readonly(key: &str) -> bool {
    key.eq_ignore_ascii_case("readonly") || key.eq_ignore_ascii_case("ro")
}

/// pflag's reading of a BOOLEAN FLAG written `--interactive=…`, which is
/// Go's `strconv.ParseBool` and is not the same set of words `--mount`'s
/// `readonly=` takes. Measured on 29.4.0: `--interactive=1` opens stdin,
/// `--interactive=0` does not, and `--interactive=yes` is refused by the
/// client before anything runs ("strconv.ParseBool: parsing \"yes\"").
/// So a spelling this does not recognise is one the far side rejects
/// too, and reading it as false costs nothing.
pub(crate) fn flag_bool(val: &str) -> bool {
    matches!(val, "1" | "t" | "T" | "TRUE" | "true" | "True")
}

/// Where the remote `run`/`create` reads its stdin from.
///
/// The policy is `docker::stdin_for`'s, and the reason is written out
/// there: ssh drains its own stdin as soon as the channel is up, whether
/// or not anything on the far side is listening, so `ulak docker run
/// --rm alpine ls` in the middle of a shell pipeline eats what the next
/// reader was going to get — which plain docker never does.
fn stdin_for(spec: &RunSpec, runs_container: bool) -> std::process::Stdio {
    if wants_this_terminals_stdin(spec, runs_container, std::io::stdin().is_terminal()) {
        std::process::Stdio::inherit()
    } else {
        std::process::Stdio::null()
    }
}

/// Split out from `stdin_for` for the reason `docker.rs` splits its own:
/// stdin is never a terminal under `cargo test`, which would leave the
/// half of this decision that is not a leaf lookup unexercised.
///
/// A terminal has nothing queued to lose, so it is always passed on.
/// What `run` cannot answer by its name alone is the other half, and
/// that is the whole of this function. Measured on 29.4.0 by watching
/// what a pipeline has LEFT afterwards — `printf 'a\nb\n' | { docker
/// run …; cat; }`:
///
/// ```text
///   run -i    --interactive    -a stdin      the pipe is drained
///   run                                      both lines still there
///   run -i -d                                both lines still there
///   create -i        create -a stdin         both lines still there
/// ```
///
/// `create` is the plain one: it writes the container down and starts
/// nothing, so there is no process on the far side that could read a
/// byte — whatever the container's own stdin will be when someone later
/// starts it.
///
/// Detach is the one that reads backwards. `-i` opens the CONTAINER's
/// stdin, and `-d` means the client prints an id and leaves rather than
/// streaming this terminal into it, so `id=$(ulak docker run -itd …)`
/// mid-pipeline must not swallow the rest of the pipe. `asked_for_a_tty`
/// makes the same cut on the same pair for the same reason.
fn wants_this_terminals_stdin(
    spec: &RunSpec,
    runs_container: bool,
    stdin_is_a_terminal: bool,
) -> bool {
    stdin_is_a_terminal || (runs_container && spec.interactive && !spec.detach)
}

/// Which filesystem a source belongs to.
///
/// An absolute path is the one ambiguous spelling, and it is read the
/// way compose already reads it: inside the workspace it is this
/// machine, outside it is the server's own filesystem. That is what
/// keeps `-v /var/run/docker.sock:/var/run/docker.sock` meaning the
/// server's socket, which is the only thing it can usefully mean.
fn classify(root: &Path, cwd: &Path, src: &str) -> Source {
    // Any tilde, not just the `~/` that survives quoting: a spelling
    // this machine cannot resolve is not one it should be syncing,
    // whichever end ends up refusing it.
    if src.starts_with('~') {
        return Source::Server;
    }
    // Any leading dot, not just `./` and `../`. A volume name is
    // `[a-zA-Z0-9][a-zA-Z0-9_.-]*`, so it cannot begin with one — which
    // makes `.env`, `.aws` and `.git` host paths to Docker and leaves no
    // second reading to weigh. Measured: `-v .env:/x` mounts this
    // machine's file, while `-v _priv:/x` is refused as an invalid
    // volume name. Spelling only `./` here sent every dotfile down the
    // named-volume arm, where nothing is synced and nothing is said.
    if src.starts_with('.') {
        return Source::Local(absolutize(cwd, src));
    }
    if src.starts_with('/') {
        let local = absolutize(cwd, src);
        return if local.starts_with(root) {
            Source::Local(local)
        } else {
            Source::Server
        };
    }
    Source::Daemon
}

fn require_inside(root: &Path, local: &Path, raw: &str, label: &str) -> Result<()> {
    if local.starts_with(root) {
        return Ok(());
    }
    Err(fail!(
        "the {label} {raw} is a path on this machine outside the workspace {}",
        root.display()
    )
    .now("move it under the project — ulak carries the workspace, and only the workspace")
    .now("or, if you meant a directory that already exists on the server, name it absolutely or as ~/…")
    .into_err())
}

/// A path that is not there yet is still a path ulak has to place —
/// docker creates a missing bind source rather than failing — so
/// canonicalize when the filesystem can answer and fold `.`/`..` by
/// hand when it cannot. Left unfolded, `starts_with(root)` accepts
/// `<root>/../elsewhere`, and the workspace check would pass on a path
/// that escapes it.
fn absolutize(cwd: &Path, raw: &str) -> PathBuf {
    let raw = Path::new(raw);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    if let Ok(real) = joined.canonicalize() {
        return real;
    }
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// How the server should spell `target`, standing where the run will
/// stand: the mirror of the local cwd inside the remote workspace.
///
/// The leading `./` is not decoration. Docker reads a bare `app:/app`
/// as a NAMED VOLUME, so a source that came in as an absolute path
/// inside the project must not come out as `app` — that would trade a
/// wrong directory for a silent daemon volume, which is worse.
fn workdir_relative(cwd: &Path, target: &Path) -> String {
    let common = common_ancestor(&[cwd, target]);
    let mut rel = PathBuf::new();
    if let Ok(up) = cwd.strip_prefix(&common) {
        for _ in up.components() {
            rel.push("..");
        }
    }
    if let Ok(down) = target.strip_prefix(&common) {
        rel.push(down);
    }
    if rel.as_os_str().is_empty() {
        return ".".into();
    }
    let climbs = rel.components().next() == Some(Component::ParentDir);
    let rel = rel.to_string_lossy();
    if climbs {
        rel.into_owned()
    } else {
        format!("./{rel}")
    }
}

fn unknown_flag(flag: &str) -> anyhow::Error {
    fail!("ulak does not know whether {flag} takes a value, so it cannot find where the image starts")
        .now(format!("write it as one word — {flag}=value — and ulak does not need to know"))
        .now("then report it: the flag table is read off `docker run --help`, so a newer docker leaves it stale")
        .into_err()
}

/// Say what docker created on the server and is not coming back.
///
/// A bind source that was not here when the run started gets no filter
/// rule in either direction, and that is deliberate upstream: the rule
/// that carried `./out` home would carry a database home with it, and a
/// real supabase stack names `./volumes/db/data` without it existing
/// locally. Docker still creates the directory — on the SERVER — and
/// whatever the container wrote into it stays there.
///
/// The rule is kept; the silence is not. Locally the user would have
/// found that directory sitting in their project afterwards. Here they
/// find nothing at all, which is this module's own stated failure mode
/// arriving from the other side.
///
/// THE OBVIOUS FIX IS WORSE, and it is obvious enough to be worth
/// writing down so it is not rediscovered as an improvement. Creating
/// the directory HERE before the sync would be docker-faithful — docker
/// creates a missing bind source on the host — and everything else
/// would follow: the entry would exist, earn a rule, and pull back
/// normally. What follows with it is
/// `run -v ./pgdata:/var/lib/postgresql/data -d postgres` on a clean
/// checkout: the directory is created, synced empty, the database
/// initialises on the SERVER, the whole data directory is dragged onto
/// this machine by the first pull, and the ledger then claims those
/// files so the next sync pushes them back at a running database. That
/// is not a bug, it is data loss with a delay. Doing nothing is
/// harmless; saying nothing was the only fault, and that is what the
/// warning below fixes.
///
/// The second reason is the one that would outlive even that argument:
/// `footprint.rs` already answered this exact ambiguity, on measured
/// evidence, and two modules answering one question two ways is
/// precisely what this repo keeps warning about.
fn missing_sources_stayed_there(footprint: &Footprint, cwd: &Path) {
    let missing = sources_left_on_the_server(footprint, cwd);
    if missing.is_empty() {
        return;
    }
    ui::warn(&format!(
        "docker created {} on the SERVER — it was not here when the run started, so nothing written into it comes back",
        missing.join(", ")
    ));
    ui::dim("create it here and run again if the container writes into it");
}

/// The sources the warning above names, spelled the way the user typed
/// them. Split out for the reason `wants_this_terminals_stdin` is: the
/// warning itself returns nothing and only writes to the terminal, so a
/// test of it could do no more than rebuild this list by hand and check
/// its own arithmetic — which stays green if the rule below changes
/// underneath it.
fn sources_left_on_the_server(footprint: &Footprint, cwd: &Path) -> Vec<String> {
    footprint
        .entries
        .iter()
        .filter(|e| !e.exists && e.why == Why::Volume)
        .map(|e| workdir_relative(cwd, &e.local))
        .collect()
}

fn pull_home(
    project: &Project,
    footprint: &Footprint,
    ssh: &Ssh,
    held: Option<WorkspaceLock>,
) -> Result<()> {
    // A foreground run gave the lock up before the exec. Take it again
    // rather than pulling into a tree the service may be syncing.
    let _lock = match held {
        Some(lock) => lock,
        None => WorkspaceLock::acquire(project.workspace_id())?,
    };
    sync::pull_back(
        project,
        footprint,
        ssh,
        true,
        &crate::proc::Budget::new(crate::proc::RECONCILE),
        None,
    )?;
    Ok(())
}

fn remote_path(project: &Project, local: &Path) -> Result<String> {
    Ok(format!(
        "{}/{}",
        project.remote_dir(),
        project.remote_rel(local)?
    ))
}

/// Make room for the id file docker is about to write.
///
/// Two things are missing on the server that are present here: the
/// directory (an empty one never travels), and the absence of last
/// run's file — docker refuses to overwrite a cidfile, and the copy it
/// would trip over is one the user cannot see from this machine.
fn clear_remote_cidfile(project: &Project, ssh: &Ssh, local: &Path) -> Result<()> {
    let remote = remote_path(project, local)?;
    let parent = Path::new(&remote)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into());
    let out = ssh.run_script(&format!(
        "mkdir -p {} && rm -f {}",
        sh_quote(&parent),
        sh_quote(&remote)
    ))?;
    if out.status.success() {
        return Ok(());
    }
    Err(
        fail!("cannot prepare {} on the server for --cidfile", remote)
            .now("check the workspace on the server: ulak doctor")
            .into_err(),
    )
}

/// The id file docker wrote, brought home. It is one line the container
/// runtime produced, not workspace content, so it is fetched by name
/// instead of widening the footprint to whatever directory holds it.
///
/// Called whatever the container's exit code was, because docker writes
/// the file when it CREATES the container. No file means docker never
/// got that far — a pull that failed, an image that is not there — and
/// that is a normal answer here, not a fault to report: docker has
/// already said what went wrong, in its own words, on this terminal.
/// Exit 3 is that answer; any other failure is a real one.
const NO_CIDFILE: i32 = 3;

fn fetch_cidfile(project: &Project, ssh: &Ssh, local: &Path) -> Result<()> {
    let remote = remote_path(project, local)?;
    let quoted = sh_quote(&remote);
    let out = ssh.run_script(&format!(
        "[ -f {quoted} ] || exit {NO_CIDFILE}\ncat {quoted}"
    ))?;
    if out.status.code() == Some(NO_CIDFILE) {
        return Ok(());
    }
    if !out.status.success() {
        return Err(
            fail!("the container id file on the server could not be read")
                .now(format!("read it by hand: ssh {} cat {remote}", ssh.dest))
                .into_err(),
        );
    }
    std::fs::write(local, &out.stdout)
        .with_context(|| format!("cannot write {}", local.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace on disk: `<root>/app`, `<root>/sub`, `<root>/.env`.
    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("app/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join(".env"), "A=1\n").unwrap();
        (temp, root)
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    fn parse(root: &Path, cwd: &Path, parts: &[&str]) -> RunSpec {
        RunSpec::parse(root, cwd, &argv(parts), 1).unwrap()
    }

    #[test]
    fn the_flag_table_never_puts_one_flag_on_both_sides() {
        for flag in LONG_WITH_VALUE {
            assert!(
                !LONG_WITHOUT_VALUE.contains(flag),
                "--{flag} cannot both take and not take a value"
            );
        }
        for c in SHORT_WITH_VALUE.chars() {
            assert!(!SHORT_WITHOUT_VALUE.contains(c), "-{c} is on both sides");
        }
    }

    #[test]
    fn an_absolute_bind_source_inside_the_workspace_becomes_workdir_relative() {
        let (_temp, root) = workspace();
        let mount = format!("{}:/app", root.join("app").display());
        let mut args = argv(&["run", "--rm", "-v", &mount, "alpine", "ls"]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

        assert_eq!(spec.inputs.len(), 1);
        assert_eq!(spec.inputs[0].local, root.join("app"));
        spec.rewrite(&root, &mut args);
        assert_eq!(args[3], "./app:/app");
    }

    /// The spelling that used to be read as a named volume.
    ///
    /// `.env` has no slash, so the old rule (`./`, `../`, `.`, `..`)
    /// dropped it down the daemon arm: no sync, no rewrite, no warning,
    /// and a container mounting an empty directory the far end created
    /// in the ssh user's home. `-v .aws:/root/.aws` mounted that user's
    /// real credentials. Docker itself has no such reading — a volume
    /// name cannot begin with a dot.
    #[test]
    fn a_dotfile_bind_source_is_a_path_here_not_a_named_volume() {
        let (_temp, root) = workspace();
        for parts in [
            vec!["run", "-v", ".env:/app/.env", "alpine"],
            vec!["run", "--volume=.env:/app/.env", "alpine"],
            vec![
                "run",
                "--mount",
                "type=bind,source=.env,target=/app/.env",
                "alpine",
            ],
        ] {
            let mut args = argv(&parts);
            let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

            assert_eq!(spec.inputs.len(), 1, "{parts:?} names one bind source");
            assert_eq!(spec.inputs[0].local, root.join(".env"), "{parts:?}");
            assert!(!spec.is_empty(), "{parts:?} must not take the fast path");

            let fp = spec.footprint(&root, "docker run");
            assert!(
                fp.filter_rules().iter().any(|r| r == "+ /.env"),
                "{parts:?} carries the file: {:?}",
                fp.filter_rules()
            );
            spec.rewrite(&root, &mut args);
            assert!(
                args.iter().any(|a| a.contains("./.env")),
                "{parts:?} was respelled: {args:?}"
            );
        }
    }

    #[test]
    fn a_relative_bind_source_keeps_its_spelling_and_joins_the_footprint() {
        let (_temp, root) = workspace();
        let mut args = argv(&["run", "-v", "./app:/app", "alpine"]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();
        let fp = spec.footprint(&root, "docker run");

        assert_eq!(fp.sync_dirs(), vec![root.join("app")]);
        assert_eq!(fp.entries[0].why, Why::Volume);
        assert!(fp.entries[0].writable);
        // The spelling is already right; what makes it resolve is the
        // remote workdir the run happens in.
        spec.rewrite(&root, &mut args);
        assert_eq!(args[2], "./app:/app");
    }

    #[test]
    fn a_source_that_climbs_out_of_the_cwd_is_respelled_from_the_cwd() {
        let (_temp, root) = workspace();
        let cwd = root.join("sub");
        let mut args = argv(&["run", "-v", "../app:/app", "alpine"]);
        let spec = RunSpec::parse(&root, &cwd, &args, 1).unwrap();

        assert_eq!(spec.inputs[0].local, root.join("app"));
        spec.rewrite(&cwd, &mut args);
        assert_eq!(args[2], "../app:/app");
    }

    /// A dot-leading source is unambiguous; a bare word is still a
    /// volume, because that is the only thing Docker will accept it as.
    #[test]
    fn a_named_volume_and_an_anonymous_one_are_left_completely_alone() {
        let (_temp, root) = workspace();
        let mut args = argv(&["run", "-v", "mydata:/var/lib/db", "-v", "/data", "alpine"]);
        let before = args.clone();
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

        assert!(spec.is_empty(), "daemon volumes claim no local path");
        // Daemon state is not the server's filesystem either: a named
        // volume that ended up in `server_refs` would be printed to the
        // user as a path this machine is not carrying.
        assert!(spec.server_refs.is_empty(), "and neither names a path");
        spec.rewrite(&root, &mut args);
        assert_eq!(args, before);
    }

    #[test]
    fn mount_carries_a_bind_source_and_leaves_a_volume_mount_untouched() {
        let (_temp, root) = workspace();
        let bind = format!(
            "type=bind,source={},target=/app,readonly",
            root.join("app").display()
        );
        let mut args = argv(&[
            "run",
            "--mount",
            &bind,
            "--mount",
            "type=volume,source=mydata,target=/var/lib/db",
            "--mount",
            "type=tmpfs,destination=/tmp",
            "alpine",
        ]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

        assert_eq!(spec.inputs.len(), 1, "only type=bind has a local source");
        assert!(!spec.inputs[0].writable, "readonly is not writable");
        spec.rewrite(&root, &mut args);
        assert_eq!(args[2], "type=bind,source=./app,target=/app,readonly");
        assert_eq!(args[4], "type=volume,source=mydata,target=/var/lib/db");
    }

    /// The spelling this module used to be blind to.
    ///
    /// Docker lowercases a `--mount` key and the `type=` value before it
    /// reads either — measured on 29.4.0, `TYPE=bind,SOURCE=…`,
    /// `type=BIND,source=…` and `TyPe=BiNd,SrC=…` all answer "bind
    /// source path does not exist", and bare `RO` and `ReadOnly=1` both
    /// produce a read-only mount. Matched case-sensitively, every one of
    /// these fell out of the spec: the directory was not synced, the
    /// source was not respelled, nothing was said, and the container
    /// read the SERVER's copy.
    #[test]
    fn a_mount_field_is_read_in_whatever_case_docker_reads_it_in() {
        let (_temp, root) = workspace();
        for value in [
            "TYPE=bind,SOURCE=./app,TARGET=/app,RO",
            "type=BIND,src=./app,target=/app,ReadOnly=1",
            "TyPe=BiNd,SrC=./app,TARGET=/app,READONLY=true",
        ] {
            let mut args = argv(&["run", "--mount", value, "alpine"]);
            let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

            assert_eq!(spec.inputs.len(), 1, "{value} names one bind source");
            assert_eq!(spec.inputs[0].local, root.join("app"), "{value}");
            assert!(!spec.inputs[0].writable, "{value} is read-only to docker");
            let fp = spec.footprint(&root, "docker run");
            assert_eq!(fp.sync_dirs(), vec![root.join("app")], "{value} travels");
            spec.rewrite(&root, &mut args);
            assert_eq!(args[2], value, "the spelling is already right");
        }
        // And the source is respelled under an upper-case key, which is
        // the half a relative source cannot show.
        let value = format!(
            "TYPE=bind,SOURCE={},TARGET=/app",
            root.join("app").display()
        );
        let mut args = argv(&["run", "--mount", &value, "alpine"]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();
        spec.rewrite(&root, &mut args);
        assert_eq!(args[2], "TYPE=bind,SOURCE=./app,TARGET=/app");

        // The obvious way to fold a key is to lowercase the whole field,
        // and it takes the PATH down with it — on a case-sensitive
        // filesystem `./OutDir` and `./outdir` are two directories, and
        // syncing the wrong one is this module's failure mode wearing
        // the fix's clothes. A source that is not there yet is folded
        // lexically, so this says the same thing on either filesystem.
        let mut args = argv(&[
            "run",
            "--mount",
            "TYPE=bind,SRC=./OutDir,TARGET=/x",
            "alpine",
        ]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();
        assert_eq!(spec.inputs[0].local, root.join("OutDir"));
        spec.rewrite(&root, &mut args);
        assert_eq!(args[2], "TYPE=bind,SRC=./OutDir,TARGET=/x");
    }

    #[test]
    fn mount_accepts_the_src_spelling_and_rebuilds_the_rest_byte_for_byte() {
        let (_temp, root) = workspace();
        let mut args = argv(&[
            "run",
            "--mount",
            "src=./app,type=bind,bind-propagation=rslave",
            "alpine",
        ]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();
        spec.rewrite(&root, &mut args);

        assert_eq!(args[2], "src=./app,type=bind,bind-propagation=rslave");
    }

    /// The spelling a naive comma split loses entirely.
    ///
    /// Docker reads a `--mount` value as a CSV record, so a directory
    /// whose name holds a comma is named by quoting the whole field.
    /// Split on every comma, `"source=./a,b"` became a field keyed
    /// `"source` — not a source — and the bind fell out of the spec
    /// with nothing said, which is the wrong machine's filesystem
    /// mounted silently.
    #[test]
    fn a_csv_quoted_mount_field_still_names_the_bind_source_inside_it() {
        let (_temp, root) = workspace();
        std::fs::create_dir_all(root.join("a,b")).unwrap();
        let mut args = argv(&[
            "run",
            "--mount",
            "\"source=./a,b\",type=bind,target=/x",
            "alpine",
        ]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

        assert_eq!(spec.inputs.len(), 1, "one field, one bind source");
        assert_eq!(spec.inputs[0].local, root.join("a,b"));
        let fp = spec.footprint(&root, "docker run");
        assert_eq!(
            fp.sync_dirs(),
            vec![root.join("a,b")],
            "the directory travels"
        );
        spec.rewrite(&root, &mut args);
        assert_eq!(args[2], "\"source=./a,b\",type=bind,target=/x");
    }

    /// A quote inside a quoted field is written twice at both ends of
    /// the trip, so the path that goes back out is the one docker reads
    /// back as the path that came in.
    #[test]
    fn a_doubled_quote_inside_a_quoted_field_survives_the_respelling() {
        let (_temp, root) = workspace();
        std::fs::create_dir_all(root.join("a\"b")).unwrap();
        let mut args = argv(&[
            "run",
            "--mount",
            "type=bind,\"source=./a\"\"b\",target=/x",
            "alpine",
        ]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

        assert_eq!(spec.inputs[0].local, root.join("a\"b"));
        spec.rewrite(&root, &mut args);
        assert_eq!(args[2], "type=bind,\"source=./a\"\"b\",target=/x");
    }

    /// Docker refuses each of these; the point is that ulak refuses
    /// them too rather than reading half a field and losing a path.
    #[test]
    fn a_mount_value_that_is_not_a_csv_record_is_refused_not_half_read() {
        let (_temp, root) = workspace();
        for bad in [
            "type=bind,\"source=./app",
            "type=bind,\"source=./app\"x",
            "type=bind,source=./a\"b",
        ] {
            let args = argv(&["run", "--mount", bad, "alpine"]);
            let err = RunSpec::parse(&root, &root, &args, 1).unwrap_err();
            assert!(format!("{err}").contains("docker can read"), "{bad}: {err}");
        }
    }

    /// `readonly` carries a value as well as a bare name, and docker
    /// takes exactly four words for it. `readonly=0` is the one that
    /// reads backwards at a glance: it leaves the mount WRITABLE, and
    /// recording it read-only described a bind mount the container
    /// writes into as one it cannot.
    #[test]
    fn a_valued_readonly_option_is_read_the_four_ways_docker_reads_it() {
        let (_temp, root) = workspace();
        for (option, writable) in [
            ("readonly=0", true),
            ("readonly=false", true),
            ("ro=0", true),
            ("readonly=1", false),
            ("readonly=true", false),
            ("readonly", false),
        ] {
            let value = format!("type=bind,source=./app,target=/app,{option}");
            let spec = parse(&root, &root, &["run", "--mount", &value, "alpine"]);
            assert_eq!(
                spec.inputs[0].writable, writable,
                "{option} is writable={writable} to docker"
            );
        }
    }

    #[test]
    fn a_mount_with_no_type_is_the_volume_docker_defaults_it_to() {
        let (_temp, root) = workspace();
        let spec = parse(
            &root,
            &root,
            &["run", "--mount", "source=./app,target=/app", "alpine"],
        );

        assert!(spec.is_empty(), "only type=bind names a path here");
    }

    /// A key with no `=` is a typo, and a typo may not take the process
    /// down. `source` alone is the shape that would be sliced for a
    /// value it does not have — the bare-option arm and the
    /// `field.text[key_len..]` slice are both reached only from here.
    /// (What a bare `readonly` MEANS is pinned in the four-ways test
    /// above, which reads it as one of its rows.)
    #[test]
    fn a_mount_key_with_no_value_is_read_not_sliced() {
        let (_temp, root) = workspace();
        let spec = parse(
            &root,
            &root,
            &["run", "--mount", "type=bind,source", "alpine"],
        );

        assert!(spec.is_empty(), "a key with no value names nothing");
    }

    #[test]
    fn the_container_command_after_the_image_is_never_rewritten() {
        let (_temp, root) = workspace();
        // The second `-v` has to be one a runaway scan would really
        // claim, or neither assertion can see the boundary move. It was
        // `sh -c -v /app:/app`, and both halves of that were dead: `-c`
        // is a value-taking shorthand, so it swallows the `-v` behind it
        // whether or not the scan stopped at the image, and `/app` is
        // absolute and outside the workspace, so `classify` calls it the
        // SERVER's and it can never enter `inputs` or be rewritten.
        // Measured: with the image `break` replaced by `continue`, that
        // argv left both assertions green.
        let mut args = argv(&[
            "run",
            "--rm",
            "-v",
            "./app:/app",
            "alpine",
            "sh",
            "-v",
            "./app:/other",
        ]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

        assert_eq!(spec.inputs.len(), 1, "the image ends the scan");
        spec.rewrite(&root, &mut args);
        assert_eq!(
            args[7], "./app:/other",
            "that path lives inside the container"
        );
    }

    #[test]
    fn a_flag_value_that_looks_like_an_image_does_not_end_the_scan() {
        let (_temp, root) = workspace();
        // `--name alpine` names the container, not the image: the scan
        // has to know that --name consumes the word after it.
        let args = argv(&["run", "--name", "alpine", "-v", "./app:/app", "alpine"]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

        assert_eq!(spec.inputs.len(), 1);
    }

    #[test]
    fn the_inline_volume_spelling_is_rewritten_in_place() {
        let (_temp, root) = workspace();
        let mount = format!("--volume={}:/app:ro", root.join("app").display());
        let mut args = argv(&["run", &mount, "alpine"]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

        assert!(!spec.inputs[0].writable, ":ro is not writable");
        spec.rewrite(&root, &mut args);
        assert_eq!(args[1], "--volume=./app:/app:ro");
    }

    #[test]
    fn a_bundled_short_flag_can_end_in_a_volume() {
        let (_temp, root) = workspace();
        for parts in [
            vec!["run", "-itv", "./app:/app", "alpine"],
            vec!["run", "-itv./app:/app", "alpine"],
            vec!["run", "-itv=./app:/app", "alpine"],
        ] {
            let mut args = argv(&parts);
            let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();
            assert_eq!(spec.inputs.len(), 1, "{parts:?} names one bind source");
            assert_eq!(spec.inputs[0].local, root.join("app"), "{parts:?}");
            spec.rewrite(&root, &mut args);
            assert!(
                args.iter().any(|a| a.ends_with("./app:/app")),
                "{parts:?} was respelled: {args:?}"
            );
        }
    }

    #[test]
    fn detach_is_seen_through_a_bundle() {
        let (_temp, root) = workspace();
        assert!(parse(&root, &root, &["run", "-itd", "alpine"]).detach);
        assert!(parse(&root, &root, &["run", "--detach", "alpine"]).detach);
        assert!(!parse(&root, &root, &["run", "-it", "alpine"]).detach);
        // The spelling that used to fall through `claim`'s `_` arm and
        // be recorded as a FOREGROUND run: the value branch runs before
        // the boolean table is ever consulted.
        assert!(parse(&root, &root, &["run", "--detach=true", "alpine"]).detach);
        assert!(!parse(&root, &root, &["run", "--detach=false", "alpine"]).detach);
    }

    /// The pair that is the whole rule, measured on 29.4.0:
    /// `echo hi | docker run -i --rm alpine cat` prints hi and drains
    /// the pipe, `echo hi | docker run --rm alpine cat` prints nothing
    /// and leaves both lines for whatever reads next.
    ///
    /// ssh does not make that distinction on its own — it drains its
    /// stdin as soon as the channel is up — so a `ulak docker run` that
    /// inherited stdin blindly ate a shell pipeline's data and handed it
    /// to nobody.
    #[test]
    fn stdin_is_only_carried_to_a_run_that_reads_it() {
        let (_temp, root) = workspace();
        for reads in [
            vec!["run", "-i", "alpine", "cat"],
            vec!["run", "--interactive", "alpine", "cat"],
            vec!["run", "-it", "alpine", "sh"],
            vec!["run", "--interactive=true", "alpine", "cat"],
            vec!["run", "--interactive=1", "alpine", "cat"],
            // `-i` bundled ahead of a value-taking letter is still an
            // `-i`, and the bundle ends at the `v`.
            vec!["run", "-iv", "./app:/app", "alpine", "cat"],
            // `-a stdin` opens no container stdin, but the client reads
            // ours for it either way — measured, it drains the pipe.
            vec!["run", "-a", "stdin", "-a", "stdout", "alpine", "cat"],
            vec!["run", "-astdin", "alpine", "cat"],
            vec!["run", "-a=stdin", "alpine", "cat"],
            vec!["run", "--attach", "STDIN", "alpine", "cat"],
            vec!["run", "--attach=stdin", "alpine", "cat"],
        ] {
            let spec = parse(&root, &root, &reads);
            assert!(
                wants_this_terminals_stdin(&spec, true, false),
                "{reads:?} reads stdin, so a pipeline into it must still arrive"
            );
        }

        for does_not in [
            vec!["run", "--rm", "alpine", "ls"],
            vec!["run", "-v", "./app:/app", "alpine", "ls"],
            vec!["run", "--interactive=false", "alpine", "ls"],
            vec!["run", "--interactive=0", "alpine", "ls"],
            vec!["run", "-a", "stdout", "-a", "stderr", "alpine", "ls"],
            // Past the image the flags are the CONTAINER's, and after
            // `--` they are docker's positionals.
            vec!["run", "alpine", "sh", "-i"],
            vec!["run", "--", "alpine", "-i"],
            // Detached: the client prints an id and leaves, so
            // `id=$(ulak docker run -itd …)` mid-pipeline must not
            // swallow what the next reader was waiting for.
            vec!["run", "-itd", "alpine", "sh"],
            vec!["run", "-i", "--detach", "alpine", "cat"],
            vec!["run", "-i", "--detach=true", "alpine", "cat"],
        ] {
            let spec = parse(&root, &root, &does_not);
            assert!(
                !wants_this_terminals_stdin(&spec, true, false),
                "{does_not:?} never reads stdin, and ssh would swallow it"
            );
            // A terminal has nothing queued to lose, and taking it away
            // would silence anything on the far side that asks.
            assert!(
                wants_this_terminals_stdin(&spec, true, true),
                "{does_not:?} must keep a terminal's stdin"
            );
        }
    }

    /// `create` records the container and starts nothing, so there is no
    /// process on the far side that could read a byte — measured,
    /// `printf 'a\nb\n' | docker create -i alpine cat` leaves both lines
    /// where `docker run -i` leaves none.
    #[test]
    fn create_reads_no_stdin_however_interactive_the_container_will_be() {
        let (_temp, root) = workspace();
        for parts in [
            vec!["create", "-i", "alpine", "cat"],
            vec!["create", "--attach", "stdin", "alpine", "cat"],
        ] {
            let spec = parse(&root, &root, &parts);
            assert!(spec.interactive, "{parts:?} opens the container's stdin");
            assert!(
                !wants_this_terminals_stdin(&spec, false, false),
                "{parts:?} still runs nothing that could read ours"
            );
        }
    }

    #[test]
    fn an_env_file_and_a_label_file_travel_with_the_container() {
        let (_temp, root) = workspace();
        std::fs::write(root.join("labels"), "team=infra\n").unwrap();
        let mut args = argv(&[
            "run",
            "--env-file",
            ".env",
            "--label-file",
            "./labels",
            "alpine",
        ]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();
        let fp = spec.footprint(&root, "docker run");

        assert_eq!(fp.entries.len(), 2);
        assert!(fp.entries.iter().all(|e| !e.is_dir && e.exists));
        assert!(fp.filter_rules().iter().any(|r| r == "+ /.env"));
        spec.rewrite(&root, &mut args);
        assert_eq!(args[2], "./.env");
        assert_eq!(args[4], "./labels");
    }

    /// A boolean shorthand written with an `=` is a command the client
    /// runs, and it was refused.
    ///
    /// pflag takes an `=` straight after a shorthand as that shorthand's
    /// value, booleans included. Measured on 29.4.0: `docker run --rm
    /// -d=false alpine echo hello` prints hello in the FOREGROUND, where
    /// `-d=true` prints an id. Ulak walked past the letter and looked the
    /// `=` up as a shorthand of its own, found it in neither table, and
    /// refused — after having already recorded the run as detached,
    /// which is the opposite of what was typed.
    #[test]
    fn a_boolean_shorthand_may_carry_its_value_with_an_equals() {
        let (_temp, root) = workspace();
        for (parts, detach, interactive) in [
            (vec!["run", "-d=false", "alpine"], false, false),
            (vec!["run", "-d=true", "alpine"], true, false),
            (vec!["run", "-i=1", "alpine"], false, true),
            (vec!["run", "-i=0", "alpine"], false, false),
            (vec!["run", "-t=false", "alpine"], false, false),
            // The value ends the cluster, so nothing after it is read as
            // another flag — `-id=false` is `-i` and then `d=false`.
            (vec!["run", "-id=false", "alpine"], false, true),
        ] {
            let spec = parse(&root, &root, &parts);
            assert_eq!(spec.detach, detach, "{parts:?} read the wrong detach");
            assert_eq!(
                spec.interactive, interactive,
                "{parts:?} read the wrong interactive"
            );
        }
    }

    /// `--security-opt seccomp=<file>` is opened by the CLI, so on the
    /// server it is the SERVER's file — a container confined by
    /// somebody else's profile, or by none, with nobody told.
    ///
    /// All four spellings, because the path hides inside the value here
    /// rather than being the whole of it, and the respelling has to put
    /// the `seccomp=` back in front of it.
    #[test]
    fn a_seccomp_profile_travels_with_the_container() {
        let (_temp, root) = workspace();
        std::fs::write(root.join("prof.json"), "{}\n").unwrap();
        // The word each spelling ends up as: the flag and the key kept
        // exactly as typed — the `:` separator is a spelling of the same
        // option, not something to normalise — with only the path
        // respelled, and a bare name given the `./` that says it is one.
        for (spelled, at, want) in [
            (
                argv(&["run", "--security-opt", "seccomp=./prof.json", "alpine"]),
                2,
                "seccomp=./prof.json",
            ),
            (
                argv(&["run", "--security-opt=seccomp=./prof.json", "alpine"]),
                1,
                "--security-opt=seccomp=./prof.json",
            ),
            (
                argv(&["run", "--security-opt", "seccomp:./prof.json", "alpine"]),
                2,
                "seccomp:./prof.json",
            ),
            (
                argv(&["create", "--security-opt", "seccomp=prof.json", "alpine"]),
                2,
                "seccomp=./prof.json",
            ),
        ] {
            let mut args = spelled.clone();
            let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();
            let fp = spec.footprint(&root, "docker run");
            assert_eq!(fp.entries.len(), 1, "{spelled:?} carried {:?}", fp.entries);
            assert!(fp.entries[0].local.ends_with("prof.json"), "{spelled:?}");
            spec.rewrite(&root, &mut args);
            assert_eq!(args[at], want, "{spelled:?} respelled wrongly");
        }
    }

    /// The values of this flag that name nothing on any disk. Without
    /// this the test above is satisfied by a claim that fires on
    /// everything, which would refuse `apparmor=` profiles that are
    /// perfectly valid names on the server and turn `unconfined` — a
    /// reserved word, measured — into a missing-file error.
    #[test]
    fn the_security_options_that_name_no_file_are_left_alone() {
        let (_temp, root) = workspace();
        for parts in [
            vec!["run", "--security-opt", "seccomp=unconfined", "alpine"],
            vec!["run", "--security-opt", "apparmor=docker-default", "alpine"],
            vec!["run", "--security-opt", "no-new-privileges", "alpine"],
            vec!["run", "--security-opt", "label=disable", "alpine"],
        ] {
            let spec = parse(&root, &root, &parts);
            assert!(
                spec.footprint(&root, "docker run").entries.is_empty(),
                "{parts:?} claimed a path it does not name"
            );
        }
    }

    /// `--env-file "$MISSING_VAR"` is one unset variable away from any
    /// script, and an empty value used to resolve to the cwd itself:
    /// `absolutize(cwd, "")` is the cwd. So the whole project became an
    /// input, `whole_anchor` was set — the entire workspace synced off a
    /// typo — and `--cidfile ""` was refused for a file nobody named.
    /// Docker answers `open : no such file or directory`, and for an
    /// empty `--cidfile` nothing at all; both are measured on 29.4.0.
    #[test]
    fn an_empty_path_value_names_nothing_rather_than_the_whole_project() {
        let (_temp, root) = workspace();
        for parts in [
            vec!["run", "--env-file=", "alpine"],
            vec!["run", "--env-file", "", "alpine"],
            vec!["run", "--label-file", "", "alpine"],
            vec!["run", "--cidfile", "", "alpine"],
        ] {
            let spec = parse(&root, &root, &parts);
            assert!(spec.is_empty(), "{parts:?} names no path on this machine");
            let fp = spec.footprint(&root, "docker run");
            assert!(!fp.whole_anchor, "{parts:?} must not sync the workspace");
            assert!(fp.entries.is_empty(), "{parts:?}");
        }
    }

    #[test]
    fn a_tilde_source_is_left_for_the_server_to_resolve() {
        let (_temp, root) = workspace();
        let mut args = argv(&["run", "-v", "~/data:/data", "alpine"]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

        assert!(spec.is_empty(), "the remote home is not ours to sync");
        assert_eq!(spec.server_refs, vec!["~/data".to_string()]);
        spec.rewrite(&root, &mut args);
        assert_eq!(args[2], "~/data:/data");
        // Only `~/` survives `sh_quote` unquoted, but every tilde
        // spelling is still the server's to resolve, so none of them is
        // ever synced or rewritten from here.
        for odd in ["~root/x:/y", "~:/y"] {
            let spec = parse(&root, &root, &["run", "-v", odd, "alpine"]);
            assert!(spec.is_empty(), "{odd} named nothing local");
            assert_eq!(spec.server_refs.len(), 1, "{odd} is a server reference");
        }
    }

    /// A bind source that was not here gets no filter rule in either
    /// direction, so whatever the container wrote into it stays on the
    /// server. Locally the user would have found the directory sitting
    /// in their project afterwards; here they find nothing, and the
    /// warning is the whole difference — so both halves of the rule are
    /// read off the list the warning actually prints.
    #[test]
    fn a_bind_source_that_was_not_here_is_placed_named_and_never_synced() {
        let (_temp, root) = workspace();
        let mut args = argv(&["run", "-v", "./out:/out", "alpine"]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();
        let fp = spec.footprint(&root, "docker run");

        assert_eq!(fp.entries.len(), 1);
        assert!(!fp.entries[0].exists);
        assert!(
            fp.entries[0].is_dir,
            "docker creates a missing source as a directory"
        );
        assert!(fp.filter_rules().iter().all(|r| !r.contains("out")));
        assert_eq!(
            sources_left_on_the_server(&fp, &root),
            vec!["./out".to_string()]
        );
        // The place is still made: the spelling goes out untouched, and
        // the remote workdir is what makes it resolve.
        spec.rewrite(&root, &mut args);
        assert_eq!(args[2], "./out:/out");

        // The other half of the rule, stated on a footprint that is not
        // empty on purpose: a source that IS here says nothing, and a
        // regression that dropped existing sources out of the footprint
        // altogether — the one that stops them syncing — would otherwise
        // read as that same silence.
        let fp = parse(&root, &root, &["run", "-v", "./app:/app", "alpine"])
            .footprint(&root, "docker run");
        assert_eq!(fp.entries.len(), 1);
        assert!(fp.entries[0].exists);
        assert!(sources_left_on_the_server(&fp, &root).is_empty());

        // And a missing source is only PLACED when it is a bind source.
        // Docker creates one of those, so it really is left up there;
        // a `--env-file` that is not here it simply refuses, measured on
        // 29.4.0 as `open ./nope: no such file or directory`. Naming
        // that file as left on the server would send the user looking
        // for it on the wrong machine — this module's own failure mode,
        // arriving through the warning meant to prevent it.
        let fp = parse(&root, &root, &["run", "--env-file", "./nope", "alpine"])
            .footprint(&root, "docker run");
        assert_eq!(fp.entries.len(), 1);
        assert!(!fp.entries[0].exists);
        assert!(sources_left_on_the_server(&fp, &root).is_empty());
    }

    #[test]
    fn the_flag_table_agrees_with_the_client_about_the_nine_it_does_not_print() {
        // `docker run --help` lists 98 flags; the client accepts 107.
        // Each of the other nine was settled by running `docker run
        // <flag>` and reading whether pflag asked for an argument.
        for flag in [
            "net",
            "net-alias",
            "dns-opt",
            "cpu-count",
            "cpu-percent",
            "io-maxbandwidth",
            "io-maxiops",
            "kernel-memory",
        ] {
            assert!(LONG_WITH_VALUE.contains(&flag), "--{flag} takes a value");
        }
        assert!(LONG_WITHOUT_VALUE.contains(&"disable-content-trust"));
        // The counts ARE the measurement — 92 value-taking and 15
        // boolean, from probing every flag-shaped string in the client
        // binary — so an edit that adds a flag without re-measuring
        // moves one of these numbers and says so here. Prose saying
        // "eight" is what let `--kernel-memory` sit missing from both
        // tables, refusing a `docker run` the client accepts.
        assert_eq!(LONG_WITH_VALUE.len(), 92, "`docker run` takes 92 of these");
        assert_eq!(LONG_WITHOUT_VALUE.len(), 15, "and 15 that take no value");
        // The two that matter in practice: `--net host` used to be
        // refused as unknown while `--net=host` parsed, and
        // `--kernel-memory 64m` swallowed nothing, so the scan stopped
        // on its value.
        let (_temp, root) = workspace();
        let spec = parse(
            &root,
            &root,
            &["run", "--net", "host", "-v", "./app:/app", "alpine", "ls"],
        );
        assert_eq!(spec.inputs.len(), 1, "--net host no longer ends the scan");
        let spec = parse(
            &root,
            &root,
            &[
                "run",
                "--kernel-memory",
                "64m",
                "-v",
                "./app:/app",
                "alpine",
                "ls",
            ],
        );
        assert_eq!(spec.inputs.len(), 1, "--kernel-memory takes its value");
    }

    #[test]
    fn an_absolute_source_outside_the_workspace_stays_the_servers_business() {
        let (_temp, root) = workspace();
        let mut args = argv(&[
            "run",
            "-v",
            "/var/run/docker.sock:/var/run/docker.sock",
            "alpine",
        ]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

        assert!(spec.is_empty());
        assert_eq!(spec.server_refs.len(), 1);
        spec.rewrite(&root, &mut args);
        assert_eq!(args[2], "/var/run/docker.sock:/var/run/docker.sock");
    }

    #[test]
    fn a_relative_source_that_escapes_the_workspace_is_refused() {
        let (_temp, root) = workspace();
        let cwd = root.join("sub");
        let args = argv(&["run", "-v", "../../elsewhere:/x", "alpine"]);
        let err = RunSpec::parse(&root, &cwd, &args, 1).unwrap_err();

        assert!(
            format!("{err}").contains("outside the workspace"),
            "the refusal has to name the reason: {err}"
        );
    }

    #[test]
    fn a_cidfile_is_an_output_and_is_respelled_like_any_other_path() {
        let (_temp, root) = workspace();
        let mut args = argv(&["run", "--cidfile", "sub/id.txt", "-d", "alpine"]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();

        assert_eq!(
            spec.cidfile.as_ref().unwrap().local,
            root.join("sub/id.txt")
        );
        assert!(spec.inputs.is_empty(), "an output is not synced up");
        assert!(spec.footprint(&root, "docker run").entries.is_empty());
        spec.rewrite(&root, &mut args);
        assert_eq!(args[2], "./sub/id.txt");
    }

    #[test]
    fn a_cidfile_that_is_already_here_is_refused_the_way_docker_refuses_it() {
        let (_temp, root) = workspace();
        std::fs::write(root.join("id.txt"), "stale\n").unwrap();
        let args = argv(&["run", "--cidfile", "id.txt", "alpine"]);
        let err = RunSpec::parse(&root, &root, &args, 1).unwrap_err();

        assert!(format!("{err}").contains("already there"), "{err}");
    }

    /// Docker opens the id file before it creates the container and
    /// makes no directory for it. Measured on 29.4.0: `docker run
    /// --cidfile nosuchdir/id.cid alpine:3.20 true` answers "failed to
    /// create the container ID file: open nosuchdir/id.cid: no such file
    /// or directory", exits 127, and creates no container.
    ///
    /// Ulak wrote that file on the SERVER, where `clear_remote_cidfile`
    /// makes the parent with `mkdir -p`. So the missing directory
    /// stopped nothing: the container ran, and only the write home
    /// failed — reported to stderr after the fact, with docker's own 0
    /// going out as the exit code. A run docker would have refused came
    /// back saying it had worked.
    #[test]
    fn a_cidfile_whose_directory_is_not_here_is_refused_before_anything_runs() {
        let (_temp, root) = workspace();
        let args = argv(&["run", "--cidfile", "nosuchdir/id.cid", "alpine"]);
        let err = RunSpec::parse(&root, &root, &args, 1).unwrap_err();

        let said = crate::ui::flatten(&err);
        assert!(said.contains("not here"), "{said}");
        assert!(
            said.contains("mkdir -p"),
            "the refusal has to say what to do: {said}"
        );
        // A file in the workspace root names no directory of its own,
        // and `Path::parent` answers `Some("")` for it — which is not a
        // directory, so reading that as the check would refuse every
        // plain `--cidfile id.cid`.
        let args = argv(&["run", "--cidfile", "id.cid", "alpine"]);
        let spec = RunSpec::parse(&root, &root, &args, 1).unwrap();
        assert_eq!(spec.cidfile.as_ref().unwrap().local, root.join("id.cid"));
    }

    /// One directory named twice is one entry, and the answer that has
    /// to survive the merge is `writable`. Keeping the first mount's
    /// answer recorded `-v ./app:/a:ro -v ./app:/b` as a directory
    /// nothing writes into, which silenced `doctor`'s
    /// unprotected-writable and writable-shared warnings for exactly the
    /// command that has one.
    #[test]
    fn a_source_mounted_twice_is_writable_if_either_mount_is() {
        let (_temp, root) = workspace();
        for parts in [
            vec!["run", "-v", "./app:/a:ro", "-v", "./app:/b", "alpine"],
            vec!["run", "-v", "./app:/a", "-v", "./app:/b:ro", "alpine"],
        ] {
            let fp = parse(&root, &root, &parts).footprint(&root, "docker run");

            assert_eq!(fp.entries.len(), 1, "{parts:?} names one directory");
            assert!(fp.entries[0].writable, "{parts:?} writes into it");
        }
    }

    #[test]
    fn mounting_the_project_itself_makes_the_whole_anchor_travel() {
        let (_temp, root) = workspace();
        let spec = parse(&root, &root, &["run", "-v", ".:/app", "alpine"]);
        let fp = spec.footprint(&root, "docker run");

        assert!(fp.whole_anchor);
        assert_eq!(fp.sync_dirs(), vec![root.clone()]);
    }

    #[test]
    fn the_nested_spelling_never_reads_the_command_path() {
        let (_temp, root) = workspace();
        let args = argv(&["container", "run", "-v", "./app:/app", "alpine"]);
        let spec = RunSpec::parse(&root, &root, &args, 2).unwrap();

        assert_eq!(spec.inputs.len(), 1);
        assert_eq!(spec.inputs[0].local, root.join("app"));
    }

    #[test]
    fn an_unknown_flag_is_refused_rather_than_guessed() {
        let (_temp, root) = workspace();
        let args = argv(&["run", "--brand-new-flag", "alpine", "-v", "./app:/app"]);
        let err = RunSpec::parse(&root, &root, &args, 1).unwrap_err();

        assert!(format!("{err}").contains("--brand-new-flag"), "{err}");
        // The inline spelling cannot swallow the image, so it is allowed
        // through — and it is the way out the error points at.
        let inline = argv(&["run", "--brand-new-flag=x", "-v", "./app:/app", "alpine"]);
        assert_eq!(
            RunSpec::parse(&root, &root, &inline, 1)
                .unwrap()
                .inputs
                .len(),
            1
        );
        // The short mirror, which is the branch that matters more: read
        // as a boolean rather than refused, `-Z 5` makes `5` the image,
        // ends the scan before the `-v`, and forwards argv verbatim — so
        // the container mounts the SERVER's `./app` and nothing is said.
        // Docker refuses both spellings ("unknown shorthand flag: 'Z' in
        // -Z"), bundled or alone.
        for parts in [
            vec!["run", "-Z", "5", "-v", "./app:/app", "alpine"],
            vec!["run", "-itZ", "-v", "./app:/app", "alpine"],
        ] {
            let err = RunSpec::parse(&root, &root, &argv(&parts), 1).unwrap_err();
            assert!(format!("{err}").contains('Z'), "{parts:?}: {err}");
        }
    }

    #[test]
    fn a_double_dash_ends_the_flags() {
        let (_temp, root) = workspace();
        let spec = parse(&root, &root, &["run", "--", "alpine", "-v", "/app:/app"]);

        assert!(
            spec.is_empty(),
            "everything after -- belongs to docker's positionals"
        );
    }

    #[test]
    fn asking_for_help_reads_nothing_local() {
        let (_temp, root) = workspace();
        let spec = parse(&root, &root, &["run", "--help", "-v", "./app:/app"]);

        assert!(spec.is_empty());
    }

    #[test]
    fn a_path_that_is_not_there_cannot_climb_out_through_dot_dot() {
        let (_temp, root) = workspace();
        // Nothing to canonicalize, so the folding has to be lexical or
        // `starts_with(root)` says yes to a path outside root.
        let escaped = absolutize(&root, "sub/../../gone");
        assert!(!escaped.starts_with(&root), "{}", escaped.display());
    }
}
