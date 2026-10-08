//! `ulak doctor` — the flagship preflight.
//!
//! Two jobs: local + remote tool checks, then a classification of
//! every local file reference in the compose model. It never guesses:
//! the model is resolved ON the server, and out-of-root paths are asked
//! about ON the server, batched — no per-path ssh round-trips.
//!
//! Doctor mutates nothing destructive: its pre-flight sync is push-only
//! (--max-delete=0), so a mis-protected tree cannot lose data here.
//!
//! Output contract: the report goes to stdout (color keyed to stdout),
//! progress/warnings to stderr; every problem is repeated in the list
//! at the end. Most of those lines carry their own "now do this". The
//! ones that do not are the ones whose remediation `render_error`
//! already printed in full further up — repeating it in the summary
//! would say it twice. Compose-scoped next steps repeat the exact selected
//! invocation: the first doctor may run before any declaration exists, so a
//! bare retry cannot be assumed to rediscover nonstandard files.

use std::collections::BTreeMap;
use std::process::ExitCode;

use anyhow::Result;

use crate::compose;
use crate::config::{Project, Workspace};
use crate::footprint::{self, Footprint, Why};
use crate::ssh::{Ssh, sh_quote};
use crate::sync;
use crate::ui::{self, fail};
use crate::walk;

pub fn run(globals: &[String]) -> Result<ExitCode> {
    match crate::management::locate(globals, "doctor", &[])? {
        crate::management::Context::Project(project) => run_compose(*project),
        crate::management::Context::Workspace(workspace) => run_workspace(*workspace),
        crate::management::Context::Outside => {
            // Keep the workspace-specific refusal and next step owned by
            // `Workspace::locate`; outside is not a Compose-free workspace.
            run_workspace(Workspace::locate()?)
        }
    }
}

fn run_compose(mut project: Project) -> Result<ExitCode> {
    let s = ui::style_stdout();
    let mut problems: Vec<String> = Vec::new();

    println!("{}ulak doctor{} — {}", s.bold, s.off, project.name);

    // ─── LOCAL ──────────────────────────────────────────────────────
    println!("\n{}LOCAL{}", s.bold, s.off);
    match sync::local_rsync() {
        Ok(bin) => {
            let ver = sync::rsync_version(&bin.to_string_lossy())
                .map(|(a, b)| format!("{a}.{b}"))
                .unwrap_or_else(|| "?".into());
            check_ok("rsync", &format!("{ver}  ({})", bin.display()));
        }
        Err(e) => {
            check_bad("rsync", "no rsync >= 3.2 on this machine");
            problems.push(ui::flatten(&e));
        }
    }
    // No "is there a compose file in the root" check any more: the
    // invocation could not have been captured without one, and WHICH
    // files were captured is the interesting fact.
    check_ok("compose files", &project.inv.flags_line());
    // The one question no other command can answer: a workspace whose stack
    // is down is deliberately silent, which on paper looks exactly like a
    // service that is not running at all.
    let service = crate::agent::health();
    if service.problem.is_some() {
        check_bad("service", &service.line);
    } else {
        check_ok("service", &service.line);
    }
    // Held back rather than pushed: a missing service must not gate the
    // server half below. It is a real problem and it is reported, but it
    // tells you nothing about whether the workspace itself would work, and
    // hiding the server diagnosis behind it would be a worse doctor.
    let service_problem = service.problem;
    check_ok("config", &project.config_home.display().to_string());
    ephemeral_state_note(project.workspace_id());
    check_ok(
        "project dir",
        &format!(
            "{} (compose resolves relative paths here)",
            project.inv.project_dir.display()
        ),
    );

    // One call, one answer. The row prints BEFORE anything is contacted,
    // so it must not read as a working link: "ok server <host>" was the
    // last line a user with a destroyed server ever saw.
    //
    // A drift is a problem for the summary, and it is held back like the
    // service problem above rather than pushed now: the gate below turns
    // any problem into "the remote half would only mislead", and a drift
    // is the one problem where the remote half is the diagnosis — whether
    // the declared server still answers is exactly what the user needs.
    let mut drift_problem: Option<String> = None;
    let dest = match project.destination() {
        Ok(d) => {
            match &d.drift {
                Some(crate::config::DestinationDrift::Configured(configured)) => {
                    check_bad(
                        "server",
                        &format!(
                            "{} — declared by a stack here; this checkout now configures {configured}",
                            d.dest
                        ),
                    );
                    drift_problem = Some(format!(
                        "this checkout addresses {}, not the configured {configured} — now: if {} is gone for good: {}",
                        d.dest,
                        d.dest,
                        crate::management::forget_command(&d.dest)
                    ));
                }
                _ => check_ok("server", &format!("{} (not contacted yet)", d.dest)),
            }
            d.dest
        }
        Err(e) => {
            check_bad("server", "not configured");
            problems.push(ui::flatten(&e));
            problems.extend(service_problem);
            return finish(&project, &problems);
        }
    };
    if !problems.is_empty() {
        // rsync or compose file missing: remote half would only mislead.
        problems.extend(service_problem);
        problems.extend(drift_problem);
        return finish(&project, &problems);
    }
    problems.extend(service_problem);
    problems.extend(drift_problem);

    // ─── SERVER, one batched round ──────────────────────────────────
    let ssh = Ssh::new(&dest)?;
    println!(
        "\n{}SERVER{} {}(one ssh round){}",
        s.bold, s.off, s.dim, s.off
    );
    // Not `?`. A dead destination used to abort the report right here, so
    // every server row AND the `doctor: N problem(s)` summary with its rerun
    // line were skipped — the report simply stopped, which is what made a
    // destroyed server feel like a broken tool rather than a diagnosis.
    // Mirrors the footprint branch below.
    let facts = match fetch_facts(&ssh) {
        Ok(facts) => facts,
        Err(e) => {
            check_bad("ssh", &format!("{dest} did not answer"));
            ui::render_error(&e);
            problems.push(format!("{dest} could not be reached (details above)"));
            return finish(&project, &problems);
        }
    };
    check_ok("ssh", &format!("connected as {}", facts.user));

    // Tool failures gate the model half; resource shortages do not.
    let mut tools_ok = true;
    let mut server_check = |ok: bool, label: &str, good: String, bad: &str, now: &str| -> bool {
        if ok {
            check_ok(label, &good);
        } else {
            check_bad(label, bad);
            problems.push(format!("{label}: {bad} — now: {now}"));
        }
        ok
    };

    tools_ok &= server_check(
        facts.daemon_ok,
        "docker",
        format!("{} (daemon reachable without sudo)", facts.docker),
        if facts.docker.is_empty() {
            "not installed"
        } else {
            "installed, but the daemon is not reachable without sudo"
        },
        if facts.docker.is_empty() {
            // Not `apt-get install docker-ce`: that package only exists
            // once docker's own apt repo has been added (measured on
            // Ubuntu 26.04 — the distro ships `docker.io` instead, and
            // its compose is a different package again).
            "install docker engine + compose v2: https://docs.docker.com/engine/install/"
        } else {
            "sudo usermod -aG docker $USER, then log out and back in — or start it: sudo systemctl enable --now docker"
        },
    );
    tools_ok &= server_check(
        facts.compose.contains("version"),
        "compose v2",
        facts.compose.clone(),
        "missing (the old hyphenated docker-compose v1 does not count)",
        "install the compose v2 plugin: https://docs.docker.com/compose/install/linux/",
    );
    let remote_rsync = sync::parse_rsync_version(&facts.rsync);
    tools_ok &= server_check(
        remote_rsync.is_some_and(|v| v >= (3, 2)),
        "rsync",
        remote_rsync
            .map(|(a, b)| format!("{a}.{b}"))
            .unwrap_or_default(),
        "missing or older than 3.2",
        "install it: apt-get install -y rsync (or your distro's equivalent)",
    );
    // A gate like the others, not only a row: the pre-flight push below
    // surveys through baseline.py, so without python3 it would end the
    // report at its `?` instead of reaching the summary.
    tools_ok &= match python_check(&facts.python_path, &facts.python, &dest) {
        Ok(detail) => server_check(true, "python3", detail, "", ""),
        Err((bad, now)) => server_check(false, "python3", String::new(), &bad, &now),
    };

    let mut resources = format!("arch {}", facts.arch);
    let mut disk_low = false;
    if let Some(gb) = facts.disk_free_gb {
        resources.push_str(&format!(" · disk {gb}G free"));
        if gb < 10 {
            disk_low = true;
            problems.push(format!(
                "only {gb}G free on the server — images will not fit — now: free space: ssh {dest} 'docker system df && docker system prune'"
            ));
        }
    }
    if let Some(gb) = facts.mem_gb {
        resources.push_str(&format!(" · mem {gb:.1}G"));
    }
    if disk_low {
        check_bad("resources", &resources);
    } else {
        check_ok("resources", &resources);
    }
    if !tools_ok {
        return finish(&project, &problems);
    }

    // ─── COMPOSE FOOTPRINT ──────────────────────────────────────────
    // Resolving comes BEFORE the pre-flight push: the footprint is what
    // decides which paths the push may even touch.
    let mut footprint = match footprint::resolve(&project, &ssh) {
        Ok((fp, _)) => fp,
        Err(e) => {
            println!("\n{}COMPOSE MODEL{}", s.bold, s.off);
            ui::render_error(&e);
            problems.push("the compose model could not be resolved (details above)".into());
            return finish(&project, &problems);
        }
    };
    // Before the pre-flight push, because the push writes the
    // manifest and the manifest carries the identity.
    project.adopt(&mut footprint);
    let (_, deletions_pending) = sync::push_no_delete(&project, &footprint, &ssh)?;
    let recorded = crate::invocation::pending_deletions(&project.state_key(&dest));
    if recorded > 0 {
        let budget = format!("--max-delete={recorded}");
        let sync = crate::management::root_command(&project, &["sync", &budget]);
        ui::warn(&format!(
            "{recorded} file(s) the project no longer has are still on the server — clear them: {sync}"
        ));
        problems.push(format!(
            "the workspace carries {recorded} stale file(s) — now: {sync}"
        ));
    } else if deletions_pending {
        let sync = crate::management::root_command(&project, &["sync"]);
        ui::warn(&format!(
            "local deletions are pending on the workspace — apply them with: {sync}"
        ));
    }

    let model = compose::parse_model(&footprint.model_json)?;
    let externals = compose::externals(&model);
    let server_paths: Vec<&str> = footprint
        .server_refs
        .iter()
        .map(|r| r.source.as_str())
        .collect();
    // One round for everything the SERVER has to answer for: paths that
    // are not synced, plus the networks and volumes compose expects to
    // find already there.
    let existing = check_server_side(&ssh, &server_paths, &externals)?;
    let swallowed = ignored_entries(&project, &footprint);

    println!("\n{}COMPOSE REFERENCES{}", s.bold, s.off);
    println!(
        "  {}anchor: {} → {}{}",
        s.dim,
        footprint.anchor.display(),
        project.remote_dir_shown(),
        s.off
    );
    let outcome = classify(
        &footprint,
        &project.config.sync.protect,
        &swallowed,
        &existing,
    );
    for line in &outcome.rows {
        println!("{line}");
    }

    if !outcome.writable_shared.is_empty() {
        println!();
        ui::warn(&format!(
            "the container can write into path(s) that also hold YOUR files: {} — anything it creates there is removed by the next sync with deletions",
            outcome.writable_shared.join(", ")
        ));
        ui::dim(
            "protect only the sub-path the container owns (e.g. a data/ inside it) — protecting the whole mount would stop your edits reaching the server",
        );
    }
    if !outcome.unprotected_writable.is_empty() {
        println!();
        let (whole, protectable): (Vec<_>, Vec<_>) =
            outcome.unprotected_writable.iter().partition(|p| *p == ".");
        if !whole.is_empty() {
            ui::warn(
                "the compose file mounts the WHOLE project directory writable (`.`) — ulak cannot protect `.`; protect the specific paths your containers write into, or they die on the next sync with deletions",
            );
        }
        if !protectable.is_empty() {
            let mut merged: Vec<String> = project.config.sync.protect.to_vec();
            for p in &protectable {
                let entry =
                    if footprint.anchor.join(p).is_dir() || !footprint.anchor.join(p).exists() {
                        format!("{p}/")
                    } else {
                        (*p).clone()
                    };
                merged.push(entry);
            }
            ui::warn(&format!(
                "writable bind mount(s) without protect: {} — the next sync with deletions could remove what containers wrote there",
                protectable
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            ui::dim("add to ulak.toml:");
            ui::dim("    [sync]");
            ui::dim(&format!(
                "    protect = [{}]",
                merged
                    .iter()
                    .map(|p| format!("\"{p}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    if !outcome.ignored.is_empty() {
        println!();
        let err = fail!(
            "{} referenced path(s) are excluded from the sync, so the server would only get an EMPTY directory: {}",
            outcome.ignored.len(),
            outcome.ignored.join(", ")
        )
        .now("if the container OWNS that data (a database dir), say so: protect = [\"<path>/\"] in ulak.toml")
        .now("if the app needs the files, force them through: include = [\"<path>/**\"]")
        .into_err();
        ui::render_error(&err);
        problems.push(format!(
            "{} referenced path(s) would arrive empty on the server",
            outcome.ignored.len()
        ));
    }

    if !outcome.hollow.is_empty() {
        println!();
        let err = fail!(
            "{} referenced path(s) would reach the container EMPTY, and something there has to read them: {}",
            outcome.hollow.len(),
            outcome.hollow.join(", ")
        )
        .now("check the path in the compose file — a typo produces exactly this")
        .now("if the files live on the SERVER, reference them with an absolute path instead")
        .into_err();
        ui::render_error(&err);
        problems.push(format!(
            "{} referenced path(s) would arrive empty",
            outcome.hollow.len()
        ));
    }

    // Compose reports these as `external: true` with the resolved name;
    // when they are absent, compose's own failure names neither the
    // resource nor the fix. ulak does not create them — docker would
    // not either — but it says exactly what to run.
    let missing_external: Vec<&compose::External> = externals
        .iter()
        .filter(|e| !existing.get(&external_key(e)).copied().unwrap_or(false))
        .collect();
    if !externals.is_empty() {
        println!();
        for e in &externals {
            let there = existing.get(&external_key(e)).copied().unwrap_or(false);
            if there {
                println!(
                    "  {}SERVER {}  {:<10} {:<8} {:<26} {}already on the server{}",
                    s.blue, s.off, "", e.kind, e.name, s.dim, s.off
                );
            } else {
                println!(
                    "  {}MISSING{}  {:<10} {:<8} {:<26} {}external, and NOT on the server{}",
                    s.red, s.off, "", e.kind, e.name, s.red, s.off
                );
            }
        }
    }
    if !missing_external.is_empty() {
        let err = fail!(
            "{} external resource(s) do not exist on the server — compose will not create them",
            missing_external.len()
        )
        .now(format!(
            "create each once: {}",
            missing_external
                .iter()
                .map(|e| {
                    if e.kind == "network" {
                        format!("ulak docker network create {}", e.name)
                    } else {
                        format!("ssh {dest} 'docker {} create {}'", e.kind, e.name)
                    }
                })
                .collect::<Vec<_>>()
                .join(" · ")
        ))
        .now("or drop `external: true` and let compose own them")
        .into_err();
        ui::render_error(&err);
        problems.push(format!(
            "{} missing external resource(s)",
            missing_external.len()
        ));
    }

    // The service brings the ports to localhost, which reads as "nothing
    // is exposed". A port published without a host IP is open on the
    // server's PUBLIC interface — on a VPS, to the internet.
    let public = compose::public_ports(&model);
    if !public.is_empty() {
        println!();
        ui::warn(&format!(
            "{} port(s) are published on EVERY interface of {dest}, not just its loopback: {}",
            public.len(),
            public
                .iter()
                .map(|(svc, p)| format!("{p} ({svc})"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        ui::dim("bind them to loopback in compose — the tunnel brings them home either way:");
        ui::dim(&format!(
            "    ports: [\"127.0.0.1:{p}:{p}\"]",
            p = public[0].1
        ));
    }

    if outcome.missing_server > 0 {
        println!();
        let err = fail!(
            "{} server-side reference(s) have no counterpart on the server",
            outcome.missing_server
        )
        .now(format!(
            "create each once on the server — a directory with: ssh {dest} 'mkdir -p <path>' · a file (configs/secrets/env) with: scp <local> {dest}:<path>"
        ))
        .now("or move it into the project and reference it with a relative path")
        .into_err();
        ui::render_error(&err);
        problems.push(format!(
            "{} missing server-side path(s) — see the MISSING rows and the steps above",
            outcome.missing_server
        ));
    }
    finish(&project, &problems)
}

/// General workspace/server preflight when Compose is not part of the
/// project yet. Compose-specific model, mounts and external resources
/// are deliberately skipped rather than treated as missing.
fn run_workspace(workspace: Workspace) -> Result<ExitCode> {
    let s = ui::style_stdout();
    let mut problems = Vec::new();
    println!("{}ulak doctor{} — {}", s.bold, s.off, workspace.name);

    println!("\n{}LOCAL{}", s.bold, s.off);
    match sync::local_rsync() {
        Ok(bin) => {
            let ver = sync::rsync_version(&bin.to_string_lossy())
                .map(|(a, b)| format!("{a}.{b}"))
                .unwrap_or_else(|| "?".into());
            check_ok("rsync", &format!("{ver}  ({})", bin.display()));
        }
        Err(e) => {
            check_bad("rsync", "no rsync >= 3.2 on this machine");
            problems.push(ui::flatten(&e));
        }
    }
    check_ok("workspace", &workspace.root.display().to_string());
    ephemeral_state_note(workspace.workspace_key.id());
    check_ok("compose", "not configured — skipped");

    let dest = match workspace.ssh_dest() {
        Ok(dest) => {
            check_ok("server", &format!("{dest} (not contacted yet)"));
            dest
        }
        Err(e) => {
            check_bad("server", "not configured");
            problems.push(ui::flatten(&e));
            return finish_workspace(&problems);
        }
    };
    if !problems.is_empty() {
        return finish_workspace(&problems);
    }

    let ssh = Ssh::new(&dest)?;
    println!(
        "\n{}SERVER{} {}(one ssh round){}",
        s.bold, s.off, s.dim, s.off
    );
    // The same shape as `run_compose`: a server that does not answer is a
    // row and a problem, never the end of the report.
    let facts = match fetch_facts(&ssh) {
        Ok(facts) => facts,
        Err(e) => {
            check_bad("ssh", &format!("{dest} did not answer"));
            ui::render_error(&e);
            problems.push(format!("{dest} could not be reached (details above)"));
            return finish_workspace(&problems);
        }
    };
    check_ok("ssh", &format!("connected as {}", facts.user));
    if facts.daemon_ok {
        check_ok("docker", &format!("{} (daemon reachable)", facts.docker));
    } else {
        check_bad("docker", "daemon is not reachable without sudo");
        problems.push("docker daemon is not reachable without sudo".into());
    }
    let remote_rsync = sync::parse_rsync_version(&facts.rsync);
    if remote_rsync.is_some_and(|v| v >= (3, 2)) {
        let (a, b) = remote_rsync.unwrap();
        check_ok("rsync", &format!("{a}.{b}"));
    } else {
        check_bad("rsync", "missing or older than 3.2");
        problems.push("remote rsync is missing or older than 3.2".into());
    }
    // A build syncs its context through the same survey as a Compose stack.
    match python_check(&facts.python_path, &facts.python, &dest) {
        Ok(detail) => check_ok("python3", &detail),
        Err((bad, now)) => {
            check_bad("python3", &bad);
            problems.push(format!("python3: {bad} — now: {now}"));
        }
    }
    if facts.compose.contains("version") {
        check_ok("compose v2", &facts.compose);
    } else {
        check_ok(
            "compose v2",
            "not installed — direct Docker commands remain available",
        );
    }
    finish_workspace(&problems)
}

fn finish_workspace(problems: &[String]) -> Result<ExitCode> {
    let s = ui::style_stdout();
    println!();
    if problems.is_empty() {
        println!(
            "{}doctor: ready{} — Docker build and daemon-only commands are available; Compose is optional",
            s.green, s.off
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}doctor: {} problem(s){}", s.red, problems.len(), s.off);
    for problem in problems {
        println!("  {}·{} {problem}", s.red, s.off);
    }
    println!("fix the items above, then rerun: ulak doctor");
    Ok(ExitCode::FAILURE)
}

fn finish(project: &Project, problems: &[String]) -> Result<ExitCode> {
    let s = ui::style_stdout();
    println!();
    if problems.is_empty() {
        let up = crate::management::compose_command_for_identity(
            project,
            &project.compose_identity(),
            &["up", "-d"],
        );
        println!("{}doctor: ready{} — next: {up}", s.green, s.off,);
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}doctor: {} problem(s){}", s.red, problems.len(), s.off);
    for p in problems {
        println!("  {}·{} {p}", s.red, s.off);
    }
    let doctor = crate::management::root_command(project, &["doctor"]);
    println!("fix the items above, then rerun: {doctor}");
    Ok(ExitCode::FAILURE)
}

/// The pure half of the note below: an identity declared for this run
/// while this machine holds no claim of its own is the wiped-runner
/// shape. It self-clears — once a claim survives locally, the state
/// directory is durable and there is nothing to warn about.
fn state_looks_ephemeral(declared: bool, recorded: bool) -> bool {
    declared && !recorded
}

/// What an ephemeral state directory means for deletions.
///
/// Declaring the workspace identity is what lets a wiped runner sync at
/// all (see `config::declared_workspace_uuid`). It does not bring the
/// LEDGER back, and the ledger is the only record of what this machine
/// put on the server — so with nothing to claim, nothing is ever
/// retired: a file deleted from the checkout stays up there and the
/// workspace grows every run.
///
/// That is the safe direction, and it is deliberate, but silent is not
/// the same as safe. It self-clears: once a claim survives locally the
/// state is durable and the note stops.
fn ephemeral_state_note(workspace_id: &str) {
    let declared = matches!(crate::config::declared_workspace_uuid(), Ok(Some(_)));
    let recorded = crate::invocation::recorded_uuid(workspace_id).is_some();
    let Some((detail, rest)) = ephemeral_state_note_lines(declared, recorded).split_first() else {
        return;
    };
    check_ok("state", detail);
    for line in rest {
        ui::dim(line);
    }
}

/// The words, apart from the printing: what the note SAYS is the point
/// of it, and the printer reaches them only on a machine that declares
/// an identity and holds no claim of its own.
///
/// The first line is the `state` check's detail, on stdout; the rest are
/// the dim lines under it, on stderr, indented to hang off it.
fn ephemeral_state_note_lines(declared: bool, recorded: bool) -> &'static [&'static str] {
    if !state_looks_ephemeral(declared, recorded) {
        return &[];
    }
    &[
        "declared identity, no local record yet",
        "       deletions do not reconcile while the state directory is fresh each run:",
        "       files removed from the checkout stay on the server",
        "       to reconcile, persist XDG_STATE_HOME between runs",
    ]
}

fn check_ok(label: &str, detail: &str) {
    let s = ui::style_stdout();
    println!("  {}ok{}   {label:<14} {detail}", s.green, s.off);
}

fn check_bad(label: &str, detail: &str) {
    let s = ui::style_stdout();
    println!("  {}BAD{}  {label:<14} {detail}", s.red, s.off);
}

// ─── remote facts, one ssh round ────────────────────────────────────

struct Facts {
    user: String,
    arch: String,
    rsync: String,
    /// Where `python3` resolves on the server's PATH; empty when it
    /// resolves nowhere.
    python_path: String,
    /// What `python3` printed for its version; empty when there is none.
    python: String,
    docker: String,
    daemon_ok: bool,
    compose: String,
    disk_free_gb: Option<u64>,
    mem_gb: Option<f64>,
}

/// Tools and resources only. The model no longer rides along: it is
/// resolved from the BOOTSTRAP directory (footprint.rs), which works
/// before the workspace exists — the old order could only ask a workspace that
/// had already been filled, which is backwards.
fn fetch_facts(ssh: &Ssh) -> Result<Facts> {
    // The workspace lives under $HOME — measure the disk it actually uses.
    let script = format!(
        "echo '==ULAK:user=='; id -un\n\
         echo '==ULAK:arch=='; uname -m\n\
         echo '==ULAK:rsync=='; rsync --version 2>/dev/null | head -1\n\
         {python}\
         echo '==ULAK:docker=='; docker --version 2>/dev/null\n\
         echo '==ULAK:daemon=='; docker info >/dev/null 2>&1 && echo OK || echo FAIL\n\
         echo '==ULAK:compose=='; docker compose version 2>/dev/null\n\
         echo '==ULAK:disk=='; df -Pk \"$HOME\" 2>/dev/null | tail -1\n\
         echo '==ULAK:mem=='; grep MemTotal /proc/meminfo 2>/dev/null\n",
        python = python_probe()
    );
    let out = ssh.run_script(&script)?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let sections = split_sections(&stdout);

    // A transport failure must never masquerade as "docker missing".
    if !sections.contains_key("user") {
        let brief: String = stderr.lines().take(4).collect::<Vec<_>>().join("\n    ");
        return Err(fail!(
            "the ssh connection to {} broke while gathering server facts:\n    {brief}",
            ssh.dest
        )
        .now(format!("test it by hand: ssh {} true", ssh.dest))
        // Doctor is the first command a user runs when a server stops
        // answering, and this error is built here rather than in ssh.rs,
        // so the retirement step ssh.rs offers has to be offered here too.
        .maybe_now(crate::management::retirement_step(&ssh.dest))
        .into_err());
    }
    let get = |k: &str| sections.get(k).cloned().unwrap_or_default();
    Ok(Facts {
        user: get("user").trim().to_string(),
        arch: get("arch").trim().to_string(),
        rsync: get("rsync").trim().to_string(),
        python_path: get("python3-path").trim().to_string(),
        python: get("python3").trim().to_string(),
        docker: get("docker").trim().to_string(),
        daemon_ok: get("daemon").trim() == "OK",
        compose: get("compose").trim().to_string(),
        disk_free_gb: parse_df_avail_gb(&get("disk")),
        mem_gb: parse_meminfo_gb(&get("mem")),
    })
}

fn split_sections(text: &str) -> BTreeMap<String, String> {
    let mut sections = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut buf = String::new();
    for line in text.lines() {
        if let Some(name) = line
            .strip_prefix("==ULAK:")
            .and_then(|r| r.strip_suffix("=="))
        {
            if let Some(prev) = current.take() {
                sections.insert(prev, std::mem::take(&mut buf));
            }
            current = Some(name.to_string());
        } else if current.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(prev) = current {
        sections.insert(prev, buf);
    }
    sections
}

fn parse_df_avail_gb(df_line: &str) -> Option<u64> {
    // "…  1K-blocks  Used  Available  Capacity  Mounted"
    let kb: u64 = df_line.split_whitespace().nth(3)?.parse().ok()?;
    Some(kb / (1024 * 1024))
}

fn parse_meminfo_gb(line: &str) -> Option<f64> {
    let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some((kb / (1024.0 * 1024.0) * 10.0).round() / 10.0)
}

/// Every sync runs baseline.py on the server, and baseline.py refuses an
/// older python3 before it reads anything. A test pins the two together;
/// scripts/check-server.sh carries the same floor.
const PYTHON_FLOOR: (u32, u32) = (3, 7);

/// The remote change index (observer.rs) also needs this, on Linux. Below
/// it the service compares in full instead, which is slower, not broken,
/// so doctor notes it on a passing row rather than counting a problem.
const PYTHON_INDEX: (u32, u32) = (3, 9);

/// Run by the server's python3 in `fetch_facts`. `%`, not an f-string
/// (3.6+), and not `--version`, which Pythons before 3.4 print to stderr:
/// an interpreter too old to sync must still say which version it is, or
/// the row would call it missing.
const PYTHON_PROBE: &str = "import sys; print(\"%d.%d.%d\" % sys.version_info[:3])";

/// The python3 lines of `fetch_facts`'s script. Where the name resolves
/// is asked apart from what it prints: a pyenv or asdf shim with no
/// version chosen, or a venv whose interpreter is gone, is on PATH and
/// prints nothing, and reading that silence as "not installed" sent users
/// to a package install that changes nothing.
fn python_probe() -> String {
    format!(
        "echo '==ULAK:python3-path=='; command -v python3 2>/dev/null\n\
         echo '==ULAK:python3=='; python3 -c {} 2>/dev/null\n",
        sh_quote(PYTHON_PROBE)
    )
}

/// `PYTHON_PROBE` prints `major.minor.micro`; anything else, including the
/// silence of a missing or failing interpreter, is `None`.
fn parse_python_version(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The python3 row apart from the printing: `Ok` is a passing row's
/// detail, `Err` the failing row's detail and the step that fixes it.
/// `on_path` is where the name resolved and `probe` what it printed.
fn python_check(
    on_path: &str,
    probe: &str,
    dest: &str,
) -> std::result::Result<String, (String, String)> {
    let found = probe.trim();
    let on_path = on_path.trim();
    let dotted = |(major, minor): (u32, u32)| format!("{major}.{minor}");
    match parse_python_version(found) {
        // An install would report python3 as already there.
        None if !on_path.is_empty() => Err((
            format!(
                "on PATH at {on_path} but did not run — syncing needs Python {} or later",
                dotted(PYTHON_FLOOR)
            ),
            format!(
                "see its error: ssh {dest} python3 --version — then repair that interpreter, or put a working Python {} or later ahead of it on the PATH a non-interactive ssh gets",
                dotted(PYTHON_FLOOR)
            ),
        )),
        None => Err((
            format!(
                "not installed — syncing needs Python {} or later",
                dotted(PYTHON_FLOOR)
            ),
            "install it: apt-get install -y python3 (or your distro's equivalent)".into(),
        )),
        Some(version) if version < PYTHON_FLOOR => Err((
            format!("{found} — syncing needs {} or later", dotted(PYTHON_FLOOR)),
            // Not "install python3": on the distros that ship an older one,
            // that command reports it is already there. What has to change
            // is which interpreter the name resolves to over ssh.
            format!(
                "install Python {} or later as the server's python3 — ulak runs the one a non-interactive ssh finds first on PATH",
                dotted(PYTHON_FLOOR)
            ),
        )),
        Some(version) if version < PYTHON_INDEX => Ok(format!(
            "{found} (the change index needs {}; the service compares in full instead)",
            dotted(PYTHON_INDEX)
        )),
        Some(_) => Ok(found.to_string()),
    }
}

/// Key an external resource by kind so a network and a volume with the
/// same name cannot answer for each other.
fn external_key(e: &compose::External) -> String {
    format!("{}:{}", e.kind, e.name)
}

/// Batched existence check for everything server-side — ONE round, and
/// printf (not echo: dash/zsh echo eats backslashes) for the replies.
fn check_server_side(
    ssh: &Ssh,
    paths: &[&str],
    externals: &[compose::External],
) -> Result<BTreeMap<String, bool>> {
    let mut result = BTreeMap::new();
    let safe: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|p| !p.contains('\n'))
        .collect();
    let safe_ext: Vec<&compose::External> = externals
        .iter()
        .filter(|e| !e.name.contains('\n'))
        .collect();
    if safe.is_empty() && safe_ext.is_empty() {
        return Ok(result);
    }
    let list = safe
        .iter()
        .map(|p| sh_quote(p))
        .collect::<Vec<_>>()
        .join(" ");
    let mut script = String::new();
    if !safe.is_empty() {
        script.push_str(&format!(
            "for p in {list}; do if [ -e \"$p\" ]; then printf 'E %s\\n' \"$p\"; else printf 'M %s\\n' \"$p\"; fi; done\n"
        ));
    }
    for e in &safe_ext {
        script.push_str(&format!(
            "if docker {kind} inspect {name} >/dev/null 2>&1; then printf 'E %s\\n' {key}; else printf 'M %s\\n' {key}; fi\n",
            kind = e.kind,
            name = sh_quote(&e.name),
            key = sh_quote(&external_key(e)),
        ));
    }
    let script = script;
    let out = ssh.run_script(&script)?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(p) = line.strip_prefix("E ") {
            result.insert(p.to_string(), true);
        } else if let Some(p) = line.strip_prefix("M ") {
            result.insert(p.to_string(), false);
        }
    }
    Ok(result)
}

// ─── classification (pure — snapshot-tested) ────────────────────────

/// Footprint entries whose path the ignore rules swallow whole: compose
/// asks for them, rsync will not send them, and docker then creates an
/// empty directory on the server. Silent in the prototype — a reported
/// problem now.
fn ignored_entries(project: &Project, fp: &Footprint) -> Vec<String> {
    let build = fp.build_filter();
    let mut out: Vec<String> = fp
        .entries
        .iter()
        .filter(|e| e.exists)
        .filter(|e| {
            walk::is_ignored(&fp.anchor, &e.local, &project.config.sync, &build).unwrap_or(false)
        })
        .filter_map(|e| crate::invocation::anchor_rel(&fp.anchor, &e.local))
        .collect();
    out.sort();
    out.dedup();
    out
}

struct Classified {
    rows: Vec<String>,
    /// Writable mounts that look container-OWNED: worth a ready-to-paste
    /// protect line.
    unprotected_writable: Vec<String>,
    /// Writable mounts that carry OUR files too (source mounted rw for
    /// hot reload). Protecting them would stop the sync, so they get a
    /// caveat, never a suggestion.
    writable_shared: Vec<String>,
    ignored: Vec<String>,
    /// Referenced paths that will arrive with nothing in them AND whose
    /// consumer needs content: a read-only mount, a build context, a
    /// config/secret file. A writable volume that is not there yet is
    /// NOT in here — docker creating it is the normal case.
    hollow: Vec<String>,
    missing_server: usize,
}

fn classify(
    fp: &Footprint,
    protect: &[String],
    swallowed: &[String],
    existing: &BTreeMap<String, bool>,
) -> Classified {
    let s = ui::style_stdout();
    let mut rows = Vec::new();
    let mut unprotected_writable = Vec::new();
    let mut writable_shared = Vec::new();
    let mut ignored = Vec::new();
    let mut hollow = Vec::new();
    let mut missing_server = 0;

    for e in &fp.entries {
        let Some(rel) = crate::invocation::anchor_rel(&fp.anchor, &e.local) else {
            continue;
        };
        let protected = protect.iter().any(|p| {
            let p = p.trim_start_matches("./").trim_end_matches('/');
            !p.is_empty() && (rel == p || rel.starts_with(&format!("{p}/")))
        });
        let swallowed_here = swallowed.contains(&rel);
        if swallowed_here && !protected {
            ignored.push(rel.clone());
        }
        // "writable bind mount" is NOT the same thing as "data the
        // container owns". Suggesting protect for a file the repo
        // tracks (supabase's roles.sql — seen for real) would stop it
        // being pushed and break the stack. Only a DIRECTORY that holds
        // nothing of ours is a real candidate: empty, absent, or
        // already ignored by git.
        if e.writable && !protected {
            let container_owned = e.is_dir && (!e.exists || e.empty || swallowed_here);
            if container_owned || rel == "." {
                unprotected_writable.push(rel.clone());
            } else {
                writable_shared.push(rel.clone());
            }
        }

        // Docker CREATES a missing bind source, so "not here yet" is
        // only a problem when something has to read content out of it.
        let needs_content = !e.writable || !matches!(e.why, Why::Volume);
        let mut notes: Vec<String> = Vec::new();
        let tag = if protected {
            // Server-owned by declaration: what it looks like locally is
            // beside the point, and it must never read as a problem.
            notes.push(format!("{}server-owned · protected{}", s.green, s.off));
            ("SYNC      ", s.green)
        } else if !e.exists && !needs_content {
            notes.push(format!(
                "{}not here yet — the container creates it{}",
                s.dim, s.off
            ));
            ("NEW    ", s.blue)
        } else if !e.exists {
            hollow.push(rel.clone());
            notes.push(format!(
                "{}not on this machine — nothing to mount{}",
                s.red, s.off
            ));
            ("EMPTY  ", s.red)
        } else if e.empty && needs_content {
            hollow.push(rel.clone());
            notes.push(format!("{}the directory is empty here{}", s.red, s.off));
            ("EMPTY  ", s.red)
        } else if swallowed_here {
            notes.push(format!(
                "{}excluded from the sync — arrives empty{}",
                s.yellow, s.off
            ));
            ("IGNORED", s.yellow)
        } else {
            ("SYNC      ", s.green)
        };
        if e.writable && !protected {
            notes.push(format!("{}writable{}", s.yellow, s.off));
        }
        rows.push(format!(
            "  {}{}{}  {:<10} {:<8} {:<26} {}",
            tag.1,
            tag.0,
            s.off,
            e.service,
            e.why.label(),
            rel,
            notes.join(" · ")
        ));
    }

    for r in &fp.server_refs {
        match existing.get(&r.source) {
            Some(true) => rows.push(format!(
                "  {}SERVER {}  {:<10} {:<8} {:<26} {}the server's own copy is used{}",
                s.blue,
                s.off,
                r.service,
                r.why.label(),
                r.source,
                s.dim,
                s.off
            )),
            _ => {
                missing_server += 1;
                rows.push(format!(
                    "  {}MISSING{}  {:<10} {:<8} {:<26} {}NOT on the server{}",
                    s.red,
                    s.off,
                    r.service,
                    r.why.label(),
                    r.source,
                    s.red,
                    s.off
                ));
            }
        }
    }

    unprotected_writable.sort();
    unprotected_writable.dedup();
    writable_shared.sort();
    writable_shared.dedup();
    Classified {
        rows,
        unprotected_writable,
        writable_shared,
        ignored,
        hollow,
        missing_server,
    }
}

#[cfg(test)]
mod tests {

    /// The note exists for the wiped runner and must not nag anyone else.
    /// It self-clears: a claim that survives locally means the state
    /// directory is durable, so there is nothing left to say.
    #[test]
    fn the_ephemeral_state_note_speaks_only_to_a_runner_with_no_record() {
        assert!(
            state_looks_ephemeral(true, false),
            "declared identity, no local claim: this is the CI case"
        );
        assert!(
            !state_looks_ephemeral(true, true),
            "a claim survived, so the state directory is durable"
        );
        assert!(
            !state_looks_ephemeral(false, false),
            "a first sync on a laptop must not be warned at"
        );
        assert!(!state_looks_ephemeral(false, true));
    }

    /// The note is the only place anyone is told that a wiped runner
    /// stops reconciling deletions — the workspace grows every job and
    /// nothing else explains why. Its body was reachable only by
    /// printing a whole `doctor` run, so any of these lines could have
    /// been dropped or reworded past the point of usefulness with every
    /// test still green. The way out is named on purpose: a note that
    /// only states the cost leaves a CI user with nothing to do.
    #[test]
    fn the_ephemeral_state_note_names_the_cost_and_the_way_out() {
        let said: &[&str] = &[
            "declared identity, no local record yet",
            "       deletions do not reconcile while the state directory is fresh each run:",
            "       files removed from the checkout stay on the server",
            "       to reconcile, persist XDG_STATE_HOME between runs",
        ];
        assert_eq!(ephemeral_state_note_lines(true, false), said);
        for (declared, recorded) in [(true, true), (false, false), (false, true)] {
            assert!(
                ephemeral_state_note_lines(declared, recorded).is_empty(),
                "declared={declared} recorded={recorded}: nothing is ephemeral here, so doctor says nothing"
            );
        }
    }
    use super::*;
    use crate::footprint::{Entry, ServerRef, Why};
    use std::path::PathBuf;

    fn entry(service: &str, why: Why, rel: &str, writable: bool, exists: bool) -> Entry {
        Entry {
            local: PathBuf::from("/m").join(rel),
            is_dir: !rel.contains('.'),
            exists,
            empty: false,
            why,
            service: service.into(),
            writable,
        }
    }

    fn fp(entries: Vec<Entry>, server_refs: Vec<ServerRef>) -> Footprint {
        Footprint {
            anchor: PathBuf::from("/m"),
            entries,
            server_refs,
            whole_anchor: false,
            contexts: vec![],
            pinned: vec![],
            model_json: String::new(),
        }
    }

    fn server(service: &str, source: &str) -> ServerRef {
        ServerRef {
            source: source.into(),
            why: Why::Volume,
            service: service.into(),
            writable: false,
        }
    }

    #[test]
    fn classification_snapshot() {
        let f = fp(
            vec![
                entry("web", Why::Volume, "site", false, true),
                entry("prober", Why::Volume, "data", true, true),
                entry("prober", Why::Build, "app", false, true),
                entry("app_conf", Why::Config, "configs/app.conf", false, true),
                entry("ghost", Why::Volume, "not-here", false, false),
                // A writable volume that is not there yet is normal:
                // docker creates it. It must NOT read as a problem.
                entry("db", Why::Volume, "pgdata", true, false),
            ],
            vec![
                server("web", "/etc/localtime"),
                server("worker", "/opt/certs"),
            ],
        );
        let existing = BTreeMap::from([
            ("/etc/localtime".to_string(), true),
            ("/opt/certs".to_string(), false),
        ]);
        let out = classify(&f, &[], &[], &existing);
        insta::assert_snapshot!(out.rows.join("\n"));
        assert_eq!(out.missing_server, 1);
        assert_eq!(
            out.hollow,
            vec!["not-here"],
            "only the read-only ref is hollow"
        );
        // `pgdata` is absent locally → container-owned → protect it.
        // `data` holds our files → a suggestion there would break the
        // sync, so it only gets the caveat.
        assert_eq!(out.unprotected_writable, vec!["pgdata"]);
        assert_eq!(out.writable_shared, vec!["data"]);
    }

    #[test]
    fn a_whole_project_mount_is_synced_not_server_side() {
        // `.:/app` and `build: .` resolve to the anchor itself.
        let f = Footprint {
            anchor: PathBuf::from("/m"),
            entries: vec![Entry {
                local: PathBuf::from("/m"),
                is_dir: true,
                exists: true,
                empty: false,
                why: Why::Volume,
                service: "app".into(),
                writable: true,
            }],
            server_refs: vec![],
            whole_anchor: true,
            contexts: vec![],
            pinned: vec![],
            model_json: String::new(),
        };
        let out = classify(&f, &[], &[], &BTreeMap::new());
        assert_eq!(out.missing_server, 0);
        assert!(out.rows.iter().all(|row| row.contains("SYNC")));
        assert_eq!(out.unprotected_writable, vec!["."]);
    }

    #[test]
    fn protected_writable_is_not_suggested() {
        let f = fp(
            vec![entry("db", Why::Volume, "data/pg", true, true)],
            vec![],
        );
        let out = classify(&f, &["data/".to_string()], &[], &BTreeMap::new());
        assert!(out.unprotected_writable.is_empty());
        assert!(out.writable_shared.is_empty());
        assert!(out.rows[0].contains("protected"));
    }

    #[test]
    fn a_writable_file_mount_is_never_a_protect_candidate() {
        // The real case: supabase mounts ./volumes/db/roles.sql writable.
        // Suggesting protect for it would stop the file being pushed and
        // break the stack.
        let f = fp(
            vec![entry("db", Why::Volume, "volumes/db/roles.sql", true, true)],
            vec![],
        );
        let out = classify(&f, &[], &[], &BTreeMap::new());
        assert!(
            out.unprotected_writable.is_empty(),
            "a tracked FILE must never be suggested for protect"
        );
        assert_eq!(out.writable_shared, vec!["volumes/db/roles.sql"]);
    }

    #[test]
    fn dot_slash_protect_entries_match() {
        // sync normalizes "./data/" — doctor must agree (was a
        // confirmed divergence).
        let f = fp(vec![entry("db", Why::Volume, "data", true, true)], vec![]);
        let out = classify(&f, &["./data/".to_string()], &[], &BTreeMap::new());
        assert!(out.unprotected_writable.is_empty());
        assert!(out.rows[0].contains("protected"));
    }

    #[test]
    fn a_reference_the_ignores_swallow_is_reported_not_hidden() {
        // The prototype's silence: compose mounts ./cache, .gitignore
        // hides it, the server grows an empty directory and nobody says
        // a word.
        let f = fp(
            vec![entry("app", Why::Volume, "cache", false, true)],
            vec![],
        );
        let out = classify(&f, &[], &["cache".to_string()], &BTreeMap::new());
        assert_eq!(out.ignored, vec!["cache"]);
        assert!(out.rows[0].contains("IGNORED"), "{:?}", out.rows);

        // …unless the user already declared it server-owned, which is
        // exactly what protect MEANS.
        let out = classify(
            &f,
            &["cache/".to_string()],
            &["cache".to_string()],
            &BTreeMap::new(),
        );
        assert!(out.ignored.is_empty());
        assert!(out.rows[0].contains("server-owned"));
    }

    #[test]
    fn facts_sections_split() {
        let text = "==ULAK:user==\nroot\n==ULAK:arch==\nx86_64\n";
        let sections = split_sections(text);
        assert_eq!(sections["user"].trim(), "root");
        assert_eq!(sections["arch"].trim(), "x86_64");
    }

    #[test]
    fn df_and_meminfo_parse() {
        assert_eq!(
            parse_df_avail_gb("/dev/sda1  78587584  2402024  73826328  4% /"),
            Some(70)
        );
        assert_eq!(parse_meminfo_gb("MemTotal:  8123456 kB"), Some(7.7));
    }

    /// The version is read as numbers: 3.10 is newer than 3.7, which a
    /// comparison of strings gets backwards. A missing python3 prints
    /// nothing, and that must read as no version rather than as 0.0.
    #[test]
    fn a_python_version_is_read_as_numbers_and_silence_is_no_version() {
        assert_eq!(parse_python_version("3.12.3\n"), Some((3, 12)));
        assert_eq!(parse_python_version("3.7.0"), Some((3, 7)));
        assert!(parse_python_version("3.10.0") > parse_python_version("3.7.17"));
        assert_eq!(parse_python_version(""), None, "no python3, no output");
        assert_eq!(parse_python_version("3"), None);
    }

    /// baseline.py refuses anything older than 3.7, so the row passes from
    /// 3.7 on and fails below it or with no python3 at all. Both halves are
    /// pinned: a floor one minor too high sends users to upgrade a server
    /// that syncs fine, and one too low calls ready a server whose first
    /// sync stops. 3.7 and 3.8 pass with the index note and 3.9 without.
    #[test]
    fn python3_passes_from_3_7_and_fails_below_it_or_when_missing() {
        for (probe, noted) in [
            ("3.7.0", true),
            ("3.8.10", true),
            ("3.9.0", false),
            ("3.10.12", false),
            ("3.12.3", false),
        ] {
            let detail = python_check("/usr/bin/python3", probe, "box")
                .unwrap_or_else(|(bad, _)| panic!("{probe} runs baseline.py, yet: {bad}"));
            assert!(detail.starts_with(probe), "{detail}");
            assert_eq!(
                detail.contains("the change index needs 3.9"),
                noted,
                "{probe}: {detail}"
            );
        }

        let (bad, now) =
            python_check("/usr/bin/python3", "3.6.15", "box").expect_err("baseline.py refuses 3.6");
        assert_eq!(bad, "3.6.15 — syncing needs 3.7 or later");
        assert!(
            now.contains("Python 3.7 or later") && now.contains("PATH"),
            "the step must say what to install and which python3 counts: {now}"
        );

        let (bad, now) = python_check("", "", "box").expect_err("no python3 cannot sync");
        assert_eq!(bad, "not installed — syncing needs Python 3.7 or later");
        assert_eq!(
            now,
            "install it: apt-get install -y python3 (or your distro's equivalent)"
        );

        assert!(python_check("/usr/bin/python3", "2.7.18", "box").is_err());
    }

    /// A pyenv shim with no version chosen is on PATH and prints no
    /// version. Doctor called it "not installed" and sent the user to
    /// apt-get, which answers that python3 is already there. The row names
    /// where the name resolved and how to see the interpreter's own error.
    #[test]
    fn a_python3_on_path_that_prints_no_version_did_not_run_and_is_not_missing() {
        let shim = "/home/me/.pyenv/shims/python3";
        let (bad, now) = python_check(shim, "", "box").expect_err("a failing python3 cannot sync");
        assert_eq!(
            bad,
            "on PATH at /home/me/.pyenv/shims/python3 but did not run — syncing needs Python 3.7 or later"
        );
        assert!(
            now.starts_with("see its error: ssh box python3 --version")
                && now.contains("Python 3.7 or later")
                && !now.contains("apt-get"),
            "the step must show the error and not offer an install: {now}"
        );

        // A version wins over the path, whatever `command -v` said.
        assert_eq!(python_check("", "3.12.3", "box").unwrap(), "3.12.3");
    }

    /// The probe runs through a shell on the server and the row reads its
    /// sections, so the real lines are run here against three PATHs: one
    /// with no python3, one whose python3 fails the way a pyenv shim does,
    /// and this machine's. A probe the parser cannot read would call
    /// python3 missing on every server.
    #[test]
    fn the_probe_tells_a_working_python3_from_a_failing_one_and_from_none() {
        use std::os::unix::fs::PermissionsExt;
        let probe = |path: &std::ffi::OsStr| {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(python_probe())
                .env("PATH", path)
                .output()
                .unwrap();
            let sections = split_sections(&String::from_utf8_lossy(&out.stdout));
            let get = |k: &str| sections.get(k).cloned().unwrap_or_default();
            python_check(&get("python3-path"), &get("python3"), "box")
        };

        let none = tempfile::tempdir().unwrap();
        let (bad, _) = probe(none.path().as_os_str()).expect_err("no python3 on PATH");
        assert!(bad.starts_with("not installed"), "{bad}");

        let shims = tempfile::tempdir().unwrap();
        let shim = shims.path().join("python3");
        std::fs::write(
            &shim,
            "#!/bin/sh\necho 'pyenv: python3: command not found' >&2\nexit 127\n",
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (bad, _) = probe(shims.path().as_os_str()).expect_err("a failing python3");
        assert!(
            bad.starts_with(&format!("on PATH at {} but did not run", shim.display())),
            "{bad}"
        );

        // Asserts python3 is on this machine, naming the fix, instead of
        // letting the shell's "not found" read as a parse failure.
        let _python3 = crate::stream::python();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let detail = probe(&path).unwrap_or_else(|(bad, _)| panic!("this python3: {bad}"));
        assert!(
            parse_python_version(&detail).is_some(),
            "doctor cannot read the probe's own output: {detail:?}"
        );
    }

    /// Doctor's floors copy what the helpers enforce, so they are read
    /// back together: baseline.py's refusal, the version observer.rs names
    /// when the change index is unavailable, and scripts/check-server.sh.
    /// A drift would have doctor call a server ready that the first sync
    /// turns away, or fail one that syncs fine.
    #[test]
    fn doctors_python_floors_are_the_ones_the_helpers_and_the_script_use() {
        let refusal = include_str!("baseline.py")
            .lines()
            .find_map(|line| line.strip_prefix("if sys.version_info < ("))
            .and_then(|rest| rest.strip_suffix("):"))
            .expect("baseline.py refuses an old python3 with `if sys.version_info < (M, N):`");
        assert_eq!(refusal, format!("{}, {}", PYTHON_FLOOR.0, PYTHON_FLOOR.1));

        let index = format!("Python {}.{} or later", PYTHON_INDEX.0, PYTHON_INDEX.1);
        assert!(
            include_str!("observer.rs").contains(&index),
            "observer.rs no longer names {index}"
        );

        let script = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/check-server.sh"),
        )
        .unwrap();
        for line in [
            format!("PYTHON_FLOOR={}.{}", PYTHON_FLOOR.0, PYTHON_FLOOR.1),
            format!("PYTHON_INDEX={}.{}", PYTHON_INDEX.0, PYTHON_INDEX.1),
        ] {
            assert!(
                script.lines().any(|l| l == line),
                "scripts/check-server.sh must carry `{line}`"
            );
        }
    }
}
