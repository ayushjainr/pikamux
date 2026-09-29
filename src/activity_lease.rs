//! One OS-owned observation lease per Pika profile. A lease is held through
//! each local or remote read, including reads finishing after their view exits.
//! Followers never steal a lock, remove its inode, or infer owner death by age.

use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub(crate) struct ObservationLease {
    path: PathBuf,
    owner: Mutex<Option<Arc<File>>>,
}

impl ObservationLease {
    pub(crate) fn new(state_dir: &Path) -> io::Result<Self> {
        if !state_dir.is_absolute() {
            return Err(io::Error::other("Observation state path must be absolute"));
        }
        let root = state_dir.join("activity-feed");
        crate::assistant_storage::directory(&root)?;
        let path = root.join("observer.lock");
        crate::assistant_storage::file(&path)?;
        Ok(Self {
            path,
            owner: Mutex::new(None),
        })
    }

    /// Only the local observation scheduler elects an owner. Remote workers
    /// use `current`, so a paused source cannot race into ownership remotely.
    pub(crate) fn acquire(&self, enabled: bool) -> io::Result<Option<Arc<File>>> {
        let mut owner = self.owner.lock().expect("observation lease");
        if !enabled {
            *owner = None;
            return Ok(None);
        }
        if let Some(lease) = owner.as_ref() {
            return Ok(Some(lease.clone()));
        }
        crate::assistant_storage::file(&self.path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(&self.path)?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                let lease = Arc::new(file);
                *owner = Some(lease.clone());
                Ok(Some(lease))
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn current(&self) -> Option<Arc<File>> {
        self.owner.lock().expect("observation lease").clone()
    }

    pub(crate) fn release(&self) {
        self.owner.lock().expect("observation lease").take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn one_os_lock_winner_across_independent_observers_and_failover() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("state");
        let barrier = Arc::new(Barrier::new(8));
        let winners = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let lease = ObservationLease::new(&root).unwrap();
                let barrier = barrier.clone();
                let winners = winners.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let held = lease.acquire(true).unwrap();
                    if held.is_some() {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                    barrier.wait();
                    assert_eq!(winners.load(Ordering::SeqCst), 1);
                    drop(held);
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let next = ObservationLease::new(&root).unwrap();
        assert!(next.acquire(true).unwrap().is_some());
    }

    #[test]
    fn paused_source_never_blocks_owner_and_inflight_read_fences_takeover() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("state");
        let paused = ObservationLease::new(&root).unwrap();
        assert!(paused.acquire(false).unwrap().is_none());
        let owner = ObservationLease::new(&root).unwrap();
        let read = owner.acquire(true).unwrap().unwrap();
        assert!(paused.acquire(true).unwrap().is_none());
        owner.release();
        assert!(paused.acquire(true).unwrap().is_none());
        drop(read);
        assert!(paused.acquire(true).unwrap().is_some());
        assert!(paused.acquire(false).unwrap().is_none());
        assert!(owner.acquire(true).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_shared_lock_fail_closed() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("state");
        let lease = ObservationLease::new(&root).unwrap();
        std::fs::set_permissions(&lease.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(lease.acquire(true).is_err());
        std::fs::remove_file(&lease.path).unwrap();
        std::os::unix::fs::symlink(temporary.path().join("missing"), &lease.path).unwrap();
        assert!(lease.acquire(true).is_err());
    }
}
