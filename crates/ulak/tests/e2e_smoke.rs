//! Phase 0 smoke: the binary skeleton runs, and says the right things
//! before it has been pointed at anything.
//!
//! Serverless, all of it, and that is a change: what used to stand here
//! as well was `server_has_everything_ulak_needs`, which asserted that
//! the fixture image has sshd, rsync >= 3.2, docker and compose v2. Every
//! one of those is a fact about `e2e/sshd/Dockerfile` rather than about
//! ulak, every one is already enforced by `wait_until_ready` or by
//! `ulak doctor`, and a drifted fixture is a loud failure across every
//! other binary in the suite. It cost this one its whole 19 s — a DinD
//! boot for four tests that finish in milliseconds. The single claim in
//! it that was about the SUITE rather than the image — that the exercised
//! ssh user is not root — moved to e2e_seed.rs, which owns the harness's
//! own contract and already has a server.

mod common;

use assert_cmd::Command as AssertCommand;
use common::Workspace;
use std::collections::BTreeSet;

#[test]
fn binary_reports_version() {
    AssertCommand::cargo_bin("ulak")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains("ulak"));
}

#[test]
fn completions_generate_for_all_shells() {
    for shell in ["bash", "zsh", "fish"] {
        AssertCommand::cargo_bin("ulak")
            .unwrap()
            .args(["completions", shell])
            .assert()
            .success()
            .stdout(predicates::str::contains("ulak"));
    }

    // Whole command names, never substrings: a `contains("rm")` stays
    // green after `rm` disappears, because `rmi` is still there.
    //
    // The EXHAUSTIVE pin moved. `docs/docker-commands.md` is generated
    // from the routing table and a unit test fails when the two drift,
    // which covers all 274 command paths instead of the top-level 58.
    // What is worth checking from out here is the property that file
    // cannot check: that the help a user actually reads still lists
    // them, once each, in the sections it claims to have.
    let help = AssertCommand::cargo_bin("ulak")
        .unwrap()
        .args(["docker", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();

    let mut listed: Vec<&str> = Vec::new();
    let mut sections = 0;
    let mut inside = false;
    for line in help.lines() {
        if line.ends_with("Commands:") {
            inside = true;
            sections += 1;
            continue;
        }
        if !inside {
            continue;
        }
        if line.trim().is_empty() {
            inside = false;
            continue;
        }
        if let Some(name) = line.split_whitespace().next() {
            listed.push(name);
        }
    }
    assert_eq!(
        sections, 3,
        "the tree is grouped Common / Management / Commands, like Docker's own:\n{help}"
    );

    let unique: BTreeSet<_> = listed.iter().copied().collect();
    assert_eq!(
        unique.len(),
        listed.len(),
        "a command is listed twice: {listed:?}"
    );

    // Every branch the old hand-written enum had, plus the ones whose
    // absence was the reason it was replaced. `rm` and `rmi` are both
    // here on purpose — they are the pair a substring check confuses.
    for must in [
        "build",
        "compose",
        "image",
        "info",
        "inspect",
        "network",
        "ps",
        "rm",
        "rmi",
        "stop",
        "volume",
        "run",
        "exec",
        "logs",
        "cp",
        "container",
        "system",
        "save",
        "load",
        "attach",
        "start",
        "context",
    ] {
        assert!(
            unique.contains(must),
            "`{must}` is missing from `ulak docker --help`:\n{help}"
        );
    }
}

#[test]
fn misplaced_commands_point_to_the_branch_that_really_exists() {
    for command in [
        "build", "network", "image", "info", "ps", "inspect", "stop", "rm", "rmi", "volume",
    ] {
        let out = AssertCommand::cargo_bin("ulak")
            .unwrap()
            .args([command, "argument"])
            .output()
            .unwrap();
        assert!(!out.status.success());
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(
            said.contains(&format!("ulak docker {command} argument")),
            "misplaced `{command}` got the wrong guidance:\n{said}"
        );
        assert!(
            !said.contains("docker compose"),
            "a Docker command was mislabeled as Compose:\n{said}"
        );
    }

    AssertCommand::cargo_bin("ulak")
        .unwrap()
        .args(["up", "-d"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("ulak docker compose up -d"));
}

// `server_has_everything_ulak_needs` used to be here; see the module
// doc for why it is not, and where its one surviving claim went.

/// The off switch for the background service must not be the thing that
/// breaks the program.
///
/// Measured: `[service] auto = false` — the line the docs themselves
/// recommended — hit `deny_unknown_fields`, and status, doctor, sync and
/// clean ALL exited 1 with "unknown field `service`".
///
/// `config.rs` pins the parse and what the values do. What only the
/// binary can answer is whether a command carrying that file gets far
/// enough to do its job, so the assertion is that it reaches the missing
/// host and says so — a positive claim. It used to be two negative
/// substring checks behind a 20 s connect timeout, one of which had
/// stopped being able to fail: it looked for "not valid ulak TOML" and
/// the code says "not valid Ulak TOML".
#[test]
fn the_service_switch_in_the_global_config_does_not_break_the_cli() {
    let ws = Workspace::new();
    let global = ws.home.join(".config/ulak");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(
        global.join("config.toml"),
        "[service]\nauto = false\nnotify = false\n",
    )
    .unwrap();

    let out = ws.ulak_alone().arg("status").output().unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !said.contains("unknown field") && !said.contains("not valid Ulak TOML"),
        "the service switch must parse:\n{said}"
    );
    assert!(
        said.contains("ulak init"),
        "the command has to get past the config and reach the thing that is really \
         missing — a host:\n{said}"
    );
}
