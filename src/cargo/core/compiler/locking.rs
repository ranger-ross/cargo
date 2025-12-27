//! This module handles the locking logic during compilation.

use crate::{
    CargoResult,
    core::compiler::{BuildRunner, Unit},
    util::{FileLock, Filesystem},
};
use anyhow::bail;
use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
    path::PathBuf,
    sync::Mutex,
};

/// A struct to store the lock handles for build units during compilation.
pub struct LockManager {
    locks: Mutex<HashMap<LockKey, FileLock>>,
}

impl LockManager {
    pub fn new() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Takes a shared lock on a given [`Unit`]
    /// This prevents other Cargo instances from compiling (writing) to
    /// this build unit.
    ///
    /// This function returns a [`LockKey`] which can be used to
    /// upgrade/unlock the lock.
    pub fn lock_shared(
        &self,
        build_runner: &BuildRunner<'_, '_>,
        unit: &Unit,
    ) -> CargoResult<LockKey> {
        let key = LockKey::from_unit(build_runner, unit);
        let mut locks = self.locks.lock().unwrap();
        if let Some(lock) = locks.get_mut(&key) {
            lock.file().lock_shared()?;
        } else {
            let fs = Filesystem::new(key.0.clone());
            let lock =
                fs.open_ro_shared_create(&key.0, build_runner.bcx.gctx, &format!("locking {key}"))?;
            locks.insert(key.clone(), lock);
        }

        Ok(key)
    }

    /// Upgrades an existing shared lock into an exclusive lock.
    pub fn upgrade_to_exclusive(&self, key: &LockKey) -> CargoResult<()> {
        let mut locks = self.locks.lock().unwrap();
        let Some(lock) = locks.get_mut(key) else {
            bail!("lock was not found in lock manager: {key}");
        };
        lock.file().lock()?;
        Ok(())
    }

    /// Upgrades an existing exclusive lock into a shared lock.
    pub fn downgrade_to_shared(&self, key: &LockKey) -> CargoResult<()> {
        let mut locks = self.locks.lock().unwrap();
        let Some(lock) = locks.get_mut(key) else {
            bail!("lock was not found in lock manager: {key}");
        };
        lock.file().lock_shared()?;
        Ok(())
    }
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
