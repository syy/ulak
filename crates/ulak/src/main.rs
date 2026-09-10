mod agent;
mod audit;
mod bake;
mod bridge;
mod buildflags;
mod catalog;
mod cli;
mod commands;
mod compose;
mod composepaths;
mod config;
mod configcmd;
mod docker;
mod dockerignore;
mod doctor;
mod footprint;
mod forward;
mod hashid;
mod init;
mod intent;
mod invocation;
mod ledger;
mod lockfile;
mod management;
mod manifest;
mod passthrough;
mod proc;
mod runspec;
mod service;
mod shim;
mod ssh;
mod stack;
mod sync;
mod ui;
mod walk;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command, ServiceCommand, ShimCommand};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            ui::render_error(&err);
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    // Compose globals typed before the command feed EVERY command,
    // including ulak's own — otherwise a repo whose files are named
    // compose.dev.yaml has no way in at all.
    let globals = cli.globals();
    // ulak is on this machine, so the service is on this machine —
    // not hung off `init` (which an upgrading user never runs again) and
    // not hung off `up -d` (whose job is a stack, not a login item).
    // `service` itself is excluded: the agent must not install itself,
    // and `completions` writes to stdout, which must stay pure.
    if !matches!(
        cli.command,
        Command::Service { .. } | Command::Completions { .. } | Command::Unrecognized(..)
    ) {
        // Before the agent, deliberately: the failure path in
        // `agent::ensure` points the user at the global config for the
        // off switch, and that sentence should name a file that exists.
        init::ensure_global();
        agent::ensure();
    }
    match cli.command {
        Command::Init { host } => {
            init::run(host, &globals)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Sync {
            dry_run,
            max_delete,
        } => {
            let mut command_args = Vec::new();
            if dry_run {
                command_args.push("--dry-run".to_string());
            }
            if let Some(value) = &max_delete {
                command_args.push("--max-delete".to_string());
                command_args.push(value.clone());
            }
            let project = management::project(&globals, "sync", &command_args)?;
            let b = config::bind_located(project)?;
            let stack_id = intent::stack_id(&b.dest, &b.project.compose_identity());
            if let Some(said) = agent::stack_complaint(&stack_id, "sync") {
                ui::warn(&said);
            }
            ui::info(&format!(
                "{}  →  {}:{}",
                b.project.name,
                b.ssh.dest,
                b.project.remote_dir_shown()
            ));
            // The human waits for the lock; only the service steps
            // aside. Both then run the SAME engine, which is what stops
            // a second one from growing.
            let _lock = (!dry_run)
                .then(|| lockfile::WorkspaceLock::acquire(b.project.workspace_id()))
                .transpose()?;
            // Parsed here rather than by clap so the refusal quotes the
            // value as it was typed, like every other Ulak refusal.
            let budget = max_delete
                .as_deref()
                .map(config::parse_delete_budget)
                .transpose()?;
            sync::reconcile_once(
                &b.project,
                &b.footprint,
                &b.ssh,
                &sync::SyncOptions {
                    dry_run,
                    max_delete_override: budget,
                    quiet: false,
                    over_budget: sync::OverBudget::Ask,
                },
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor => doctor::run(&globals),
        Command::Service { command } => match command {
            ServiceCommand::Run { foreground } => service::run(foreground),
            ServiceCommand::Install { no_start } => agent::install(!no_start),
            ServiceCommand::Uninstall => agent::uninstall(),
        },
        Command::Config { json } => configcmd::run(&globals, json),
        Command::Status => commands::status(&globals),
        Command::Clean { forget_destination } => {
            commands::clean(&globals, forget_destination.as_deref())
        }
        Command::Docker { args } => docker::dispatch(&globals, args),
        Command::Shim { command } => match command {
            ShimCommand::Install => shim::install(),
            ShimCommand::Uninstall => shim::uninstall(),
            ShimCommand::Status => shim::status(),
            ShimCommand::Run { args } => shim::run(args),
        },
        Command::Completions { shell } => {
            let mut cmd = cli::completion_command();
            clap_complete::generate(shell, &mut cmd, "ulak", &mut std::io::stdout());
            Ok(ExitCode::SUCCESS)
        }
        Command::Unrecognized(argv) => {
            let name = argv
                .first()
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<empty>".into());
            let mut err = ui::fail!("`ulak {name}` is not a root command");
            let forwarded = argv
                .iter()
                .map(|v| v.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            if cli::is_docker_command(&name) {
                err = err.now(format!(
                    "Docker commands live below `ulak docker`: ulak docker {forwarded}"
                ));
            } else if catalog::find(&["compose", &name]).is_some() {
                err = err.now(format!(
                    "Compose lives in Docker's tree: ulak docker compose {forwarded}"
                ));
            } else {
                err = err.now(format!(
                    "Docker commands live below `ulak docker`: ulak docker {name} …"
                ));
            }
            Err(err.into_err())
        }
    }
}
