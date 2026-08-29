//! Build-cache helpers (staging design).
//!
//! The original per-unit `.rmeta.lock` / `.rlib.lock` coordination
//! (`CacheCoordination`) is no longer needed. Each Cargo process builds
//! cacheable units in its own `_staging/<pid>/<pkg>/<hash>` directory and
//! publishes atomically via `rename` to `$CARGO_HOME/build-cache/<pkg>/<hash>`.
//! Concurrent publishers that race resolve via `AlreadyExists` / `DirectoryNotEmpty`
//! and discard their staging copy. Hit detection is done via
//! `fingerprint::CacheCompletionState::is_complete` without locking.
