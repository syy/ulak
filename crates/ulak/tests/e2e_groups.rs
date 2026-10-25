//! A group the account gains reaches the next command over the link, and
//! running commands finish. Fixture only: it changes the account as root.
//! A binary of its own, since a gained group re-logs every link scenario.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use common::{TestServer, Workspace};

const ACCOUNT: &str = "dev";
const GROUP: &str = "ulak-e2e-gained";
/// `python3` is the link's helper. busybox's ps has no `-p`.
const WHO: &str = "id -Gn; cat /proc/$PPID/comm";

fn link(server: &TestServer, ws: &Workspace, control: &Path, remote: &str) -> Command {
    let mut cmd = ws.ulak_raw(server);
    cmd.arg("__link")
        .args(["-o", "ControlMaster=auto", "-o"])
        .arg(format!("ControlPath={}", control.display()))
        .args([
            "-o",
            "ControlPersist=60",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "BatchMode=yes",
            "--",
        ])
        .arg(&server.alias)
        .arg(remote);
    cmd
}

fn said(out: &Output) -> (Option<i32>, String, String) {
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_group_the_account_gains_reaches_the_next_command_and_running_ones_finish() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    if server.is_real_host() {
        assert!(
            std::env::var("ULAK_TEST_E2E")
                .unwrap_or_default()
                .starts_with("host:"),
            "the backend reports itself a real host without ULAK_TEST_E2E asking for one"
        );
        return;
    }
    let ws = Workspace::new();
    let runtime = server
        .env
        .iter()
        .find(|(k, _)| k == "XDG_RUNTIME_DIR")
        .map(|(_, v)| PathBuf::from(v))
        .expect("the harness gives every server a runtime dir");
    let dir = runtime.join("ulak");
    std::fs::create_dir_all(&dir).unwrap();
    let control = dir.join("gained");

    let before = link(&server, &ws, &control, WHO).output().unwrap();
    let (code, text, err) = said(&before);
    assert_eq!(code, Some(0), "{text}{err}");
    assert!(
        text.ends_with("\npython3\n"),
        "not over the link: {text}{err}"
    );
    assert!(!text.contains(GROUP), "{text}");

    let running = link(&server, &ws, &control, "sleep 4; echo kept")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));

    let added = server
        .as_root(&format!(
            "addgroup -S {GROUP} && addgroup {ACCOUNT} {GROUP}"
        ))
        .expect("the fixture runs commands as root");
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    // A helper asks after the account's groups at most once a second.
    std::thread::sleep(Duration::from_millis(1200));

    for which in ["first", "second"] {
        let after = link(&server, &ws, &control, WHO).output().unwrap();
        let (code, text, err) = said(&after);
        assert_eq!(code, Some(0), "{which}: {text}{err}");
        assert!(
            text.split_whitespace().any(|g| g == GROUP),
            "the {which} command after the group was added lacks it: {text}{err}"
        );
        assert!(
            text.ends_with("\npython3\n"),
            "the {which} command did not take the link: {text}"
        );
    }

    let kept = running.wait_with_output().unwrap();
    assert_eq!(
        (kept.status.code(), kept.stdout.as_slice()),
        (Some(0), b"kept\n".as_slice()),
        "the command the old connection carried did not finish: {}",
        String::from_utf8_lossy(&kept.stderr)
    );
}
