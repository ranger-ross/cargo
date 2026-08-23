//! Coordination for the cross-workspace build cache.
//!
//! Cacheable build units (see [`Unit::is_cacheable`]) are compiled directly
//! inside `$CARGO_HOME/build-cache` and are immutable once complete. Because
//! multiple Cargo processes may build and read the same unit concurrently,
//! each cacheable unit directory carries two state lock files:
//!
//! * `.rmeta.lock` — held **exclusively** while the unit is being compiled and
//!   its `.rmeta` has not yet been produced; downgraded to **shared** as soon
//!   as rustc reports the `.rmeta` artifact (which rustc writes completely
//!   before emitting the artifact notification). Other processes acquiring it
//!   shared therefore know the `.rmeta` is safe to read, even though the
//!   `.rlib` (or other final artifacts) are still being produced.
//! * `.rlib.lock` — held **exclusively** while the unit is being compiled and
//!   downgraded to **shared** only after all artifacts have been written *and*
//!   the fingerprint file has been persisted. Acquiring it shared therefore
//!   guarantees the fingerprint file exists, which is the cache's definition of
//!   "complete".
//!
//! Locks are always acquired in the same order (`.rmeta.lock` before
//! `.rlib.lock`) and contenders release their shared locks before attempting
//! an exclusive upgrade, which keeps the multi-lock protocol free of
//! lock-order cycles.
//!
//! ## Roles
//!
//! A job for a cacheable unit that was dirty at fingerprint-check time runs
//! [`CacheCoordination::coordinate`] before compiling. It either becomes the
//! **builder** (it compiles the unit) or a **waiter** (it observes the
//! builder's progress):
//!
//! * **Builder**: holds both locks exclusively (after a fingerprint
//!   double-check, since another process may have completed the unit while it
//!   waited), compiles, downgrades `.rmeta.lock` to shared as soon as rustc
//!   reports the `.rmeta` artifact so other processes can start pipelined
//!   compilation against the metadata, and finally downgrades `.rlib.lock` to
//!   shared after the fingerprint is written.
//! * **Waiter**: takes `.rmeta.lock` shared (blocking until the rmeta is ready
//!   or the builder is gone). If the unit is complete it is done. If the
//!   builder is still compiling, it signals `rmeta_produced` early so its own
//!   dependents can start type-checking against the metadata while the builder
//!   still codegens, then blocks on `.rlib.lock` shared. If the builder crashed
//!   (the fingerprint never appeared), it transitions to the builder role.
//!
//! ## Crash recovery
//!
//! If a builder dies mid-compile it releases its locks, leaving a partial unit
//! with no fingerprint. The next process to run the protocol observes the
//! missing fingerprint and takes over the build. All shared locks acquired
//! while waiting are released before taking the exclusive locks, and the
//! fingerprint is double-checked after acquisition, so concurrent contenders
//! resolve to a single builder without deadlock.
//!
//! ## Known limitation
//!
//! If a builder crashes *after* producing the `.rmeta` but before completing
//! the unit, a waiter may already have signaled `rmeta_produced` and started
//! its dependents compiling against that `.rmeta`. When the waiter then takes
//! over the build, its rustc invocation rewrites the `.rmeta` file while those
//! dependents may still be reading it, which can in theory cause a torn read.
//! The rewrite is byte-identical in practice (same unit, same inputs, same
//! rustc), and the window requires a builder crash at a precise moment, so
//! this is accepted for the PoC.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::compiler::job_queue::{JobState, Work};
use crate::compiler::locking::{LockKey, LockMode};
use crate::compiler::{BuildRunner, Unit};
use crate::util::CargoResult;

/// The lock keys and role flags for one coordinated cacheable unit.
///
/// The keys are known up front (they are derived from the unit's cache
/// directory), so they are constructed once instead of being recovered from
/// lookups after acquisition. The wait phases hold both keys shared; the
/// builder holds them exclusively after the exchange, and downgrades them
/// in [`CacheCoordination::downgrade_rmeta`] and
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

/// The next step of the coordination protocol.
enum Attempt {
    /// Take `.rmeta.lock` shared, waiting until the rmeta is readable or the
    /// builder is gone.
    WaitForRmeta,
    /// Detect an active builder via a non-blocking probe of `.rlib.lock`.
    ProbeBuilder,
    /// Convert the held shared locks into exclusive ownership of the unit
    /// (builder takeover), re-evaluating if another process got there first.
    TakeOver,
    /// Finished. `true` means compile (builder role); `false` means skip
    /// (the unit is complete).
    Done(bool),
}

/// Per-job coordination state for building one cacheable unit.
pub struct CacheCoordination {
    locks: UnitLocks,
    fingerprint: PathBuf,
    /// Hex fingerprint hash this unit would persist. A cached unit is
    /// "complete for this identity" when the stored fingerprint file matches
    /// this value (the content encodes identity information such as the git
    /// revision that the unit hash does not capture).
    expected_hash: String,
    /// Whether the fingerprint's filesystem-status was up-to-date when it was
    /// computed (see [`crate::compiler::fingerprint::CacheCompletionState`]).
    /// A cache hit additionally requires this so a partially written entry
    /// (e.g. missing outputs) is rebuilt rather than skipped.
    fs_up_to_date: bool,
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
            expected_hash: completion.hash,
            fs_up_to_date: completion.fs_up_to_date,
        }))
    }

    /// Returns whether the cached unit is complete for this unit's identity
    /// and filesystem state.
    ///
    /// `builder_active` says whether another process was observed holding the
    /// unit's locks (i.e. actively compiling) before this check. A builder
    /// resolves the unit's dirtiness, so a content match alone is enough to
    /// skip when one was seen. Without an active builder, the unit is only
    /// complete when the fingerprint matches *and* the filesystem status was
    /// up-to-date at prepare time — otherwise the staleness (our own outputs
    /// missing on a cold or partially-written cache) still needs to be
    /// resolved by building.
    fn is_complete(&self, builder_active: bool) -> bool {
        let content_matches = match cargo_util::paths::read(&self.fingerprint) {
            Ok(stored) => stored == self.expected_hash,
            Err(_) => false,
        };
        content_matches && (self.fs_up_to_date || builder_active)
    }

    /// Returns whether any fingerprint file exists at all.
    ///
    /// A missing fingerprint means the unit was never built (cold cache). A
    /// *present but mismatched* fingerprint means the unit is being rebuilt in
    /// place with different identity content (e.g. a new git revision), in
    /// which case pipelining against the stale rmeta would be incorrect.
    fn fingerprint_exists(&self) -> bool {
        self.fingerprint.exists()
    }

    /// Runs the coordination protocol for a cacheable unit.
    ///
    /// Returns `Ok(true)` if this process should compile the unit (builder
    /// role). Returns `Ok(false)` if the unit is already complete (or was
    /// completed while we waited), in which case the caller must skip the
    /// compilation.
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

    /// Takes `.rmeta.lock` shared, blocking until the rmeta is readable (the
    /// builder downgraded its lock), the unit is complete, or the builder
    /// crashed and released its locks.
    fn wait_for_rmeta(&self, state: &JobState<'_, '_>) -> CargoResult<Attempt> {
        state.lock_shared_path(self.locks.rmeta.path().clone())?;
        self.locks.held_rmeta.store(true, Ordering::SeqCst);

        // A fingerprint matching our expected hash, with an up-to-date
        // filesystem status, means the unit is complete and immutable;
        // there is nothing for us to do.
        if self.is_complete(false) {
            self.locks.release_shared(state)?;
            return Ok(Attempt::Done(false));
        }
        Ok(Attempt::ProbeBuilder)
    }

    /// Detects an active builder via a non-blocking probe of `.rlib.lock`,
    /// then either waits for it or prepares a takeover of the crashed build.
    fn probe_builder(&self, state: &JobState<'_, '_>) -> CargoResult<Attempt> {
        match state.try_lock_shared_path(self.locks.rlib.path().clone())? {
            // No builder is currently compiling the unit. The unit was
            // either never built, already complete (completed while we
            // waited for the rmeta lock), or a previous builder crashed;
            // the first and last cases are handled by taking over below.
            Some(_) => {
                self.locks.held_rlib.store(true, Ordering::SeqCst);
                if self.is_complete(false) {
                    self.locks.release_shared(state)?;
                    return Ok(Attempt::Done(false));
                }
                Ok(Attempt::TakeOver)
            }
            // A builder is actively compiling. If the unit was never built
            // (no fingerprint at all), our `.rmeta.lock` acquisition proves
            // its `.rmeta` is ready, so signal our own dependents to start
            // pipelined compilation against the metadata while the builder
            // still codegens. If the unit is being *rebuilt* in place (a
            // stale fingerprint still exists), pipelining against the
            // about-to-be-replaced rmeta would be incorrect, so we simply
            // wait for completion.
            None => {
                // The early pipelining signal requires that we can prove the
                // rmeta is readable, which rests on holding `.rmeta.lock`
                // shared.
                state.assert_locked(&self.locks.rmeta, LockMode::Shared);
                if !self.fingerprint_exists() {
                    state.rmeta_produced();
                }
                // Wait for the builder to finish (or crash and release).
                // A builder was observed, so a matching fingerprint is
                // sufficient: it resolved the dirtiness.
                state.lock_shared_path(self.locks.rlib.path().clone())?;
                self.locks.held_rlib.store(true, Ordering::SeqCst);
                if self.is_complete(true) {
                    self.locks.release_shared(state)?;
                    return Ok(Attempt::Done(false));
                }
                // The builder crashed after producing the rmeta; we must
                // complete the unit ourselves.
                Ok(Attempt::TakeOver)
            }
        }
    }

    /// Converts the held shared locks into exclusive ownership of the unit.
    ///
    /// `exchange_for_exclusive` releases our shared locks first so we hold
    /// nothing while probing, then probes the rmeta lock: if another process
    /// holds it (a builder that just finished, or another waiter), it will
    /// resolve the unit's state. Waiting for that process on the shared rlib
    /// lock (again holding nothing) is bounded by its job and cannot
    /// deadlock, even when it is itself waiting on a unit whose locks we
    /// hold.
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
            if self.is_complete(true) {
                state.unlock(&self.locks.rlib)?;
                return Ok(Attempt::Done(false));
            }
            state.unlock(&self.locks.rlib)?;
            return Ok(Attempt::WaitForRmeta);
        }
        // `exchange_for_exclusive` already took `.rmeta.lock` exclusively;
        // only the rlib lock still needs acquiring. (A redundant
        // re-acquisition would double-count against the lock manager's
        // recursion accounting.)
        state.lock_exclusive(&self.locks.rlib)?;
        if self.is_complete(false) {
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
                // The fingerprint is what makes shared `.rlib.lock` holders
                // consider the unit complete, so it must be on disk before
                // the lock goes shared. The content is not compared here:
                // dependency rmeta checksums are legitimately refreshed at
                // write time, so the persisted hash may differ from the one
                // prepared at plan time.
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
