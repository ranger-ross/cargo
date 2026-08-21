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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::compiler::job_queue::{JobState, Work};
use crate::compiler::locking::LockKey;
use crate::compiler::{BuildRunner, Unit};
use crate::util::{CargoResult, internal};

/// Per-job coordination state for building one cacheable unit.
pub struct CacheCoordination {
    rmeta_path: PathBuf,
    rlib_path: PathBuf,
    fingerprint: PathBuf,
    /// Hex fingerprint hash this unit would persist. A cached unit is
    /// "complete for this identity" when the stored fingerprint file matches
    /// this value (the content encodes identity information such as the git
    /// revision that the unit hash does not capture).
    expected_hash: String,
    /// Whether the fingerprint's filesystem-status was up-to-date when it was
    /// computed (see [`crate::compiler::fingerprint::CacheCompletionState`]).
    /// A cache hit additionally requires this, so that changes to
    /// non-cacheable dependencies (path patches) invalidate the unit.
    fs_up_to_date: bool,    /// Lock keys, populated by [`CacheCoordination::coordinate`].
    rmeta_key: OnceLock<LockKey>,
    rlib_key: OnceLock<LockKey>,
    /// Whether this process is (or became) the builder of the unit.
    builder: AtomicBool,
    /// Whether the rmeta lock has already been downgraded to shared.
    rmeta_shared: AtomicBool,
}

impl CacheCoordination {
    /// Creates the coordination state for a cacheable `unit`.
    pub fn new(
        build_runner: &mut BuildRunner<'_, '_>,
        unit: &Unit,
        completion: crate::compiler::fingerprint::CacheCompletionState,
    ) -> CargoResult<Arc<Self>> {
        Ok(Arc::new(CacheCoordination {
            rmeta_path: build_runner.files().cache_rmeta_lock(unit),
            rlib_path: build_runner.files().cache_rlib_lock(unit),
            fingerprint: build_runner.files().fingerprint_file_path(unit, ""),
            expected_hash: completion.hash,
            fs_up_to_date: completion.fs_up_to_date,
            rmeta_key: OnceLock::new(),
            rlib_key: OnceLock::new(),
            builder: AtomicBool::new(false),
            rmeta_shared: AtomicBool::new(false),
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
    /// up-to-date at prepare time — otherwise the staleness (e.g. a changed
    /// path patch, or our own outputs missing on a cold cache) still needs to
    /// be resolved by building.
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
        loop {
            // Wait until the `.rmeta` is (or was) available: the builder
            // downgraded its rmeta lock to shared after rustc produced the
            // file, the unit is complete, or the builder crashed and released
            // its locks.
            let rmeta = state.lock_shared_path(self.rmeta_path.clone())?;
            let _ = self.rmeta_key.set(rmeta.clone());

            // A fingerprint matching our expected hash, with an up-to-date
            // filesystem status, means the unit is complete and immutable;
            // there is nothing for us to do.
            if self.is_complete(false) {
                state.unlock(&rmeta)?;
                return Ok(false);
            }

            // Try the rlib lock without blocking to detect whether a builder
            // is still actively compiling.
            let rlib = match state.try_lock_shared_path(self.rlib_path.clone())? {
                // No builder is currently compiling the unit. The unit was
                // either never built, already complete (completed while we
                // waited for the rmeta lock), or a previous builder crashed;
                // the first and last cases are handled by taking over below.
                Some(rlib) => {
                    // No builder observed; the unit is only complete if the
                    // fingerprint matches and the filesystem status was
                    // up-to-date at prepare time.
                    if self.is_complete(false) {
                        state.unlock(&rmeta)?;
                        state.unlock(&rlib)?;
                        return Ok(false);
                    }
                    rlib
                }
                // A builder is actively compiling. If the unit was never built
                // (no fingerprint at all), our rmeta lock acquisition above
                // proves its `.rmeta` is ready, so signal our own dependents
                // to start pipelined compilation against the metadata while
                // the builder still codegens. If the unit is being *rebuilt*
                // in place (a stale fingerprint still exists), pipelining
                // against the about-to-be-replaced rmeta would be incorrect,
                // so we simply wait for completion.
                None => {
                    if !self.fingerprint_exists() {
                        state.rmeta_produced();
                    }
                    // Wait for the builder to finish (or crash and release).
                    // A builder was observed, so a matching fingerprint is
                    // sufficient: it resolved the dirtiness.
                    let rlib = state.lock_shared_path(self.rlib_path.clone())?;
                    let _ = self.rlib_key.set(rlib.clone());
                    if self.is_complete(true) {
                        state.unlock(&rmeta)?;
                        state.unlock(&rlib)?;
                        return Ok(false);
                    }
                    // The builder crashed after producing the rmeta; we must
                    // complete the unit ourselves.
                    rlib
                }
            };
            let _ = self.rlib_key.set(rlib.clone());

            // Take the locks exclusively. Release our shared locks first so we
            // hold nothing while acquiring, then probe the rmeta lock: if
            // another process holds it (a builder that just finished, or
            // another waiter), it will resolve the unit's state — waiting for
            // it on the shared rlib lock (again holding nothing) is bounded by
            // that process's job and cannot deadlock, even when that process
            // is itself waiting on a unit whose locks we hold.
            state.unlock(&rlib)?;
            state.unlock(&rmeta)?;
            let contended = !state.try_lock_exclusive(&rmeta)?;
            if contended {
                // Someone else holds this unit's locks. They are either
                // building it or waiting on it; either way their job will
                // resolve the state. Wait for that job, then re-evaluate from
                // scratch rather than blocking on the exclusive lock.
                let rlib = state.lock_shared_path(self.rlib_path.clone())?;
                let _ = self.rlib_key.set(rlib.clone());
                if self.is_complete(true) {
                    state.unlock(&rlib)?;
                    return Ok(false);
                }
                state.unlock(&rlib)?;
                continue;
            }
            state.lock_exclusive(&rmeta)?;
            state.lock_exclusive(&rlib)?;
            if self.is_complete(false) {
                state.unlock(&rlib)?;
                state.unlock(&rmeta)?;
                return Ok(false);
            }
            self.builder.store(true, Ordering::SeqCst);
            return Ok(true);
        }
    }

    /// Downgrades the rmeta lock to shared. Called when rustc reports the
    /// `.rmeta` artifact (builder path) and again after the fingerprint is
    /// written (in case rustc never reported an rmeta, e.g. for units whose
    /// dependents do not pipeline). Idempotent.
    pub(crate) fn downgrade_rmeta(&self, state: &JobState<'_, '_>) -> CargoResult<()> {
        if !self.rmeta_shared.swap(true, Ordering::SeqCst) {
            let key = self
                .rmeta_key
                .get()
                .ok_or_else(|| internal("cache rmeta lock was not acquired"))?;
            state.downgrade_to_shared(key)?;
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
            if this.builder.load(Ordering::SeqCst) {
                this.downgrade_rmeta(state)?;
                let key = this
                    .rlib_key
                    .get()
                    .ok_or_else(|| internal("cache rlib lock was not acquired"))?;
                state.downgrade_to_shared(key)?;
            }
            Ok(())
        })
    }
}
