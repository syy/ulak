//! `ulak config`: which setting is in effect, and which layer won it.
//!
//! `doctor` JUDGES — it reaches the server, weighs what it finds and
//! exits on a verdict. This one only REPORTS, from local files and the
//! environment, with no round trip. Keeping them apart is why neither
//! grows the other's job.
//!
//! Explicit root globals or owned `COMPOSE_*` input select Compose's config
//! home. Without them, the nearest Ulak config is the reporting boundary and
//! wins before Compose walks up to an unrelated ancestor file. This command
//! reports local configuration, not a stack lifecycle, so Desired is not an
//! input here.
//!
//! Two rules the rest of the file is built around:
//!
//! It runs even when a layer will not parse. Every other command refuses
//! there, and must — but a diagnostic that dies on the thing it exists to
//! diagnose is the dead end `ui.rs` forbids. So a broken layer is shown
//! with its error, the readable layers still report, and the exit code
//! carries the failure.
//!
//! It never prints a secret. The output is written to be pasted into a
//! bug report and read by an agent that logs it, so a variable marked
//! `secret` shows only that it is set. `--json` obeys the same rule; the
//! machine-readable copy of a leak is still a leak.

use std::collections::BTreeMap;
use std::process::ExitCode;

use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::config::{self, Config, LayerState, Source};
use crate::ui;

pub fn run(globals: &[String], json: bool) -> Result<ExitCode> {
    // Deliberately NOT `Project::locate`: that loads the layers itself
    // and refuses a file it cannot parse, which would make this command
    // die on exactly the state it exists to explain. Only the explicit
    // invocation or local workspace boundary is borrowed — enough to know
    // which directory's layers apply — and the merge is then run in
    // reporting mode.
    let cwd = std::env::current_dir()?
        .canonicalize()
        .context("cannot canonicalize current directory")?;
    let local_workspace = (globals.is_empty()
        && !crate::invocation::compose_environment_is_explicit())
    .then(|| config::workspace_home(&cwd))
    .flatten();
    let (name, home) = match local_workspace {
        Some(home) => (
            home.file_name()
                .map(|n| config::sanitize_name(&n.to_string_lossy())),
            home,
        ),
        None => match crate::invocation::Invocation::capture(globals) {
            Ok(inv) => (
                inv.project_dir
                    .file_name()
                    .map(|n| config::sanitize_name(&n.to_string_lossy())),
                config::find_config_home(&inv.project_dir),
            ),
            // No compose file here: still worth answering, because "what is
            // this machine set to" is a question people ask from anywhere,
            // and `status` already answers its own version of it.
            Err(e) if globals.is_empty() && config::is_projectless(&e) => (None, cwd),
            Err(e) => return Err(e),
        },
    };
    let resolved = config::resolve_layers(&home, config::global_config_path().as_deref())?;

    let env: BTreeMap<String, String> = std::env::vars().collect();
    let mut report = Report::build(name, &resolved, &env);
    // Read here, not in `build`: the declarations live in local state, and
    // the pure builder stays a function of the layers so its tests never
    // touch a state directory.
    report.declared = crate::management::declared_here()
        .into_iter()
        .map(|(identity, destination)| DeclaredRow {
            drift: match config::drift_of(&resolved.config, &destination) {
                Ok(None) => None,
                Ok(Some(config::DestinationDrift::Configured(_))) => Some("configured"),
                Ok(Some(config::DestinationDrift::Unconfigured)) => Some("unconfigured"),
                Err(_) => Some("invalid"),
            },
            identity,
            destination,
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        report.print();
    }
    Ok(exit_code(report.broken))
}

/// The header's first rule ends here: a layer that will not parse is
/// reported rather than fatal, and the exit code is then the only thing
/// left to tell a pipeline the config is unusable.
fn exit_code(broken: bool) -> ExitCode {
    if broken {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[derive(Serialize)]
struct Report {
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    layers: Vec<LayerRow>,
    /// Files at the prototype's `.config/` location, which no layer read.
    /// Absent from the JSON when there are none, so the pinned shape holds.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ignored: Vec<String>,
    /// Stacks declared for this directory. The `host` row says where the
    /// config points; these say where Ulak's own bare commands actually go
    /// while a declaration stands, and the two can differ. Absent from the
    /// JSON when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    declared: Vec<DeclaredRow>,
    settings: Vec<SettingRow>,
    environment: Vec<EnvRow>,
    #[serde(skip)]
    broken: bool,
}

#[derive(Serialize)]
struct LayerRow {
    label: &'static str,
    path: String,
    /// `loaded`, `absent`, or `broken`.
    state: &'static str,
    /// ULAK_CONFIG named this file instead of the default location.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pointed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct SettingRow {
    key: &'static str,
    value: String,
    /// `default`, a file path, or `env`.
    from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    variable: Option<&'static str>,
}

#[derive(Serialize)]
struct DeclaredRow {
    identity: String,
    destination: String,
    /// `configured` when the layers name a different server, `unconfigured`
    /// when they name none, `invalid` when the host they name could never
    /// reach OpenSSH; absent when they agree. Graded by `config::drift_of`,
    /// the same judge every route uses.
    #[serde(skip_serializing_if = "Option::is_none")]
    drift: Option<&'static str>,
}

#[derive(Serialize)]
struct EnvRow {
    name: String,
    /// Absent for a secret: the value never leaves the process.
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    /// `setting`, `ulak`, `test-hook` or `unknown`.
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<&'static str>,
}

/// A rendered line and the stream it belongs on. The table is report
/// output and goes to stdout; a layer's parse error is a diagnostic and
/// keeps `ui::dim`'s stderr, so the two cannot be one `Vec<String>`.
enum Line {
    Out(String),
    Dim(String),
}

impl Report {
    fn build(
        project: Option<String>,
        resolved: &config::Resolved,
        env: &BTreeMap<String, String>,
    ) -> Report {
        let mut broken = false;
        let layers = resolved
            .layers
            .iter()
            .map(|l| {
                let (state, error) = match &l.state {
                    LayerState::Loaded => ("loaded", None),
                    LayerState::Absent => ("absent", None),
                    LayerState::Broken(why) => {
                        broken = true;
                        ("broken", Some(why.clone()))
                    }
                };
                LayerRow {
                    label: l.label,
                    path: l.path.display().to_string(),
                    state,
                    pointed: l.pointed,
                    error,
                }
            })
            .collect();

        let settings = settings_of(&resolved.config)
            .into_iter()
            .map(|(key, value)| {
                let (from, variable) = match resolved.won.of(key) {
                    Source::Default => ("default".to_string(), None),
                    Source::File(p) => (p.display().to_string(), None),
                    Source::Env(var) => ("env".to_string(), Some(var)),
                };
                SettingRow {
                    key,
                    value,
                    from,
                    variable,
                }
            })
            .collect();

        Report {
            project,
            layers,
            ignored: resolved
                .ignored
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            declared: Vec::new(),
            settings,
            environment: environment_of(env),
            broken,
        }
    }

    fn print(&self) {
        for line in self.lines(&ui::style_stdout()) {
            match line {
                Line::Out(text) => println!("{text}"),
                Line::Dim(text) => ui::dim(&text),
            }
        }
    }

    /// The whole human form, rendered but not yet emitted. What the
    /// table decides — a newline escaped, a secret shown only as `set`,
    /// an error kept under the layer it belongs to — is testable this
    /// way and invisible inside a `println!`.
    fn lines(&self, s: &ui::Style) -> Vec<Line> {
        let mut out = Vec::new();
        out.push(Line::Out(match &self.project {
            Some(name) => format!("{}ulak config{} — {name}", s.bold, s.off),
            None => format!("{}ulak config{} — this machine", s.bold, s.off),
        }));

        out.push(Line::Out(format!("\n{}LAYERS{}", s.bold, s.off)));
        for l in &self.layers {
            let marked = if l.pointed {
                format!("{} (ULAK_CONFIG)", l.path)
            } else {
                l.path.clone()
            };
            out.push(Line::Out(format!(
                "  {:<10} {:<44} {}",
                l.label, marked, l.state
            )));
            if let Some(why) = &l.error {
                for line in why.lines() {
                    out.push(Line::Dim(format!("    {line}")));
                }
            }
        }
        // Under the layers, in the same table, because that is where a
        // user looks for the file they edited and does not find it.
        for path in &self.ignored {
            out.push(Line::Out(format!("  {:<10} {:<44} ignored", "stale", path)));
            out.push(Line::Dim(
                "    not read — Ulak's layers live at the config home itself, not under .config/; move or delete it".to_string(),
            ));
        }

        out.push(Line::Out(format!("\n{}SETTINGS{}", s.bold, s.off)));
        for row in &self.settings {
            let from = match row.variable {
                Some(var) => format!("env ({var})"),
                None => row.from.clone(),
            };
            out.push(Line::Out(format!(
                "  {:<22} {:<28} ← {from}",
                row.key, row.value
            )));
        }

        // After the settings, because it qualifies the `host` row just
        // above: this is the report the user reached for when the two
        // disagreed, and it used to show only the half that had moved.
        if !self.declared.is_empty() {
            out.push(Line::Out(format!("\n{}DECLARED{}", s.bold, s.off)));
            for row in &self.declared {
                out.push(Line::Out(format!(
                    "  {:<22} on {}",
                    row.identity, row.destination
                )));
                // Only a disagreement earns the line: with no host at all
                // there is no second answer, and the routing side says the
                // same (`DestinationDrift::Unconfigured`).
                if row.drift == Some("invalid") {
                    out.push(Line::Dim(
                        "    the configured host above is not a valid SSH destination; Ulak's own commands here refuse until it is fixed".to_string(),
                    ));
                }
                if row.drift == Some("configured") {
                    out.push(Line::Dim(format!(
                        "    a declaration outranks the host above for Ulak's own bare commands here; if {} is gone for good: ulak clean --forget-destination {}",
                        row.destination,
                        crate::ssh::sh_quote(&row.destination)
                    )));
                }
            }
        }

        out.push(Line::Out(format!("\n{}ENVIRONMENT{}", s.bold, s.off)));
        if self.environment.is_empty() {
            out.push(Line::Out("  no ULAK_* variables are set".to_string()));
        }
        for row in &self.environment {
            // A list variable holds newlines by design; printed raw it
            // would tear the table in half, so it is shown the way it
            // would be typed back into a shell.
            let shown = row
                .value
                .as_deref()
                .map(|v| v.replace('\n', "\\n"))
                .unwrap_or_else(|| "set".to_string());
            let note = match (row.role, row.key) {
                ("setting", Some(key)) => format!("→ {key}"),
                ("ulak", _) => "→ ulak".to_string(),
                ("test-hook", _) => "test hook".to_string(),
                _ => "not an Ulak setting".to_string(),
            };
            out.push(Line::Out(format!(
                "  {:<26} {:<24} {note}",
                row.name, shown
            )));
        }
        out
    }
}

/// Every setting, rendered the way the user would write it back.
fn settings_of(c: &Config) -> Vec<(&'static str, String)> {
    vec![
        ("host", c.host.clone().unwrap_or_else(|| "(not set)".into())),
        (
            "workspace.namespace",
            c.workspace
                .namespace
                .clone()
                .unwrap_or_else(|| "(automatic)".into()),
        ),
        ("sync.exclude", list(&c.sync.exclude)),
        ("sync.include", list(&c.sync.include)),
        ("sync.protect", list(&c.sync.protect)),
        ("sync.max_delete", c.sync.max_delete.to_string()),
        ("forward.auto", c.forward.auto.to_string()),
        ("service.auto", c.service.auto.to_string()),
    ]
}

fn list(v: &[String]) -> String {
    if v.is_empty() {
        "(none)".to_string()
    } else {
        v.join(", ")
    }
}

/// Every `ULAK_*` in the environment, labelled. This is the only place a
/// typo shows itself: Ulak deliberately says nothing about a name it does
/// not recognise, because the prefix is not its property — someone's own
/// wrapper script may own `ULAK_DEPLOY_TARGET`.
fn environment_of(env: &BTreeMap<String, String>) -> Vec<EnvRow> {
    env.iter()
        .filter(|(k, _)| k.starts_with("ULAK_"))
        .map(|(name, value)| {
            let known = config::ENV_VARS.iter().find(|v| v.name == name);
            let (role, key) = match known {
                Some(v) => (if v.key.is_some() { "setting" } else { "ulak" }, v.key),
                None if name.starts_with(config::ENV_TEST_PREFIX) => ("test-hook", None),
                None => ("unknown", None),
            };
            EnvRow {
                name: name.clone(),
                value: if known.is_some_and(|v| v.secret) {
                    None
                } else {
                    Some(value.clone())
                },
                role,
                key,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn write(path: &std::path::Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The styles `ui` hands a redirected stdout. Passing them in keeps
    /// the assertions off `is_terminal`, which cargo answers with a pipe
    /// today and a tty under `--nocapture`.
    fn plain() -> ui::Style {
        ui::Style {
            dim: "",
            red: "",
            green: "",
            yellow: "",
            blue: "",
            bold: "",
            off: "",
        }
    }

    fn rendered(report: &Report) -> Vec<String> {
        report
            .lines(&plain())
            .into_iter()
            .map(|l| match l {
                Line::Out(text) | Line::Dim(text) => text,
            })
            .collect()
    }

    /// The one row whose first column is `label`. A variable's name also
    /// appears in the settings row it won, and a layer's path inside the
    /// setting that came from it, so a row is found by the column it
    /// owns rather than by the name turning up anywhere on the line.
    fn row_for<'a>(lines: &'a [String], label: &str) -> &'a str {
        let head = format!("  {label} ");
        let found: Vec<&String> = lines.iter().filter(|l| l.starts_with(&head)).collect();
        assert_eq!(found.len(), 1, "expected one row for {label}: {found:?}");
        found[0]
    }

    fn resolved_in(home: &std::path::Path, vars: &[(&str, &str)]) -> config::Resolved {
        config::resolve_layers_in(home, None, &env(vars)).unwrap()
    }

    /// The DECLARED section grades a declaration exactly as routing does:
    /// a config naming another server is a disagreement worth the escape
    /// line, a config naming none is not. A first draft compared the
    /// rendered host text instead, and "(not set)" read as a disagreement.
    #[test]
    fn a_declared_row_earns_the_escape_line_only_when_the_config_disagrees() {
        let tmp = tempfile::tempdir().unwrap();
        let mut report = Report::build(
            Some("demo".into()),
            &resolved_in(tmp.path(), &[]),
            &env(&[]),
        );
        let rendered = |report: &Report| -> Vec<String> {
            report
                .lines(&plain())
                .into_iter()
                .map(|l| match l {
                    Line::Out(text) | Line::Dim(text) => text,
                })
                .collect()
        };
        assert!(
            !rendered(&report).iter().any(|l| l.contains("DECLARED")),
            "no declarations, no section"
        );

        report.declared = vec![DeclaredRow {
            identity: "api".into(),
            destination: "dead-server".into(),
            drift: Some("configured"),
        }];
        let lines = rendered(&report);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("api") && l.contains("on dead-server"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("ulak clean --forget-destination dead-server")),
            "{lines:?}"
        );

        report.declared[0].drift = Some("unconfigured");
        assert!(
            !rendered(&report)
                .iter()
                .any(|l| l.contains("--forget-destination")),
            "no host configured is not a disagreement"
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["declared"][0]["drift"], "unconfigured");

        // A host that is there and unusable is neither: the row says so
        // and offers no retirement, because the fix is the host line.
        report.declared[0].drift = Some("invalid");
        let lines = rendered(&report);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("not a valid SSH destination")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains("--forget-destination")));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["declared"][0]["drift"], "invalid");

        report.declared[0].drift = None;
        assert!(
            serde_json::to_value(&report).unwrap()["declared"][0]
                .get("drift")
                .is_none(),
            "agreement leaves no drift key"
        );
    }

    /// A file at the prototype's `.config/` location looks like the one to
    /// edit and is not read. Measured on a real checkout: the stale copy
    /// named a destroyed server, the live one its replacement, and the
    /// report listed only the files it had read — so nothing said which of
    /// the two the user was looking at.
    #[test]
    fn a_stale_layer_under_dot_config_is_listed_as_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write(&home.join("ulak.local.toml"), "host = \"new-server\"\n");
        write(
            &home.join(".config/ulak.local.toml"),
            "host = \"dead-server\"\n",
        );

        let report = Report::build(Some("demo".into()), &resolved_in(home, &[]), &env(&[]));
        let lines: Vec<String> = report
            .lines(&plain())
            .into_iter()
            .map(|l| match l {
                Line::Out(text) | Line::Dim(text) => text,
            })
            .collect();
        let stale = row_for(&lines, "stale");
        assert!(
            stale.contains(".config/ulak.local.toml") && stale.ends_with("ignored"),
            "{stale}"
        );
        assert!(
            lines.iter().any(|l| l.contains("not read")),
            "the row needs its reason: {lines:?}"
        );
        assert!(row_for(&lines, "host").contains("new-server"));

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json["ignored"],
            serde_json::json!([home.join(".config/ulak.local.toml").display().to_string()])
        );
        let bare = Report::build(
            Some("demo".into()),
            &resolved_in(tempfile::tempdir().unwrap().path(), &[]),
            &env(&[]),
        );
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("ignored")
                .is_none(),
            "an ordinary checkout must not grow a key for a file it does not have"
        );
    }

    /// `--json` is a machine contract: an agent parses it, and a field
    /// quietly renamed breaks that agent with no test failing here first.
    #[test]
    fn the_json_shape_is_pinned() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write(&home.join("ulak.toml"), "host = \"deploy@server\"\n");
        let resolved = config::resolve_layers_in(
            home,
            None,
            &env(&[
                ("ULAK_SYNC_MAX_DELETE", "unlimited"),
                ("ULAK_TEST_E2E", "skip"),
                ("ULAK_DEPLOY_TARGET", "someone-elses"),
            ]),
        )
        .unwrap();
        let report = Report::build(
            Some("demo".into()),
            &resolved,
            &env(&[
                ("ULAK_SYNC_MAX_DELETE", "unlimited"),
                ("ULAK_TEST_E2E", "skip"),
                ("ULAK_DEPLOY_TARGET", "someone-elses"),
            ]),
        );
        // The tempdir differs per run; the SHAPE is what is pinned.
        let json = serde_json::to_string_pretty(&report)
            .unwrap()
            .replace(tmp.path().to_str().unwrap(), "[HOME]");
        insta::assert_snapshot!(json);
    }

    /// The output is written to be pasted into a bug report and logged by
    /// an agent. A machine-readable copy of a leak is still a leak, so
    /// the JSON obeys the same rule as the printed form.
    #[test]
    fn a_secret_variable_never_carries_its_value_into_either_form() {
        let secret = "-----BEGIN OPENSSH PRIVATE KEY-----\nhorse-battery\n";
        // Named outright, because a loop over "whatever is marked secret"
        // passes by running zero times the moment the flag is dropped —
        // measured: clearing it broke nothing and every test stayed
        // green. The key material must BE marked, and that is the claim.
        let marked: Vec<&str> = config::ENV_VARS
            .iter()
            .filter(|v| v.secret)
            .map(|v| v.name)
            .collect();
        assert!(
            marked.contains(&"ULAK_SSH_KEY"),
            "the private key must be marked secret; marked today: {marked:?}"
        );

        for var in config::ENV_VARS.iter().filter(|v| v.secret) {
            let rows = environment_of(&env(&[(var.name, secret)]));
            let row = rows.iter().find(|r| r.name == var.name).unwrap();
            assert!(row.value.is_none(), "{} carried its value", var.name);

            let json = serde_json::to_string(&rows).unwrap();
            assert!(
                !json.contains("horse-battery"),
                "{} leaked through JSON: {json}",
                var.name
            );
        }
    }

    /// Ulak says nothing about a name it does not know — the `ULAK_`
    /// prefix is not its property — but it must still SHOW it, because
    /// this listing is the only place a typo becomes visible.
    #[test]
    fn every_ulak_variable_is_listed_and_labelled() {
        let rows = environment_of(&env(&[
            ("ULAK_HOST", "server"),
            ("ULAK_CONFIG", "/somewhere.toml"),
            ("ULAK_TEST_E2E", "skip"),
            ("ULAK_HOTS", "typo"),
            ("PATH", "/usr/bin"),
        ]));
        let role = |name: &str| rows.iter().find(|r| r.name == name).unwrap().role;
        assert_eq!(role("ULAK_HOST"), "setting");
        assert_eq!(role("ULAK_CONFIG"), "ulak");
        assert_eq!(role("ULAK_TEST_E2E"), "test-hook");
        assert_eq!(role("ULAK_HOTS"), "unknown");
        assert!(!rows.iter().any(|r| r.name == "PATH"));
    }

    /// A diagnostic that dies on the thing it diagnoses is the dead end
    /// `ui.rs` forbids: the readable layers still report, and the exit
    /// code carries the failure.
    #[test]
    fn a_broken_layer_is_reported_rather_than_fatal_but_still_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write(&home.join("ulak.toml"), "hosts = { nope = 1 }\n");
        write(&home.join("ulak.local.toml"), "host = \"still-read\"\n");
        let resolved = config::resolve_layers_in(home, None, &BTreeMap::new()).unwrap();
        let report = Report::build(None, &resolved, &BTreeMap::new());

        assert!(
            report.broken,
            "a layer that will not parse must fail the run"
        );
        assert!(
            report
                .layers
                .iter()
                .any(|l| l.state == "broken" && l.error.is_some())
        );
        let host = report.settings.iter().find(|s| s.key == "host").unwrap();
        assert_eq!(host.value, "still-read", "the readable layers still report");
    }

    /// Which layer won is the whole question this command answers.
    #[test]
    fn each_setting_names_the_layer_that_won_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write(&home.join("ulak.toml"), "host = \"from-file\"\n");
        let resolved =
            config::resolve_layers_in(home, None, &env(&[("ULAK_FORWARD_AUTO", "false")])).unwrap();
        let report = Report::build(None, &resolved, &BTreeMap::new());
        let row = |key: &str| {
            report
                .settings
                .iter()
                .find(|s| s.key == key)
                .unwrap_or_else(|| panic!("{key} missing from the report"))
        };
        assert!(row("host").from.ends_with("ulak.toml"));
        assert_eq!(row("forward.auto").variable, Some("ULAK_FORWARD_AUTO"));
        assert_eq!(row("sync.protect").from, "default");
    }

    /// A list variable is newline-separated by design, so its value
    /// reaches the report carrying real newlines. Printed raw it tore
    /// the ENVIRONMENT table in half and the rows below it lost their
    /// columns; it is shown the way it would be typed back instead.
    #[test]
    fn a_multi_line_variable_is_shown_on_one_line_with_its_newline_escaped() {
        let tmp = tempfile::tempdir().unwrap();
        let vars = [("ULAK_SYNC_EXCLUDE", "node_modules\ntarget")];
        let report = Report::build(None, &resolved_in(tmp.path(), &vars), &env(&vars));
        let lines = rendered(&report);
        let row = row_for(&lines, "ULAK_SYNC_EXCLUDE");
        assert!(
            row.contains("node_modules\\ntarget"),
            "the newline must survive as an escape: {row:?}"
        );
        assert!(!row.contains('\n'), "the row tore the table: {row:?}");
    }

    /// The printed form is the one people paste into a bug report, so it
    /// obeys the same rule as `--json`: whatever the inventory marks
    /// secret shows that it is set and nothing more.
    #[test]
    fn a_secret_variable_prints_that_it_is_set_and_never_its_value() {
        let tmp = tempfile::tempdir().unwrap();
        let secret = "-----BEGIN OPENSSH PRIVATE KEY-----\nhorse-battery\n";
        let secrets: Vec<&str> = config::ENV_VARS
            .iter()
            .filter(|v| v.secret)
            .map(|v| v.name)
            .collect();
        assert!(!secrets.is_empty(), "no secret variable left to prove this");
        for name in secrets {
            let vars = [(name, secret)];
            let report = Report::build(None, &resolved_in(tmp.path(), &vars), &env(&vars));
            let lines = rendered(&report);
            let row = row_for(&lines, name);
            assert!(row.contains(" set "), "{name} did not print `set`: {row:?}");
            assert!(
                !lines.iter().any(|l| l.contains("horse-battery")),
                "{name} leaked its value into the printed form"
            );
        }
    }

    /// The command exists to explain a layer that will not parse, so the
    /// reason has to reach the reader — and it belongs under the row it
    /// is about, on `ui::dim`'s stderr, because it is a diagnostic and
    /// not part of the table that `ulak config > file` captures.
    #[test]
    fn a_broken_layer_prints_its_reason_under_its_own_row() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("ulak.toml"), "hosts = { nope = 1 }\n");
        let report = Report::build(None, &resolved_in(tmp.path(), &[]), &env(&[]));

        let why = report
            .layers
            .iter()
            .find_map(|l| l.error.clone())
            .expect("nothing was reported broken");
        assert!(why.contains("is not valid Ulak TOML"), "{why:?}");

        let lines = report.lines(&plain());
        let at = lines
            .iter()
            .position(
                |l| matches!(l, Line::Out(t) if t.contains("ulak.toml") && t.contains("broken")),
            )
            .expect("no broken layer row was printed");
        let under: Vec<&str> = lines[at + 1..]
            .iter()
            .map_while(|l| match l {
                Line::Dim(text) => Some(text.as_str()),
                Line::Out(_) => None,
            })
            .collect();
        let expected: Vec<String> = why.lines().map(|l| format!("    {l}")).collect();
        assert_eq!(under, expected, "the reason did not follow its own row");
    }

    /// This listing is the only place a typo becomes visible, so every
    /// row has to say what its name means — including the two that mean
    /// "not yours to worry about", which an unlabelled row would leave
    /// looking like a setting that silently did nothing.
    #[test]
    fn every_environment_row_says_what_its_name_means() {
        let tmp = tempfile::tempdir().unwrap();
        let pointed = tmp.path().join("global.toml");
        write(&pointed, "");
        let vars = [
            ("ULAK_HOST", "server"),
            ("ULAK_CONFIG", pointed.to_str().unwrap()),
            ("ULAK_TEST_E2E", "skip"),
            ("ULAK_HOTS", "typo"),
        ];
        let report = Report::build(None, &resolved_in(tmp.path(), &vars), &env(&vars));
        let lines = rendered(&report);
        assert!(row_for(&lines, "ULAK_HOST").ends_with("→ host"));
        assert!(row_for(&lines, "ULAK_CONFIG").ends_with("→ ulak"));
        assert!(row_for(&lines, "ULAK_TEST_E2E").ends_with("test hook"));
        assert!(row_for(&lines, "ULAK_HOTS").ends_with("not an Ulak setting"));
    }

    /// An empty ENVIRONMENT heading reads as truncated output. Saying so
    /// is the difference between "nothing is set" and "the report gave
    /// up", which is the whole question someone runs this to settle.
    #[test]
    fn an_empty_environment_says_so_rather_than_printing_a_bare_heading() {
        let tmp = tempfile::tempdir().unwrap();
        let report = Report::build(None, &resolved_in(tmp.path(), &[]), &env(&[]));
        let lines = rendered(&report);
        let at = lines
            .iter()
            .position(|l| l.ends_with("ENVIRONMENT"))
            .expect("no ENVIRONMENT heading");
        assert_eq!(
            lines.get(at + 1).map(String::as_str),
            Some("  no ULAK_* variables are set")
        );
    }

    /// `ulak config` is run from anywhere — the heading is what tells the
    /// reader whether the settings below belong to a project or only to
    /// the machine, and a blank there makes the two indistinguishable.
    #[test]
    fn the_heading_names_the_project_or_says_this_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolved_in(tmp.path(), &[]);
        let named = Report::build(Some("demo".into()), &resolved, &env(&[]));
        assert_eq!(rendered(&named)[0], "ulak config — demo");
        let anonymous = Report::build(None, &resolved, &env(&[]));
        assert_eq!(rendered(&anonymous)[0], "ulak config — this machine");
    }

    /// A setting nobody wrote still has an answer, and a blank cell would
    /// read as one Ulak failed to compute. Each unset value says which
    /// kind of nothing it is: absent, derived, or an empty list.
    #[test]
    fn a_setting_nobody_wrote_names_the_kind_of_nothing_it_is() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolved_in(tmp.path(), &[]);
        let settings: BTreeMap<&str, String> = settings_of(&resolved.config).into_iter().collect();
        assert_eq!(settings["host"].as_str(), "(not set)");
        assert_eq!(settings["workspace.namespace"].as_str(), "(automatic)");
        assert_eq!(settings["sync.exclude"].as_str(), "(none)");

        let report = Report::build(None, &resolved, &env(&[]));
        let lines = rendered(&report);
        assert!(row_for(&lines, "host").contains("(not set)"));
    }

    /// "Which layer won" is the question, and `env` alone does not answer
    /// it: the reader has to know which variable to unset, which is a
    /// different name from the setting's own.
    #[test]
    fn a_setting_won_by_the_environment_names_the_variable_that_won_it() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("ulak.toml"), "forward = { auto = true }\n");
        let vars = [("ULAK_FORWARD_AUTO", "false")];
        let report = Report::build(None, &resolved_in(tmp.path(), &vars), &env(&vars));
        let lines = rendered(&report);
        let row = row_for(&lines, "forward.auto");
        assert!(
            row.ends_with("← env (ULAK_FORWARD_AUTO)"),
            "the winning variable is not named: {row:?}"
        );
        assert!(row.contains("false"), "the winning value is wrong: {row:?}");
    }

    /// ULAK_CONFIG relocates the global layer rather than adding one,
    /// so its row shows a path that is nowhere near the default. Without
    /// the marker the reader cannot tell that from a stale checkout.
    #[test]
    fn a_relocated_global_layer_says_which_variable_moved_it() {
        let tmp = tempfile::tempdir().unwrap();
        let pointed = tmp.path().join("elsewhere/global.toml");
        write(&pointed, "host = \"from-elsewhere\"\n");
        let vars = [("ULAK_CONFIG", pointed.to_str().unwrap())];
        let report = Report::build(None, &resolved_in(tmp.path(), &vars), &env(&vars));
        let lines = rendered(&report);
        let row = row_for(&lines, "global");
        assert!(
            row.contains(&format!("{} (ULAK_CONFIG)", pointed.display())),
            "the relocated layer is not marked: {row:?}"
        );
    }
}
