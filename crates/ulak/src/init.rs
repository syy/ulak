//! `ulak init [host]` — create an Ulak project config.
//!
//! A hand-written `ulak.toml` is equally valid; init is only a convenient
//! local scaffolder. When init was given a Compose invocation, its doctor
//! next step repeats that invocation because no stack declaration exists yet
//! to recover it from.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{self, PROJECT_CONFIG};
use crate::ui::{self, fail};

pub fn run(host_arg: Option<String>, globals: &[String]) -> Result<()> {
    // Compose is optional. When it is present, its project directory is
    // the natural home for ulak.toml; otherwise the current workspace
    // (or simply cwd) is enough to start using daemon-only commands.
    let command_args = host_arg.iter().cloned().collect::<Vec<_>>();
    let (root, doctor) = match crate::management::locate(globals, "init", &command_args)? {
        crate::management::Context::Project(project) => {
            let doctor = crate::management::root_command(&project, &["doctor"]);
            (project.config_home, Some(doctor))
        }
        crate::management::Context::Workspace(workspace) => (workspace.root, None),
        crate::management::Context::Outside => (config::Workspace::for_init()?.root, None),
    };
    let host = match host_arg.as_deref() {
        Some(raw) if raw.trim().is_empty() => {
            return Err(fail!("SSH host cannot be empty")
                .now("run: ulak init <ssh-host>, or omit the host to create a template")
                .into_err());
        }
        Some(raw) => Some(config::validate_ssh_dest(raw)?),
        None => None,
    };

    write_project_layer(&root, host.as_deref())?;

    if let Some(doctor) = doctor {
        ui::dim(&format!(
            "next: {doctor}   (checks the server and your compose file)"
        ));
    } else {
        ui::info("no Compose file found — the workspace config is still ready");
        ui::dim("next: ulak doctor, or add a Compose file later");
    }
    Ok(())
}

fn write_project_layer(root: &Path, host: Option<&str>) -> Result<()> {
    let path = root.join(PROJECT_CONFIG);
    if path.exists() {
        if let Some(host) = host {
            return Err(fail!("{} already exists", path.display())
                .now(format!(
                    "host \"{host}\" was not applied; edit the existing file explicitly"
                ))
                .into_err());
        }
        ui::dim(&format!(
            "project    {} already exists — left as is",
            path.display()
        ));
        return Ok(());
    }

    let host_line = match host {
        Some(host) => format!("host = {}\n", toml::Value::String(host.to_string())),
        None => {
            "# host = \"my-server\"  # optional when a global default is configured\n".to_string()
        }
    };
    let text = format!(
        "\
# Ulak project settings. Commit this file when these choices belong to
# the project; use ulak.local.toml for any checkout-specific overrides.

{host_line}
[workspace]
# Optional client label for transported files. With no value Ulak creates
# one stable client-… namespace in local state, so two machines using the
# same SSH account and checkout path do not collide. Prefer a global
# config or ulak.local.toml for a human-readable per-client value.
#namespace = \"alice-laptop\"

[sync]
# Extra ignore patterns on top of .gitignore (gitignore syntax).
# Excluded paths are never pushed AND never deleted on the server — a
# container may have created them there (e.g. node_modules built
# remotely). To reset the workspace wholesale: ulak clean.
#exclude = [\"node_modules\", \".venv\", \"target\"]

# Server-owned data: never pushed, pulled, or deleted. Protect database
# files and uploads that must stay on the server. Leave generated source
# files unprotected to copy them back. `ulak doctor` suggests candidates.
#protect = [\"data/\", \"uploads/\"]

# Files compose needs on the server even though .gitignore hides them.
#include = [\".env\"]          # already the default

# Server-side deletions one sync may perform without asking.
#max_delete = 25

[forward]
# Bring every compose-published port home to localhost while the stack
# is up. The service opens the tunnels; set this to false and nothing on
# this project is forwarded.
auto = true
"
    );

    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    if let Some(host) = host {
        ui::ok(&format!(
            "project    {} — host = \"{host}\"",
            path.display()
        ));
    } else {
        ui::ok(&format!("project    {}", path.display()));
        ui::dim("add host here, in ulak.local.toml, or in your global config");
    }
    Ok(())
}

/// Ulak being on this machine is what gives this machine a global
/// config.
///
/// `[service]` is read from this layer ALONE, so until the file existed
/// the only way to turn the background service off was an environment
/// variable that does not survive the shell — an off switch that lived in
/// a file nobody was told to create. `agent::ensure` already makes the
/// same bargain one line further down, for the same reason: `cargo
/// install` and Homebrew have no post-install hook, so the first run of
/// the binary is the moment we have.
///
/// Gated on a terminal, exactly as `agent::ensure` is: this writes into
/// the user's home directory, and doing that where nobody can read the
/// sentence explaining it would be a surprise. A test, a CI job and a
/// container therefore cannot grow a `$HOME` file through ulak by
/// construction rather than by convention.
///
/// Failing to write it is not worth failing a command over. The user
/// typed `up`, not `init`.
pub fn ensure_global() {
    if !std::io::stderr().is_terminal() {
        return;
    }
    if !config::global_config_auto() {
        return;
    }
    let Some(path) = config::global_config_path() else {
        return;
    };
    match write_global_layer(&path) {
        Ok(false) => {}
        Ok(true) => {
            ui::info(&format!("global config written: {}", path.display()));
            ui::dim("machine-wide defaults; every line is commented out — see: ulak config");
        }
        Err(e) => {
            ui::warn(&format!("could not write {}", path.display()));
            ui::dim(&ui::flatten(&e));
            ui::dim("ulak works without it; never offer again with ULAK_GLOBAL_CONFIG_AUTO=false");
        }
    }
}

/// Every line commented, unlike the project template, and that difference
/// is the whole licence for writing this file uninvited: a config nobody
/// asked for must not change what ulak does by existing. Uncommenting is
/// the user's act. `[service] notify` is deliberately absent — it is
/// parsed but unread, and a template that advertises it would be
/// inventory that lies.
///
/// Returns whether it wrote. A file that is already there is the user's
/// and is never touched.
fn write_global_layer(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let text = "\
# Ulak global settings — this MACHINE, every project.
#
# Ulak wrote this on its first run so the machine-wide settings have a
# visible home. Nothing here is active until you uncomment a line.
#
# Layers, later wins: this file, then <project>/ulak.toml, then
# <project>/ulak.local.toml, then ULAK_* variables.
# What is in effect, and which layer won it:  ulak config
#
# Never write this file again:  ULAK_GLOBAL_CONFIG_AUTO=false

# The server projects use when they name none of their own.
#host = \"my-server\"

[service]
# The background service: it holds the tunnels of every stack you left
# up, re-syncs after a suspend and returns after a reboot. Read ONLY
# from this file — no project decides what one machine's daemon does.
#auto = true

[workspace]
# Client label for transported files. With no value Ulak keeps one
# stable client-… namespace in local state, so two machines sharing an
# SSH account and a checkout path do not collide on the server.
#namespace = \"alice-laptop\"

[sync]
# Extra ignore patterns on top of .gitignore, for every project.
#exclude = [\"node_modules\", \".venv\", \"target\"]

# Server-side deletions one sync may perform without asking.
#max_delete = 25

[forward]
# Bring every compose-published port home to localhost while a stack is
# up. Set false and nothing on this machine is forwarded.
#auto = true
";
    // 0600 for `invocation::write_private`'s reason: ulak wrote it into
    // someone's home, and a host line names an account.
    crate::invocation::write_private(path, text.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_template_contains_host_when_given() {
        let tmp = tempfile::tempdir().unwrap();
        write_project_layer(tmp.path(), Some("me@dev-box")).unwrap();

        let text = std::fs::read_to_string(tmp.path().join(PROJECT_CONFIG)).unwrap();
        assert!(text.contains("host = \"me@dev-box\""));
        assert!(text.contains("[workspace]"));
        assert!(text.contains("#namespace = \"alice-laptop\""));
        assert!(text.contains("[sync]"));
        assert!(text.contains("[forward]"));
        assert!(!tmp.path().join("ulak.local.toml").exists());
        assert!(!tmp.path().join(".gitignore").exists());
    }

    #[test]
    fn project_template_can_use_a_global_host_or_be_completed_later() {
        let tmp = tempfile::tempdir().unwrap();
        write_project_layer(tmp.path(), None).unwrap();

        let text = std::fs::read_to_string(tmp.path().join(PROJECT_CONFIG)).unwrap();
        assert!(text.contains("# host = \"my-server\""));
        assert!(toml::from_str::<toml::Value>(&text).is_ok());
    }

    #[test]
    fn an_existing_project_config_does_not_swallow_a_host_argument() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(PROJECT_CONFIG);
        std::fs::write(&path, "host = \"keep-me\"\n").unwrap();

        let err = write_project_layer(tmp.path(), Some("replace-me")).unwrap_err();

        let said = ui::flatten(&err);
        assert!(said.contains("already exists"));
        assert!(said.contains("was not applied"));
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "host = \"keep-me\"\n"
        );
    }

    /// The whole licence for writing this file uninvited is that it
    /// changes nothing. Compared as complete Debug renderings rather than
    /// field by field: `Config` has no `PartialEq`, and a hand-listed set
    /// of fields stops covering the template the day a setting joins it —
    /// the shape of green-over-nothing this repo keeps paying for.
    #[test]
    fn the_global_template_resolves_to_exactly_the_built_in_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        assert!(write_global_layer(&path).unwrap(), "the first call writes");

        let env = std::collections::BTreeMap::new();
        let bare = config::resolve_layers_in(tmp.path(), None, &env).unwrap();
        let seeded = config::resolve_layers_in(tmp.path(), Some(&path), &env).unwrap();

        assert!(
            seeded
                .layers
                .iter()
                .all(|l| !matches!(l.state, config::LayerState::Broken(_))),
            "a template ulak writes must parse under deny_unknown_fields"
        );
        assert_eq!(
            format!("{:?}", seeded.config),
            format!("{:?}", bare.config),
            "a file nobody asked for must not move a single setting"
        );
    }

    /// `ensure_global` runs before EVERY command, so a template that
    /// overwrote would erase a machine's settings on the next `up`.
    #[test]
    fn an_existing_global_config_is_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "host = \"mine\"\n").unwrap();

        assert!(
            !write_global_layer(&path).unwrap(),
            "the second call writes nothing"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "host = \"mine\"\n");
    }

    /// `ui.rs` forbids dead ends, and a file that appeared uninvited is
    /// one unless it says how to stop it appearing.
    #[test]
    fn the_template_names_the_switch_that_stops_it_coming_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        write_global_layer(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("ULAK_GLOBAL_CONFIG_AUTO=false"), "{text}");
        assert!(text.contains("ulak config"), "{text}");
        // Parsed but unread: advertising it would be inventory that lies.
        assert!(!text.contains("notify"), "{text}");
    }

    /// The template is documentation Ulak writes into someone's home, and
    /// for a while it said the project layers lived under
    /// `<project>/.config/` — a place nothing reads. A user followed it,
    /// and then could not tell which of two files named their server. The
    /// sentence is read back against the names the code actually opens.
    #[test]
    fn the_global_template_names_the_layer_files_the_code_actually_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        write_global_layer(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let layers = text
            .lines()
            .find(|l| l.contains("Layers, later wins"))
            .expect("the layer sentence to still be in the template");
        assert!(
            layers.contains(&format!("<project>/{}", config::PROJECT_CONFIG)),
            "{layers}"
        );
        assert!(
            !text.contains(".config/ulak"),
            "the template must not point at a path no layer reads:\n{text}"
        );
    }
}
