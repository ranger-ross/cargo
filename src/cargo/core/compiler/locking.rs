//! This module handles the locking logic during compilation.
//!
//! The locking scheme is based on build unit level locking.
//! Each build unit consists of a partial and full lock used to represent multiple lock states.
//!
//! | State                  | `partial.lock` | `full.lock`  |
//! |------------------------|----------------|--------------|
//! | Unlocked               | `unlocked`     | `unlocked`   |
//! | Building Exclusive     | `exclusive`    | `exclusive`  |
//! | Building Non-Exclusive | `shared`       | `exclusive`  |
//! | Shared Partial         | `shared`       | `unlocked`   |
//! | Shared Full            | `shared`       | `shared`     |
//!
//! Generally a build unit will full the following flow:
//! 1. Acquire a "building exclusive" lock for the current build unit.
//! 2. Acquire "shared" locks on all dependency build units.
//! 3. Begin building with rustc
//! 4. If we are building a library, downgrade to a "building non-exclusive" lock when the `.rmeta` has been generated.
//! 5. Once complete release all locks.
//!
//! Most build units only require metadata (.rmeta) from dependencies, so they can begin building
//! once the dependency units have produced the .rmeta. These units take a "shared partial" lock
//! which can be taken while the dependency still holds the "build non-exclusive" lock.
//!
//! Note that some build unit types like bin and proc-macros require the full dependency build
//! (.rlib). For these unit types they must take a "shared full" lock on dependency units which will
//! block until the dependency unit is fully built.
//!
//! The primary reason for the complexity here it to enable fine grain locking while also allowing pipelined builds.
//!
//! [`LockManager`] is the primary interface for locking.

use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::Context;
use itertools::Itertools;
use tracing::{instrument, trace};

use crate::{
    CargoResult,
    core::compiler::{BuildRunner, Unit, layout::BuildUnitLockLocation},
};

/// The locking mode that will be used for build-dir.
#[derive(Debug)]
pub enum LockingMode {
    /// Fine grain locking (Build unit level)
    Fine,
    /// Coarse grain locking (Profile level)
    Coarse,
}

/// The type of lock to take when taking a shared lock.
/// See the module documentation for more information about shared lock types.
#[derive(Debug, Clone, Copy)]
pub enum SharedLockType {
    /// A shared lock that might still be compiling a .rlib
    Partial,
    /// A shared lock that is guaranteed to not be compiling
    Full,
}

pub struct LockManager {
    locks: Mutex<HashMap<LockKey, UnitLock>>,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct LockKey(String);

impl LockManager {
    pub fn new() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn lock_fingerprint(
        &self,
        build_runner: &BuildRunner<'_, '_>,
        unit: &Unit,
    ) -> CargoResult<LockKey> {
        let mut locks = self.locks.lock().unwrap();

        let location = build_runner.files().build_unit_lock(unit);
        let key = location_to_key(&location);

        if let Some(lock) = locks.get(&key) {
            assert_eq!(
                lock.guard.as_ref().unwrap().state,
                UnitLockState::ReadFingerprint
            );
            // We already locked this unit
            return Ok(key);
        }

        let mut unit_lock = UnitLock::new(location);
        unit_lock.read_fingerprint()?;
        locks.insert(key.clone(), unit_lock);

        Ok(key)
    }

    pub fn start_compiling(&self, lock: &CompilationLockRef) -> CargoResult<LockKey> {
        let mut locks = self.locks.lock().unwrap();

        let (key, location) = &lock.unit;

        if let Some(lock) = locks.get_mut(&key) {
            assert_eq!(
                lock.guard.as_ref().unwrap().state,
                UnitLockState::ReadFingerprint
            );
            lock.start_compile()?;
        } else {
            panic!("Should have read fingerprint before starting compilation");
        }

        let mut dependency_units = lock
            .dependency_units
            .iter()
            .filter_map(|(key, location)| {
                if let Some(lock) = locks.get_mut(&key) {
                    // TODO: Unwrap
                    lock.read_as_dependency().unwrap();
                    return None;
                }

                Some((key.clone(), UnitLock::new(location.clone())))
            })
            .collect_vec();

        for (_, lock) in dependency_units.iter_mut() {
            lock.read_as_dependency()?;
        }

        locks.extend(dependency_units);

        Ok(key.clone())
    }

    pub fn rmeta_produced(&self, key: &LockKey) -> CargoResult<()> {
        if let Some(lock) = self.locks.lock().unwrap().get_mut(&key) {
            lock.rmeta_produced()?;
        } else {
            panic!("missing lock when rmeta produced");
        }

        Ok(())
    }

    pub fn unlock(&self, lock: &CompilationLockRef) -> CargoResult<()> {
        let (key, location) = &lock.unit;
        println!("Unlocking {}", location.full.parent().unwrap().display());
        if let Some(lock) = self.locks.lock().unwrap().remove(&key) {
            drop(lock); // TODO: clean up
        }
        Ok(())
    }

    pub fn read_as_dependency(&self, lock: &CompilationLockRef) -> CargoResult<()> {
        let (key, location) = &lock.unit;
        if let Some(lock) = self.locks.lock().unwrap().get_mut(&key) {
            lock.read_as_dependency()?;
        }
        Ok(())
    }
}

fn location_to_key(location: &BuildUnitLockLocation) -> LockKey {
    LockKey(location.partial.parent().unwrap().display().to_string())
}

#[derive(Clone)]
pub struct CompilationLockRef {
    /// The path to the lock file of the unit to compile
    unit: (LockKey, BuildUnitLockLocation),
    /// The paths to lock files of the unit's dependencies
    dependency_units: Vec<(LockKey, BuildUnitLockLocation)>,
}

impl CompilationLockRef {
    pub fn new(build_runner: &BuildRunner<'_, '_>, unit: &Unit) -> Self {
        let dependency_units = all_dependency_units(build_runner, unit)
            .into_iter()
            .map(|u| build_runner.files().build_unit_lock(u))
            .map(|location| (location_to_key(&location), location))
            .collect_vec();
        let location = build_runner.files().build_unit_lock(unit);
        Self {
            unit: (location_to_key(&location), location),
            dependency_units,
        }
    }
}

/// A lock for a single build unit.
struct UnitLock {
    active_build: PathBuf,
    share: PathBuf,
    guard: Option<UnitLockGuard>,
}

struct UnitLockGuard {
    active_build: Option<File>,
    share: Option<File>,
    state: UnitLockState,
}

impl UnitLock {
    pub fn new(location: BuildUnitLockLocation) -> Self {
        Self {
            active_build: location.partial,
            share: location.full,
            guard: None,
        }
    }

    #[instrument(skip(self))]
    pub fn read_fingerprint(&mut self) -> CargoResult<()> {
        assert!(self.guard.is_none());

        let active_build = open_file(&self.active_build)?;
        active_build.lock()?;

        self.guard = Some(UnitLockGuard {
            active_build: Some(active_build),
            share: None,
            state: UnitLockState::ReadFingerprint,
        });

        Ok(())
    }

    #[instrument(skip(self))]
    pub fn start_compile(&mut self) -> CargoResult<()> {
        assert!(self.guard.is_some());
        assert_eq!(
            self.guard.as_ref().unwrap().state,
            UnitLockState::ReadFingerprint
        );

        let share = open_file(&self.share)?;
        share.lock()?;

        self.guard.as_mut().unwrap().share = Some(share);
        self.guard.as_mut().unwrap().state = UnitLockState::CompilingRMeta;

        Ok(())
    }

    #[instrument(skip(self))]
    pub fn rmeta_produced(&mut self) -> CargoResult<()> {
        assert!(self.guard.is_some());
        assert_eq!(
            self.guard.as_ref().unwrap().state,
            UnitLockState::CompilingRMeta
        );

        self.guard
            .as_mut()
            .unwrap()
            .active_build
            .as_mut()
            .unwrap()
            .lock_shared()?;
        self.guard.as_mut().unwrap().state = UnitLockState::CompilingRlib;

        Ok(())
    }

    #[instrument(skip(self))]
    pub fn read_as_dependency(&mut self) -> CargoResult<()> {
        if let Some(guard) = self.guard.as_mut() {
            match guard.state {
                UnitLockState::ReadFingerprint => {
                    let share = open_file(&self.share)?;
                    share.lock_shared()?;

                    guard.share = Some(share);
                    guard.active_build.as_mut().unwrap().lock_shared()?;
                    guard.state = UnitLockState::ReadAsDependency;
                }
                UnitLockState::CompilingRMeta => {
                    guard.share.as_mut().unwrap().lock_shared()?;
                    guard.active_build.as_mut().unwrap().lock_shared()?;
                    guard.state = UnitLockState::ReadAsDependency;
                }
                UnitLockState::CompilingRlib => {
                    guard.share.as_mut().unwrap().lock_shared()?;
                    guard.state = UnitLockState::ReadAsDependency;
                }
                UnitLockState::ReadAsDependency => return Ok(()),
            }
        } else {
            let active_build = open_file(&self.active_build)?;
            active_build.lock_shared()?;

            let share = open_file(&self.share)?;
            share.lock_shared()?;

            drop(active_build);

            self.guard = Some(UnitLockGuard {
                active_build: None,
                share: Some(share),
                state: UnitLockState::ReadAsDependency,
            });
        }

        Ok(())
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum UnitLockState {
    ReadFingerprint,
    CompilingRMeta,
    CompilingRlib,
    ReadAsDependency,
}

fn open_file<T: AsRef<Path>>(f: T) -> CargoResult<File> {
    Ok(OpenOptions::new()
        .read(true)
        .create(true)
        .write(true)
        .append(true)
        .open(f)?)
}

fn all_dependency_units<'a>(
    build_runner: &'a BuildRunner<'a, '_>,
    unit: &Unit,
) -> HashSet<&'a Unit> {
    fn inner<'a>(
        build_runner: &'a BuildRunner<'a, '_>,
        unit: &Unit,
        results: &mut HashSet<&'a Unit>,
    ) {
        for dep in build_runner.unit_deps(unit) {
            if results.insert(&dep.unit) {
                inner(&build_runner, &dep.unit, results);
            }
        }
    }

    let mut results = HashSet::new();
    inner(build_runner, unit, &mut results);
    return results;
}
