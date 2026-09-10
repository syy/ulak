//! What a `docker buildx bake` invocation reads and writes locally.
//!
//! Bake targets live in HCL, JSON or Compose files with variables,
//! functions, `inherits` chains and `--set` overrides layered on top.
//! Ulak does not parse any of that: Buildx already resolves the whole
//! thing, and `docker buildx bake --print` hands back the finished plan
//! as JSON. The important boundary is where that command runs: Buildx is
//! a SERVER dependency, never a local one. Ulak sends the definition
//! files into a private bootstrap directory and asks the server's Buildx.
//!
//! The bootstrap reproduces the absolute local layout below itself:
//! `<boot>/Users/you/repo/docker-bake.hcl`. Relative paths therefore
//! resolve against the mirror of the local cwd, while absolute paths
//! outside `<boot>` retain their normal meaning as paths on the server.
//! If Compose `include:` or an explicit key makes Buildx ask for another
//! local file, its fully resolved failure path is back-mapped and one
//! more small bootstrap round carries it. This is the same measured
//! mechanism the Compose footprint resolver uses.
//!
//! Two resolution rules decide every path below, and both were settled
//! by running real builds rather than by reading docs:
//!
//!   * every relative path in the plan resolves against the process's
//!     working directory — NOT against the bake file that declared it.
//!     `cd sub && docker buildx bake -f ../docker-bake.hcl` with
//!     `context = "ctx"` looks for `sub/ctx` and fails when only
//!     `../ctx` exists. So `cwd` is load-bearing here, not incidental.
//!   * `dockerfile` is the one exception: it resolves against the
//!     target's own resolved context.
//!
//! Unit tests below replay recorded Buildx JSON and need no Docker. The
//! live contract belongs in `e2e_bake`: the process under test is given a
//! poisoned local `docker`, while the server provides the real Buildx.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::Result;
use serde::Deserialize;

use crate::ssh::{Ssh, sh_quote};
use crate::ui::{self, fail};

const MAX_BOOTSTRAP_ROUNDS: usize = 6;

#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    /// Build context, absolute. None when it is a URL or git ref —
    /// the server fetches those itself.
    pub context: Option<PathBuf>,
    /// Dockerfile, absolute, when it is a local file.
    pub dockerfile: Option<PathBuf>,
    /// Named additional contexts that are local paths.
    pub contexts: Vec<(String, PathBuf)>,
    /// Local paths this target READS (secrets, ssh keys, local cache).
    pub reads: Vec<PathBuf>,
    /// Local paths this target WRITES (local outputs, metadata files).
    pub writes: Vec<Write>,
}

/// A path a build will create, and which SHAPE it will create.
///
/// The shape cannot be recovered by looking: a write path does not exist
/// yet when the plan is made, so `is_dir()` on it answers false for a
/// directory and a file alike. Only the exporter knows, and it says so in
/// the plan — `type=local` writes a tree, `type=tar` writes one tarball.
/// Guessing either way breaks the other: carried as a file, an exported
/// directory never comes home; carried as a directory, a metadata file
/// never does.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Write {
    pub path: PathBuf,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct Plan {
    /// Definition/support files Buildx needed to resolve the Bake.
    pub files: Vec<PathBuf>,
    pub targets: Vec<Target>,
}

/// What resolving the Bake invocation produced.
pub enum Resolution {
    /// Buildx was asked for help; no definition needs bootstrapping.
    Forward,
    /// `--list` is Buildx's already-formatted answer from the server.
    Answer(Answer),
    /// A real bake, resolved by the server's `buildx bake --print`.
    Plan(Plan),
}

/// Resolve a Bake invocation with the server's Buildx.
///
/// `args` is the full Docker argv with the command path in front;
/// `tail_start` is where Bake's own arguments begin (1 for `bake`, 2 for
/// `buildx bake`). Nothing before it is scanned or rewritten.
pub fn resolve(
    cwd: &Path,
    args: &[String],
    tail_start: usize,
    ssh: &Ssh,
    remote_workspace_root: &str,
) -> Result<Resolution> {
    let tail = args.get(tail_start..).unwrap_or(&[]);

    // Help needs neither a plan nor a local workspace mirror. It can go
    // straight to Docker in the remote HOME, just like every daemon-only
    // command.
    if boolean_flag(tail, &["-h", "--help"]) {
        return Ok(Resolution::Forward);
    }

    let mut files = definition_files(cwd, tail)?;
    // Compose-flavoured Bake definitions auto-load `.env` from the
    // working directory. Carry it into both the bootstrap and the real
    // workspace so planning and building cannot interpolate differently.
    let dot_env = cwd.join(".env");
    if dot_env.is_file() {
        files.push(dot_env.canonicalize().unwrap_or(dot_env));
    }
    let listing = !flag_values(tail, "--list").is_empty();
    let ours = if listing { &[][..] } else { &["--print"][..] };
    let mut run = run_remote_buildx(cwd, tail, ours, files, ssh, remote_workspace_root)?;

    if listing {
        if !run.output.status.success() && remote_buildx_version(ssh).is_none() {
            return Err(missing_remote_buildx(ssh));
        }
        if run.output.status.success() {
            clear_bootstrap(ssh, remote_workspace_root, &run.boot.rel);
        }
        return Ok(Resolution::Answer(Answer {
            stdout: run.output.stdout,
            stderr: run.output.stderr,
            status: run.output.status,
        }));
    }

    if !run.output.status.success() {
        if remote_buildx_version(ssh).is_none() {
            return Err(missing_remote_buildx(ssh));
        }
        let stderr = String::from_utf8_lossy(&run.output.stderr);
        let detail = stderr
            .lines()
            .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n       ");
        let mut error = fail!(
            "Buildx could not resolve this bake definition on {}",
            ssh.dest
        );
        if !detail.is_empty() {
            error = error.now(detail);
        }
        return Err(error
            .now(format!(
                "the bootstrap was left at ~/{} so the server-side failure can be inspected",
                run.boot.rel
            ))
            .into_err());
    }

    let mut printed: PrintedPlan = serde_json::from_slice(&run.output.stdout).map_err(|error| {
        fail!("could not read the plan `docker buildx bake --print` returned: {error}")
            .now(format!(
                "Ulak reads the format Buildx {MEASURED_BUILDX} prints; {} has {}",
                ssh.dest,
                remote_buildx_version(ssh).unwrap_or_else(|| "an unknown version".into())
            ))
            .now("upgrade the docker-buildx plugin on the server")
            .into_err()
    })?;
    printed.back_map(&run.boot);
    run.files.sort();
    run.files.dedup();
    clear_bootstrap(ssh, remote_workspace_root, &run.boot.rel);
    Ok(Resolution::Plan(resolve_plan(cwd, run.files, printed)?))
}

/// One `--print`, turned into the plan, given the definition files it was
/// produced from.
///
/// Split out so everything downstream of Buildx can be unit-tested from
/// recorded JSON without either local Docker or a server. Live parity is
/// exercised by the server-backed Bake e2e suite.
fn resolve_plan(cwd: &Path, files: Vec<PathBuf>, printed: PrintedPlan) -> Result<Plan> {
    // Every target in the map, not just the ones named on the command
    // line: a `contexts = { prev = "target:base" }` entry pulls `base`
    // into the plan, and its context is as local as any other.
    let mut targets = Vec::with_capacity(printed.target.len());
    for (name, t) in printed.target {
        targets.push(resolve_target(cwd, &name, &t)?);
    }
    Ok(Plan { files, targets })
}

/// The server-side Buildx's already-formatted `--list` answer.
pub struct Answer {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    status: std::process::ExitStatus,
}

impl Answer {
    pub fn emit(self) -> Result<ExitCode> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(&self.stderr);
        let _ = std::io::stdout().write_all(&self.stdout);
        crate::docker::status_code(self.status)
    }
}

/// The `--metadata-file` path, which Buildx writes after a real bake.
///
/// It is deliberately outside `Plan`: `--print` neither reports nor
/// writes it, and it belongs to the invocation rather than to any one
/// target. Without pulling it back, `bake --metadata-file meta.json`
/// leaves the user's file untouched on a run that reported success.
pub fn metadata_file(cwd: &Path, args: &[String], tail_start: usize) -> Option<PathBuf> {
    let tail = args.get(tail_start..).unwrap_or(&[]);
    flag_values(tail, "--metadata-file")
        .into_iter()
        .next_back()
        .map(|raw| for_write(cwd, &raw))
}

fn resolve_target(cwd: &Path, name: &str, t: &PrintedTarget) -> Result<Target> {
    let context = match t.context.as_deref() {
        Some(raw) if !is_remote(raw) => {
            Some(for_read(cwd, local_path(raw), name, "build context")?)
        }
        _ => None,
    };

    // A `dockerfile-inline` target still prints `dockerfile: "Dockerfile"`,
    // naming a file that need not exist. Treating that as an input fails
    // every inline bake with a "cannot be read" that is our invention.
    let dockerfile = match (&context, t.dockerfile.as_deref()) {
        (Some(ctx), Some(raw)) if t.dockerfile_inline.is_none() && !is_remote(raw) => {
            Some(for_read(ctx, local_path(raw), name, "Dockerfile")?)
        }
        _ => None,
    };

    let mut contexts = Vec::new();
    for (alias, raw) in &t.contexts {
        // `target:other` names another target in this same plan and
        // `docker-image://` names a registry image; neither is a path.
        if is_remote(raw) || raw.starts_with("target:") {
            continue;
        }
        contexts.push((
            alias.clone(),
            for_read(cwd, local_path(raw), name, &format!("context `{alias}`"))?,
        ));
    }
    contexts.sort();
    contexts.dedup();

    let mut reads = Vec::new();
    for secret in &t.secret {
        // `type=env` secrets carry `env` and no `src`: the value comes
        // from the environment, so there is no file to carry.
        if let Some(src) = &secret.src {
            reads.push(for_read(cwd, src, name, "secret")?);
        }
    }
    for ssh in &t.ssh {
        // A bare `default` is the agent socket, which forwards rather
        // than syncs; only explicit key paths are files.
        for path in &ssh.paths {
            reads.push(for_read(cwd, path, name, "ssh key")?);
        }
    }
    for cache in &t.cache_from {
        // A local cache that is not there is a cache MISS, not a
        // failure. Measured on Buildx 0.33.0: a bake whose target says
        // `cache-from = ["type=local,src=.buildcache"]` with no such
        // directory builds and exits 0, saying only "WARNING: local
        // cache import at .buildcache skipped due to err: … no such
        // file or directory".
        //
        // `for_read`'s refusal turned that warning into a hard stop, so
        // ulak refused the ordinary first run of a project that builds
        // fine without it — a fresh clone, a cold CI, any build before
        // the cache has been written once. `buildflags` already says
        // this on the direct route, where `--cache-from` is the one
        // carrier marked `strict: false` for exactly this reason.
        if let Some(src) = cache.local_path(&cache.src)
            && let Ok(path) = for_read(cwd, src, name, "cache-from")
        {
            reads.push(path);
        }
    }
    reads.sort();
    reads.dedup();

    let mut writes: Vec<Write> = Vec::new();
    for out in &t.output {
        // `dest: "-"` is stdout, and registry/image/cacheonly outputs
        // carry no dest at all.
        if let Some(dest) = out.dest.as_deref().filter(|dest| *dest != "-") {
            writes.push(Write {
                path: for_write(cwd, dest),
                is_dir: out.writes_a_directory(),
            });
        }
    }
    for cache in &t.cache_to {
        // A local cache export is always a directory of blobs.
        if let Some(dest) = cache.local_path(&cache.dest) {
            writes.push(Write {
                path: for_write(cwd, dest),
                is_dir: true,
            });
        }
    }
    writes.sort();
    // Two exporters aimed at one path is a broken bake, but if it
    // happens the directory reading wins: descending into a path that
    // turned out to be a file costs nothing, while treating a directory
    // as a file loses everything inside it.
    writes.dedup_by(|a, b| {
        let same = a.path == b.path;
        if same {
            b.is_dir |= a.is_dir;
        }
        same
    });

    Ok(Target {
        name: name.to_string(),
        context,
        dockerfile,
        contexts,
        reads,
        writes,
    })
}

/// The Buildx whose `--print` output this parser was measured against.
const MEASURED_BUILDX: &str = "v0.33.0";

struct RemoteRun {
    output: std::process::Output,
    files: Vec<PathBuf>,
    boot: Boot,
}

struct Boot {
    abs: String,
    rel: String,
}

impl Boot {
    fn rel_of(local: &Path) -> String {
        local.to_string_lossy().trim_start_matches('/').to_string()
    }

    fn back(&self, remote: &str) -> Option<PathBuf> {
        let rest = remote.strip_prefix(&self.abs)?;
        rest.starts_with('/').then(|| PathBuf::from(rest))
    }
}

fn run_remote_buildx(
    cwd: &Path,
    tail: &[String],
    ours: &[&str],
    mut files: Vec<PathBuf>,
    ssh: &Ssh,
    remote_workspace_root: &str,
) -> Result<RemoteRun> {
    let boot_rel = format!(
        "{remote_workspace_root}/bake-bootstrap.{}",
        std::process::id()
    );
    let work_rel = format!("{boot_rel}/{}", Boot::rel_of(cwd));
    let prepared = ssh.run_checked(
        &format!(
            "umask 077 && rm -rf {boot} && mkdir -p {work} && chmod 700 {boot} && cd {boot} && pwd",
            boot = sh_quote(&boot_rel),
            work = sh_quote(&work_rel),
        ),
        "preparing the Bake bootstrap directory",
    )?;
    let boot = Boot {
        abs: String::from_utf8_lossy(&prepared.stdout).trim().to_string(),
        rel: boot_rel,
    };
    if boot.abs.is_empty() {
        return Err(fail!(
            "the Bake bootstrap directory could not be read on {}",
            ssh.dest
        )
        .into_err());
    }

    let mut remote_tail = tail.to_vec();
    rewrite_definition_tail(cwd, &mut remote_tail)?;
    for round in 0..MAX_BOOTSTRAP_ROUNDS {
        push_bootstrap(ssh, &boot.rel, &files)?;
        let output = ssh.run_script(&format!(
            "cd {work} || exit 9\n{command}\n",
            work = sh_quote(&work_rel),
            command = buildx_command(ours, &remote_tail),
        ))?;
        if output.status.success() {
            return Ok(RemoteRun {
                output,
                files,
                boot,
            });
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let grown: Vec<PathBuf> = missing_local_paths(&stderr, &boot, cwd)
            .into_iter()
            .filter(|path| !files.contains(path))
            .collect();
        if grown.is_empty() || round + 1 == MAX_BOOTSTRAP_ROUNDS {
            return Ok(RemoteRun {
                output,
                files,
                boot,
            });
        }
        for path in grown {
            ui::dim(&format!("Bake bootstrap also needs {}", path.display()));
            files.push(path);
        }
    }
    unreachable!("the bootstrap loop returns on success or final failure")
}

fn buildx_command(ours: &[&str], tail: &[String]) -> String {
    let mut words = vec!["docker".to_string(), "buildx".into(), "bake".into()];
    words.extend(ours.iter().map(|word| (*word).to_string()));
    words.extend(tail.iter().cloned());
    words
        .iter()
        .map(|word| sh_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn push_bootstrap(ssh: &Ssh, boot_rel: &str, files: &[PathBuf]) -> Result<()> {
    let rsync = crate::sync::local_rsync()?;
    let mut command = Command::new(&rsync);
    command.args([
        "--relative",
        "--links",
        "--perms",
        "--times",
        "--compress",
        "--from0",
        "--files-from=-",
        "--timeout=60",
    ]);
    command.arg("-e").arg(ssh.rsync_transport());
    command.arg("--").arg("/");
    command.arg(format!("{}:{boot_rel}/", ssh.dest));
    let payload = files
        .iter()
        .flat_map(|path| {
            let mut bytes = Boot::rel_of(path).into_bytes();
            bytes.push(0);
            bytes
        })
        .collect();
    let out = crate::proc::run_bounded(&mut command, Some(payload), crate::proc::BOOTSTRAP)?;
    crate::audit::record_command("rsync", &command, out.status.code());
    if out.timed_out {
        return Err(fail!(
            "sending the Bake definitions to {} took longer than {}s and was stopped",
            ssh.dest,
            crate::proc::BOOTSTRAP.as_secs()
        )
        .now("check the link, then rerun the command")
        .into_err());
    }
    if !out.status.success() {
        return Err(fail!(
            "the Bake definitions could not be sent to {} for resolving:\n    {}",
            ssh.dest,
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .take(4)
                .collect::<Vec<_>>()
                .join("\n    ")
        )
        .now("run the preflight checks: ulak doctor")
        .into_err());
    }
    Ok(())
}

fn missing_local_paths(stderr: &str, boot: &Boot, cwd: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut remember = |candidate: &str, allow_relative: bool| {
        let candidate = candidate.trim_matches(|c| matches!(c, ':' | ',' | '"' | '\''));
        let path = Path::new(candidate);
        let local = boot
            .back(candidate)
            .or_else(|| (allow_relative && path.is_relative()).then(|| cwd.join(path)));
        if let Some(local) = local
            && local.is_file()
        {
            let local = local.canonicalize().unwrap_or(local);
            if !paths.contains(&local) {
                paths.push(local);
            }
        }
    };

    // Absolute paths underneath the bootstrap can arrive anywhere in a
    // diagnostic. Individual words are enough for these because Unix
    // absolute paths with spaces are also caught by the structured
    // `open`/`stat` scan below.
    for token in stderr.split_whitespace() {
        remember(token, false);
    }

    // Buildx reports inputs discovered while loading in two common
    // shapes: `open path: ...` for Compose includes and `stat path: ...`
    // for secret/SSH files. Keep the whole field as well as individual
    // words so paths containing spaces are not split beyond recovery.
    for line in stderr.lines() {
        for marker in ["open ", "stat "] {
            let mut rest = line;
            while let Some((_, after)) = rest.split_once(marker) {
                remember(after.split_once(": ").map_or(after, |(path, _)| path), true);
                rest = after;
            }
        }
    }
    paths.sort();
    paths
}

fn clear_bootstrap(ssh: &Ssh, remote_workspace_root: &str, boot_rel: &str) {
    let home = remote_workspace_root;
    let _ = ssh.run_checked(
        &format!(
            "rm -rf {boot}; find {home} -maxdepth 1 -name 'bake-bootstrap.*' -mmin +1440 -exec rm -rf {{}} \\; 2>/dev/null; true",
            boot = sh_quote(boot_rel),
            home = sh_quote(home),
        ),
        "clearing the Bake bootstrap directory",
    );
}

fn remote_buildx_version(ssh: &Ssh) -> Option<String> {
    let out = ssh.run_script("docker buildx version").ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().nth(1).map(str::to_string)
}

fn missing_remote_buildx(ssh: &Ssh) -> anyhow::Error {
    fail!(
        "the `docker buildx` plugin is not available on {}",
        ssh.dest
    )
    .now(format!(
        "check with: ssh {} docker buildx version",
        ssh.dest
    ))
    .now("install or upgrade the docker-buildx plugin on the server")
    .into_err()
}

/// Re-spell local `-f/--file` values for the mirrored remote cwd.
///
/// Relative spellings normally travel unchanged, but canonicalising and
/// making them relative also covers an absolute `-f`, `..`, and a local
/// symlink. Remote definitions are left for Buildx to fetch.
pub fn rewrite_definition_args(cwd: &Path, args: &mut [String], tail_start: usize) -> Result<()> {
    rewrite_definition_tail(cwd, args.get_mut(tail_start..).unwrap_or(&mut []))
}

fn rewrite_definition_tail(cwd: &Path, tail: &mut [String]) -> Result<()> {
    let mut index = 0;
    while index < tail.len() {
        let argument = tail[index].clone();
        if argument == "--file" || argument == "-f" {
            if let Some(value) = tail.get_mut(index + 1) {
                *value = rewritten_definition(cwd, value)?;
            }
            index += 2;
            continue;
        }
        if let Some(raw) = argument.strip_prefix("--file=") {
            tail[index] = format!("--file={}", rewritten_definition(cwd, raw)?);
            index += 1;
            continue;
        }
        if let Some(raw) = argument.strip_prefix("-f").filter(|raw| !raw.is_empty()) {
            let raw = raw.strip_prefix('=').unwrap_or(raw);
            tail[index] = format!("-f{}", rewritten_definition(cwd, raw)?);
            index += 1;
            continue;
        }
        index += if VALUE_FLAGS
            .iter()
            .any(|(long, short)| argument == *long || Some(argument.as_str()) == *short)
        {
            2
        } else {
            1
        };
    }
    Ok(())
}

fn rewritten_definition(cwd: &Path, raw: &str) -> Result<String> {
    if raw == "-" || is_remote(raw) {
        return Ok(raw.to_string());
    }
    let local = for_read(cwd, raw, "bake", "bake definition")?;
    Ok(relative_from(cwd, &local))
}

fn relative_from(from: &Path, target: &Path) -> String {
    let common = crate::invocation::common_ancestor(&[from, target]);
    let mut relative = PathBuf::new();
    if let Ok(up) = from.strip_prefix(&common) {
        for _ in up.components() {
            relative.push("..");
        }
    }
    if let Ok(down) = target.strip_prefix(&common) {
        relative.push(down);
    }
    if relative.as_os_str().is_empty() {
        ".".into()
    } else {
        relative.to_string_lossy().into_owned()
    }
}

/// Which files this invocation reads its definition from.
///
/// `-f` wins when given. Otherwise Buildx reads EVERY default name it
/// finds rather than the first, so all of them are inputs: a project
/// with both `compose.yaml` and `docker-bake.hcl` has its plan built
/// from the two together, and syncing only one would change the plan.
fn definition_files(cwd: &Path, tail: &[String]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut named = false;
    for raw in flag_values(tail, "--file") {
        named = true;
        if raw == "-" {
            return Err(fail!("a bake definition read from stdin cannot be carried to the server")
                .now("write it to a file and pass it with -f, for example: ulak docker bake -f docker-bake.hcl")
                .into_err());
        }
        // A remote definition is the server's to fetch, exactly like a
        // remote context.
        if is_remote(&raw) {
            continue;
        }
        files.push(for_read(cwd, &raw, "bake", "bake definition")?);
    }
    if !named {
        for name in DEFAULT_FILES {
            let candidate = cwd.join(name);
            if candidate.is_file() {
                files.push(candidate.canonicalize().unwrap_or(candidate));
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// The names Buildx looks for when no `-f` is given, in the order it
/// reads them. Measured by creating all eight and watching every one
/// get read.
const DEFAULT_FILES: &[&str] = &[
    "compose.yaml",
    "compose.yml",
    "docker-compose.yml",
    "docker-compose.yaml",
    "docker-bake.json",
    "docker-bake.hcl",
    "docker-bake.override.json",
    "docker-bake.override.hcl",
];

/// Flags that take a separate value, so a scan never mistakes that
/// value for something of its own — `--set '*.dockerfile=-f'` must not
/// look like a `-f`.
const VALUE_FLAGS: &[(&str, Option<&str>)] = &[
    ("--file", Some("-f")),
    ("--set", None),
    ("--var", None),
    ("--metadata-file", None),
    ("--progress", None),
    ("--builder", None),
    ("--call", None),
    ("--allow", None),
    ("--sbom", None),
    ("--provenance", None),
    ("--list", None),
];

/// Whether one of `names` appears as a flag in its own right.
///
/// Walks with the same value-skipping rule as `flag_values`, so
/// `--set '*.dockerfile=--help'` is a value and not a request for help.
fn boolean_flag(tail: &[String], names: &[&str]) -> bool {
    let mut i = 0;
    while i < tail.len() {
        let arg = tail[i].as_str();
        if names.contains(&arg) {
            return true;
        }
        i += if VALUE_FLAGS
            .iter()
            .any(|(l, s)| arg == *l || Some(arg) == *s)
        {
            2
        } else {
            1
        };
    }
    false
}

/// Every value given to `long` (or its short spelling), in argv order,
/// in both the `--flag value` and `--flag=value` spellings.
fn flag_values(tail: &[String], long: &str) -> Vec<String> {
    let short = VALUE_FLAGS
        .iter()
        .find(|(l, _)| *l == long)
        .and_then(|(_, s)| *s);
    let prefix = format!("{long}=");

    let mut found = Vec::new();
    let mut i = 0;
    while i < tail.len() {
        let arg = tail[i].as_str();
        if let Some(value) = arg.strip_prefix(&prefix) {
            found.push(value.to_string());
            i += 1;
            continue;
        }
        // pflag also lets a shorthand carry its value inside its own
        // word, and Buildx honours it: measured on v0.33.0 with a
        // `release.hcl` and a `docker-bake.hcl` side by side, both
        // `bake -frelease.hcl app --print` and `bake -f=release.hcl app
        // --print` printed release.hcl's target where a bare `bake app
        // --print` printed the default's.
        //
        // Walking past those two spellings left `definition_files` on
        // its DEFAULT_FILES scan, so Buildx planned from the file the
        // user named while Ulak carried a different one — or, in a
        // project that keeps no `docker-bake.hcl`, carried no definition
        // at all. pflag strips exactly one `=` between a shorthand and
        // its attached value, so `-f=x` names `x` and `-f==x` names
        // `=x`; that is the rule `buildflags::file_flag` already encodes
        // for `docker build`, and this is the two agreeing again. An
        // attached value consumes no following word, so this advances by
        // one.
        if let Some(short) = short
            && let Some(rest) = arg.strip_prefix(short)
            && !rest.is_empty()
        {
            found.push(rest.strip_prefix('=').unwrap_or(rest).to_string());
            i += 1;
            continue;
        }
        let takes_value = VALUE_FLAGS
            .iter()
            .any(|(l, s)| arg == *l || Some(arg) == *s);
        if takes_value {
            if (arg == long || Some(arg) == short)
                && let Some(value) = tail.get(i + 1)
            {
                found.push(value.clone());
            }
            // Skip the value either way: it belongs to this flag and is
            // not an argument in its own right.
            i += 2;
            continue;
        }
        i += 1;
    }
    found
}

/// A path the bake will read. It must exist — Buildx would fail on the
/// server otherwise, and failing here names the target that owns it.
///
/// Whether it sits inside the project is NOT decided here: `plan` only
/// reports what the bake touches. A context above the workspace is a
/// real and legitimate shape (Compose's `additional_contexts: ../libs`
/// prints exactly that), so the caller applies `require_inside` and
/// decides.
fn for_read(base: &Path, raw: &str, target: &str, label: &str) -> Result<PathBuf> {
    let path = join(base, raw);
    path.canonicalize().map_err(|_| {
        fail!(
            "the {label} for bake target `{target}` cannot be read: {}",
            path.display()
        )
        .now("check the path in the bake file; this is what the server's `buildx bake --print` resolved it to")
        .into_err()
    })
}

/// A path the bake will write. It must NOT be required to exist —
/// `output = ["type=local,dest=out/api"]` names a directory the build
/// is about to create.
///
/// The deepest existing ancestor is still canonicalized, so a write
/// under a symlinked directory compares equal to the same path reached
/// as a read and the caller's dedup actually catches it.
fn for_write(base: &Path, raw: &str) -> PathBuf {
    let path = join(base, raw);
    let mut rest = Vec::new();
    let mut probe = path.as_path();
    loop {
        if let Ok(real) = probe.canonicalize() {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (probe.file_name(), probe.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                probe = parent;
            }
            _ => return lexical(&path),
        }
    }
}

fn join(base: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Last resort for a path with no existing ancestor: fold away `.` and
/// `..` textually so two spellings of one path still compare equal.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Buildx's own spelling for "this one is on the machine the command was
/// typed on", and the single scheme that means the opposite of the rest.
///
/// It earns its keep with a REMOTE definition — the case
/// `definition_files` hands to the server to fetch — where a bare
/// relative path belongs to the fetched repository and `cwd://` is the
/// only way left to name a directory here. Reading its `://` as "the
/// server will fetch this" would skip precisely the directory that
/// nothing can fetch.
const CWD_SCHEME: &str = "cwd://";

/// Contexts and definitions Buildx fetches itself. Matches the rule
/// `docker build` already uses for its positional context, minus the one
/// scheme that points back here.
fn is_remote(raw: &str) -> bool {
    if raw.starts_with(CWD_SCHEME) {
        return false;
    }
    raw.contains("://") || raw.starts_with("git@")
}

/// The path a value names on this machine, once `cwd://` has had its say.
///
/// Every caller pairs this with `is_remote`, so that "not remote" and
/// "here is the path" cannot drift apart. `--print` resolves the prefix
/// away wherever it can — a target's own `context = "cwd://api"` comes
/// back as plain `api` (measured, Buildx 0.33.0) — which leaves the
/// named contexts as the place it survives into the plan; the other
/// call sites keep it so that a Buildx which one day prints less than it
/// resolves cannot turn a carried path into a directory called `cwd:`.
///
/// A bare `cwd://` names the working directory itself, and `join` reads
/// the empty path that is left as exactly that.
fn local_path(raw: &str) -> &str {
    raw.strip_prefix(CWD_SCHEME).unwrap_or(raw)
}

// ── the shape `--print` returns ─────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PrintedPlan {
    #[serde(default)]
    target: BTreeMap<String, PrintedTarget>,
}

impl PrintedPlan {
    /// Turn absolute paths below the remote bootstrap back into their
    /// local absolute spelling. Other absolute paths stay untouched: the
    /// footprint layer will report that they are outside the workspace
    /// and the real Bake will interpret them on the server.
    fn back_map(&mut self, boot: &Boot) {
        for target in self.target.values_mut() {
            map_option(&mut target.context, boot);
            map_option(&mut target.dockerfile, boot);
            for value in target.contexts.values_mut() {
                map_value(value, boot);
            }
            for secret in &mut target.secret {
                map_option(&mut secret.src, boot);
            }
            for ssh in &mut target.ssh {
                for path in &mut ssh.paths {
                    map_value(path, boot);
                }
            }
            for cache in target
                .cache_from
                .iter_mut()
                .chain(target.cache_to.iter_mut())
            {
                map_option(&mut cache.src, boot);
                map_option(&mut cache.dest, boot);
            }
            for output in &mut target.output {
                map_option(&mut output.dest, boot);
            }
        }
    }
}

fn map_option(value: &mut Option<String>, boot: &Boot) {
    if let Some(value) = value {
        map_value(value, boot);
    }
}

fn map_value(value: &mut String, boot: &Boot) {
    if !Path::new(value).is_absolute() {
        return;
    }
    if let Some(local) = boot.back(value) {
        *value = local.to_string_lossy().into_owned();
    }
}

#[derive(Debug, Default, Deserialize)]
struct PrintedTarget {
    context: Option<String>,
    dockerfile: Option<String>,
    #[serde(rename = "dockerfile-inline")]
    dockerfile_inline: Option<String>,
    #[serde(default)]
    contexts: BTreeMap<String, String>,
    #[serde(default)]
    secret: Vec<PrintedSecret>,
    #[serde(default)]
    ssh: Vec<PrintedSsh>,
    #[serde(rename = "cache-from", default)]
    cache_from: Vec<PrintedCache>,
    #[serde(rename = "cache-to", default)]
    cache_to: Vec<PrintedCache>,
    #[serde(default)]
    output: Vec<PrintedOutput>,
}

#[derive(Debug, Deserialize)]
struct PrintedSecret {
    src: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PrintedSsh {
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PrintedCache {
    #[serde(rename = "type")]
    kind: Option<String>,
    src: Option<String>,
    dest: Option<String>,
}

impl PrintedCache {
    /// Only `type=local` cache lives on this filesystem; `registry`,
    /// `gha` and `inline` name things the daemon reaches on its own.
    fn local_path<'a>(&self, side: &'a Option<String>) -> Option<&'a str> {
        match self.kind.as_deref() {
            Some("local") => side.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PrintedOutput {
    #[serde(rename = "type")]
    kind: Option<String>,
    dest: Option<String>,
    /// Buildx prints this as the STRING "false", not a JSON bool.
    tar: Option<String>,
}

impl PrintedOutput {
    /// Whether this exporter's `dest` is a directory or a single file.
    ///
    /// `local` unpacks a tree. `tar` is one archive. `docker` and `oci`
    /// are archives too — unless `tar=false`, which makes them write an
    /// unpacked image directory instead (measured: it arrives in the plan
    /// as `"tar": "false"`).
    fn writes_a_directory(&self) -> bool {
        match self.kind.as_deref() {
            Some("local") => true,
            Some("docker" | "oci") => self.tar.as_deref() == Some("false"),
            // `tar` is the only remaining exporter that takes a dest,
            // and it writes one archive. An exporter this map has not
            // met is read the same way; there is no reading that is safe
            // for both shapes, so it falls to the caller's own
            // `is_dir || path.is_dir()` to correct it once the path
            // exists, and to this map to be extended when one appears.
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `docker buildx bake --print` as Buildx v0.33.0 really printed
    /// it, together with the definition and the arguments that produced
    /// it.
    ///
    /// Recording the answer lets every path rule be tested on a machine
    /// with no Docker at all. Representative live shapes are also built
    /// by `e2e_bake` against the server's Buildx.
    struct Recording {
        /// The definition file's name and body, written into the project
        /// before either half runs.
        file: (&'static str, &'static str),
        /// Everything after `bake` on the command line.
        tail: &'static [&'static str],
        printed: &'static str,
    }

    impl Recording {
        /// The plan the recording says this invocation has, with no
        /// Buildx consulted.
        fn replay(&self, cwd: &Path) -> Result<Plan> {
            self.write_definition(cwd);
            let tail = v(self.tail);
            let files = definition_files(cwd, &tail)?;
            resolve_plan(cwd, files, serde_json::from_str(self.printed).unwrap())
        }

        fn write_definition(&self, cwd: &Path) {
            std::fs::write(cwd.join(self.file.0), self.file.1).unwrap();
        }
    }

    /// A context, a named context, a secret, both cache directions and a
    /// local output — every kind of local path one target can name.
    const EVERYTHING: Recording = Recording {
        file: (
            "everything.hcl",
            r#"
            target "api" {
              context = "api"
              contexts = {
                common   = "shared"
                upstream = "docker-image://alpine:3.20"
              }
              secret     = ["id=npmrc,src=secrets/npmrc"]
              cache-from = ["type=local,src=cache-in"]
              cache-to   = ["type=local,dest=cache-out"]
              output     = ["type=local,dest=out/api"]
            }
            "#,
        ),
        tail: &["-f", "everything.hcl", "api"],
        printed: r#"{
  "group": { "default": { "targets": ["api"] } },
  "target": {
    "api": {
      "context": "api",
      "contexts": {
        "common": "shared",
        "upstream": "docker-image://alpine:3.20"
      },
      "dockerfile": "Dockerfile",
      "cache-from": [ { "src": "cache-in", "type": "local" } ],
      "cache-to": [ { "dest": "cache-out", "type": "local" } ],
      "secret": [ { "id": "npmrc", "src": "secrets/npmrc" } ],
      "output": [ { "dest": "out/api", "type": "local" } ]
    }
  }
}"#,
    };

    /// Every exporter that takes a `dest`, so the tree-or-file question
    /// is answered from the plan rather than from the filesystem.
    const EXPORTERS: Recording = Recording {
        file: (
            "exporters.hcl",
            r#"
            target "api" {
              context  = "api"
              cache-to = ["type=local,dest=cache-out"]
              output = [
                "type=local,dest=out/tree",
                "type=tar,dest=out/one.tar",
                "type=oci,dest=out/oci.tar",
                "type=oci,dest=out/oci-tree,tar=false",
                "type=docker,dest=out/docker.tar",
                "type=docker,dest=out/docker-tree,tar=false",
                "type=registry",
                "type=cacheonly",
              ]
            }
            "#,
        ),
        tail: &["-f", "exporters.hcl", "api"],
        printed: r#"{
  "group": { "default": { "targets": ["api"] } },
  "target": {
    "api": {
      "context": "api",
      "dockerfile": "Dockerfile",
      "cache-to": [ { "dest": "cache-out", "type": "local" } ],
      "output": [
        { "dest": "out/tree", "type": "local" },
        { "dest": "out/one.tar", "type": "tar" },
        { "dest": "out/oci.tar", "type": "oci" },
        { "dest": "out/oci-tree", "tar": "false", "type": "oci" },
        { "dest": "out/docker.tar", "type": "docker" },
        { "dest": "out/docker-tree", "tar": "false", "type": "docker" },
        { "type": "registry" },
        { "type": "cacheonly" }
      ]
    }
  }
}"#,
    };

    /// Contexts the server fetches for itself.
    const REMOTE_CONTEXTS: Recording = Recording {
        file: (
            "remote.hcl",
            r#"
            target "fromweb" { context = "https://github.com/docker/buildx.git#master" }
            target "fromgit" { context = "git@github.com:docker/buildx.git" }
            "#,
        ),
        tail: &["-f", "remote.hcl", "fromweb", "fromgit"],
        printed: r#"{
  "group": { "default": { "targets": ["fromgit", "fromweb"] } },
  "target": {
    "fromgit": {
      "context": "git@github.com:docker/buildx.git",
      "dockerfile": "Dockerfile"
    },
    "fromweb": {
      "context": "https://github.com/docker/buildx.git#master",
      "dockerfile": "Dockerfile"
    }
  }
}"#,
    };

    /// The one scheme that points back at this machine, recorded because
    /// the whole handling rests on a fact only Buildx can confirm: the
    /// prefix SURVIVES `--print` on a named context, while the target's
    /// own `context` has it resolved away. Read `://` as "the server
    /// fetches it" and `common` is the directory nobody carries.
    const CWD_SCHEME_CONTEXTS: Recording = Recording {
        file: (
            "cwdscheme.hcl",
            r#"
            target "api" {
              context = "cwd://api"
              contexts = {
                common = "cwd://shared"
                here   = "cwd://"
                store  = "oci-layout://sha256:abc"
              }
            }
            "#,
        ),
        tail: &["-f", "cwdscheme.hcl", "api"],
        printed: r#"{
  "group": { "default": { "targets": ["api"] } },
  "target": {
    "api": {
      "context": "api",
      "contexts": {
        "common": "cwd://shared",
        "here": "cwd://",
        "store": "oci-layout://sha256:abc"
      },
      "dockerfile": "Dockerfile"
    }
  }
}"#,
    };

    /// The trap this module's `dockerfile` handling exists for: Buildx
    /// prints `"dockerfile": "Dockerfile"` for an inline target too,
    /// naming a file that need not exist.
    const INLINE_DOCKERFILE: Recording = Recording {
        file: (
            "inline.hcl",
            "target \"inline\" {\n context = \"bare\"\n dockerfile-inline = \"FROM scratch\\n\"\n}\n",
        ),
        tail: &["-f", "inline.hcl", "inline"],
        printed: r#"{
  "group": { "default": { "targets": ["inline"] } },
  "target": {
    "inline": {
      "context": "bare",
      "dockerfile": "Dockerfile",
      "dockerfile-inline": "FROM scratch\n"
    }
  }
}"#,
    };

    /// `api` was asked for; `base` arrives because `api` names it.
    const TARGET_CONTEXT: Recording = Recording {
        file: (
            "targetctx.hcl",
            r#"
            target "base" { context = "shared" }
            target "api" {
              context  = "api"
              contexts = { prev = "target:base" }
            }
            "#,
        ),
        tail: &["-f", "targetctx.hcl", "api"],
        printed: r#"{
  "group": { "default": { "targets": ["api"] } },
  "target": {
    "api": {
      "context": "api",
      "contexts": { "prev": "target:base" },
      "dockerfile": "Dockerfile"
    },
    "base": {
      "context": "shared",
      "dockerfile": "Dockerfile",
      "output": [ { "type": "cacheonly" } ]
    }
  }
}"#,
    };

    /// One path named twice within a target, and one context shared by
    /// three of them.
    const SHARED_CONTEXT: Recording = Recording {
        file: (
            "shared.hcl",
            r#"
            group "default" { targets = ["one", "two", "three"] }
            target "one" {
              context    = "shared"
              cache-from = ["type=local,src=cache-in", "type=local,src=cache-in"]
              secret     = ["id=a,src=secrets/npmrc", "id=b,src=secrets/npmrc"]
            }
            target "two"   { context = "shared" }
            target "three" { context = "shared" }
            "#,
        ),
        tail: &["-f", "shared.hcl"],
        printed: r#"{
  "group": { "default": { "targets": ["one", "three", "two"] } },
  "target": {
    "one": {
      "context": "shared",
      "dockerfile": "Dockerfile",
      "cache-from": [ { "src": "cache-in", "type": "local" } ],
      "secret": [
        { "id": "a", "src": "secrets/npmrc" },
        { "id": "b", "src": "secrets/npmrc" }
      ]
    },
    "three": { "context": "shared", "dockerfile": "Dockerfile" },
    "two": { "context": "shared", "dockerfile": "Dockerfile" }
  }
}"#,
    };

    /// A Compose file taken as a bake definition, found without `-f`.
    const COMPOSE: Recording = Recording {
        file: (
            "docker-compose.yml",
            "services:\n  \
               api:\n    \
                 build:\n      \
                   context: ./api\n      \
                   additional_contexts:\n        \
                     libs: ./shared\n    \
                 image: api:latest\n  \
               cache:\n    \
                 image: redis:7\n",
        ),
        tail: &[],
        printed: r#"{
  "group": { "default": { "targets": ["api"] } },
  "target": {
    "api": {
      "context": "api",
      "contexts": { "libs": "shared" },
      "dockerfile": "Dockerfile",
      "tags": ["api:latest"]
    }
  }
}"#,
    };

    /// A plan Buildx resolves happily and we cannot: the context is not
    /// there. Both halves must fail, and in our words.
    const MISSING_CONTEXT: Recording = Recording {
        file: (
            "missing.hcl",
            "target \"api\" {\n context = \"nowhere\"\n}\n",
        ),
        tail: &["-f", "missing.hcl", "api"],
        printed: r#"{
  "group": { "default": { "targets": ["api"] } },
  "target": {
    "api": { "context": "nowhere", "dockerfile": "Dockerfile" }
  }
}"#,
    };

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn project() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for dir in ["api", "web", "shared", "secrets", "cache-in", "bare"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for df in ["api/Dockerfile", "web/Dockerfile", "shared/Dockerfile"] {
            std::fs::write(root.join(df), "FROM scratch\n").unwrap();
        }
        std::fs::write(root.join("secrets/npmrc"), "token\n").unwrap();
        temp
    }

    fn find<'a>(plan: &'a Plan, name: &str) -> &'a Target {
        plan.targets
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("target `{name}` missing from the plan"))
    }

    /// A local cache that is not there is a cache MISS, not a failure.
    /// Measured on Buildx 0.33.0: a bake whose target says `cache-from
    /// = ["type=local,src=.buildcache"]` with no such directory builds
    /// and exits 0, saying only "WARNING: local cache import at
    /// .buildcache skipped due to err: … no such file or directory".
    ///
    /// `for_read`'s refusal turned that warning into a hard stop, so
    /// ulak refused the ordinary first run of a project that builds
    /// fine without it: a fresh clone, a cold CI, any build before the
    /// cache has been written once.
    #[test]
    fn a_cache_that_is_not_written_yet_is_a_miss_and_not_a_refusal() {
        let temp = project();
        let cwd = temp.path().canonicalize().unwrap();
        std::fs::remove_dir(cwd.join("cache-in")).unwrap();

        let plan = EVERYTHING.replay(&cwd).unwrap();
        assert_eq!(
            find(&plan, "api").reads,
            vec![cwd.join("secrets/npmrc")],
            "a cache directory that is not here has nothing to carry"
        );
    }

    #[test]
    fn a_plan_reports_every_local_path_its_targets_read_and_write() {
        let temp = project();
        let cwd = temp.path().canonicalize().unwrap();
        let plan = EVERYTHING.replay(&cwd).unwrap();
        let api = find(&plan, "api");

        assert_eq!(api.context.as_deref(), Some(cwd.join("api").as_path()));
        assert_eq!(
            api.dockerfile.as_deref(),
            Some(cwd.join("api/Dockerfile").as_path()),
            "the default Dockerfile resolves inside the context, not beside the bake file"
        );
        assert_eq!(
            api.contexts,
            vec![("common".to_string(), cwd.join("shared"))],
            "a docker-image:// context is the server's to pull"
        );
        assert_eq!(
            api.reads,
            vec![cwd.join("cache-in"), cwd.join("secrets/npmrc")]
        );
        assert_eq!(
            api.writes,
            vec![
                Write {
                    path: cwd.join("cache-out"),
                    is_dir: true
                },
                Write {
                    path: cwd.join("out/api"),
                    is_dir: true
                },
            ],
            "outputs are written, so they must not have to exist yet"
        );
        assert_eq!(plan.files, vec![cwd.join("everything.hcl")]);
    }

    #[test]
    fn each_exporter_says_whether_its_dest_is_a_tree_or_a_single_file() {
        let temp = project();
        let cwd = temp.path().canonicalize().unwrap();
        // None of these exist, which is the entire difficulty: the shape
        // cannot be read off the filesystem, only off the exporter.
        let plan = EXPORTERS.replay(&cwd).unwrap();
        let dir = |rel: &str| Write {
            path: cwd.join(rel),
            is_dir: true,
        };
        let file = |rel: &str| Write {
            path: cwd.join(rel),
            is_dir: false,
        };
        assert_eq!(
            find(&plan, "api").writes,
            // Sorted by path, so `-` (0x2D) lands before `.` (0x2E).
            vec![
                dir("cache-out"),
                dir("out/docker-tree"),
                file("out/docker.tar"),
                dir("out/oci-tree"),
                file("out/oci.tar"),
                file("out/one.tar"),
                dir("out/tree"),
            ],
            "registry and cacheonly write nothing local; tar=false unpacks"
        );
    }

    #[test]
    fn a_context_that_is_a_url_or_git_ref_is_left_for_the_server() {
        let temp = project();
        let cwd = temp.path().canonicalize().unwrap();
        let plan = REMOTE_CONTEXTS.replay(&cwd).unwrap();
        for name in ["fromweb", "fromgit"] {
            let t = find(&plan, name);
            assert!(t.context.is_none(), "{name} has no local context");
            assert!(
                t.dockerfile.is_none(),
                "{name}'s Dockerfile lives in the tree the server fetches"
            );
        }
    }

    #[test]
    fn a_cwd_scheme_context_is_a_path_here_rather_than_something_to_fetch() {
        let temp = project();
        let cwd = temp.path().canonicalize().unwrap();
        let plan = CWD_SCHEME_CONTEXTS.replay(&cwd).unwrap();
        let api = find(&plan, "api");
        // `cwd://` is what a REMOTE definition uses to reach back to
        // this machine, and a definition fetched by the server is a
        // shape this module already supports — so these have to travel.
        // Bare `cwd://` is the working directory itself.
        assert_eq!(
            api.contexts,
            vec![
                ("common".to_string(), cwd.join("shared")),
                ("here".to_string(), cwd.clone()),
            ],
            "cwd:// names a directory here; oci-layout:// is the daemon's own store"
        );
        assert_eq!(
            api.context.as_ref(),
            Some(&cwd.join("api")),
            "and the prefix Buildx already resolved away leaves a plain path, \
             not a directory called `cwd:`"
        );
    }

    #[test]
    fn an_inline_dockerfile_is_not_mistaken_for_a_file_on_disk() {
        let temp = project();
        let cwd = temp.path().canonicalize().unwrap();
        // `bare` deliberately holds no Dockerfile. Buildx names one all
        // the same, and believing it would fail a build that is valid.
        let plan = INLINE_DOCKERFILE.replay(&cwd).unwrap();
        let t = find(&plan, "inline");
        assert_eq!(t.context.as_deref(), Some(cwd.join("bare").as_path()));
        assert!(t.dockerfile.is_none());
    }

    #[test]
    fn a_target_pulled_in_by_another_targets_context_is_planned_too() {
        let temp = project();
        let cwd = temp.path().canonicalize().unwrap();
        // Only `api` was asked for; `base` arrives because api depends
        // on it, and its context has to be synced or the build starves.
        let plan = TARGET_CONTEXT.replay(&cwd).unwrap();
        assert_eq!(
            find(&plan, "base").context.as_deref(),
            Some(cwd.join("shared").as_path())
        );
        assert!(
            find(&plan, "api").contexts.is_empty(),
            "target: contexts name plan entries, not paths"
        );
    }

    #[test]
    fn one_context_shared_by_many_targets_is_never_reported_twice_within_a_target() {
        let temp = project();
        let cwd = temp.path().canonicalize().unwrap();
        let plan = SHARED_CONTEXT.replay(&cwd).unwrap();
        let one = find(&plan, "one");
        assert_eq!(
            one.reads,
            vec![cwd.join("cache-in"), cwd.join("secrets/npmrc")],
            "the same file named by two secrets is one file"
        );
        // Across targets the repetition is real and stays visible; the
        // paths are canonical, so the caller's dedup collapses them.
        let shared: Vec<_> = plan
            .targets
            .iter()
            .filter_map(|t| t.context.clone())
            .collect();
        assert_eq!(shared.len(), 3);
        assert!(shared.iter().all(|c| *c == cwd.join("shared")));
    }

    #[test]
    fn a_compose_file_is_planned_through_the_same_path_as_hcl() {
        let temp = project();
        let cwd = temp.path().canonicalize().unwrap();
        let plan = COMPOSE.replay(&cwd).unwrap();
        let api = find(&plan, "api");
        assert_eq!(api.context.as_deref(), Some(cwd.join("api").as_path()));
        assert_eq!(api.contexts, vec![("libs".to_string(), cwd.join("shared"))]);
        assert!(
            plan.targets.iter().all(|t| t.name != "cache"),
            "a service with no build section builds nothing"
        );
        assert_eq!(plan.files, vec![cwd.join("docker-compose.yml")]);
    }

    #[test]
    fn a_missing_context_names_the_target_that_wanted_it() {
        let temp = project();
        let cwd = temp.path().canonicalize().unwrap();
        let err = MISSING_CONTEXT.replay(&cwd).unwrap_err();
        let shown = crate::ui::flatten(&err);
        assert!(shown.contains("api"), "{shown}");
        assert!(shown.contains("nowhere"), "{shown}");
    }

    #[test]
    fn every_default_definition_file_present_is_an_input() {
        let temp = project();
        let cwd = temp.path().canonicalize().unwrap();
        // Buildx reads all of these, not the first one it finds, so a
        // plan built from both is only reproducible if both are synced.
        std::fs::write(
            cwd.join("docker-compose.yml"),
            "services:\n  web:\n    build: ./web\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join("docker-bake.hcl"),
            "target \"api\" {\n context = \"api\"\n}\n",
        )
        .unwrap();

        assert_eq!(
            definition_files(&cwd, &v(&["api", "web"])).unwrap(),
            vec![cwd.join("docker-bake.hcl"), cwd.join("docker-compose.yml")]
        );
        // A named `-f` replaces the defaults rather than adding to them.
        assert_eq!(
            definition_files(&cwd, &v(&["-f", "docker-bake.hcl"])).unwrap(),
            vec![cwd.join("docker-bake.hcl")]
        );

        // And the two spellings that keep the value inside the flag's
        // own word. Measured on Buildx v0.33.0 with a `release.hcl` and
        // a `docker-bake.hcl` side by side: `bake -frelease.hcl app
        // --print` and `bake -f=release.hcl app --print` each printed
        // release.hcl's target, where a bare `bake app --print` printed
        // the default's. Unread, `definition_files` fell back to the
        // DEFAULT_FILES scan — so Buildx planned from the file the user
        // named and Ulak carried a different one, or, in a project with
        // no default name, carried nothing at all.
        std::fs::write(cwd.join("release.hcl"), "target \"api\" {}\n").unwrap();
        for spelling in [
            v(&["-frelease.hcl", "api"]),
            v(&["-f=release.hcl", "api"]),
            v(&["-f", "release.hcl", "api"]),
        ] {
            assert_eq!(
                definition_files(&cwd, &spelling).unwrap(),
                vec![cwd.join("release.hcl")],
                "{spelling:?} did not name the file Buildx plans from"
            );
        }
    }

    #[test]
    fn listing_targets_is_answered_without_a_plan() {
        // `--print` and `--list` are mutually exclusive in Buildx, so
        // asking for a plan here would turn "show me the targets" into
        // "Buildx could not resolve this bake definition".
        for listing in [
            v(&["bake", "--list=targets"]),
            v(&["bake", "--list", "variables"]),
        ] {
            assert!(
                !flag_values(&listing[1..], "--list").is_empty(),
                "{listing:?} must take the answer path, not --print"
            );
        }
        // And a value that merely looks like one is still just a value.
        assert!(
            boolean_flag(&v(&["bake", "--help"]), &["-h", "--help"]),
            "a real --help is found"
        );
        assert!(
            !boolean_flag(
                &v(&["bake", "--set", "*.dockerfile=--help"]),
                &["-h", "--help"]
            ),
            "a --set value that spells --help is not a request for help"
        );
    }

    #[test]
    fn asking_buildx_for_its_help_is_not_a_plan() {
        // No Buildx needed and none consulted: help is answered before
        // anything is resolved, which is why this returns None rather
        // than an empty plan the caller has to interpret.
        for help in [
            v(&["bake", "--help"]),
            v(&["bake", "-h"]),
            v(&["bake", "-f", "docker-bake.hcl", "--help"]),
        ] {
            assert!(
                boolean_flag(&help[1..], &["-h", "--help"]),
                "{help:?} asks Docker to explain itself, not to build"
            );
        }
    }

    // ── argv scanning, which needs no Buildx ────────────────────────

    #[test]
    fn a_definition_read_from_stdin_is_refused_not_silently_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let err = definition_files(&cwd, &v(&["-f", "-"])).unwrap_err();
        assert!(crate::ui::flatten(&err).contains("stdin"));
    }

    #[test]
    fn a_remote_definition_is_the_servers_to_fetch() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        // Named, so the defaults are not probed; remote, so there is
        // nothing local to carry. An empty list, not an error.
        let files =
            definition_files(&cwd, &v(&["-f", "https://example.com/docker-bake.hcl"])).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn a_flags_value_is_never_mistaken_for_a_definition_file() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        std::fs::write(cwd.join("docker-bake.hcl"), "target \"t\" {}\n").unwrap();

        // `--set`'s value merely looks like a flag pair. Reading it as
        // one would send us hunting for a file called `bake.hcl`.
        let files = definition_files(&cwd, &v(&["--set", "*.dockerfile=-f", "--var", "X=-f"]));
        assert_eq!(files.unwrap(), vec![cwd.join("docker-bake.hcl")]);
    }

    #[test]
    fn every_local_definition_spelling_is_rewritten_for_the_remote_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let cwd = root.join("work");
        std::fs::create_dir_all(root.join("defs")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let definition = root.join("defs/release.hcl");
        std::fs::write(&definition, "target \"api\" {}\n").unwrap();
        let expected = "../defs/release.hcl";

        for mut args in [
            v(&["bake", "-f", definition.to_str().unwrap(), "api"]),
            v(&["bake", "--file=../defs/release.hcl", "api"]),
            v(&["bake", "-f=../defs/release.hcl", "api"]),
            v(&["bake", "-f../defs/release.hcl", "api"]),
        ] {
            rewrite_definition_args(&cwd, &mut args, 1).unwrap();
            let value = flag_values(&args[1..], "--file");
            assert_eq!(value, vec![expected], "{args:?}");
        }

        let mut remote = v(&["bake", "-f", "https://example.com/docker-bake.hcl", "api"]);
        rewrite_definition_args(&cwd, &mut remote, 1).unwrap();
        assert_eq!(remote[2], "https://example.com/docker-bake.hcl");
    }

    #[test]
    fn paths_printed_inside_the_remote_bootstrap_map_back_home() {
        let temp = project();
        let cwd = temp.path().canonicalize().unwrap();
        let boot = Boot {
            abs: "/home/builder/.ulak/workspaces/id/bake-bootstrap.1".into(),
            rel: ".ulak/workspaces/id/bake-bootstrap.1".into(),
        };
        let printed_context = format!("{}{}", boot.abs, cwd.join("api").display());
        let mut printed: PrintedPlan = serde_json::from_value(serde_json::json!({
            "target": {
                "api": {
                    "context": printed_context,
                    "dockerfile": "Dockerfile"
                }
            }
        }))
        .unwrap();

        printed.back_map(&boot);
        let plan = resolve_plan(&cwd, Vec::new(), printed).unwrap();
        assert_eq!(
            find(&plan, "api").context.as_deref(),
            Some(cwd.join("api").as_path())
        );
    }

    #[test]
    fn bootstrap_discovers_relative_compose_includes_and_secret_files() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(cwd.join("sub")).unwrap();
        std::fs::create_dir_all(cwd.join("secrets")).unwrap();
        std::fs::write(cwd.join("sub/extra file.yaml"), "services: {}\n").unwrap();
        std::fs::write(cwd.join("secrets/token"), "secret\n").unwrap();
        let boot = Boot {
            abs: "/home/builder/.ulak/workspaces/id/bake-bootstrap.1".into(),
            rel: ".ulak/workspaces/id/bake-bootstrap.1".into(),
        };

        let found = missing_local_paths(
            "ERROR: open sub/extra file.yaml: no such file or directory\n\
             ERROR: failed to stat secrets/token: stat secrets/token: no such file or directory\n",
            &boot,
            &cwd,
        );

        assert_eq!(
            found,
            vec![cwd.join("secrets/token"), cwd.join("sub/extra file.yaml")]
        );
    }

    #[test]
    fn bootstrap_maps_absolute_remote_paths_back_to_the_local_tree() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        std::fs::write(cwd.join("extra.hcl"), "target \"api\" {}\n").unwrap();
        let boot = Boot {
            abs: "/home/builder/.ulak/workspaces/id/bake-bootstrap.1".into(),
            rel: ".ulak/workspaces/id/bake-bootstrap.1".into(),
        };
        let remote = format!("{}{}", boot.abs, cwd.join("extra.hcl").display());

        assert_eq!(
            missing_local_paths(
                &format!("ERROR: open {remote}: no such file or directory"),
                &boot,
                &cwd,
            ),
            vec![cwd.join("extra.hcl")]
        );
    }

    #[test]
    fn the_command_path_before_the_tail_is_never_read_as_an_argument() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        // A file literally named `bake` in front of the tail must not
        // become a `-f` value or a target.
        let args = v(&["buildx", "bake", "--metadata-file", "meta.json"]);
        assert_eq!(
            metadata_file(&cwd, &args, 2),
            Some(cwd.join("meta.json")),
            "the tail begins after `buildx bake`"
        );
        assert_eq!(
            metadata_file(&cwd, &args, 4),
            None,
            "nothing before tail_start is ever scanned"
        );
    }

    #[test]
    fn the_metadata_file_is_found_in_both_spellings() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        for spelling in [
            v(&["bake", "--metadata-file", "out/meta.json"]),
            v(&["bake", "--metadata-file=out/meta.json"]),
        ] {
            assert_eq!(
                metadata_file(&cwd, &spelling, 1),
                Some(cwd.join("out/meta.json")),
                "{spelling:?}"
            );
        }
        assert_eq!(metadata_file(&cwd, &v(&["bake"]), 1), None);
    }

    #[test]
    fn a_path_that_does_not_exist_yet_still_normalises() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(cwd.join("out")).unwrap();
        // `out` exists and `out/api/img` does not: the existing part is
        // canonicalized and the rest is appended, so this compares equal
        // to the same directory reached by any other spelling.
        assert_eq!(for_write(&cwd, "out/api/img"), cwd.join("out/api/img"));
        assert_eq!(for_write(&cwd, "./out/./api/../api"), cwd.join("out/api"));
        // An absolute dest is taken as given rather than joined to cwd.
        // Note the canonicalizing: on macOS `/tmp` is a symlink to
        // `/private/tmp`, and resolving it is the whole point — it is
        // what lets a write compare equal to the same path reached as a
        // read, so the caller's dedup catches the pair.
        let absolute = cwd.join("out/deep/img");
        assert_eq!(
            for_write(&cwd, absolute.to_str().unwrap()),
            cwd.join("out/deep/img")
        );
    }
}
