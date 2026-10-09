//! Fetch remote cache entries before the job queue reaches their units.
//!
//! A unit's input guard hashes its dependencies' artifacts. Those hashes are
//! the blob hashes recorded by each dependency's cache entry or unit output, so
//! the guard can be computed from metadata as soon as every dependency has been
//! looked up, restored, captured, or found fresh.
//!
//! Fetches run in two phases. The first looks up the entry and downloads every
//! output except rlibs and object files, which lets dependents compile against
//! the restored rmeta. The second downloads the rest and publishes the local
//! cache entry. Each phase has its own workers so large downloads do not delay
//! lookups. Units that build scripts or proc-macros link against are fetched
//! whole in the first phase, ahead of other units.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::Context as _;
use parking_lot::{Condvar, Mutex};

use super::deferred_output;
use super::exchange::{self, Found};
use super::format::{CacheEntry, Digest};
use super::snapshots::SnapshotStore;
use super::upload::Remote;
use crate::CargoResult;
use crate::util::data_structures::{HashMap, HashSet};

/// Lookup and metadata workers. A unit's first phase is a few small round
/// trips, so throughput depends on how many run at once.
const METADATA_WORKERS: usize = 32;
/// Workers downloading rlibs and object files.
const BULK_WORKERS: usize = 8;

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
    /// Unit directories of build-script runs that must finish first. Their
    /// output is part of the guard.
    fn build_scripts(&self) -> &[PathBuf];
    /// The guard computed from dependency artifact hashes in `dependencies` order.
    fn key_from(&self, dependency_hashes: &[Digest]) -> CargoResult<u64>;
    /// Whether a build script or proc-macro links against this unit, so its
    /// full outputs are needed early.
    fn urgent(&self) -> bool;
}

/// What a job finds after claiming its unit.
pub(super) enum Prefetched {
    /// The entry for this guard is in local storage.
    Hit(u64),
    /// The remote cache had no entry for this guard.
    Miss(u64),
    /// Blobs other than deferred outputs are in local storage and the rest are
    /// downloading. See [`Prefetcher::wait_for_outputs`].
    Metadata(u64, Arc<CacheEntry>),
}

enum Status {
    /// Registered, with the number of unit directories whose outputs are not
    /// known yet. At zero the unit is in the ready queue.
    Waiting(usize),
    InFlight,
    Metadata(u64, Arc<CacheEntry>),
    Hit(u64),
    Miss(u64),
    /// The job took over, or the fetch failed.
    Claimed,
}

/// The second phase of a fetch.
struct Bulk {
    unit_hash: String,
    key: u64,
    found: Found,
}

#[derive(Default)]
struct State {
    status: HashMap<String, Status>,
    /// Registered units by unit hash, until a worker or job takes them.
    sources: HashMap<String, Arc<dyn PrefetchSource>>,
    /// Units whose inputs are all known. Urgent units are queued first.
    ready: VecDeque<String>,
    /// Waiting units by the unit directories they still need.
    dependents: HashMap<PathBuf, Vec<String>>,
    bulk: VecDeque<Bulk>,
    /// Blob hashes of finished or found units, by unit directory and relative
    /// path. Finished build-script runs have no entries.
    known: HashMap<PathBuf, HashMap<Vec<u8>, Digest>>,
    shutdown: bool,
}

impl State {
    /// Record a unit directory's outputs and queue dependents that became ready.
    fn record(&mut self, unit_dir: &Path, outputs: HashMap<Vec<u8>, Digest>) {
        if self.known.insert(unit_dir.to_path_buf(), outputs).is_some() {
            return;
        }
        for unit_hash in self.dependents.remove(unit_dir).unwrap_or_default() {
            if let Some(Status::Waiting(missing)) = self.status.get_mut(&unit_hash) {
                *missing -= 1;
                if *missing == 0 {
                    self.queue_ready(unit_hash);
                }
            }
        }
    }

    fn queue_ready(&mut self, unit_hash: String) {
        if self.sources[&unit_hash].urgent() {
            self.ready.push_front(unit_hash);
        } else {
            self.ready.push_back(unit_hash);
        }
    }

    /// Take the next ready unit with its dependency hashes.
    fn next_ready(&mut self) -> Option<(Arc<dyn PrefetchSource>, Vec<Digest>)> {
        while let Some(unit_hash) = self.ready.pop_front() {
            // A job may have claimed it since.
            let Some(source) = self.sources.remove(&unit_hash) else {
                continue;
            };
            let hashes = source
                .dependencies()
                .iter()
                .map(|dependency| {
                    self.known[&dependency.unit_dir]
                        .get(&dependency.path)
                        .copied()
                })
                .collect::<Option<Vec<_>>>();
            // A dependency without the expected artifact cannot be keyed here.
            let status = if hashes.is_some() {
                Status::InFlight
            } else {
                Status::Claimed
            };
            self.status.insert(unit_hash, status);
            if let Some(hashes) = hashes {
                return Some((source, hashes));
            }
        }
        None
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    /// Wakes the job queue when a unit's status changes.
    waker: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Shared {
    fn wake(&self) {
        self.changed.notify_all();
        if let Some(wake) = &*self.waker.lock() {
            wake();
        }
    }
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
            let unit_hash = source.unit_hash().to_owned();
            if state.shutdown || state.status.contains_key(&unit_hash) {
                return;
            }
            let needed = source
                .dependencies()
                .iter()
                .map(|dependency| &dependency.unit_dir)
                .chain(source.build_scripts())
                .filter(|unit_dir| !state.known.contains_key(*unit_dir))
                .cloned()
                .collect::<HashSet<_>>();
            let missing = needed.len();
            for unit_dir in needed {
                state
                    .dependents
                    .entry(unit_dir)
                    .or_default()
                    .push(unit_hash.clone());
            }
            state
                .status
                .insert(unit_hash.clone(), Status::Waiting(missing));
            state.sources.insert(unit_hash.clone(), source);
            if missing == 0 {
                state.queue_ready(unit_hash);
            }
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

    /// Whether the job should wait without a job slot. A ready unit is taken
    /// by a worker while the remote is usable, a running fetch always
    /// finishes, and both status changes wake the job queue.
    pub fn blocks(&self, unit_hash: &str, metadata_first: bool) -> bool {
        match self.shared.state.lock().status.get(unit_hash) {
            Some(Status::Waiting(missing)) => *missing == 0 && self.remote.usable(),
            Some(Status::InFlight) => true,
            Some(Status::Metadata(..)) => !metadata_first,
            _ => false,
        }
    }

    /// Take over a unit from the prefetcher. Waits for a running lookup, and
    /// with `metadata_first` unset also for the remaining downloads.
    pub fn claim(&self, unit_hash: &str, metadata_first: bool) -> Option<Prefetched> {
        let mut state = self.shared.state.lock();
        loop {
            match state.status.get(unit_hash) {
                None | Some(Status::Claimed) => return None,
                Some(Status::Waiting(_)) => {
                    state.sources.remove(unit_hash);
                    state.status.insert(unit_hash.to_owned(), Status::Claimed);
                    return None;
                }
                Some(Status::Metadata(key, entry)) if metadata_first => {
                    return Some(Prefetched::Metadata(*key, Arc::clone(entry)));
                }
                Some(Status::InFlight | Status::Metadata(..)) => {
                    self.shared.changed.wait(&mut state);
                }
                Some(Status::Hit(key)) => return Some(Prefetched::Hit(*key)),
                Some(Status::Miss(key)) => return Some(Prefetched::Miss(*key)),
            }
        }
    }

    /// Wait for the downloads following [`Prefetched::Metadata`]. Returns
    /// whether every blob and the cache entry are in local storage.
    pub fn wait_for_outputs(&self, unit_hash: &str) -> bool {
        let mut state = self.shared.state.lock();
        loop {
            match state.status.get(unit_hash) {
                Some(Status::Metadata(..)) => self.shared.changed.wait(&mut state),
                Some(Status::Hit(_)) => return true,
                _ => return false,
            }
        }
    }

    /// Stop the workers and drop pending units, which reference this storage.
    /// Queued downloads are dropped because no job is left to use them.
    pub fn shutdown(&self) {
        {
            let mut state = self.shared.state.lock();
            state.shutdown = true;
            state.sources.clear();
            state.ready.clear();
            state.bulk.clear();
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
        let kinds = std::iter::repeat_n(fetch_metadata as Worker, METADATA_WORKERS)
            .chain(std::iter::repeat_n(fetch_bulk as Worker, BULK_WORKERS));
        for work in kinds {
            let shared = Arc::clone(&self.shared);
            let remote = Arc::clone(&self.remote);
            let root = self.root.clone();
            match std::thread::Builder::new()
                .name("cargo-cache-prefetch".to_owned())
                .spawn(move || work(&shared, &remote, &root))
            {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    // Without bulk workers, queued second phases would never
                    // finish. Stopping the remote makes running workers exit
                    // and jobs restore their own units.
                    self.remote
                        .failed(anyhow::Error::new(error).context("starting prefetch worker"));
                    break;
                }
            }
        }
    }
}

type Worker = fn(&Shared, &Remote, &Path);

fn record_outputs<'a>(
    shared: &Shared,
    unit_dir: &Path,
    outputs: impl IntoIterator<Item = (&'a [u8], Digest)>,
) {
    let outputs = outputs
        .into_iter()
        .map(|(path, hash)| (path.to_vec(), hash))
        .collect();
    let became_ready = {
        let mut state = shared.state.lock();
        let ready = state.ready.len();
        state.record(unit_dir, outputs);
        state.ready.len() > ready
    };
    if became_ready {
        shared.changed.notify_all();
    }
}

fn record_entry(shared: &Shared, unit_dir: &Path, entry: &CacheEntry) {
    record_outputs(
        shared,
        unit_dir,
        entry
            .outputs
            .iter()
            .map(|output| (output.path.as_slice(), output.hash)),
    );
}

/// First phase: look up a ready unit and download its metadata.
fn fetch_metadata(shared: &Shared, remote: &Remote, root: &Path) {
    loop {
        let (source, hashes) = {
            let mut state = shared.state.lock();
            loop {
                if state.shutdown || !remote.usable() {
                    return;
                }
                if let Some(next) = state.next_ready() {
                    break next;
                }
                shared.changed.wait(&mut state);
            }
        };
        let unit_hash = source.unit_hash();
        let mut done = Completion::new(shared, unit_hash);
        match lookup(shared, source.as_ref(), &hashes, remote, root) {
            Ok(Lookup::Done(status)) => done.status = Some(status),
            Ok(Lookup::Metadata(key, found)) => {
                tracing::debug!(unit_hash, "prefetched unit metadata from remote cache");
                let mut state = shared.state.lock();
                // Bulk workers abandon their queue under this lock once the
                // remote fails, so nothing may be queued after that.
                if remote.usable() {
                    state.status.insert(
                        unit_hash.to_owned(),
                        Status::Metadata(key, Arc::clone(&found.entry)),
                    );
                    state.bulk.push_back(Bulk {
                        unit_hash: unit_hash.to_owned(),
                        key,
                        found,
                    });
                    // The bulk worker owns the final status from here.
                    done.status = None;
                }
            }
            Err(error) => remote.failed(error.context("prefetching remote cache entry")),
        }
    }
}

enum Lookup {
    Done(Status),
    Metadata(u64, Found),
}

fn lookup(
    shared: &Shared,
    source: &dyn PrefetchSource,
    hashes: &[Digest],
    remote: &Remote,
    root: &Path,
) -> CargoResult<Lookup> {
    let key = source
        .key_from(hashes)
        .context("computing prefetch input guard")?;
    let unit_hash = source.unit_hash();
    if let Some((_, entry)) = SnapshotStore::new(root).read_cache_entry(unit_hash)?
        && entry.fingerprint == key
    {
        tracing::debug!(unit_hash, "prefetch found unit in local cache");
        record_entry(shared, source.unit_dir(), &entry);
        return Ok(Lookup::Done(Status::Hit(key)));
    }
    let Some(mut found) = exchange::lookup(&remote.cache, root, unit_hash, key)? else {
        tracing::debug!(unit_hash, "remote cache miss during prefetch");
        return Ok(Lookup::Done(Status::Miss(key)));
    };
    // Dependents can compute their guards before any blob arrives.
    record_entry(shared, source.unit_dir(), &found.entry);
    // Build tools link against urgent units, so their rlibs cannot wait.
    if !source.urgent() {
        found.download(&remote.cache, root, |output| !deferred_output(&output.path))?;
        if !found.is_complete() {
            return Ok(Lookup::Metadata(key, found));
        }
    }
    found.finish(&remote.cache, root, unit_hash)?;
    tracing::debug!(unit_hash, "prefetched unit from remote cache");
    Ok(Lookup::Done(Status::Hit(key)))
}

/// Second phase: download rlibs and object files.
fn fetch_bulk(shared: &Shared, remote: &Remote, root: &Path) {
    loop {
        let Bulk {
            unit_hash,
            key,
            found,
        } = {
            let mut state = shared.state.lock();
            loop {
                if !remote.usable() {
                    // Jobs waiting for these downloads must not wait forever.
                    for bulk in state.bulk.drain(..).collect::<Vec<_>>() {
                        state.status.insert(bulk.unit_hash, Status::Claimed);
                    }
                    drop(state);
                    shared.wake();
                    return;
                }
                if let Some(bulk) = state.bulk.pop_front() {
                    break bulk;
                }
                if state.shutdown {
                    return;
                }
                shared.changed.wait(&mut state);
            }
        };
        let mut done = Completion::new(shared, &unit_hash);
        match found.finish(&remote.cache, root, &unit_hash) {
            Ok(_) => {
                tracing::debug!(unit_hash, "prefetched unit from remote cache");
                done.status = Some(Status::Hit(key));
            }
            Err(error) => remote.failed(error.context("downloading remote cache outputs")),
        }
    }
}

/// Publishes a fetch's status when dropped, even if the fetch panics, so
/// waiting jobs can proceed. A panic or error leaves the unit claimed.
struct Completion<'a> {
    shared: &'a Shared,
    unit_hash: &'a str,
    status: Option<Status>,
}

impl<'a> Completion<'a> {
    fn new(shared: &'a Shared, unit_hash: &'a str) -> Self {
        Self {
            shared,
            unit_hash,
            status: Some(Status::Claimed),
        }
    }
}

impl Drop for Completion<'_> {
    fn drop(&mut self) {
        if let Some(status) = self.status.take() {
            self.shared
                .state
                .lock()
                .status
                .insert(self.unit_hash.to_owned(), status);
        }
        self.shared.wake();
    }
}
