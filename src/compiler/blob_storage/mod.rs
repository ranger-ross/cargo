mod format;
mod snapshots;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use cargo_util::paths::create_dir_all;
use filetime::FileTime;
use format::{Digest, Output, UnitResult, encode_output_path};
use parking_lot::Mutex;
use tracing::instrument;

use crate::ops::CleanContext;
use crate::util::cache_lock::CacheLockMode;
use crate::util::data_structures::HashSet;
use crate::{CargoResult, GlobalContext};
use snapshots::SnapshotStore;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BlobRecord {
    pub revision: [u8; 32],
    pub result: [u8; 32],
}

pub struct BlobStorage {
    root: PathBuf,
    build_dir_id: Vec<u8>,
    revision: Digest,
    validate_all: bool,
    used: Mutex<HashSet<Digest>>,
}

impl BlobStorage {
    pub fn new(root: PathBuf, build_dir: &Path, gctx: &GlobalContext) -> CargoResult<Self> {
        create_dir_all(&root)?;

        let build_dir_id = std::fs::canonicalize(build_dir)?
            .as_os_str()
            .as_encoded_bytes()
            .to_vec();
        let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
        let store = SnapshotStore::new(&root);
        let revision = store.revision()?;
        let validate_all = !store.has_receipt(&build_dir_id)?;
        Ok(Self {
            root,
            build_dir_id,
            revision,
            validate_all,
            used: Mutex::new(HashSet::default()),
        })
    }

    /// Reuses a fingerprint pointer, checking its inventory after collection.
    pub(super) fn prepare_unit(
        &self,
        record: Option<BlobRecord>,
    ) -> CargoResult<Option<BlobRecord>> {
        let Some(record) = record else {
            return Ok(None);
        };
        if (self.validate_all || record.revision != self.revision)
            && !SnapshotStore::new(&self.root).unit_is_complete(&record.result)?
        {
            return Ok(None);
        }
        self.used.lock().insert(record.result);
        Ok(Some(BlobRecord {
            revision: self.revision,
            result: record.result,
        }))
    }

    /// Captures a newly completed unit or adopts previously untracked outputs.
    #[instrument(skip_all)]
    pub(super) fn capture_unit(&self, unit_dir: &Path, out_dir: &Path) -> CargoResult<BlobRecord> {
        let mut outputs = Vec::new();
        for entry in walkdir::WalkDir::new(out_dir) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let size = entry.metadata()?.len();
            let hash = Self::hash(path)?;
            let storage_path = self
                .root
                .join(blake3::Hash::from_bytes(hash).to_hex().as_str());
            if !self.insert(path, &storage_path, false)? {
                self.dedup(path, &storage_path, &hash)?;
            }
            outputs.push(Output {
                path: encode_output_path(path.strip_prefix(unit_dir)?)?,
                hash,
                size,
            });
        }
        let result = UnitResult::new(outputs);
        SnapshotStore::new(&self.root).publish_unit(&result)?;
        self.used.lock().insert(result.id);
        Ok(BlobRecord {
            revision: self.revision,
            result: result.id,
        })
    }

    /// Only successful builds publish a graph snapshot and refresh usage.
    pub fn finish(&self, gctx: &GlobalContext, successful: bool) -> CargoResult<()> {
        let used = self.used.lock();
        if !successful || used.is_empty() {
            return Ok(());
        }
        let results = used.iter().copied().collect();
        let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
        SnapshotStore::new(&self.root).save(&self.build_dir_id, results, now())
    }

    /// Collects expired snapshots under the package-cache mutation lock.
    pub fn clean(
        root: &Path,
        clean_ctx: &mut CleanContext<'_>,
        max_size: Option<u64>,
    ) -> CargoResult<()> {
        if !root.try_exists()? {
            return Ok(());
        }
        SnapshotStore::new(root).clean(clean_ctx, max_size, now())
    }

    fn insert(
        &self,
        artifact_path: &Path,
        storage_path: &Path,
        replace_corrupt: bool,
    ) -> CargoResult<bool> {
        if !replace_corrupt && storage_path.try_exists()? {
            return Ok(false);
        }

        // Publish complete bytes atomically. A concurrent producer of the same
        // digest has identical contents, so replacing its name is harmless.
        let staging_dir = tempfile::Builder::new()
            .prefix(".blob")
            .tempdir_in(&self.root)?;
        let staged = staging_dir.path().join("artifact");
        if reflink_copy::reflink(artifact_path, &staged).is_err()
            && std::fs::hard_link(artifact_path, &staged).is_err()
        {
            std::fs::copy(artifact_path, &staged)?;
        }
        #[cfg(target_os = "linux")]
        ensure_no_writers(&staged)?;
        std::fs::rename(&staged, storage_path)?;
        Ok(true)
    }

    fn dedup(&self, path: &Path, storage_path: &Path, expected: &Digest) -> CargoResult<()> {
        let metadata = path.metadata()?;
        let stored = std::fs::symlink_metadata(storage_path)?;
        if !stored.is_file() || stored.len() != metadata.len() {
            self.insert(path, storage_path, true)?;
            return Ok(());
        }
        let staging_dir = tempfile::Builder::new()
            .prefix(".blob")
            .tempdir_in(path.parent().unwrap())?;
        let replacement = staging_dir.path().join("artifact");
        let private_copy = if reflink_copy::reflink(storage_path, &replacement).is_ok() {
            true
        } else {
            let compatible = stored.permissions() == metadata.permissions()
                && FileTime::from_last_modification_time(&stored)
                    == FileTime::from_last_modification_time(&metadata);
            if compatible && std::fs::hard_link(storage_path, &replacement).is_ok() {
                false
            } else {
                std::fs::copy(storage_path, &replacement)?;
                true
            }
        };
        if private_copy {
            std::fs::set_permissions(&replacement, metadata.permissions())?;
            filetime::set_file_times(
                &replacement,
                FileTime::from_last_access_time(&metadata),
                FileTime::from_last_modification_time(&metadata),
            )?;
        }
        #[cfg(target_os = "linux")]
        ensure_no_writers(&replacement)?;
        // Restored cache objects must not replace good compiler outputs with
        // bytes that do not match their content-addressed name.
        if &Self::hash(&replacement)? != expected {
            self.insert(path, storage_path, true)?;
            return Ok(());
        }
        std::fs::rename(&replacement, path)?;

        Ok(())
    }

    #[instrument]
    fn hash(path: &Path) -> CargoResult<Digest> {
        let mut hasher = blake3::Hasher::new();
        let file = File::open(path)?;
        hasher.update_reader(file)?;
        Ok(*hasher.finalize().as_bytes())
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Errors if there we cannot get a read lease to a file (ensuring no writers)
///
/// On Linux we need this to avoid concurrency issues when deduplicating with reflinks.
/// During reflinking, there is a brief window where we hold a write lease to the file.
/// If during this period, another worker fork's (say to spawn a rustc process) that process
/// will inhierit the writable fd. This is problematic as we cannot execute a file that has a
/// writable fd which breaks things like executing build scripts.
#[cfg(target_os = "linux")]
fn ensure_no_writers(path: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let file = File::open(path)?;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_RDLCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn restored_blob_preserves_executability_and_mtime() {
        let root = tempfile::tempdir().unwrap();
        let build_dir = tempfile::tempdir().unwrap();
        let storage = BlobStorage {
            root: root.path().to_path_buf(),
            build_dir_id: Vec::new(),
            revision: [0; 32],
            validate_all: false,
            used: Mutex::default(),
        };
        let contents = b"#!/bin/sh\nprintf 'cache-ok\\n'\n";
        let hash = *blake3::hash(contents).as_bytes();
        let blob = root
            .path()
            .join(blake3::Hash::from_bytes(hash).to_hex().as_str());
        let artifact = build_dir.path().join("program");
        std::fs::write(&blob, contents).unwrap();
        std::fs::write(&artifact, contents).unwrap();
        std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&artifact, std::fs::Permissions::from_mode(0o755)).unwrap();
        let downloaded = FileTime::from_unix_time(1_600_000_000, 0);
        let compiled = FileTime::from_unix_time(1_600_000_100, 0);
        filetime::set_file_mtime(&blob, downloaded).unwrap();
        filetime::set_file_mtime(&artifact, compiled).unwrap();

        storage.dedup(&artifact, &blob, &hash).unwrap();

        let metadata = artifact.metadata().unwrap();
        assert_eq!(FileTime::from_last_modification_time(&metadata), compiled);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o755);
        let output = std::process::Command::new(&artifact).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"cache-ok\n");
        let metadata = blob.metadata().unwrap();
        assert_eq!(FileTime::from_last_modification_time(&metadata), downloaded);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o444);
    }

    #[test]
    fn restored_blob_symlink_cannot_redirect_compiler_output() {
        let root = tempfile::tempdir().unwrap();
        let build_dir = tempfile::tempdir().unwrap();
        let storage = BlobStorage {
            root: root.path().to_path_buf(),
            build_dir_id: Vec::new(),
            revision: [0; 32],
            validate_all: false,
            used: Mutex::default(),
        };
        let contents = b"compiled output";
        let hash = *blake3::hash(contents).as_bytes();
        let blob = root
            .path()
            .join(blake3::Hash::from_bytes(hash).to_hex().as_str());
        let artifact = build_dir.path().join("artifact");
        std::fs::write(&artifact, contents).unwrap();
        std::os::unix::fs::symlink(&artifact, &blob).unwrap();

        storage.dedup(&artifact, &blob, &hash).unwrap();

        assert!(std::fs::symlink_metadata(&artifact).unwrap().is_file());
        assert!(std::fs::symlink_metadata(&blob).unwrap().is_file());
        assert_eq!(std::fs::read(&artifact).unwrap(), contents);
        assert_eq!(std::fs::read(&blob).unwrap(), contents);
    }
}
