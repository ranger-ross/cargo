mod exchange;
mod format;
mod remote;
mod snapshots;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use cargo_util::paths::create_dir_all;
use filetime::FileTime;
use format::{
    CacheEntry, Digest, Output, OutputMetadata, UnitOutput, decode_output_path, encode_output_path,
};
use parking_lot::Mutex;
use remote::RemoteCache;
use tracing::instrument;

use crate::ops::CleanContext;
use crate::util::cache_lock::CacheLockMode;
use crate::util::data_structures::{HashMap, HashSet};
use crate::{CargoResult, GlobalContext};
use snapshots::{SnapshotStore, check_directory, hex, regular_size};

pub(super) type UnitOutputHash = [u8; 32];

pub struct BlobStorage {
    root: PathBuf,
    workspace_id: Digest,
    used: Mutex<HashSet<Digest>>,
    remote: Option<RemoteCache>,
    remote_error: Mutex<Option<String>>,
    // Rejected hits can still publish if recompilation changes either identity.
    restored_units: Mutex<HashMap<String, (u64, Digest)>>,
}

impl BlobStorage {
    pub fn new(root: PathBuf, workspace_root: &Path, gctx: &GlobalContext) -> CargoResult<Self> {
        let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
        check_directory(&root)?;
        create_dir_all(&root)?;
        let workspace_root = fs::canonicalize(workspace_root)?;
        let workspace_id = *blake3::hash(workspace_root.as_os_str().as_encoded_bytes()).as_bytes();
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
        Ok(Self {
            root,
            workspace_id,
            used: Mutex::default(),
            remote,
            remote_error: Mutex::default(),
            restored_units: Mutex::default(),
        })
    }

    /// Validate the immutable unit output before reusing a fingerprint pointer.
    pub(super) fn prepare_unit(
        &self,
        unit_output: Option<UnitOutputHash>,
    ) -> CargoResult<Option<UnitOutputHash>> {
        let Some(id) = unit_output else {
            return Ok(None);
        };
        if !SnapshotStore::new(&self.root).unit_is_complete(&id)? {
            return Ok(None);
        }
        self.used.lock().insert(id);
        Ok(Some(id))
    }

    /// Capture all regular compiler outputs, including raw rustc dep-info.
    /// Hardlinks are allowed only when the compiler detaches the output tree
    /// before every dirty run, including builds with tracking disabled.
    #[instrument(skip_all)]
    pub(super) fn capture_unit(
        &self,
        unit_dir: &Path,
        out_dir: &Path,
        hardlink_allowed: bool,
    ) -> CargoResult<UnitOutputHash> {
        ensure!(
            out_dir == unit_dir.join("out"),
            "unexpected unit output directory"
        );
        check_directory(out_dir)?;
        let mut outputs = Vec::new();
        for entry in walkdir::WalkDir::new(out_dir) {
            let entry = entry?;
            ensure!(
                !entry.file_type().is_symlink(),
                "symlink in unit output: {}",
                entry.path().display()
            );
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let size = entry.metadata()?.len();
            let hash = Self::hash(path)?;
            let storage_path = self.root.join(hex(&hash));
            if !self.insert(path, &storage_path, &hash, false, hardlink_allowed)? {
                self.dedup(path, &storage_path, &hash, hardlink_allowed)?;
            }
            outputs.push(Output {
                path: encode_output_path(path.strip_prefix(unit_dir)?)?,
                hash,
                size,
            });
        }
        let output = UnitOutput::new(outputs);
        SnapshotStore::new(&self.root).publish_unit(&output)?;
        self.used.lock().insert(output.id);
        Ok(output.id)
    }

    pub(super) fn publish_cache_entry(
        &self,
        unit_hash: &str,
        fingerprint: u64,
        unit_output: UnitOutputHash,
        unit_dir: &Path,
    ) -> CargoResult<()> {
        let store = SnapshotStore::new(&self.root);
        let outputs = store
            .read_unit(&unit_output)?
            .context("missing captured unit output")?;
        let mut metadata = Vec::with_capacity(outputs.len());
        for output in &outputs {
            let relative = restore_path(&output.path)?;
            let path = unit_dir.join(relative);
            let file = fs::symlink_metadata(&path)?;
            ensure!(
                file.is_file() && file.len() == output.size,
                "captured output changed: {}",
                path.display()
            );
            let mtime = FileTime::from_last_modification_time(&file);
            metadata.push(OutputMetadata {
                mode: permission_mode(&file),
                mtime_seconds: mtime.unix_seconds(),
                mtime_nanos: mtime.nanoseconds(),
            });
        }
        let entry = CacheEntry {
            unit_output,
            fingerprint,
            outputs: metadata,
        };
        store.publish_cache_entry(unit_hash, &entry)?;
        if let Some(remote) = self.remote()
            && !remote.is_read_only()
        {
            let restored =
                self.restored_units.lock().get(unit_hash) == Some(&(fingerprint, unit_output));
            if restored {
                tracing::debug!(
                    unit_hash,
                    "skipping remote publication of unchanged cache hit"
                );
            }
            if !restored
                && let Err(error) =
                    exchange::publish(remote, &self.root, unit_hash, &entry, &outputs)
            {
                self.remote_failed(error);
            }
        }
        Ok(())
    }

    /// Stage and verify all restored bytes before replacing the output tree.
    /// The compiler calls `prepare_unit` after accepting dep-info/environment
    /// validation, so a rejected hit cannot become a successful snapshot member.
    pub(super) fn restore_cache_entry(
        &self,
        unit_hash: &str,
        fingerprint: u64,
        unit_dir: &Path,
    ) -> CargoResult<Option<UnitOutputHash>> {
        let store = SnapshotStore::new(&self.root);
        if let Some(entry) = store.read_cache_entry(unit_hash)?
            && entry.fingerprint == fingerprint
        {
            match self.restore_outputs(&store, &entry, unit_dir) {
                Ok(()) => {
                    if self.remote().is_some_and(|remote| !remote.is_read_only()) {
                        self.restored_units
                            .lock()
                            .insert(unit_hash.to_owned(), (fingerprint, entry.unit_output));
                    }
                    tracing::debug!(unit_hash, "restored unit from local cache");
                    return Ok(Some(entry.unit_output));
                }
                Err(error) => {
                    tracing::debug!(?error, unit_hash, "discarding invalid local cache entry");
                    store.evict_cache_entry(unit_hash)?;
                }
            }
        }
        tracing::debug!(unit_hash, "local cache miss");
        let Some(remote) = self.remote() else {
            tracing::debug!(unit_hash, "remote cache unavailable");
            return Ok(None);
        };
        let result = (|| {
            let Some(entry) = exchange::fetch(remote, &self.root, unit_hash, fingerprint)? else {
                return Ok(None);
            };
            if let Err(error) = self.restore_outputs(&store, &entry, unit_dir) {
                store.evict_cache_entry(unit_hash)?;
                return Err(error);
            }
            self.restored_units
                .lock()
                .insert(unit_hash.to_owned(), (fingerprint, entry.unit_output));
            tracing::debug!(unit_hash, "restored unit from remote cache");
            CargoResult::Ok(Some(entry.unit_output))
        })();
        match result {
            Ok(output) => Ok(output),
            Err(error) => {
                self.remote_failed(error);
                Ok(None)
            }
        }
    }

    fn remote(&self) -> Option<&RemoteCache> {
        let remote = self.remote.as_ref()?;
        self.remote_error.lock().is_none().then_some(remote)
    }

    fn remote_failed(&self, error: anyhow::Error) {
        self.remote_error
            .lock()
            .get_or_insert_with(|| format!("{error:#}"));
    }

    fn restore_outputs(
        &self,
        store: &SnapshotStore<'_>,
        entry: &CacheEntry,
        unit_dir: &Path,
    ) -> CargoResult<()> {
        let outputs = store
            .read_unit(&entry.unit_output)?
            .context("missing cached unit output")?;
        ensure!(
            outputs.len() == entry.outputs.len(),
            "cached output metadata count mismatch"
        );
        check_directory(unit_dir)?;
        fs::create_dir_all(unit_dir)?;
        let staging = tempfile::Builder::new()
            .prefix(".cache-restore")
            .tempdir_in(unit_dir)?;
        let staged_out = staging.path().join("out");
        fs::create_dir(&staged_out)?;
        for (output, metadata) in outputs.iter().zip(&entry.outputs) {
            let relative = restore_path(&output.path)?;
            let source = self.root.join(hex(&output.hash));
            ensure!(
                regular_size(&source)? == Some(output.size),
                "missing or invalid cached blob"
            );
            let dest = staging.path().join(relative);
            fs::create_dir_all(dest.parent().unwrap())?;
            private_copy(&source, &dest)?;
            ensure!(
                Self::hash(&dest)? == output.hash,
                "cached blob digest mismatch"
            );
            set_permission_mode(&dest, metadata.mode)?;
            let mtime = FileTime::from_unix_time(metadata.mtime_seconds, metadata.mtime_nanos);
            filetime::set_file_times(&dest, mtime, mtime)?;
            #[cfg(target_os = "linux")]
            ensure_no_writers(&dest)?;
        }
        let out = unit_dir.join("out");
        check_directory(&out)?;
        let backup = staging.path().join("previous-out");
        let had_out = out.try_exists()?;
        if had_out {
            fs::rename(&out, &backup)?;
        }
        if let Err(error) = fs::rename(&staged_out, &out) {
            if had_out {
                if let Err(rollback) = fs::rename(&backup, &out) {
                    // Preserve the original output if rollback itself fails.
                    let preserved = staging.keep();
                    return Err(rollback).with_context(|| {
                        format!(
                            "restoring output after {error}; original output preserved at {}",
                            preserved.join("previous-out").display()
                        )
                    });
                }
            }
            return Err(error.into());
        }
        Ok(())
    }

    /// Only successful builds publish a snapshot and refresh workspace history.
    pub fn finish(&self, gctx: &GlobalContext, successful: bool) -> CargoResult<()> {
        if let Some(error) = self.remote_error.lock().as_ref() {
            gctx.shell()
                .warn(format!("remote cache disabled for this build: {error}"))?;
        }
        let used = self.used.lock();
        if !successful || used.is_empty() {
            return Ok(());
        }
        let outputs = used.iter().copied().collect();
        let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
        SnapshotStore::new(&self.root).save(&self.workspace_id, outputs, now())
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
        fs::rename(&staged, blob)?;
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

fn restore_path(bytes: &[u8]) -> CargoResult<PathBuf> {
    let path = decode_output_path(bytes)?;
    let suffix = path
        .strip_prefix("out")
        .context("cached output is outside the output tree")?;
    ensure!(
        !suffix.as_os_str().is_empty(),
        "cached output has no filename"
    );
    Ok(path)
}

fn private_copy(source: &Path, dest: &Path) -> CargoResult<()> {
    if reflink_copy::reflink(source, dest).is_err() {
        fs::copy(source, dest)?;
    }
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
        let root = root.join("blobs");
        fs::create_dir(&root).unwrap();
        BlobStorage {
            root,
            workspace_id: [1; 32],
            used: Mutex::default(),
            remote: None,
            remote_error: Mutex::default(),
            restored_units: Mutex::default(),
        }
    }

    fn capture(storage: &BlobStorage, unit: &Path, hardlink_allowed: bool) -> UnitOutputHash {
        fs::create_dir_all(unit.join("out")).unwrap();
        fs::write(unit.join("out/artifact"), b"compiled bytes").unwrap();
        fs::write(unit.join("out/artifact.d"), b"artifact: input.rs\n").unwrap();
        let output = storage
            .capture_unit(unit, &unit.join("out"), hardlink_allowed)
            .unwrap();
        storage
            .publish_cache_entry("1234", 42, output, unit)
            .unwrap();
        output
    }

    #[test]
    fn cache_hit_restores_tree_and_dep_info_without_tracking_rejected_hits() {
        let root = tempfile::tempdir().unwrap();
        let storage = storage(root.path());
        let unit = root.path().join("unit");
        let output = capture(&storage, &unit, true);
        storage.used.lock().clear();
        fs::remove_dir_all(unit.join("out")).unwrap();
        assert_eq!(
            storage.restore_cache_entry("1234", 41, &unit).unwrap(),
            None
        );
        assert!(!unit.join("out").exists());
        assert_eq!(
            storage.restore_cache_entry("1234", 42, &unit).unwrap(),
            Some(output)
        );
        assert_eq!(
            fs::read(unit.join("out/artifact")).unwrap(),
            b"compiled bytes"
        );
        assert_eq!(
            fs::read(unit.join("out/artifact.d")).unwrap(),
            b"artifact: input.rs\n"
        );
        assert!(storage.used.lock().is_empty());
        assert_eq!(storage.prepare_unit(Some(output)).unwrap(), Some(output));
        assert!(storage.used.lock().contains(&output));
    }

    #[test]
    fn corrupt_blob_misses_without_replacing_any_output() {
        let root = tempfile::tempdir().unwrap();
        let storage = storage(root.path());
        let unit = root.path().join("unit");
        capture(&storage, &unit, true);
        let blob = storage
            .root
            .join(blake3::hash(b"compiled bytes").to_hex().as_str());
        fs::write(blob, b"corrupted data").unwrap();
        fs::write(unit.join("out/artifact"), b"keep original").unwrap();
        fs::write(unit.join("out/artifact.d"), b"keep dep-info").unwrap();
        assert_eq!(
            storage.restore_cache_entry("1234", 42, &unit).unwrap(),
            None
        );
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
            .capture_unit(&unit, &unit.join("out"), false)
            .unwrap();
        fs::write(unit.join("out/artifact"), b"later compiler output").unwrap();
        let blob = storage
            .root
            .join(blake3::hash(b"compiled bytes").to_hex().as_str());
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
        let output = storage
            .capture_unit(&unit, &unit.join("out"), true)
            .unwrap();
        storage
            .publish_cache_entry("1234", 42, output, &unit)
            .unwrap();
        let blob = storage.root.join(blake3::hash(contents).to_hex().as_str());
        fs::set_permissions(&blob, fs::Permissions::from_mode(0o444)).unwrap();
        let downloaded = FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_mtime(&blob, downloaded).unwrap();
        fs::remove_dir_all(unit.join("out")).unwrap();
        assert_eq!(
            storage.restore_cache_entry("1234", 42, &unit).unwrap(),
            Some(output)
        );
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
        let blob = storage.root.join(hex(&hash));
        std::os::unix::fs::symlink(&artifact, &blob).unwrap();
        storage.dedup(&artifact, &blob, &hash, true).unwrap();
        assert!(fs::symlink_metadata(&artifact).unwrap().is_file());
        assert!(fs::symlink_metadata(&blob).unwrap().is_file());
        assert_eq!(fs::read(artifact).unwrap(), contents);
        assert_eq!(fs::read(blob).unwrap(), contents);
    }
}
