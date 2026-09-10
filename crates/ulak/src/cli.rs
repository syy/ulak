use std::ffi::OsString;

use clap::{Parser, Subcommand};

/// Ulak — run Docker workloads on your own server while you edit locally.
///
/// Use `ulak init <ssh-host>` to configure a project, then `ulak doctor`
/// to check it. Run Docker commands with `ulak docker <command>`.

#[derive(Parser, Debug)]
#[command(name = "ulak", version, about, max_term_width = 100)]
pub struct Cli {
    /// Compose file, repeatable — exactly like `docker compose -f`
    ///
    /// For Ulak management commands this selects the project model.
    /// Docker Compose globals may instead stay in Docker's own tree:
    /// `ulak docker compose -f compose.dev.yaml up -d`.
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    pub file: Vec<String>,

    /// Compose project name (`docker compose -p`)
    #[arg(short = 'p', long = "project-name", value_name = "NAME")]
    pub project_name: Option<String>,

    /// Enable a compose profile, repeatable
    #[arg(long, value_name = "PROFILE")]
    pub profile: Vec<String>,

    /// Alternate env file, repeatable (`docker compose --env-file`)
    #[arg(long, value_name = "FILE")]
    pub env_file: Vec<String>,

    /// Base directory for relative paths (`docker compose --project-directory`)
    #[arg(long, value_name = "DIR")]
    pub project_directory: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// The compose globals, back in compose's own argv shape, so the one
    /// canonical invocation is built from a single kind of input no
    /// matter which door the user came through.
    pub fn globals(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut push = |flag: &str, value: &str| {
            out.push(flag.to_string());
            out.push(value.to_string());
        };
        for f in &self.file {
            push("-f", f);
        }
        if let Some(p) = &self.project_name {
            push("-p", p);
        }
        for p in &self.profile {
            push("--profile", p);
        }
        for e in &self.env_file {
            push("--env-file", e);
        }
        if let Some(d) = &self.project_directory {
            push("--project-directory", d);
        }
        out
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create an ulak.toml for this project (optional convenience)
    Init {
        /// SSH destination to write; omit to create a template
        host: Option<String>,
    },

    /// Preflight checks: local tools, remote server, compose file references
    Doctor,

    /// Synchronize project files with the server once
    ///
    /// Send local changes and bring back files written by the stack.
    /// Only paths referenced by the Compose model are synchronized.
    Sync {
        /// Preview uploads and remote deletions without applying them
        ///
        /// Resolving the Compose model may upload temporary configuration
        /// files to the server. Downloads are not previewed.
        #[arg(long)]
        dry_run: bool,
        /// Allow up to N server-side deletions without asking, or
        /// `unlimited` to lift the guard
        /// (overrides [sync] max_delete from the config)
        #[arg(long, value_name = "N")]
        max_delete: Option<String>,
    },

    /// Manage the local background service for sync and port forwarding
    ///
    /// The service watches files, brings back container-generated files,
    /// and maintains SSH tunnels for running stacks.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },

    /// Every setting in effect, and which layer won it
    ///
    /// Reports; it does not judge. `doctor` is the one that reaches the
    /// server and decides whether this machine is ready.
    Config {
        /// Print one JSON object instead of the report
        #[arg(long)]
        json: bool,
    },

    /// Show workspace target, compose ps and disk usage in one round-trip
    Status,

    /// Delete the remote workspace directory (refuses while a stack still uses it)
    Clean {
        /// Forget a destroyed server locally, without contacting it
        ///
        /// Named in full, never a bare --force: this retires the stack
        /// declarations this checkout made on THAT server, and typing it
        /// out is the assertion that the server is gone. Files already sent
        /// there are neither deleted nor forgotten.
        #[arg(long, value_name = "SSH_DEST")]
        forget_destination: Option<String>,
    },

    // Keep routing in catalog: the old clap variants left 316 of Docker's 327
    // command paths unreachable. catalog::tests pins this count.
    /// Run a supported Docker command on the configured server
    ///
    /// Use `ulak docker --help` to see the supported command tree.
    #[command(disable_help_flag = true)]
    Docker {
        #[arg(value_name = "ARGV", num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },

    /// A stand-in `docker` on your PATH, so a project's own scripts reach the server
    ///
    /// Use `ulak shim run -- <command>` for one script, or `ulak shim install`
    /// to make the stand-ins available in your shell.
    Shim {
        #[command(subcommand)]
        command: ShimCommand,
    },

    /// Print shell completions (bash/zsh/fish) to stdout
    ///
    /// e.g.  ulak completions zsh > "${fpath[1]}/_ulak"
    Completions {
        /// Shell to generate for
        shell: clap_complete::Shell,
    },

    /// A root command that belongs below `ulak docker`
    #[command(external_subcommand)]
    Unrecognized(Vec<OsString>),
}

/// Whether a name is an actual first-level command in Docker's tree.
/// Used only on the error path, to point a misplaced command home.
pub fn is_docker_command(name: &str) -> bool {
    crate::catalog::find(&[name]).is_some()
}

/// The clap command used for SHELL COMPLETIONS only.
///
/// `Cli::command()` describes `ulak docker` as one trailing-var-arg
/// bucket, which is right for parsing (Docker's flags are Docker's
/// business) and useless for completion — it would offer nothing after
/// the word `docker`. So the bucket is swapped for a tree built from
/// the catalog, which knows every command path and its description.
pub fn completion_command() -> clap::Command {
    use clap::CommandFactory;

    Cli::command().mut_subcommand("docker", |_| {
        crate::catalog::completion_tree(clap::Command::new("docker").about("Docker's command tree"))
    })
}

#[derive(Subcommand, Debug)]
pub enum ShimCommand {
    /// Write the stand-ins and offer to put them on your PATH
    ///
    /// The PATH line goes into one marked block, and only after you say
    /// yes — so this never edits a shell profile from a script. Run it
    /// again after moving the ulak binary; it replaces the block rather
    /// than adding a second one.
    Install,

    /// Remove the stand-ins and the block this added
    ///
    /// Everything outside Ulak's own two fence lines is left exactly
    /// where it was.
    Uninstall,

    /// Say whether the stand-ins exist, whether they are on this PATH,
    /// and which `docker` would run right now
    Status,

    /// Run one command with the stand-ins in front, installing nothing
    ///
    /// e.g.  ulak shim run -- ./scripts/start.sh
    #[command(disable_help_flag = true)]
    Run {
        #[arg(value_name = "ARGV", num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServiceCommand {
    /// Run the service here, in this terminal (Ctrl-C stops it)
    ///
    /// Every workspace you left `up` keeps syncing, keeps its ports on
    /// localhost, and comes back by itself after a suspend. This is the
    /// same thing the installed service does — the manual crank for when
    /// it is off, for CI, and for watching what it actually does.
    Run {
        /// Say what is happening, and run even when `[service] auto` is
        /// off — you are asking for it by hand
        #[arg(long)]
        foreground: bool,
    },

    /// Install (or refresh) the background service on this machine
    ///
    /// ulak does this by itself the first time you run it in a
    /// terminal, so you rarely need it. Running it again is also how you
    /// restart the service — after an upgrade, or once the binary has
    /// moved. Nothing restarts by itself: that would cut somebody's `up`
    /// in half.
    Install {
        /// Write the agent but do not start it now (it starts at your
        /// next login)
        #[arg(long)]
        no_start: bool,
    },

    /// Remove the installed service and its port tunnels
    ///
    /// To prevent automatic installation on the next terminal command,
    /// also set [service] auto = false in the global configuration.
    Uninstall,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("ulak").chain(args.iter().copied())).unwrap()
    }

    /// The argv `ulak docker` hands to the catalog.
    fn docker_argv(args: &[&str]) -> Vec<String> {
        let Command::Docker { args } = parse(args).command else {
            panic!("expected the docker branch");
        };
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn globals_before_the_command_are_ours() {
        // The shape a large monorepo actually uses: many compose files,
        // none of them named compose.yaml.
        let cli = parse(&[
            "-f",
            "compose.base.yaml",
            "-f",
            "compose.dev.yaml",
            "-p",
            "demo",
            "docker",
            "compose",
            "up",
            "-d",
        ]);
        assert_eq!(cli.file, vec!["compose.base.yaml", "compose.dev.yaml"]);
        assert_eq!(cli.project_name.as_deref(), Some("demo"));
        assert_eq!(
            cli.globals(),
            vec![
                "-f",
                "compose.base.yaml",
                "-f",
                "compose.dev.yaml",
                "-p",
                "demo"
            ]
        );
        let Command::Docker { args } = cli.command else {
            panic!("expected docker compose");
        };
        assert_eq!(args, vec!["compose", "up", "-d"]);
    }

    #[test]
    fn a_flag_after_the_command_belongs_to_the_command() {
        // `logs -f` is FOLLOW, not a file — the trap that makes wrapping
        // a CLI dangerous. Same rule compose itself uses: globals come
        // first, everything after the command is the command's.
        let cli = parse(&["docker", "compose", "logs", "-f", "web"]);
        assert!(cli.file.is_empty(), "-f after the command is not a file");
        assert!(cli.globals().is_empty());
        let Command::Docker { args } = cli.command else {
            panic!("expected docker compose");
        };
        assert_eq!(args, vec!["compose", "logs", "-f", "web"]);
    }

    #[test]
    fn docker_argv_reaches_the_catalog_untouched() {
        // Nothing between the user and Docker's own parser: clap must
        // not eat `-t`, `-f`, `--filter` or a bare `.`.
        assert_eq!(
            docker_argv(&["docker", "build", "-t", "demo", "."]),
            vec!["build", "-t", "demo", "."]
        );
        assert_eq!(
            docker_argv(&["docker", "network", "create", "shared"]),
            vec!["network", "create", "shared"]
        );
        assert_eq!(
            docker_argv(&["docker", "ps", "-aq", "--filter", "label=project"]),
            vec!["ps", "-aq", "--filter", "label=project"]
        );
        assert_eq!(
            docker_argv(&["docker", "info", "--format", "{{.ServerVersion}}"]),
            vec!["info", "--format", "{{.ServerVersion}}"]
        );
    }

    #[test]
    fn the_commands_the_old_enum_never_had_now_arrive() {
        // Every one of these used to die at clap with "unrecognized
        // subcommand" before reaching any routing decision at all.
        for argv in [
            vec!["docker", "exec", "-it", "web", "sh"],
            vec!["docker", "logs", "-f", "--tail", "50", "api"],
            vec!["docker", "run", "--rm", "-v", ".:/app", "alpine", "ls"],
            vec!["docker", "cp", "./x", "web:/x"],
            vec!["docker", "container", "prune", "-f"],
            vec!["docker", "system", "df", "-v"],
            vec!["docker", "buildx", "history", "inspect", "attachment", "id"],
        ] {
            let got = docker_argv(&argv);
            assert_eq!(
                got,
                argv[1..].iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "{argv:?} must arrive verbatim"
            );
        }
    }

    #[test]
    fn old_implicit_compose_commands_are_captured_only_for_guidance() {
        let cli = parse(&["up", "-d"]);
        let Command::Unrecognized(args) = cli.command else {
            panic!("the old shape must not become a real command");
        };
        assert_eq!(args, vec!["up", "-d"]);
    }

    #[test]
    fn ulaks_own_commands_take_them_too() {
        // Without this, a repo with non-standard names has no way in:
        // `init` and `doctor` could not find a project at all.
        for cmd in ["init", "doctor", "sync", "status"] {
            let cli = parse(&["-f", "compose.dev.yaml", cmd]);
            assert_eq!(
                cli.globals(),
                vec!["-f", "compose.dev.yaml"],
                "{cmd} must accept compose globals"
            );
        }
    }

    /// A misplaced doc comment once made `config` advertise status while
    /// `status` had no summary at all, hiding the command's actual contract.
    #[test]
    fn status_and_config_keep_their_own_help_summaries() {
        let command = Cli::command();
        let config = command
            .find_subcommand("config")
            .expect("config subcommand");
        let status = command
            .find_subcommand("status")
            .expect("status subcommand");

        assert_eq!(
            config.get_about().map(ToString::to_string).as_deref(),
            Some("Every setting in effect, and which layer won it")
        );
        assert_eq!(
            status.get_about().map(ToString::to_string).as_deref(),
            Some("Show workspace target, compose ps and disk usage in one round-trip")
        );
    }
}
