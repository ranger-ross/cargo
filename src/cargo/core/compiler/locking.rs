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

    pub fn lock(self) -> Self {
        let unit_lock = self.unit.lock_exclusive();

        let dependency_locks = self
            .dependency_units
            .into_iter()
            .map(|d| d.lock_shared())
            .collect::<Vec<_>>();

        CompilationLock {
            unit: unit_lock,
            dependency_units: dependency_locks,
        }
    }

    pub fn rmeta_produced(&self) {
        // Downgrade the lock on the unit we are building so that we can unblock other units to
        // compile. We do not need to downgrade our dependency locks since they should always be a
        // shared lock.
        self.unit.downgrade();
    }
}

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
    pub fn lock_exclusive(self) -> UnitLock {
        let primary_lock = file_lock(&self.primary);
        primary_lock.lock().unwrap();

        let secondary_lock = file_lock(&self.secondary);
        secondary_lock.lock().unwrap();

        UnitLock {
            primary: self.primary,
            secondary: self.secondary,
            gaurd: Some(UnitLockGuard {
                primary: primary_lock,
                _secondary: Some(secondary_lock),
            }),
        }
    }

    pub fn lock_shared(self) -> UnitLock {
        let primary_lock = file_lock(&self.primary);
        primary_lock.lock_shared().unwrap();

        UnitLock {
            primary: self.primary,
            secondary: self.secondary,
            gaurd: Some(UnitLockGuard {
                primary: primary_lock,
                _secondary: None,
            }),
        }
    }

    pub fn downgrade(&self) {
        let gaurd = self.gaurd.as_ref().unwrap();

        // TODO: Add debug asserts to verify the lock state?

        // We know we have an exclusive lock here so we should never block.
        // This is not super well documented but `lock_shared()` should downgrade the lock.
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
