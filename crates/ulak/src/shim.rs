//! The stand-in `docker`: how a project's own entry points reach the
//! server without being edited.
//!
//! A project rarely types `docker` at a prompt. Its entry points do — a
//! start script, a make target, a task runner, an installer — and they
//! spell it `docker`, because that is its name on the machine they were
//! written for. Rewriting every one of them is not a trade anybody makes
//! to try a tool out, so those scripts keep reaching whichever daemon is
//! local: the exact split this product exists to close, arriving through
//! the project's own front door.
//!
//! So Ulak writes a `docker` of its own and lets the user put it in
//! front. It is a four-line shell script that calls `ulak docker`, and
//! that is the entire mechanism — a drop-in replacement, not an
//! interception layer.
//!
//! NOTHING HAPPENS WITHOUT BEING ASKED, and that is the design rather
//! than a detail. `install` is typed; `uninstall` is typed; a machine
//! where neither was typed is untouched. The PATH line goes into one
//! marked block that `uninstall` removes exactly, so "go back to the
//! local Docker" is never an archaeology exercise in somebody's shell
//! profile. Ulak edits that profile only after `ui::confirm` says yes,
//! which means never from a script, because `confirm` answers no without
//! a terminal. A partial or duplicated fence is refused before any byte
//! changes; uninstall removes the profile reference before its executable,
//! and an atomic rewrite follows a dotfile-manager symlink rather than
//! replacing it. Those three rules keep a failed cleanup from turning the
//! next shell's `docker` into a dead command or eating the rest of a profile.
//!
//! Both spellings are written. The hyphenated one predates the CLI
//! plugin and survives in installers and older task runners, which is
//! precisely the kind of script this exists to carry; writing one and
//! not the other would carry half a project.
//!
//! Why there is no defence against recursion. With the directory on a
//! lasting PATH, a `docker` that Ulak itself spawned would find the
//! stand-in and loop. It cannot: Ulak reaches Docker over ssh and never
//! runs a local one. That is an invariant rather than a coincidence, so
//! it is pinned by a test that reads this crate's own source
//! (`no_production_code_spawns_a_local_docker`) instead of by a fragile
//! guard inside the script.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;

use crate::ssh::sh_quote;
use crate::ui::{self, fail};

/// The names a project's own scripts use to reach Docker, and the Ulak
/// command each one stands for.
const SPELLINGS: &[(&str, &str)] = &[("docker", ""), ("docker-compose", "compose")];

/// The fences around the lines Ulak owns in a shell profile. `uninstall`
/// removes exactly what lies between them and nothing else, so a user who
/// edited the line by hand still gets their file back intact around it.
const BEGIN: &str = "# >>> ulak shim >>>";
const END: &str = "# <<< ulak shim <<<";

// ─── the commands ──────────────────────────────────────────────────────

pub fn install() -> Result<ExitCode> {
    let dir = write_stand_ins()?;
    ui::ok(&format!(
        "wrote {} stand-in(s): {}",
        SPELLINGS.len(),
        dir.display()
    ));

    let Some(shell) = Shell::detect() else {
        ui::warn("this shell is not one Ulak knows how to edit, so it did not touch anything");
        ui::dim("put this on your PATH by hand, in whatever your shell reads at startup:");
        ui::dim(&format!("    {}", Shell::Posix.export_line(&dir)?));
        return Ok(ExitCode::SUCCESS);
    };

    let rc = shell.rc(&crate::config::home_dir()?);
    let line = shell.export_line(&dir)?;
    let existing = read_or_empty(&rc)?;

    if checked_profile(&rc, block_of(&existing))? == Some(line.trim()) {
        ui::ok(&format!("already on your PATH, from {}", rc.display()));
        return Ok(ExitCode::SUCCESS);
    }

    ui::info(&format!("{} would gain:", rc.display()));
    for l in block(&line).lines() {
        ui::dim(&format!("    {l}"));
    }
    if !ui::confirm(&format!("add it to {}?", rc.display()))? {
        ui::info("left your shell alone — add this line yourself when you want it:");
        ui::dim(&format!("    {line}"));
        return Ok(ExitCode::SUCCESS);
    }

    let updated = checked_profile(&rc, with_block(&existing, &line))?;
    write_preserving(&rc, &updated)?;
    ui::ok(&format!("added the block to {}", rc.display()));
    if let Some(reload) = shell.reload(&rc) {
        ui::dim("open a new shell, or load it here:");
        ui::dim(&format!("    {reload}"));
    } else {
        ui::dim("open a new shell to use it");
    }
    ui::dim("take it back at any time: ulak shim uninstall");
    Ok(ExitCode::SUCCESS)
}

pub fn uninstall() -> Result<ExitCode> {
    let dir = shim_dir()?;
    // Not asked again: `uninstall` IS the answer to that question, and
    // the only lines touched are the ones between Ulak's own fences.
    //
    // The profile goes FIRST. Removing the executable and then failing
    // to rewrite this file leaves `docker` resolving to a path that no
    // longer exists in every new shell — worse than an uninstall that
    // stopped safely and left the working stand-in in place.
    if let Some(shell) = Shell::detect() {
        let rc = shell.rc(&crate::config::home_dir()?);
        let existing = read_or_empty(&rc)?;
        if checked_profile(&rc, block_of(&existing))?.is_some() {
            let updated = checked_profile(&rc, without_block(&existing))?;
            write_preserving(&rc, &updated)?;
            ui::ok(&format!("removed the block from {}", rc.display()));
            ui::dim("this shell still has the old PATH — open a new one");
        } else {
            ui::info(&format!("{} had no Ulak block", rc.display()));
        }
    } else {
        ui::dim("if you added a PATH line by hand, remove it yourself");
    }

    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| {
            fail!("cannot remove {}: {e}", dir.display())
                .now("remove it by hand, then rerun: ulak shim uninstall")
                .into_err()
        })?;
        ui::ok(&format!("removed {}", dir.display()));
    } else {
        ui::info("there were no stand-ins to remove");
    }
    Ok(ExitCode::SUCCESS)
}

pub fn status() -> Result<ExitCode> {
    let dir = shim_dir()?;
    let s = ui::style_stdout();
    println!("{}ulak shim{}", s.bold, s.off);

    let written: Vec<&str> = SPELLINGS
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| dir.join(n).exists())
        .collect();
    if written.is_empty() {
        println!("  no stand-ins written — install them: ulak shim install");
    } else {
        println!("  written    {} ({})", dir.display(), written.join(", "));
    }

    // A stand-in names the binary that wrote it, by absolute path. Move
    // or replace that binary and every `docker` in every script becomes
    // "not found" — a dead end whose cause is in a file nobody thinks to
    // open. Say it here instead.
    let stale = std::fs::read_to_string(dir.join("docker"))
        .ok()
        .and_then(|body| target_of(&body))
        .filter(|target| !Path::new(target).exists());
    if let Some(gone) = stale {
        ui::warn(&format!(
            "the stand-ins call {gone}, which is not there any more — every `docker` in a script will say \"not found\""
        ));
        ui::dim("point them at this ulak: ulak shim install");
    }

    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path = path_entries(&path).any(|p| p == dir);
    println!(
        "  on PATH    {}",
        if on_path { "yes" } else { "no (this shell)" }
    );

    // The question a user actually has, answered the way the shell will
    // answer it: whatever comes first wins.
    match first_on_path("docker", &path) {
        Some(found) if found.starts_with(&dir) => {
            println!(
                "  docker     {} {}→{} the server",
                found.display(),
                s.dim,
                s.off
            );
        }
        Some(found) => println!("  docker     {}", found.display()),
        None => println!("  docker     nothing named docker is on this PATH"),
    }
    Ok(ExitCode::SUCCESS)
}

/// Run one command with the stand-ins in front, installing nothing.
pub fn run(argv: Vec<OsString>) -> Result<ExitCode> {
    let Some((program, rest)) = argv.split_first() else {
        return Err(fail!("there is no command to run")
            .now("name one after `--`, e.g. ulak shim run -- ./scripts/start.sh")
            .into_err());
    };
    let dir = write_stand_ins()?;
    let inherited = std::env::var_os("PATH").filter(|p| !p.is_empty());
    let path = path_with_shim(&dir, inherited.as_deref()).map_err(|e| {
        fail!("cannot put {} on PATH: {e}", dir.display())
            .now("set XDG_STATE_HOME to a path without ':' and retry")
            .into_err()
    })?;
    ui::dim("docker in this command goes to the server");
    let status = std::process::Command::new(program)
        .args(rest)
        .env("PATH", &path)
        .status()
        .map_err(|e| {
            fail!("cannot run {}: {e}", Path::new(program).display())
                .now("check that it exists and is executable from this directory")
                .into_err()
        })?;
    crate::docker::status_code(status)
}

// ─── writing the stand-ins ─────────────────────────────────────────────

fn shim_dir() -> Result<PathBuf> {
    Ok(crate::invocation::state_dir_required()?.join("shim"))
}

/// Write both stand-ins and return the directory holding them.
fn write_stand_ins() -> Result<PathBuf> {
    let me = std::env::current_exe().map_err(|e| {
        fail!("cannot locate this ulak binary, so the stand-in has nothing to call: {e}")
            .now("run ulak by its installed path rather than through a shell function or alias")
            .into_err()
    })?;
    let ulak = me.to_str().ok_or_else(|| {
        fail!("this ulak binary's path is not valid UTF-8, so no stand-in can call it")
            .now("install ulak somewhere its path is plain text")
            .into_err()
    })?;

    let dir = shim_dir()?;
    crate::invocation::private_dir(&dir).map_err(|e| {
        fail!("cannot create {}: {e}", dir.display())
            .now("check that the state directory is writable")
            .into_err()
    })?;
    for (name, subcommand) in SPELLINGS {
        write_runnable(&dir.join(name), &script(ulak, subcommand))?;
    }
    Ok(dir)
}

/// `write_private` owns the 0600 create; the execute bit is added here
/// rather than there because every other file Ulak writes is data and
/// this is the one that has to run. The directory is already 0700, so
/// widening the file does not widen who can reach it.
fn write_runnable(path: &Path, body: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    crate::invocation::write_private(path, body.as_bytes())
        .and_then(|()| std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)))
        .map_err(|e| {
            fail!("cannot write {}: {e}", path.display())
                .now("check that the state directory is writable")
                .into_err()
        })
}

/// One stand-in's body.
///
/// `sh_quote` is `ssh.rs`'s, and the reuse is exact rather than
/// approximate: both jobs are "make one word survive a POSIX shell
/// verbatim", and a second quoter would be a second answer to it.
fn script(ulak: &str, subcommand: &str) -> String {
    let mut call = format!("exec {} docker", sh_quote(ulak));
    if !subcommand.is_empty() {
        call.push(' ');
        call.push_str(subcommand);
    }
    format!(
        "#!/bin/sh\n\
         # Written by `ulak shim install`. Remove it with `ulak shim uninstall`.\n\
         {call} \"$@\"\n"
    )
}

/// The binary a stand-in calls, read back out of its own `exec` line.
///
/// This undoes exactly what `sh_quote` does — a bare word, or a
/// single-quoted one where an embedded quote is written `'\''` — and
/// nothing else. It is not a shell parser and must not grow into one:
/// the only strings it ever reads are ones `script` wrote.
fn target_of(body: &str) -> Option<String> {
    let rest = body.lines().find_map(|l| l.strip_prefix("exec "))?;
    let Some(quoted) = rest.strip_prefix('\'') else {
        return Some(rest.split(' ').next()?.to_string());
    };
    let mut out = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        if c != '\'' {
            out.push(c);
            continue;
        }
        let mut probe = chars.clone();
        if probe.next() == Some('\\') && probe.next() == Some('\'') && probe.next() == Some('\'') {
            out.push('\'');
            chars = probe;
            continue;
        }
        return Some(out);
    }
    None
}

// ─── the shell profile ─────────────────────────────────────────────────

/// The shells Ulak will edit, and the one spelling it falls back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shell {
    Zsh,
    Bash,
    Fish,
    /// Not detected from `$SHELL`: the wording used when telling a user
    /// to add the line themselves.
    Posix,
}

impl Shell {
    fn detect() -> Option<Shell> {
        let shell = std::env::var_os("SHELL")?;
        Shell::from_name(Path::new(&shell).file_name()?.to_str()?)
    }

    fn from_name(name: &str) -> Option<Shell> {
        match name {
            "zsh" => Some(Shell::Zsh),
            "bash" => Some(Shell::Bash),
            "fish" => Some(Shell::Fish),
            _ => None,
        }
    }

    fn rc(&self, home: &Path) -> PathBuf {
        match self {
            Shell::Zsh => home.join(".zshrc"),
            Shell::Bash => home.join(".bashrc"),
            Shell::Fish => home.join(".config/fish/config.fish"),
            Shell::Posix => home.join(".profile"),
        }
    }

    /// Fish is not a POSIX shell and `export VAR=…` is a syntax error in
    /// it, so the line is generated rather than templated. The path is a
    /// shell WORD, never display text: a home directory containing a
    /// space, quote or `$` must not split, expand or become syntax when
    /// the next shell starts. `:` is refused because Unix PATH has no
    /// escape for its separator — quoting does not change that meaning.
    fn export_line(&self, dir: &Path) -> Result<String> {
        let raw = profile_path_entry(dir)?;
        Ok(match self {
            Shell::Fish => format!("set -gx PATH {} $PATH", fish_quote(raw)),
            _ => format!("export PATH={}:\"$PATH\"", sh_quote(raw)),
        })
    }

    fn reload(&self, rc: &Path) -> Option<String> {
        let raw = rc.to_str()?;
        if raw.contains(['\n', '\r']) {
            return None;
        }
        Some(match self {
            Shell::Fish => format!("source {}", fish_quote(raw)),
            _ => format!(". {}", sh_quote(raw)),
        })
    }
}

fn profile_path_entry(path: &Path) -> Result<&str> {
    let raw = path.to_str().ok_or_else(|| {
        fail!(
            "{} is not valid UTF-8, so it cannot be written into a shell profile",
            path.display()
        )
        .now("set XDG_STATE_HOME to a plain-text path and retry")
        .into_err()
    })?;
    if raw.contains([':', '\n', '\r']) {
        return Err(fail!(
            "{} cannot be one PATH entry because its name contains ':' or a newline",
            path.display()
        )
        .now("set XDG_STATE_HOME to a path without ':' or newlines and retry")
        .into_err());
    }
    Ok(raw)
}

/// Fish single quotes preserve everything except a quote and a
/// backslash; those two are escaped inside the quotes. POSIX quoting is
/// owned by `ssh::sh_quote`, but fish deliberately has different
/// grammar and therefore a different, tiny encoder.
fn fish_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn block(line: &str) -> String {
    format!("{BEGIN}\n{line}\n{END}\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockSpan {
    start: usize,
    body_start: usize,
    body_end: usize,
    end: usize,
}

/// Locate exactly one complete block without changing a byte.
///
/// A missing end fence is not "the block runs to EOF": the rest of a
/// shell profile belongs to the user. The old line-walker made that
/// assumption in both install and uninstall and silently discarded
/// everything after a stray begin fence. More than one block is also
/// refused; choosing one to keep would be another guess about user data.
fn block_span(existing: &str) -> std::result::Result<Option<BlockSpan>, &'static str> {
    let mut offset = 0;
    let mut open: Option<(usize, usize)> = None;
    let mut found = None;
    for line in existing.split_inclusive('\n') {
        let start = offset;
        let end = start + line.len();
        offset = end;
        match line.trim() {
            BEGIN => {
                if open.is_some() {
                    return Err("an opening fence appears before the previous block ends");
                }
                if found.is_some() {
                    return Err("more than one Ulak shim block is present");
                }
                open = Some((start, end));
            }
            END => {
                let Some((block_start, body_start)) = open.take() else {
                    return Err("an ending fence appears without an opening fence");
                };
                found = Some(BlockSpan {
                    start: block_start,
                    body_start,
                    body_end: start,
                    end,
                });
            }
            _ => {}
        }
    }
    if open.is_some() {
        return Err("an opening fence has no ending fence");
    }
    Ok(found)
}

/// The line Ulak currently owns in this file, if any.
fn block_of(existing: &str) -> std::result::Result<Option<&str>, &'static str> {
    Ok(block_span(existing)?.map(|span| existing[span.body_start..span.body_end].trim()))
}

/// Replace Ulak's block if it is there, append it if it is not.
///
/// Idempotent on purpose: `install` is the command people run again after
/// moving the binary, and a second block would leave the older PATH entry
/// winning while the newer one looked correct.
fn with_block(existing: &str, line: &str) -> std::result::Result<String, &'static str> {
    let fresh = block(line);
    if let Some(span) = block_span(existing)? {
        let mut out = String::with_capacity(existing.len() - (span.end - span.start) + fresh.len());
        out.push_str(&existing[..span.start]);
        out.push_str(&fresh);
        out.push_str(&existing[span.end..]);
        return Ok(out);
    }
    let mut out = existing.to_string();
    if !out.is_empty() {
        // One added newline is enough: after an existing newline it is
        // the blank separator; after a non-newline it only terminates
        // that last line. The distinction lets `without_block` restore
        // whether the original file ended in a newline.
        out.push('\n');
    }
    out.push_str(&fresh);
    Ok(out)
}

/// Remove Ulak's block and the blank line it was given, leaving every
/// other line exactly where it was.
fn without_block(existing: &str) -> std::result::Result<String, &'static str> {
    let Some(span) = block_span(existing)? else {
        return Ok(existing.to_string());
    };
    let mut start = span.start;
    let before = &existing[..start];
    // `with_block` gives an appended block one empty separator line.
    // Take back exactly that one, never every blank line the user kept.
    if before.ends_with("\r\n\r\n") {
        start -= 2;
    } else if before.ends_with('\n') {
        start -= 1;
    }
    let mut out = String::with_capacity(existing.len() - (span.end - start));
    out.push_str(&existing[..start]);
    out.push_str(&existing[span.end..]);
    Ok(out)
}

fn checked_profile<T>(path: &Path, result: std::result::Result<T, &str>) -> Result<T> {
    result.map_err(|why| {
        fail!(
            "{} has a malformed Ulak shim block: {why}",
            path.display()
        )
        .now(format!(
            "leave every other line intact; keep one complete {BEGIN:?} through {END:?} block, or remove the stray fence, then retry"
        ))
        .into_err()
    })
}

fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // `read_to_string` says NotFound for both an absent profile
            // and a dangling symlink. The first may be created; replacing
            // the second would silently destroy the link and leave the
            // dotfiles file it was meant to name untouched.
            match std::fs::symlink_metadata(path) {
                Err(meta) if meta.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
                Ok(meta) if meta.file_type().is_symlink() => Err(fail!(
                    "cannot read {}: its symlink target is missing",
                    path.display()
                )
                .now("restore the target, or remove the dangling symlink, then rerun")
                .into_err()),
                _ => Err(fail!("cannot read {}: {e}", path.display())
                    .now("check that you can read it, then rerun")
                    .into_err()),
            }
        }
        Err(e) => Err(fail!("cannot read {}: {e}", path.display())
            .now("check that you can read it, then rerun")
            .into_err()),
    }
}

/// Write a file somebody else owns: tmp plus rename so a shell starting
/// mid-write never sources half a profile, and the mode it already had is
/// kept — a profile is not Ulak's state, and quietly narrowing it to 0600
/// would be a change nobody asked for.
fn write_preserving(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    // Renaming over a symlink replaces the LINK, not the file it names.
    // Dotfile managers commonly make ~/.zshrc a symlink, so follow it
    // first and put the atomic sibling beside the real target.
    let target = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(path).map_err(|e| {
            fail!(
                "cannot follow {} to the profile it names: {e}",
                path.display()
            )
            .now("restore its symlink target, then rerun")
            .into_err()
        })?,
        Ok(_) => path.to_path_buf(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => {
            return Err(fail!("cannot inspect {}: {e}", path.display())
                .now("check that you can read and write it, then rerun")
                .into_err());
        }
    };
    let mode = std::fs::metadata(&target)
        .map(|m| m.permissions().mode() & 0o7777)
        .ok()
        .unwrap_or(0o644);
    let tmp = target.with_extension(format!("ulak-tmp.{}", std::process::id()));
    let mut created = false;
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // The profile may itself be 0600 and contain secrets. Give the
        // temp its final mode at CREATE time; create-then-chmod leaves a
        // wider file visible until the second syscall.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        created = true;
        // `mode` only affects a newly-created inode. `create_new` makes
        // that guarantee explicit and also refuses a stale temp or a
        // symlink at this predictable name rather than following it.
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.write_all(contents.as_bytes())?;
        std::fs::rename(&tmp, &target)
    })();
    result.map_err(|e| {
        if created {
            let _ = std::fs::remove_file(&tmp);
        }
        fail!("cannot write {}: {e}", path.display())
            .now(format!(
                "check that you can write it; if no other shim command is running, remove {} and retry",
                tmp.display()
            ))
            .into_err()
    })
}

// ─── reading PATH the way the shell reads it ───────────────────────────

fn path_entries(path: &OsStr) -> impl Iterator<Item = PathBuf> + '_ {
    std::env::split_paths(path)
}

fn path_with_shim(
    dir: &Path,
    inherited: Option<&OsStr>,
) -> std::result::Result<OsString, std::env::JoinPathsError> {
    let mut entries = vec![dir.to_path_buf()];
    if let Some(path) = inherited {
        entries.extend(path_entries(path));
    }
    std::env::join_paths(entries)
}

/// The first entry a shell would run for `name`, or `None`.
fn first_on_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    path_entries(path).find_map(|dir| {
        let candidate = dir.join(name);
        let runnable = std::fs::metadata(&candidate)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        runnable.then_some(candidate)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stand-in must call Ulak, not whatever `docker` the PATH would
    /// have found next — otherwise it forwards to the local daemon, which
    /// is the failure this command exists to remove.
    #[test]
    fn a_stand_in_calls_ulak_rather_than_the_next_docker_on_the_path() {
        let body = script("/opt/ulak", "");
        assert!(body.contains("exec /opt/ulak docker \"$@\""), "{body}");
        assert!(!body.contains("exec docker"), "{body}");
    }

    /// The hyphenated spelling is a different command word, not a
    /// different binary: it has to become `docker compose`, or every
    /// script written before the plugin loses its subcommand.
    #[test]
    fn the_hyphenated_spelling_becomes_the_compose_subcommand() {
        let body = script("/opt/ulak", "compose");
        assert!(
            body.contains("exec /opt/ulak docker compose \"$@\""),
            "{body}"
        );
    }

    /// A path with a space in it is one word to the shell only if it is
    /// quoted; unquoted, the stand-in calls a program that is not there
    /// and the script sees "not found" for a Docker that exists.
    #[test]
    fn a_binary_path_with_a_space_survives_the_shell() {
        let body = script("/Applications/My Tools/ulak", "");
        assert!(
            body.contains("exec '/Applications/My Tools/ulak' docker"),
            "{body}"
        );
    }

    /// `status` warns when the binary a stand-in calls is gone, and it
    /// can only do that by reading the path back out. Round-tripping it
    /// through `sh_quote` is the half that could silently rot: a quoter
    /// change would make every stand-in look stale, or worse, a stale one
    /// look fine.
    #[test]
    fn the_binary_a_stand_in_calls_can_be_read_back_out_of_it() {
        for path in [
            "/opt/ulak",
            "/Applications/My Tools/ulak",
            "/home/o'brien/bin/ulak",
        ] {
            assert_eq!(
                target_of(&script(path, "")).as_deref(),
                Some(path),
                "did not survive the round trip"
            );
            assert_eq!(target_of(&script(path, "compose")).as_deref(), Some(path));
        }
        assert_eq!(target_of("#!/bin/sh\n"), None);
    }

    /// Both spellings get a file. A project that reaches Docker by the
    /// older name would otherwise be carried halfway.
    #[test]
    fn both_docker_spellings_get_a_stand_in() {
        let names: Vec<&str> = SPELLINGS.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["docker", "docker-compose"]);
    }

    /// `export VAR=…` is a syntax error in fish, so a templated line
    /// would leave that shell printing an error on every startup.
    #[test]
    fn each_shell_gets_the_syntax_it_can_actually_read() {
        let dir = Path::new("/state/ulak/shim");
        assert_eq!(
            Shell::Zsh.export_line(dir).unwrap(),
            "export PATH=/state/ulak/shim:\"$PATH\""
        );
        assert_eq!(
            Shell::Bash.export_line(dir).unwrap(),
            "export PATH=/state/ulak/shim:\"$PATH\""
        );
        assert_eq!(
            Shell::Fish.export_line(dir).unwrap(),
            "set -gx PATH '/state/ulak/shim' $PATH"
        );
        assert_eq!(
            Shell::Fish.rc(Path::new("/home/a")).to_string_lossy(),
            "/home/a/.config/fish/config.fish"
        );
    }

    /// A profile is executable shell input. Rendering a path with
    /// `display()` let `$`, quotes and spaces expand or split the next
    /// time the shell started; a colon is worse, because no amount of
    /// quoting can stop PATH from reading it as two entries.
    #[test]
    fn a_path_in_a_profile_is_quoted_for_that_shell_and_never_split_on_a_colon() {
        let odd = Path::new("/state/My Tools/$prod/o'brien\\bin");
        assert_eq!(
            Shell::Zsh.export_line(odd).unwrap(),
            "export PATH='/state/My Tools/$prod/o'\\''brien\\bin':\"$PATH\""
        );
        assert_eq!(
            Shell::Fish.export_line(odd).unwrap(),
            "set -gx PATH '/state/My Tools/$prod/o\\'brien\\\\bin' $PATH"
        );
        assert!(Shell::Zsh.export_line(Path::new("/state/a:b")).is_err());
    }

    /// A shell Ulak does not know must not be guessed at: writing bash
    /// syntax into an unknown startup file is how somebody's login breaks.
    #[test]
    fn an_unknown_shell_is_not_guessed() {
        assert_eq!(Shell::from_name("zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::from_name("nu"), None);
        assert_eq!(Shell::from_name("tcsh"), None);
    }

    /// Running `install` twice is how people refresh the stand-ins after
    /// moving the binary. A second block would leave the older PATH entry
    /// winning while the newer one looked correct.
    #[test]
    fn installing_twice_replaces_the_block_rather_than_adding_one() {
        let rc = "export EDITOR=vim\n";
        let once = with_block(rc, "export PATH=\"/a:$PATH\"").unwrap();
        let twice = with_block(&once, "export PATH=\"/b:$PATH\"").unwrap();
        assert_eq!(twice.matches(BEGIN).count(), 1, "{twice}");
        assert!(twice.contains("/b"), "{twice}");
        assert!(!twice.contains("/a"), "{twice}");
        assert!(twice.contains("export EDITOR=vim"), "{twice}");
    }

    /// Uninstall must give back the file the user had. Anything it
    /// touches outside its own fences is somebody's shell configuration
    /// that Ulak was never asked to edit.
    #[test]
    fn uninstall_removes_the_block_and_leaves_every_other_line_alone() {
        let rc = "export EDITOR=vim\nalias ll='ls -la'\n";
        let installed = with_block(rc, "export PATH=\"/a:$PATH\"").unwrap();
        assert_eq!(without_block(&installed).unwrap(), rc);

        let no_final_newline = "export EDITOR=vim";
        let installed = with_block(no_final_newline, "export PATH=\"/a:$PATH\"").unwrap();
        assert_eq!(
            without_block(&installed).unwrap(),
            no_final_newline,
            "uninstall must restore whether the user's last line had a newline"
        );
    }

    /// A file that is nothing but the block comes back empty rather than
    /// as a stray blank line.
    #[test]
    fn a_profile_that_held_only_the_block_comes_back_empty() {
        let installed = with_block("", "export PATH=\"/a:$PATH\"").unwrap();
        assert_eq!(without_block(&installed).unwrap(), "");
    }

    /// `install` recognises its own line so a user who runs it twice is
    /// told nothing changed instead of being asked to confirm a no-op.
    #[test]
    fn the_line_already_in_place_is_recognised() {
        let line = "export PATH=\"/a:$PATH\"";
        let rc = with_block("export EDITOR=vim\n", line).unwrap();
        assert_eq!(block_of(&rc).unwrap(), Some(line));
        assert_eq!(block_of("export EDITOR=vim\n").unwrap(), None);
    }

    /// A hand-edited or interrupted block is ambiguous ownership, not a
    /// licence to treat the rest of somebody's profile as Ulak's. The
    /// old walker read a missing end fence as "through EOF" and both
    /// install and uninstall silently deleted every line after it.
    #[test]
    fn an_incomplete_or_duplicated_block_is_refused_without_rewriting_a_profile() {
        let broken = [
            format!("before\n{BEGIN}\nafter\n"),
            format!("before\n{END}\nafter\n"),
            format!("{BEGIN}\none\n{END}\n{BEGIN}\ntwo\n{END}\n"),
        ];
        for profile in broken {
            assert!(block_of(&profile).is_err(), "{profile:?}");
            assert!(with_block(&profile, "export PATH=/safe").is_err());
            assert!(without_block(&profile).is_err());
            assert!(profile.contains("after") || profile.contains("two"));
        }
    }

    /// Atomic replacement must target the file a dotfile-manager link
    /// names, not replace the link itself. Replacing ~/.zshrc with a
    /// regular file leaves the real dotfiles checkout unchanged and
    /// silently disconnects future updates from it.
    #[test]
    fn updating_a_symlinked_profile_keeps_the_link_and_the_targets_mode() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("managed-zshrc");
        let link = root.path().join(".zshrc");
        std::fs::write(&target, "export EDITOR=vim\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink("managed-zshrc", &link).unwrap();

        let updated = with_block(
            &read_or_empty(&link).unwrap(),
            "export PATH=/state/ulak/shim:\"$PATH\"",
        )
        .unwrap();
        write_preserving(&link, &updated).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(std::fs::read_to_string(&target).unwrap().contains(BEGIN));
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// `shim run` must prepend one PATH entry, not build PATH by string
    /// concatenation. A colon in the state path cannot be escaped in the
    /// Unix format and therefore has to fail before the child runs.
    #[test]
    fn a_one_command_path_is_joined_as_entries_and_refuses_an_unrepresentable_one() {
        let path = path_with_shim(
            Path::new("/state/ulak/shim"),
            Some(OsStr::new("/bin:/usr/bin")),
        )
        .unwrap();
        assert_eq!(
            path_entries(&path).collect::<Vec<_>>(),
            vec![
                PathBuf::from("/state/ulak/shim"),
                PathBuf::from("/bin"),
                PathBuf::from("/usr/bin")
            ]
        );
        assert!(path_with_shim(Path::new("/state/a:b"), None).is_err());
    }

    /// The profile block must be removed before its executable. If a
    /// profile write then fails (permissions, a broken symlink, a full
    /// disk), keeping the stand-in leaves `docker` working; the reverse
    /// order leaves every new shell pointing at a command that vanished.
    #[test]
    fn uninstall_never_removes_the_executable_before_the_profile_reference() {
        let mine = include_str!("shim.rs");
        let start = mine.find("pub fn uninstall()").expect("uninstall exists");
        let tail = &mine[start..];
        let body = &tail[..tail.find("\npub fn status()").expect("uninstall ends")];
        let profile = body
            .find("write_preserving(&rc")
            .expect("uninstall rewrites the profile");
        let executable = body
            .find("remove_dir_all(&dir)")
            .expect("uninstall removes the stand-ins");
        assert!(
            profile < executable,
            "a failed profile write would leave PATH pointing at a removed docker"
        );
    }

    /// `status` answers with whatever the shell would run, so it has to
    /// walk PATH in order and skip an entry that is not executable.
    #[test]
    fn the_first_runnable_entry_on_the_path_is_the_one_reported() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("ulak-shim-path-{}", std::process::id()));
        let (first, second) = (root.join("first"), root.join("second"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        // A file that is not executable is not what the shell would pick.
        std::fs::write(first.join("docker"), "").unwrap();
        std::fs::write(second.join("docker"), "").unwrap();
        std::fs::set_permissions(
            second.join("docker"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let path = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(
            first_on_path("docker", &path),
            Some(second.join("docker")),
            "an unreadable entry was treated as runnable"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The stand-in carries no guard against calling itself, because Ulak
    /// reaches Docker over ssh and never runs a local one. If that ever
    /// stops being true, a `docker` on the user's PATH becomes an
    /// infinite loop the moment `ulak shim install` is typed — so the
    /// invariant is pinned here rather than defended in shell.
    #[test]
    fn no_production_code_spawns_a_local_docker() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&src).expect("the source directory is readable") {
            let path = entry.expect("a source entry").path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("a source file");
            // Everything from the first test module on belongs to tests.
            let production = text.split("#[cfg(test)]").next().unwrap_or_default();
            for (n, line) in production.lines().enumerate() {
                // `clap::Command` builds the help tree and spawns nothing;
                // it is the one place this name is not a process.
                if line.contains("Command::new(\"docker") && !line.contains("clap::Command") {
                    offenders.push(format!("{}:{}", path.display(), n + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "production code spawns a local docker, which the installed stand-in would \
             turn into an infinite loop: {offenders:?}"
        );
    }
}
