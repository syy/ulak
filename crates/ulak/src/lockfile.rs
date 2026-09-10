//! Per-workspace lock: sync/up/build/clean on the SAME workspace serialize.
//!
//! A local OS file lock is the right scope for the race it guards:
//! "a background sync runs while a manual `up` reads the tree", and both
//! processes live on this machine.
//!
//! It is NOT a claim on the remote workspace. Independent clients receive
//! separate namespaces, but two clients can deliberately configure the same
//! one and their local locks still cannot see each other. The manifest UUID
//! on the server is the final ownership proof for that explicit-sharing case.

use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::ui;

pub struct WorkspaceLock {
    file: File,
}

/// The one long-lived ulak service allowed on this machine.
///
/// Per-workspace locks protect a short reconcile from a command in the
/// foreground. They cannot stop two service loops from observing the
/// same intent and racing to own the same tunnels. This lock lives for
/// the whole service process, and the OS releases it even after SIGKILL.
pub struct ServiceLock {
    file: File,
}

impl ServiceLock {
    /// Take the machine-wide service door without waiting.
    ///
    /// A second launchd/systemd job must exit cleanly rather than sit in
    /// a restart loop. The service already holding this file remains the
    /// sole writer of the heartbeat, status files and local tunnels.
    pub fn try_acquire() -> Result<Option<ServiceLock>> {
        let dir = crate::invocation::state_dir_required()?.join("service");
        crate::invocation::private_dir(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join("instance.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot create service lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(ServiceLock { file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("cannot lock {}", path.display()))
            }
        }
    }
}

impl Drop for ServiceLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl WorkspaceLock {
    /// Acquire the lock for a workspace (by path-hash), waiting with a
    /// visible message if another ulak holds it.
    pub fn acquire(path_hash: &str) -> Result<WorkspaceLock> {
        let path = lock_path(path_hash)?;
        let file = File::create(&path)
            .with_context(|| format!("cannot create lock file {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                ui::info(
                    "another Ulak process is working on this workspace — waiting for it to finish",
                );
                file.lock()
                    .with_context(|| format!("cannot wait on lock {}", path.display()))?;
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("cannot lock {}", path.display()));
            }
        }
        Ok(WorkspaceLock { file })
    }

    /// Take the lock only if it is free, and never wait.
    ///
    /// This is the ONLY door the service uses. "A human always wins" is
    /// not a policy the service can follow by being polite — it follows
    /// from the service having no way to queue behind anyone. A tick
    /// that cannot get the lock is simply skipped; the next one is at
    /// most a few seconds away, and whatever held the lock was doing the
    /// same work anyway.
    pub fn try_acquire(path_hash: &str) -> Result<Option<WorkspaceLock>> {
        let path = lock_path(path_hash)?;
        let file = File::create(&path)
            .with_context(|| format!("cannot create lock file {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(WorkspaceLock { file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("cannot lock {}", path.display()))
            }
        }
    }
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn lock_path(path_hash: &str) -> Result<PathBuf> {
    // Through the one state root, or a service started with a different
    // XDG_STATE_HOME than the shell would take a DIFFERENT lock and the
    // serialization this file promises would quietly stop happening.
    let dir = crate::invocation::state_dir_required()?.join("locks");
    crate::invocation::private_dir(&dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir.join(format!("{path_hash}.lock")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_holder_waits_until_release() {
        let hash = format!("test-{}", std::process::id());
        let first = WorkspaceLock::acquire(&hash).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let h2 = hash.clone();
        let t = std::thread::spawn(move || {
            let _second = WorkspaceLock::acquire(&h2).unwrap();
            tx.send(()).unwrap();
        });

        // While the first lock is held, the second must not acquire.
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "second lock acquired while first was held"
        );
        drop(first);
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "second lock never acquired after release"
        );
        t.join().unwrap();
    }

    #[test]
    fn the_service_door_never_waits() {
        // The whole "a human always wins" promise: the service asks, and
        // if the answer is no it skips the tick instead of queueing. A
        // blocking acquire here would put the service's 15-minute
        // reconcile in front of the user's next command.
        let hash = format!("try-{}", std::process::id());
        let held = WorkspaceLock::acquire(&hash).unwrap();

        let started = std::time::Instant::now();
        assert!(
            WorkspaceLock::try_acquire(&hash).unwrap().is_none(),
            "try_acquire must refuse a held lock"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "try_acquire waited {:?} — it must not wait at all",
            started.elapsed()
        );

        drop(held);
        assert!(
            WorkspaceLock::try_acquire(&hash).unwrap().is_some(),
            "a free lock must be taken"
        );
    }

    #[test]
    fn only_one_service_can_own_the_machine() {
        let first = ServiceLock::try_acquire()
            .unwrap()
            .expect("the first service must take the machine");
        assert!(
            ServiceLock::try_acquire().unwrap().is_none(),
            "a second service must leave while the first one is alive"
        );
        drop(first);
        assert!(
            ServiceLock::try_acquire().unwrap().is_some(),
            "the OS lock must become available when the service exits"
        );
    }
}
