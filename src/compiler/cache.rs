//! Coordination for the cross-workspace build cache.
//!
//! Cacheable units (see [`Unit::is_cacheable`]) are built directly in
//! `$CARGO_HOME/build-cache` and treated as immutable once complete. Multiple
//! Cargo processes can build and read the same unit at the same time, so each
//! cache directory uses two state locks:
//!
//! * `.rmeta.lock`: held exclusively while the unit compiles and the `.rmeta`
//!   has not been produced. It is downgraded to shared once rustc reports the
//!   `.rmeta` (rustc finishes writing it before sending the notification).
//!   A shared holder can then read the `.rmeta` safely while the `.rlib`
//!   or other outputs are still being produced.
//! * `.rlib.lock`: held exclusively while the unit compiles and downgraded
//!   to shared only after all outputs are written and the fingerprint is
//!   persisted. A shared holder knows the fingerprint exists, which means the
//!   cache entry is complete.
//!
//! Locks are always taken in the same order (`.rmeta.lock` then `.rlib.lock`).
//! Contenders release shared locks before trying for exclusive, which avoids
//! lock-order cycles.
//!
//! ## Roles
//!
//! A job for a cacheable unit that is dirty at fingerprint-check time runs
//! [`CacheCoordination::coordinate`] before compiling. It becomes either the
//! **builder** (it compiles the unit) or a **waiter** (it watches the
//! builder):
//!
//! * **Builder**: holds both locks exclusively (after double-checking the
//!   fingerprint, since another process may have finished the unit while it
//!   waited), compiles, downgrades `.rmeta.lock` to shared as soon as rustc
//!   reports the `.rmeta` so other processes can pipeline against it, then
//!   downgrades `.rlib.lock` to shared after the fingerprint is written.
//! * **Waiter**: takes `.rmeta.lock` shared (blocks until the rmeta is ready
//!   or the builder is gone). If the unit is complete it returns. If the
//!   builder is still active and no fingerprint exists yet (cold cache), it
//!   signals `rmeta_produced` early so its own dependents can start
//!   type-checking against the metadata while the builder finishes codegen.
//!   A stale fingerprint (rebuild in place) means the current `.rmeta` is
//!   about to be replaced, so it waits without signaling. Either way it then
//!   blocks on `.rlib.lock` shared. If the builder crashed (no fingerprint
//!   appeared), it takes over the build.
//!
//! ## Crash recovery
//!
//! If a builder dies mid-compile it releases its locks and leaves a partial
//! unit with no fingerprint. The next process to run the protocol sees the
//! missing fingerprint and takes over. All shared locks are released before
//! trying for exclusive, and the fingerprint is checked again after
//! acquisition, so concurrent contenders resolve to a single builder without
//! deadlock.
//!
//! ## Known limitation
//!
//! If a builder crashes after producing the `.rmeta` but before finishing,
//! a waiter may have already signaled `rmeta_produced` and started dependents
//! against that `.rmeta`. When the waiter takes over, rustc rewrites the
//! `.rmeta` while dependents may still be reading it, which can cause a torn
//! read in theory. The rewrite is byte-identical in practice (same unit, same
//! inputs, same rustc) and needs a crash at a precise moment, so this is
//! accepted for now.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::compiler::job_queue::{JobState, Work};
use crate::compiler::locking::{LockKey, LockMode};
use crate::compiler::{BuildRunner, Unit};
use crate::util::CargoResult;

/// Lock keys and role flags for one coordinated cacheable unit.
///
/// Keys are derived from the unit's cache directory and built once upfront,
/// instead of being looked up after acquisition. Waiters hold both keys
/// shared; the builder holds them exclusively after the exchange and
/// downgrades them in [`CacheCoordination::downgrade_rmeta`] and
/// [`CacheCoordination::after_work`].
struct UnitLocks {
    /// `.rmeta.lock`: held exclusively while compiling, downgraded to shared
    /// as soon as rustc reports the `.rmeta` artifact.
    rmeta: LockKey,
    /// `.rlib.lock`: held exclusively while compiling, downgraded to shared
    /// only after the fingerprint is persisted.
    rlib: LockKey,
    /// Whether the wait phases currently hold each key shared.
    held_rmeta: AtomicBool,
    held_rlib: AtomicBool,
    /// Whether this process is (or became) the builder of the unit.
    builder: AtomicBool,
    /// Whether the rmeta lock has already been downgraded to shared.
    rmeta_shared: AtomicBool,
}

impl UnitLocks {
    fn new(rmeta: PathBuf, rlib: PathBuf) -> Self {
        UnitLocks {
            rmeta: LockKey::from_path(rmeta),
            rlib: LockKey::from_path(rlib),
            held_rmeta: AtomicBool::new(false),
            held_rlib: AtomicBool::new(false),
            builder: AtomicBool::new(false),
            rmeta_shared: AtomicBool::new(false),
        }
    }

    /// Releases every shared acquisition held by the wait phases.
    fn release_shared(&self, state: &JobState<'_, '_>) -> CargoResult<()> {
        if self.held_rlib.swap(false, Ordering::SeqCst) {
            state.unlock(&self.rlib)?;
        }
        if self.held_rmeta.swap(false, Ordering::SeqCst) {
            state.unlock(&self.rmeta)?;
        }
        Ok(())
    }
}

/// Next step in the coordination state machine.
enum Attempt {
    /// Acquire `.rmeta.lock` shared. Blocks until the rmeta is readable or
    /// the builder releases its locks.
    WaitForRmeta,
    /// Probe `.rlib.lock` without blocking to see if a builder is active.
    ProbeBuilder,
    /// Turn held shared locks into exclusive ownership (take over the build),
    /// checking again whether another process already finished it.
    TakeOver,
    /// Done. `true` means this process should compile, `false` means skip
    /// because the unit is already complete.
    Done(bool),
}

/// Per-job coordination state for a single cacheable unit.
pub struct CacheCoordination {
    locks: UnitLocks,
    fingerprint: PathBuf,
    /// Normalized fingerprint and filesystem state that decides whether a
    /// cache entry counts as complete (see
    /// [`crate::compiler::fingerprint::CacheCompletionState`]).
    completion: crate::compiler::fingerprint::CacheCompletionState,
}

impl CacheCoordination {
    /// Creates the coordination state for a cacheable `unit`.
    pub fn new(
        build_runner: &mut BuildRunner<'_, '_>,
        unit: &Unit,
        completion: crate::compiler::fingerprint::CacheCompletionState,
    ) -> CargoResult<Arc<Self>> {
        Ok(Arc::new(CacheCoordination {
            locks: UnitLocks::new(
                build_runner.files().cache_rmeta_lock(unit),
                build_runner.files().cache_rlib_lock(unit),
            ),
            fingerprint: build_runner.files().fingerprint_file_path(unit, ""),
            completion,
        }))
    }

    /// Checks whether the cached unit is complete for this identity and
    /// filesystem state.
    ///
    /// `builder_active` is true when another holder was observed on the
    /// unit's locks shortly before this check. Contention is treated as
    /// evidence that another process is resolving the unit's state — it may
    /// be a compiling builder or another waiter that observed one — so a
    /// content match alone is enough to skip, because the holder will
    /// resolve any remaining dirtiness. Without contention the unit is only
    /// complete when the fingerprint matches and the filesystem was up to
    /// date at prepare time (otherwise missing outputs on a cold or partial
    /// cache still need a build).
    pub(crate) fn is_complete(&self, builder_active: bool) -> CargoResult<bool> {
        let content_matches = match cargo_util::paths::read(&self.fingerprint) {
            Ok(stored) => stored == self.completion.expected_hash()?,
            Err(_) => false,
        };
        Ok(content_matches && (self.completion.fs_up_to_date || builder_active))
    }

    /// Checks whether any fingerprint file exists.
    ///
    /// Missing means the unit has never been built (cold cache). Present but
    /// mismatched means the unit is being rebuilt in place with different
    /// content (for example a new git revision). In the latter case pipelining
    /// against the stale rmeta would be wrong.
    fn fingerprint_exists(&self) -> bool {
        self.fingerprint.exists()
    }

    /// Runs the coordination protocol for one cacheable unit.
    ///
    /// Returns `true` if this process should compile the unit (builder role)
    /// and `false` if the unit is already complete.
    pub(crate) fn coordinate(&self, state: &JobState<'_, '_>) -> CargoResult<bool> {
        let mut attempt = Attempt::WaitForRmeta;
        loop {
            attempt = match attempt {
                Attempt::WaitForRmeta => self.wait_for_rmeta(state)?,
                Attempt::ProbeBuilder => self.probe_builder(state)?,
                Attempt::TakeOver => self.take_over(state)?,
                Attempt::Done(build) => return Ok(build),
            };
        }
    }

    /// Takes `.rmeta.lock` shared. Blocks until the rmeta is readable,
    /// the unit is complete, or the builder crashes and releases its locks.
    fn wait_for_rmeta(&self, state: &JobState<'_, '_>) -> CargoResult<Attempt> {
        state.lock_shared_path(self.locks.rmeta.path().clone())?;
        self.locks.held_rmeta.store(true, Ordering::SeqCst);

        // Matching fingerprint with up-to-date filesystem state means the unit
        // is complete and immutable, so there is nothing to do.
        if self.is_complete(false)? {
            self.locks.release_shared(state)?;
            return Ok(Attempt::Done(false));
        }
        Ok(Attempt::ProbeBuilder)
    }

    /// Checks for an active builder by probing `.rlib.lock` without blocking,
    /// then either waits for it or prepares to take over a crashed build.
    fn probe_builder(&self, state: &JobState<'_, '_>) -> CargoResult<Attempt> {
        match state.try_lock_shared_path(self.locks.rlib.path().clone())? {
            // No builder is active. The unit was either never built,
            // already complete (finished while we waited for the rmeta lock),
            // or a previous builder crashed. The first and last cases take over below.
            Some(_) => {
                self.locks.held_rlib.store(true, Ordering::SeqCst);
                if self.is_complete(false)? {
                    self.locks.release_shared(state)?;
                    return Ok(Attempt::Done(false));
                }
                Ok(Attempt::TakeOver)
            }
            // A builder is active. If the unit was never built, our
            // `.rmeta.lock` acquisition proves the `.rmeta` is ready, so we
            // signal dependents to start pipelining against it while the
            // builder finishes codegen. If a stale fingerprint still exists
            // (rebuild in place), pipelining against the about-to-be-replaced
            // rmeta would be wrong, so we wait for completion.
            None => {
                // Early pipelining needs proof that the rmeta is readable,
                // which means holding `.rmeta.lock` shared.
                state.assert_locked(&self.locks.rmeta, LockMode::Shared);
                if !self.fingerprint_exists() {
                    state.rmeta_produced();
                }
                // Wait for the builder to finish (or crash and release).
                // A builder was observed, so a matching fingerprint is
                // sufficient: it resolved the dirtiness.
                state.lock_shared_path(self.locks.rlib.path().clone())?;
                self.locks.held_rlib.store(true, Ordering::SeqCst);
                if self.is_complete(true)? {
                    self.locks.release_shared(state)?;
                    return Ok(Attempt::Done(false));
                }
                // The builder crashed after producing the rmeta; we must
                // complete the unit ourselves.
                Ok(Attempt::TakeOver)
            }
        }
    }

    /// Turns held shared locks into exclusive ownership of the unit.
    ///
    /// Releases shared locks first so we hold nothing while probing, then
    /// probes the rmeta lock. If another process holds it, that process
    /// will resolve the unit's state. Waiting for it on the shared rlib lock
    /// (still holding nothing) is bounded by its job and cannot deadlock,
    /// even if it is waiting on a unit whose locks we hold.
    fn take_over(&self, state: &JobState<'_, '_>) -> CargoResult<Attempt> {
        debug_assert!(
            self.locks.held_rmeta.load(Ordering::SeqCst)
                && self.locks.held_rlib.load(Ordering::SeqCst),
            "takeover requires both locks held shared"
        );
        self.locks.held_rmeta.store(false, Ordering::SeqCst);
        self.locks.held_rlib.store(false, Ordering::SeqCst);
        let contended = !state.exchange_for_exclusive(
            &self.locks.rmeta,
            &[self.locks.rmeta.clone(), self.locks.rlib.clone()],
        )?;
        if contended {
            // Someone else holds this unit's locks. They are either building
            // it or waiting on it; either way their job will resolve the
            // state. Wait for that job, then re-evaluate from scratch rather
            // than blocking on the exclusive lock.
            state.lock_shared_path(self.locks.rlib.path().clone())?;
            if self.is_complete(true)? {
                state.unlock(&self.locks.rlib)?;
                return Ok(Attempt::Done(false));
            }
            state.unlock(&self.locks.rlib)?;
            return Ok(Attempt::WaitForRmeta);
        }
        // `exchange_for_exclusive` already took `.rmeta.lock` exclusively;
        // only the rlib lock still needs acquiring.
        state.lock_exclusive(&self.locks.rlib)?;
        if self.is_complete(false)? {
            state.unlock(&self.locks.rlib)?;
            state.unlock(&self.locks.rmeta)?;
            return Ok(Attempt::Done(false));
        }
        self.locks.builder.store(true, Ordering::SeqCst);
        Ok(Attempt::Done(true))
    }

    pub(crate) fn downgrade_rmeta(&self, state: &JobState<'_, '_>) -> CargoResult<()> {
        if !self.locks.rmeta_shared.swap(true, Ordering::SeqCst) {
            // Only a builder in possession of the exclusive lock may
            // downgrade it.
            debug_assert!(
                self.locks.builder.load(Ordering::SeqCst),
                "rmeta downgrade outside the builder role"
            );
            state.assert_locked(&self.locks.rmeta, LockMode::Exclusive);
            state.downgrade_to_shared(&self.locks.rmeta)?;
        }
        Ok(())
    }

    /// Work to run after the unit's fingerprint has been written.
    ///
    /// Downgrades the rlib lock to shared, completing the unit from the
    /// perspective of other processes. No-op for waiters.
    pub fn after_work(this: &Arc<Self>) -> Work {
        let this = Arc::clone(this);
        Work::new(move |state| {
            if this.locks.builder.load(Ordering::SeqCst) {
                // Shared `.rlib.lock` holders treat the unit as complete
                // when the fingerprint exists, so it must be on disk before
                // downgrading. Content is not compared here: dependency rmeta
                // checksums are refreshed at write time, so the persisted hash
                // can differ from the one prepared at plan time.
                if cfg!(debug_assertions) {
                    let stored = cargo_util::paths::read(&this.fingerprint).ok();
                    assert!(
                        stored.is_some_and(|s| !s.is_empty()),
                        "fingerprint was not persisted before downgrading .rlib.lock"
                    );
                }
                this.downgrade_rmeta(state)?;
                state.assert_locked(&this.locks.rlib, LockMode::Exclusive);
                state.downgrade_to_shared(&this.locks.rlib)?;
            }
            Ok(())
        })
    }
}
