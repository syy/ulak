//! Bounded subprocesses: nothing ulak waits on is unbounded.
//!
//! `ConnectTimeout` bounds only the CONNECT phase. Once bytes have
//! flowed there is nothing left for TCP to time out, so a link that dies
//! mid-command (the laptop lid, a wifi handover, a VPN drop) leaves ssh
//! blocked in read() with no deadline of its own. Measured on this
//! machine: a bare `wait_with_output()` on such a link sat for
//! **13 minutes 21 seconds** and never returned — the measurement was
//! cut, not the command. The limit has to live in the WAIT.
//!
//! Draining is CONCURRENT on purpose. Measured here: a child whose
//! stdout nobody reads blocks at **1024 bytes**. That is not a large
//! budget for anything this crate runs: the service's own probe answers
//! with three absolute Compose paths per container — 9891 bytes for 21
//! containers, ~470 each, measured on Docker 29.6.2 — so it passes 1024
//! at the THIRD container on the destination. The naive shape — spawn,
//! poll `try_wait()`, read once it exits — would hang that probe until
//! the deadline and lock every sync behind an itemize of tens of
//! thousands of lines, i.e. it would trade an infinite hang for a
//! 15-second one. So the pipes get their own threads from the first
//! instant, exactly like the feeder thread `sync.rs` already used.
//!
//! What is NOT bounded, deliberately: foreground commands that stream to
//! the user's terminal (`logs -f`, `exec`, an interactive shell). Those
//! are not silent waits — output is on screen and Ctrl-C works. Their
//! protection is `ConnectTimeout` on the way in.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// One remote script (`ssh … sh -s`). Most are shell builtins plus
/// docker calls that answer from local state — `compose config`,
/// `compose ps`, `docker ps`, `volume`/`network inspect`, `--version`
/// and `info` — which is what makes 60 s generous rather than tight.
/// `clean` is the exception this bound does not really cover: besides
/// `compose down` it sends a `docker run --rm … busybox` to wipe the
/// workspace, and pulling that image on a server that lacks it goes to the
/// network.
pub const SCRIPT: Duration = Duration::from_secs(60);

/// One liveness question. A warm master answers in 0.22–0.35 s and a
/// cold connection in 0.92–1.32 s (measured), so 15 s is pure headroom.
pub const PROBE: Duration = Duration::from_secs(15);

/// The bootstrap push: the compose files, the env files, nothing else.
pub const BOOTSTRAP: Duration = Duration::from_secs(120);

/// A full reconcile. A large workspace — hundreds of MB, tens of thousands
/// of files — reconciles in ~2.9 s warm (measured); 15 minutes is room
/// for the FIRST sync of a large project over a slow uplink.
pub const RECONCILE: Duration = Duration::from_secs(900);

/// A deadline that several bounded steps SHARE.
///
/// `RECONCILE` bounds one rsync leg. A reconcile is two legs plus a
/// remote script, so bounding each of them on its own let the whole
/// thing cost ~32 minutes — and, because the per-workspace lock is held
/// throughout, that was also the worst a human `ulak docker compose up` could wait
/// behind the service. One budget for the whole reconcile turns "the
/// human always wins" into a number instead of a hope.
pub struct Budget {
    until: Instant,
}

impl Budget {
    pub fn new(limit: Duration) -> Budget {
        Budget {
            until: Instant::now() + limit,
        }
    }

    /// What the next step gets. Never zero: a step given no time at all
    /// cannot even report why it stopped, and "the budget ran out" is a
    /// better sentence from a command that ran than from one that was
    /// never spawned.
    pub fn remaining(&self) -> Duration {
        self.until
            .saturating_duration_since(Instant::now())
            .max(Duration::from_secs(1))
    }
}

/// After the child is reaped its pipes are normally already at EOF. They
/// are not when a grandchild inherited them — rsync's own `ssh` keeps
/// our stderr open — so the drain gets a bounded grace and then we take
/// what arrived. Waiting forever here would reopen the hole this module
/// exists to close.
///
/// It is ONE grace for both pipes, not one each. They are drained
/// concurrently, so spending it twice bounds nothing extra and only
/// doubles the wait. Measured on Linux, where `/bin/sh` does NOT exec a
/// single command and `sh -c "sleep 30"` therefore leaves the `sleep` as
/// a grandchild holding both ends: a 0.3 s deadline took **10.3 s** to
/// return, one grace per pipe. macOS hid it — its `sh` execs, so the
/// killed child IS the only writer and EOF is immediate.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// The result of a bounded run. `timed_out` is the one thing a plain
/// `Output` cannot say, and it is exactly what callers steer on.
pub struct Bounded {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

impl Bounded {
    pub fn into_output(self) -> Output {
        Output {
            status: self.status,
            stdout: self.stdout,
            stderr: self.stderr,
        }
    }
}

/// Run `cmd` to completion or to `limit`, whichever comes first.
///
/// stdin is fed from a thread when `stdin` is `Some` (and closed after,
/// which is the child's EOF); stdout and stderr are drained from their
/// own threads throughout. On expiry the child is killed and reaped, and
/// whatever output arrived is still returned — a timeout report that
/// throws away the server's last words is a worse timeout report.
pub fn run_bounded(cmd: &mut Command, stdin: Option<Vec<u8>>, limit: Duration) -> Result<Bounded> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot spawn {program}"))?;

    if let Some(payload) = stdin {
        let mut sink = child.stdin.take().expect("piped stdin");
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = sink.write_all(&payload);
            // `sink` drops here: that drop IS the child's EOF.
        });
    }
    let out = drain(child.stdout.take().expect("piped stdout"));
    let err = drain(child.stderr.take().expect("piped stderr"));

    let (status, timed_out) = match wait_for(&mut child, limit)? {
        Some(status) => (status, false),
        None => {
            let _ = child.kill();
            let status = child.wait().context("cannot reap the timed-out child")?;
            (status, true)
        }
    };

    // One deadline shared by both pipes, for the reason DRAIN_GRACE gives.
    let give_up = Instant::now() + DRAIN_GRACE;
    Ok(Bounded {
        status,
        stdout: out.take_by(give_up),
        stderr: err.take_by(give_up),
        timed_out,
    })
}

/// Wait for an already-spawned child, up to `limit`. `None` means the
/// limit ran out and the child is still alive — the caller decides
/// whether that is a failure (a hung script) or the normal shape (an
/// ssh that legitimately holds the tunnels open).
///
/// The nap ramps because both ends matter: a probe that answers in
/// 0.25 s must not pay a fixed poll interval, and a 15-minute reconcile
/// must not spin. Sleeping is also the reason there is no libc here —
/// `Child::try_wait` plus `thread::sleep` is the whole mechanism.
pub fn wait_for(child: &mut Child, limit: Duration) -> Result<Option<ExitStatus>> {
    let deadline = Instant::now() + limit;
    let mut nap = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait().context("cannot poll a child process")? {
            return Ok(Some(status));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        std::thread::sleep(nap.min(left));
        nap = (nap * 2).min(Duration::from_millis(50));
    }
}

/// A pipe being read on its own thread: the bytes so far, and the signal
/// that no more are coming.
///
/// The bytes accumulate behind the lock instead of arriving with the
/// signal because of what a timeout IS. The grandchild still holds the
/// write end, so EOF never comes, so a drain that only hands its buffer
/// over at EOF hands over nothing — and it does that precisely when the
/// last thing the command said is the only explanation the user gets.
struct Drain {
    so_far: Arc<Mutex<Vec<u8>>>,
    eof: mpsc::Receiver<()>,
}

fn drain<R: Read + Send + 'static>(mut pipe: R) -> Drain {
    let so_far = Arc::new(Mutex::new(Vec::new()));
    let (tx, eof) = mpsc::channel();
    let into = Arc::clone(&so_far);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => grab(&into).extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = tx.send(());
    });
    Drain { so_far, eof }
}

impl Drain {
    /// Everything the pipe produced by `deadline`, whether or not it
    /// reached EOF. A deadline already past is not a special case: there
    /// is nothing left to wait for, and the bytes are still taken.
    fn take_by(self, deadline: Instant) -> Vec<u8> {
        let _ = self
            .eof
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
        std::mem::take(&mut *grab(&self.so_far))
    }
}

/// The drain thread has no state of its own to corrupt, so a poisoned
/// lock means only that some other thread panicked — taking the bytes
/// anyway beats losing them to a panic that had nothing to do with them.
fn grab(bytes: &Mutex<Vec<u8>>) -> std::sync::MutexGuard<'_, Vec<u8>> {
    bytes.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_that_never_ends_is_killed_at_the_deadline() {
        // The whole point: without this, `sleep 30` is a 30-second hang
        // and a dead ssh is a 13-minute one.
        //
        // `sleep` is spawned directly rather than under a shell so that
        // the process we kill is the only one holding the pipes: EOF
        // follows the kill, no grace is owed, and the deadline is the
        // whole cost. The shell version is the test below, and writing
        // it as `sh -c` here is what let one grace per pipe hide for as
        // long as it did — on macOS that `sh` execs, so this was the
        // only case ever actually measured.
        let started = Instant::now();
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let out = run_bounded(&mut cmd, None, Duration::from_millis(300)).unwrap();
        assert!(out.timed_out, "the deadline must fire");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "killing a child that holds its own pipes owes no grace, but took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_grandchild_holding_the_pipes_costs_one_grace_and_not_one_each() {
        // rsync's own `ssh` in miniature: the process we kill is not the
        // one holding our stdout and stderr open, so both drains run out
        // their grace. `sleep 30 & wait` forces that shape on every
        // platform — a backgrounded job is one no shell can exec away.
        //
        // The bound is the contract, not the implementation: a deadline
        // costs at most itself plus ONE grace. 10.3 s — a grace per pipe
        // — is what CI measured before the drains shared a deadline.
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & wait"]);
        let out = run_bounded(&mut cmd, None, Duration::from_millis(300)).unwrap();
        assert!(out.timed_out, "the deadline must fire");
        assert!(
            started.elapsed() < Duration::from_millis(300) + DRAIN_GRACE + Duration::from_secs(2),
            "one deadline plus one grace is the whole bill, but it took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_timed_out_command_still_reports_what_it_managed_to_say() {
        // The last words of a command that then hung are the only
        // explanation its user gets, and they are held by a grandchild
        // that never reaches EOF. Draining only at EOF returned an empty
        // stderr here — a timeout report with the reason cut out of it.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo talking; echo dying >&2; sleep 30 & wait"]);
        let out = run_bounded(&mut cmd, None, Duration::from_millis(300)).unwrap();
        assert!(out.timed_out, "the deadline must fire");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "talking\n");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "dying\n");
    }

    #[test]
    fn output_bigger_than_a_pipe_buffer_does_not_deadlock() {
        // Measured at 1024 bytes on this machine: a child nobody reads
        // blocks in write(). 512 KB is far past every platform's buffer,
        // so a serial implementation cannot pass.
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "yes 0123456789abcdefghijklmnopqrstuvwxyz | head -c 524288",
        ]);
        let out = run_bounded(&mut cmd, None, Duration::from_secs(20)).unwrap();
        assert!(!out.timed_out, "large output must not hit the deadline");
        assert_eq!(out.stdout.len(), 524_288);
        assert_eq!(out.status.code(), Some(0));
    }

    #[test]
    fn stdin_is_fed_and_closed_so_the_child_sees_eof() {
        // `sh -s` is exactly how every remote script travels, and a
        // script that never gets EOF never runs.
        let mut cmd = Command::new("sh");
        cmd.arg("-s");
        let payload = b"printf 'hello %s\\n' world\n".to_vec();
        let out = run_bounded(&mut cmd, Some(payload), Duration::from_secs(20)).unwrap();
        assert!(!out.timed_out);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello world\n");
    }

    #[test]
    fn a_large_stdin_payload_does_not_deadlock_either() {
        // `sync::delete_remote`'s exact shape: the doomed paths ride
        // inside a heredoc in the script itself, and a branch switch
        // makes that list far longer than any pipe buffer. The old
        // blocking `write_all` on the main thread was one busy pipe away
        // from deadlocking here.
        let mut script = String::from("n=0\nwhile IFS= read -r p; do n=$((n+1)); done <<'END'\n");
        for i in 0..40_000 {
            script.push_str(&format!("path/number/{i}\n"));
        }
        script.push_str("END\nprintf 'lines %s\\n' \"$n\"\n");
        assert!(
            script.len() > 512_000,
            "payload must exceed any pipe buffer"
        );

        let mut cmd = Command::new("sh");
        cmd.arg("-s");
        let out =
            run_bounded(&mut cmd, Some(script.into_bytes()), Duration::from_secs(30)).unwrap();
        assert!(!out.timed_out);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "lines 40000\n");
    }

    #[test]
    fn one_budget_covers_every_leg_of_a_reconcile() {
        // Without this, a reconcile made of two rsync legs could cost
        // two full RECONCILE windows while holding the per-workspace lock.
        let budget = Budget::new(Duration::from_secs(10));
        let first = budget.remaining();
        assert!(first <= Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(120));
        assert!(
            budget.remaining() < first,
            "a later leg must get LESS time, not a fresh window"
        );

        // Exhausted still means "run and report", never "never start".
        let spent = Budget::new(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(spent.remaining(), Duration::from_secs(1));
    }

    #[test]
    fn stderr_and_exit_codes_survive() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 7"]);
        let out = run_bounded(&mut cmd, None, Duration::from_secs(20)).unwrap();
        assert_eq!(out.status.code(), Some(7));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
    }
}
