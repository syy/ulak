//! Phase 1 e2e: init wizard + the sync engine against a real server
//! (dockerized sshd fixture, or my-server via ULAK_TEST_E2E=host:...).
//!
//! Most of this file is ONE test on purpose, and it is the only suite
//! here that still is. The sibling suites were split because their
//! scenarios were merely co-located — each one asks its own question of
//! its own workspace, and threading them together only ever cost
//! information. These do not: the chain below inits a project, syncs it,
//! edits it, protects part of it, overruns its deletion budget and races
//! two windows through it, and every one of those steps is the state the
//! next one needs. Splitting that would either race or quietly re-impose
//! the same order through the back door, and a hidden ordering is worse
//! than a declared one.
//!
//! What splitting WOULD have bought is bought instead by `Chain`: a break
//! names the scenario it happened in and lists the ones that never ran,
//! so a regression sweep gets a map rather than one red line. The three
//! scenarios that genuinely stood alone — the one that needs no server at
//! all, the one that only needs a workspace that has been synced once,
//! and the one about two footprint resolutions colliding — are `#[test]`s
//! of their own below.

mod common;

use std::time::{Duration, Instant};

use common::{Chain, TestServer, Workspace, extract_remote_dir};

struct RemotePathCleanup<'a> {
    server: &'a TestServer,
    paths: Vec<String>,
}

impl<'a> RemotePathCleanup<'a> {
    fn new(server: &'a TestServer) -> RemotePathCleanup<'a> {
        RemotePathCleanup {
            server,
            paths: Vec::new(),
        }
    }

    fn remember(&mut self, path: String) -> String {
        self.paths.push(path.clone());
        path
    }
}

impl Drop for RemotePathCleanup<'_> {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = self.server.ssh_quiet(&format!("rm -rf {path}"));
        }
        if let Some(parent) = self.paths.first().and_then(|p| p.rsplit_once('/')) {
            let _ = self.server.ssh_quiet(&format!("rmdir {}", parent.0));
        }
    }
}

/// The chain, in order and by name, so a break can say what fell over
/// and what never got the chance to.
///
/// The names live here and nowhere else: `Chain::link` takes the next one
/// off this list rather than being handed it at the call site, because a
/// map that names the wrong scenario is worse than no map, and two copies
/// of an order are two things to keep in step. `Chain::finish` is what
/// keeps this list honest — it refuses a run in which the calls and the
/// plan disagree about how many links there are.
const CHAIN: &[&str] = &[
    "init writes only the project config and leaves Git alone",
    "the first sync puts the tree on the server, privately",
    "an edit travels and a deleted file disappears",
    "a directory the user removed leaves the server",
    "protect keeps the container's own data",
    "the deletion budget refuses, states the drift, then applies it",
    "a dry run touches nothing",
    "a file born mid-sync is still deletable",
    "a deletion that lands mid-pull stays deleted",
];

#[test]
fn phase1_init_and_sync() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    // One scenario on this server at a time: the stall shim is the
    // server's, so a sibling's rsync leg would satisfy a window this one
    // thinks it is holding. See `TestServer::exclusive_sync`.
    let _solo = TestServer::exclusive_sync();
    let ws = Workspace::new();
    let mut chain = Chain::new(CHAIN);

    chain.link(|| scenario_init(&server, &ws));
    let remote_dir = chain.link(|| scenario_first_sync(&server, &ws));
    chain.link(|| scenario_edit_and_delete(&server, &ws, &remote_dir));
    chain.link(|| scenario_an_empty_directory_leaves_when_you_remove_it(&server, &ws, &remote_dir));
    chain.link(|| scenario_protect_survives(&server, &ws, &remote_dir));
    chain.link(|| scenario_deletion_budget(&server, &ws, &remote_dir));
    chain.link(|| scenario_dry_run_touches_nothing(&server, &ws, &remote_dir));
    chain.link(|| scenario_a_file_born_mid_sync_is_still_deletable(&server, &ws, &remote_dir));
    chain.link(|| scenario_a_deletion_that_lands_mid_pull_stays_deleted(&server, &ws, &remote_dir));
    chain.finish();

    // Leave the server as we found it (harness boundary: only our own
    // ~/.ulak/workspaces subtree).
    if let Some((hash_dir, _)) = remote_dir.rsplit_once('/') {
        server.ssh(&format!("rm -rf {hash_dir}"));
    }
}

/// Without any config, sync must point at init — not stack-trace.
///
/// Its own test, and the only one here that needs no server: it is about
/// what ulak says before a server has ever been named, so making it wait
/// on a container would be paying for a fixture in order to prove the
/// fixture is irrelevant.
#[test]
fn sync_with_no_host_configured_points_at_init() {
    let ws = Workspace::new();
    let out = ws.ulak_alone().arg("sync").assert().failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("ulak init"),
        "error must suggest init, got:\n{stderr}"
    );
}

/// Exercise the actual command rather than only the template writer:
/// no Compose, no host, an existing .gitignore, and a second idempotent run.
#[test]
fn init_without_a_host_scaffolds_only_the_project_file() {
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
    std::fs::write(ws.project.join(".gitignore"), "keep-this/\n").unwrap();

    let out = ws.ulak_alone().arg("init").assert().success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr);
    assert!(stderr.contains("no Compose file found"));

    let path = ws.project.join("ulak.toml");
    let first = std::fs::read_to_string(&path).unwrap();
    assert!(toml::from_str::<toml::Value>(&first).is_ok());
    assert!(first.contains("# host = \"my-server\""));
    assert!(!ws.project.join("ulak.local.toml").exists());
    assert!(!ws.home.join(".config/ulak/config.toml").exists());
    assert_eq!(
        std::fs::read_to_string(ws.project.join(".gitignore")).unwrap(),
        "keep-this/\n"
    );

    ws.ulak_alone().arg("init").assert().success();
    assert_eq!(std::fs::read_to_string(path).unwrap(), first);
}

/// Passing a host must never report success while silently leaving an
/// existing file untouched.
#[test]
fn init_with_a_host_refuses_to_overwrite_an_existing_project_file() {
    let ws = Workspace::new();
    let path = ws.project.join("ulak.toml");
    std::fs::write(&path, "host = \"keep-me\"\n").unwrap();

    let out = ws
        .ulak_alone()
        .args(["init", "replace-me"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr);
    assert!(stderr.contains("already exists"));
    assert!(stderr.contains("was not applied"));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "host = \"keep-me\"\n"
    );
}

/// Invalid destinations are rejected by the public CLI before either an
/// SSH process can be started or init can persist an unusable config.
#[test]
fn option_shaped_hosts_are_rejected_by_the_real_cli() {
    let ws = Workspace::new();
    let path = ws.project.join("ulak.toml");
    std::fs::write(&path, "host = \"user@-oProxyCommand=anything\"\n").unwrap();

    let out = ws.ulak_alone().args(["docker", "info"]).assert().failure();
    assert!(String::from_utf8_lossy(&out.get_output().stderr).contains("not a valid destination"));

    std::fs::remove_file(&path).unwrap();
    let out = ws
        .ulak_alone()
        .args(["init", "--", "-oProxyCommand=anything"])
        .assert()
        .failure();
    assert!(String::from_utf8_lossy(&out.get_output().stderr).contains("not a valid destination"));
    assert!(!path.exists(), "init must not persist an unusable host");
}

/// Prove the entire host cascade through the shipped binary and a real
/// OpenSSH round trip to the Docker test server. Each earlier layer is
/// deliberately unusable once a later layer is introduced, so success
/// proves which value won.
#[test]
fn global_project_and_local_hosts_drive_real_ssh_in_cascade_order() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = Workspace::new();
    let xdg = ws.home.join("xdg-config");
    let global = xdg.join("ulak/config.toml");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, format!("host = {:?}\n", server.alias)).unwrap();

    // The global layer supplies defaults; an explicit project file still
    // marks which directory is an Ulak workspace.
    ws.ulak(&server)
        .env("XDG_CONFIG_HOME", &xdg)
        .arg("init")
        .assert()
        .success();

    ws.ulak(&server)
        .env("XDG_CONFIG_HOME", &xdg)
        .args(["docker", "info", "--format", "{{.Name}}"])
        .assert()
        .success();

    std::fs::write(&global, "host = \"-global-must-not-win\"\n").unwrap();
    std::fs::write(
        ws.project.join("ulak.toml"),
        format!("host = {:?}\n", server.alias),
    )
    .unwrap();
    ws.ulak(&server)
        .env("XDG_CONFIG_HOME", &xdg)
        .args(["docker", "info", "--format", "{{.Name}}"])
        .assert()
        .success();

    std::fs::write(
        ws.project.join("ulak.toml"),
        "host = \"-project-must-not-win\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.project.join("ulak.local.toml"),
        format!("host = {:?}\n", server.alias),
    )
    .unwrap();
    ws.ulak(&server)
        .env("XDG_CONFIG_HOME", &xdg)
        .args(["docker", "info", "--format", "{{.Name}}"])
        .assert()
        .success();
}

fn scenario_init(server: &TestServer, ws: &Workspace) {
    std::fs::create_dir_all(ws.project.join(".git")).unwrap();
    std::fs::write(ws.project.join(".gitignore"), "existing-rule/\n").unwrap();

    let out = ws
        .ulak(server)
        .args(["init", &server.alias])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    // The exact selected invocation, `ulak -f … doctor`, not a bare
    // `ulak doctor`: the next step must paste into the same project.
    assert!(
        stderr.contains("next: ulak ") && stderr.contains(" doctor"),
        "init must point at doctor:\n{stderr}"
    );
    assert!(!stderr.contains("probing"), "init must not contact SSH");

    let project = std::fs::read_to_string(ws.project.join("ulak.toml")).unwrap();
    assert!(project.contains(&format!("host = \"{}\"", server.alias)));
    assert!(!ws.home.join(".config/ulak/config.toml").exists());
    assert!(!ws.project.join("ulak.local.toml").exists());
    let gitignore = std::fs::read_to_string(ws.project.join(".gitignore")).unwrap();
    assert_eq!(gitignore, "existing-rule/\n");
}

#[test]
fn init_is_offline_even_for_an_unreachable_host() {
    let ws = Workspace::new();
    ws.ulak_alone()
        .args(["init", "192.0.2.1"])
        .assert()
        .success();

    let project = std::fs::read_to_string(ws.project.join("ulak.toml")).unwrap();
    assert!(project.contains("host = \"192.0.2.1\""));
}

fn scenario_first_sync(server: &TestServer, ws: &Workspace) -> String {
    // .gitignore hides node_modules and .env — but .env must sync anyway.
    let gi = ws.project.join(".gitignore");
    let mut text = std::fs::read_to_string(&gi).unwrap();
    text.push_str("node_modules/\n.env\n");
    std::fs::write(&gi, text).unwrap();

    ws.write("site/index.html", "hello-v1");
    ws.write(".env", "GREETING=from-dotenv");
    ws.write("node_modules/pkg/big.js", "never-synced");
    ws.write("app/main.py", "print(1)");

    let out = ws.ulak(server).arg("sync").assert().success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    let remote_dir =
        extract_remote_dir(&stderr).unwrap_or_else(|| panic!("no remote dir in output:\n{stderr}"));

    let cat = server.ssh(&format!("cat {remote_dir}/site/index.html"));
    assert_eq!(String::from_utf8_lossy(&cat.stdout).trim(), "hello-v1");
    assert!(
        server
            .ssh(&format!("grep -q from-dotenv {remote_dir}/.env"))
            .status
            .success(),
        ".env must be force-included despite .gitignore"
    );
    assert!(
        server
            .ssh(&format!("test ! -e {remote_dir}/node_modules"))
            .status
            .success(),
        "gitignored node_modules must not reach the server"
    );
    // Secrets hygiene: the workspace tree is sealed at the top. (The project
    // dir itself follows local perms — rsync --perms — but nobody can
    // traverse into it past the 0700 parents.)
    let workspace_root = remote_dir.rsplit_once('/').unwrap().0;
    let namespace_root = workspace_root.rsplit_once('/').unwrap().0;
    for dir in [".ulak/workspaces", namespace_root, workspace_root] {
        let mode = server.ssh(&format!(
            "stat -c %a {dir} 2>/dev/null || stat -f %Lp {dir}"
        ));
        assert_eq!(
            String::from_utf8_lossy(&mode.stdout).trim(),
            "700",
            "{dir} must be private"
        );
    }
    remote_dir
}

fn scenario_edit_and_delete(server: &TestServer, ws: &Workspace, remote_dir: &str) {
    ws.write("site/index.html", "hello-v2");
    ws.ulak(server).arg("sync").assert().success();
    let cat = server.ssh(&format!("cat {remote_dir}/site/index.html"));
    assert_eq!(String::from_utf8_lossy(&cat.stdout).trim(), "hello-v2");

    // A small deletion flows through the budget passes automatically.
    std::fs::remove_file(ws.project.join("app/main.py")).unwrap();
    ws.ulak(server).arg("sync").assert().success();
    assert!(
        server
            .ssh(&format!("test ! -e {remote_dir}/app/main.py"))
            .status
            .success(),
        "deleted local file must disappear from the workspace"
    );
}

/// A directory is a path ulak put in the workspace, so removing it here
/// has to remove it there — and an empty one is the case that proves it.
///
/// Measured before the fix, with no race anywhere in sight: the walk
/// reports FILES, `claim` drops rsync's directory rows on purpose, and
/// the doomed set therefore never names a directory. rsync creates
/// directories implicitly on whichever side is receiving, so an empty
/// one pushed once sat on the server for good — and the pull carried it
/// straight back into the repo, where nothing may delete it either. Two
/// permanent piles of junk from one hole, which is what made the
/// renamed-directory scenario lock: the rename only had to leave an
/// empty directory behind ONCE.
fn scenario_an_empty_directory_leaves_when_you_remove_it(
    server: &TestServer,
    ws: &Workspace,
    remote_dir: &str,
) {
    // Two shapes, because they fail for different reasons: an empty
    // directory is invisible to the ledger from the start, while one with
    // content has its FILE doomed and only the directory left behind.
    // The second is the deterministic twin of the renamed-directory
    // scenario in e2e_service — a rename is exactly "removed whole" plus
    // a race.
    for (name, file) in [("moved", Some("inner.txt")), ("hollow", None)] {
        match file {
            Some(f) => ws.write(&format!("site/{name}/deeper/{f}"), "dir-content"),
            // A bind mount would show an empty directory, so the workspace
            // shows it too. The bug is never that it goes UP.
            None => {
                std::fs::create_dir_all(ws.project.join(format!("site/{name}/deeper"))).unwrap()
            }
        }
        ws.ulak(server).arg("sync").assert().success();
        assert!(
            server
                .ssh(&format!("test -d {remote_dir}/site/{name}/deeper"))
                .status
                .success(),
            "site/{name}/deeper must reach the workspace before the test can remove it"
        );

        // Now the user removes it, whole. Nothing else changed.
        std::fs::remove_dir_all(ws.project.join(format!("site/{name}"))).unwrap();
        ws.ulak(server).arg("sync").assert().success();
        assert!(
            server
                .ssh(&format!("test ! -e {remote_dir}/site/{name}"))
                .status
                .success(),
            "the directory the user removed is still on the server, and nothing \
             in the pipeline can ever remove it:\n{}",
            String::from_utf8_lossy(
                &server
                    .ssh(&format!("ls -laR {remote_dir}/site/{name} 2>&1"))
                    .stdout
            )
        );
        // The other side of the same hole: what stays on the server comes
        // home on the pull, and ulak never removes anything local — so
        // the repo grows a directory nobody can delete, and the next push
        // sends it back up. That pair is the lock.
        assert!(
            !ws.project.join(format!("site/{name}")).exists(),
            "site/{name} came back into the repo from the server — it is now on \
             both sides, and neither side can remove it"
        );
    }
}

fn scenario_protect_survives(server: &TestServer, ws: &Workspace, remote_dir: &str) {
    // "cache[old]" has rsync glob metacharacters in its literal name —
    // unescaped filter rules silently drop such names.
    ws.write(
        "ulak.toml",
        &format!(
            "host = {:?}\n[sync]\nprotect = [\"data/\", \"cache[old]/\", \"/logs\"]\n[forward]\nauto = true\n",
            server.alias
        ),
    );
    // `/logs` is the gitignore spelling, and it used to protect nothing:
    // no anchor-relative path begins with a slash, so the walker claimed
    // the tree into the ledger, and `rsync_exclude_pattern` adds its own
    // anchor, making the rule `P //logs` — which matches nothing either.
    // The two halves fail differently and both are checked below: the
    // push overwrites the live copy, and a later local delete takes
    // `rm -f` to the server's only one. A stale local copy is what makes
    // the first half visible at all.
    ws.write("logs/app.log", "LOCAL-STALE\n");
    // The "container" writes into the protected dirs on the server.
    assert!(
        server
            .ssh(&format!(
                "mkdir -p {remote_dir}/data '{remote_dir}/cache[old]' {remote_dir}/logs \
                 && echo server-owned > {remote_dir}/data/db.bin \
                 && echo bracket-owned > '{remote_dir}/cache[old]/state.bin' \
                 && echo live-log > {remote_dir}/logs/app.log"
            ))
            .status
            .success()
    );

    ws.ulak(server).arg("sync").assert().success();
    let cat = server.ssh(&format!("cat {remote_dir}/data/db.bin"));
    assert_eq!(
        String::from_utf8_lossy(&cat.stdout).trim(),
        "server-owned",
        "protect path must survive a sync that would otherwise delete it"
    );
    let cat = server.ssh(&format!("cat '{remote_dir}/cache[old]/state.bin'"));
    assert_eq!(
        String::from_utf8_lossy(&cat.stdout).trim(),
        "bracket-owned",
        "protect must hold even when the path contains rsync glob chars"
    );
    let cat = server.ssh(&format!("cat {remote_dir}/logs/app.log"));
    assert_eq!(
        String::from_utf8_lossy(&cat.stdout).trim(),
        "live-log",
        "a leading slash must protect, not void: the stale local copy \
         overwrote the server's live one"
    );
    assert_eq!(
        std::fs::read_to_string(ws.project.join("logs/app.log")).unwrap(),
        "LOCAL-STALE\n",
        "and protect holds in the other direction too: the server's copy \
         must not come home"
    );

    // The second half of the chain: what the walker claimed, a local
    // delete can doom. One file is under the default budget of 25, so
    // nothing would have asked first.
    std::fs::remove_dir_all(ws.project.join("logs")).unwrap();
    ws.ulak(server).arg("sync").assert().success();
    let cat = server.ssh(&format!("cat {remote_dir}/logs/app.log"));
    assert_eq!(
        String::from_utf8_lossy(&cat.stdout).trim(),
        "live-log",
        "deleting the local copy must not take `rm -f` to the server's only one"
    );
}

fn scenario_deletion_budget(server: &TestServer, ws: &Workspace, remote_dir: &str) {
    // The files must be OURS: ulak only deletes what it put there, so
    // a pile created directly on the server is (correctly) left alone.
    // Sync them first, THEN delete them locally — that is what a branch
    // switch looks like.
    for i in 1..=30 {
        ws.write(&format!("junk/f{i}.txt"), "x");
    }
    ws.ulak(server).arg("sync").assert().success();
    assert!(
        server
            .ssh(&format!("test -e {remote_dir}/junk/f30.txt"))
            .status
            .success(),
        "the files must reach the server before they can be deleted"
    );
    std::fs::remove_dir_all(ws.project.join("junk")).unwrap();

    // 30 pending deletions > default budget 25; no tty → refuse, delete nothing.
    let out = ws.ulak(server).arg("sync").assert().failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(
        stderr.contains("--max-delete"),
        "budget error must show the escape hatch:\n{stderr}"
    );
    assert!(
        server
            .ssh(&format!("test -e {remote_dir}/junk/f30.txt"))
            .status
            .success(),
        "over-budget sync must not delete anything"
    );

    // After a refused deletion set the workspace carries files the project
    // no longer has. The prototype let that drift quietly and re-asked
    // on every sync; it has to be stated, with the way out.
    let out = ws.ulak(server).arg("status").output().unwrap();
    let shown =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        shown.contains("no longer has are still on the server") && shown.contains("--max-delete"),
        "status must state the drift and how to end it:\n{shown}"
    );

    // Explicitly raised budget → deletions happen.
    ws.ulak(server)
        .args(["sync", "--max-delete", "40"])
        .assert()
        .success();
    assert!(
        server
            .ssh(&format!("test ! -e {remote_dir}/junk"))
            .status
            .success(),
        "raised budget must allow the deletions"
    );

    // …and once they are applied, the drift notice is gone.
    let out = ws.ulak(server).arg("status").output().unwrap();
    let shown =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        !shown.contains("no longer has are still on the server"),
        "the drift notice must clear once the deletions are applied:\n{shown}"
    );
}

/// A file can be born INSIDE a sync: the local walk that decides what
/// the ledger will claim runs before the transfer, with a remote
/// round-trip in between, and rsync scans the tree again when it starts.
/// A save that lands in that window travels to the server — and if the
/// ledger is built from the walk alone it is never claimed, which the
/// ledger reads as "born on the server". Deleting it locally then leaves
/// it up there forever, in silence.
///
/// Found on my-server as an intermittent watch failure: a save that
/// landed while watch waited on the per-workspace lock became undeletable.
///
/// The window is HELD open (`stall_the_push`) rather than aimed at,
/// because aiming did not work and said so quietly. What stood here was
/// five sleeps from 200 ms to 900 ms, each one racing a whole sync, and
/// on the dockerized fixture not one of the five ever landed the file on
/// the server — a warm control master gets ulak from its walk to rsync's
/// scan in less time than the shortest of them. The scenario then printed
/// "inconclusive" and returned, which libtest captures for a test that
/// PASSES: the bug this exists for had been going untested under a green
/// tick, on the backend that runs by default. Holding the transport still
/// puts the write strictly after the walk and strictly before the scan,
/// every run, and costs one sync instead of five.
fn scenario_a_file_born_mid_sync_is_still_deletable(
    server: &TestServer,
    ws: &Workspace,
    remote_dir: &str,
) {
    let _ = std::fs::remove_file(ws.project.join("site/racy.txt"));

    server.stall_the_push(5);
    let sync = ws
        .ulak_raw(server)
        .arg("sync")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn ulak sync");
    assert!(
        server.wait_until_the_push_starts(Duration::from_secs(180)),
        "the push never started, so the window was never open and nothing was proved"
    );
    // The walk has been and gone; rsync has not yet looked at this
    // machine. This is the save nobody thinks twice about.
    ws.write("site/racy.txt", "born-mid-sync");
    let out = sync.wait_with_output().expect("sync");
    server.stop_stalling_the_push();
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "mid-sync push failed:\n{said}");

    assert!(
        server
            .ssh(&format!("test -e {remote_dir}/site/racy.txt"))
            .status
            .success(),
        "the file written inside the held window never travelled, so the ledger \
         hole it opens cannot be reached from here:\n{said}"
    );

    // The consequence is what matters: the server copy must still be
    // ulak's to remove.
    std::fs::remove_file(ws.project.join("site/racy.txt")).unwrap();
    ws.ulak(server).arg("sync").assert().success();
    assert!(
        server
            .ssh(&format!("test ! -e {remote_dir}/site/racy.txt"))
            .status
            .success(),
        "a file that arrived mid-sync is stuck on the server forever"
    );
}

/// The same window, crossed the other way — and this half costs a file.
///
/// `reconcile_once` pushes, then pulls. The pull works out what it must
/// NOT carry home from a walk of this machine, and only then spawns
/// rsync, which asks the server for its file list and looks here
/// afterwards. Delete a synced file inside that gap and every check
/// says yes: the walk saw it, so nothing holds it back; the ledger
/// claims it, so it is legitimate; rsync finds it missing here and
/// present there, so it CREATES it. ulak never deletes local files,
/// so the resurrected copy stays, the next walk syncs it, `doomed` can
/// never name it again — and the deletion is lost for good.
///
/// Measured on my-server as `e2e_service::phase5` failing 3 full-suite
/// runs out of 7, with the service's own log reading
/// `1 pushed, 0 deleted, 1 came back`. Here the window is held open on
/// purpose (`stall_the_pull`), because a fix cannot be proved against a
/// coin flip.
///
/// The second half of the scenario is the reason this is hard: the pull
/// exists to bring the stack's own work home. Everything happens in the
/// one held window — the user deletes a file, and the container invents
/// one and rewrites another — because that window is the only place
/// where all three are live at once. Stopping the resurrection by
/// stopping the pull would not be a fix, it would be a trade.
///
/// The container's rewrite has to happen HERE and not earlier, and that
/// is measured, not stylistic: the push leg runs first and sends the
/// local copy of every file that exists on both sides, so a rewrite made
/// before the reconcile is overwritten by the push before the pull ever
/// looks. Last writer wins, and inside one reconcile the push is the
/// last writer.
fn scenario_a_deletion_that_lands_mid_pull_stays_deleted(
    server: &TestServer,
    ws: &Workspace,
    remote_dir: &str,
) {
    ws.write("site/deleted-mid-pull.txt", "the user's file");
    ws.write("site/rewritten-by-the-stack.txt", "before");
    // A whole directory goes the same way in the same window. It was an
    // open question whether `--prune-empty-dirs` really closes the
    // directory case or only looks like it does; crossing the window
    // with a removed subtree is what answers it.
    ws.write("site/dir-removed-mid-pull/deeper/inner.txt", "dir-content");
    ws.ulak(server).arg("sync").assert().success();
    for f in [
        "deleted-mid-pull.txt",
        "rewritten-by-the-stack.txt",
        "dir-removed-mid-pull/deeper/inner.txt",
    ] {
        assert!(
            server
                .ssh(&format!("test -e {remote_dir}/site/{f}"))
                .status
                .success(),
            "site/{f} must be in the workspace, and claimed by the ledger, \
             before the race means anything"
        );
    }

    // Old enough to be somebody else's work. `touched_since` protects
    // anything stamped within one second of the reconcile's start, on
    // purpose — mtimes are whole seconds on the wire and ulak breaks
    // that tie for the local file every time. The dockerized fixture
    // runs this scenario about fifteen times faster than my-server
    // does, fast enough that `rewritten-by-the-stack.txt` landed INSIDE
    // that window and was quite correctly held back; measured, and it
    // cost a red run on one backend and not the other. A file the
    // container is about to rewrite has to be older than an edit still
    // warm in the editor, or the scenario is asking ulak to tell two
    // identical things apart.
    std::thread::sleep(std::time::Duration::from_secs(2));

    server.stall_the_pull(20);
    let sync = ws
        .ulak_raw(server)
        .arg("sync")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn ulak sync");
    assert!(
        server.wait_until_the_pull_starts(std::time::Duration::from_secs(180)),
        "the pull never started, so nothing was proved either way"
    );
    // The push is done, the ledger claims the file, and the pull has
    // already decided what to hold back. This is the moment a save-heavy
    // afternoon lands in by itself.
    std::fs::remove_file(ws.project.join("site/deleted-mid-pull.txt")).unwrap();
    std::fs::remove_dir_all(ws.project.join("site/dir-removed-mid-pull")).unwrap();
    // And this is what the stack is doing meanwhile: inventing one file
    // and rewriting one ulak put there.
    server.ssh(&format!(
        "printf 'made-by-the-stack' > {remote_dir}/site/made-by-the-stack.txt && \
         printf 'after' > {remote_dir}/site/rewritten-by-the-stack.txt"
    ));
    let out = sync.wait_with_output().expect("sync");
    server.stop_stalling_the_pull();
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "sync failed:\n{said}");

    // All three facts, before the first assert stops the run: a fix that
    // trades one half for the other has to be visible as a trade, in the
    // same window, in one run.
    let seen = |rel: &str| match std::fs::read_to_string(ws.project.join(rel)) {
        Ok(text) => format!("{text:?}"),
        Err(_) => "absent".to_string(),
    };
    // `--update` is decided on whole-second mtimes, so when the updating
    // pass declines to carry something home the two clocks are the first
    // thing worth seeing — not the tenth.
    eprintln!(
        "held-pull window mtimes: local rewritten={:?} remote rewritten={} remote now={}",
        std::fs::metadata(ws.project.join("site/rewritten-by-the-stack.txt"))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
        String::from_utf8_lossy(
            &server
                .ssh(&format!(
                    "stat -c %Y {remote_dir}/site/rewritten-by-the-stack.txt 2>/dev/null"
                ))
                .stdout
        )
        .trim(),
        String::from_utf8_lossy(&server.ssh("date +%s").stdout).trim(),
    );
    eprintln!(
        "held-pull window: deleted-mid-pull.txt {} · dir-removed-mid-pull {} · \
         made-by-the-stack.txt {} · rewritten-by-the-stack.txt {}",
        seen("site/deleted-mid-pull.txt"),
        if ws.project.join("site/dir-removed-mid-pull").exists() {
            "back"
        } else {
            "absent"
        },
        seen("site/made-by-the-stack.txt"),
        seen("site/rewritten-by-the-stack.txt"),
    );

    assert!(
        !ws.project.join("site/deleted-mid-pull.txt").exists(),
        "the file came back from the server. Nothing local may be deleted to \
         undo that, so the user's deletion is now unrecoverable:\n{said}"
    );
    assert!(
        !ws.project.join("site/dir-removed-mid-pull").exists(),
        "a directory the user removed came back — as a shell or with its \
         contents, both are junk nothing here may delete:\n{said}"
    );
    // The half that must not be traded away for the half above.
    assert_eq!(
        std::fs::read_to_string(ws.project.join("site/made-by-the-stack.txt")).ok(),
        Some("made-by-the-stack".to_string()),
        "a file the stack created never came home — the pull's whole \
         reason to exist:\n{said}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.project.join("site/rewritten-by-the-stack.txt")).ok(),
        Some("after".to_string()),
        "a file the stack rewrote never came home; codegen over a file ulak itself \
         put there is exactly what the down leg is for:\n{said}"
    );

    // And the deletion still reaches the server, because it is still a
    // deletion — the ledger claims a path that is no longer here.
    ws.ulak(server).arg("sync").assert().success();
    for gone in ["deleted-mid-pull.txt", "dir-removed-mid-pull"] {
        assert!(
            server
                .ssh(&format!("test ! -e {remote_dir}/site/{gone}"))
                .status
                .success(),
            "the deletion of {gone} never reached the server: {}",
            String::from_utf8_lossy(&server.ssh(&format!("ls -laR {remote_dir}/site")).stdout)
        );
    }
}

fn scenario_dry_run_touches_nothing(server: &TestServer, ws: &Workspace, remote_dir: &str) {
    ws.write("newfile.txt", "not-yet");
    let out = ws
        .ulak(server)
        .args(["sync", "--dry-run"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("would sync"), "dry-run wording:\n{stderr}");
    assert!(
        server
            .ssh(&format!("test ! -e {remote_dir}/newfile.txt"))
            .status
            .success(),
        "dry-run must not touch the server"
    );
    std::fs::remove_file(ws.project.join("newfile.txt")).unwrap();
}

/// A workspace on the server that this machine has no claim on is a
/// question, not a verdict — and a process with no terminal must not
/// answer it.
///
/// Measured on a real project: a v0.3 workspace THIS machine had created
/// was met by a ulak whose state directory had since been reset. The
/// old code decided silently ("not mine"), recorded that forever, and
/// left the owner with one documented way out: `ulak clean`, which
/// destroys the workspace. The decision now belongs to whoever is standing
/// at the terminal, and where nobody is standing, nothing is decided.
///
/// Split out of the chain because everything it needs is one sync: it
/// asks what happens when this machine's memory of a workspace is gone,
/// and the ten scenarios' worth of history it used to inherit was never
/// part of the question. Being last but one in the chain also made it the
/// scenario that DESTROYED that history — it deletes the claim the rest
/// of the chain was built on — so its own workspace is the honest shape.
#[test]
fn an_unclaimed_workspace_is_not_decided_by_a_machine() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    // One scenario on this server at a time: the stall shim is the
    // server's, so a sibling's rsync leg would satisfy a window this one
    // thinks it is holding. See `TestServer::exclusive_sync`.
    let _solo = TestServer::exclusive_sync();
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws.write("site/index.html", "claimed-by-this-machine");
    ws.ulak(&server).arg("sync").assert().success();

    let id = ws
        .workspace_ids()
        .first()
        .cloned()
        .expect("a synced workspace");
    let remote_root = ws.remote_workspace_root(&id);
    let claim = ws.workspaces_dir().join(&id).join("uuid");
    assert!(
        claim.is_file(),
        "the first sync must have claimed the workspace"
    );

    // Exactly what a reset state directory looks like: the workspace on the
    // server lives on, this machine's memory of it does not.
    std::fs::remove_dir_all(ws.workspaces_dir().join(&id)).unwrap();

    // A test has no terminal, which is also what the service has.
    let out = ws.ulak(&server).arg("sync").output().unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a machine that cannot ask must not proceed:\n{said}"
    );
    assert!(
        said.contains("left no record"),
        "it has to say what it found:\n{said}"
    );
    assert!(
        said.contains("in a terminal"),
        "and where the answer comes from:\n{said}"
    );
    assert!(
        !ws.workspaces_dir().join(&id).join("uuid").is_file(),
        "nothing may be decided on the owner's behalf"
    );

    // And the workspace on the server is untouched — the whole point of
    // stopping rather than guessing.
    assert!(
        server
            .ssh(&format!("test -f {remote_root}/manifest.json"))
            .status
            .success(),
        "the workspace must survive a refusal"
    );

    // By hand rather than through `forget_on_server`: that reads the ids
    // out of this machine's state directory, and erasing exactly that is
    // what the scenario is about.
    server.ssh(&format!("rm -rf {remote_root}"));
    if let Some(namespace) = ws.workspace_namespace() {
        server.ssh(&format!(
            "rmdir .ulak/workspaces/{namespace} 2>/dev/null || true"
        ));
    }
}

/// One server's deletion receipt must not erase another server's claim.
///
/// The two destination strings reach the same fixture account, while the
/// remote root is snapshotted and restored between them. That gives each
/// destination an independent filesystem image without a second daemon and
/// reproduces the production failure exactly: a shared ledger let A retire
/// the row, then B's stale bytes looked server-born and came back locally.
#[test]
fn each_destination_retires_only_its_own_synced_files() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let _solo = TestServer::exclusive_sync();
    let ws = Workspace::new();
    let destination_a = server.alias.clone();
    let who = server.ssh("id -un");
    assert!(who.status.success(), "cannot identify the fixture SSH user");
    let user = String::from_utf8_lossy(&who.stdout).trim().to_string();
    let destination_b = format!("{user}@{}", server.alias);
    assert_ne!(destination_a, destination_b);
    ws.write("owned.txt", "the stale bytes must never come home\n");
    ws.set_host(&destination_a);

    let first = ws.ulak(&server).arg("sync").output().unwrap();
    assert!(
        first.status.success(),
        "server A sync failed:\n{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let remote_dir = extract_remote_dir(&String::from_utf8_lossy(&first.stderr))
        .expect("sync reports its remote directory");
    let remote_root = remote_dir
        .rsplit_once('/')
        .map(|(root, _)| root.to_string())
        .expect("the project directory sits below the workspace root");
    let mut cleanup = RemotePathCleanup::new(&server);
    cleanup.remember(remote_root.clone());
    let snapshot_a = cleanup.remember(format!("{remote_root}-destination-a"));
    let snapshot_b = cleanup.remember(format!("{remote_root}-destination-b"));
    let remote_file = format!("{remote_dir}/owned.txt");
    let initial = server.ssh(&format!("cat {remote_file}"));
    assert_eq!(initial.stdout, b"the stale bytes must never come home\n");
    assert!(
        server
            .ssh(&format!("cp -a {remote_root} {snapshot_a}"))
            .status
            .success(),
        "cannot snapshot destination A"
    );

    // Swap in a blank remote filesystem for destination B, but keep the
    // same local checkout and state root.
    server.ssh(&format!("rm -rf {remote_root}"));
    ws.set_host(&destination_b);
    let second = ws.ulak(&server).arg("sync").output().unwrap();
    assert!(
        second.status.success(),
        "server B sync failed:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        server
            .ssh(&format!("cp -a {remote_root} {snapshot_b}"))
            .status
            .success(),
        "cannot snapshot destination B"
    );

    std::fs::remove_file(ws.project.join("owned.txt")).unwrap();

    // A confirms its own deletion and retires only A's ledger row.
    server.ssh(&format!(
        "rm -rf {remote_root} && cp -a {snapshot_a} {remote_root}"
    ));
    ws.set_host(&destination_a);
    let delete_a = ws.ulak(&server).arg("sync").output().unwrap();
    assert!(
        delete_a.status.success(),
        "server A deletion failed:\n{}",
        String::from_utf8_lossy(&delete_a.stderr)
    );
    assert!(
        server
            .ssh(&format!("test ! -e {remote_file}"))
            .status
            .success(),
        "destination A kept bytes it confirmed deleting"
    );

    // B still owns its row. It must delete its stale copy; if A had retired
    // the shared row, the pull would instead recreate these exact bytes.
    server.ssh(&format!(
        "rm -rf {remote_root} && cp -a {snapshot_b} {remote_root}"
    ));
    ws.set_host(&destination_b);
    let delete_b = ws.ulak(&server).arg("sync").output().unwrap();
    assert!(
        delete_b.status.success(),
        "server B deletion failed:\n{}",
        String::from_utf8_lossy(&delete_b.stderr)
    );
    assert!(
        !ws.project.join("owned.txt").exists(),
        "destination B's stale bytes were pulled back into the checkout: {:?}",
        std::fs::read(ws.project.join("owned.txt"))
    );
    let final_remote = server.ssh(&format!(
        "if test -e {remote_file}; then cat {remote_file}; else printf ABSENT; fi"
    ));
    assert_eq!(
        final_remote.stdout, b"ABSENT",
        "destination B did not retire its own stale bytes"
    );
}

/// Two ulaks resolving the compose model at the same moment.
///
/// The bug this pins, measured on a real machine from
/// ulak's own audit trail: they ran `cargo install`, then
/// `ulak service install`, then `ulak sync`. The version bump had
/// invalidated the footprint cache, so the service and the CLI both went
/// to resolve the model — and `resolve` began by emptying ONE shared
/// bootstrap directory. Inside a single second the service's rsync came
/// out at 23 (its files vanished under it) and the CLI's
/// `docker compose config` came out at 1, telling the user that
/// `compose.dev.yaml` did not exist while it sat on their disk.
///
/// Invisible until then because a warm cache means `resolve` hardly ever
/// runs; the window opens exactly when the cache is invalidated, which
/// is the upgrade that ships the change. So the cache is cleared here on
/// purpose — that is the state the bug needs, and the only one.
///
/// Four at once rather than two: the losing interleaving needs one
/// process to be between its rsync and its `config` when another wipes
/// the directory, and four makes that near-certain instead of a coin
/// flip. With a directory per invocation there is nothing shared left to
/// wipe, so the count stops mattering.
///
/// Its own `#[test]`, and it had to become one: it needs a synced
/// workspace and a COLD footprint cache, and it got the first by being
/// welded into the sync chain — where it then deliberately destroyed the
/// second, `remove_dir_all`-ing the cache the chain above it had warmed.
/// A break here would have painted the chain's map with a failure that
/// has nothing to do with sync or the ledger. One sync of its own buys
/// everything the ten scenarios' worth of inherited history did.
#[test]
fn two_ulaks_resolving_at_once_do_not_erase_each_other() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    // One scenario on this server at a time: the stall shim is the
    // server's, so a sibling's rsync leg would satisfy a window this one
    // thinks it is holding. See `TestServer::exclusive_sync`.
    let _solo = TestServer::exclusive_sync();
    let server = &*server;
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    ws.write("site/index.html", "resolved-by-four-at-once");
    let out = ws.ulak(server).arg("sync").output().unwrap();
    assert!(
        out.status.success(),
        "the first sync must work:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let remote_dir = extract_remote_dir(&String::from_utf8_lossy(&out.stderr))
        .expect("no remote dir in the sync's output");
    let ws = &ws;
    let remote_dir = remote_dir.as_str();

    let cache = ws.home.join(".local/state/ulak/footprints");
    std::fs::remove_dir_all(&cache).ok();

    // STAGGERED, and that is the whole trick. Four resolves started in
    // the same millisecond stay in step — they all empty the directory,
    // then they all fill it, then they all read it — and step on nobody.
    // Measured: with the shared directory and no stagger, all four
    // passed. The incident had a service and a CLI at DIFFERENT points,
    // and the losing move is a latecomer emptying the directory while an
    // earlier one is between its rsync and its `docker compose config`.
    // A resolve takes a second or two, so a few hundred milliseconds
    // apart puts each new arrival squarely inside somebody's window.
    //
    // Spawned first and waited on second: `wait_with_output` blocks, so
    // reading any of them early would turn this back into four runs in
    // a row.
    let mut running: Vec<std::process::Child> = Vec::new();
    for i in 0..4 {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        running.push(
            ws.ulak_raw(server)
                .arg("status")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn ulak status"),
        );
    }

    let mut broke: Vec<String> = Vec::new();
    for (i, child) in running.into_iter().enumerate() {
        let out = child.wait_with_output().expect("wait for ulak status");
        if !out.status.success() {
            broke.push(format!(
                "#{i} exited {:?}:\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    assert!(
        broke.is_empty(),
        "{} of 4 concurrent resolves failed — they are emptying one \
         another's bootstrap directory:\n{}",
        broke.len(),
        broke.join("\n")
    );

    // …and the ones that succeeded took their directories away with
    // them. A bootstrap left standing is how the shared one came to be
    // the thing everybody kept wiping.
    //
    // The hash directory comes from `remote_dir` rather than from
    // `workspace_ids()`, because `read_dir` does not promise an order and
    // a workspace that made more than one of them would turn this
    // assertion into a coin flip.
    let hash_dir = remote_dir
        .rsplit_once('/')
        .map(|(d, _)| d)
        .unwrap_or(remote_dir);
    let left = server.ssh(&format!("ls -d {hash_dir}/bootstrap* 2>/dev/null | wc -l"));
    assert_eq!(
        String::from_utf8_lossy(&left.stdout).trim(),
        "0",
        "a successful resolve clears the directory it made"
    );

    server.ssh(&format!("rm -rf {hash_dir}"));
}

/// Two saves inside one second, and the second one has to arrive. The
/// hole and the fix are `sync::hidden_by_the_quick_check`.
///
/// `e2e_service::phase5` found it and cannot confirm it: that burst only
/// enters the hole when the service's first push lands mid-burst, which
/// GitHub's runner does and an 8-core box does not — measured, with the
/// fix removed, it passed there in 0.4 s. A green that depends on losing
/// a race is evidence about the machine.
///
/// So the timing is an input here. The second save is made from this
/// test, aimed at an INSTANT inside the first save's second, while a
/// plain `ulak sync` is running. An instant and not a delay because
/// under load a save lands nowhere near where a sleep was told to put
/// it: on two cores, seven attempts out of eight wrote on the far side
/// of the boundary they were aiming inside.
///
/// An attempt asserts nothing until it has built all three facts — the
/// server holds the first save, the local file holds the second, and the
/// two share a second — and one that misses says which it missed. The
/// shared second is the fix's precondition in disguise: the push runs
/// between the two saves, so sharing a second means the push started
/// inside it, which is exactly when the file falls in the window.
#[test]
fn a_same_size_save_in_the_same_second_still_reaches_the_server() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    // This scenario is about WHEN two writes happen, so it cannot be
    // timed next to the heavy siblings cargo runs alongside it.
    let _solo = TestServer::exclusive_sync();
    let server = &*server;
    let ws = Workspace::new();
    ws.set_host(&server.alias);
    let page = ws.project.join("site/index.html");

    // Warm: the layout, the footprint cache and the ledger all exist
    // after this, so the syncs the rounds time are the ordinary kind.
    ws.write("site/index.html", "warming-up");
    let out = ws.ulak(server).arg("sync").output().unwrap();
    assert!(
        out.status.success(),
        "the warm-up sync must work:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let remote_dir = extract_remote_dir(&String::from_utf8_lossy(&out.stderr))
        .expect("no remote dir in the sync's output");

    let mtime_of = |p: &std::path::Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .expect("the file was just written")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a modern mtime")
    };
    let remote_says = || {
        String::from_utf8_lossy(
            &server
                .ssh(&format!("cat {remote_dir}/site/index.html"))
                .stdout,
        )
        .trim()
        .to_string()
    };
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
    };

    // How far into the first save's second the second save is aimed.
    // Late on purpose: everything before it is time rsync has to read the
    // first save, which is what puts the copy on the server.
    const AIM: Duration = Duration::from_millis(900);
    // The first save has to land early enough to leave that room. Checked
    // rather than assumed, and tight: rejecting costs only a write.
    const HEAD_ROOM: u128 = 300;

    // mtimes come from the kernel's coarse time, not from the clock this
    // test reads, so a write issued just after a second begins can be
    // stamped just BEFORE it. Measured, 200 writes: Linux stamps up to
    // 2.46 ms behind, macOS 0.15 ms — and aiming at the boundary put
    // every one of forty CI attempts at "998 ms into its second".
    //
    // Worst of five probes, because one would be timing its own write.
    let mut lag = Duration::ZERO;
    let probe = ws.project.join("site/.mtime-probe");
    for i in 0..5 {
        std::fs::write(&probe, format!("probe {i}")).expect("write the probe");
        let stamped = mtime_of(&probe);
        lag = lag.max(now().saturating_sub(stamped));
    }
    std::fs::remove_file(&probe).ok();
    // Margin for the write itself, which the probe timed warm.
    let clears_the_boundary = lag + Duration::from_millis(30);
    assert!(
        clears_the_boundary < Duration::from_millis(HEAD_ROOM as u64),
        "the filesystem stamps mtimes {lag:?} behind this test's clock, so a save \
         cannot be placed inside a known second at all — with a resolution that \
         coarse the hole this test is about cannot form either"
    );

    let mut missed = Vec::new();
    // Bounded by the clock as well as the count: a failed alignment costs
    // a write, a failed round costs a whole sync, and only seconds bound
    // both. Pinned to two cores like CI, six runs built the case on
    // attempts 1, 12, 1, 2, 8 and 4 — the budget is set past the worst of
    // those, and the numbers are here so a later edit can tell a slower
    // runner from a harder case.
    const BUDGET: Duration = Duration::from_secs(90);
    let began = Instant::now();
    for attempt in 1..=40 {
        if began.elapsed() > BUDGET {
            missed.push(format!(
                "  (stopped after {BUDGET:?} with {} attempts left)",
                40 - attempt + 1
            ));
            break;
        }
        // Just past the top of a second: sleep most of the way, spin the
        // last stretch. Sleeping the whole way overshot far enough on two
        // cores to land the save near the END of its second — seven CI
        // rounds out of eight. A sleep is a lower bound; a spin is not.
        let edge = now();
        let target = Duration::from_secs(edge.as_secs() + 1) + clears_the_boundary;
        if let Some(most) = target
            .checked_sub(edge)
            .and_then(|d| d.checked_sub(Duration::from_millis(60)))
        {
            std::thread::sleep(most);
        }
        while now() < target {
            std::hint::spin_loop();
        }

        // Same length on purpose — a size change is the one thing the
        // quick check cannot miss. Numbered so a later attempt cannot be
        // satisfied by an earlier one's bytes.
        let v1 = format!("<h1>same-second-{attempt:02}-a</h1>");
        let v2 = format!("<h1>same-second-{attempt:02}-b</h1>");
        assert_eq!(v1.len(), v2.len(), "the two saves must be the same size");

        ws.write("site/index.html", &v1);
        let first = mtime_of(&page);
        if first.subsec_millis() as u128 > HEAD_ROOM {
            // No room left for a sync and a second save. Nothing spent.
            missed.push(format!(
                "  attempt {attempt}: first save landed {} ms into its second, too late to aim at",
                first.subsec_millis()
            ));
            continue;
        }

        // Piped and kept: a round that missed because the push FAILED
        // reads exactly like one that missed on timing.
        let syncing = ws
            .ulak_raw(server)
            .arg("sync")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("sync spawn");

        // An instant, not a delay: a delay measured from where the save
        // was SUPPOSED to land is what split the two seconds before.
        let target = Duration::from_secs(first.as_secs()) + AIM;
        if let Some(wait) = target.checked_sub(now()) {
            std::thread::sleep(wait);
        }
        let late = now() >= Duration::from_secs(first.as_secs() + 1);
        if !late {
            ws.write("site/index.html", &v2);
        }
        let second = mtime_of(&page);
        let done = syncing.wait_with_output().expect("sync to finish");

        let landed = remote_says();
        if !done.status.success() || late || second.as_secs() != first.as_secs() || landed != v1 {
            // A fact the hole is made of is missing, so there is nothing
            // to ask about. Which one is worth keeping.
            missed.push(format!(
                "  attempt {attempt}: exit={:?}, saves in {}, server took {landed:?}{}",
                done.status.code(),
                match (late, second.as_secs() == first.as_secs()) {
                    (true, _) => "— the second ran out before the second save".to_string(),
                    (_, true) => "one second".to_string(),
                    _ => format!("{} then {}", first.as_secs(), second.as_secs()),
                },
                match done.status.success() {
                    true => String::new(),
                    false => format!("\n{}", String::from_utf8_lossy(&done.stderr)),
                },
            ));
            continue;
        }

        // Built: same size, same second, different bytes.
        ws.ulak(server).arg("sync").assert().success();
        assert_eq!(
            remote_says(),
            v2,
            "a same-size save made in the same second as the last push never \
             travelled (attempt {attempt}, second {}). rsync's quick check \
             cannot see it and never will — closing that is what \
             `sync::offer_again` is for.\nlocal:  {:?}\nremote listing:\n{}",
            first.as_secs(),
            std::fs::read_to_string(&page).unwrap_or_default(),
            String::from_utf8_lossy(&server.ssh(&format!("ls -la {remote_dir}/site")).stdout),
        );
        return;
    }

    panic!(
        "no attempt got a save into the push's own second, so the case this \
         test is about was never built and nothing here was tested. That is \
         not a pass. This machine stamps mtimes {lag:?} behind its own clock, \
         so every save was aimed {clears_the_boundary:?} past a second \
         boundary — if the attempts below all landed at the END of a second, \
         that measurement is where to look first. Every attempt:\n{}",
        missed.join("\n")
    );
}

/// What a wiped runner needs before its SECOND job works.
///
/// Two different things are fresh when a runner is thrown away, and they
/// fail differently. The NAMESPACE is half the remote locator, so a new
/// one lands the job in a brand-new workspace: nothing is refused, the
/// server just grows a second copy and the first is orphaned. The UUID is
/// the ownership answer, so with the locator pinned but the uuid fresh
/// the sync IS refused — `settle_uuid` cannot tell "mine before the wipe"
/// from "another machine on the same checkout path", and there is no
/// terminal to ask in. Both have to be declared; measured here, because
/// the first version of this test pinned only the uuid and quietly
/// synced into a second workspace.
#[test]
fn a_declared_identity_lets_a_wiped_runner_sync_again() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = Workspace::new();
    let uuid = "e2e-declared-identity-9f2c";
    let namespace = "e2e-declared-identity";

    // The project has to name a server before anything can sync to one;
    // without this the run below fails on "no server is configured" and
    // never reaches the ownership question this test is about.
    ws.ulak(&server)
        .args(["init", &server.alias])
        .assert()
        .success();

    ws.write("site/index.html", "run-one");
    let first = ws
        .ulak(&server)
        .arg("sync")
        .env("ULAK_WORKSPACE_NAMESPACE", namespace)
        .env("ULAK_WORKSPACE_UUID", uuid)
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&first.get_output().stderr).to_string();
    let remote_dir =
        extract_remote_dir(&stderr).unwrap_or_else(|| panic!("no remote dir in output:\n{stderr}"));

    // The runner is thrown away and a fresh one checks the same commit
    // out at the same path: everything Ulak remembered is gone.
    let wipe = || {
        std::fs::remove_dir_all(ws.home.join(".local/state")).expect("a state directory to wipe")
    };
    wipe();

    ws.write("site/index.html", "run-two");
    ws.ulak(&server)
        .arg("sync")
        .env("ULAK_WORKSPACE_NAMESPACE", namespace)
        .env("ULAK_WORKSPACE_UUID", uuid)
        .assert()
        .success();

    let cat = server.ssh(&format!("cat {remote_dir}/site/index.html"));
    assert_eq!(
        String::from_utf8_lossy(&cat.stdout).trim(),
        "run-two",
        "the second run must reach the SAME workspace, not a new one"
    );

    // Locator pinned, identity not: this is the refusal the uuid answers,
    // and it must stay — on a laptop that question is worth asking.
    wipe();
    let refused = ws
        .ulak(&server)
        .arg("sync")
        .env("ULAK_WORKSPACE_NAMESPACE", namespace)
        .assert()
        .failure();
    let said = String::from_utf8_lossy(&refused.get_output().stderr).to_string();
    assert!(
        said.contains("already exists"),
        "expected the ownership refusal, got:\n{said}"
    );

    server.ssh(&format!("rm -rf .ulak/workspaces/{namespace}"));
}
