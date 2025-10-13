//! This module handles the locking logic during compilation.
//!
//! The locking scheme is based on build unit level locking.
//! Each build unit consists of a primary and secondary lock used to represent multiple lock states.
//!
//! | State                  | Primary     | Secondary   |
//! |------------------------|-------------|-------------|
//! | Building Exclusive     | `exclusive` | `exclusive` |
//! | Building Non-Exclusive | `shared`    | `exclusive` |
//! | Shared                 | `shared`    | `none`      |
//!
//! Generally a build unit will full the following flow:
//! 1. Acquire a "building exclusive" lock for the current build unit.
//! 2. Acquire "shared" locks on all dependency build units.
//! 3. Begin building with rustc
//! 4. If we are building a library, downgrade to a "building non-exclusive" lock when the `.rmeta` has been generated.
//! 5. Once complete release all locks.
//!
//! The primary reason for the complexity here it to allow dependant crates to proceed with thier
//! compilation as possible.
//!
//! [`CompilationLock`] is the primary interface for locking.

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

use itertools::Itertools;

use crate::core::compiler::{BuildRunner, Unit};

pub struct CompilationLock {
    /// The path to the lock file of the unit to compile
    unit: UnitLock,
    /// The paths to lock files of the unit's dependencies
    dependency_units: Vec<UnitLock>,
}

impl CompilationLock {
    pub fn new(build_runner: &BuildRunner<'_, '_>, unit: &Unit) -> Self {
        let unit_lock = build_runner.files().build_unit_lock(unit).into();

        let dependency_units = build_runner
            .unit_deps(unit)
            .into_iter()
            .map(|unit| build_runner.files().build_unit_lock(&unit.unit).into())
            .collect_vec();

        Self {
            unit: unit_lock,
            dependency_units,
        }
    }

    pub fn lock(&mut self) {
        self.unit.lock_exclusive();

        self.dependency_units
            .iter_mut()
            .for_each(|d| d.lock_shared());
    }

    pub fn rmeta_produced(&mut self) {
        // Downgrade the lock on the unit we are building so that we can unblock other units to
        // compile. We do not need to downgrade our dependency locks since they should always be a
        // shared lock.
        self.unit.downgrade();
    }
}

/// A lock for a single build unit.
struct UnitLock {
    primary: PathBuf,
    secondary: PathBuf,
    gaurd: Option<UnitLockGuard>,
}

struct UnitLockGuard {
    primary: File,
    _secondary: Option<File>,
}

impl UnitLock {
    pub fn lock_exclusive(&mut self) {
        assert!(self.gaurd.is_none());

        let primary_lock = file_lock(&self.primary);
        primary_lock.lock().unwrap();

        let secondary_lock = file_lock(&self.secondary);
        secondary_lock.lock().unwrap();

        self.gaurd = Some(UnitLockGuard {
            primary: primary_lock,
            _secondary: Some(secondary_lock),
        });
    }

    pub fn lock_shared(&mut self) {
        assert!(self.gaurd.is_none());

        let primary_lock = file_lock(&self.primary);
        primary_lock.lock_shared().unwrap();

        self.gaurd = Some(UnitLockGuard {
            primary: primary_lock,
            _secondary: None,
        });
    }

    pub fn downgrade(&mut self) {
        let gaurd = self.gaurd.as_ref().unwrap();

        // TODO: Add debug asserts to verify the lock state?

        // NOTE:
        // > Subsequent flock() calls on an already locked file will convert an existing lock to the new lock mode.
        // https://man7.org/linux/man-pages/man2/flock.2.html
        //
        // However, the `std::file::File::lock/lock_shared` is allowed to change this in the
        // future. So its probably up to us if we are okay with using this or if we want to use a
        // different interace to flock.
        //
        // TODO: Need to validate on other platforms...
        gaurd.primary.lock_shared().unwrap();
    }
}

fn file_lock<T: AsRef<Path>>(f: T) -> File {
    OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(f)
        .unwrap()
}

impl From<(PathBuf, PathBuf)> for UnitLock {
    fn from(value: (PathBuf, PathBuf)) -> Self {
        Self {
            primary: value.0,
            secondary: value.1,
            gaurd: None,
        }
    }
}
