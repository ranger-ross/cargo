//! Fetch remote cache entries before the job queue reaches their units.
//!
//! A unit's input guard hashes its dependencies' artifacts. Those hashes are
//! the blob hashes recorded by each dependency's cache entry or unit output, so
//! the guard can be computed from metadata as soon as every dependency has been
//! fetched, restored, captured, or found fresh. Prefetched entries land in
//! local storage, and the job's own restore becomes a local hit.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context as _;
use parking_lot::{Condvar, Mutex};

use super::exchange;
use super::format::{CacheEntry, Digest};
use super::upload::Remote;
use crate::CargoResult;
use crate::util::data_structures::HashMap;

/// Each worker blocks on network requests. Transfers within a fetch are
/// already concurrent. Units a worker has not started are restored by their
/// jobs, which hold job slots while they wait on the network.
const WORKERS: usize = 16;
/// Build-script output becomes available without a notification, so idle
/// workers recheck periodically.
const POLL: Duration = Duration::from_millis(50);

/// A dependency artifact: its unit directory and encoded relative path.
pub(in crate::compiler) struct DependencyArtifact {
    pub unit_dir: PathBuf,
    pub path: Vec<u8>,
}

/// A cacheable unit that can be fetched ahead of its job.
pub(in crate::compiler) trait PrefetchSource: Send + Sync {
    fn unit_hash(&self) -> &str;
    fn unit_dir(&self) -> &Path;
    /// Artifacts hashed into the guard, in guard order.
    fn dependencies(&self) -> &[DependencyArtifact];
    /// Whether inputs outside the dependency artifacts, such as build-script
    /// output, are available.
    fn inputs_ready(&self) -> bool;
    /// The guard computed from dependency artifact hashes in `dependencies` order.
    fn key_from(&self, dependency_hashes: &[Digest]) -> CargoResult<u64>;
}

/// What a job should do after claiming its unit.
pub(super) enum Prefetched {
    /// The entry for this guard is in local storage.
    Hit(u64),
    /// The remote cache had no entry for this guard.
    Miss(u64),
}

enum Status {
    Waiting,
    InFlight,
    Done(Prefetched),
    /// The job took over before the prefetch started.
    Claimed,
}

#[derive(Default)]
struct State {
    waiting: Vec<Arc<dyn PrefetchSource>>,
    status: HashMap<String, Status>,
    /// Blob hashes of finished units, by unit directory and relative path.
    known: HashMap<PathBuf, HashMap<Vec<u8>, Digest>>,
    shutdown: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    /// Wakes the job queue when a fetch finishes.
    waker: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

pub(super) struct Prefetcher {
    shared: Arc<Shared>,
    remote: Arc<Remote>,
    root: PathBuf,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl Prefetcher {
    pub fn new(remote: Arc<Remote>, root: PathBuf) -> Self {
        Self {
            shared: Arc::default(),
            remote,
            root,
            workers: Mutex::default(),
        }
    }

    pub fn register(&self, source: Arc<dyn PrefetchSource>) {
        {
            let mut state = self.shared.state.lock();
            if state.shutdown || state.status.contains_key(source.unit_hash()) {
                return;
            }
            state
                .status
                .insert(source.unit_hash().to_owned(), Status::Waiting);
            state.waiting.push(source);
        }
        self.start();
        self.shared.changed.notify_all();
    }

    /// Record a finished unit's outputs so dependents can compute their guards.
    pub fn record_outputs<'a>(
        &self,
        unit_dir: &Path,
        outputs: impl IntoIterator<Item = (&'a [u8], Digest)>,
    ) {
        record_outputs(&self.shared, unit_dir, outputs);
    }

    pub fn set_waker(&self, waker: Box<dyn Fn() + Send + Sync>) {
        *self.shared.waker.lock() = Some(waker);
    }

    /// Whether a fetch for this unit is running. It always finishes, so jobs
    /// can wait for it without holding a job slot.
    pub fn in_flight(&self, unit_hash: &str) -> bool {
        matches!(
            self.shared.state.lock().status.get(unit_hash),
            Some(Status::InFlight)
        )
    }

    /// Take over a unit from the prefetcher. Waits for an in-flight fetch.
    pub fn claim(&self, unit_hash: &str) -> Option<Prefetched> {
        let mut state = self.shared.state.lock();
        loop {
            match state.status.get(unit_hash) {
                None | Some(Status::Claimed) => return None,
                Some(Status::Waiting) => {
                    state
                        .waiting
                        .retain(|source| source.unit_hash() != unit_hash);
                    state.status.insert(unit_hash.to_owned(), Status::Claimed);
                    return None;
                }
                Some(Status::InFlight) => self.shared.changed.wait(&mut state),
                Some(Status::Done(Prefetched::Hit(key))) => return Some(Prefetched::Hit(*key)),
                Some(Status::Done(Prefetched::Miss(key))) => {
                    return Some(Prefetched::Miss(*key));
                }
            }
        }
    }

    /// Stop the workers and drop pending units, which reference this storage.
    pub fn shutdown(&self) {
        {
            let mut state = self.shared.state.lock();
            state.shutdown = true;
            state.waiting.clear();
        }
        self.shared.changed.notify_all();
        for worker in self.workers.lock().drain(..) {
            let _ = worker.join();
        }
        *self.shared.waker.lock() = None;
    }

    fn start(&self) {
        let mut workers = self.workers.lock();
        if !workers.is_empty() {
            return;
        }
        for _ in 0..WORKERS {
            let shared = Arc::clone(&self.shared);
            let remote = Arc::clone(&self.remote);
            let root = self.root.clone();
            match std::thread::Builder::new()
                .name("cargo-cache-prefetch".to_owned())
                .spawn(move || work(&shared, &remote, &root))
            {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    // Jobs restore their own units without a prefetcher.
                    tracing::debug!(%error, "failed to start remote cache prefetch worker");
                    break;
                }
            }
        }
    }
}

fn record_outputs<'a>(
    shared: &Shared,
    unit_dir: &Path,
    outputs: impl IntoIterator<Item = (&'a [u8], Digest)>,
) {
    let outputs = outputs
        .into_iter()
        .map(|(path, hash)| (path.to_vec(), hash))
        .collect();
    shared
        .state
        .lock()
        .known
        .insert(unit_dir.to_path_buf(), outputs);
    shared.changed.notify_all();
}

/// Take the next unit whose inputs are all known, with its dependency hashes.
fn next_ready(state: &mut State) -> Option<(Arc<dyn PrefetchSource>, Vec<Digest>)> {
    let known = &state.known;
    let (index, hashes) = state
        .waiting
        .iter()
        .enumerate()
        .find_map(|(index, source)| {
            let hashes = source
                .dependencies()
                .iter()
                .map(|dependency| {
                    known
                        .get(&dependency.unit_dir)?
                        .get(&dependency.path)
                        .copied()
                })
                .collect::<Option<Vec<_>>>()?;
            source.inputs_ready().then_some((index, hashes))
        })?;
    let source = state.waiting.swap_remove(index);
    state
        .status
        .insert(source.unit_hash().to_owned(), Status::InFlight);
    Some((source, hashes))
}

fn work(shared: &Shared, remote: &Remote, root: &Path) {
    loop {
        let (source, hashes) = {
            let mut state = shared.state.lock();
            loop {
                if state.shutdown || !remote.usable() {
                    return;
                }
                if let Some(next) = next_ready(&mut state) {
                    break next;
                }
                shared.changed.wait_for(&mut state, POLL);
            }
        };
        let unit_hash = source.unit_hash();
        let mut in_flight = InFlight {
            shared,
            unit_hash,
            status: Status::Claimed,
        };
        in_flight.status = match prefetch(source.as_ref(), &hashes, remote, root) {
            Ok((key, Some(entry))) => {
                tracing::debug!(unit_hash, "prefetched unit from remote cache");
                record_outputs(
                    shared,
                    source.unit_dir(),
                    entry
                        .outputs
                        .iter()
                        .map(|output| (output.path.as_slice(), output.hash)),
                );
                Status::Done(Prefetched::Hit(key))
            }
            Ok((key, None)) => {
                tracing::debug!(unit_hash, "remote cache miss during prefetch");
                Status::Done(Prefetched::Miss(key))
            }
            Err(error) => {
                remote.failed(error.context("prefetching remote cache entry"));
                Status::Claimed
            }
        };
    }
}

/// Publishes a fetch's final status when dropped, even if the fetch panics,
/// so waiting jobs can proceed.
struct InFlight<'a> {
    shared: &'a Shared,
    unit_hash: &'a str,
    status: Status,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let status = std::mem::replace(&mut self.status, Status::Claimed);
        self.shared
            .state
            .lock()
            .status
            .insert(self.unit_hash.to_owned(), status);
        self.shared.changed.notify_all();
        if let Some(wake) = &*self.shared.waker.lock() {
            wake();
        }
    }
}

fn prefetch(
    source: &dyn PrefetchSource,
    hashes: &[Digest],
    remote: &Remote,
    root: &Path,
) -> CargoResult<(u64, Option<CacheEntry>)> {
    let key = source
        .key_from(hashes)
        .context("computing prefetch input guard")?;
    let unit_hash = source.unit_hash();
    let store = super::snapshots::SnapshotStore::new(root);
    if let Some((_, entry)) = store.read_cache_entry(unit_hash)?
        && entry.fingerprint == key
    {
        return Ok((key, Some(entry)));
    }
    let fetched = exchange::fetch(&remote.cache, root, unit_hash, key)?;
    Ok((key, fetched.map(|(_, entry)| entry)))
}
