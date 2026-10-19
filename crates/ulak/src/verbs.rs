//! What the dashboard may ask the server to do, and through which door.
//!
//! The dashboard builds argv from rows it shows, never from free text,
//! and every daemon command it builds still goes through
//! `catalog::resolve`, which must answer `Route::Daemon` or
//! `Route::Stream`. The routing table stays the one answer to "how does
//! this reach the server": a verb whose route ever changes is refused
//! here by name instead of being forwarded on a guess.
//!
//! Anything that touches the workspace or a stack's declared lifecycle —
//! Compose, `sync`, `doctor`, and the command line typed after `:` — is
//! Ulak itself, run as a child process with the project's exact selectors
//! (`management::compose_spawn`). The dashboard therefore grows no second
//! implementation of sync, intent, nudges or tunnels; it watches the same
//! commands a user would type.
//!
//! A destructive verb carries the question the dashboard asks before
//! running it. The dialog's answer defaults to No, as `ui::confirm` does.

use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;

use crate::catalog::{self, Route};
use crate::docker::{Remote, Tty};
use crate::engine::{Container, Image, Network, Usage, Volume, Want};
use crate::ssh::Ssh;
use crate::ui::fail;

/// How many lines of history a log view starts with.
pub const LOG_TAIL: usize = 500;

/// The interactive shell `e` opens: bash where the image has it.
const SHELL: &str = "if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi";

/// One thing a keypress asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    /// What the task list calls it: `stop web`.
    pub title: String,
    /// Asked first when set; No unless the user says yes.
    pub confirm: Option<String>,
    pub plan: Plan,
    /// The sections to re-read once it is done.
    pub refresh: Want,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// `docker <argv>` on the server, validated against the catalog.
    Daemon(Vec<String>),
    /// Ulak itself: `ulak <args>` started in `cwd`.
    Ulak(Driven),
}

/// A stack the dashboard can drive with Ulak's own commands: the current
/// project, or a stack this machine declared. Built once, from the
/// `Project`, through `management::*_spawn`.
#[derive(Debug, Clone, PartialEq)]
pub struct Driven {
    pub cwd: PathBuf,
    pub env: Option<(&'static str, String)>,
    /// Words after `ulak`.
    pub args: Vec<String>,
}

/// How the dashboard reaches one Compose stack's lifecycle.
#[derive(Debug, Clone, PartialEq)]
pub struct Driver {
    pub cwd: PathBuf,
    pub env: Option<(&'static str, String)>,
    /// `docker compose <selectors>` — the words after `ulak`.
    pub compose: Vec<String>,
    /// `<selectors>` — what root commands take before their name.
    pub root: Vec<String>,
}

impl Driver {
    pub fn of(project: &crate::config::Project) -> Driver {
        let compose = crate::management::compose_spawn(project, &[]);
        let root = crate::management::root_spawn(project, &[]);
        Driver {
            cwd: project.inv.cwd.clone(),
            env: compose.env,
            compose: compose.words.into_iter().skip(1).collect(),
            root: root.words.into_iter().skip(1).collect(),
        }
    }

    fn compose(&self, tail: &[&str]) -> Plan {
        let mut args = self.compose.clone();
        args.extend(tail.iter().map(|w| (*w).to_string()));
        self.ulak(args)
    }

    fn root(&self, name: &str) -> Plan {
        let mut args = self.root.clone();
        args.push(name.to_string());
        self.ulak(args)
    }

    fn ulak(&self, args: Vec<String>) -> Plan {
        Plan::Ulak(Driven {
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            args,
        })
    }
}

fn containers() -> Want {
    Want::default()
}

fn names(cs: &[&Container]) -> String {
    match cs {
        [one] => one.name.clone(),
        many => format!("{} containers", many.len()),
    }
}

fn ids(verb: &[&str], cs: &[&Container]) -> Vec<String> {
    verb.iter()
        .map(|w| (*w).to_string())
        .chain(cs.iter().map(|c| c.id.clone()))
        .collect()
}

pub fn start(cs: &[&Container]) -> Action {
    Action {
        title: format!("start {}", names(cs)),
        confirm: None,
        plan: Plan::Daemon(ids(&["start"], cs)),
        refresh: containers(),
    }
}

pub fn stop(cs: &[&Container]) -> Action {
    Action {
        title: format!("stop {}", names(cs)),
        confirm: None,
        plan: Plan::Daemon(ids(&["stop"], cs)),
        refresh: containers(),
    }
}

pub fn restart(cs: &[&Container]) -> Action {
    Action {
        title: format!("restart {}", names(cs)),
        confirm: None,
        plan: Plan::Daemon(ids(&["restart"], cs)),
        refresh: containers(),
    }
}

pub fn pause(c: &Container) -> Action {
    let (verb, word) = if c.paused() {
        ("unpause", "resume")
    } else {
        ("pause", "pause")
    };
    Action {
        title: format!("{word} {}", c.name),
        confirm: None,
        plan: Plan::Daemon(ids(&[verb], &[c])),
        refresh: containers(),
    }
}

pub fn kill(c: &Container) -> Action {
    Action {
        title: format!("kill {}", c.name),
        confirm: Some(format!(
            "Kill {}? It gets SIGKILL now, with no chance to shut down cleanly.",
            c.name
        )),
        plan: Plan::Daemon(ids(&["kill"], &[c])),
        refresh: containers(),
    }
}

/// `rm`, forced only when something is still running — the question says
/// so, because that is the part that cannot be taken back.
pub fn remove(cs: &[&Container]) -> Action {
    let running = cs.iter().filter(|c| c.running() || c.paused()).count();
    let verb: &[&str] = if running > 0 { &["rm", "-f"] } else { &["rm"] };
    let what = names(cs);
    let stopping = match (cs.len(), running) {
        (_, 0) => "",
        (1, _) => " It is running and will be stopped first.",
        (_, 1) => " One is running and will be stopped first.",
        _ => " The running ones will be stopped first.",
    };
    let question =
        format!("Delete {what}?{stopping} Its files are gone for good; named volumes stay.");
    Action {
        title: format!("delete {what}"),
        confirm: Some(question),
        plan: Plan::Daemon(ids(verb, cs)),
        refresh: Want {
            images: true,
            volumes: true,
            ..Want::default()
        },
    }
}

/// Untag by name where there is one: `rmi <id>` refuses an image two
/// tags share, while `rmi repo:tag` removes exactly the row selected.
pub fn remove_image(image: &Image) -> Action {
    let target = if image.dangling() {
        image.short_id().to_string()
    } else {
        image.reference()
    };
    Action {
        title: format!("delete image {target}"),
        confirm: Some(format!(
            "Delete image {target}? Pulling or building it again is the only way back."
        )),
        plan: Plan::Daemon(vec!["rmi".into(), target]),
        refresh: Want {
            images: true,
            usage: true,
            ..Want::default()
        },
    }
}

pub fn pull_image(image: &Image) -> Option<Action> {
    if image.dangling() || image.repository == "<none>" {
        return None;
    }
    let reference = image.reference();
    Some(Action {
        title: format!("pull {reference}"),
        confirm: None,
        plan: Plan::Daemon(vec!["pull".into(), reference]),
        refresh: Want {
            images: true,
            usage: true,
            ..Want::default()
        },
    })
}

/// `docker system df`'s RECLAIMABLE without its share: `3.404GB`.
fn reclaimable(u: &Usage) -> &str {
    u.reclaimable.split(" (").next().unwrap_or_default().trim()
}

/// Every image no container uses: `-a`. Plain `image prune` takes only
/// dangling ones, while the number shown beside `P` is `system df`'s,
/// which counts every unused image. Measured on Docker 29.8.1: 3.404GB
/// reclaimable across 5 unused images, and `docker images -f
/// dangling=true` empty — the old `P` had nothing to take.
pub fn prune_images(usage: Option<&Usage>) -> Action {
    let counted = usage
        .and_then(|u| {
            let total: u64 = u.total.trim().parse().ok()?;
            let active: u64 = u.active.trim().parse().ok()?;
            Some(format!(
                " ({} of {total}, {})",
                total.saturating_sub(active),
                reclaimable(u)
            ))
        })
        .unwrap_or_default();
    Action {
        title: "prune unused images".into(),
        confirm: Some(format!(
            "Delete every image no container uses{counted}? A stack that is down pulls or builds its images again on its next up."
        )),
        plan: Plan::Daemon(vec![
            "image".into(),
            "prune".into(),
            "-a".into(),
            "-f".into(),
        ]),
        refresh: Want {
            images: true,
            usage: true,
            ..Want::default()
        },
    }
}

/// The build cache, which the dashboard never showed: 9.975GB on that
/// same server, more than all its images. Plain `builder prune`, as
/// typing it would; `-a` also takes what BuildKit keeps by default. What
/// it freed is Docker's to say, and the task's toast repeats it.
pub fn prune_build_cache(usage: Option<&Usage>) -> Action {
    let counted = usage
        .map(|u| format!(" ({} of {})", reclaimable(u), u.size))
        .unwrap_or_default();
    Action {
        title: "prune build cache".into(),
        confirm: Some(format!(
            "Delete the build cache Docker counts as reclaimable{counted}? Builds after this start colder."
        )),
        plan: Plan::Daemon(vec!["builder".into(), "prune".into(), "-f".into()]),
        refresh: Want {
            usage: true,
            ..Want::default()
        },
    }
}

pub fn remove_volume(volume: &Volume) -> Action {
    Action {
        title: format!("delete volume {}", volume.name),
        confirm: Some(format!(
            "Delete volume {}? The data in it is gone for good.",
            volume.name
        )),
        plan: Plan::Daemon(vec!["volume".into(), "rm".into(), volume.name.clone()]),
        refresh: Want {
            volumes: true,
            usage: true,
            ..Want::default()
        },
    }
}

/// Anonymous volumes only, by their label as well as by Docker's default:
/// before Docker 23, `volume prune` took every unused volume, named ones
/// — a stack's database — among them. With the filter an older engine,
/// which labels nothing, prunes nothing instead. On Docker 29.8.1 every
/// anonymous volume carries `com.docker.volume.anonymous=`, and a
/// Compose-named one does not.
pub fn prune_volumes() -> Action {
    Action {
        title: "prune unused anonymous volumes".into(),
        confirm: Some(
            "Delete every anonymous volume no container uses? Their data is gone for good. Named volumes stay; d deletes one.".into(),
        ),
        plan: Plan::Daemon(vec![
            "volume".into(),
            "prune".into(),
            "-f".into(),
            "--filter".into(),
            "label=com.docker.volume.anonymous".into(),
        ]),
        refresh: Want {
            volumes: true,
            usage: true,
            ..Want::default()
        },
    }
}

pub fn remove_network(network: &Network) -> Action {
    Action {
        title: format!("delete network {}", network.name),
        confirm: Some(format!("Delete network {}?", network.name)),
        plan: Plan::Daemon(vec!["network".into(), "rm".into(), network.id.clone()]),
        refresh: Want {
            networks: true,
            ..Want::default()
        },
    }
}

pub fn prune_networks() -> Action {
    Action {
        title: "prune unused networks".into(),
        confirm: Some("Delete every network no container uses?".into()),
        plan: Plan::Daemon(vec!["network".into(), "prune".into(), "-f".into()]),
        refresh: Want {
            networks: true,
            ..Want::default()
        },
    }
}

pub fn prune_containers() -> Action {
    Action {
        title: "prune stopped containers".into(),
        confirm: Some("Delete every stopped container? Their files are gone for good.".into()),
        plan: Plan::Daemon(vec!["container".into(), "prune".into(), "-f".into()]),
        refresh: Want {
            images: true,
            usage: true,
            ..Want::default()
        },
    }
}

/// A Compose lifecycle verb on a stack the dashboard can drive.
pub fn compose(driver: &Driver, stack: &str, tail: &[&str]) -> Action {
    let words = tail.join(" ");
    let confirm = match tail.first().copied() {
        Some("down") => Some(format!(
            "Take {stack} down? Its containers and networks are removed; volumes stay. The service stops keeping it."
        )),
        _ => None,
    };
    Action {
        title: format!("compose {words} ({stack})"),
        confirm,
        plan: driver.compose(tail),
        refresh: Want::all(),
    }
}

pub fn sync(driver: &Driver, stack: &str) -> Action {
    Action {
        title: format!("sync {stack}"),
        confirm: None,
        plan: driver.root("sync"),
        refresh: Want::default(),
    }
}

pub fn doctor(driver: &Driver, stack: &str) -> Action {
    Action {
        title: format!("doctor {stack}"),
        confirm: None,
        plan: driver.root("doctor"),
        refresh: Want::default(),
    }
}

/// What `ulak service install` does to the service this machine has.
/// The button beside a service problem and the question `I` asks both
/// name it from here, so they cannot disagree: `App::service_fix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// No login agent and nothing running: it writes one and starts it.
    Fresh,
    /// The agent is written and runs nothing that reports: it is
    /// rewritten for this ulak and started, which ends a silent one.
    Again,
    /// The agent's service runs, and is restarted on this ulak.
    Restart,
    /// A service started by hand runs without an agent. It stays in
    /// charge: the agent written now steps aside for it
    /// (`lockfile::ServiceLock`) and takes over at the next login.
    Beside,
}

impl Install {
    pub fn of(running: bool, installed: bool) -> Install {
        match (running, installed) {
            (false, false) => Install::Fresh,
            (false, true) => Install::Again,
            (true, true) => Install::Restart,
            (true, false) => Install::Beside,
        }
    }

    /// The words after `I` on that button.
    pub fn button(self) -> &'static str {
        match self {
            Install::Fresh | Install::Beside => "install it",
            Install::Again => "start it",
            Install::Restart => "restart it",
        }
    }
}

/// `ulak service install`, the fix every service problem names, asked as
/// what it will do to the service there is. A restart says so: whatever
/// the service is in the middle of starts over.
pub fn install_service(cwd: PathBuf, how: Install) -> Action {
    let (title, question) = match how {
        Install::Fresh => (
            "install the Ulak service",
            "Install Ulak's background service and start it (ulak service install)? It starts again at every login, syncs the stacks you leave up and forwards their ports. ulak service uninstall removes it.",
        ),
        Install::Again => (
            "start the Ulak service",
            "Start Ulak's background service again (ulak service install)? It rewrites the login agent for this ulak and starts it, so the stacks you leave up are synced and their ports forwarded.",
        ),
        Install::Restart => (
            "restart the Ulak service",
            "Restart Ulak's background service on this ulak (ulak service install)? Its tunnels close for a moment, and a sync it is in the middle of starts over.",
        ),
        Install::Beside => (
            "install the Ulak service",
            "Install Ulak's background service (ulak service install)? The one you started by hand stays in charge until it stops; from your next login, the installed one syncs the stacks you leave up and forwards their ports.",
        ),
    };
    Action {
        title: title.into(),
        confirm: Some(question.into()),
        plan: Plan::Ulak(Driven {
            cwd,
            env: None,
            args: vec!["service".into(), "install".into()],
        }),
        refresh: Want::default(),
    }
}

/// A command typed after `:` — `compose pull`, `image ls`, … — run as
/// `ulak docker <words>` so the catalog and every route's own rules
/// apply exactly as on the command line. Words are split like a shell
/// would for the plain cases; a quote the splitter cannot read is
/// refused rather than guessed at.
pub fn typed(cwd: PathBuf, env: Option<(&'static str, String)>, line: &str) -> Result<Action> {
    let mut words = split_words(line).ok_or_else(|| {
        fail!("cannot read the quotes in `{line}`")
            .now("type it in a terminal instead: ulak docker …")
            .into_err()
    })?;
    if words.first().map(String::as_str) == Some("docker") {
        words.remove(0);
    }
    if words.is_empty() {
        return Err(fail!("nothing to run")
            .now("type a Docker command, e.g. :compose pull")
            .into_err());
    }
    let mut args = vec!["docker".to_string()];
    args.extend(words.iter().cloned());
    Ok(Action {
        title: format!("docker {}", words.join(" ")),
        confirm: None,
        plan: Plan::Ulak(Driven { cwd, env, args }),
        refresh: Want::all(),
    })
}

/// Shell-like word splitting for one line: spaces separate, single and
/// double quotes group, backslash escapes outside single quotes.
/// `None` for an unterminated quote.
fn split_words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => word.push(chars.next()?),
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                started = true;
                word.push(chars.next()?);
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        words.push(word);
    }
    Some(words)
}

// ─── streams and reads ──────────────────────────────────────────────

/// Through the catalog, which must answer with a daemon-side route.
fn checked(argv: Vec<String>) -> Result<catalog::Resolved> {
    let resolved = catalog::resolve(&argv)?;
    match resolved.entry.route {
        Route::Daemon | Route::Stream => Ok(resolved),
        _ => Err(fail!(
            "`docker {}` does not route straight to the daemon",
            resolved.entry.name()
        )
        .now(
            "the dashboard only sends daemon commands itself — run it in a terminal: ulak docker …",
        )
        .into_err()),
    }
}

pub fn logs_argv(id: &str) -> Result<Vec<String>> {
    Ok(checked(vec![
        "logs".into(),
        "--follow".into(),
        "--timestamps".into(),
        "--tail".into(),
        LOG_TAIL.to_string(),
        id.into(),
    ])?
    .argv)
}

pub fn stats_argv() -> Result<Vec<String>> {
    Ok(checked(vec!["stats".into(), "--format".into(), "{{json .}}".into()])?.argv)
}

pub fn events_argv() -> Result<Vec<String>> {
    let mut argv: Vec<String> = vec!["events".into(), "--format".into(), "{{json .}}".into()];
    for kind in ["container", "image", "volume", "network"] {
        argv.push("--filter".into());
        argv.push(format!("type={kind}"));
    }
    Ok(checked(argv)?.argv)
}

/// `kind` is Docker's object type: container, image, volume, network.
pub fn inspect_argv(kind: &str, id: &str) -> Result<Vec<String>> {
    Ok(checked(vec![
        "inspect".into(),
        "--type".into(),
        kind.into(),
        id.into(),
    ])?
    .argv)
}

/// The process for an action, plus the audit-safe remote line for a
/// daemon one (a Ulak child audits itself).
pub fn command(action: &Action, ssh: &Ssh) -> Result<(Command, Option<String>)> {
    match &action.plan {
        Plan::Daemon(argv) => {
            let resolved = checked(argv.clone())?;
            let (cmd, redacted) = Remote { ssh: ssh.clone() }.spell(
                &resolved.argv,
                None,
                Tty::Never,
                resolved.entry.secret_flags,
            );
            Ok((cmd, Some(redacted)))
        }
        Plan::Ulak(driven) => {
            let exe = std::env::current_exe().map_err(|e| {
                fail!("cannot find the running ulak binary: {e}")
                    .now("run the same command in a terminal instead")
                    .into_err()
            })?;
            let mut cmd = Command::new(exe);
            cmd.args(&driven.args).current_dir(&driven.cwd);
            if let Some((var, value)) = &driven.env {
                cmd.env(var, value);
            }
            Ok((cmd, None))
        }
    }
}

/// `docker exec -it <id> <bash or sh>`, for the foreground: the dashboard
/// gives the terminal back while it runs. The second value is the
/// audit-safe remote line.
pub fn shell(ssh: &Ssh, c: &Container) -> Result<(Command, String)> {
    let resolved = checked(vec![
        "exec".into(),
        "-it".into(),
        c.id.clone(),
        "sh".into(),
        "-c".into(),
        SHELL.into(),
    ])?;
    Ok(Remote { ssh: ssh.clone() }.spell(
        &resolved.argv,
        None,
        Tty::Force,
        resolved.entry.secret_flags,
    ))
}

/// The local opener for a URL: `open` on macOS, `xdg-open` elsewhere.
pub fn opener(url: &str) -> Command {
    let mut cmd = Command::new(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    });
    cmd.arg(url);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(name: &str, state: &str) -> Container {
        Container {
            id: format!("{name}-0123456789abcdef"),
            name: name.into(),
            state: state.into(),
            ..Container::default()
        }
    }

    /// Every daemon argv a verb builds is one the catalog routes to the
    /// daemon; a route change would turn this red before it turned into
    /// a forwarded guess.
    #[test]
    fn every_daemon_verb_routes_to_the_daemon_through_the_catalog() {
        let web = container("web", "running");
        let image = Image {
            id: "sha256:df221db836e1".into(),
            repository: "nginx".into(),
            tag: "alpine".into(),
            ..Image::default()
        };
        let volume = Volume {
            name: "demo_db".into(),
            ..Volume::default()
        };
        let network = Network {
            id: "28fcf94110f9".into(),
            name: "demo_default".into(),
            ..Network::default()
        };
        let actions = vec![
            start(&[&web]),
            stop(&[&web]),
            restart(&[&web]),
            pause(&web),
            kill(&web),
            remove(&[&web]),
            remove_image(&image),
            pull_image(&image).unwrap(),
            prune_images(None),
            prune_build_cache(None),
            remove_volume(&volume),
            prune_volumes(),
            remove_network(&network),
            prune_networks(),
            prune_containers(),
        ];
        assert!(!actions.is_empty());
        for action in actions {
            let Plan::Daemon(argv) = &action.plan else {
                panic!("{} should be a daemon verb", action.title);
            };
            let resolved = checked(argv.clone()).unwrap();
            assert_eq!(&resolved.argv, argv, "{}", action.title);
        }
        for argv in [
            logs_argv("abc").unwrap(),
            stats_argv().unwrap(),
            events_argv().unwrap(),
            inspect_argv("container", "abc").unwrap(),
        ] {
            assert!(!argv.is_empty());
        }
    }

    fn usage(kind: &str, total: &str, active: &str, size: &str, reclaimable: &str) -> Usage {
        Usage {
            kind: kind.into(),
            total: total.into(),
            active: active.into(),
            size: size.into(),
            reclaimable: reclaimable.into(),
        }
    }

    #[test]
    fn pruning_images_takes_every_unused_one_and_asks_with_dockers_own_numbers() {
        let images = usage("Images", "19", "14", "8.978GB", "3.404GB (37%)");
        let action = prune_images(Some(&images));
        assert_eq!(
            action.plan,
            Plan::Daemon(vec![
                "image".into(),
                "prune".into(),
                "-a".into(),
                "-f".into()
            ]),
            "plain `image prune` takes dangling images only"
        );
        let question = action.confirm.unwrap();
        assert!(question.contains("(5 of 19, 3.404GB)"), "{question}");
        assert!(question.contains("pulls or builds"), "{question}");
        // Before the first `system df` there is nothing to quote.
        let question = prune_images(None).confirm.unwrap();
        assert!(
            question.starts_with("Delete every image no container uses?"),
            "{question}"
        );
    }

    #[test]
    fn pruning_the_build_cache_quotes_what_docker_counts() {
        let cache = usage("Build Cache", "264", "11", "9.975GB", "3.532GB");
        let action = prune_build_cache(Some(&cache));
        assert_eq!(
            action.plan,
            Plan::Daemon(vec!["builder".into(), "prune".into(), "-f".into()])
        );
        let question = action.confirm.unwrap();
        assert!(question.contains("(3.532GB of 9.975GB)"), "{question}");
        assert!(action.refresh.usage);
    }

    #[test]
    fn pruning_volumes_names_anonymous_ones_on_every_engine() {
        let Plan::Daemon(argv) = prune_volumes().plan else {
            panic!("a daemon verb");
        };
        let filter = argv.iter().position(|a| a == "--filter").unwrap();
        assert_eq!(argv[filter + 1], "label=com.docker.volume.anonymous");
        let question = prune_volumes().confirm.unwrap();
        assert!(question.contains("Named volumes stay"), "{question}");
    }

    #[test]
    fn a_route_that_is_not_the_daemon_is_refused_by_name() {
        for argv in [
            vec!["build", "."],
            vec!["compose", "up"],
            vec!["run", "alpine"],
        ] {
            let err = checked(argv.iter().map(|s| s.to_string()).collect()).unwrap_err();
            let err = crate::ui::flatten(&err);
            assert!(
                err.contains("does not route straight to the daemon"),
                "{err}"
            );
        }
    }

    #[test]
    fn deleting_a_running_container_forces_and_says_so() {
        let web = container("web", "running");
        let action = remove(&[&web]);
        assert_eq!(
            action.plan,
            Plan::Daemon(vec!["rm".into(), "-f".into(), web.id.clone()])
        );
        assert!(
            action
                .confirm
                .unwrap()
                .contains("It is running and will be stopped first")
        );
        let gone = container("old", "exited");
        let action = remove(&[&gone]);
        assert_eq!(
            action.plan,
            Plan::Daemon(vec!["rm".into(), gone.id.clone()])
        );
        assert!(!action.confirm.unwrap().contains("running"));
    }

    #[test]
    fn every_destructive_verb_asks_and_the_harmless_ones_do_not() {
        let web = container("web", "running");
        assert!(start(&[&web]).confirm.is_none());
        assert!(stop(&[&web]).confirm.is_none());
        assert!(restart(&[&web]).confirm.is_none());
        assert!(kill(&web).confirm.is_some());
        assert!(remove(&[&web]).confirm.is_some());
        assert!(prune_images(None).confirm.is_some());
        assert!(prune_build_cache(None).confirm.is_some());
        assert!(prune_volumes().confirm.is_some());
        assert!(prune_networks().confirm.is_some());
        assert!(prune_containers().confirm.is_some());
    }

    #[test]
    fn an_image_is_removed_by_the_tag_selected_and_a_dangling_one_by_id() {
        let tagged = Image {
            id: "df221db836e1".into(),
            repository: "nginx".into(),
            tag: "alpine".into(),
            ..Image::default()
        };
        assert_eq!(
            remove_image(&tagged).plan,
            Plan::Daemon(vec!["rmi".into(), "nginx:alpine".into()])
        );
        let dangling = Image {
            id: "c4624b89a382".into(),
            repository: "<none>".into(),
            tag: "<none>".into(),
            ..Image::default()
        };
        assert_eq!(
            remove_image(&dangling).plan,
            Plan::Daemon(vec!["rmi".into(), "c4624b89a382".into()])
        );
        assert!(pull_image(&dangling).is_none(), "nothing names it to pull");
    }

    #[test]
    fn compose_verbs_are_ulak_with_the_stacks_own_selectors() {
        let driver = Driver {
            cwd: PathBuf::from("/repo"),
            env: Some(("ULAK_HOST", "old".into())),
            compose: vec![
                "docker".into(),
                "compose".into(),
                "-f".into(),
                "/repo/compose.yaml".into(),
            ],
            root: vec!["-f".into(), "/repo/compose.yaml".into()],
        };
        let up = compose(&driver, "demo", &["up", "-d"]);
        assert!(up.confirm.is_none());
        let Plan::Ulak(driven) = up.plan else {
            panic!("compose must be Ulak itself");
        };
        assert_eq!(
            driven.args,
            vec!["docker", "compose", "-f", "/repo/compose.yaml", "up", "-d"]
        );
        assert_eq!(driven.env, Some(("ULAK_HOST", "old".into())));
        assert!(compose(&driver, "demo", &["down"]).confirm.is_some());
        let Plan::Ulak(driven) = sync(&driver, "demo").plan else {
            panic!("sync must be Ulak itself");
        };
        assert_eq!(driven.args, vec!["-f", "/repo/compose.yaml", "sync"]);
    }

    #[test]
    fn a_typed_line_is_split_like_a_shell_and_runs_below_ulak_docker() {
        let action = typed(
            PathBuf::from("/x"),
            None,
            "docker compose logs --tail 5 'my web'",
        )
        .unwrap();
        let Plan::Ulak(driven) = action.plan else {
            panic!("typed commands are Ulak itself");
        };
        assert_eq!(
            driven.args,
            vec!["docker", "compose", "logs", "--tail", "5", "my web"]
        );
        assert_eq!(
            split_words(r#"a "b \" c" d\ e ''"#).unwrap(),
            vec!["a", "b \" c", "d e", ""]
        );
        assert!(split_words("unterminated 'quote").is_none());
        assert!(typed(PathBuf::from("/x"), None, "docker").is_err());
    }

    #[test]
    fn the_shell_asks_for_a_terminal_on_the_far_side() {
        let ssh = Ssh::new_in(
            "example",
            &std::collections::BTreeMap::new(),
            false,
            std::path::Path::new("/nonexistent"),
        )
        .unwrap();
        let (cmd, redacted) = shell(&ssh, &container("web", "running")).unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"-t".to_string()), "{args:?}");
        assert!(redacted.contains("docker exec -it"), "{redacted}");
    }
}
