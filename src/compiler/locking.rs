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
    locks: RwLock<HashMap<LockKey, ManagedLock>>,
}

/// The mode a [`LockManager`] entry was last acquired or transitioned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockMode {
    Shared,
    Exclusive,
}

/// A lock handle tracked by [`LockManager`], mirroring the counting
/// discipline of the package cache locker (`util::cache_lock`).
#[derive(Debug)]
struct ManagedLock {
    file: Arc<FileLock>,
    /// Number of active acquisitions of this key within this process. The
    /// underlying `flock` is taken on the 0-to-1 transition and released on
    /// the 1-to-0 transition.
    ///
    /// Counts are per key, not per acquisition fd: one build unit maps to
    /// exactly one job inside a process (the job queue dedupes units), so a
    /// key has a single in-process owner. [`LockManager`] debug-asserts
    /// acquisitions that would violate this.
    count: u32,
    mode: LockMode,
}

impl ManagedLock {
    fn new(file: Arc<FileLock>) -> Self {
        ManagedLock {
            file,
            count: 0,
            mode: LockMode::Shared,
        }
    }
}

impl LockManager {
    pub fn new() -> Self {
        Self {
            locks: RwLock::new(HashMap::default()),
        }
    }

    /// Bumps the recursion count when `key` is already locked shared in this
    /// process, mirroring the recursive locking of the package cache locker
    /// (`util::cache_lock`). Returns whether the bump happened.
    ///
    /// This only touches the lock table and never calls `flock`, so holding
    /// the table lock while doing it cannot turn a cross-process wait into a
    /// deadlock.
    fn increment_shared(&self, key: &LockKey) -> bool {
        let mut locks = self.locks.write();
        match locks.get_mut(key) {
            Some(entry) if entry.count > 0 && entry.mode == LockMode::Shared => {
                entry.count += 1;
                true
            }
            _ => false,
        }
    }

    /// Returns the tracked handle for `key` without blocking.
    fn handle(&self, key: &LockKey) -> CargoResult<Arc<FileLock>> {
        let locks = self.locks.read();
        match locks.get(key) {
            Some(entry) => Ok(Arc::clone(&entry.file)),
            None => bail!("lock was not found in lock manager: {key}"),
        }
    }

    /// Takes a shared lock on a given [`Unit`]
    /// This prevents other Cargo instances from compiling (writing) to
    /// this build unit.
    ///
    /// Recursive acquisitions are counted: the underlying `flock` is taken
    /// once and released when the last acquisition is unlocked.
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

        // Fast path: already held shared by this process; recursion is a
        // pure count bump.
        if self.increment_shared(&key) {
            return Ok(key);
        }

        // Slow path: ensure a handle exists, opening (and, if necessary,
        // blocking for) the lock without holding the lock table.
        if self.locks.read().get(&key).is_none() {
            let fs = Filesystem::new(key.0.clone());
            let lock_msg = format!(
                "{} ({})",
                unit.pkg.name(),
                build_runner.files().unit_hash(unit)
            );
            let lock = fs.open_ro_shared_create(&key.0, build_runner.bcx.gctx, &lock_msg)?;
            self.locks
                .write()
                .entry(key.clone())
                .or_insert_with(|| ManagedLock::new(Arc::new(lock)));
        }
        let file = self.handle(&key)?;
        flock::lock_shared(file.file())?;
        let mut locks = self.locks.write();
        let entry = locks.get_mut(&key).expect("handle lookup succeeded");
        entry.count += 1;
        entry.mode = LockMode::Shared;
        Ok(key)
    }

    /// Takes a shared lock on an arbitrary lock file (not necessarily tied to
    /// a build unit). Used for the per-build-unit state locks of the
    /// cross-workspace build cache, which are acquired from worker threads
    /// that have no shell access, so no "Blocking" message is printed.
    ///
    /// Recursive acquisitions are counted: the underlying `flock` is taken
    /// once and released when the last acquisition is unlocked.
    ///
    /// This function returns a [`LockKey`] which can be used to
    /// upgrade/unlock the lock.
    #[instrument(skip_all, fields(key))]
    pub fn lock_shared_path(&self, path: PathBuf) -> CargoResult<LockKey> {
        let key = LockKey(path);
        tracing::Span::current().record("key", key.0.to_str());

        // Fast path: already held shared by this process; recursion is a
        // pure count bump.
        if self.increment_shared(&key) {
            return Ok(key);
        }

        // Slow path: ensure a handle exists, opening (and, if necessary,
        // blocking for) the lock without holding the lock table, so a
        // blocking `flock` does not serialize every other lock operation in
        // this process.
        if self.locks.read().get(&key).is_none() {
            let lock = flock::open_ro_shared_no_msg(&key.0)?;
            self.locks
                .write()
                .entry(key.clone())
                .or_insert_with(|| ManagedLock::new(Arc::new(lock)));
        }
        let file = self.handle(&key)?;
        flock::lock_shared(file.file())?;
        let mut locks = self.locks.write();
        let entry = locks.get_mut(&key).expect("handle lookup succeeded");
        entry.count += 1;
        entry.mode = LockMode::Shared;
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

        // Fast path: already held shared by this process; recursion is a
        // pure count bump.
        if self.increment_shared(&key) {
            return Ok(Some(key));
        }

        // Slow path: ensure a handle exists. Creating the lock file and
        // probing it are non-blocking.
        if self.locks.read().get(&key).is_none() {
            let fs = Filesystem::new(key.0.clone());
            let Some(lock) = fs.try_open_ro_shared_create(&key.0)? else {
                return Ok(None);
            };
            self.locks
                .write()
                .entry(key.clone())
                .or_insert_with(|| ManagedLock::new(Arc::new(lock)));
        }
        {
            let locks = self.locks.read();
            let entry = locks.get(&key).expect("handle lookup succeeded");
            debug_assert!(
                !(entry.count > 0 && entry.mode == LockMode::Exclusive),
                "taking shared while exclusively held within this process \
                 would silently downgrade the exclusive lock"
            );
        }
        let file = self.handle(&key)?;
        if !try_acquire_shared(&key.0, file.file())? {
            return Ok(None);
        }
        let mut locks = self.locks.write();
        let entry = locks.get_mut(&key).expect("handle lookup succeeded");
        entry.count += 1;
        entry.mode = LockMode::Shared;
        Ok(Some(key))
    }

    /// Takes an exclusive lock, blocking if another process holds it.
    ///
    /// Callers must not hold the same key shared: a blocking shared-to-
    /// exclusive conversion on one descriptor drops the old lock while
    /// waiting, which allows lock-order cycles between contenders. Use
    /// [`LockManager::exchange_for_exclusive`] to convert held shared locks.
    #[instrument(skip(self))]
    pub fn lock(&self, key: &LockKey) -> CargoResult<()> {
        {
            let locks = self.locks.read();
            match locks.get(key) {
                Some(entry) => {
                    debug_assert!(
                        entry.count == 0 || entry.mode == LockMode::Exclusive,
                        "shared-to-exclusive conversion requires releasing the \
                         shared lock first"
                    );
                }
                None => bail!("lock was not found in lock manager: {key}"),
            }
        }
        let file = self.handle(key)?;
        // Block outside the lock table (see struct docs).
        flock::lock_exclusive(file.file())?;
        let mut locks = self.locks.write();
        let entry = locks.get_mut(key).expect("handle lookup succeeded");
        entry.count += 1;
        entry.mode = LockMode::Exclusive;
        Ok(())
    }

    /// Non-blocking variant of [`LockManager::lock`].
    ///
    /// Returns `Ok(true)` if the exclusive lock was acquired, `Ok(false)` if
    /// the file is currently locked by another process. A failed attempt
    /// leaves any existing lock intact.
    #[instrument(skip(self))]
    pub fn try_lock_exclusive(&self, key: &LockKey) -> CargoResult<bool> {
        {
            let locks = self.locks.read();
            match locks.get(key) {
                Some(entry) => {
                    debug_assert!(
                        entry.count == 0 || entry.mode == LockMode::Exclusive,
                        "shared-to-exclusive conversion requires releasing the \
                         shared lock first"
                    );
                }
                None => bail!("lock was not found in lock manager: {key}"),
            }
        }
        let file = self.handle(key)?;
        if !crate::util::flock::try_lock_exclusive_simple(&key.0, file.file())? {
            return Ok(false);
        }
        let mut locks = self.locks.write();
        let entry = locks.get_mut(key).expect("handle lookup succeeded");
        entry.count += 1;
        entry.mode = LockMode::Exclusive;
        Ok(true)
    }

    /// Downgrades an existing exclusive lock into a shared lock. The
    /// acquisition count is unchanged; the lock stays held (now shared) until
    /// it is unlocked as many times as it was acquired.
    #[instrument(skip(self))]
    pub fn downgrade_to_shared(&self, key: &LockKey) -> CargoResult<()> {
        {
            let locks = self.locks.read();
            match locks.get(key) {
                Some(entry) => {
                    debug_assert!(
                        entry.count > 0 && entry.mode == LockMode::Exclusive,
                        "downgrade requires an exclusively held lock"
                    );
                }
                None => bail!("lock was not found in lock manager: {key}"),
            }
        }
        let file = self.handle(key)?;
        // An exclusive-to-shared conversion on the descriptor we hold cannot
        // block.
        flock::lock_shared(file.file())?;
        let mut locks = self.locks.write();
        let entry = locks.get_mut(key).expect("handle lookup succeeded");
        entry.mode = LockMode::Shared;
        Ok(())
    }

    /// Releases one acquisition of `key`. The underlying `flock` is released
    /// when the last acquisition is unlocked.
    #[instrument(skip(self))]
    pub fn unlock(&self, key: &LockKey) -> CargoResult<()> {
        let mut locks = self.locks.write();
        let Some(entry) = locks.get_mut(key) else {
            debug_assert!(false, "unlock of unknown lock {key}");
            return Ok(());
        };
        debug_assert!(entry.count > 0, "unlock of unheld lock {key}");
        if entry.count == 0 {
            return Ok(());
        }
        entry.count -= 1;
        if entry.count == 0 {
            // `LOCK_UN` does not block.
            flock::unlock(entry.file.file())?;
        }
        Ok(())
    }

    /// Converts held shared locks into an exclusive lock on `primary`,
    /// following the package cache locker's rule that shared-to-exclusive
    /// conversions never happen in place.
    ///
    /// Each key in `keys` (which must include `primary`) has one acquisition
    /// unlocked, then `primary` is probed non-blockingly. Returns `Ok(true)`
    /// when the exclusive lock on `primary` was acquired; the caller must
    /// re-acquire any remaining keys it needs. Returns `Ok(false)` when
    /// another process holds `primary`; nothing is held afterwards, and the
    /// caller should re-evaluate from scratch.
    ///
    /// Releasing before probing is what keeps the multi-lock protocol free
    /// of lock-order cycles: a blocking conversion would drop the old locks
    /// while waiting anyway, but contenders probing at the same time could
    /// then wait on each other in a cycle.
    pub fn exchange_for_exclusive(&self, primary: &LockKey, keys: &[LockKey]) -> CargoResult<bool> {
        debug_assert!(
            keys.iter().any(|k| k == primary),
            "the probed key must be among the released keys"
        );
        for key in keys {
            self.unlock(key)?;
        }
        self.try_lock_exclusive(primary)
    }

    /// Asserts (in debug builds) that `key` is currently held in `mode` by
    /// this process.
    ///
    /// Mirrors the package cache locker's `assert_package_cache_locked`:
    /// low-level code verifies the lock state its correctness depends on
    /// instead of trusting its callers. Compiled out of release builds.
    #[track_caller]
    pub(crate) fn assert_locked(&self, key: &LockKey, mode: LockMode) {
        if !cfg!(debug_assertions) {
            return;
        }
        let locks = self.locks.read();
        let found = locks
            .get(key)
            .map(|e| (e.count, e.mode))
            .unwrap_or((0, mode));
        assert!(
            found.0 > 0 && found.1 == mode,
            "expected lock {key} held {mode:?}, found count {} mode {:?}",
            found.0,
            found.1
        );
    }

    /// Summarizes the locks currently held, for observability.
    ///
    /// Worker threads acquire locks silently (they have no shell access to
    /// print "Blocking" messages), so this post-build summary restores
    /// visibility into what the build ended up holding. Returns
    /// `(path, mode, count)` sorted by path for stable output.
    pub fn active_locks(&self) -> Vec<(String, &'static str, u32)> {
        let locks = self.locks.read();
        let mut summary: Vec<_> = locks
            .iter()
            .filter(|(_, entry)| entry.count > 0)
            .map(|(key, entry)| {
                let mode = match entry.mode {
                    LockMode::Shared => "shared",
                    LockMode::Exclusive => "exclusive",
                };
                (key.0.display().to_string(), mode, entry.count)
            })
            .collect();
        summary.sort_by(|a, b| a.0.cmp(&b.0));
        summary
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

    /// Creates a key for an arbitrary lock file path. Used by the build cache
    /// coordination, which knows its state-lock paths up front.
    pub(crate) fn from_path(path: PathBuf) -> Self {
        Self(path)
    }

    /// The lock file path this key guards.
    pub(crate) fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Display for LockKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::TempDir;

    /// Opens an independent descriptor on `path` for probing the lock state
    /// from the perspective of "another process". `flock` locks are per
    /// open-file description, so a separate `File` sees exactly what another
    /// process would see.
    fn probe(path: &std::path::Path) -> File {
        File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    fn try_exclusive(path: &std::path::Path, f: &File) -> bool {
        crate::util::flock::try_lock_exclusive_simple(path, f).unwrap()
    }

    #[test]
    fn shared_acquisitions_are_counted() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".rmeta.lock");
        let lm = LockManager::new();

        let k1 = lm.lock_shared_path(path.clone()).unwrap();
        let k2 = lm.lock_shared_path(path.clone()).unwrap();
        assert_eq!(k1, k2);
        {
            let locks = lm.locks.read();
            let entry = locks.get(&k1).unwrap();
            assert_eq!(entry.count, 2);
            assert_eq!(entry.mode, LockMode::Shared);
        }

        // Another description cannot take the lock while we hold it.
        let p = probe(&path);
        assert!(
            !try_exclusive(&path, &p),
            "exclusive probe succeeded while shared"
        );

        // One unlock must not release anything; the last one must.
        lm.unlock(&k1).unwrap();
        assert!(!try_exclusive(&path, &p), "released too early");
        lm.unlock(&k2).unwrap();
        assert!(
            try_exclusive(&path, &p),
            "lock still held after last unlock"
        );
    }

    #[test]
    fn unlock_below_zero_is_a_bug() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".rlib.lock");
        let lm = LockManager::new();
        let key = lm.lock_shared_path(path).unwrap();
        lm.unlock(&key).unwrap();
        // The second unlock has no matching acquisition; this is a protocol
        // violation that the debug assertions catch.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lm.unlock(&key).unwrap();
        }))
        .unwrap_err();
    }

    #[test]
    fn exclusive_downgrade_and_release_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".state.lock");
        let lm = LockManager::new();

        // Create the entry via a shared acquisition, release it, then take
        // the lock exclusively (the sequence the cache protocol uses).
        let key = lm.lock_shared_path(path.clone()).unwrap();
        lm.unlock(&key).unwrap();

        assert!(lm.try_lock_exclusive(&key).unwrap());
        {
            let locks = lm.locks.read();
            let entry = locks.get(&key).unwrap();
            assert_eq!(entry.count, 1);
            assert_eq!(entry.mode, LockMode::Exclusive);
        }
        let p = probe(&path);
        assert!(
            !try_exclusive(&path, &p),
            "exclusive probe succeeded while held exclusively"
        );

        // Downgrade keeps the acquisition; readers can now observe shared.
        lm.downgrade_to_shared(&key).unwrap();
        {
            let locks = lm.locks.read();
            let entry = locks.get(&key).unwrap();
            assert_eq!(entry.count, 1);
            assert_eq!(entry.mode, LockMode::Shared);
        }
        let reader = probe(&path);
        assert!(
            crate::util::flock::try_lock_shared_simple(&path, &reader).unwrap(),
            "shared probe failed after downgrade"
        );

        // The single remaining acquisition releases for everyone. The probe
        // descriptors must be gone first: the reader still holds shared.
        drop(reader);
        lm.unlock(&key).unwrap();
        assert!(try_exclusive(&path, &p), "lock still held after unlock");
    }

    #[test]
    fn failed_try_leaves_the_existing_lock_intact() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".contended.lock");
        let lm = LockManager::new();

        // Create the entry, then release it so the manager tracks a handle
        // without holding the lock (the state `try_lock_exclusive` sees when
        // the cache protocol probes after releasing its shared locks).
        let key = lm.lock_shared_path(path.clone()).unwrap();
        lm.unlock(&key).unwrap();

        // Simulate another process holding the file exclusively. A separate
        // open-file description conflicts with the manager's descriptor even
        // within one process, and only non-blocking calls are used here: a
        // blocking exclusive acquisition would deadlock against our own
        // recorded handle.
        let other = probe(&path);
        assert!(
            crate::util::flock::try_lock_exclusive_simple(&path, &other).unwrap(),
            "contender could not take the released lock"
        );

        // The attempt fails and must not record an acquisition.
        assert!(!lm.try_lock_exclusive(&key).unwrap());
        {
            let locks = lm.locks.read();
            let entry = locks.get(&key).unwrap();
            assert_eq!(entry.count, 0);
        }

        drop(other);
        // Once the contender is gone the same key is acquirable.
        assert!(lm.try_lock_exclusive(&key).unwrap());
        lm.unlock(&key).unwrap();
    }

    #[test]
    fn exchange_acquires_exclusive_and_releases_the_rest() {
        let tmp = TempDir::new().unwrap();
        let rmeta = tmp.path().join(".rmeta.lock");
        let rlib = tmp.path().join(".rlib.lock");
        let lm = LockManager::new();

        let kr = lm.lock_shared_path(rmeta.clone()).unwrap();
        let kl = lm.lock_shared_path(rlib.clone()).unwrap();

        assert!(
            lm.exchange_for_exclusive(&kr, &[kr.clone(), kl.clone()])
                .unwrap()
        );
        {
            let locks = lm.locks.read();
            let entry = locks.get(&kr).unwrap();
            assert_eq!(entry.count, 1);
            assert_eq!(entry.mode, LockMode::Exclusive);
            let entry = locks.get(&kl).unwrap();
            assert_eq!(entry.count, 0, "the non-primary key must stay released");
        }

        // The released key is free for another description; the probed one is
        // not.
        let p_rlib = probe(&rlib);
        assert!(try_exclusive(&rlib, &p_rlib));
        let p_rmeta = probe(&rmeta);
        assert!(!try_exclusive(&rmeta, &p_rmeta));
    }

    #[test]
    fn exchange_releases_every_acquisition() {
        let tmp = TempDir::new().unwrap();
        let rmeta = tmp.path().join(".rmeta.lock");
        let rlib = tmp.path().join(".rlib.lock");
        let lm = LockManager::new();

        // The wait loop holds each key exactly once when it reaches the
        // exchange (re-acquisitions happen only after a full release).
        let kr = lm.lock_shared_path(rmeta.clone()).unwrap();
        let kl = lm.lock_shared_path(rlib.clone()).unwrap();
        {
            let locks = lm.locks.read();
            assert_eq!(locks.get(&kr).unwrap().count, 1);
            assert_eq!(locks.get(&kl).unwrap().count, 1);
        }

        assert!(lm.exchange_for_exclusive(&kr, &[kr.clone(), kl]).unwrap());
        {
            let locks = lm.locks.read();
            let entry = locks.get(&kr).unwrap();
            assert_eq!(entry.count, 1);
            assert_eq!(entry.mode, LockMode::Exclusive);
        }

        // The fully released key is free for another description.
        let p_rlib = probe(&rlib);
        assert!(try_exclusive(&rlib, &p_rlib));
    }
    #[test]
    fn concurrent_acquisitions_balance_out() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".race.lock");
        let lm = std::sync::Arc::new(LockManager::new());

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let lm = std::sync::Arc::clone(&lm);
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let key = lm.lock_shared_path(path.clone()).unwrap();
                        lm.unlock(&key).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        {
            let locks = lm.locks.read();
            let entry = locks.values().next().expect("entry exists");
            assert_eq!(entry.count, 0);
        }
        let p = probe(&path);
        assert!(try_exclusive(&path, &p));
    }

    #[test]
    fn assert_locked_accepts_the_held_mode() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".assert.lock");
        let lm = LockManager::new();
        let key = lm.lock_shared_path(path).unwrap();
        // No panic when the recorded state matches.
        lm.assert_locked(&key, LockMode::Shared);
        lm.unlock(&key).unwrap();
    }

    #[test]
    #[should_panic(expected = "expected lock")]
    fn assert_locked_rejects_a_wrong_mode() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".assert-wrong.lock");
        let lm = LockManager::new();
        let key = lm.lock_shared_path(path).unwrap();
        lm.assert_locked(&key, LockMode::Exclusive);
    }

    #[test]
    #[should_panic(expected = "expected lock")]
    fn assert_locked_rejects_an_unknown_key() {
        let tmp = TempDir::new().unwrap();
        let key = LockKey::from_path(tmp.path().join(".never.lock"));
        LockManager::new().assert_locked(&key, LockMode::Shared);
    }

    #[test]
    fn active_locks_summarizes_held_entries_only() {
        let tmp = TempDir::new().unwrap();
        let rmeta = tmp.path().join(".rmeta.lock");
        let rlib = tmp.path().join(".rlib.lock");
        let lm = LockManager::new();

        let kr = lm.lock_shared_path(rmeta.clone()).unwrap();
        lm.lock_shared_path(rmeta.clone()).unwrap();
        // The rlib entry was created and released again; released entries do
        // not appear in the summary.
        let kl = lm.lock_shared_path(rlib.clone()).unwrap();
        lm.unlock(&kl).unwrap();

        // Only the held key appears, with its mode and acquisition count.
        assert_eq!(
            lm.active_locks(),
            vec![(rmeta.display().to_string(), "shared", 2)]
        );
        lm.unlock(&kr).unwrap();
        lm.unlock(&kr).unwrap();
        assert!(lm.active_locks().is_empty());
    }
}
