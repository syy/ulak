//! The daily pair: status / clean. Each stays a thin composition of the
//! tested building blocks.
//!
//! `dev` used to live here, and with it a `Babysat` machine that watched
//! background children for an unexpected death. Both are gone. The work
//! moved into the service, and the babysitting moved into the thing that
//! actually needed it: a tunnel child that owns its own connection
//! (`forward.rs`), where an exit means failure and nothing else.
//!
//! `shell` used to live here too. `shell <service>` was
//! `docker compose exec <svc> bash||sh` spelled a second way, and the
//! bare form was one ssh call into the workspace directory — neither is
//! Ulak's own capability, and the first was a second answer to a
//! question `ulak docker compose exec` already answers. Refusals that
//! need a session on the server name the host through `ulak status`
//! now, or hand over the `scp -r` that does the specific job.

use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::compose;
use crate::config::{self, Bound};
use crate::lockfile::WorkspaceLock;
use crate::ssh::sh_quote;
use crate::ui::{self, fail};

// ─── status ─────────────────────────────────────────────────────────

/// What this MACHINE is doing — every workspace at once, from local files
/// only.
///
/// The workspaces a user leaves up accumulate, and what was settled about
/// it is: not a TTL and not an `adopt` command, because
/// docker does not forget a stack either. The problem was never that
/// they pile up, it is that there was nowhere to SEE them piled. Every
/// number here is read from disk — no ssh, no docker, so it answers
/// instantly and works with the laptop shut off the network.
fn fleet() -> Result<ExitCode> {
    let s = ui::style_stdout();
    let health = crate::agent::health();
    println!("{}Ulak{} — this machine\n", s.bold, s.off);
    println!("{}service{}   {}", s.bold, s.off, health.line);

    let catalog = crate::intent::catalog();
    if catalog.is_empty() {
        println!("{}stacks{}      none yet", s.bold, s.off);
        println!("\nstart one: cd into a compose project, then ulak docker compose up -d");
        return finish_fleet(health);
    }

    let mut rows: Vec<(bool, String)> = Vec::new();
    let (mut live, mut idle) = (0, 0);
    for (id, desired) in &catalog {
        let where_ = match desired.rebuild_for_stack(id) {
            Ok(_) => Some((desired.identity.clone(), desired.destination.clone())),
            // The compose file it names is gone: a checkout that was
            // deleted or moved. Named rather than hidden, because the
            // workspace it left on a server is still taking up space there.
            Err(_) => None,
        };
        let status = crate::intent::read_status(id);
        let detail = match (&where_, desired.live) {
            (None, _) => format!(
                "{}the compose file it named is gone — its workspace may still be on {}{}",
                s.dim, desired.destination, s.off
            ),
            (Some(_), false) => {
                format!(
                    "{}not maintained (ulak docker compose up -d starts it){}",
                    s.dim, s.off
                )
            }
            (Some(_), true) => fleet_detail(status.as_ref(), &s),
        };
        let (name, dest) =
            where_.unwrap_or_else(|| (desired.identity.clone(), desired.destination.clone()));
        let word = if desired.live {
            live += 1;
            format!("{} live{}", s.green, s.off)
        } else {
            idle += 1;
            format!("{} idle{}", s.dim, s.off)
        };
        rows.push((
            desired.live,
            format!("{word}  {name:<24} {dest:<20} {detail}"),
        ));
    }
    println!("{}stacks{}      {live} live · {idle} idle", s.bold, s.off);
    println!();
    rows.sort_by(|a, b| (!a.0).cmp(&(!b.0)).then_with(|| a.1.cmp(&b.1)));
    for (_, row) in &rows {
        println!("{row}");
    }
    println!("\ndetails for one: cd into it, then ulak status");
    finish_fleet(health)
}

fn fleet_detail(status: Option<&crate::intent::Status>, s: &ui::Style) -> String {
    let Some(st) = status else {
        return format!("{}the service has not reported on it yet{}", s.dim, s.off);
    };
    if st.connection != "up" {
        return format!("{}link {}{}", s.red, st.connection, s.off);
    }
    // A real stack publishes a lot — measured, one workspace held 21 open
    // ports. Printing them all turns one row into an unreadable line and
    // buries the twenty other workspaces under it, so the fleet view counts
    // and the per-project `status` lists.
    //
    // A port that is open with no container behind it is NOT "home":
    // home means a connect reaches the service, and this one accepts and
    // then dies. It is not "not tunneled" either — the port is bound and
    // safe — so it is its own count. `None` counts as home: an older
    // service's file simply does not know, and crying wolf over every
    // port of a mixed-version machine would be worse than the gap.
    let open: Vec<u32> = st
        .tunnels
        .iter()
        .filter(|t| t.open && t.service_running != Some(false))
        .map(|t| t.port)
        .collect();
    let empty = st
        .tunnels
        .iter()
        .filter(|t| t.open && t.service_running == Some(false))
        .count();
    let shut = st.tunnels.len() - open.len() - empty;
    let mut out = match open.len() {
        0 => "link up".to_string(),
        1..=3 => format!(
            "link up · {}",
            open.iter()
                .map(|p| format!("localhost:{p}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        n => format!("link up · {n} ports home"),
    };
    if empty > 0 {
        out.push_str(&format!(
            "{} · {empty} port(s) with no container behind them{}",
            s.dim, s.off
        ));
    }
    if shut > 0 {
        out.push_str(&format!("{} · {shut} not tunneled{}", s.red, s.off));
    }
    if let Some(note) = &st.note {
        out.push_str(&format!("\n       {}{note}{}", s.dim, s.off));
    }
    out
}

/// One open tunnel as `status` lists it. A function rather than the
/// `println!` itself so the THIRD state has a test: the port is bound
/// (deliberately, from the moment the stack was declared) but no
/// container answers behind it — a connect succeeds and then dies. Said
/// here, where the expectation is created, because the `compose ps` a
/// few lines down shows that service missing and one screen must not
/// contradict itself. `None` is an older service's report and renders
/// as it always did.
fn tunnel_line(t: &crate::intent::TunnelState, s: &ui::Style) -> String {
    let nobody = match t.service_running {
        Some(false) => format!("   {}(no container is running for it){}", s.dim, s.off),
        _ => String::new(),
    };
    format!(
        "localhost:{} {}→{} {}{nobody}",
        t.port, s.dim, s.off, t.service
    )
}

fn finish_fleet(health: crate::agent::Health) -> Result<ExitCode> {
    if let Some(problem) = health.problem {
        ui::warn(&problem);
    }
    Ok(ExitCode::SUCCESS)
}

pub fn status(globals: &[String]) -> Result<ExitCode> {
    // Typed anywhere else, this is not a mistake to correct. The answer
    // to "these workspaces pile up" is not a TTL, it is being able to SEE
    // them — and seeing them must not need a new
    // command, because the question is the same one: what is going on?
    let crate::management::Context::Project(selected) =
        crate::management::locate(globals, "status", &[])?
    else {
        return fleet();
    };
    let Bound {
        project,
        ssh,
        dest,
        footprint,
    } = config::bind_located(*selected)?;
    let s = ui::style_stdout();
    println!("{}project{}   {}", s.bold, s.off, project.name);
    println!(
        "{}local{}     {}",
        s.bold,
        s.off,
        project.inv.project_dir.display()
    );
    // The captured invocation is shown, not implied: a `-p` or `-f` set
    // must never steer a command invisibly.
    println!("{}compose{}   {}", s.bold, s.off, project.inv.flags_line());
    if project.anchor != project.inv.project_dir {
        println!("{}anchor{}    {}", s.bold, s.off, project.anchor.display());
    }
    // What actually travels — the answer the prototype could not give,
    // because it sent a tree and hoped.
    let dirs = footprint.sync_dirs().len();
    let files = footprint.entries.iter().filter(|e| !e.is_dir).count();
    let server = footprint.server_refs.len();
    let mut what = format!("{dirs} dir(s) + {files} file(s)");
    if server > 0 {
        what.push_str(&format!(" · {server} server-side ref(s)"));
    }
    let doctor = crate::management::root_command(&project, &["doctor"]);
    println!("{}syncs{}     {what}   (details: {doctor})", s.bold, s.off);
    println!("{}server{}    {}", s.bold, s.off, dest);
    println!(
        "{}workspace{}    {}",
        s.bold,
        s.off,
        project.remote_dir_shown()
    );
    let identity = project.compose_identity();
    println!("{}identity{}  {identity}", s.bold, s.off);

    // Local truth, before a single byte goes over the wire: what did you
    // ASK for, and what has the background actually managed? Everything
    // above this line describes a workspace; these two lines are the first
    // that describe its life.
    let id = crate::intent::stack_id(&dest, &identity);
    let live = crate::intent::read_desired(&id);
    // Rendered from the pinned project, so when the config has moved on the
    // lines carry the declared server — see `management::render_project`.
    let down = crate::management::compose_command(&project, &["down"]);
    let up = crate::management::compose_command(&project, &["up", "-d"]);
    match &live {
        Some(d) if d.live => println!(
            "{}intent{}    live since {}   (ends with: {down})",
            s.bold,
            s.off,
            ago(d.updated_unix)
        ),
        _ => println!(
            "{}intent{}    not live   (start it with: {up})",
            s.bold, s.off
        ),
    }
    let live_now = live.as_ref().is_some_and(|d| d.live);
    if let Some(st) = crate::intent::read_status(&id) {
        let mut line = format!("link {}", st.connection);
        if st.last_sync_unix > 0 {
            line.push_str(&format!(" · synced {}", ago(st.last_sync_unix)));
        }
        if st.pulled_total > 0 {
            line.push_str(&format!(" · {} file(s) came back", st.pulled_total));
        }
        // A workspace nobody left up is not being maintained, so the last
        // thing the service saw is HISTORY. Presenting it as current is
        // how this command ends up announcing a port that closed hours
        // ago — measured here: four hours after `down`, status still
        // read "link up · localhost:5434" while nothing listened there,
        // which is precisely the failure class the product exists to
        // remove, committed by the product.
        if !live_now {
            println!(
                "{}service{}   {}not maintained · last seen {}: {line}{}",
                s.bold,
                s.off,
                s.dim,
                ago(st.updated_unix),
                s.off
            );
        } else {
            println!("{}service{}   {line}", s.bold, s.off);
            // The addresses themselves, not a count. "2/3 tunnel(s)" told
            // you that something was reachable without telling you WHERE,
            // which is the one thing a port forward exists to answer —
            // and it is the first question a user actually asked of this
            // command.
            let mut first = true;
            for t in st.tunnels.iter().filter(|t| t.open) {
                if first {
                    print!("{}tunnels{}   ", s.bold, s.off);
                    first = false;
                } else {
                    print!("          ");
                }
                println!("{}", tunnel_line(t, &s));
            }
            for t in st.tunnels.iter().filter(|t| !t.open) {
                ui::warn(&format!(
                    "{} NOT tunneled ({}) — who holds it: lsof -i :{}",
                    t.port, t.service, t.port
                ));
            }
            if let Some(note) = &st.note {
                ui::warn(note);
            }
        }
    }
    // The other half of that same answer: a stack you left up in another
    // Compose project is still being maintained, still holding local ports, and
    // until now there was nowhere that said so.
    let others = crate::intent::catalog()
        .iter()
        .filter(|(other, d)| d.live && *other != id)
        .count();
    if others > 0 {
        println!(
            "{}others{}    {others} more stack(s) live on this machine   {}(all of them: ulak status, outside a project){}",
            s.bold, s.off, s.dim, s.off
        );
    }

    // `ps` without -a shows RUNNING containers only, so a stopped stack
    // printed nothing but a column header (73 bytes, exit 0) and "never
    // started", "shut down cleanly" and "crashed 43 hours ago" all
    // looked identical. `-a` shows them; the state roll-up underneath
    // answers the question at a glance. Both travel in ONE round trip,
    // and the count comes from the engine's own label filter so it needs
    // no compose model at all.
    let script = format!(
        "if [ -d {dir} ]; then echo ULAK_WORKSPACE_OK; else echo ULAK_WORKSPACE_MISSING; fi\n\
         (cd {dir} 2>/dev/null && docker compose -p {id} ps -a) 2>&1\n\
         echo '==ULAK:states=='\n\
         docker ps -a --filter {label} --format '{{{{.State}}}}' 2>/dev/null || echo NO_ENGINE\n\
         echo '==ULAK:disk=='\n\
         df -Pk \"$HOME\" 2>/dev/null | tail -1\n",
        dir = sh_quote(&project.remote_dir()),
        id = sh_quote(&identity),
        label = sh_quote(&format!("label=com.docker.compose.project={identity}")),
    );
    let out = ssh.run_checked(&script, "reading server status")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (ps_part, rest) = text
        .split_once("==ULAK:states==")
        .unwrap_or((text.as_ref(), ""));
    let (states_part, disk_part) = rest.split_once("==ULAK:disk==").unwrap_or((rest, ""));

    if ps_part.contains("ULAK_WORKSPACE_MISSING") {
        println!();
        let sync = crate::management::root_command(&project, &["sync"]);
        ui::warn(&format!(
            "the workspace does not exist on the server yet — run: {sync}"
        ));
        return Ok(ExitCode::SUCCESS);
    }
    println!();
    for line in ps_part.lines().filter(|l| *l != "ULAK_WORKSPACE_OK") {
        println!("{line}");
    }
    match summarize_states(states_part) {
        Stack::Counts(line) => println!("{}{line}{}", s.dim, s.off),
        Stack::Absent => {
            ui::warn(
                "no containers for this project on the server — it has never come up here, or it was taken down",
            );
            ui::dim(&format!("start it: {up}"));
        }
        Stack::Unknown => {}
    }
    // A refused deletion set used to leave the workspace quietly out of
    // step. It says so now, every time, until it is done.
    let pending = crate::invocation::pending_deletions(&project.state_key(&dest));
    if pending > 0 {
        println!();
        ui::warn(&format!(
            "{pending} file(s) the project no longer has are still on the server"
        ));
        let budget = format!("--max-delete={pending}");
        let sync = crate::management::root_command(&project, &["sync", &budget]);
        ui::dim(&format!("clear them: {sync}"));
    }
    if let Some(gb) = disk_part
        .split_whitespace()
        .nth(3)
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb / (1024 * 1024))
    {
        if gb < 10 {
            ui::warn(&format!(
                "server disk low: {gb}G free — free space: ssh {dest} 'docker system prune'"
            ));
        } else {
            println!();
            println!("{}disk{}      {gb}G free on the server", s.bold, s.off);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A wall-clock age in words. `SystemTime` on purpose: this measures how
/// long ago something happened in the world, which is exactly the thing
/// `Instant` cannot see across a laptop sleep.
fn ago(unix: u64) -> String {
    let now = crate::intent::now_unix();
    if unix == 0 || unix > now {
        return "just now".into();
    }
    let secs = now - unix;
    match secs {
        0..=90 => "just now".into(),
        91..=5399 => format!("{}m ago", secs / 60),
        5400..=172_799 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// What the engine's own `.State` words add up to.
#[derive(Debug, PartialEq)]
enum Stack {
    /// The roll-up line.
    Counts(String),
    /// Nothing carries this project's label. That is NOT "0 running" —
    /// it means the stack has never existed on this server, or was taken
    /// down — and it deserves its own sentence.
    Absent,
    /// The engine did not answer. `compose ps` has already printed the
    /// real problem right above; a roll-up would only add noise.
    Unknown,
}

fn summarize_states(states: &str) -> Stack {
    if states.contains("NO_ENGINE") {
        return Stack::Unknown;
    }
    let (mut running, mut exited, mut other) = (0usize, 0usize, 0usize);
    for word in states.split_whitespace() {
        match word {
            "running" => running += 1,
            "exited" | "dead" => exited += 1,
            _ => other += 1, // created, restarting, paused, removing
        }
    }
    let total = running + exited + other;
    if total == 0 {
        return Stack::Absent;
    }
    let mut line = format!("{total} container(s): {running} running, {exited} exited");
    if other > 0 {
        line.push_str(&format!(", {other} in between"));
    }
    Stack::Counts(line)
}

// ─── clean ──────────────────────────────────────────────────────────

/// `clean` removes the remote workspace; `--forget-destination` removes
/// this checkout's memory that it ever declared a stack on one server. The
/// split is the point: one needs the server, the other exists precisely
/// because the server is gone.
pub fn clean(globals: &[String], forget: Option<&str>) -> Result<ExitCode> {
    match forget {
        Some(destination) => forget_destination(globals, destination),
        None => clean_remote(globals),
    }
}

/// Retire what this checkout declared on a server that no longer exists.
///
/// Reaches for nothing: no `Ssh`, no `management::project` (which would
/// select by today's config and re-derive a destination the flag already
/// named), no Docker labels, no protect prompt. Everything it can refuse it
/// refuses from local files, and the source-pinning test below keeps it so.
fn forget_destination(globals: &[String], destination: &str) -> Result<ExitCode> {
    if !globals.is_empty() {
        return Err(fail!(
            "--forget-destination selects by server, so the Compose globals `{}` would be ignored",
            globals.join(" ")
        )
        .now(format!(
            "name only the server: {}",
            crate::management::forget_command(destination)
        ))
        .into_err());
    }
    // Trimmed, not validated: the value is only ever COMPARED against
    // destinations that were validated when their declaration was rebuilt.
    // A value that matches nothing cannot report a false success —
    // `declared_on` refuses and names what was declared — and the config
    // validator would answer a typo by offering to edit a TOML file the
    // value never came from.
    let destination = destination.trim();
    let workspaces = crate::management::declared_on(destination)?;
    // The human waits for the locks: a concurrent `up` on this checkout may
    // be re-declaring the very stack this retires, and one of the two would
    // otherwise lose silently. Every matching workspace is held before the
    // first removal, in `declared_on`'s sorted order, so two overlapping
    // forgets cannot wait on each other.
    let _locks = workspaces
        .iter()
        .map(|id| WorkspaceLock::acquire(id))
        .collect::<Result<Vec<_>>>()?;

    // Declarations FIRST. They are the entire wedge — management selection,
    // the service worker and `Fleet::reload` all read `desired.json` — and
    // they are the only removal here with no data-safety consequence, since
    // a declaration describes intent and never bytes. A failure below still
    // leaves the user unwedged.
    let mut report = crate::intent::Retired::default();
    for workspace_id in &workspaces {
        let part = crate::intent::retire_declarations(workspace_id, destination);
        report.stacks.extend(part.stacks);
        report.failures.extend(part.failures);
    }
    report.stacks.sort_by(|a, b| a.identity.cmp(&b.identity));
    if !report.failures.is_empty() {
        let mut error = fail!(
            "{} declaration(s) for {destination} could not be removed",
            report.failures.len()
        );
        for trouble in &report.failures {
            error = error.now(format!("{}: {}", trouble.path.display(), trouble.why));
        }
        return Err(error
            .now("remove them by hand — nothing else on this machine points at them")
            .into_err());
    }
    // A cached model for that server is only a cache: safe to drop, and
    // wrong to keep once nothing will ever be resolved against it again.
    for workspace_id in &workspaces {
        crate::footprint::forget_cache_destination(workspace_id, destination);
    }

    let mut lines = forget_receipt(destination, &report).into_iter();
    if let Some(headline) = lines.next() {
        ui::ok(&headline);
    }
    for line in lines {
        ui::dim(&line);
    }
    Ok(ExitCode::SUCCESS)
}

/// What the retirement prints. Built here and printed by the caller: the
/// words are the deliverable, and cargo gives tests a pipe, not a tty.
fn forget_receipt(destination: &str, report: &crate::intent::Retired) -> Vec<String> {
    let mut lines = vec![format!(
        "{destination} is no longer declared here — it was not contacted, and nothing on it was deleted"
    )];
    for stack in &report.stacks {
        lines.push(if stack.live {
            format!(
                "retired {} — it was still declared live, so the service stops keeping it and closes its tunnels",
                stack.identity
            )
        } else {
            format!("retired {} — it was already idle", stack.identity)
        });
    }
    // Aimed: with the declaration gone, nothing pins that server any more,
    // and a bare `ulak clean` would select by today's config and remove the
    // REPLACEMENT server's workspace.
    lines.push(format!(
        "this checkout still remembers what it sent to {destination}; if that server ever answers again, remove those files properly with: {}",
        crate::management::aimed_at(destination, "ulak clean")
    ));
    lines
}

fn clean_remote(globals: &[String]) -> Result<ExitCode> {
    // Cleaning asks who owns an already-transported directory, not what
    // today's Compose model says. Going through `config::bind` here made
    // a syntax error in compose.yaml prevent the very cleanup that could
    // safely remove its old workspace. The invocation fixes the local
    // workspace id; intent and Docker's labels below decide whether it is
    // still in use. No resolved model or project identity participates.
    let project = crate::management::project(globals, "clean", &[])?;
    let dest = project.ssh_dest()?;
    let ssh = crate::ssh::Ssh::new(&dest)?;
    let workspace_id = project.workspace_id();
    let state = project.state_key(&dest);
    let _lock = WorkspaceLock::acquire(workspace_id)?;

    // A sync workspace can serve several Docker stacks (`-p a`, `-p b`),
    // on this destination. Removing its bytes because the CURRENT identity
    // happens to be down would leave another one running over a vanished
    // tree. A different destination has a different remote filesystem and
    // destination-specific receipts, so it neither vetoes this clean nor
    // loses its ledger. The declaration is pinned at `up`, so this remains
    // answerable even after the compose file changes or stops parsing.
    let mut live: Vec<_> = crate::intent::catalog()
        .into_iter()
        .filter(|(_, desired)| {
            desired.live && desired.workspace_id == workspace_id && desired.destination == dest
        })
        .map(|(_, desired)| desired)
        .collect();
    live.sort_by(|a, b| (&a.destination, &a.identity).cmp(&(&b.destination, &b.identity)));
    if !live.is_empty() {
        let stacks = live
            .iter()
            .map(|d| format!("{} on {}", d.identity, d.destination))
            .collect::<Vec<_>>()
            .join(", ");
        let first = &live[0];
        // Both pastes come from pinned projects, so `render_project` aims
        // them at the declared server when the config has moved — which is
        // the trap that closed the loop on the user who reported this: the
        // declared server was gone, `down` re-bound to its replacement and
        // addressed a stack that did not exist there. The hand-spelled
        // fallback has no project to carry the pin, so it is aimed here.
        let stop = match first.project_for_stack(&first.stack_id()) {
            Ok(first_project) => format!(
                "stop each one first, for example: {}",
                crate::management::compose_command(&first_project, &["down", "--remove-orphans"])
            ),
            Err(_) => format!(
                "stop each one first from its declaring checkout: {}",
                crate::management::aimed_at(
                    &dest,
                    &format!(
                        "ulak docker compose -p {} down --remove-orphans",
                        sh_quote(&first.identity)
                    )
                )
            ),
        };
        let clean = crate::management::root_command(&project, &["clean"]);
        return Err(fail!(
            "the workspace is still used by {} live Docker stack(s): {stacks}",
            live.len()
        )
        .now(stop)
        .now(format!("then: {clean}"))
        .now(format!(
            "or, if {dest} is gone for good: {}",
            crate::management::forget_command(&dest)
        ))
        .into_err());
    }

    // Local intent is the service contract; Docker's own labels are the
    // independent data-safety check. Created and stopped containers
    // count too: they still retain bind mounts into the workspace and
    // can be started again without another Compose invocation. Reading
    // every Compose container catches an explicit `-p` stack even if
    // local state was reset, without inventing a second Ulak ledger on
    // the server.
    //
    // How the labels are asked for is `compose::label_format`'s decision,
    // including why `{{.Labels}}` cannot be used and why it must stay one
    // `docker ps` round.
    let labels = compose::container_labels(&ssh, None)?;
    let existing = compose::projects_using_workspace(&labels, &project.remote_workspace_root());
    if !existing.is_empty() {
        let stop = crate::management::compose_command_for_identity(
            &project,
            &existing[0],
            &["down", "--remove-orphans"],
        );
        let clean = crate::management::root_command(&project, &["clean"]);
        return Err(fail!(
            "the workspace is still used by existing Docker stack(s): {}",
            existing.join(", ")
        )
        .now(format!("remove each one first, for example: {stop}"))
        .now(format!("then: {clean}"))
        .into_err());
    }

    // Resist when protect paths exist: they are server-owned DATA.
    let protect = &project.config.sync.protect;
    if !protect.is_empty() {
        ui::warn(&format!(
            "protect paths live only on the server and will be DELETED with the workspace: {}",
            protect.join(", ")
        ));
        if !confirm("delete the workspace including protected data?")? {
            let clean = crate::management::root_command(&project, &["clean"]);
            return Err(fail!("clean aborted — protected data stays on the server")
                .now(format!(
                    "back it up first, e.g.: scp -r {dest}:{}/{} .",
                    project.remote_dir_shown(),
                    protect[0]
                ))
                .now(format!("or rerun and confirm: {clean}"))
                .into_err());
        }
    }

    // Containers may have written ROOT-owned files into bind mounts
    // (measured on the non-root fixture): when plain rm fails, a
    // throwaway busybox container deletes as root — docker access is
    // root-equivalent on the server anyway, and it stays a visible argv.
    let hash_dir = project.remote_workspace_root();
    let namespace_dir = project.remote_namespace_root();
    let script = format!(
        "case {dir} in .ulak/workspaces/*/*) ;; *) echo REFUSED; exit 0 ;; esac\n\
         rm -rf {dir} 2>/dev/null\n\
         if [ -e {dir} ]; then\n\
           docker run --rm -v \"$HOME/{raw_dir}:/ulak-wipe\" busybox \
             sh -c 'rm -rf /ulak-wipe/* /ulak-wipe/.[!.]* /ulak-wipe/..?*' >/dev/null 2>&1\n\
         rmdir {dir} 2>/dev/null\n\
         fi\n\
         if [ -e {dir} ]; then echo FAILED; else rmdir {namespace} 2>/dev/null; echo REMOVED; fi",
        dir = sh_quote(&hash_dir),
        namespace = sh_quote(&namespace_dir),
        raw_dir = hash_dir, // expanded inside double quotes with $HOME
    );
    let out = ssh.run_checked(&script, "cleaning the workspace")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.contains("REMOVED") {
        crate::invocation::forget_workspace_destination(&state, &dest);
        crate::footprint::forget_cache_destination(workspace_id, &dest);
        ui::ok(&format!("workspace removed from {dest} ({hash_dir})"));
        let volumes = crate::management::compose_command(&project, &["down", "--volumes"]);
        ui::dim(&format!(
            "named volumes were kept — remove them with: {volumes} (before clean)"
        ));
        Ok(ExitCode::SUCCESS)
    } else if stdout.contains("REFUSED") {
        Err(
            fail!("the workspace path looked unexpected and was NOT removed")
                .now(format!(
                    "inspect it yourself: ssh {dest} 'ls ~/.ulak/workspaces'"
                ))
                .into_err(),
        )
    } else {
        Err(fail!(
            "some files in the workspace could not be removed (container-written, foreign owner)"
        )
        .now(format!(
            "remove them as root: ssh {dest} 'sudo rm -rf ~/{hash_dir}'"
        ))
        .into_err())
    }
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    match inquire::Confirm::new(question).with_default(false).prompt() {
        Ok(v) => Ok(v),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(false),
        Err(e) => Err(e).context("confirmation prompt failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The receipt is the only thing that tells a user what an offline
    /// retirement did and, above all, what it did NOT do: the server was
    /// never contacted, so nothing there changed. A user who reads it as
    /// "the workspace is gone from that machine" leaves a stale copy behind
    /// believing they cleaned up.
    #[test]
    fn the_receipt_says_the_server_was_never_contacted_and_nothing_there_deleted() {
        let report = crate::intent::Retired {
            stacks: vec![crate::intent::RetiredStack {
                identity: "dev-resources".into(),
                live: true,
            }],
            failures: Vec::new(),
        };
        let said = forget_receipt("dead-server", &report).join("\n");

        assert!(said.contains("dead-server"), "{said}");
        assert!(
            said.contains("not contacted") && said.contains("nothing on it was deleted"),
            "the receipt must not read as a remote cleanup: {said}"
        );
        assert!(
            said.contains("dev-resources") && said.contains("declared live"),
            "a live stack losing its service and tunnels is the surprising half: {said}"
        );
        assert!(
            said.contains("env ULAK_HOST=dead-server ulak clean"),
            "the way back must be aimed at the retired server, or it cleans the replacement: {said}"
        );
    }

    /// An idle declaration costs the user nothing when it goes, and saying
    /// "the service stops keeping it" about one would be false.
    #[test]
    fn an_idle_declaration_is_not_reported_as_losing_its_service() {
        let report = crate::intent::Retired {
            stacks: vec![crate::intent::RetiredStack {
                identity: "api".into(),
                live: false,
            }],
            failures: Vec::new(),
        };
        let said = forget_receipt("dead-server", &report).join("\n");
        assert!(said.contains("already idle"), "{said}");
        assert!(!said.contains("closes its tunnels"), "{said}");
    }

    /// The whole promise of `--forget-destination` is that it works when
    /// the server does not. One `?` on anything that reaches the network
    /// would put the escape behind the very failure it exists to escape,
    /// and a green suite cannot show that — the scenario needs a destroyed
    /// server. So the function's source is read back instead.
    #[test]
    fn the_offline_forget_reaches_for_nothing() {
        let src = include_str!("commands.rs");
        let start = src
            .find("fn forget_destination(")
            .expect("the offline branch to still be a named function");
        let end = src[start..]
            .find("\nfn forget_receipt(")
            .expect("the receipt builder to still follow it")
            + start;
        let body = &src[start..end];
        assert!(
            body.len() > 400,
            "the slice went empty: {} bytes",
            body.len()
        );

        for needle in [
            "Ssh",
            "ssh_dest",
            "container_labels",
            "run_checked",
            "management::project",
            "management::locate",
            "bind_located",
            "config::bind",
        ] {
            assert!(
                !body.contains(needle),
                "`{needle}` is back in the offline forget — it now needs the server it exists to give up on"
            );
        }
    }

    /// `ulak docker --help` ends by pointing at Ulak's own root commands,
    /// and it kept naming `shell` for as long as `shell` had not existed.
    /// A sentence in a help screen is a promise; this reads it back.
    #[test]
    fn every_root_command_named_in_docker_help_still_exists() {
        let help = crate::catalog::help(&[]).expect("the docker help tree to render");
        let line = help
            .lines()
            .find(|l| l.contains("own commands ("))
            .expect("the root-command sentence to still be printed");
        let open = line.find('(').expect("an opening parenthesis");
        let close = line.find(')').expect("a closing parenthesis");
        let mut named: Vec<String> = line[open + 1..close]
            .split(',')
            .map(|name| name.trim().to_string())
            .collect();
        named.sort();
        assert!(named.len() >= 4, "parsed nothing useful from: {line}");

        // Both directions: a name the sentence invents fails, and so does a
        // root command the sentence forgot. `docker` is the tree the reader
        // is already inside, and `help` is clap's own.
        let mut real: Vec<String> = crate::cli::completion_command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .filter(|name| name != "docker" && name != "help")
            .collect();
        real.sort();
        assert_eq!(
            named, real,
            "`ulak docker --help` must name exactly the root commands that exist"
        );
    }

    /// The fleet's "home" count is a promise that a connect reaches the
    /// service. A tunnel that is open with no container behind it
    /// accepts the connect and then dies, so counting it as home is the
    /// exact lie `ulak status` printed on a real server — while an older
    /// service's report, which does not carry the answer at all, must
    /// keep counting as it always did rather than alarm a whole machine
    /// over a version skew.
    /// Cargo gives tests a pipe, so this is what `style_stdout()` would
    /// answer — spelled out so a `--nocapture` run cannot turn it into
    /// escape codes and fail the string assertions below.
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

    /// The line a user reads for each open port. Measured on a real
    /// server before the fix: a never-started service's port was listed
    /// exactly like a running one, and the user went to debug a
    /// server-side service that had never been brought up.
    #[test]
    fn an_open_port_with_nobody_behind_it_is_listed_with_the_reason() {
        let mut t = crate::intent::TunnelState {
            port: 18083,
            service: "later".into(),
            open: true,
            service_running: Some(false),
        };
        let line = tunnel_line(&t, &plain());
        assert!(line.starts_with("localhost:18083"), "{line}");
        assert!(
            line.contains("later") && line.contains("no container"),
            "{line}"
        );

        t.service_running = Some(true);
        assert!(!tunnel_line(&t, &plain()).contains("no container"));
        t.service_running = None;
        assert!(
            !tunnel_line(&t, &plain()).contains("no container"),
            "an older service's report does not know, and must not cry wolf"
        );
    }

    #[test]
    fn a_port_with_nobody_behind_it_is_not_counted_home() {
        let plain = plain();
        let tunnel = |port, open, service_running| crate::intent::TunnelState {
            port,
            service: "svc".into(),
            open,
            service_running,
        };
        let st = crate::intent::Status {
            connection: "up".into(),
            tunnels: vec![
                tunnel(8080, true, Some(true)),
                tunnel(8081, true, None), // an older service: not known
                tunnel(8082, true, Some(false)),
                tunnel(8083, false, Some(true)),
            ],
            ..Default::default()
        };
        let line = fleet_detail(Some(&st), &plain);
        assert!(
            line.contains("localhost:8080") && line.contains("localhost:8081"),
            "{line}"
        );
        assert!(
            !line.contains("localhost:8082"),
            "an unanswered port listed as home is the lie this exists to remove: {line}"
        );
        assert!(line.contains("1 port(s) with no container"), "{line}");
        assert!(line.contains("1 not tunneled"), "{line}");
    }

    #[test]
    fn the_state_roll_up_separates_never_started_from_stopped() {
        // The measured hole: with `ps` (no -a) a stopped stack printed a
        // bare column header and exit 0, so "never came up" and
        // "crashed 43 hours ago" were the same output.
        assert_eq!(
            summarize_states("running\nrunning\nexited\n"),
            Stack::Counts("3 container(s): 2 running, 1 exited".into())
        );
        assert_eq!(
            summarize_states("exited\ndead\ncreated\n"),
            Stack::Counts("3 container(s): 0 running, 2 exited, 1 in between".into())
        );
        // Nothing under the label at all is NOT "0 running" — it is a
        // stack that has never existed here, and it gets its own line.
        assert_eq!(summarize_states("\n  \n"), Stack::Absent);
        // A broken engine must not be reported as an empty stack: the
        // compose error printed above is the real news.
        assert_eq!(summarize_states("NO_ENGINE\n"), Stack::Unknown);
    }
}
