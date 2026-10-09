mod exchange;
mod format;
mod prefetch;
mod remote;
mod snapshots;
mod upload;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use cargo_util::paths::create_dir_all;
use filetime::FileTime;
use format::{
    CacheEntry, CachedOutput, Digest, SYMLINK_MODE, UnitOutput, decode_output_path,
    encode_output_path,
};
use parking_lot::Mutex;
use remote::RemoteCache;
use tracing::instrument;

use crate::ops::CleanContext;
use crate::util::cache_lock::CacheLockMode;
use crate::util::data_structures::{HashMap, HashSet};
use crate::{CargoResult, GlobalContext};
use prefetch::{Prefetched, Prefetcher};
use snapshots::{SnapshotStore, blob_path, check_directory, publish_blob, regular_size};
use upload::{Remote, Uploader};

pub(super) use format::Output;
pub(super) use prefetch::{DependencyArtifact, PrefetchSource};

/// Encode a path relative to a unit directory as stored in cache metadata.
pub(super) fn output_path_key(relative: &Path) -> CargoResult<Vec<u8>> {
    encode_output_path(relative)
}

/// The prefetch key recording that a build-script run's job has finished.
/// Its outputs are recorded under the unit directory as soon as a prefetch
/// finds them, but consumers also need the parsed output and a final `OUT_DIR`.
pub(super) fn build_script_marker(run_unit_dir: &Path) -> PathBuf {
    run_unit_dir.join("run")
}

/// Outputs that dependents need only for linking or debugging. A prefetch
/// downloads them last so dependents can compile against the rmeta sooner.
fn deferred_output(path: &[u8]) -> bool {
    decode_output_path(path).is_ok_and(|path| {
        path.extension()
            .is_some_and(|extension| extension == "rlib" || extension == "o")
    })
}

/// The result of restoring a cacheable unit.
pub(super) enum Restored {
    Miss,
    Complete,
    /// Outputs other than deferred ones are restored. Pass this to
    /// [`BlobStorage::finish_restore`] for the rest.
    Metadata(PendingOutputs),
}

pub(super) struct PendingOutputs {
    unit_hash: String,
    fingerprint: u64,
    entry: Arc<CacheEntry>,
}

/// The tracking object recorded in a unit's fingerprint. Cacheable units are
/// tracked by their cache entry and other non-local units by a unit output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TrackedOutput {
    UnitOutput(Digest),
    /// The entry named by the unit hash. Its contents are not pinned because
    /// workspaces sharing a unit hash can each publish it.
    CacheEntry,
}

#[derive(Default)]
struct Used {
    unit_outputs: HashSet<Digest>,
    cache_entries: HashSet<String>,
}

pub struct BlobStorage {
    root: PathBuf,
    used: Mutex<Used>,
    remote: Option<Arc<Remote>>,
    /// Started on the first remote publication.
    uploader: Mutex<Option<Uploader>>,
    /// Fetches remote entries ahead of the job queue.
    prefetcher: Option<Prefetcher>,
    // Rejected hits can still publish if recompilation changes either identity.
    restored_units: Mutex<HashMap<String, (u64, Digest)>>,
    /// Hashes of finished build scripts' `OUT_DIR`s, shared by their consumers.
    tree_hashes: Mutex<HashMap<PathBuf, Digest>>,
}

impl BlobStorage {
    pub fn new(root: PathBuf, gctx: &GlobalContext) -> CargoResult<Self> {
        let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
        check_directory(&root)?;
        create_dir_all(&root)?;
        let remote = match RemoteCache::from_config(gctx) {
            Ok(remote) => remote,
            Err(error) => {
                gctx.shell()
                    .warn(format!("remote cache disabled: {error:#}"))?;
                None
            }
        };
        tracing::debug!(
            root = %root.display(),
            remote_enabled = remote.is_some(),
            remote_read_only = remote.as_ref().is_some_and(RemoteCache::is_read_only),
            "shared blob storage initialized"
        );
        let remote = remote.map(|cache| Arc::new(Remote::new(cache)));
        let prefetcher = remote
            .as_ref()
            .map(|remote| Prefetcher::new(Arc::clone(remote), root.clone()));
        Ok(Self {
            root,
            used: Mutex::default(),
            remote,
            uploader: Mutex::default(),
            prefetcher,
            restored_units: Mutex::default(),
            tree_hashes: Mutex::default(),
        })
    }

    /// Validate a fingerprint's tracking pointer before reusing it. `cache_key`
    /// is the unit hash when the unit is cacheable in this invocation. A pointer
    /// of the other kind is not reused, so the unit is recaptured as that kind.
    pub(super) fn prepare_unit(
        &self,
        tracked: Option<TrackedOutput>,
        cache_key: Option<&str>,
        unit_dir: &Path,
    ) -> CargoResult<Option<TrackedOutput>> {
        let store = SnapshotStore::new(&self.root);
        match (tracked, cache_key) {
            (Some(TrackedOutput::UnitOutput(id)), None) => {
                let Some(outputs) = store.complete_unit(&id)? else {
                    return Ok(None);
                };
                self.record_outputs(
                    unit_dir,
                    outputs
                        .iter()
                        .map(|output| (output.path.as_slice(), output.hash)),
                );
                self.used.lock().unit_outputs.insert(id);
                Ok(tracked)
            }
            (Some(TrackedOutput::CacheEntry), Some(unit_hash)) => {
                let Some(entry) = store.complete_cache_entry(unit_hash)? else {
                    return Ok(None);
                };
                self.record_outputs(
                    unit_dir,
                    entry
                        .outputs
                        .iter()
                        .map(|output| (output.path.as_slice(), output.hash)),
                );
                self.used.lock().cache_entries.insert(unit_hash.to_owned());
                Ok(tracked)
            }
            _ => Ok(None),
        }
    }

    /// Queue a dirty cacheable unit for remote prefetching.
    pub(super) fn prefetch(&self, source: Arc<dyn PrefetchSource>) {
        if let Some(prefetcher) = &self.prefetcher
            && self.remote().is_some()
        {
            prefetcher.register(source);
        }
    }

    /// Called when a prefetch finishes, so deferred jobs can start.
    pub fn set_waker(&self, waker: Box<dyn Fn() + Send + Sync>) {
        if let Some(prefetcher) = &self.prefetcher {
            prefetcher.set_waker(waker);
        }
    }

    /// Whether a remote prefetch should keep this unit's job from starting.
    /// With `metadata_first`, the job may start once metadata is available.
    pub(super) fn prefetching(&self, unit_hash: &str, metadata_first: bool) -> bool {
        self.prefetcher
            .as_ref()
            .is_some_and(|prefetcher| prefetcher.blocks(unit_hash, metadata_first))
    }

    /// Make a finished build-script run available to prefetching dependents.
    pub(super) fn build_script_finished(&self, unit_dir: &Path) {
        self.record_outputs(&build_script_marker(unit_dir), []);
    }

    /// A directory hash computed at most once per build in the common case.
    /// The directory must not change for the rest of the build.
    pub(super) fn tree_hash(
        &self,
        dir: &Path,
        compute: impl FnOnce() -> CargoResult<Digest>,
    ) -> CargoResult<Digest> {
        if let Some(hash) = self.tree_hashes.lock().get(dir) {
            return Ok(*hash);
        }
        // Hashing outside the lock lets unrelated directories proceed.
        let hash = compute()?;
        self.tree_hashes.lock().insert(dir.to_path_buf(), hash);
        Ok(hash)
    }

    /// Make a finished unit's blob hashes available to prefetching dependents.
    fn record_outputs<'a>(
        &self,
        unit_dir: &Path,
        outputs: impl IntoIterator<Item = (&'a [u8], Digest)>,
    ) {
        if let Some(prefetcher) = &self.prefetcher {
            prefetcher.record_outputs(unit_dir, outputs);
        }
    }

    /// Capture all regular compiler outputs, including raw rustc dep-info.
    /// Hardlinks are allowed only when the compiler detaches the output tree
    /// before every dirty run, including builds with tracking disabled.
    pub(super) fn capture_outputs(
        &self,
        unit_dir: &Path,
        out_dir: &Path,
        hardlink_allowed: bool,
    ) -> CargoResult<Vec<Output>> {
        ensure!(
            out_dir == unit_dir.join("out"),
            "unexpected unit output directory"
        );
        self.capture_files(unit_dir, &[out_dir.to_path_buf()], hardlink_allowed)
    }

    /// Capture the regular files under each root, which must lie in one of the
    /// unit's output trees.
    #[instrument(skip_all)]
    pub(super) fn capture_files(
        &self,
        unit_dir: &Path,
        roots: &[PathBuf],
        hardlink_allowed: bool,
    ) -> CargoResult<Vec<Output>> {
        let mut outputs = Vec::new();
        for root in roots {
            let relative = root.strip_prefix(unit_dir)?;
            ensure!(
                OUTPUT_TREES.iter().any(|tree| relative.starts_with(tree)),
                "unexpected unit output path {}",
                root.display()
            );
            if root.is_dir() {
                check_directory(root)?;
            }
            for entry in walkdir::WalkDir::new(root) {
                let entry = entry?;
                let path = entry.path();
                let (hash, size) = if entry.file_type().is_symlink() {
                    self.capture_symlink(path)?
                } else if entry.file_type().is_file() {
                    let size = entry.metadata()?.len();
                    let hash = Self::hash(path)?;
                    let storage_path = blob_path(&self.root, &hash);
                    if !self.insert(path, &storage_path, &hash, false, hardlink_allowed)? {
                        self.dedup(path, &storage_path, &hash, hardlink_allowed)?;
                    }
                    (hash, size)
                } else {
                    continue;
                };
                outputs.push(Output {
                    path: encode_output_path(path.strip_prefix(unit_dir)?)?,
                    hash,
                    size,
                });
            }
        }
        self.record_outputs(
            unit_dir,
            outputs
                .iter()
                .map(|output| (output.path.as_slice(), output.hash)),
        );
        Ok(outputs)
    }

    /// Store a symlink's target as a blob. Build scripts may link generated
    /// trees to sources.
    fn capture_symlink(&self, path: &Path) -> CargoResult<(Digest, u64)> {
        let target = symlink_target(path)?;
        let hash = *blake3::hash(&target).as_bytes();
        let blob = blob_path(&self.root, &hash);
        if fs::symlink_metadata(&blob).is_err() {
            let staging = tempfile::Builder::new()
                .prefix(".blob")
                .tempdir_in(&self.root)?;
            let staged = staging.path().join("artifact");
            fs::write(&staged, &target)?;
            publish_blob(&staged, &blob)?;
        }
        Ok((hash, target.len() as u64))
    }

    /// Track a non-cacheable unit by its content-addressed unit output.
    pub(super) fn publish_unit_output(&self, outputs: Vec<Output>) -> CargoResult<TrackedOutput> {
        let output = UnitOutput::new(outputs);
        SnapshotStore::new(&self.root).publish_unit(&output)?;
        self.used.lock().unit_outputs.insert(output.id);
        Ok(TrackedOutput::UnitOutput(output.id))
    }

    /// Track a cacheable unit by a cache entry that also records restore metadata.
    pub(super) fn publish_cache_entry(
        &self,
        unit_hash: &str,
        fingerprint: u64,
        outputs: Vec<Output>,
        unit_dir: &Path,
    ) -> CargoResult<TrackedOutput> {
        let mut cached = Vec::with_capacity(outputs.len());
        for output in outputs {
            let path = unit_dir.join(restore_path(&output.path)?);
            let file = fs::symlink_metadata(&path)?;
            let (unchanged, mode) = if file.file_type().is_symlink() {
                let target = symlink_target(&path)?;
                (target.len() as u64 == output.size, SYMLINK_MODE)
            } else {
                (
                    file.is_file() && file.len() == output.size,
                    permission_mode(&file),
                )
            };
            ensure!(unchanged, "captured output changed: {}", path.display());
            let mtime = FileTime::from_last_modification_time(&file);
            cached.push(CachedOutput {
                path: output.path,
                hash: output.hash,
                size: output.size,
                mode,
                mtime_seconds: mtime.unix_seconds(),
                mtime_nanos: mtime.nanoseconds(),
            });
        }
        let entry = CacheEntry::new(fingerprint, cached);
        let digest = SnapshotStore::new(&self.root).publish_cache_entry(unit_hash, &entry)?;
        self.used.lock().cache_entries.insert(unit_hash.to_owned());
        if let Some(remote) = &self.remote
            && remote.usable()
            && !remote.cache.is_read_only()
        {
            let restored =
                self.restored_units.lock().get(unit_hash) == Some(&(fingerprint, digest));
            if restored {
                tracing::debug!(
                    unit_hash,
                    "skipping remote publication of unchanged cache hit"
                );
            } else {
                self.enqueue_upload(remote, unit_hash, entry);
            }
        }
        Ok(TrackedOutput::CacheEntry)
    }

    /// Stage and verify restored bytes before replacing the output tree.
    /// The compiler calls `accept_cache_entry` after dep-info and environment
    /// validation, so a rejected hit cannot become a successful snapshot member.
    /// With `metadata_first`, a prefetch that has only downloaded metadata
    /// restores that part and returns [`Restored::Metadata`].
    pub(super) fn restore_cache_entry(
        &self,
        unit_hash: &str,
        fingerprint: u64,
        unit_dir: &Path,
        metadata_first: bool,
    ) -> CargoResult<Restored> {
        // Waits for a running lookup. A prefetched hit is a local entry.
        let prefetched = self
            .prefetcher
            .as_ref()
            .and_then(|prefetcher| prefetcher.claim(unit_hash, metadata_first));
        if let Some(Prefetched::Metadata(key, entry)) = &prefetched
            && *key == fingerprint
        {
            self.restore_outputs(entry, unit_dir, |output| !deferred_output(&output.path))?;
            tracing::debug!(unit_hash, "restored unit metadata from remote cache");
            return Ok(Restored::Metadata(PendingOutputs {
                unit_hash: unit_hash.to_owned(),
                fingerprint,
                entry: Arc::clone(entry),
            }));
        }
        let store = SnapshotStore::new(&self.root);
        if let Some((digest, entry)) = store.read_cache_entry(unit_hash)?
            && entry.fingerprint == fingerprint
        {
            match self.restore_outputs(&entry, unit_dir, |_| true) {
                Ok(()) => {
                    if self.remote().is_some_and(|remote| !remote.is_read_only()) {
                        self.restored_units
                            .lock()
                            .insert(unit_hash.to_owned(), (fingerprint, digest));
                    }
                    tracing::debug!(unit_hash, "restored unit from local cache");
                    return Ok(Restored::Complete);
                }
                Err(error) => {
                    tracing::debug!(?error, unit_hash, "discarding invalid local cache entry");
                    store.evict_cache_entry(unit_hash)?;
                }
            }
        }
        tracing::debug!(unit_hash, "local cache miss");
        match prefetched {
            Some(Prefetched::Miss(key)) if key == fingerprint => {
                tracing::debug!(unit_hash, "remote cache miss already seen by prefetch");
                return Ok(Restored::Miss);
            }
            Some(Prefetched::Hit(key) | Prefetched::Miss(key) | Prefetched::Metadata(key, _))
                if key != fingerprint =>
            {
                tracing::debug!(unit_hash, "prefetch used a different input guard");
            }
            _ => {}
        }
        let Some(remote) = self.remote() else {
            tracing::debug!(unit_hash, "remote cache unavailable");
            return Ok(Restored::Miss);
        };
        let result = (|| {
            let Some((digest, entry)) =
                exchange::fetch(remote, &self.root, unit_hash, fingerprint)?
            else {
                return Ok(Restored::Miss);
            };
            if let Err(error) = self.restore_outputs(&entry, unit_dir, |_| true) {
                store.evict_cache_entry(unit_hash)?;
                return Err(error);
            }
            self.restored_units
                .lock()
                .insert(unit_hash.to_owned(), (fingerprint, digest));
            tracing::debug!(unit_hash, "restored unit from remote cache");
            CargoResult::Ok(Restored::Complete)
        })();
        match result {
            Ok(restored) => Ok(restored),
            Err(error) => {
                self.remote_failed(error);
                Ok(Restored::Miss)
            }
        }
    }

    /// Wait for the downloads after [`Restored::Metadata`] and restore the
    /// remaining outputs next to the restored metadata.
    pub(super) fn finish_restore(
        &self,
        pending: PendingOutputs,
        unit_dir: &Path,
    ) -> CargoResult<()> {
        let PendingOutputs {
            unit_hash,
            fingerprint,
            entry,
        } = pending;
        let downloaded = self
            .prefetcher
            .as_ref()
            .is_some_and(|prefetcher| prefetcher.wait_for_outputs(&unit_hash));
        if !downloaded {
            let reason = self
                .remote
                .as_ref()
                .and_then(|remote| remote.error())
                .unwrap_or_else(|| "the download did not finish".to_owned());
            anyhow::bail!("failed to download cached outputs: {reason}");
        }
        self.restore_more_outputs(&entry, unit_dir, |output| deferred_output(&output.path))?;
        if self.remote().is_some_and(|remote| !remote.is_read_only()) {
            let digest = *blake3::hash(&entry.encode()).as_bytes();
            self.restored_units
                .lock()
                .insert(unit_hash.clone(), (fingerprint, digest));
        }
        tracing::debug!(unit_hash, "restored unit from remote cache");
        Ok(())
    }

    /// Record an accepted cache hit as a snapshot member.
    pub(super) fn accept_cache_entry(&self, unit_hash: &str, unit_dir: &Path) -> CargoResult<()> {
        let entry = SnapshotStore::new(&self.root)
            .complete_cache_entry(unit_hash)?
            .context("restored cache entry disappeared from shared storage")?;
        self.record_outputs(
            unit_dir,
            entry
                .outputs
                .iter()
                .map(|output| (output.path.as_slice(), output.hash)),
        );
        self.used.lock().cache_entries.insert(unit_hash.to_owned());
        Ok(())
    }

    fn enqueue_upload(&self, remote: &Arc<Remote>, unit_hash: &str, entry: CacheEntry) {
        let mut uploader = self.uploader.lock();
        if uploader.is_none() {
            match Uploader::start(Arc::clone(remote), self.root.clone()) {
                Ok(started) => *uploader = Some(started),
                Err(error) => {
                    remote.failed(error);
                    return;
                }
            }
        }
        if let Some(uploader) = uploader.as_ref() {
            uploader.enqueue(unit_hash, entry);
        }
    }

    fn remote(&self) -> Option<&RemoteCache> {
        let remote = self.remote.as_ref()?;
        remote.usable().then_some(&remote.cache)
    }

    fn remote_failed(&self, error: anyhow::Error) {
        if let Some(remote) = &self.remote {
            remote.failed(error);
        }
    }

    /// Restore the outputs matching `select`, replacing the output tree.
    fn restore_outputs(
        &self,
        entry: &CacheEntry,
        unit_dir: &Path,
        select: impl Fn(&CachedOutput) -> bool,
    ) -> CargoResult<()> {
        let staging = self.stage_outputs(entry, unit_dir, select)?;
        for tree in OUTPUT_TREES {
            let staged = staging.path().join(tree);
            // The output tree always exists. Build-script runs also have `run`.
            if *tree != "out" && !staged.exists() {
                continue;
            }
            if let Err(error) = replace_tree(staging.path(), &staged, &unit_dir.join(tree)) {
                if error.downcast_ref::<OriginalPreserved>().is_some() {
                    let _ = staging.keep();
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Restore the outputs matching `select` into an existing output tree.
    fn restore_more_outputs(
        &self,
        entry: &CacheEntry,
        unit_dir: &Path,
        select: impl Fn(&CachedOutput) -> bool,
    ) -> CargoResult<()> {
        let staging = self.stage_outputs(entry, unit_dir, &select)?;
        check_directory(&unit_dir.join("out"))?;
        for output in entry.outputs.iter().filter(|output| select(output)) {
            let relative = restore_path(&output.path)?;
            let dest = unit_dir.join(&relative);
            fs::create_dir_all(dest.parent().unwrap())?;
            fs::rename(staging.path().join(&relative), &dest)?;
        }
        Ok(())
    }

    /// Copy and verify the outputs matching `select` into a staging directory
    /// within `unit_dir`, laid out like the unit directory.
    fn stage_outputs(
        &self,
        entry: &CacheEntry,
        unit_dir: &Path,
        select: impl Fn(&CachedOutput) -> bool,
    ) -> CargoResult<tempfile::TempDir> {
        check_directory(unit_dir)?;
        fs::create_dir_all(unit_dir)?;
        let staging = tempfile::Builder::new()
            .prefix(".cache-restore")
            .tempdir_in(unit_dir)?;
        fs::create_dir(staging.path().join("out"))?;
        for output in entry.outputs.iter().filter(|output| select(output)) {
            let relative = restore_path(&output.path)?;
            let source = blob_path(&self.root, &output.hash);
            ensure!(
                regular_size(&source)? == Some(output.size),
                "missing or invalid cached blob"
            );
            let dest = staging.path().join(relative);
            fs::create_dir_all(dest.parent().unwrap())?;
            if output.mode == SYMLINK_MODE {
                let target = fs::read(&source)?;
                ensure!(
                    blake3::hash(&target).as_bytes() == &output.hash,
                    "cached blob digest mismatch"
                );
                create_symlink(&cargo_util::paths::bytes2path(&target)?, &dest)?;
                continue;
            }
            private_copy(&source, &dest)?;
            ensure!(
                Self::hash(&dest)? == output.hash,
                "cached blob digest mismatch"
            );
            set_permission_mode(&dest, output.mode)?;
            let mtime = FileTime::from_unix_time(output.mtime_seconds, output.mtime_nanos);
            filetime::set_file_times(&dest, mtime, mtime)?;
            #[cfg(target_os = "linux")]
            ensure_no_writers(&dest)?;
        }
        Ok(staging)
    }

    /// Waits for background uploads. Only successful builds publish a snapshot
    /// and refresh its usage timestamp.
    pub fn finish(&self, gctx: &GlobalContext, successful: bool) -> CargoResult<()> {
        if let Some(prefetcher) = &self.prefetcher {
            prefetcher.shutdown();
        }
        let uploader = self.uploader.lock().take();
        if let Some(uploader) = uploader {
            let pending = uploader.pending();
            if pending > 0 {
                let units = if pending == 1 { "unit" } else { "units" };
                gctx.shell().status(
                    "Uploading",
                    format!("{pending} {units} to the remote cache"),
                )?;
            }
            uploader.finish();
        }
        if let Some(error) = self.remote.as_ref().and_then(|remote| remote.error()) {
            gctx.shell()
                .warn(format!("remote cache disabled for this build: {error}"))?;
        }
        let used = self.used.lock();
        if !successful || (used.unit_outputs.is_empty() && used.cache_entries.is_empty()) {
            return Ok(());
        }
        let unit_outputs = used.unit_outputs.iter().copied().collect();
        let cache_entries = used.cache_entries.iter().cloned().collect();
        let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
        SnapshotStore::new(&self.root).save(unit_outputs, cache_entries, now())
    }

    pub fn clean(
        root: &Path,
        clean_ctx: &mut CleanContext<'_>,
        max_size: Option<u64>,
    ) -> CargoResult<()> {
        SnapshotStore::new(root).clean(clean_ctx, max_size, now())
    }

    fn insert(
        &self,
        artifact: &Path,
        blob: &Path,
        expected: &Digest,
        replace: bool,
        hardlink_allowed: bool,
    ) -> CargoResult<bool> {
        if !replace && fs::symlink_metadata(blob).is_ok() {
            return Ok(false);
        }
        let staging = tempfile::Builder::new()
            .prefix(".blob")
            .tempdir_in(&self.root)?;
        let staged = staging.path().join("artifact");
        if reflink_copy::reflink(artifact, &staged).is_err()
            && (!hardlink_allowed || fs::hard_link(artifact, &staged).is_err())
        {
            fs::copy(artifact, &staged)?;
        }
        ensure!(
            &Self::hash(&staged)? == expected,
            "compiler output changed during capture"
        );
        #[cfg(target_os = "linux")]
        ensure_no_writers(&staged)?;
        publish_blob(&staged, blob)?;
        Ok(true)
    }

    fn dedup(
        &self,
        path: &Path,
        blob: &Path,
        expected: &Digest,
        hardlink_allowed: bool,
    ) -> CargoResult<()> {
        let metadata = path.metadata()?;
        if regular_size(blob)? != Some(metadata.len()) {
            self.insert(path, blob, expected, true, hardlink_allowed)?;
            return Ok(());
        }
        let staging = tempfile::Builder::new()
            .prefix(".blob")
            .tempdir_in(path.parent().unwrap())?;
        let replacement = staging.path().join("artifact");
        let stored = fs::symlink_metadata(blob)?;
        let private = if reflink_copy::reflink(blob, &replacement).is_ok() {
            true
        } else {
            let compatible = hardlink_allowed
                && stored.permissions() == metadata.permissions()
                && FileTime::from_last_modification_time(&stored)
                    == FileTime::from_last_modification_time(&metadata);
            if compatible && fs::hard_link(blob, &replacement).is_ok() {
                false
            } else {
                fs::copy(blob, &replacement)?;
                true
            }
        };
        if &Self::hash(&replacement)? != expected {
            self.insert(path, blob, expected, true, hardlink_allowed)?;
            return Ok(());
        }
        if private {
            fs::set_permissions(&replacement, metadata.permissions())?;
            filetime::set_file_times(
                &replacement,
                FileTime::from_last_access_time(&metadata),
                FileTime::from_last_modification_time(&metadata),
            )?;
        }
        #[cfg(target_os = "linux")]
        ensure_no_writers(&replacement)?;
        fs::rename(&replacement, path)?;
        Ok(())
    }

    #[instrument]
    fn hash(path: &Path) -> CargoResult<Digest> {
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(File::open(path)?)?;
        Ok(*hasher.finalize().as_bytes())
    }
}

/// Top-level directories of a unit that cache entries may contain. `run` holds
/// a build script's captured stdout, stderr, and `OUT_DIR` record.
const OUTPUT_TREES: &[&str] = &["out", "run"];

fn restore_path(bytes: &[u8]) -> CargoResult<PathBuf> {
    let path = decode_output_path(bytes)?;
    let tree = OUTPUT_TREES
        .iter()
        .find(|tree| path.starts_with(tree))
        .context("cached output is outside the output tree")?;
    ensure!(
        !path.strip_prefix(tree)?.as_os_str().is_empty(),
        "cached output has no filename"
    );
    Ok(path)
}

/// Marks a failed restore whose rollback also failed. The staging directory
/// holds the original output and must be kept.
#[derive(Debug)]
struct OriginalPreserved(PathBuf);

impl std::fmt::Display for OriginalPreserved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original output preserved at {}", self.0.display())
    }
}

/// Replace `dest` with `staged`, restoring the original if the swap fails.
fn replace_tree(staging: &Path, staged: &Path, dest: &Path) -> CargoResult<()> {
    check_directory(dest)?;
    let backup = staging.join(format!("previous-{}", dest.file_name().unwrap().display()));
    let had_dest = dest.try_exists()?;
    if had_dest {
        fs::rename(dest, &backup)?;
    }
    if let Err(error) = fs::rename(staged, dest) {
        if had_dest && let Err(rollback) = fs::rename(&backup, dest) {
            return Err(anyhow::Error::new(rollback)
                .context(format!("restoring output after {error}"))
                .context(OriginalPreserved(backup)));
        }
        return Err(error.into());
    }
    Ok(())
}

fn private_copy(source: &Path, dest: &Path) -> CargoResult<()> {
    if reflink_copy::reflink(source, dest).is_err() {
        fs::copy(source, dest)?;
    }
    Ok(())
}

fn symlink_target(path: &Path) -> CargoResult<Vec<u8>> {
    Ok(cargo_util::paths::path2bytes(&fs::read_link(path)?)?.to_vec())
}

fn create_symlink(target: &Path, link: &Path) -> CargoResult<()> {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link)?;
    #[cfg(not(unix))]
    anyhow::bail!("cannot restore symlink {} on this platform", link.display());
    #[cfg(unix)]
    Ok(())
}

fn permission_mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        if metadata.permissions().readonly() {
            0o444
        } else {
            0o666
        }
    }
}

fn set_permission_mode(path: &Path, mode: u32) -> CargoResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(mode & 0o222 == 0);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// Reflinking briefly holds a writable fd which a concurrently forked compiler
/// may inherit. Wait for its read lease before executing or publishing the file.
#[cfg(target_os = "linux")]
fn ensure_no_writers(path: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let file = File::open(path)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_RDLCK) } == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EAGAIN) || std::time::Instant::now() >= deadline {
            return Err(error);
        }
        // A child can retain our closed writable descriptor until it execs.
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage(root: &Path) -> BlobStorage {
        let root = root.join("shared-storage");
        fs::create_dir(&root).unwrap();
        BlobStorage {
            root,
            used: Mutex::default(),
            remote: None,
            uploader: Mutex::default(),
            prefetcher: None,
            restored_units: Mutex::default(),
            tree_hashes: Mutex::default(),
        }
    }

    fn capture(storage: &BlobStorage, unit: &Path, hardlink_allowed: bool) {
        fs::create_dir_all(unit.join("out")).unwrap();
        fs::write(unit.join("out/artifact"), b"compiled bytes").unwrap();
        fs::write(unit.join("out/artifact.d"), b"artifact: input.rs\n").unwrap();
        let outputs = storage
            .capture_outputs(unit, &unit.join("out"), hardlink_allowed)
            .unwrap();
        assert_eq!(
            storage
                .publish_cache_entry("1234", 42, outputs, unit)
                .unwrap(),
            TrackedOutput::CacheEntry
        );
    }

    #[test]
    fn cache_hit_restores_tree_and_dep_info_without_tracking_rejected_hits() {
        let root = tempfile::tempdir().unwrap();
        let storage = storage(root.path());
        let unit = root.path().join("unit");
        capture(&storage, &unit, true);
        storage.used.lock().cache_entries.clear();
        fs::remove_dir_all(unit.join("out")).unwrap();
        assert!(matches!(
            storage
                .restore_cache_entry("1234", 41, &unit, true)
                .unwrap(),
            Restored::Miss
        ));
        assert!(!unit.join("out").exists());
        assert!(matches!(
            storage
                .restore_cache_entry("1234", 42, &unit, true)
                .unwrap(),
            Restored::Complete
        ));
        assert_eq!(
            fs::read(unit.join("out/artifact")).unwrap(),
            b"compiled bytes"
        );
        assert_eq!(
            fs::read(unit.join("out/artifact.d")).unwrap(),
            b"artifact: input.rs\n"
        );
        assert!(storage.used.lock().cache_entries.is_empty());
        storage.accept_cache_entry("1234", &unit).unwrap();
        assert!(storage.used.lock().cache_entries.contains("1234"));
        let tracked = Some(TrackedOutput::CacheEntry);
        assert_eq!(
            storage.prepare_unit(tracked, Some("1234"), &unit).unwrap(),
            tracked
        );
        // Units that are no longer cacheable are recaptured as unit outputs.
        assert_eq!(storage.prepare_unit(tracked, None, &unit).unwrap(), None);
    }

    #[test]
    fn corrupt_blob_misses_without_replacing_any_output() {
        let root = tempfile::tempdir().unwrap();
        let storage = storage(root.path());
        let unit = root.path().join("unit");
        capture(&storage, &unit, true);
        let blob = blob_path(&storage.root, blake3::hash(b"compiled bytes").as_bytes());
        fs::write(blob, b"corrupted data").unwrap();
        fs::write(unit.join("out/artifact"), b"keep original").unwrap();
        fs::write(unit.join("out/artifact.d"), b"keep dep-info").unwrap();
        assert!(matches!(
            storage
                .restore_cache_entry("1234", 42, &unit, true)
                .unwrap(),
            Restored::Miss
        ));
        assert_eq!(
            fs::read(unit.join("out/artifact")).unwrap(),
            b"keep original"
        );
        assert_eq!(
            fs::read(unit.join("out/artifact.d")).unwrap(),
            b"keep dep-info"
        );
        assert!(
            SnapshotStore::new(&storage.root)
                .read_cache_entry("1234")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn capture_does_not_share_mutable_blob_inodes() {
        let root = tempfile::tempdir().unwrap();
        let storage = storage(root.path());
        let unit = root.path().join("unit");
        capture(&storage, &unit, false);
        storage
            .capture_outputs(&unit, &unit.join("out"), false)
            .unwrap();
        fs::write(unit.join("out/artifact"), b"later compiler output").unwrap();
        let blob = blob_path(&storage.root, blake3::hash(b"compiled bytes").as_bytes());
        assert_eq!(fs::read(blob).unwrap(), b"compiled bytes");
    }

    #[test]
    fn restores_only_paths_inside_output_tree() {
        for path in [
            "../escape",
            "out/../escape",
            "fingerprint/entry",
            "out",
            "/out/file",
        ] {
            assert!(
                encode_output_path(Path::new(path))
                    .and_then(|path| restore_path(&path))
                    .is_err()
            );
        }
        assert_eq!(
            restore_path(&encode_output_path(Path::new("out/a/b")).unwrap()).unwrap(),
            Path::new("out/a/b")
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_preserves_execution_and_mtime_without_mutating_blob_metadata() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let storage = storage(root.path());
        let unit = root.path().join("unit");
        fs::create_dir_all(unit.join("out")).unwrap();
        let artifact = unit.join("out/program");
        let contents = b"#!/bin/sh\nprintf 'cache-ok\\n'\n";
        fs::write(&artifact, contents).unwrap();
        fs::set_permissions(&artifact, fs::Permissions::from_mode(0o755)).unwrap();
        let compiled = FileTime::from_unix_time(1_600_000_100, 0);
        filetime::set_file_mtime(&artifact, compiled).unwrap();
        let outputs = storage
            .capture_outputs(&unit, &unit.join("out"), true)
            .unwrap();
        assert_eq!(
            storage
                .publish_cache_entry("1234", 42, outputs, &unit)
                .unwrap(),
            TrackedOutput::CacheEntry
        );
        let blob = blob_path(&storage.root, blake3::hash(contents).as_bytes());
        fs::set_permissions(&blob, fs::Permissions::from_mode(0o444)).unwrap();
        let downloaded = FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_mtime(&blob, downloaded).unwrap();
        fs::remove_dir_all(unit.join("out")).unwrap();
        assert!(matches!(
            storage
                .restore_cache_entry("1234", 42, &unit, true)
                .unwrap(),
            Restored::Complete
        ));
        let metadata = artifact.metadata().unwrap();
        assert_eq!(FileTime::from_last_modification_time(&metadata), compiled);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o755);
        let result = std::process::Command::new(&artifact).output().unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout, b"cache-ok\n");
        let metadata = blob.metadata().unwrap();
        assert_eq!(FileTime::from_last_modification_time(&metadata), downloaded);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o444);
    }

    #[cfg(unix)]
    #[test]
    fn blob_symlink_cannot_redirect_compiler_output() {
        let root = tempfile::tempdir().unwrap();
        let storage = storage(root.path());
        let unit = root.path().join("unit");
        fs::create_dir_all(unit.join("out")).unwrap();
        let artifact = unit.join("out/artifact");
        let contents = b"compiled output";
        fs::write(&artifact, contents).unwrap();
        let hash = *blake3::hash(contents).as_bytes();
        let blob = blob_path(&storage.root, &hash);
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&artifact, &blob).unwrap();
        storage.dedup(&artifact, &blob, &hash, true).unwrap();
        assert!(fs::symlink_metadata(&artifact).unwrap().is_file());
        assert!(fs::symlink_metadata(&blob).unwrap().is_file());
        assert_eq!(fs::read(artifact).unwrap(), contents);
        assert_eq!(fs::read(blob).unwrap(), contents);
    }
}
