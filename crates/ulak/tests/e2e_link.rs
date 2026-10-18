//! The link against a real sshd: more commands than sshd's MaxSessions run
//! side by side on one connection, a killed caller takes its remote
//! command and its pipe with it, and what ssh's own contract promises —
//! exit codes, separate streams, stdin's bytes, a pty for `-tt` — comes
//! through the link the same.
//!
//! Most scenarios drive `ulak __link` directly with the arguments
//! `Ssh::command` gives it, each around a control path of its own, so they
//! can hold a command open, kill it, or start sixteen at once without a
//! Docker workload. The last one goes through `ulak docker` and `ulak
//! doctor` to prove that ulak's own commands take the link.
//!
//! The helpers these start are left to leave on their own, ten minutes on
//! and once their last command ends, as the product leaves them: one per
//! scenario's connection, so no suite here waits on another's.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{TestServer, Workspace};

/// One scenario's own master, and the way to reach it.
struct Master<'a> {
    server: &'a TestServer,
    ws: Workspace,
    control: PathBuf,
}

impl<'a> Master<'a> {
    /// A control path nothing has used yet: no master, no link.
    fn cold(server: &'a TestServer, scenario: &str) -> Master<'a> {
        let runtime = server
            .env
            .iter()
            .find(|(k, _)| k == "XDG_RUNTIME_DIR")
            .map(|(_, v)| PathBuf::from(v))
            .expect("the harness gives every server a runtime dir");
        let dir = runtime.join("ulak");
        std::fs::create_dir_all(&dir).unwrap();
        Master {
            server,
            ws: Workspace::new(),
            control: dir.join(scenario),
        }
    }

    /// The first command logs in, starts the helper and forwards to it,
    /// and must still answer like ssh.
    fn warm(server: &'a TestServer, scenario: &str) -> Master<'a> {
        let master = Master::cold(server, scenario);
        let out = master.run(&[], "echo warm", b"");
        assert_eq!(
            (out.status.code(), out.stdout.as_slice()),
            (Some(0), b"warm\n".as_slice()),
            "the first command failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            master.forwarded(),
            "a first command and still no link beside its master in {}",
            master.control.parent().unwrap().display()
        );
        master
    }

    /// `ulak __link` with `Ssh::command`'s options, `extra` before `--`.
    fn command(&self, extra: &[&str], remote: &str) -> Command {
        let mut cmd = self.ws.ulak_raw(self.server);
        cmd.arg("__link")
            .args(["-o", "ControlMaster=auto", "-o"])
            .arg(format!("ControlPath={}", self.control.display()))
            .args([
                "-o",
                "ControlPersist=60",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "BatchMode=yes",
            ])
            .args(extra)
            .arg("--")
            .arg(&self.server.alias)
            .arg(remote);
        cmd
    }

    fn run(&self, extra: &[&str], remote: &str, stdin: &[u8]) -> Output {
        let mut child = self
            .command(extra, remote)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("ulak binary");
        let mut input = child.stdin.take().unwrap();
        let bytes = stdin.to_vec();
        let feeder = std::thread::spawn(move || {
            let _ = input.write_all(&bytes);
        });
        let out = child.wait_with_output().unwrap();
        let _ = feeder.join();
        out
    }

    /// The local end of the link: a socket beside the master's own.
    fn forwarded(&self) -> bool {
        use std::os::unix::fs::FileTypeExt;
        let name = self.control.file_name().unwrap().to_string_lossy();
        std::fs::read_dir(self.control.parent().unwrap())
            .unwrap()
            .flatten()
            .any(|e| {
                let file = e.file_name().to_string_lossy().into_owned();
                file.starts_with(&format!("{name}-l"))
                    && !file.contains('.')
                    && e.file_type().is_ok_and(|t| t.is_socket())
            })
    }

    /// The link started by a caller of its own, `sh`, which passes nothing
    /// on to it, as `ulak docker` does not. The `:` keeps sh from exec'ing
    /// the link in its own place.
    fn under_a_caller(&self, remote: &str) -> Command {
        let link = self.command(&[], remote);
        let mut wrapper = Command::new("sh");
        wrapper.arg("-c").arg("\"$0\" \"$@\"; :");
        wrapper.arg(link.get_program()).args(link.get_args());
        for (k, v) in link.get_envs() {
            match v {
                Some(v) => wrapper.env(k, v),
                None => wrapper.env_remove(k),
            };
        }
        if let Some(dir) = link.get_current_dir() {
            wrapper.current_dir(dir);
        }
        wrapper
    }

    /// A command held open on the server; its first line of output is
    /// the remote shell's pid.
    fn hold(&self, through_a_parent: bool) -> (Child, BufReader<std::process::ChildStdout>, i64) {
        let remote = "echo $$; exec sleep 300";
        let mut cmd = if through_a_parent {
            self.under_a_caller(remote)
        } else {
            self.command(&[], remote)
        };
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let pid = line
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("no pid from the held command: {line:?}"));
        (child, stdout, pid)
    }

    fn remote_alive(&self, pid: i64) -> bool {
        self.server
            .ssh(&format!("kill -0 {pid} 2>/dev/null"))
            .status
            .success()
    }
}

fn ends_within(mut stdout: impl Read + Send + 'static, limit: Duration) -> bool {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut rest = Vec::new();
        let _ = stdout.read_to_end(&mut rest);
        let _ = tx.send(());
    });
    rx.recv_timeout(limit).is_ok()
}

fn gone_within(master: &Master, pid: i64, limit: Duration) -> bool {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if !master.remote_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

/// Sixteen commands at once through one master: past sshd's MaxSessions
/// (10 by default) every one of them still runs at once, and none falls
/// out to a connection of its own with `session request failed` in its
/// stderr, which is what a session-per-command master did.
#[test]
fn more_commands_than_sshd_has_sessions_run_side_by_side_on_one_connection() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let master = Master::warm(&server, "e2e-link-wide");
    let started = Instant::now();
    let outs: Vec<Output> = std::thread::scope(|scope| {
        let master = &master;
        let handles: Vec<_> = (0..16)
            .map(|i| scope.spawn(move || master.run(&[], &format!("sleep 3; echo {i}"), b"")))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let took = started.elapsed();
    for (i, out) in outs.iter().enumerate() {
        assert_eq!(
            (out.status.code(), String::from_utf8_lossy(&out.stdout)),
            (Some(0), format!("{i}\n").into()),
            "command {i}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stderr.is_empty(),
            "command {i} said: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(
        took < Duration::from_secs(12),
        "sixteen 3-second commands took {took:?}: they waited for each other"
    );
}

/// A caller killed mid-command, or the process that started it: the pipe
/// ends at once and the command on the server goes with it. Through a
/// session the command ran on in both cases, and a client orphaned by its
/// caller kept the pipe open.
#[test]
fn a_killed_caller_takes_its_remote_command_and_its_pipe_with_it() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let master = Master::warm(&server, "e2e-link-kill");
    for through_a_parent in [false, true] {
        let (mut child, stdout, pid) = master.hold(through_a_parent);
        assert!(master.remote_alive(pid), "the held command is not running");
        child.kill().unwrap();
        let _ = child.wait();
        assert!(
            ends_within(stdout, Duration::from_secs(5)),
            "the caller's pipe was still open 5 s after it was killed (parent: {through_a_parent})"
        );
        assert!(
            gone_within(&master, pid, Duration::from_secs(15)),
            "the remote command outlived its killed caller by 15 s (parent: {through_a_parent})"
        );
    }
}

/// `timeout`, or a CI runner cancelling a job: TERM to the caller's whole
/// process group, which the caller dies of at once. The command it was
/// passed to still ends its own way, a trap that takes two seconds, and not
/// under a hangup a moment after its caller went. A second copy of the same
/// TERM, which a caller that passes signals on would send, is not a second
/// ask.
#[test]
fn a_signal_to_the_callers_whole_group_leaves_the_command_its_own_ending() {
    use std::os::unix::process::CommandExt;
    let Some(server) = TestServer::shared() else {
        return;
    };
    let master = Master::warm(&server, "e2e-link-group-term");
    let mut caller = master
        .under_a_caller(
            "trap 'echo stopping; sleep 2; echo stopped; exit 7' TERM; \
             echo ready; while :; do sleep 0.1; done",
        )
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(caller.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    let pgrep = Command::new("pgrep")
        .args(["-P", &caller.id().to_string()])
        .output()
        .unwrap();
    let link = String::from_utf8_lossy(&pgrep.stdout).trim().to_string();
    assert!(!link.is_empty(), "no link under the caller");
    for target in [format!("-{}", caller.id()), link] {
        assert!(
            Command::new("kill")
                .args(["-TERM", "--", &target])
                .status()
                .unwrap()
                .success()
        );
    }
    let _ = caller.wait();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut rest = String::new();
        let _ = stdout.read_to_string(&mut rest);
        let _ = tx.send(rest);
    });
    let rest = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("the pipe was still open 15 s after the TERM");
    assert_eq!(rest, "stopping\nstopped\n");
}

/// Each connection has a helper of its own, with that connection's login:
/// its groups, its forwarded agent. Commands through one connection share
/// one helper; another connection's commands do not reach it.
#[test]
fn each_connection_has_a_helper_of_its_own() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let (a, b) = (
        Master::warm(&server, "e2e-link-own-a"),
        Master::warm(&server, "e2e-link-own-b"),
    );
    let helper = |master: &Master| {
        let out = master.run(&[], "echo $PPID", b"");
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let first = helper(&a);
    assert_eq!(helper(&a), first);
    assert_ne!(helper(&b), first);
}

/// The first command after an idle ControlPersist is a command like any
/// other: its caller's parent killed, the pipe ends and so does the
/// command. Left to ssh, it was a session that kept both.
#[test]
fn the_first_command_logs_in_and_still_goes_with_its_caller() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let master = Master::cold(&server, "e2e-link-cold");
    let (mut child, stdout, pid) = master.hold(true);
    assert!(
        master.forwarded(),
        "the first command went without the link"
    );
    child.kill().unwrap();
    let _ = child.wait();
    assert!(
        ends_within(stdout, Duration::from_secs(5)),
        "the caller's pipe was still open 5 s after its parent was killed"
    );
    assert!(
        gone_within(&master, pid, Duration::from_secs(15)),
        "the first remote command outlived its killed caller by 15 s"
    );
}

/// Sixteen first commands at once are one login: every one runs under
/// the helper, and none meets sshd's MaxStartups or a master that another
/// one made first.
#[test]
fn first_commands_at_once_log_in_once_and_all_take_the_link() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let master = Master::cold(&server, "e2e-link-cold-wide");
    let ran_by = "case $(ps -o comm= -p $PPID) in python*) echo link;; *) echo session;; esac";
    let outs: Vec<Output> = std::thread::scope(|scope| {
        let master = &master;
        let handles: Vec<_> = (0..16)
            .map(|_| scope.spawn(move || master.run(&[], ran_by, b"")))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (i, out) in outs.iter().enumerate() {
        assert_eq!(
            (out.status.code(), String::from_utf8_lossy(&out.stdout)),
            (Some(0), "link\n".into()),
            "command {i}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stderr.is_empty(),
            "command {i} said: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn exit_codes_and_the_two_streams_come_back_as_ssh_gives_them() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let master = Master::warm(&server, "e2e-link-codes");
    let out = master.run(&[], "printf out; printf err >&2; exit 7", b"");
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(out.stdout, b"out");
    assert_eq!(out.stderr, b"err");
    // The shell's convention for a command killed by a signal.
    let out = master.run(&[], "kill -TERM $$", b"");
    assert_eq!(out.status.code(), Some(143), "{out:?}");
    // The words after the destination are joined and split again by the
    // far shell, as ssh does with them.
    let mut cmd = master.command(&[], "printf");
    cmd.args(["%s-", "a", "b"]);
    let out = cmd.output().unwrap();
    assert_eq!(out.stdout, b"a-b-", "{out:?}");
}

#[test]
fn stdin_reaches_the_server_byte_for_byte() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let master = Master::warm(&server, "e2e-link-stdin");
    let blob: Vec<u8> = (0..2_000_000u32).map(|i| (i * 31 % 253) as u8).collect();
    let out = master.run(&[], "cat", &blob);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout == blob,
        "{} of {} bytes came back",
        out.stdout.len(),
        blob.len()
    );
    // A command that stops reading early still answers.
    let out = master.run(&[], "head -c 3; exit 5", &vec![b'z'; 8 << 20]);
    assert_eq!(
        (out.status.code(), out.stdout.as_slice()),
        (Some(5), b"zzz".as_slice())
    );
    // -n is stdin from /dev/null, as with ssh.
    let out = master.run(&["-n"], "wc -c", b"never read");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");
}

/// `-tt` without a terminal here: a pty there, of the size a terminal
/// would most likely have had.
#[test]
fn a_forced_tty_is_a_pty_on_the_server() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let master = Master::warm(&server, "e2e-link-pty");
    let out = master.run(&["-tt"], "stty size; tty", b"");
    let said = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{said}");
    assert!(said.contains("24 80"), "{said:?}");
    assert!(said.contains("/dev/pts/"), "{said:?}");
}

/// sshd ends a pty session with its command, even when a child left in the
/// background still holds the terminal; so does the link.
#[test]
fn a_forced_tty_ends_with_its_command_whatever_it_left_behind() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let master = Master::warm(&server, "e2e-link-pty-left");
    let started = Instant::now();
    let out = master.run(
        &["-tt"],
        "trap '' HUP; sleep 60 & echo pid=$!; echo bye",
        b"",
    );
    let elapsed = started.elapsed();
    let said = String::from_utf8_lossy(&out.stdout);
    if let Some(pid) = said
        .split("pid=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
    {
        server.ssh(&format!("kill {pid} 2>/dev/null"));
    }
    assert_eq!(out.status.code(), Some(0), "{said}");
    assert!(said.contains("bye"), "{said:?}");
    assert!(
        elapsed < Duration::from_secs(15),
        "the answer waited {elapsed:?} for a background child"
    );
}

/// ulak's own commands take the link, and doctor says so.
#[test]
fn ulaks_own_commands_ride_the_link_and_doctor_says_so() {
    let Some(server) = TestServer::shared() else {
        return;
    };
    let ws = Workspace::new();
    std::fs::remove_file(ws.project.join("compose.yaml")).unwrap();
    let init = ws
        .ulak(&server)
        .args(["init", &server.alias])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    for _ in 0..2 {
        let info = ws
            .ulak(&server)
            .args(["docker", "info", "--format", "{{.ServerVersion}}"])
            .output()
            .unwrap();
        assert!(
            info.status.success() && !info.stdout.is_empty(),
            "docker info failed: {}",
            String::from_utf8_lossy(&info.stderr)
        );
    }
    let doctor = ws.ulak(&server).arg("doctor").output().unwrap();
    let report = String::from_utf8_lossy(&doctor.stdout);
    let row = report
        .lines()
        .find(|l| l.split_whitespace().nth(1) == Some("link"))
        .unwrap_or_else(|| panic!("no link row in doctor's report:\n{report}"));
    assert!(row.contains("one connection"), "{row}");
}
