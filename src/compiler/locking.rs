//! This module handles the locking logic during compilation.

use crate::util::flock;
use crate::{
    CargoResult,
    compiler::{BuildRunner, Unit},
    util::{FileLock, Filesystem},
};

use crate::util::data_structures::HashMap;
use anyhow::bail;
use parking_lot::RwLock;
use std::{
    fmt::{Display, Formatter},
    path::PathBuf,
    sync::Arc,
};
use tracing::instrument;

/// A struct to store the lock handles for build units during compilation.
///
/// Handles are stored behind an [`Arc`] so that a blocking `flock` can be
/// performed without holding the internal lock table: blocking on a file lock
/// while holding the table lock would serialize every other lock operation in
/// this process (including operations that the lock we are waiting on may
/// depend on), turning a cross-process wait into a deadlock.
pub struct LockManager {
    locks: RwLock<HashMap<LockKey, Arc<FileLock>>>,
}

impl LockManager {
    pub fn new() -> Self {
        Self {
            locks: RwLock::new(HashMap::default()),
        }
    }

    /// Takes a shared lock on a given [`Unit`]
    /// This prevents other Cargo instances from compiling (writing) to
    /// this build unit.
    ///
    /// This function returns a [`LockKey`] which can be used to
    /// upgrade/unlock the lock.
    #[instrument(skip_all, fields(key))]
    pub fn lock_shared(
        &self,
        build_runner: &BuildRunner<'_, '_>,
        unit: &Unit,
    ) -> CargoResult<LockKey> {
        let key = LockKey::from_unit(build_runner, unit);
        tracing::Span::current().record("key", key.0.to_str());

        if let Some(lock) = self.locks.read().get(&key) {
            let lock = Arc::clone(lock);
            flock::lock_shared(lock.file())?;
            return Ok(key);
        }

        // Open (and, if necessary, block for) the lock without holding the
        // lock table.
        let fs = Filesystem::new(key.0.clone());
        let lock_msg = format!(
            "{} ({})",
            unit.pkg.name(),
            build_runner.files().unit_hash(unit)
        );
        let lock = fs.open_ro_shared_create(&key.0, build_runner.bcx.gctx, &lock_msg)?;
        self.locks.write().insert(key.clone(), Arc::new(lock));

        Ok(key)
    }

    /// Takes a shared lock on an arbitrary lock file (not necessarily tied to
    /// a build unit). Used for the per-build-unit state locks of the
    /// cross-workspace build cache, which are acquired from worker threads
    /// that have no shell access, so no "Blocking" message is printed.
    ///
    /// This function returns a [`LockKey`] which can be used to
    /// upgrade/unlock the lock.
    #[instrument(skip_all, fields(key))]
    pub fn lock_shared_path(&self, path: PathBuf) -> CargoResult<LockKey> {
        let key = LockKey(path);
        tracing::Span::current().record("key", key.0.to_str());

        // Fast path: a handle already exists and can be (re-)locked without
        // blocking.
        if let Some(lock) = self.locks.read().get(&key) {
            let lock = Arc::clone(lock);
            if try_acquire_shared(&key.0, lock.file())? {
                return Ok(key);
            }
        }

        // Slow path: open (and, if necessary, block for) the lock without
        // holding the lock table, so a blocking `flock` does not serialize
        // every other lock operation in this process.
        let lock = flock::open_ro_shared_no_msg(&key.0)?;
        self.locks.write().insert(key.clone(), Arc::new(lock));

        Ok(key)
    }

    /// Non-blocking variant of [`LockManager::lock_shared_path`].
    ///
    /// Returns `Ok(None)` if the lock is currently held exclusively by another
    /// process.
    #[instrument(skip_all, fields(key))]
    pub fn try_lock_shared_path(&self, path: PathBuf) -> CargoResult<Option<LockKey>> {
        let key = LockKey(path);
        tracing::Span::current().record("key", key.0.to_str());

        if let Some(lock) = self.locks.read().get(&key) {
            let lock = Arc::clone(lock);
            if try_acquire_shared(&key.0, lock.file())? {
                return Ok(Some(key));
            }
            return Ok(None);
        }
        let fs = Filesystem::new(key.0.clone());
        let Some(lock) = fs.try_open_ro_shared_create(&key.0)? else {
            return Ok(None);
        };
        self.locks.write().insert(key.clone(), Arc::new(lock));
        Ok(Some(key))
    }

    #[instrument(skip(self))]
    pub fn lock(&self, key: &LockKey) -> CargoResult<()> {
        let Some(lock) = self.locks.read().get(key).cloned() else {
            bail!("lock was not found in lock manager: {key}");
        };
        // Block outside the lock table (see struct docs).
        flock::lock_exclusive(lock.file())?;
        Ok(())
    }

    /// Non-blocking variant of [`LockManager::lock`].
    ///
    /// Returns `Ok(true)` if the exclusive lock was acquired (converting the
    /// process's existing shared lock on the same file), `Ok(false)` if the
    /// file is currently locked by another process. A failed attempt leaves
    /// the existing shared lock intact.
    #[instrument(skip(self))]
    pub fn try_lock_exclusive(&self, key: &LockKey) -> CargoResult<bool> {
        let Some(lock) = self.locks.read().get(key).cloned() else {
            bail!("lock was not found in lock manager: {key}");
        };
        crate::util::flock::try_lock_exclusive_simple(&key.0, lock.file())
    }

    /// Upgrades an existing exclusive lock into a shared lock.
    #[instrument(skip(self))]
    pub fn downgrade_to_shared(&self, key: &LockKey) -> CargoResult<()> {
        let Some(lock) = self.locks.read().get(key).cloned() else {
            bail!("lock was not found in lock manager: {key}");
        };
        flock::lock_shared(lock.file())?;
        Ok(())
    }

    #[instrument(skip(self))]
    pub fn unlock(&self, key: &LockKey) -> CargoResult<()> {
        if let Some(lock) = self.locks.read().get(key) {
            flock::unlock(lock.file())?;
        };
        Ok(())
    }
}

/// Attempts to take a shared lock on `f`, ignoring NFS mounts and filesystems
/// that do not implement file locking (matching [`Filesystem`] behavior).
fn try_acquire_shared(path: &std::path::Path, f: &std::fs::File) -> CargoResult<bool> {
    crate::util::flock::try_lock_shared_simple(path, f)
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct LockKey(PathBuf);

impl LockKey {
    fn from_unit(build_runner: &BuildRunner<'_, '_>, unit: &Unit) -> Self {
        Self(build_runner.files().build_unit_lock(unit))
    }
}

impl Display for LockKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}
